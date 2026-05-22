import type { Provider, RequestContext, CliHandlers } from "../types.ts"
import type { AnthropicRequest } from "../../anthropic/schema.ts"
import { postAnthropic, AnthropicError } from "./client.ts"
import { tokenStatus } from "./auth/token-store.ts"

// These are the real Anthropic model IDs. By claiming them here, requests
// using these models are routed to api.anthropic.com instead of codex/kimi.
export const SUPPORTED_MODELS = new Set([
  // Latest generation
  "claude-opus-4-7",
  "claude-sonnet-4-6",
  "claude-haiku-4-5",
  "claude-haiku-4-5-20251001",
  // Previous generation (still routed to api.anthropic.com when explicitly requested
  // via /model command in Claude Code; older Claude Code versions also default to these)
  "claude-opus-4-6",
  "claude-sonnet-4-5",
  "claude-haiku-4-4",
])

function jsonError(status: number, type: string, message: string): Response {
  return new Response(JSON.stringify({ type: "error", error: { type, message } }), {
    status,
    headers: { "content-type": "application/json" },
  })
}

// Strip Claude Code-specific extension fields that the official API doesn't accept
function stripExtensions(body: AnthropicRequest): Omit<AnthropicRequest, "context_management" | "output_config"> {
  const { context_management: _, output_config: __, ...clean } = body
  return clean
}

async function handleMessages(body: AnthropicRequest, ctx: RequestContext): Promise<Response> {
  const log = ctx.childLogger("provider.anthropic")
  log.debug("forwarding to anthropic", { model: body.model })

  let upstream: Response
  try {
    upstream = await postAnthropic("/v1/messages", stripExtensions(body), ctx.signal)
  } catch (err) {
    if (err instanceof AnthropicError) {
      log.warn("anthropic error", { status: err.status, detail: err.detail })
      const type = err.status === 401 || err.status === 403 ? "authentication_error" : "api_error"
      if (err.status === 429) {
        return jsonError(429, "rate_limit_error", err.detail)
      }
      return jsonError(err.status, type, err.detail)
    }
    throw err
  }

  if (!upstream.ok && upstream.status !== 200) {
    const body_text = await upstream.text()
    log.warn("anthropic upstream error", { status: upstream.status, body: body_text })
    // Pass through the error response as-is — it's already Anthropic format
    return new Response(body_text, {
      status: upstream.status,
      headers: { "content-type": "application/json" },
    })
  }

  // Pass through response as-is (already in Anthropic format, streaming or not)
  const headers = new Headers()
  headers.set("content-type", upstream.headers.get("content-type") ?? "application/json")
  if (upstream.headers.has("cache-control")) {
    headers.set("cache-control", upstream.headers.get("cache-control")!)
  }

  return new Response(upstream.body, {
    status: upstream.status,
    headers,
  })
}

async function handleCountTokens(body: AnthropicRequest, ctx: RequestContext): Promise<Response> {
  const log = ctx.childLogger("provider.anthropic")
  log.debug("count_tokens via anthropic", { model: body.model })

  // count_tokens endpoint only accepts a subset of fields; also strip CC extensions
  const { max_tokens: _, stream: __, ...countBody } = stripExtensions(body) as typeof body & { max_tokens?: unknown; stream?: unknown }

  let upstream: Response
  try {
    upstream = await postAnthropic("/v1/messages/count_tokens", countBody, ctx.signal)
  } catch (err) {
    if (err instanceof AnthropicError) {
      return jsonError(err.status, "api_error", err.detail)
    }
    throw err
  }

  const text = await upstream.text()
  return new Response(text, {
    status: upstream.status,
    headers: { "content-type": "application/json" },
  })
}

const cli: CliHandlers = {
  async status() {
    const { found, account } = tokenStatus()
    if (!found) {
      console.log(`No Claude Code credentials found for account "${account}"`)
      console.log("Run Claude Code at least once to authenticate, then retry.")
      process.exit(1)
    }
    console.log(`Account: ${account}`)
    console.log("Token: found in keychain (managed by Claude Code)")
    console.log("Note: token refresh is handled automatically by Claude Code")
  },
  async logout() {
    console.log("Anthropic tokens are managed by Claude Code — use Claude Code to log out.")
  },
}

export const anthropicProvider: Provider = {
  name: "anthropic",
  supportedModels: SUPPORTED_MODELS,
  handleMessages,
  handleCountTokens,
  cli,
}
