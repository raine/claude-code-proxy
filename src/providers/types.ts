import type { AnthropicRequest } from "../anthropic/schema.ts"

import type { Logger } from "../log.ts"

export interface RequestContext {
  reqId: string
  sessionId?: string
  sessionSeq?: number
  signal: AbortSignal
  childLogger(service: string): Logger
}

export interface CliHandlers {
  login?: () => Promise<void>
  device?: () => Promise<void>
  status: () => Promise<void>
  logout: () => Promise<void>
}

export interface Provider {
  name: string
  // Unambiguous model identifiers this provider claims. Cross-provider
  // Anthropic-style aliases are resolved by registry-level alias routing.
  supportedModels: Set<string>
  handleMessages(body: AnthropicRequest, ctx: RequestContext): Promise<Response>
  handleCountTokens(body: AnthropicRequest, ctx: RequestContext): Promise<Response>
  cli: CliHandlers
}
