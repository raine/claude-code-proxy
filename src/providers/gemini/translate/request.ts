import type {
  AnthropicContentBlock,
  AnthropicImageBlock,
  AnthropicMessage,
  AnthropicRequest,
  AnthropicTextBlock,
  AnthropicTool,
} from "../../../anthropic/schema.ts"
import { geminiDefaultEffort } from "../../../config.ts"
import { geminiToolSignature } from "./signature-cache.ts"

export type GeminiEffort = "none" | "low" | "medium" | "high" | "max" | "xhigh"

export interface GeminiGenerateRequest {
  model: string
  contents: GeminiContent[]
  config?: GeminiConfig
}

export interface CodeAssistGenerateRequest {
  model: string
  project: string
  user_prompt_id: string
  enabled_credit_types?: string[]
  request: {
    contents: GeminiContent[]
    systemInstruction?: GeminiContent
    tools?: GeminiTool[]
    toolConfig?: GeminiToolConfig
    generationConfig?: GeminiGenerationConfig
    session_id?: string
  }
}

export interface CodeAssistCountTokensRequest {
  request: {
    model: string
    contents: GeminiContent[]
  }
}

export interface GeminiConfig {
  systemInstruction?: GeminiContent
  tools?: GeminiTool[]
  toolConfig?: GeminiToolConfig
  generationConfig?: GeminiGenerationConfig
}

export interface GeminiGenerationConfig {
  maxOutputTokens?: number
  temperature?: number
  topP?: number
  responseMimeType?: string
  responseJsonSchema?: unknown
  thinkingConfig?: GeminiThinkingConfig
}

export interface GeminiThinkingConfig {
  includeThoughts?: boolean
  thinkingLevel?: "LOW" | "MEDIUM" | "HIGH"
  thinkingBudget?: number
}

export interface GeminiContent {
  role: "user" | "model"
  parts: GeminiPart[]
}

export type GeminiPart =
  | { text: string; thought?: boolean; thoughtSignature?: string }
  | { inlineData: { mimeType: string; data: string } }
  | { fileData: { fileUri: string; mimeType?: string } }
  | { functionCall: { id?: string; name: string; args: unknown }; thoughtSignature?: string }
  | { functionResponse: { id?: string; name: string; response: unknown; parts?: GeminiPart[] } }

export interface GeminiTool {
  functionDeclarations: GeminiFunctionDeclaration[]
}

export interface GeminiFunctionDeclaration {
  name: string
  description?: string
  parametersJsonSchema: unknown
}

export interface GeminiToolConfig {
  functionCallingConfig: {
    mode: "AUTO" | "ANY" | "NONE"
    allowedFunctionNames?: string[]
  }
}

export interface TranslateOptions {
  sessionId?: string
}

const VALID_EFFORTS = new Set<GeminiEffort>([
  "none",
  "low",
  "medium",
  "high",
  "max",
  "xhigh",
])

const ANTHROPIC_EFFORTS = new Set(["low", "medium", "high", "max", "xhigh"])

export function translateRequest(
  req: AnthropicRequest,
  opts: TranslateOptions = {},
): GeminiGenerateRequest {
  assertValidAnthropicEffort(req.output_config?.effort)
  const toolNamesById = new Map<string, string>()
  const contents = buildContents(req.messages, toolNamesById)
  const tools = req.tools?.map(toGeminiTool)
  const systemInstruction = buildSystemInstruction(req.system)
  const generationConfig = buildGenerationConfig(req)
  const toolConfig = mapToolChoice(req.tool_choice)

  const config: GeminiConfig = {}
  if (systemInstruction) config.systemInstruction = systemInstruction
  if (tools && tools.length) config.tools = [{ functionDeclarations: tools }]
  if (toolConfig) config.toolConfig = toolConfig
  if (generationConfig) config.generationConfig = generationConfig

  return {
    model: req.model,
    contents,
    ...(Object.keys(config).length ? { config } : {}),
  }
}

