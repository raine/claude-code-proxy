import { readFileSync } from "node:fs"
import { join } from "node:path"
import { configDir } from "./paths.ts"

// Config precedence per setting:
//   provider-specific env > generic-fallback env (where one exists) > config.json > default
//
// The config file is parsed once on first access and cached. Empty strings
// from either env or the file are treated as "unset" so they fall through
// to the next layer (matches existing CCP_CODEX_MODEL behavior).

export interface FileConfig {
  port?: number
  routing?: {
    claudeAliasProvider?: string
  }
  codex?: {
    originator?: string
    userAgent?: string
    model?: string
    effort?: string
    defaultEffort?: string
  }
  gemini?: {
    model?: string
    smallFastModel?: string
    oauthCredsPath?: string
    enableFallback?: boolean
    enableGoogleOneCredits?: boolean
    defaultEffort?: string
    endpoint?: string
    apiVersion?: string
  }
  kimi?: {
    userAgent?: string
    oauthHost?: string
    baseUrl?: string
  }
  log?: {
    stderr?: boolean
    verbose?: boolean
  }
}

interface LoadedConfig {
  file: FileConfig
  env: NodeJS.ProcessEnv
}

interface LoadOptions {
  configPath?: string
  env?: NodeJS.ProcessEnv
  forceReload?: boolean
}

let cached: LoadedConfig | undefined

// Most env-var consumers historically used `??` semantics — empty string is
// a real value that wins. Only CCP_CODEX_MODEL and CCP_CODEX_EFFORT had
// explicit empty-string-as-unset handling in the legacy code, so only those
// getters use emptyOrUnset.
function emptyOrUnset(v: string | undefined): string | undefined {
  return v === undefined || v === "" ? undefined : v
}

function warnInvalid(key: string, expected: string, got: unknown): void {
  process.stderr.write(
    `claude-code-proxy: ignoring config.json key "${key}": expected ${expected}, got ${typeof got}\n`,
  )
}

function validate(raw: unknown): FileConfig {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return {}
  const r = raw as Record<string, unknown>
  const out: FileConfig = {}

  if (r.port !== undefined) {
    if (typeof r.port === "number" && Number.isFinite(r.port)) out.port = r.port
    else warnInvalid("port", "number", r.port)
  }

  const validateStringSection = <K extends "routing" | "codex" | "gemini" | "kimi" | "log">(
    key: K,
    keys: ReadonlyArray<keyof NonNullable<FileConfig[K]>>,
    types: Record<string, "string" | "boolean">,
  ): NonNullable<FileConfig[K]> | undefined => {
    if (r[key] === undefined) return undefined
    const sec = r[key]
    if (!sec || typeof sec !== "object" || Array.isArray(sec)) {
      warnInvalid(key, "object", sec)
      return undefined
    }
    const acc: Record<string, unknown> = {}
    for (const k of keys) {
      const v = (sec as Record<string, unknown>)[k as string]
      if (v === undefined) continue
      const expected = types[k as string]
      if (expected && typeof v === expected) acc[k as string] = v
      else warnInvalid(`${key}.${String(k)}`, expected ?? "unknown", v)
    }
    return acc as NonNullable<FileConfig[K]>
  }

  const routing = validateStringSection("routing", ["claudeAliasProvider"], {
    claudeAliasProvider: "string",
  })
  if (routing) out.routing = routing

  const codex = validateStringSection("codex", ["originator", "userAgent", "model", "effort", "defaultEffort"], {
    originator: "string",
    userAgent: "string",
    model: "string",
    effort: "string",
    defaultEffort: "string",
  })
  if (codex) out.codex = codex

  const gemini = validateStringSection(
    "gemini",
    [
      "model",
      "smallFastModel",
      "oauthCredsPath",
      "enableFallback",
      "enableGoogleOneCredits",
      "defaultEffort",
      "endpoint",
      "apiVersion",
    ],
    {
      model: "string",
      smallFastModel: "string",
      oauthCredsPath: "string",
      enableFallback: "boolean",
      enableGoogleOneCredits: "boolean",
      defaultEffort: "string",
      endpoint: "string",
      apiVersion: "string",
    },
  )
  if (gemini) out.gemini = gemini

  const kimi = validateStringSection("kimi", ["userAgent", "oauthHost", "baseUrl"], {
    userAgent: "string",
    oauthHost: "string",
    baseUrl: "string",
  })
  if (kimi) out.kimi = kimi

  const log = validateStringSection("log", ["stderr", "verbose"], {
    stderr: "boolean",
    verbose: "boolean",
  })
  if (log) out.log = log

  return out
}

