import { mkdir, readFile, writeFile, unlink, rename } from "node:fs/promises"
import { dirname, join } from "node:path"
import { homedir } from "node:os"
import { keychainGet, keychainSet, keychainDelete } from "../../../keychain.ts"

export interface StoredAuth {
  access: string
  refresh: string
  expires: number
  accountId?: string
}

const DIR = join(homedir(), ".config", "claude-code-proxy", "codex")
const FILE = join(DIR, "auth.json")
const KEYCHAIN_SERVICE = "claude-code-proxy.codex"
const KEYCHAIN_ACCOUNT = "auth"

export async function loadAuth(): Promise<StoredAuth | undefined> {
  if (process.platform === "darwin") {
    const raw = keychainGet(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
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
    keychainSet(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, JSON.stringify(auth))
    return
  }

  await mkdir(dirname(FILE), { recursive: true })
  const tmp = `${FILE}.${process.pid}.${Date.now()}.tmp`
  await writeFile(tmp, JSON.stringify(auth, null, 2), { encoding: "utf8", mode: 0o600 })
  await rename(tmp, FILE)
}

export async function clearAuth(): Promise<void> {
  if (process.platform === "darwin") {
    keychainDelete(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
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