export function toCodeAssistGenerateRequest(
  req: GeminiGenerateRequest,
  opts: {
    project: string
    userPromptId: string
    sessionId?: string
    enabledCreditTypes?: string[]
  } = {
    project: "",
    userPromptId: "",
  },
): CodeAssistGenerateRequest {
  return {
    model: req.model,
    project: opts.project,
    user_prompt_id: opts.userPromptId,
    ...(opts.enabledCreditTypes?.length ? { enabled_credit_types: opts.enabledCreditTypes } : {}),
    request: {
      contents: req.contents,
      systemInstruction: req.config?.systemInstruction,
      tools: req.config?.tools,
      toolConfig: req.config?.toolConfig,
      generationConfig: req.config?.generationConfig,
      session_id: opts.sessionId,
    },
  }
}

export function toCodeAssistCountTokensRequest(
  req: GeminiGenerateRequest,
): CodeAssistCountTokensRequest {
  return {
    request: {
      model: `models/${req.model}`,
      contents: req.contents,
    },
  }
}

export function buildSystemInstruction(
  system: AnthropicRequest["system"],
): GeminiContent | undefined {
  if (!system) return undefined
  const blocks: AnthropicTextBlock[] =
    typeof system === "string" ? [{ type: "text", text: system }] : system
  const texts = blocks
    .filter((b) => b && b.type === "text" && typeof b.text === "string")
    .map((b) => b.text)
    .filter((t) => !t.startsWith("x-anthropic-billing-header:"))
  if (!texts.length) return undefined
  return { role: "user", parts: [{ text: texts.join("\n\n") }] }
}

function buildContents(
  messages: AnthropicMessage[],
  toolNamesById: Map<string, string>,
): GeminiContent[] {
  const out: GeminiContent[] = []
  for (const msg of messages) {
    const blocks = normalizeContent(msg.content)
    if (msg.role === "user") {
      pushUserContents(out, blocks, toolNamesById)
    } else {
      pushAssistantContent(out, blocks, toolNamesById)
    }
  }
  return out
}

function pushUserContents(
  out: GeminiContent[],
  blocks: AnthropicContentBlock[],
  toolNamesById: Map<string, string>,
): void {
  let parts: GeminiPart[] = []
  const flush = () => {
    if (!parts.length) return
    out.push({ role: "user", parts })
    parts = []
  }

  for (const block of blocks) {
    if (block.type === "text") {
      parts.push({ text: block.text })
    } else if (block.type === "image") {
      parts.push(imageToGeminiPart(block))
    } else if (block.type === "tool_result") {
      flush()
      const name = toolNamesById.get(block.tool_use_id) ?? "tool"
      out.push({
        role: "user",
        parts: toolResultParts(name, block.tool_use_id, block.content, block.is_error),
      })
    }
  }
  flush()
}

function pushAssistantContent(
  out: GeminiContent[],
  blocks: AnthropicContentBlock[],
  toolNamesById: Map<string, string>,
): void {
  const parts: GeminiPart[] = []
  for (const block of blocks) {
    if (block.type === "text") {
      if (block.text) parts.push({ text: block.text })
    } else if (block.type === "thinking") {
      if (block.thinking) {
        parts.push({
          text: block.thinking,
          thought: true,
          ...(block.signature ? { thoughtSignature: block.signature } : {}),
        })
      }
    } else if (block.type === "tool_use") {
      toolNamesById.set(block.id, block.name)
      const thoughtSignature = geminiToolSignature(block.id)
      parts.push({
        functionCall: {
          id: block.id,
          name: block.name,
          args: block.input ?? {},
        },
        ...(thoughtSignature ? { thoughtSignature } : {}),
      })
    }
  }
  if (parts.length) out.push({ role: "model", parts })
}

export function normalizeContent(content: AnthropicMessage["content"]): AnthropicContentBlock[] {
  if (typeof content === "string") return [{ type: "text", text: content }]
  return content
}

function imageToGeminiPart(block: Extract<AnthropicContentBlock, { type: "image" }>): GeminiPart {
  if (block.source.type === "base64") {
    return {
      inlineData: {
        mimeType: block.source.media_type,
        data: block.source.data,
      },
    }
  }
  return { fileData: { fileUri: block.source.url } }
}

