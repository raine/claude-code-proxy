import { apiBaseUrl } from "./auth/constants.ts"
import { commonHeaders } from "./auth/headers.ts"
import { forceRefresh, getAuth, KimiAuthUnauthorizedError } from "./auth/manager.ts"
import type { Logger } from "../../log.ts"
import type { RequestContext } from "../types.ts"
import type { KimiChatRequest } from "./translate/request.ts"
import { retryOn429 } from "../retry.ts"

export interface KimiResponse {
  body: ReadableStream<Uint8Array>
  status: number
  headers: Headers
  requestStartTime: number
}

export class KimiError extends Error {
  constructor(
    public status: number,
    message: string,
    public detail?: string,
    public meta?: { retryAfter?: string },
  ) {
    super(message)
    this.name = "KimiError"
  }
}

export async function postKimi(
  body: KimiChatRequest,
  ctx: RequestContext,
): Promise<KimiResponse> {
  const log = ctx.childLogger("kimi.client")
  return retryOn429(() => attemptPostKimi(body, ctx, log), {
    log,
    signal: ctx.signal,
    classify: (err) =>
      err instanceof KimiError && err.status === 429
        ? { retryAfter: err.meta?.retryAfter }
        : undefined,
  })
}

async function attemptPostKimi(
  body: KimiChatRequest,
  ctx: RequestContext,
  log: Logger,
): Promise<KimiResponse> {
  let auth = await getAuth()
  const requestStartTime = Date.now()
  let resp = await doFetch(auth.access, body, log, ctx.signal)

  if (resp.status === 401) {
    log.warn("got 401, refreshing token", {})
    try {
      auth = await forceRefresh()
      resp = await doFetch(auth.access, body, log, ctx.signal)
    } catch (err) {
      if (err instanceof KimiAuthUnauthorizedError) {
        throw new KimiError(401, "Unauthorized", err.message)
      }
      log.error("refresh after 401 failed", { err: String(err) })
    }
  }

  if (resp.status === 429) {
    const retryAfter = resp.headers.get("retry-after") || undefined
    const text = await safeText(resp)
    throw new KimiError(429, "Rate limited", text, { retryAfter })
  }

  if (!resp.ok) {
    const text = await safeText(resp)
    const type = resp.status === 401 || resp.status === 403 ? "Unauthorized" : "Upstream error"
    throw new KimiError(resp.status, type, text)
  }

  if (!resp.body) throw new KimiError(500, "Upstream returned no body")

  log.debug("upstream response", {
    status: resp.status,
    timeToHeadersMs: Date.now() - requestStartTime,
  })

  return { body: resp.body, status: resp.status, headers: resp.headers, requestStartTime }
}

async function doFetch(
  accessToken: string,
  body: KimiChatRequest,
  log: Logger,
  signal?: AbortSignal,
): Promise<Response> {
  const fp = await commonHeaders()
  const headers = new Headers({
    "Content-Type": "application/json",
    Accept: "application/json",
    Authorization: `Bearer ${accessToken}`,
    ...fp,
  })

  const bodyJson = JSON.stringify(body)
  log.debug("posting to kimi", {
    url: `${apiBaseUrl()}/chat/completions`,
    model: body.model,
    messageCount: body.messages.length,
    toolCount: body.tools?.length ?? 0,
    requestBodyBytes: new TextEncoder().encode(bodyJson).length,
  })

  return fetch(`${apiBaseUrl()}/chat/completions`, {
    method: "POST",
    headers,
    body: bodyJson,
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
