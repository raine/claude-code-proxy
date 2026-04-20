import { CODEX_API_ENDPOINT, ORIGINATOR, USAGE_API_ENDPOINT } from "./auth/constants.ts"
import { forceRefresh, getAuth } from "./auth/manager.ts"
import type { Logger } from "../../log.ts"
import type { RequestContext } from "../types.ts"
import type { ResponsesRequest } from "./translate/request.ts"
import { mapUsageSnapshot, type RateLimitsSidecarWriter } from "./rate-limits.ts"

export interface CodexResponse {
  body: ReadableStream<Uint8Array>
  status: number
  headers: Headers
  accountId?: string
  rateLimitsTracker: RateLimitsTracker
}

export class RateLimitsTracker {
  private seen = false

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
    if (this.seen || !opts.success || !opts.rateLimitsWriter) return
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
  const rateLimitsTracker = new RateLimitsTracker()
  let auth = await getAuth()
  let resp = await doFetch(auth.access, auth.accountId, body, log, ctx.signal, ctx.sessionId)

  if (resp.status === 401) {
    log.warn("got 401, refreshing token", {})
    try {
      auth = await forceRefresh()
      resp = await doFetch(auth.access, auth.accountId, body, log, ctx.signal, ctx.sessionId)
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

async function fetchUsageSnapshot(
  accountId: string | undefined,
  signal: AbortSignal | undefined,
  log: Logger,
) {
  const auth = await getAuth()
  const headers = new Headers({
    authorization: `Bearer ${auth.access}`,
    originator: ORIGINATOR,
  })
  if (accountId ?? auth.accountId) headers.set("ChatGPT-Account-Id", accountId ?? auth.accountId ?? "")
  const resp = await fetch(USAGE_API_ENDPOINT, { headers, signal })
  if (!resp.ok) {
    log.warn("usage refresh returned non-ok", { status: resp.status })
    return null
  }
  return mapUsageSnapshot(await resp.json(), accountId ?? auth.accountId)
}

async function doFetch(
  accessToken: string,
  accountId: string | undefined,
  body: ResponsesRequest,
  log: Logger,
  signal?: AbortSignal,
  sessionId?: string,
): Promise<Response> {
  const headers = new Headers({
    "Content-Type": "application/json",
    accept: "text/event-stream",
    authorization: `Bearer ${accessToken}`,
    originator: ORIGINATOR,
    "openai-beta": "responses=experimental",
  })
  if (accountId) headers.set("ChatGPT-Account-Id", accountId)
  if (sessionId) {
    headers.set("session_id", sessionId)
    headers.set("x-client-request-id", sessionId)
    headers.set("x-codex-window-id", `${sessionId}:0`)
  }

  log.debug("posting to codex", {
    url: CODEX_API_ENDPOINT,
    model: body.model,
    inputCount: body.input.length,
    toolCount: body.tools?.length ?? 0,
  })

  return fetch(CODEX_API_ENDPOINT, {
    method: "POST",
    headers,
    body: JSON.stringify(body),
    signal,
  })
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