export function loadConfig(opts: LoadOptions = {}): LoadedConfig {
  if (cached && !opts.forceReload && !opts.configPath && !opts.env) {
    return cached
  }
  const env = opts.env ?? process.env
  const path = opts.configPath ?? join(configDir(), "config.json")
  let file: FileConfig = {}
  try {
    const raw = readFileSync(path, "utf8")
    try {
      file = validate(JSON.parse(raw))
    } catch (err) {
      process.stderr.write(
        `claude-code-proxy: failed to parse ${path} (${(err as Error).message}); using defaults\n`,
      )
    }
  } catch (err: unknown) {
    if ((err as NodeJS.ErrnoException).code !== "ENOENT") {
      process.stderr.write(
        `claude-code-proxy: failed to read ${path} (${(err as Error).message}); using defaults\n`,
      )
    }
  }
  const result: LoadedConfig = { file, env }
  // Always update the cache when forceReload is requested (lets tests
  // install a custom env+path under the same singleton other modules read).
  if (opts.forceReload || (!opts.configPath && !opts.env)) cached = result
  return result
}

export function getConfig(): LoadedConfig {
  return cached ?? loadConfig()
}

// Per-setting getters. Each encodes its precedence chain explicitly.

// Preserves legacy `Number(process.env.PORT ?? 18765)` semantics: an env-set
// PORT of empty string parsed to NaN under the old code (effectively broken),
// so we treat it as unset rather than returning NaN.
export function port(): number {
  const c = getConfig()
  const envPort = c.env.PORT
  if (envPort !== undefined && envPort !== "") {
    const n = Number(envPort)
    if (Number.isFinite(n)) return n
  }
  return c.file.port ?? 18765
}

export type ClaudeAliasProvider = "codex" | "gemini" | "kimi" | "none"

const CLAUDE_ALIAS_PROVIDERS = new Set<ClaudeAliasProvider>([
  "codex",
  "gemini",
  "kimi",
  "none",
])

export function claudeAliasProvider(): ClaudeAliasProvider {
  const c = getConfig()
  const raw = emptyOrUnset(c.env.CCP_CLAUDE_ALIAS_PROVIDER) ??
    emptyOrUnset(c.file.routing?.claudeAliasProvider) ??
    "none"
  if (CLAUDE_ALIAS_PROVIDERS.has(raw as ClaudeAliasProvider)) {
    return raw as ClaudeAliasProvider
  }
  process.stderr.write(
    `claude-code-proxy: ignoring invalid Claude alias provider "${raw}"; expected one of ${Array.from(CLAUDE_ALIAS_PROVIDERS).join(", ")}\n`,
  )
  return "none"
}

export function codexOriginator(defaultValue: string): string {
  const c = getConfig()
  return (
    c.env.CCP_CODEX_ORIGINATOR ??
    c.env.CCP_ORIGINATOR ??
    c.file.codex?.originator ??
    defaultValue
  )
}

export function codexUserAgent(defaultValue: string): string {
  const c = getConfig()
  return (
    c.env.CCP_CODEX_USER_AGENT ??
    c.env.CCP_USER_AGENT ??
    c.file.codex?.userAgent ??
    defaultValue
  )
}

// Returns undefined when neither env nor file specifies a value. Empty
// string in env is intentionally treated as "unset" (preserves the
// long-standing model/effort escape hatches).
export function codexModel(): string | undefined {
  const c = getConfig()
  return emptyOrUnset(c.env.CCP_CODEX_MODEL) ?? emptyOrUnset(c.file.codex?.model)
}

