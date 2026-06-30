import { readFileSync } from "node:fs";
import { join } from "node:path";
import { configDir } from "./paths.ts";

// Config precedence per setting:
//   provider-specific env > generic-fallback env (where one exists) > config.json > default
//
// The config file is parsed once on first access and cached. Empty strings
// from either env or the file are treated as "unset" so they fall through
// to the next layer (matches existing CCP_CODEX_MODEL behavior).

export type AliasProvider = "codex" | "kimi";
export type CodexTransport = "http" | "websocket" | "auto";

export interface FileConfig {
  port?: number;
  aliasProvider?: AliasProvider;
  codex?: {
    originator?: string;
    userAgent?: string;
    model?: string;
    effort?: string;
    reasoningSummary?: string;
    serviceTier?: string;
    baseUrl?: string;
    transport?: CodexTransport;
    previousResponseId?: boolean;
  };
  kimi?: {
    userAgent?: string;
    oauthHost?: string;
    baseUrl?: string;
  };
  cursor?: {
    baseUrl?: string;
    clientVersion?: string;
    agentBundle?: string;
  };
  log?: {
    stderr?: boolean;
    verbose?: boolean;
  };
}

export interface LoadedConfig {
  file: FileConfig;
  env: NodeJS.ProcessEnv;
}

interface LoadOptions {
  configPath?: string;
  env?: NodeJS.ProcessEnv;
  forceReload?: boolean;
}

let cached: LoadedConfig | undefined;

export function configPath(configDirectory = configDir()): string {
  return join(configDirectory, "config.json");
}

// Most env-var consumers historically used `??` semantics — empty string is
// a real value that wins. Only CCP_CODEX_MODEL and CCP_CODEX_EFFORT had
// explicit empty-string-as-unset handling in the legacy code, so only those
// getters use emptyOrUnset.
function emptyOrUnset(v: string | undefined): string | undefined {
  return v === undefined || v === "" ? undefined : v;
}

function warnInvalid(key: string, expected: string, got: unknown): void {
  process.stderr.write(
    `claude-code-proxy: ignoring config.json key "${key}": expected ${expected}, got ${typeof got}\n`,
  );
}

function parseAliasProvider(key: string, value: unknown): AliasProvider | undefined {
  if (value === undefined) return undefined;
  if (value === "codex" || value === "kimi") return value;
  warnInvalid(key, '"codex" or "kimi"', value);
  return undefined;
}

function parseCodexTransport(key: string, value: unknown): CodexTransport | undefined {
  if (value === undefined) return undefined;
  if (value === "http" || value === "websocket" || value === "auto") return value;
  warnInvalid(key, '"http", "websocket", or "auto"', value);
  return undefined;
}

function parseBoolean(key: string, value: unknown): boolean | undefined {
  if (value === undefined) return undefined;
  if (typeof value === "boolean") return value;
  warnInvalid(key, "boolean", value);
  return undefined;
}

function parseBooleanEnv(value: string | undefined): boolean | undefined {
  const normalized = emptyOrUnset(value)?.toLowerCase();
  if (normalized === undefined) return undefined;
  return normalized === "1" || normalized === "true" || normalized === "yes";
}

