import { createLogger, logDir } from "./log.ts"
import type { AnthropicRequest } from "./anthropic/schema.ts"
import { assertAllowedModel, ModelNotAllowedError, resolveModel } from "./translate/model-allowlist.ts"
import { translateRequest } from "./translate/request.ts"
import { translateStream } from "./translate/stream.ts"
import { accumulateResponse, UpstreamStreamError } from "./translate/accumulate.ts"
import { mapUsageToAnthropic } from "./translate/reducer.ts"
import { CodexError, postCodex } from "./codex/client.ts"
import { countTokens, countTranslatedTokens } from "./count-tokens.ts"

const log = createLogger("server")
const VERBOSE = !!process.env.CCP_LOG_VERBOSE

export interface ServeOptions {
  port: number
}

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
    serverReasoningIncluded: headers.get("X-Reasoning-Included") === "true",
  }
}

interface SessionTimelineState {
  seq: number
  lastCount?: SessionCountSnapshot
  lastMessage?: SessionMessageSnapshot
}

const sessionTimeline = new Map<string, SessionTimelineState>()

function nextSessionTimeline(sessionId?: string): {
  state?: SessionTimelineState
  sessionSeq?: number
} {
  if (!sessionId) return {}
  let state = sessionTimeline.get(sessionId)
  if (!state) {
    state = { seq: 0 }
    sessionTimeline.set(sessionId, state)
  }
  state.seq += 1
  return { state, sessionSeq: state.seq }
}

export function startServer(opts: ServeOptions): { stop: () => void; port: number } {
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: opts.port,
    idleTimeout: 255,
    async fetch(req) {
      const url = new URL(req.url)
      const start = Date.now()
      const reqId = crypto.randomUUID()
      log.info("request", { reqId, method: req.method, path: url.pathname, query: url.search })
      try {
        const resp = await route(req, url, reqId)
        log.info("response", { reqId, status: resp.status, ms: Date.now() - start })
        return resp
      } catch (err) {
        log.error("handler error", { reqId, err: String(err), stack: (err as Error)?.stack })
        return jsonError(500, "internal_error", String(err))
      }
    },
  })
  log.info("server listening", { port: server.port, logDir: logDir() })
  return {
    port: Number(server.port),
    stop: () => server.stop(),
  }
}

async function route(req: Request, url: URL, reqId: string): Promise<Response> {
  if (url.pathname === "/healthz") {
    return new Response(JSON.stringify({ ok: true }), {
      headers: { "content-type": "application/json" },
    })
  }

  if (req.method === "POST" && url.pathname === "/v1/messages/count_tokens") {
    const body = (await req.json()) as AnthropicRequest
    const resolvedModel = resolveModel(body.model)
    const translated = translateRequest({ ...body, model: resolvedModel })
    const tokens = countTranslatedTokens(translated)
    const sessionId = req.headers.get("x-claude-code-session-id") || undefined
    const { state, sessionSeq } = nextSessionTimeline(sessionId)
    const messageCount = body.messages?.length ?? 0
    const toolCount = body.tools?.length ?? 0
    log.debug("count_tokens", { reqId, tokens })
    if (state) {
      state.lastCount = {
        reqId,
        model: body.model,
        messageCount,
        toolCount,
        tokens,
      }
    }
    if (VERBOSE) {
      log.info("compaction telemetry", {
        reqId,
        phase: "count_tokens",
        path: url.pathname,
        sessionId,
        sessionSeq,
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

  if (req.method === "POST" && url.pathname === "/v1/messages") {
    return handleMessages(req, reqId)
  }

  return jsonError(404, "not_found", `No route for ${req.method} ${url.pathname}`)
}

async function handleMessages(req: Request, reqId: string): Promise<Response> {
  let body: AnthropicRequest
  try {
    body = (await req.json()) as AnthropicRequest
  } catch (err) {
    return jsonError(400, "invalid_request_error", `Invalid JSON: ${err}`)
  }

  const sessionId = req.headers.get("x-claude-code-session-id") || undefined
  const { state, sessionSeq } = nextSessionTimeline(sessionId)
  const messageId = `msg_${crypto.randomUUID().replace(/-/g, "")}`
  const wantStream = body.stream !== false
  const messageCount = body.messages?.length ?? 0
  const toolCount = body.tools?.length ?? 0
  const contextManagement = body.context_management

  log.debug("anthropic request", {
    reqId,
    model: body.model,
    messageCount,
    toolCount,
    stream: wantStream,
    sessionId,
    requestedMaxTokens: body.max_tokens,
    hasContextManagement: contextManagement !== undefined,
    hasJsonSchemaFormat: body.output_config?.format?.type === "json_schema",
  })
  if (VERBOSE) log.debug("anthropic request body", { reqId, body })

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

  const translated = translateRequest({ ...body, model: resolvedModel }, { sessionId })
  const localInputTokens = VERBOSE ? countTokens(body) : undefined
  const translatedInputTokens = VERBOSE ? countTranslatedTokens(translated) : undefined
  if (state) {
    state.lastMessage = {
      reqId,
      model: body.model,
      messageCount,
      toolCount,
      localInputTokens,
      translatedInputTokens,
    }
  }
  log.debug("translated request", {
    reqId,
    requestedModel: body.model,
    resolvedModel,
    inputItems: translated.input.length,
    tools: translated.tools?.length ?? 0,
    hasInstructions: !!translated.instructions,
    requestedMaxTokens: body.max_tokens,
    hasContextManagement: contextManagement !== undefined,
    promptCacheKey: translated.prompt_cache_key,
  })
  if (VERBOSE) log.debug("translated request body", { reqId, body: translated })
  if (VERBOSE) {
    log.info("compaction telemetry", {
      reqId,
      phase: "translated_request",
      sessionId,
      sessionSeq,
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
    upstream = await postCodex(translated, { sessionId, signal: req.signal })
  } catch (err) {
    if (err instanceof CodexError) {
      log.warn("codex error", { reqId, status: err.status, detail: err.detail })
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
      reqId,
      sessionId,
      onFinish: VERBOSE
        ? (finish) => {
            const mappedUsage = finish.usage ? mapUsageToAnthropic(finish.usage) : undefined
            log.info("compaction telemetry", {
              reqId,
              phase: "upstream_finish",
              mode: "stream",
              sessionId,
              sessionSeq,
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
    const result = await accumulateResponse(upstream.body, { messageId, model: body.model })
    if (VERBOSE) {
      const { serverModel, serverReasoningIncluded } = upstreamHeaderSnapshot(upstream.headers)
      log.info("compaction telemetry", {
        reqId,
        phase: "upstream_finish",
        mode: "non_stream",
        sessionId,
        sessionSeq,
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
        reqId,
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

function jsonError(status: number, type: string, message: string): Response {
  return new Response(JSON.stringify({ type: "error", error: { type, message } }), {
    status,
    headers: { "content-type": "application/json" },
  })
}