export function codexEffort(): string | undefined {
  const c = getConfig()
  return emptyOrUnset(c.env.CCP_CODEX_EFFORT) ?? emptyOrUnset(c.file.codex?.effort)
}

export function codexDefaultEffort(): string | undefined {
  const c = getConfig()
  return emptyOrUnset(c.env.CCP_CODEX_DEFAULT_EFFORT) ?? emptyOrUnset(c.file.codex?.defaultEffort)
}

export function geminiModel(): string | undefined {
  const c = getConfig()
  return emptyOrUnset(c.env.CCP_GEMINI_MODEL) ?? emptyOrUnset(c.file.gemini?.model)
}

export function geminiSmallFastModel(): string | undefined {
  const c = getConfig()
  return emptyOrUnset(c.env.CCP_GEMINI_SMALL_FAST_MODEL) ??
    emptyOrUnset(c.file.gemini?.smallFastModel)
}

export function geminiOauthCredsPath(): string | undefined {
  const c = getConfig()
  return emptyOrUnset(c.env.CCP_GEMINI_OAUTH_CREDS_PATH) ??
    emptyOrUnset(c.file.gemini?.oauthCredsPath)
}

export function geminiEnableFallback(): boolean {
  const c = getConfig()
  const raw = emptyOrUnset(c.env.CCP_GEMINI_ENABLE_FALLBACK)
  if (raw !== undefined) return !["0", "false", "no", "off"].includes(raw.toLowerCase())
  return c.file.gemini?.enableFallback ?? true
}

export function geminiEnableGoogleOneCredits(): boolean {
  const c = getConfig()
  const raw = emptyOrUnset(c.env.CCP_GEMINI_ENABLE_GOOGLE_ONE_CREDITS)
  if (raw !== undefined) return !["0", "false", "no", "off"].includes(raw.toLowerCase())
  return c.file.gemini?.enableGoogleOneCredits ?? false
}

export function geminiDefaultEffort(): string | undefined {
  const c = getConfig()
  return emptyOrUnset(c.env.CCP_GEMINI_DEFAULT_EFFORT) ??
    emptyOrUnset(c.file.gemini?.defaultEffort)
}

export function geminiEndpoint(): string {
  const c = getConfig()
  return c.env.CCP_GEMINI_CODE_ASSIST_ENDPOINT ??
    c.file.gemini?.endpoint ??
    "https://cloudcode-pa.googleapis.com"
}

export function geminiApiVersion(): string {
  const c = getConfig()
  return c.env.CCP_GEMINI_CODE_ASSIST_API_VERSION ??
    c.file.gemini?.apiVersion ??
    "v1internal"
}

export function kimiUserAgent(defaultValue: string): string {
  const c = getConfig()
  return (
    c.env.CCP_KIMI_USER_AGENT ??
    c.env.CCP_USER_AGENT ??
    c.file.kimi?.userAgent ??
    defaultValue
  )
}

export function kimiOauthHost(): string {
  const c = getConfig()
  return c.env.CCP_KIMI_OAUTH_HOST ?? c.file.kimi?.oauthHost ?? "https://auth.kimi.com"
}

export function kimiBaseUrl(): string {
  const c = getConfig()
  return c.env.CCP_KIMI_BASE_URL ?? c.file.kimi?.baseUrl ?? "https://api.kimi.com/coding/v1"
}

// Additive: error/warn always go to stderr in log.ts; this getter only
// controls whether *all* levels are also mirrored to stderr. Matches the
// pre-existing `!!process.env.CCP_LOG_STDERR` semantics where any value
// (including the empty string) enables it.
export function logStderr(): boolean {
  const c = getConfig()
  if (c.env.CCP_LOG_STDERR !== undefined) return true
  return c.file.log?.stderr ?? false
}

export function logVerbose(): boolean {
  const c = getConfig()
  if (c.env.CCP_LOG_VERBOSE !== undefined) return true
  return c.file.log?.verbose ?? false
}