function validate(raw: unknown): FileConfig {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return {};
  const r = raw as Record<string, unknown>;
  const out: FileConfig = {};

  if (r.port !== undefined) {
    if (typeof r.port === "number" && Number.isFinite(r.port)) out.port = r.port;
    else warnInvalid("port", "number", r.port);
  }

  out.aliasProvider = parseAliasProvider("aliasProvider", r.aliasProvider);

  const validateStringSection = <K extends "codex" | "kimi" | "cursor" | "log">(
    key: K,
    keys: ReadonlyArray<keyof NonNullable<FileConfig[K]>>,
    types: Record<string, "string" | "boolean">,
  ): NonNullable<FileConfig[K]> | undefined => {
    if (r[key] === undefined) return undefined;
    const sec = r[key];
    if (!sec || typeof sec !== "object" || Array.isArray(sec)) {
      warnInvalid(key, "object", sec);
      return undefined;
    }
    const acc: Record<string, unknown> = {};
    for (const k of keys) {
      const v = (sec as Record<string, unknown>)[k as string];
      if (v === undefined) continue;
      const expected = types[k as string];
      if (expected && typeof v === expected) acc[k as string] = v;
      else warnInvalid(`${key}.${String(k)}`, expected ?? "unknown", v);
    }
    return acc as NonNullable<FileConfig[K]>;
  };

  const codex = validateStringSection(
    "codex",
    [
      "originator",
      "userAgent",
      "model",
      "effort",
      "reasoningSummary",
      "serviceTier",
      "baseUrl",
      "transport",
      "previousResponseId",
    ],
    {
      originator: "string",
      userAgent: "string",
      model: "string",
      effort: "string",
      reasoningSummary: "string",
      serviceTier: "string",
      baseUrl: "string",
      transport: "string",
      previousResponseId: "boolean",
    },
  );
  if (codex) {
    codex.transport = parseCodexTransport("codex.transport", codex.transport);
    codex.previousResponseId = parseBoolean("codex.previousResponseId", codex.previousResponseId);
    out.codex = codex;
  }

  const kimi = validateStringSection("kimi", ["userAgent", "oauthHost", "baseUrl"], {
    userAgent: "string",
    oauthHost: "string",
    baseUrl: "string",
  });
  if (kimi) out.kimi = kimi;

  const cursor = validateStringSection("cursor", ["baseUrl", "clientVersion", "agentBundle"], {
    baseUrl: "string",
    clientVersion: "string",
    agentBundle: "string",
  });
  if (cursor) out.cursor = cursor;

  const log = validateStringSection("log", ["stderr", "verbose"], {
    stderr: "boolean",
    verbose: "boolean",
  });
  if (log) out.log = log;

  return out;
}

export function loadConfig(opts: LoadOptions = {}): LoadedConfig {
  if (cached && !opts.forceReload && !opts.configPath && !opts.env) {
    return cached;
  }
  const env = opts.env ?? process.env;
  const path = opts.configPath ?? configPath();
  let file: FileConfig = {};
  try {
    const raw = readFileSync(path, "utf8");
    try {
      file = validate(JSON.parse(raw));
    } catch (err) {
      process.stderr.write(
        `claude-code-proxy: failed to parse ${path} (${(err as Error).message}); using defaults\n`,
      );
    }
  } catch (err: unknown) {
    if ((err as NodeJS.ErrnoException).code !== "ENOENT") {
      process.stderr.write(
        `claude-code-proxy: failed to read ${path} (${(err as Error).message}); using defaults\n`,
      );
    }
  }
  const result: LoadedConfig = { file, env };
  // Always update the cache when forceReload is requested (lets tests
  // install a custom env+path under the same singleton other modules read).
  if (opts.forceReload || (!opts.configPath && !opts.env)) cached = result;
  return result;
}

export function getConfig(): LoadedConfig {
  return cached ?? loadConfig();
}

// Per-setting getters. Each encodes its precedence chain explicitly.

// Preserves legacy `Number(process.env.PORT ?? 18765)` semantics: an env-set
// PORT of empty string parsed to NaN under the old code (effectively broken),
// so we treat it as unset rather than returning NaN.
export function port(): number {
  const c = getConfig();
  const envPort = c.env.PORT;
  if (envPort !== undefined && envPort !== "") {
    const n = Number(envPort);
    if (Number.isFinite(n)) return n;
  }
  return c.file.port ?? 18765;
}

export function codexOriginator(defaultValue: string): string {
  const c = getConfig();
  return (
    c.env.CCP_CODEX_ORIGINATOR ?? c.env.CCP_ORIGINATOR ?? c.file.codex?.originator ?? defaultValue
  );
}

export function codexUserAgent(defaultValue: string): string {
  const c = getConfig();
  return (
    c.env.CCP_CODEX_USER_AGENT ?? c.env.CCP_USER_AGENT ?? c.file.codex?.userAgent ?? defaultValue
  );
}

// Returns undefined when neither env nor file specifies a value. Empty
// string in env is intentionally treated as "unset" (preserves the
// long-standing CCP_CODEX_MODEL escape hatch).
export function codexModel(): string | undefined {
  const c = getConfig();
  return emptyOrUnset(c.env.CCP_CODEX_MODEL) ?? emptyOrUnset(c.file.codex?.model);
}

export function codexEffort(): string | undefined {
  const c = getConfig();
  return emptyOrUnset(c.env.CCP_CODEX_EFFORT) ?? emptyOrUnset(c.file.codex?.effort);
}

export function codexReasoningSummary(): string | undefined {
  const c = getConfig();
  return (
    emptyOrUnset(c.env.CCP_CODEX_REASONING_SUMMARY) ?? emptyOrUnset(c.file.codex?.reasoningSummary)
  );
}

