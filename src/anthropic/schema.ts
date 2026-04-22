export interface AnthropicTextBlock {
  type: "text"
  text: string
  cache_control?: { type: "ephemeral" }
}

export interface AnthropicImageBlock {
  type: "image"
  source:
    | { type: "base64"; media_type: string; data: string }
    | { type: "url"; url: string }
}

export interface AnthropicToolUseBlock {
  type: "tool_use"
  id: string
  name: string
  input: unknown
}

export interface AnthropicToolResultBlock {
  type: "tool_result"
  tool_use_id: string
  content: string | Array<AnthropicTextBlock | AnthropicImageBlock>
  is_error?: boolean
}

export interface AnthropicThinkingBlock {
  type: "thinking"
  thinking: string
  signature?: string
}

export type AnthropicContentBlock =
  | AnthropicTextBlock
  | AnthropicImageBlock
  | AnthropicToolUseBlock
  | AnthropicToolResultBlock
  | AnthropicThinkingBlock

export interface AnthropicMessage {
  role: "user" | "assistant"
  content: string | AnthropicContentBlock[]
}

export interface AnthropicTool {
  name: string
  description?: string
  input_schema: unknown
}

export interface AnthropicRequest {
  model: string
  messages: AnthropicMessage[]
  system?: string | AnthropicTextBlock[]
  tools?: AnthropicTool[]
  tool_choice?: { type: "auto" | "any" | "tool" | "none"; name?: string }
  max_tokens?: number
  temperature?: number
  top_p?: number
  stream?: boolean
  thinking?: { type: string; [k: string]: unknown }
  output_config?: {
    effort?: "low" | "medium" | "high" | "xhigh"
    format?: { type: "json_schema"; schema: unknown; name?: string; strict?: boolean }
  }
  context_management?: unknown
  metadata?: unknown
}
