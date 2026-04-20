import { mkdir, readFile, writeFile, chmod } from "node:fs/promises"
import { dirname, join } from "node:path"
import { homedir } from "node:os"

const PATH = join(homedir(), ".config", "claude-code-proxy", "kimi", "device_id")

export async function getDeviceId(): Promise<string> {
  try {
    const raw = await readFile(PATH, "utf8")
    const trimmed = raw.trim()
    if (trimmed) return trimmed
  } catch (err: any) {
    if (err?.code !== "ENOENT") throw err
  }
  const id = crypto.randomUUID().replace(/-/g, "")
  await mkdir(dirname(PATH), { recursive: true })
  await writeFile(PATH, id, { encoding: "utf8", mode: 0o600 })
  try {
    await chmod(PATH, 0o600)
  } catch {}
  return id
}
