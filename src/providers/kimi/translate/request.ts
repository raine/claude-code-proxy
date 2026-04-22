import type {
  AnthropicContentBlock,
  AnthropicImageBlock,
  AnthropicMessage,
  AnthropicRequest,
  AnthropicTextBlock,
  AnthropicTool,
} from "../../../anthropic/schema.ts"

// OpenAI-compatible chat-completions request shape used by Kimi.
// Only the fields kimi-cli is known to send are included; unknown
// fields are not forwarded.
export interface KimiChatRequest {
  model: string
  messages: KimiMessage[]
  tools?: KimiTool[]
  tool_choice?: KimiToolChoice
  stream: true
  stream_options: { include_usage: true }
  max_tokens: number
  reasoning_effort?: "low" | "medium" | "high"
  thinking?: { type: "enabled" }
  prompt_cache_key?: string
}

export type KimiToolChoice =
  | "auto"
  | "none"
  | "required"
  | { type: "function"; function: { name: string } }

export type KimiAssistantMessage = {
  role: "assistant"
  content?: string | null
  reasoning_content?: string
  tool_calls?: KimiAssistantToolCall[]
}

export type KimiMessage =
  | { role: "system"; content: string }
  | { role: "user"; content: string | KimiUserContentPart[] }
  | KimiAssistantMessage
  | { role: "tool"; tool_call_id: string; content: string | KimiToolResultPart[] }

export type KimiUserContentPart =
  | { type: "text"; text: string }
  | { type: "image_url"; image_url: { url: string } }

export type KimiToolResultPart =
  | { type: "text"; text: string }
  | { type: "image_url"; image_url: { url: string } }

export interface KimiAssistantToolCall {
  id: string
  type: "function"
  function: { name: string; arguments: string }
}

export interface KimiTool {
  type: "function"
  function: { name: string; description?: string; parameters: unknown }
}

export interface TranslateOptions {
  sessionId?: string
}

const DEFAULT_MAX_TOKENS = 32000

// Kimi's `kimi-for-coding` is a reasoning model: it always produces
// reasoning_content and its server always enforces that prior assistant
// tool_call messages carry reasoning_content back. The `thinking` /
// `reasoning_effort` flags on the request don't gate that contract —
// they're a model-level property. So we always enable thinking on the
// outbound Kimi request and always forward/replay reasoning end-to-end.
export function translateRequest(
  req: AnthropicRequest,
  opts: TranslateOptions = {},
): KimiChatRequest {
  const messages = buildMessages(req)
  const tools = req.tools?.map(toKimiTool)

  const out: KimiChatRequest = {
    model: req.model,
    messages,
    stream: true,
    stream_options: { include_usage: true },
    max_tokens: clampMaxTokens(req.max_tokens),
    reasoning_effort: mapReasoningEffort(req.output_config?.effort),
    thinking: { type: "enabled" },
  }
  if (tools && tools.length) out.tools = tools
  const tool_choice = mapToolChoice(req.tool_choice)
  if (tool_choice !== "auto") out.tool_choice = tool_choice
  if (opts.sessionId) out.prompt_cache_key = opts.sessionId
  return out
}

function clampMaxTokens(requested: number | undefined): number {
  if (!requested || requested <= 0) return DEFAULT_MAX_TOKENS
  return Math.min(requested, DEFAULT_MAX_TOKENS)
}

// Kimi's reasoning_effort is capped at "high"; collapse the proxy's "max"
// to "high" since Kimi has no stronger tier, and default to "medium" when no
// effort is requested.
function mapReasoningEffort(
  effort: NonNullable<AnthropicRequest["output_config"]>["effort"],
): "low" | "medium" | "high" {
  if (effort === "max") return "high"
  return effort ?? "medium"
}

function mapToolChoice(choice: AnthropicRequest["tool_choice"]): KimiToolChoice {
  if (!choice) return "auto"
  switch (choice.type) {
    case "auto":
      return "auto"
    case "none":
      return "none"
    case "any":
      return "required"
    case "tool":
      return choice.name
        ? { type: "function", function: { name: choice.name } }
        : "required"
  }
}

export function buildSystemMessage(system: AnthropicRequest["system"]): string | undefined {
  if (!system) return undefined
  const blocks: AnthropicTextBlock[] =
    typeof system === "string" ? [{ type: "text", text: system }] : system
  const texts = blocks
    .filter((b) => b && b.type === "text" && typeof b.text === "string")
    .map((b) => b.text)
    .filter((t) => !t.startsWith("x-anthropic-billing-header:"))
  if (!texts.length) return undefined
  return texts.join("\n\n")
}

