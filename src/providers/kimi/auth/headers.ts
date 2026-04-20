import { hostname, release, arch, platform } from "node:os"
import { KIMI_CLI_VERSION } from "./constants.ts"
import { getDeviceId } from "./device-id.ts"

function deviceModel(): string {
  const a = arch()
  const p = platform()
  if (p === "darwin") return `macOS ${release()} ${a}`.trim()
  if (p === "win32") return `Windows ${release()} ${a}`.trim()
  return `${p} ${release()} ${a}`.trim()
}

function asciiOnly(value: string, fallback = "unknown"): string {
  // Strip non-ASCII, which would be rejected as an HTTP header value.
  const cleaned = value.replace(/[^\x20-\x7e]/g, "").trim()
  return cleaned || fallback
}

export async function commonHeaders(): Promise<Record<string, string>> {
  const deviceId = await getDeviceId()
  return {
    "X-Msh-Platform": "kimi_cli",
    "X-Msh-Version": KIMI_CLI_VERSION,
    "X-Msh-Device-Name": asciiOnly(hostname()),
    "X-Msh-Device-Model": asciiOnly(deviceModel()),
    "X-Msh-Os-Version": asciiOnly(release()),
    "X-Msh-Device-Id": deviceId,
    "User-Agent": `KimiCLI/${KIMI_CLI_VERSION}`,
  }
}
