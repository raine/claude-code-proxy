import { encode } from "gpt-tokenizer/model/gpt-4o"
import type { AnthropicRequest } from "../../anthropic/schema.ts"
import type { GeminiGenerateRequest, GeminiPart } from "./translate/request.ts"
import { buildSystemInstruction, normalizeContent } from "./translate/request.ts"

const IMAGE_TOKEN_ESTIMATE = 3000

export function countTokens(req: AnthropicRequest): number {
  let total = 0
  const system = buildSystemInstruction(req.system)
  if (system) total += countParts(system.parts)

  for (const msg of req.messages) {
    for (const block of normalizeContent(msg.content)) {
      if (block.type === "text") total += encode(block.text).length
      else if (block.type === "image") total += IMAGE_TOKEN_ESTIMATE
      else if (block.type === "thinking") total += encode(block.thinking).length
      else if (block.type === "tool_use") {
        total += encode(block.name).length
        total += encode(JSON.stringify(block.input ?? {})).length
      } else if (block.type === "tool_result") {
        total += encode(JSON.stringify(block.content)).length
      }
    }
  }

  for (const tool of req.tools ?? []) {
    total += encode(tool.name).length
    if (tool.description) total += encode(tool.description).length
    total += encode(JSON.stringify(tool.input_schema ?? {})).length
  }

  total += req.messages.length * 4
  return total
}

export function countTranslatedTokens(req: GeminiGenerateRequest): number {
  let total = 0
  if (req.config?.systemInstruction) total += countParts(req.config.systemInstruction.parts)
  for (const content of req.contents) total += countParts(content.parts)
  for (const tool of req.config?.tools ?? []) {
    total += encode(JSON.stringify(tool)).length
  }
  total += req.contents.length * 4
  return total
}

function countParts(parts: GeminiPart[]): number {
  let total = 0
  for (const part of parts) {
    if ("text" in part) total += encode(part.text).length
    else if ("inlineData" in part || "fileData" in part) total += IMAGE_TOKEN_ESTIMATE
    else if ("functionCall" in part) total += encode(JSON.stringify(part.functionCall)).length
    else if ("functionResponse" in part) {
      total += encode(JSON.stringify(part.functionResponse.response ?? {})).length
      if (part.functionResponse.parts) total += countParts(part.functionResponse.parts)
    }
  }
  return total
}