function toolResultParts(
  name: string,
  callId: string,
  content: string | Array<AnthropicTextBlock | AnthropicImageBlock>,
  isError: boolean | undefined,
): GeminiPart[] {
  const prefix = isError ? "[tool execution error]\n" : ""
  if (typeof content === "string") {
    return [
      {
        functionResponse: {
          id: callId,
          name,
          response: { output: prefix + content },
        },
      },
    ]
  }

  const textParts: string[] = []
  const mediaParts: GeminiPart[] = []
  for (const block of content) {
    if (block.type === "text") textParts.push(block.text)
    else mediaParts.push(imageToGeminiPart(block))
  }
  return [
    {
      functionResponse: {
        id: callId,
        name,
        response: { output: prefix + textParts.join("\n") },
        ...(mediaParts.length ? { parts: mediaParts } : {}),
      },
    },
  ]
}

function toGeminiTool(tool: AnthropicTool): GeminiFunctionDeclaration {
  return {
    name: tool.name,
    description: tool.description,
    parametersJsonSchema: tool.input_schema,
  }
}

function mapToolChoice(choice: AnthropicRequest["tool_choice"]): GeminiToolConfig | undefined {
  if (!choice || choice.type === "auto") return undefined
  if (choice.type === "none") {
    return { functionCallingConfig: { mode: "NONE" } }
  }
  if (choice.type === "any") {
    return { functionCallingConfig: { mode: "ANY" } }
  }
  return {
    functionCallingConfig: {
      mode: "ANY",
      ...(choice.name ? { allowedFunctionNames: [choice.name] } : {}),
    },
  }
}

function buildGenerationConfig(req: AnthropicRequest): GeminiGenerationConfig | undefined {
  const out: GeminiGenerationConfig = {}
  if (req.max_tokens && req.max_tokens > 0) out.maxOutputTokens = req.max_tokens
  if (req.temperature !== undefined) out.temperature = req.temperature
  if (req.top_p !== undefined) out.topP = req.top_p
  const fmt = req.output_config?.format
  if (fmt?.type === "json_schema") {
    out.responseMimeType = "application/json"
    out.responseJsonSchema = fmt.schema
  }

  const effort = resolveEffort(req.output_config?.effort)
  if (effort && effort !== "none") {
    out.thinkingConfig = toThinkingConfig(effort, req.model)
  }

  return Object.keys(out).length ? out : undefined
}

function assertValidAnthropicEffort(effort: unknown): void {
  if (effort !== undefined && !ANTHROPIC_EFFORTS.has(effort as string)) {
    throw new Error(
      `Invalid output_config.effort: "${effort}". Must be one of: ${Array.from(ANTHROPIC_EFFORTS).join(", ")}`,
    )
  }
}

function resolveEffort(
  effort: NonNullable<AnthropicRequest["output_config"]>["effort"],
): GeminiEffort | undefined {
  if (effort !== undefined) return normalizeEffort(effort)
  const defaultEffort = geminiDefaultEffort()
  if (defaultEffort !== undefined) return normalizeEffort(defaultEffort)
  return undefined
}

function normalizeEffort(effort: string): GeminiEffort {
  if (!VALID_EFFORTS.has(effort as GeminiEffort)) {
    throw new Error(
      `Invalid Gemini effort: "${effort}". Must be one of: ${Array.from(VALID_EFFORTS).join(", ")}`,
    )
  }
  return effort as GeminiEffort
}

function toThinkingConfig(effort: GeminiEffort, model: string): GeminiThinkingConfig {
  if (model.startsWith("gemini-3")) {
    return {
      includeThoughts: true,
      thinkingLevel: effort === "low" ? "LOW" : effort === "medium" ? "MEDIUM" : "HIGH",
    }
  }

  return {
    includeThoughts: true,
    thinkingBudget:
      effort === "low"
        ? 1024
        : effort === "medium"
          ? 4096
          : -1,
  }
}
