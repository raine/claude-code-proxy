import { CODEX_API_ENDPOINT, ORIGINATOR as ORIGINATOR_DEFAULT, USAGE_API_ENDPOINT } from "./auth/constants.ts"
import {
  codexApiKey,
  codexAuthMode,
  codexBaseUrl,
  codexOriginator,
  codexUserAgent,
} from "../../config.ts"
import { forceRefresh, getAuth } from "./auth/manager.ts"
import type { Logger } from "../../log.ts"
import type { RequestContext } from "../types.ts"
import type { ResponsesRequest } from "./translate/request.ts"
import { isTransientNetworkError, retryTransient } from "../retry.ts"
import { mapUsageSnapshot, type RateLimitsSidecarWriter } from "./rate-limits.ts"

declare const BUILD_VERSION: string | undefined
const PROXY_VERSION = typeof BUILD_VERSION === "string" ? BUILD_VERSION : "dev"

export type CodexAuthKind = "chatgpt" | "openai"

interface CodexAuthContext {
  kind: CodexAuthKind
  token: string
  accountId?: string
}

export interface CodexResponse {
  body: ReadableStream<Uint8Array>
  status: number
  headers: Headers
  accountId?: string
  rateLimitsTracker: RateLimitsTracker
}

export class RateLimitsTracker {
  private seen = false
  private readonly enabled: boolean

  constructor(opts: { enabled?: boolean } = {}) {
    this.enabled = opts.enabled ?? true
  }

  markSeen(): void {
    this.seen = true
  }

  async refreshIfNeeded(opts: {
    success: boolean
    accountId?: string
    signal?: AbortSignal
    rateLimitsWriter?: RateLimitsSidecarWriter
    log: Logger
  }): Promise<void> {
    if (!this.enabled || this.seen || !opts.success || !opts.rateLimitsWriter) return
    try {
      const snapshot = await fetchUsageSnapshot(opts.accountId, opts.signal, opts.log)
      if (snapshot) {
        await opts.rateLimitsWriter.write(snapshot)
        this.seen = true
      }
    } catch (err) {
      opts.log.warn("failed to refresh usage snapshot", { err: String(err) })
    }
  }
}

export async function postCodex(
  body: ResponsesRequest,
  ctx: RequestContext,
): Promise<CodexResponse> {
  const log = ctx.childLogger("codex.client")
  return retryTransient(() => attemptPostCodex(body, ctx, log), {
    log,
    signal: ctx.signal,
    classify: (err) => {
      if (err instanceof CodexError && err.status === 429) {
        return { retryAfter: err.meta?.retryAfter, reason: "rate_limit" }
      }
      if (isTransientNetworkError(err)) return { reason: "network" }
      return undefined
    },
  })
}

async function attemptPostCodex(
  body: ResponsesRequest,
  ctx: RequestContext,
  log: Logger,
): Promise<CodexResponse> {
  let auth = await getCodexAuth()
  const rateLimitsTracker = new RateLimitsTracker({ enabled: auth.kind === "chatgpt" })
  let resp = await doFetch(auth, body, log, ctx.signal, ctx.sessionId)

  if (resp.status === 401 && auth.kind === "chatgpt") {
    log.warn("got 401, refreshing token", {})
    try {
      auth = chatgptAuth(await forceRefresh())
      resp = await doFetch(auth, body, log, ctx.signal, ctx.sessionId)
    } catch (err) {
      log.error("refresh after 401 failed", { err: String(err) })
    }
  }

  if (resp.status === 403) {
    const text = await safeText(resp)
    log.error("403 from upstream (non-refreshable)", { body: text })
    throw new CodexError(403, "Forbidden", text)
  }

  if (resp.status === 429) {
    const retryAfter = resp.headers.get("retry-after") || undefined
    const text = await safeText(resp)
    throw new CodexError(429, "Rate limited", text, { retryAfter })
  }

  if (!resp.ok) {
    const text = await safeText(resp)
    throw new CodexError(resp.status, "Upstream error", text)
  }

  if (!resp.body) throw new CodexError(500, "Upstream returned no body")

  return {
    body: resp.body,
    status: resp.status,
    headers: resp.headers,
    accountId: auth.accountId,
    rateLimitsTracker,
  }
}

