import { geminiApiVersion, geminiEndpoint } from "../../config.ts"
import type { Logger } from "../../log.ts"
import type { RequestContext } from "../types.ts"
import type {
  CodeAssistCountTokensRequest,
  CodeAssistGenerateRequest,
} from "./translate/request.ts"
import { accessToken, GeminiAuthError } from "./auth/oauth.ts"
import { retryOn429 } from "../retry.ts"

export interface GeminiResponse {
  body: ReadableStream<Uint8Array>
  status: number
  headers: Headers
  requestStartTime: number
}

export interface GeminiSetup {
  project: string
  googleOneCreditBalance: number | null
}

export class GeminiError extends Error {
  constructor(
    public status: number,
    message: string,
    public detail?: string,
    public meta?: { retryAfter?: string },
  ) {
    super(message)
    this.name = "GeminiError"
  }
}

let setupCache: Promise<GeminiSetup> | undefined

export async function setupGemini(ctx: RequestContext): Promise<GeminiSetup> {
  setupCache ??= doSetupGemini(ctx).catch((err) => {
    setupCache = undefined
    throw err
  })
  return setupCache
}

async function doSetupGemini(ctx: RequestContext): Promise<GeminiSetup> {
  const project = process.env.GOOGLE_CLOUD_PROJECT || process.env.GOOGLE_CLOUD_PROJECT_ID || undefined
  const metadata = {
    ideType: "IDE_UNSPECIFIED",
    platform: "PLATFORM_UNSPECIFIED",
    pluginType: "GEMINI",
    duetProject: project,
  }
  const response = await postJson(
    "loadCodeAssist",
    {
      cloudaicompanionProject: project,
      metadata,
    },
    ctx,
    ctx.childLogger("gemini.setup"),
  )
  const body = response as {
    cloudaicompanionProject?: string
    currentTier?: { id?: string; name?: string; hasOnboardedPreviously?: boolean }
    paidTier?: {
      id?: string
      name?: string
      availableCredits?: Array<{ creditType?: string; creditAmount?: string }>
    }
    ineligibleTiers?: Array<{ reasonMessage?: string }>
  }
  const resolvedProject = body.cloudaicompanionProject ?? project
  if (!resolvedProject) {
    const reason = body.ineligibleTiers?.map((t) => t.reasonMessage).filter(Boolean).join(", ")
    throw new GeminiError(
      400,
      "Project required",
      reason ||
        "Gemini Code Assist did not return a project. Set GOOGLE_CLOUD_PROJECT or run Gemini CLI onboarding.",
    )
  }
  const googleOneCreditBalance = googleOneCredits(body.paidTier)
  ctx.childLogger("gemini.setup").debug("setup complete", {
    project: resolvedProject,
    googleOneCreditBalance,
  })
  return { project: resolvedProject, googleOneCreditBalance }
}

export async function postGeminiStream(
  body: CodeAssistGenerateRequest,
  ctx: RequestContext,
): Promise<GeminiResponse> {
  const log = ctx.childLogger("gemini.client")
  return retryOn429(() => attemptPostGeminiStream(body, ctx, log), {
    log,
    signal: ctx.signal,
    classify: (err) =>
      err instanceof GeminiError && err.status === 429
        ? { retryAfter: err.meta?.retryAfter }
        : undefined,
  })
}

export async function countGeminiTokens(
  body: CodeAssistCountTokensRequest,
  ctx: RequestContext,
): Promise<number> {
  const log = ctx.childLogger("gemini.client")
  const response = await postJson("countTokens", body, ctx, log)
  const totalTokens = (response as { totalTokens?: number }).totalTokens
  return typeof totalTokens === "number" ? totalTokens : 0
}

