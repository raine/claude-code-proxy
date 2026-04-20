import type { AnthropicRequest } from "../../anthropic/schema.ts"
import type { Provider, RequestContext, CliHandlers } from "../types.ts"
import {
  assertAllowedModel,
  ModelNotAllowedError,
  resolveModel,
} from "./translate/model-allowlist.ts"
import { translateRequest } from "./translate/request.ts"
import { translateStream } from "./translate/stream.ts"
import { accumulateResponse, UpstreamStreamError } from "./translate/accumulate.ts"
import { mapUsageToAnthropic } from "./translate/reducer.ts"
import { CodexError, postCodex } from "./client.ts"
import { countTokens, countTranslatedTokens } from "./count-tokens.ts"
import { runBrowserLogin } from "./auth/pkce.ts"
import { runDeviceLogin } from "./auth/device.ts"
import { persistInitialTokens } from "./auth/manager.ts"
import { loadAuth, authPath, clearAuth } from "./auth/token-store.ts"
import { RateLimitsSidecarWriter } from "./rate-limits.ts"

const VERBOSE = !!process.env.CCP_LOG_VERBOSE
const rateLimitsWriter = new RateLimitsSidecarWriter()

interface SessionCountSnapshot {
  reqId: string
  model: string
  messageCount: number
  toolCount: number
  tokens: number
}

interface SessionMessageSnapshot {
  reqId: string
  model: string
  messageCount: number
  toolCount: number
  localInputTokens?: number
  translatedInputTokens?: number
}

interface SessionTimelineState {
  lastCount?: SessionCountSnapshot
  lastMessage?: SessionMessageSnapshot
}

const sessionTimeline = new Map<string, SessionTimelineState>()

function sessionState(sessionId?: string): SessionTimelineState | undefined {
  if (!sessionId) return undefined
  let state = sessionTimeline.get(sessionId)
  if (!state) {
    state = {}
    sessionTimeline.set(sessionId, state)
  }
  return state
}

function usageWindowTokens(usage: {
  input_tokens: number
  output_tokens: number
  cache_creation_input_tokens: number
  cache_read_input_tokens: number
}): number {
  return (
    usage.input_tokens +
    usage.output_tokens +
    usage.cache_creation_input_tokens +
    usage.cache_read_input_tokens
  )
}

function upstreamHeaderSnapshot(headers: Headers): {
  serverModel?: string
  serverReasoningIncluded: boolean
} {
  return {
    serverModel: headers.get("OpenAI-Model") || undefined,
    serverReasoningIncluded: headers.has("X-Reasoning-Included"),
  }
}

function jsonError(status: number, type: string, message: string): Response {
  return new Response(JSON.stringify({ type: "error", error: { type, message } }), {
    status,
    headers: { "content-type": "application/json" },
  })
}

async function handleCountTokens(body: AnthropicRequest, ctx: RequestContext): Promise<Response> {
  const log = ctx.childLogger("provider.codex")
  const resolvedModel = resolveModel(body.model)
  const translated = translateRequest({ ...body, model: resolvedModel })
  const tokens = countTranslatedTokens(translated)
  const messageCount = body.messages?.length ?? 0
  const toolCount = body.tools?.length ?? 0
  const state = sessionState(ctx.sessionId)
  log.debug("count_tokens", { tokens })
  if (state) {
    state.lastCount = {
      reqId: ctx.reqId,
      model: body.model,
      messageCount,
      toolCount,
      tokens,
    }
  }
  if (VERBOSE) {
    log.info("compaction telemetry", {
      phase: "count_tokens",
      model: body.model,
      resolvedModel,
      tokens,
      messageCount,
      toolCount,
      previousMessageReqId: state?.lastMessage?.reqId,
      previousMessageModel: state?.lastMessage?.model,
      previousMessageCount: state?.lastMessage?.messageCount,
      previousMessageToolCount: state?.lastMessage?.toolCount,
      previousMessageLocalInputTokens: state?.lastMessage?.localInputTokens,
      previousMessageTranslatedInputTokens: state?.lastMessage?.translatedInputTokens,
    })
  }
  return new Response(JSON.stringify({ input_tokens: tokens }), {
    headers: { "content-type": "application/json" },
  })
}