async function getCodexAuth(): Promise<CodexAuthContext> {
  const mode = codexAuthMode() ?? "auto"
  const apiKey = codexApiKey()
  if (mode === "openai") {
    if (!apiKey) throw new Error("OpenAI-compatible Codex auth selected but no API key found. Set CCP_CODEX_API_KEY or OPENAI_API_KEY.")
    return openaiAuth(apiKey)
  }
  if (mode === "auto" && apiKey && !isChatGptBackendUrl(codexBaseUrl(CODEX_API_ENDPOINT))) {
    return openaiAuth(apiKey)
  }
  return chatgptAuth(await getAuth())
}

function openaiAuth(apiKey: string): CodexAuthContext {
  return { kind: "openai", token: apiKey }
}

function chatgptAuth(auth: { access: string; accountId?: string }): CodexAuthContext {
  return { kind: "chatgpt", token: auth.access, accountId: auth.accountId }
}

function isChatGptBackendUrl(rawUrl: string): boolean {
  try {
    const url = new URL(rawUrl)
    return url.hostname === "chatgpt.com" && url.pathname.includes("/backend-api/")
  } catch {
    return false
  }
}

async function fetchUsageSnapshot(
  accountId: string | undefined,
  signal: AbortSignal | undefined,
  log: Logger,
) {
  const auth = chatgptAuth(await getAuth())
  const headers = codexHeaders(auth)
  const resp = await fetch(codexBaseUrl(USAGE_API_ENDPOINT), { headers, signal })
  if (!resp.ok) {
    log.warn("usage refresh returned non-ok", { status: resp.status })
    return null
  }
  return mapUsageSnapshot(await resp.json(), accountId ?? auth.accountId)
}

async function doFetch(
  auth: CodexAuthContext,
  body: ResponsesRequest,
  log: Logger,
  signal?: AbortSignal,
  sessionId?: string,
): Promise<Response> {
  const headers = codexHeaders(auth, sessionId)
  const codexUrl = codexBaseUrl(CODEX_API_ENDPOINT)

  log.debug("posting to codex", {
    url: codexUrl,
    authKind: auth.kind,
    model: body.model,
    inputCount: body.input.length,
    toolCount: body.tools?.length ?? 0,
  })

  return fetch(codexUrl, {
    method: "POST",
    headers,
    body: JSON.stringify(body),
    signal,
  })
}

function codexHeaders(auth: CodexAuthContext, sessionId?: string): Headers {
  const headers = new Headers({
    "Content-Type": "application/json",
    accept: "text/event-stream",
    authorization: `Bearer ${auth.token}`,
  })
  const userAgent = codexUserAgent(`claude-code-proxy/${PROXY_VERSION}`)
  if (userAgent) headers.set("User-Agent", userAgent)

  if (auth.kind === "chatgpt") {
    headers.set("originator", codexOriginator(ORIGINATOR_DEFAULT))
    headers.set("openai-beta", "responses=experimental")
    if (auth.accountId) headers.set("ChatGPT-Account-Id", auth.accountId)
    if (sessionId) {
      headers.set("session_id", sessionId)
      headers.set("x-client-request-id", sessionId)
      headers.set("x-codex-window-id", `${sessionId}:0`)
    }
  }

  return headers
}

async function safeText(resp: Response): Promise<string> {
  try {
    return await resp.text()
  } catch {
    return ""
  }
}

export class CodexError extends Error {
  constructor(
    public status: number,
    message: string,
    public detail?: string,
    public meta?: { retryAfter?: string },
  ) {
    super(message)
    this.name = "CodexError"
  }
}
