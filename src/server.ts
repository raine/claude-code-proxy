import { createLogger, logDir } from "./log.ts"

import type { AnthropicRequest } from "./anthropic/schema.ts"
import type { Provider, RequestContext } from "./providers/types.ts"
import { allSupportedModels, providerForModel } from "./providers/registry.ts"

const rootLog = createLogger("server")

export interface ServeOptions {
  port: number
}

const sessionSeqs = new Map<string, number>()

function nextSessionSeq(sessionId?: string): number | undefined {
  if (!sessionId) return undefined
  const seq = (sessionSeqs.get(sessionId) ?? 0) + 1
  sessionSeqs.set(sessionId, seq)
  return seq
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
      rootLog.info("request", {
        reqId,
        method: req.method,
        path: url.pathname,
        query: url.search,
      })
      try {
        const resp = await route(req, url, reqId)
        const ms = Date.now() - start
        rootLog.info("response", { reqId, status: resp.status, ms })
        if (!resp.body) return resp
        return wrapStreamResponse(resp, reqId, start, rootLog)
      } catch (err) {
        if (isAbortError(err)) {
          rootLog.info("client disconnected", { reqId, ms: Date.now() - start })
          return new Response(null, { status: 499 })
        }
        rootLog.error("handler error", { reqId, err: String(err), stack: (err as Error)?.stack })
        return jsonError(500, "internal_error", String(err))
      }
    },
  })
  rootLog.info("server listening", { port: server.port, logDir: logDir() })
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
    const body = await parseJsonBody(req)
    if (body instanceof Response) return body
    const provider = routeProvider(body, reqId)
    if (provider instanceof Response) return provider
    const ctx = buildCtx(req, reqId, provider.name)
    ctx.childLogger("server").info("dispatch", { model: body.model })
    return provider.handleCountTokens(body, ctx)
  }

  if (req.method === "POST" && url.pathname === "/v1/messages") {
    const body = await parseJsonBody(req)
    if (body instanceof Response) return body
    const provider = routeProvider(body, reqId)
    if (provider instanceof Response) return provider
    const ctx = buildCtx(req, reqId, provider.name)
    ctx.childLogger("server").info("dispatch", { model: body.model })
    return provider.handleMessages(body, ctx)
  }

  return jsonError(404, "not_found", `No route for ${req.method} ${url.pathname}`)
}

function buildCtx(req: Request, reqId: string, providerName: string): RequestContext {
  const sessionId = req.headers.get("x-claude-code-session-id") || undefined
  const sessionSeq = nextSessionSeq(sessionId)
  const bindings = { reqId, sessionId, sessionSeq, provider: providerName }
  return {
    reqId,
    sessionId,
    sessionSeq,
    signal: req.signal,
    childLogger: (service) => createLogger(service, bindings),
  }
}

// Claude Code uses a [1m] suffix convention (e.g. "gpt-5.4[1m]") to
// signal that the model should be treated as having a 1M-token context
// window. Claude Code normalizes this away before sending requests to
// the API, but we strip it here too as defense-in-depth in case a
// future version or a different client includes it.
function normalizeIncomingModel(model: string): string {
  return model.replace(/\[1m\]$/i, "")
}

function routeProvider(body: AnthropicRequest, reqId: string): Provider | Response {
  if (!body.model) {
    return jsonError(
      400,
      "invalid_request_error",
      `Missing "model" in request body. ${knownModelsMessage()}`,
    )
  }
  body.model = normalizeIncomingModel(body.model)
  const provider = providerForModel(body.model)
  if (!provider) {
    rootLog.warn("unknown model", { reqId, model: body.model })
    return jsonError(
      400,
      "invalid_request_error",
      `Unknown model "${body.model}". ${knownModelsMessage()}`,
    )
  }
  return provider
}

function knownModelsMessage(): string {
  const groups = new Map<string, string[]>()
  for (const { model, provider } of allSupportedModels()) {
    const list = groups.get(provider) ?? []
    list.push(model)
    groups.set(provider, list)
  }
  const parts: string[] = []
  for (const [provider, models] of groups) {
    parts.push(`${provider}: ${models.join(", ")}`)
  }
  return `Supported: ${parts.join("; ")}.`
}

async function parseJsonBody(req: Request): Promise<AnthropicRequest | Response> {
  try {
    return (await req.json()) as AnthropicRequest
  } catch (err) {
    return jsonError(400, "invalid_request_error", `Invalid JSON: ${err}`)
  }
}

function isAbortError(err: unknown): boolean {
  return err instanceof Error && err.name === "AbortError"
}

function wrapStreamResponse(
  resp: Response,
  reqId: string,
  start: number,
  log: ReturnType<typeof createLogger>,
): Response {
  const body = resp.body!
  const reader = body.getReader()
  const stream = new ReadableStream<Uint8Array>({
    async pull(controller) {
      try {
        const { done, value } = await reader.read()
        if (done) {
          log.info("request_completed", { reqId, status: resp.status, ms: Date.now() - start })
          controller.close()
          return
        }
        controller.enqueue(value)
      } catch (err) {
        if (isAbortError(err)) {
          log.info("client disconnected", { reqId, ms: Date.now() - start })
        } else {
          log.error("stream error", { reqId, err: String(err) })
        }
        controller.error(err)
      }
    },
    cancel() {
      reader.cancel().catch(() => {})
    },
  })
  return new Response(stream, {
    status: resp.status,
    statusText: resp.statusText,
    headers: resp.headers,
  })
}

function jsonError(status: number, type: string, message: string): Response {
  return new Response(JSON.stringify({ type: "error", error: { type, message } }), {
    status,
    headers: { "content-type": "application/json" },
  })
}
