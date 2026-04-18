import { mkdir, readFile, writeFile, chmod, unlink, rename } from "node:fs/promises"
import { dirname, join } from "node:path"
import { homedir } from "node:os"

export interface StoredAuth {
  access: string
  refresh: string
  expires: number
  accountId?: string
}

const DIR = join(homedir(), ".config", "claude-codex-proxy")
const FILE = join(DIR, "auth.json")
const KEYCHAIN_SERVICE = "claude-codex-proxy"
const KEYCHAIN_ACCOUNT = "auth"

export async function loadAuth(): Promise<StoredAuth | undefined> {
  if (process.platform === "darwin") {
    const raw = await readKeychain().catch((err: Error & { code?: number }) => {
      if (err.code === 44) return undefined
      throw err
    })
    if (!raw) return undefined
    return JSON.parse(raw) as StoredAuth
  }

  try {
    const raw = await readFile(FILE, "utf8")
    return JSON.parse(raw) as StoredAuth
  } catch (err: any) {
    if (err?.code === "ENOENT") return undefined
    throw err
  }
}

export async function saveAuth(auth: StoredAuth): Promise<void> {
  if (process.platform === "darwin") {
    await runSecurity([
      "add-generic-password",
      "-U",
      "-a",
      KEYCHAIN_ACCOUNT,
      "-s",
      KEYCHAIN_SERVICE,
      "-w",
      JSON.stringify(auth),
    ])
    return
  }

  await mkdir(dirname(FILE), { recursive: true })
  const tmp = `${FILE}.${process.pid}.${Date.now()}.tmp`
  await writeFile(tmp, JSON.stringify(auth, null, 2), { encoding: "utf8", mode: 0o600 })
  try {
    await chmod(tmp, 0o600)
  } catch {
    // best-effort; mode in writeFile options usually suffices
  }
  await rename(tmp, FILE)
}

export async function clearAuth(): Promise<void> {
  if (process.platform === "darwin") {
    await runSecurity(["delete-generic-password", "-a", KEYCHAIN_ACCOUNT, "-s", KEYCHAIN_SERVICE]).catch(
      (err: Error & { code?: number }) => {
        if (err.code !== 44) throw err
      },
    )
    return
  }

  try {
    await unlink(FILE)
  } catch (err: any) {
    if (err?.code !== "ENOENT") throw err
  }
}

export function authPath(): string {
  return process.platform === "darwin" ? "macOS Keychain" : FILE
}

async function readKeychain(): Promise<string> {
  const { stdout } = await runSecurity(["find-generic-password", "-w", "-a", KEYCHAIN_ACCOUNT, "-s", KEYCHAIN_SERVICE])
  return stdout.trim()
}

async function runSecurity(args: string[]): Promise<{ stdout: string; stderr: string }> {
  const proc = Bun.spawn(["security", ...args], {
    stdout: "pipe",
    stderr: "pipe",
  })
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ])
  if (exitCode !== 0) {
    const err = new Error(stderr.trim() || `security exited with ${exitCode}`) as Error & { code?: number }
    err.code = exitCode
    throw err
  }
  return { stdout, stderr }
}
