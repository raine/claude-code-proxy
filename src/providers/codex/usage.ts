import { codexOriginator, codexUserAgent } from "../../config.ts"
import type { Logger } from "../../log.ts"
import { CODEX_USAGE_ENDPOINT, ORIGINATOR as ORIGINATOR_DEFAULT } from "./auth/constants.ts"
import { forceRefresh, getAuth } from "./auth/manager.ts"

declare const BUILD_VERSION: string | undefined
const PROXY_VERSION = typeof BUILD_VERSION === "string" ? BUILD_VERSION : "dev"
const CACHE_TTL_MS = 120_000
const FETCH_TIMEOUT_MS = 10_000

export interface CodexUsageWindow {
  used_percent: number
  window_minutes: number | null
  resets_at: number | null
}

export interface CodexUsageSnapshot {
  provider: "codex"
  updated_at: number
  stale: boolean
  plan_type: string | null
  rate_limit_reached: boolean | null
  rate_limits: {
    primary: CodexUsageWindow | null
    secondary: CodexUsageWindow | null
  }
}

interface CacheEntry {
  snapshot: CodexUsageSnapshot
  fetchedAtMs: number
}

let cached: CacheEntry | undefined
let inflight: Promise<CodexUsageSnapshot> | undefined

export async function getCodexUsage(
  log: Logger,
  opts: { signal?: AbortSignal } = {},
): Promise<CodexUsageSnapshot> {
  const now = Date.now()
  if (cached && now - cached.fetchedAtMs < CACHE_TTL_MS) {
    return withStale(cached.snapshot, false)
  }

  if (cached) {
    void refreshCodexUsage(log).catch((err) => {
      log.warn("background usage refresh failed", { err: String(err) })
    })
    return withStale(cached.snapshot, true)
  }

  return refreshCodexUsage(log, opts.signal)
}

async function refreshCodexUsage(log: Logger, signal?: AbortSignal): Promise<CodexUsageSnapshot> {
  if (inflight) return inflight
  inflight = fetchCodexUsage(log, signal)
    .then((snapshot) => {
      cached = { snapshot: withStale(snapshot, false), fetchedAtMs: Date.now() }
      return withStale(snapshot, false)
    })
    .finally(() => {
      inflight = undefined
    })
  return inflight
}

async function fetchCodexUsage(log: Logger, signal?: AbortSignal): Promise<CodexUsageSnapshot> {
  let auth = await getAuth()
  let resp = await doFetchUsage(auth.access, auth.accountId, signal)

  if (resp.status === 401) {
    log.warn("usage endpoint got 401, refreshing token", {})
    auth = await forceRefresh()
    resp = await doFetchUsage(auth.access, auth.accountId, signal)
  }

  if (!resp.ok) {
    const text = await safeText(resp)
    throw new CodexUsageError(resp.status, "Codex usage request failed", text)
  }

  const raw = await resp.json()
  return normalizeCodexUsage(raw)
}

async function doFetchUsage(
  accessToken: string,
  accountId: string | undefined,
  signal?: AbortSignal,
): Promise<Response> {
  const controller = new AbortController()
  let timedOut = false
  const timeout = setTimeout(() => {
    timedOut = true
    controller.abort()
  }, FETCH_TIMEOUT_MS)
  const abort = () => controller.abort()
  if (signal?.aborted) controller.abort()
  else signal?.addEventListener("abort", abort, { once: true })

  const headers = new Headers({
    accept: "application/json",
    authorization: `Bearer ${accessToken}`,
    originator: codexOriginator(ORIGINATOR_DEFAULT),
    "openai-beta": "responses=experimental",
  })
  const userAgent = codexUserAgent(`claude-code-proxy/${PROXY_VERSION}`)
  if (userAgent) headers.set("User-Agent", userAgent)
  if (accountId) headers.set("ChatGPT-Account-Id", accountId)

  try {
    return await fetch(CODEX_USAGE_ENDPOINT, { headers, signal: controller.signal })
  } catch (err) {
    if (timedOut) throw new CodexUsageError(504, "Codex usage request timed out")
    throw err
  } finally {
    clearTimeout(timeout)
    signal?.removeEventListener("abort", abort)
  }
}

export function normalizeCodexUsage(raw: unknown): CodexUsageSnapshot {
  const root = asRecord(raw)
  const rateLimit = asRecord(root.rate_limit)
  const nowSeconds = Math.floor(Date.now() / 1000)
  return {
    provider: "codex",
    updated_at: nowSeconds,
    stale: false,
    plan_type: stringOrNull(root.plan_type),
    rate_limit_reached: booleanOrNull(rateLimit.limit_reached),
    rate_limits: {
      primary: normalizeWindow(rateLimit.primary_window, nowSeconds),
      secondary: normalizeWindow(rateLimit.secondary_window, nowSeconds),
    },
  }
}

function normalizeWindow(raw: unknown, nowSeconds: number): CodexUsageWindow | null {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null
  const window = raw as Record<string, unknown>
  const usedPercent = numberOrNull(window.used_percent)
  const windowSeconds = numberOrNull(window.limit_window_seconds)
  const resetAt = numberOrNull(window.reset_at)
  const resetAfter = numberOrNull(window.reset_after_seconds)
  return {
    used_percent: usedPercent ?? 0,
    window_minutes: windowSeconds === null ? null : Math.round(windowSeconds / 60),
    resets_at: resetAt ?? (resetAfter === null ? null : nowSeconds + resetAfter),
  }
}

function withStale(snapshot: CodexUsageSnapshot, stale: boolean): CodexUsageSnapshot {
  return { ...snapshot, stale }
}

function asRecord(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) return {}
  return value as Record<string, unknown>
}

function numberOrNull(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null
}

function booleanOrNull(value: unknown): boolean | null {
  return typeof value === "boolean" ? value : null
}

function stringOrNull(value: unknown): string | null {
  return typeof value === "string" ? value : null
}

async function safeText(resp: Response): Promise<string> {
  try {
    return await resp.text()
  } catch {
    return ""
  }
}

export class CodexUsageError extends Error {
  constructor(
    public status: number,
    message: string,
    public detail?: string,
  ) {
    super(message)
    this.name = "CodexUsageError"
  }
}

export function resetCodexUsageCacheForTests(): void {
  cached = undefined
  inflight = undefined
}