async function handleMessages(body: AnthropicRequest, ctx: RequestContext): Promise<Response> {
  const log = ctx.childLogger("provider.codex")
  const messageId = `msg_${crypto.randomUUID().replace(/-/g, "")}`
  const wantStream = body.stream !== false
  const messageCount = body.messages?.length ?? 0
  const toolCount = body.tools?.length ?? 0
  const contextManagement = body.context_management
  const state = sessionState(ctx.sessionId)

  log.debug("anthropic request", {
    model: body.model,
    messageCount,
    toolCount,
    stream: wantStream,
    requestedMaxTokens: body.max_tokens,
    hasContextManagement: contextManagement !== undefined,
    hasJsonSchemaFormat: body.output_config?.format?.type === "json_schema",
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

  const translated = translateRequest({ ...body, model: resolvedModel }, { sessionId: ctx.sessionId })
  const localInputTokens = VERBOSE ? countTokens(body) : undefined
  const translatedInputTokens = VERBOSE ? countTranslatedTokens(translated) : undefined
  if (state) {
    state.lastMessage = {
      reqId: ctx.reqId,
      model: body.model,
      messageCount,
      toolCount,
      localInputTokens,
      translatedInputTokens,
    }
  }
  log.debug("translated request", {
    requestedModel: body.model,
    resolvedModel,
    inputItems: translated.input.length,
    tools: translated.tools?.length ?? 0,
    hasInstructions: !!translated.instructions,
    requestedMaxTokens: body.max_tokens,
    hasContextManagement: contextManagement !== undefined,
    promptCacheKey: translated.prompt_cache_key,
  })
  if (VERBOSE) log.debug("translated request body", { body: translated })
  if (VERBOSE) {
    log.info("compaction telemetry", {
      phase: "translated_request",
      requestedModel: body.model,
      resolvedModel,
      messageCount,
      toolCount,
      localInputTokens,
      translatedInputTokens,
      inputItems: translated.input.length,
      translatedToolCount: translated.tools?.length ?? 0,
      hasInstructions: !!translated.instructions,
      requestedMaxTokens: body.max_tokens,
      hasContextManagement: contextManagement !== undefined,
      contextManagement,
      previousCountReqId: state?.lastCount?.reqId,
      previousCountModel: state?.lastCount?.model,
      previousCountTokens: state?.lastCount?.tokens,
      previousCountMessageCount: state?.lastCount?.messageCount,
      previousCountToolCount: state?.lastCount?.toolCount,
    })
  }

  let upstream
  try {
    upstream = await postCodex(translated, ctx)
  } catch (err) {
    if (err instanceof CodexError) {
      log.warn("codex error", { status: err.status, detail: err.detail })
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
    const { serverModel, serverReasoningIncluded } = upstreamHeaderSnapshot(upstream.headers)
    const stream = translateStream(upstream.body, {
      messageId,
      model: body.model,
      log: ctx.childLogger("codex.stream"),
      accountId: upstream.accountId,
      rateLimitsWriter,
      rateLimitsTracker: upstream.rateLimitsTracker,
      signal: ctx.signal,
      onFinish: VERBOSE
        ? (finish) => {
            const mappedUsage = finish.usage ? mapUsageToAnthropic(finish.usage) : undefined
            log.info("compaction telemetry", {
              phase: "upstream_finish",
              mode: "stream",
              requestedModel: body.model,
              resolvedModel,
              serverModel,
              serverReasoningIncluded,
              messageCount,
              toolCount,
              localInputTokens,
              translatedInputTokens,
              requestedMaxTokens: body.max_tokens,
              hasContextManagement: contextManagement !== undefined,
              contextManagement,
              upstreamInputTokens: finish.usage?.input_tokens ?? 0,
              upstreamOutputTokens: finish.usage?.output_tokens ?? 0,
              upstreamCachedInputTokens: finish.usage?.input_tokens_details?.cached_tokens ?? 0,
              upstreamReasoningTokens:
                finish.usage?.output_tokens_details?.reasoning_tokens ?? 0,
              mappedInputTokens: mappedUsage?.input_tokens ?? 0,
              mappedOutputTokens: mappedUsage?.output_tokens ?? 0,
              mappedCachedInputTokens: mappedUsage?.cache_read_input_tokens ?? 0,
              mappedContextWindowTokens: mappedUsage ? usageWindowTokens(mappedUsage) : 0,
              stopReason: finish.stopReason,
            })
          }
        : undefined,
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
      model: body.model,
      log: ctx.childLogger("codex.accumulate"),
      accountId: upstream.accountId,
      rateLimitsWriter,
      rateLimitsTracker: upstream.rateLimitsTracker,
      signal: ctx.signal,
    })
    if (VERBOSE) {
      const { serverModel, serverReasoningIncluded } = upstreamHeaderSnapshot(upstream.headers)
      log.info("compaction telemetry", {
        phase: "upstream_finish",
        mode: "non_stream",
        requestedModel: body.model,
        resolvedModel,
        serverModel,
        serverReasoningIncluded,
        messageCount,
        toolCount,
        localInputTokens,
        translatedInputTokens,
        requestedMaxTokens: body.max_tokens,
        hasContextManagement: contextManagement !== undefined,
        contextManagement,
        upstreamInputTokens: result.rawUsage?.input_tokens ?? 0,
        upstreamOutputTokens: result.rawUsage?.output_tokens ?? 0,
        upstreamCachedInputTokens: result.rawUsage?.input_tokens_details?.cached_tokens ?? 0,
        upstreamReasoningTokens: result.rawUsage?.output_tokens_details?.reasoning_tokens ?? 0,
        mappedInputTokens: result.response.usage.input_tokens,
        mappedOutputTokens: result.response.usage.output_tokens,
        mappedCachedInputTokens: result.response.usage.cache_read_input_tokens,
        mappedContextWindowTokens: usageWindowTokens(result.response.usage),
        stopReason: result.response.stop_reason,
      })
    }
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
    const tokens = await runBrowserLogin()
    const saved = await persistInitialTokens(tokens)
    console.log(`Auth saved in ${authPath()}`)
    if (saved.accountId) console.log(`Account: ${saved.accountId}`)
  },
  async device() {
    const tokens = await runDeviceLogin()
    const saved = await persistInitialTokens(tokens)
    console.log(`Auth saved in ${authPath()}`)
    if (saved.accountId) console.log(`Account: ${saved.accountId}`)
  },
  async status() {
    const auth = await loadAuth()
    if (!auth) {
      console.log("Not authenticated")
      process.exit(1)
    }
    const ms = auth.expires - Date.now()
    console.log(`Account: ${auth.accountId ?? "(none)"}`)
    console.log(`Expires: ${new Date(auth.expires).toISOString()} (in ${Math.floor(ms / 1000)}s)`)
    console.log(`Storage: ${authPath()}`)
  },
  async logout() {
    await clearAuth()
    console.log("Logged out")
  },
}

export const codexProvider: Provider = {
  name: "codex",
  supportedModels: new Set(["gpt-5.2", "gpt-5.3-codex", "gpt-5.4", "gpt-5.4-mini"]),
  handleMessages,
  handleCountTokens,
  cli,
}
