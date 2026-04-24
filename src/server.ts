import { createLogger, logDir } from "./log.ts"

import type { AnthropicRequest } from "./anthropic/schema.ts"
import type { Provider, RequestContext } from "./providers/types.ts"
import { allSupportedModels, providerForModel } from "./providers/registry.ts"

const rootLog = createLogger("server")

export interface ServeOptions {
  port: number
}

const MAX_BODY_BYTES = 10 * 1024 * 1024 // 10 MiB
const MAX_SESSION_ID_LEN = 128
const SESSION_ID_RE = /^[a-zA-Z0-9_-]+$/
const MAX_SESSION_SEQ_ENTRIES = 10000

const sessionSeqs = new Map<string, number>()
let sessionSeqsLastCleanup = Date.now()

function maybeCleanupSessionSeqs(): void {
  if (sessionSeqs.size < MAX_SESSION_SEQ_ENTRIES) return
  if (Date.now() - sessionSeqsLastCleanup < 60000) return
  const cutoff = Date.now() - 3600000 // 1h TTL
  for (const [k, v] of sessionSeqs) {
    if (v < cutoff) sessionSeqs.delete(k)
  }
  sessionSeqsLastCleanup = Date.now()
}

function nextSessionSeq(sessionId?: string): number | undefined {
  if (!sessionId) return undefined
  if (sessionId.length > MAX_SESSION_ID_LEN || !SESSION_ID_RE.test(sessionId)) {
    rootLog.warn("invalid sessionId rejected", { length: sessionId.length })
    return undefined
  }
  maybeCleanupSessionSeqs()
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
        return jsonError(500, "internal_error", "Internal Server Error")
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
    const headers = new Headers({ "content-type": "application/json" })
    addSecurityHeaders(headers)
    return new Response(JSON.stringify({ ok: true }), { headers })
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

  return jsonError(404, "not_found", "Not Found")
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

function routeProvider(body: AnthropicRequest, reqId: string): Provider | Response {
  if (!body.model) {
    return jsonError(
      400,
      "invalid_request_error",
      `Missing "model" in request body. ${knownModelsMessage()}`,
    )
  }
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

function validateBody(body: unknown): string | undefined {
  if (!body || typeof body !== "object") return "Request body must be an object"
  const req = body as Record<string, unknown>
  if (typeof req.model !== "string" || !req.model.trim()) {
    return `"model" must be a non-empty string`
  }
  if (!Array.isArray(req.messages)) {
    return `"messages" must be an array`
  }
  for (const msg of req.messages) {
    if (!msg || typeof msg !== "object") return "Each message must be an object"
    const m = msg as Record<string, unknown>
    if (m.role !== "user" && m.role !== "assistant") {
      return `Invalid message role: ${m.role}`
    }
    const content = m.content
    if (typeof content !== "string" && !Array.isArray(content)) {
      return `Message content must be a string or array`
    }
  }
  const max_tokens = req.max_tokens as number | undefined
  if (max_tokens !== undefined && (!Number.isFinite(max_tokens) || max_tokens <= 0)) {
    return `"max_tokens" must be a positive finite number`
  }
  const temperature = req.temperature as number | undefined
  if (temperature !== undefined && (!Number.isFinite(temperature) || temperature < 0 || temperature > 2)) {
    return `"temperature" must be between 0 and 2`
  }
  const top_p = req.top_p as number | undefined
  if (top_p !== undefined && (!Number.isFinite(top_p) || top_p < 0 || top_p > 1)) {
    return `"top_p" must be between 0 and 1`
  }
  if (req.tools !== undefined && !Array.isArray(req.tools)) {
    return `"tools" must be an array`
  }
  return undefined
}

async function parseJsonBody(req: Request): Promise<AnthropicRequest | Response> {
  try {
    const buf = await req.arrayBuffer()
    if (buf.byteLength > MAX_BODY_BYTES) {
      return jsonError(413, "invalid_request_error", `Request body too large. Max ${MAX_BODY_BYTES} bytes.`)
    }
    const parsed = JSON.parse(new TextDecoder().decode(buf))
    const error = validateBody(parsed)
    if (error) return jsonError(400, "invalid_request_error", error)
    return parsed as AnthropicRequest
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
  const headers = filterUpstreamHeaders(resp.headers)
  addSecurityHeaders(headers)
  return new Response(stream, {
    status: resp.status,
    statusText: resp.statusText,
    headers,
  })
}

const SENSITIVE_UPSTREAM_HEADERS = new Set([
  "set-cookie",
  "server",
  "via",
  "x-request-id",
])

function addSecurityHeaders(headers: Headers): void {
  headers.set("X-Content-Type-Options", "nosniff")
  headers.set("X-Frame-Options", "DENY")
  headers.set("Referrer-Policy", "strict-origin-when-cross-origin")
}

function filterUpstreamHeaders(headers: Headers): Headers {
  const out = new Headers()
  headers.forEach((value, key) => {
    if (!SENSITIVE_UPSTREAM_HEADERS.has(key.toLowerCase())) {
      out.set(key, value)
    }
  })
  return out
}

function jsonError(status: number, type: string, message: string): Response {
  const headers = new Headers({ "content-type": "application/json" })
  addSecurityHeaders(headers)
  return new Response(JSON.stringify({ type: "error", error: { type, message } }), {
    status,
    headers,
  })
}
