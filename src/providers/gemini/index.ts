import type { AnthropicRequest } from "../../anthropic/schema.ts"
import { geminiEnableFallback, geminiEnableGoogleOneCredits, logVerbose } from "../../config.ts"
import type { CliHandlers, Provider, RequestContext } from "../types.ts"
import { countTokens, countTranslatedTokens } from "./count-tokens.ts"
import {
  countGeminiTokens,
  GeminiError,
  postGeminiStream,
  setupGemini,
} from "./client.ts"
import {
  assertAllowedModel,
  fallbackChain,
  GEMINI_DEFAULT_MODEL,
  GEMINI_SMALL_FAST_MODEL,
  isGoogleOneCreditEligibleModel,
  ModelNotAllowedError,
  resolveModel,
} from "./translate/model-allowlist.ts"
import {
  toCodeAssistCountTokensRequest,
  toCodeAssistGenerateRequest,
  translateRequest,
} from "./translate/request.ts"
import { translateStream } from "./translate/stream.ts"
import { accumulateResponse, UpstreamStreamError } from "./translate/accumulate.ts"
import { mapUsageToAnthropic } from "./translate/reducer.ts"
import {
  authPath,
  clearCredentials,
  printStatus,
  runBrowserLogin,
} from "./auth/oauth.ts"

function jsonError(status: number, type: string, message: string): Response {
  return new Response(JSON.stringify({ type: "error", error: { type, message } }), {
    status,
    headers: { "content-type": "application/json" },
  })
}

async function handleCountTokens(body: AnthropicRequest, ctx: RequestContext): Promise<Response> {
  const log = ctx.childLogger("provider.gemini")
  const resolvedModel = resolveModel(body.model)
  try {
    assertAllowedModel(resolvedModel)
  } catch (err) {
    if (err instanceof ModelNotAllowedError) {
      return jsonError(
        400,
        "invalid_request_error",
        `Model "${body.model}" resolves to unsupported model "${err.model}"`,
      )
    }
    throw err
  }

  const translated = translateRequest({ ...body, model: resolvedModel })
  let tokens = countTranslatedTokens(translated)
  try {
    tokens = await countGeminiTokens(toCodeAssistCountTokensRequest(translated), ctx)
  } catch (err) {
    log.warn("count_tokens upstream failed; using local estimate", { err: String(err) })
  }
  log.debug("count_tokens", { tokens })
  return new Response(JSON.stringify({ input_tokens: tokens }), {
    headers: { "content-type": "application/json" },
  })
}