export function codexServiceTier(): string | undefined {
  const c = getConfig();
  return emptyOrUnset(c.env.CCP_CODEX_SERVICE_TIER) ?? emptyOrUnset(c.file.codex?.serviceTier);
}

export function codexBaseUrl(defaultValue: string): string {
  const c = getConfig();
  return c.env.CCP_CODEX_BASE_URL ?? c.file.codex?.baseUrl ?? defaultValue;
}

export function codexTransport(): CodexTransport {
  return resolvedCodexTransport(getConfig());
}

export function codexPreviousResponseId(): boolean {
  return resolvedCodexPreviousResponseId(getConfig());
}

export function aliasProvider(): AliasProvider {
  return resolvedAliasProvider(getConfig());
}

export function kimiUserAgent(defaultValue: string): string {
  const c = getConfig();
  return (
    c.env.CCP_KIMI_USER_AGENT ?? c.env.CCP_USER_AGENT ?? c.file.kimi?.userAgent ?? defaultValue
  );
}

export function kimiOauthHost(): string {
  const c = getConfig();
  return c.env.CCP_KIMI_OAUTH_HOST ?? c.file.kimi?.oauthHost ?? "https://auth.kimi.com";
}

export function kimiBaseUrl(): string {
  const c = getConfig();
  return c.env.CCP_KIMI_BASE_URL ?? c.file.kimi?.baseUrl ?? "https://api.kimi.com/coding/v1";
}

export function cursorBaseUrl(): string {
  const c = getConfig();
  return c.env.CCP_CURSOR_BASE_URL ?? c.file.cursor?.baseUrl ?? "https://api2.cursor.sh";
}

export function cursorClientVersion(): string {
  const c = getConfig();
  return (
    c.env.CCP_CURSOR_CLIENT_VERSION ?? c.file.cursor?.clientVersion ?? "cli-2026.06.04-5fd875e"
  );
}

function resolvedCodexTransport(c: LoadedConfig): CodexTransport {
  return (
    parseCodexTransport("CCP_CODEX_TRANSPORT", emptyOrUnset(c.env.CCP_CODEX_TRANSPORT)) ??
    c.file.codex?.transport ??
    "websocket"
  );
}

function resolvedCodexPreviousResponseId(c: LoadedConfig): boolean {
  return (
    parseBooleanEnv(c.env.CCP_CODEX_PREVIOUS_RESPONSE_ID) ??
    c.file.codex?.previousResponseId ??
    false
  );
}

function resolvedAliasProvider(c: LoadedConfig): AliasProvider {
  return (
    parseAliasProvider("CCP_ALIAS_PROVIDER", emptyOrUnset(c.env.CCP_ALIAS_PROVIDER)) ??
    c.file.aliasProvider ??
    "codex"
  );
}

function resolvedCursorBaseUrl(c: LoadedConfig): string {
  return c.env.CCP_CURSOR_BASE_URL ?? c.file.cursor?.baseUrl ?? "https://api2.cursor.sh";
}

function resolvedCursorClientVersion(c: LoadedConfig): string {
  return (
    c.env.CCP_CURSOR_CLIENT_VERSION ?? c.file.cursor?.clientVersion ?? "cli-2026.06.04-5fd875e"
  );
}