async function attemptPostGeminiStream(
  body: CodeAssistGenerateRequest,
  ctx: RequestContext,
  log: Logger,
): Promise<GeminiResponse> {
  const requestStartTime = Date.now()
  const resp = await doFetch("streamGenerateContent", body, ctx, log, true)

  if (resp.status === 429) {
    const text = await safeText(resp)
    const retryAfter = resp.headers.get("retry-after") || geminiRetryAfter(text)
    throw new GeminiError(429, "Rate limited", text, { retryAfter })
  }

  if (!resp.ok) {
    const text = await safeText(resp)
    const type = resp.status === 401 || resp.status === 403 ? "Unauthorized" : "Upstream error"
    throw new GeminiError(resp.status, type, text)
  }

  if (!resp.body) throw new GeminiError(500, "Upstream returned no body")

  log.debug("upstream response", {
    status: resp.status,
    timeToHeadersMs: Date.now() - requestStartTime,
  })

  return { body: resp.body, status: resp.status, headers: resp.headers, requestStartTime }
}

async function postJson(
  method: string,
  body: unknown,
  ctx: RequestContext,
  log: Logger,
): Promise<unknown> {
  const resp = await doFetch(method, body, ctx, log, false)
  if (!resp.ok) {
    const text = await safeText(resp)
    const type = resp.status === 401 || resp.status === 403 ? "Unauthorized" : "Upstream error"
    throw new GeminiError(resp.status, type, text)
  }
  return resp.json()
}

async function doFetch(
  method: string,
  body: unknown,
  ctx: RequestContext,
  log: Logger,
  stream: boolean,
): Promise<Response> {
  let token: string
  try {
    token = await accessToken()
  } catch (err) {
    if (err instanceof GeminiAuthError) {
      throw new GeminiError(401, "Unauthorized", err.message)
    }
    throw err
  }

  const url = `${baseUrl()}:${method}${stream ? "?alt=sse" : ""}`
  const bodyJson = JSON.stringify(body)
  log.debug("posting to gemini", {
    url,
    method,
    stream,
    requestBodyBytes: new TextEncoder().encode(bodyJson).length,
  })

  return fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Accept: stream ? "text/event-stream" : "application/json",
      Authorization: `Bearer ${token}`,
    },
    body: bodyJson,
    signal: ctx.signal,
  })
}

function baseUrl(): string {
  return `${geminiEndpoint().replace(/\/$/, "")}/${geminiApiVersion()}`
}

async function safeText(resp: Response): Promise<string> {
  try {
    return await resp.text()
  } catch {
    return ""
  }
}

function googleOneCredits(
  paidTier?: { availableCredits?: Array<{ creditType?: string; creditAmount?: string }> },
): number | null {
  const credits = paidTier?.availableCredits?.filter((c) => c.creditType === "GOOGLE_ONE_AI")
  if (!credits?.length) return null
  return credits.reduce((sum, credit) => {
    const amount = Number.parseInt(credit.creditAmount ?? "0", 10)
    return sum + (Number.isFinite(amount) ? amount : 0)
  }, 0)
}

export function geminiRetryAfter(detail: string): string | undefined {
  try {
    const parsed = JSON.parse(detail) as { error?: { message?: string } }
    const fromJson = retryAfterFromMessage(parsed.error?.message)
    if (fromJson) return fromJson
  } catch {
    // Fall through to regex scan below.
  }
  return retryAfterFromMessage(detail)
}

function retryAfterFromMessage(message: unknown): string | undefined {
  if (typeof message !== "string") return undefined
  const match = /reset\s+after\s+(\d+(?:\.\d+)?)\s*(ms|msec|milliseconds?|s|sec|seconds?|m|min|minutes?)\b/i.exec(
    message,
  )
  if (!match) return undefined
  const value = Number.parseFloat(match[1] ?? "")
  if (!Number.isFinite(value)) return undefined
  const unit = (match[2] ?? "s").toLowerCase()
  if (unit.startsWith("m") && !unit.startsWith("ms") && !unit.startsWith("msec")) {
    return String(Math.ceil(value * 60))
  }
  if (unit.startsWith("ms") || unit.startsWith("msec") || unit.startsWith("millisecond")) {
    return String(Math.ceil(value / 1000))
  }
  return String(Math.ceil(value))
}