async function handleMessages(body: AnthropicRequest, ctx: RequestContext): Promise<Response> {
  const log = ctx.childLogger("provider.gemini")
  const messageId = `msg_${crypto.randomUUID().replace(/-/g, "")}`
  const wantStream = body.stream !== false
  const messageCount = body.messages?.length ?? 0
  const toolCount = body.tools?.length ?? 0

  log.debug("anthropic request", {
    model: body.model,
    messageCount,
    toolCount,
    stream: wantStream,
    requestedMaxTokens: body.max_tokens,
    requestedEffort: body.output_config?.effort ?? null,
  })
  if (logVerbose()) log.debug("anthropic request body", { body })

  const requestedModel = resolveModel(body.model)
  try {
    assertAllowedModel(requestedModel)
  } catch (err) {
    if (err instanceof ModelNotAllowedError) {
      return jsonError(
        400,
        "invalid_request_error",
        `Model "${body.model}" resolves to unsupported model "${err.model}"`,
      )
    }
    throw err
  }

  const setup = await setupGemini(ctx)
  const enableGoogleOneCredits =
    geminiEnableGoogleOneCredits() &&
    setup.googleOneCreditBalance !== null &&
    setup.googleOneCreditBalance >= 50
  let upstream
  let resolvedModel = requestedModel
  const modelsToTry = geminiEnableFallback() ? fallbackChain(requestedModel) : [requestedModel]
  for (const candidateModel of modelsToTry) {
    const translated = translateRequest({ ...body, model: candidateModel }, { sessionId: ctx.sessionId })
    const enabledCreditTypes =
      enableGoogleOneCredits && isGoogleOneCreditEligibleModel(candidateModel)
        ? ["GOOGLE_ONE_AI"]
        : undefined
    const localInputTokens = logVerbose() ? countTokens(body) : undefined
    const translatedInputTokens = logVerbose() ? countTranslatedTokens(translated) : undefined
    log.debug("translated request", {
      requestedModel: body.model,
      candidateModel,
      messageCount: translated.contents.length,
      toolCount: translated.config?.tools?.[0]?.functionDeclarations.length ?? 0,
      localInputTokens,
      translatedInputTokens,
      promptCacheKey: ctx.sessionId,
      thinkingConfig: translated.config?.generationConfig?.thinkingConfig ?? null,
      maxOutputTokens: translated.config?.generationConfig?.maxOutputTokens,
      googleOneCreditsEnabled: Boolean(enabledCreditTypes?.length),
    })
    if (logVerbose()) log.debug("translated request body", { body: translated })

    try {
      upstream = await postGeminiStream(
        toCodeAssistGenerateRequest(translated, {
          project: setup.project,
          userPromptId: crypto.randomUUID(),
          sessionId: ctx.sessionId,
          enabledCreditTypes,
        }),
        ctx,
      )
      resolvedModel = candidateModel
      break
    } catch (err) {
      if (
        err instanceof GeminiError &&
        shouldTryFallback(err) &&
        candidateModel !== modelsToTry.at(-1)
      ) {
        log.warn("gemini model failed; trying fallback", {
          model: candidateModel,
          status: err.status,
          detail: err.detail,
          nextModel: modelsToTry[modelsToTry.indexOf(candidateModel) + 1],
        })
        continue
      }
      if (err instanceof GeminiError) {
        return geminiErrorResponse(err, log)
      }
      throw err
    }
  }

  if (!upstream) {
    return jsonError(502, "api_error", "Gemini request did not produce an upstream response")
  }

  if (wantStream) {
    const stream = translateStream(upstream.body, {
      messageId,
      model: resolvedModel,
      log: ctx.childLogger("gemini.stream"),
      requestStartTime: upstream.requestStartTime,
      onFinish: (finish) => {
        const mappedUsage = finish.usage ? mapUsageToAnthropic(finish.usage) : undefined
        log.debug("stream finish", {
          stopReason: finish.stopReason,
          upstreamInputTokens: finish.usage?.promptTokenCount ?? 0,
          upstreamOutputTokens: finish.usage?.candidatesTokenCount ?? 0,
          upstreamCachedInputTokens: finish.usage?.cachedContentTokenCount ?? 0,
          upstreamThoughtTokens: finish.usage?.thoughtsTokenCount ?? 0,
          mappedInputTokens: mappedUsage?.input_tokens ?? 0,
          mappedOutputTokens: mappedUsage?.output_tokens ?? 0,
          mappedCacheReadTokens: mappedUsage?.cache_read_input_tokens ?? 0,
        })
      },
    })
    return new Response(stream, {
      status: 200,
      headers: {
        "content-type": "text/event-stream",
        "cache-control": "no-cache",
        connection: "keep-alive",
      },
    })
  }

  try {
    const result = await accumulateResponse(upstream.body, {
      messageId,
      model: resolvedModel,
      log: ctx.childLogger("gemini.accumulate"),
    })
    return new Response(JSON.stringify(result.response), {
      headers: { "content-type": "application/json" },
    })
  } catch (err) {
    if (err instanceof UpstreamStreamError) {
      log.warn("upstream stream error (non-streaming)", {
        kind: err.kind,
        message: err.message,
      })
      if (err.kind === "rate_limit") {
        const headers: Record<string, string> = { "content-type": "application/json" }
        if (err.retryAfterSeconds) headers["retry-after"] = String(err.retryAfterSeconds)
        return new Response(
          JSON.stringify({
            type: "error",
            error: { type: "rate_limit_error", message: err.message },
          }),
          { status: 429, headers },
        )
      }
      return jsonError(502, "api_error", err.message)
    }
    throw err
  }
}

function shouldTryFallback(err: GeminiError): boolean {
  if (err.status === 429 || err.status === 404 || err.status === 503) return true
  if (err.status >= 500 && err.status <= 599) return true
  if (err.status === 400) {
    const text = `${err.detail ?? ""} ${err.message}`.toLowerCase()
    return text.includes("model") || text.includes("unsupported") || text.includes("not found")
  }
  return false
}

function geminiErrorResponse(err: GeminiError, log: ReturnType<RequestContext["childLogger"]>): Response {
  log.warn("gemini error", { status: err.status, detail: err.detail })
  if (err.status === 429) {
    const headers: Record<string, string> = { "content-type": "application/json" }
    if (err.meta?.retryAfter) headers["retry-after"] = err.meta.retryAfter
    return new Response(
      JSON.stringify({
        type: "error",
        error: { type: "rate_limit_error", message: err.detail || err.message },
      }),
      { status: 429, headers },
    )
  }
  const type = err.status === 401 || err.status === 403 ? "authentication_error" : "api_error"
  return jsonError(err.status, type, err.detail || err.message)
}

const cli: CliHandlers = {
  async login() {
    await runBrowserLogin()
  },
  async status() {
    await printStatus()
  },
  async logout() {
    await clearCredentials()
    console.log(`Logged out (${authPath()})`)
  },
}

export const geminiProvider: Provider = {
  name: "gemini",
  supportedModels: new Set([
    GEMINI_DEFAULT_MODEL,
    "gemini-3-pro-preview",
    GEMINI_SMALL_FAST_MODEL,
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
  ]),
  handleMessages,
  handleCountTokens,
  cli,
}