export function configOverrideSummaryLines(cfg: LoadedConfig = getConfig()): string[] {
  const fromFile = cfg.file;
  const overrides: string[] = [];

  if (cfg.env.CCP_CODEX_ORIGINATOR) overrides.push("CCP_CODEX_ORIGINATOR (env)");
  else if (fromFile.codex?.originator) overrides.push("codex.originator (config)");

  if (cfg.env.CCP_CODEX_USER_AGENT) overrides.push("CCP_CODEX_USER_AGENT (env)");
  else if (cfg.env.CCP_USER_AGENT) overrides.push("CCP_USER_AGENT (env)");
  else if (fromFile.codex?.userAgent) overrides.push("codex.userAgent (config)");

  if (cfg.env.CCP_KIMI_USER_AGENT) overrides.push("CCP_KIMI_USER_AGENT (env)");
  else if (fromFile.kimi?.userAgent) overrides.push("kimi.userAgent (config)");

  if (cfg.env.CCP_CODEX_MODEL) overrides.push("CCP_CODEX_MODEL (env)");
  else if (fromFile.codex?.model) overrides.push("codex.model (config)");

  if (cfg.env.CCP_CODEX_EFFORT) overrides.push("CCP_CODEX_EFFORT (env)");
  else if (fromFile.codex?.effort) overrides.push("codex.effort (config)");

  if (cfg.env.CCP_CODEX_REASONING_SUMMARY) overrides.push("CCP_CODEX_REASONING_SUMMARY (env)");
  else if (fromFile.codex?.reasoningSummary) overrides.push("codex.reasoningSummary (config)");

  if (cfg.env.CCP_CODEX_SERVICE_TIER) overrides.push("CCP_CODEX_SERVICE_TIER (env)");
  else if (fromFile.codex?.serviceTier) overrides.push("codex.serviceTier (config)");

  if (cfg.env.CCP_CODEX_BASE_URL) overrides.push("CCP_CODEX_BASE_URL (env)");
  else if (fromFile.codex?.baseUrl) overrides.push("codex.baseUrl (config)");

  if (cfg.env.CCP_CODEX_TRANSPORT)
    overrides.push(`CCP_CODEX_TRANSPORT=${resolvedCodexTransport(cfg)} (env)`);
  else if (fromFile.codex?.transport)
    overrides.push(`codex.transport=${fromFile.codex.transport} (config)`);

  if (cfg.env.CCP_CODEX_PREVIOUS_RESPONSE_ID !== undefined)
    overrides.push(`CCP_CODEX_PREVIOUS_RESPONSE_ID=${resolvedCodexPreviousResponseId(cfg)} (env)`);
  else if (fromFile.codex?.previousResponseId !== undefined)
    overrides.push(`codex.previousResponseId=${fromFile.codex.previousResponseId} (config)`);

  if (cfg.env.CCP_ALIAS_PROVIDER)
    overrides.push(`CCP_ALIAS_PROVIDER=${resolvedAliasProvider(cfg)} (env)`);
  else if (fromFile.aliasProvider)
    overrides.push(`aliasProvider=${fromFile.aliasProvider} (config)`);

  if (cfg.env.CCP_LOG_VERBOSE !== undefined) overrides.push("CCP_LOG_VERBOSE (env)");
  else if (fromFile.log?.verbose) overrides.push("log.verbose (config)");

  if (cfg.env.CCP_LOG_STDERR !== undefined) overrides.push("CCP_LOG_STDERR (env)");
  else if (fromFile.log?.stderr) overrides.push("log.stderr (config)");

  if (cfg.env.CCP_KIMI_OAUTH_HOST) overrides.push("CCP_KIMI_OAUTH_HOST (env)");
  else if (fromFile.kimi?.oauthHost) overrides.push("kimi.oauthHost (config)");

  if (cfg.env.CCP_KIMI_BASE_URL) overrides.push("CCP_KIMI_BASE_URL (env)");
  else if (fromFile.kimi?.baseUrl) overrides.push("kimi.baseUrl (config)");

  if (cfg.env.CCP_CURSOR_BASE_URL)
    overrides.push(`CCP_CURSOR_BASE_URL=${resolvedCursorBaseUrl(cfg)} (env)`);
  else if (fromFile.cursor?.baseUrl) overrides.push("cursor.baseUrl (config)");

  if (cfg.env.CCP_CURSOR_CLIENT_VERSION)
    overrides.push(`CCP_CURSOR_CLIENT_VERSION=${resolvedCursorClientVersion(cfg)} (env)`);
  else if (fromFile.cursor?.clientVersion) overrides.push("cursor.clientVersion (config)");

  if (cfg.env.CCP_CURSOR_AGENT_BUNDLE) overrides.push("CCP_CURSOR_AGENT_BUNDLE (env)");
  else if (fromFile.cursor?.agentBundle) overrides.push("cursor.agentBundle (config)");

  return overrides;
}

export function cursorAgentBundle(): string | undefined {
  const c = getConfig();
  return emptyOrUnset(c.env.CCP_CURSOR_AGENT_BUNDLE) ?? emptyOrUnset(c.file.cursor?.agentBundle);
}

// Additive: error/warn always go to stderr in log.ts; this getter only
// controls whether *all* levels are also mirrored to stderr. Matches the
// pre-existing `!!process.env.CCP_LOG_STDERR` semantics where any value
// (including the empty string) enables it.
export function logStderr(): boolean {
  const c = getConfig();
  if (c.env.CCP_LOG_STDERR !== undefined) return true;
  return c.file.log?.stderr ?? false;
}

export function logVerbose(): boolean {
  const c = getConfig();
  if (c.env.CCP_LOG_VERBOSE !== undefined) return true;
  return c.file.log?.verbose ?? false;
}
