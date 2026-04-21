import type { Provider, CliHandlers, RequestContext } from "../types.ts"
import type { AnthropicRequest } from "../../anthropic/schema.ts"
import {
  assertAllowedModel,
  ModelNotAllowedError,
  resolveModel,
} from "./translate/model-allowlist.ts"
import { translateRequest } from "./translate/request.ts"
import { translateStream } from "./translate/stream.ts"
import { accumulateResponse, UpstreamStreamError } from "./translate/accumulate.ts"
import { mapUsageToAnthropic } from "./translate/reducer.ts"
import { countTokens, countTranslatedTokens } from "./count-tokens.ts"
import { KimiError, postKimi } from "./client.ts"
import { runDeviceLogin } from "./auth/login.ts"
import { persistInitialTokens } from "./auth/manager.ts"
import { loadAuth, clearAuth, authPath } from "./auth/token-store.ts"

const VERBOSE = !!process.env.CCP_LOG_VERBOSE

function jsonError(status: number, type: string, message: string): Response {
  return new Response(JSON.stringify({ type: "error", error: { type, message } }), {
    status,
    headers: { "content-type": "application/json" },
  })
}

async function handleCountTokens(body: AnthropicRequest, ctx: RequestContext): Promise<Response> {
  const log = ctx.childLogger("provider.kimi")
  const resolvedModel = resolveModel(body.model)
  const translated = translateRequest({ ...body, model: resolvedModel })
  const tokens = countTranslatedTokens(translated)
  log.debug("count_tokens", { tokens })
  return new Response(JSON.stringify({ input_tokens: tokens }), {
    headers: { "content-type": "application/json" },
  })
}

async function handleMessages(body: AnthropicRequest, ctx: RequestContext): Promise<Response> {
  const log = ctx.childLogger("provider.kimi")
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
  })
  if (VERBOSE) log.debug("anthropic request body", { body })

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

  const translated = translateRequest(
    { ...body, model: resolvedModel },
    { sessionId: ctx.sessionId },
  )
  const localInputTokens = countTokens(body)
  const translatedInputTokens = countTranslatedTokens(translated)
  log.debug("translated request", {
    requestedModel: body.model,
    resolvedModel,
    messageCount: translated.messages.length,
    toolCount: translated.tools?.length ?? 0,
    localInputTokens,
    translatedInputTokens,
    promptCacheKey: translated.prompt_cache_key,
    reasoningEffort: translated.reasoning_effort,
    thinking: translated.thinking?.type,
    maxTokens: translated.max_tokens,
  })
  if (VERBOSE) log.debug("translated request body", { body: translated })

  let upstream
  try {
    upstream = await postKimi(translated, ctx)
  } catch (err) {
    if (err instanceof KimiError) {
      log.warn("kimi error", { status: err.status, detail: err.detail })
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
      const type =
        err.status === 401 || err.status === 403 ? "authentication_error" : "api_error"
      return jsonError(err.status, type, err.detail || err.message)
    }
    throw err
  }

  if (wantStream) {
    const stream = translateStream(upstream.body, {
      messageId,
      model: body.model,
      log: ctx.childLogger("kimi.stream"),
      requestStartTime: upstream.requestStartTime,
      onFinish: (finish) => {
        const mappedUsage = finish.usage ? mapUsageToAnthropic(finish.usage) : undefined
        log.debug("stream finish", {
          stopReason: finish.stopReason,
          upstreamInputTokens: finish.usage?.prompt_tokens ?? 0,
          upstreamOutputTokens: finish.usage?.completion_tokens ?? 0,
          upstreamCachedInputTokens:
            finish.usage?.prompt_tokens_details?.cached_tokens ??
            finish.usage?.cached_tokens ??
            0,
          upstreamReasoningTokens:
            finish.usage?.completion_tokens_details?.reasoning_tokens ?? 0,
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
    const result = await accumulateResponse(upstream.body, { messageId, model: body.model, log: ctx.childLogger("kimi.accumulate") })
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

const cli: CliHandlers = {
  async login() {
    const tokens = await runDeviceLogin()
    const saved = await persistInitialTokens(tokens)
    console.log(`Auth saved in ${authPath()}`)
    if (saved.userId) console.log(`User: ${saved.userId}`)
    const secs = Math.floor((saved.expires - Date.now()) / 1000)
    console.log(`Expires in ${secs}s`)
  },
  async status() {
    const auth = await loadAuth()
    if (!auth) {
      console.log("Not authenticated")
      process.exit(1)
    }
    const ms = auth.expires - Date.now()
    console.log(`User: ${auth.userId ?? "(none)"}`)
    console.log(`Expires: ${new Date(auth.expires).toISOString()} (in ${Math.floor(ms / 1000)}s)`)
    console.log(`Scope: ${auth.scope ?? "(none)"}`)
    console.log(`Storage: ${authPath()}`)
  },
  async logout() {
    await clearAuth()
    console.log("Logged out")
  },
}

export const kimiProvider: Provider = {
  name: "kimi",
  supportedModels: new Set([
    "kimi-for-coding",
    "kimi-k2.6",
    "k2.6",
    "haiku",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "sonnet",
    "claude-sonnet-4-6",
    "opus",
    "claude-opus-4-7",
  ]),
  handleMessages,
  handleCountTokens,
  cli,
}
