import { mkdir, readFile, writeFile, chmod } from "node:fs/promises"
import { dirname, join } from "node:path"
import { kimiDeviceIdFile, legacyConfigDir } from "../../../paths.ts"

function path(): string {
  return kimiDeviceIdFile()
}
function legacyPath(): string {
  return join(legacyConfigDir(), "kimi", "device_id")
}

export async function getDeviceId(): Promise<string> {
  for (const candidate of [path(), legacyPath()]) {
    try {
      const raw = await readFile(candidate, "utf8")
      const trimmed = raw.trim()
      if (trimmed) return trimmed
    } catch (err: any) {
      if (err?.code !== "ENOENT") throw err
    }
  }
  const id = crypto.randomUUID().replace(/-/g, "")
  const target = path()
  await mkdir(dirname(target), { recursive: true })
  await writeFile(target, id, { encoding: "utf8", mode: 0o600 })
  try {
    await chmod(target, 0o600)
  } catch {}
  return id
}
