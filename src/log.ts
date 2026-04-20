import { mkdir, appendFile, stat, rename } from "node:fs/promises"
import { createWriteStream, type WriteStream } from "node:fs"
import { join } from "node:path"
import { homedir } from "node:os"

const MAX_LOG_BYTES = 20 * 1024 * 1024 // 20 MiB
const REDACT_KEYS = new Set([
  "authorization",
  "Authorization",
  "access",
  "access_token",
  "refresh",
  "refresh_token",
  "id_token",
  "code",
  "code_verifier",
  "ChatGPT-Account-Id",
  "chatgpt-account-id",
  "x-api-key",
])

export function stateDir(): string {
  const base = process.env.XDG_STATE_HOME || join(homedir(), ".local", "state")
  return join(base, "claude-code-proxy")
}

export function logDir(): string {
  return stateDir()
}

let stream: WriteStream | undefined
let rotating: Promise<void> | undefined

async function ensureStream(): Promise<WriteStream> {
  if (stream) return stream
  const dir = stateDir()
  await mkdir(dir, { recursive: true })
  const file = join(dir, "proxy.log")
  stream = createWriteStream(file, { flags: "a" })
  return stream
}

async function maybeRotate(): Promise<void> {
  if (rotating) return rotating
  rotating = (async () => {
    try {
      const dir = stateDir()
      const file = join(dir, "proxy.log")
      const s = await stat(file).catch(() => undefined)
      if (!s || s.size < MAX_LOG_BYTES) return
      const rotated = join(dir, `proxy.log.${Date.now()}`)
      if (stream) {
        stream.end()
        stream = undefined
      }
      await rename(file, rotated).catch(() => {})
    } catch {
      // Never propagate rotation errors — logging must never crash the proxy.
    } finally {
      rotating = undefined
    }
  })()
  return rotating
}

const VERBOSE = !!process.env.CCP_LOG_VERBOSE

function redact(value: unknown, depth = 0): unknown {
  if (depth > 6) return "[depth-limit]"
  if (value == null) return value
  if (typeof value === "string") {
    if (!VERBOSE && value.length > 4000) return value.slice(0, 4000) + `…[${value.length - 4000} more]`
    return value
  }
  if (typeof value !== "object") return value
  if (Array.isArray(value)) return value.map((v) => redact(v, depth + 1))
  const out: Record<string, unknown> = {}
  for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
    if (REDACT_KEYS.has(k)) {
      out[k] = typeof v === "string" ? `[redacted len=${v.length}]` : "[redacted]"
    } else {
      out[k] = redact(v, depth + 1)
    }
  }
  return out
}

type Level = "debug" | "info" | "warn" | "error"

async function write(level: Level, service: string, msg: string, fields?: Record<string, unknown>): Promise<void> {
  const line = JSON.stringify({
    t: new Date().toISOString(),
    level,
    service,
    msg,
    ...(fields ? { fields: redact(fields) as Record<string, unknown> } : {}),
  })
  try {
    const s = await ensureStream()
    s.write(line + "\n")
    maybeRotate().catch(() => {})
  } catch {
    // swallow; also print to stderr for visibility
  }
  if (level === "error" || level === "warn" || process.env.CCP_LOG_STDERR) {
    process.stderr.write(line + "\n")
  }
}

export interface Logger {
  debug(msg: string, fields?: Record<string, unknown>): void
  info(msg: string, fields?: Record<string, unknown>): void
  warn(msg: string, fields?: Record<string, unknown>): void
  error(msg: string, fields?: Record<string, unknown>): void
  child(bindings: Record<string, unknown>): Logger
}

export function createLogger(
  service: string,
  baseFields: Record<string, unknown> = {},
): Logger {
  const merge = (f?: Record<string, unknown>) =>
    f ? { ...baseFields, ...f } : baseFields
  return {
    debug: (msg, fields) => void write("debug", service, msg, merge(fields)),
    info: (msg, fields) => void write("info", service, msg, merge(fields)),
    warn: (msg, fields) => void write("warn", service, msg, merge(fields)),
    error: (msg, fields) => void write("error", service, msg, merge(fields)),
    child: (bindings) => createLogger(service, { ...baseFields, ...bindings }),
  }
}