function buildMessages(req: AnthropicRequest): KimiMessage[] {
  const out: KimiMessage[] = []
  const system = buildSystemMessage(req.system)
  if (system) out.push({ role: "system", content: system })

  for (const msg of req.messages) {
    const blocks = normalizeContent(msg.content)
    if (msg.role === "user") {
      pushUserMessages(out, blocks)
    } else {
      pushAssistantMessage(out, blocks)
    }
  }
  return out
}

// In Anthropic-land a single user message can carry a mix of text, image,
// and tool_result blocks. OpenAI-style chat-completions requires tool
// results to be their own role="tool" messages, so we split on tool_result
// boundaries, preserving order.
function pushUserMessages(out: KimiMessage[], blocks: AnthropicContentBlock[]): void {
  let buffer: KimiUserContentPart[] = []
  const flushBuffer = () => {
    if (!buffer.length) return
    // Collapse to a string when every part is text — matches what kimi-cli
    // sends and keeps the wire payload compact.
    const allText = buffer.every((p) => p.type === "text")
    if (allText) {
      out.push({
        role: "user",
        content: buffer.map((p) => (p as { type: "text"; text: string }).text).join(""),
      })
    } else {
      out.push({ role: "user", content: buffer })
    }
    buffer = []
  }

  for (const block of blocks) {
    if (block.type === "text") {
      buffer.push({ type: "text", text: block.text })
    } else if (block.type === "image") {
      buffer.push({ type: "image_url", image_url: { url: imageToUrl(block) } })
    } else if (block.type === "tool_result") {
      flushBuffer()
      out.push({
        role: "tool",
        tool_call_id: block.tool_use_id,
        content: toolResultContent(block.content, block.is_error),
      })
    }
  }
  flushBuffer()
}

function pushAssistantMessage(out: KimiMessage[], blocks: AnthropicContentBlock[]): void {
  const textParts: string[] = []
  const thinkingParts: string[] = []
  const toolCalls: KimiAssistantToolCall[] = []
  for (const block of blocks) {
    if (block.type === "text") {
      if (block.text) textParts.push(block.text)
    } else if (block.type === "thinking") {
      if (block.thinking) thinkingParts.push(block.thinking)
    } else if (block.type === "tool_use") {
      toolCalls.push({
        id: block.id,
        type: "function",
        function: {
          name: block.name,
          arguments: JSON.stringify(block.input ?? {}),
        },
      })
    }
    // image blocks from the assistant are dropped — Kimi's assistant
    // schema does not express them.
  }

  if (!textParts.length && !toolCalls.length && !thinkingParts.length) return

  const msg: KimiAssistantMessage = { role: "assistant" }
  msg.content = textParts.length ? textParts.join("") : ""
  // Kimi accepts one reasoning_content string per assistant turn. If a turn
  // has multiple interleaved thinking blocks we concatenate in order —
  // exact block/tool_use pairing can't be preserved over the wire.
  if (thinkingParts.length) msg.reasoning_content = thinkingParts.join("\n\n")
  if (toolCalls.length) msg.tool_calls = toolCalls
  out.push(msg)
}

export function normalizeContent(content: AnthropicMessage["content"]): AnthropicContentBlock[] {
  if (typeof content === "string") return [{ type: "text", text: content }]
  return content
}

function imageToUrl(block: Extract<AnthropicContentBlock, { type: "image" }>): string {
  if (block.source.type === "url") return block.source.url
  return `data:${block.source.media_type};base64,${block.source.data}`
}

export function toolResultContent(
  content: string | Array<AnthropicTextBlock | AnthropicImageBlock>,
  isError: boolean | undefined,
): string | KimiToolResultPart[] {
  const prefix = isError ? "[tool execution error]\n" : ""
  if (typeof content === "string") return prefix + content

  const parts: KimiToolResultPart[] = []
  if (prefix) parts.push({ type: "text", text: prefix })
  for (const b of content) {
    if (b.type === "text") {
      parts.push({ type: "text", text: b.text })
    } else {
      // Kimi accepts image_url parts inside role="tool" content (its own
      // ReadMediaFile tool uses exactly this shape).
      parts.push({ type: "image_url", image_url: { url: imageToUrl(b) } })
    }
  }
  if (parts.length === 1 && parts[0]!.type === "text") return parts[0]!.text
  return parts
}

export function toolResultToString(
  content: string | Array<AnthropicTextBlock | AnthropicImageBlock>,
): string {
  // Kept for the token counter, which wants a flat string.
  if (typeof content === "string") return content
  return content
    .map((b) => {
      if (b.type === "text") return b.text
      const mt = b.source.type === "base64" ? b.source.media_type : "url"
      return `[image omitted: ${mt}]`
    })
    .join("\n")
}

function toKimiTool(tool: AnthropicTool): KimiTool {
  return {
    type: "function",
    function: {
      name: tool.name,
      description: tool.description,
      parameters: tool.input_schema,
    },
  }
}
