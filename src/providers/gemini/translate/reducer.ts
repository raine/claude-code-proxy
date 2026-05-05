import { parseSseStream } from "../../../sse.ts"
import type { Logger } from "../../../log.ts"
import { cacheGeminiToolSignature } from "./signature-cache.ts"

export class UpstreamStreamError extends Error {
  constructor(
    public kind: "rate_limit" | "failed",
    message: string,
    public retryAfterSeconds?: number,
  ) {
    super(message)
    this.name = "UpstreamStreamError"
  }
}

export interface GeminiUsage {
  promptTokenCount?: number
  candidatesTokenCount?: number
  totalTokenCount?: number
  cachedContentTokenCount?: number
  thoughtsTokenCount?: number
}

export type StopReason = "end_turn" | "tool_use" | "max_tokens"

export type ReducerEvent =
  | { kind: "thinking-start"; index: number }
  | { kind: "thinking-delta"; index: number; text: string }
  | { kind: "thinking-stop"; index: number }
  | { kind: "text-start"; index: number }
  | { kind: "text-delta"; index: number; text: string }
  | { kind: "text-stop"; index: number }
  | { kind: "tool-start"; index: number; id: string; name: string }
  | { kind: "tool-delta"; index: number; partialJson: string }
  | { kind: "tool-stop"; index: number }
  | { kind: "finish"; stopReason: StopReason; usage: GeminiUsage | undefined }

interface GeminiStreamChunk {
  response?: {
    candidates?: GeminiCandidate[]
    usageMetadata?: GeminiUsage
  }
  error?: { message?: string; status?: string; code?: number }
}

interface GeminiCandidate {
  content?: {
    parts?: GeminiResponsePart[]
  }
  finishReason?: string
}

type GeminiResponsePart =
  | { text?: string; thought?: boolean }
  | { functionCall?: { id?: string; name?: string; args?: unknown }; thoughtSignature?: string }

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null
}

function invalidAskUserQuestionFallback(name: string, args: unknown): string | undefined {
  if (name !== "AskUserQuestion") return undefined

  if (!isRecord(args)) return "What would you like to do next?"
  const questions = args.questions
  if (!Array.isArray(questions) || questions.length === 0 || questions.length > 3) {
    return "What would you like to do next?"
  }

  let firstQuestionText: string | undefined
  let hasInvalidShape = false

  for (const question of questions) {
    if (!isRecord(question)) {
      hasInvalidShape = true
      continue
    }

    if (firstQuestionText === undefined && typeof question.question === "string") {
      firstQuestionText = question.question.trim()
    }

    const options = question.options
    if (
      typeof question.header !== "string" ||
      typeof question.question !== "string" ||
      !Array.isArray(options) ||
      options.length < 2 ||
      options.length > 3 ||
      options.some(
        (option) =>
          !isRecord(option) ||
          typeof option.label !== "string" ||
          typeof option.description !== "string",
      )
    ) {
      hasInvalidShape = true
    }
  }

  if (!hasInvalidShape) return undefined
  return firstQuestionText || "What would you like to do next?"
}

export interface ReducerStats {
  chunkCount: number
}

export async function* reduceUpstream(
  upstream: ReadableStream<Uint8Array>,
  stats?: ReducerStats,
  log?: Logger,
): AsyncGenerator<ReducerEvent> {
  let nextBlockIndex = 0
  let thinkingIndex: number | undefined
  let textIndex: number | undefined
  let sawToolCalls = false
  let finalUsage: GeminiUsage | undefined
  let finishReason: string | undefined

  const closeThinking = function* () {
    if (thinkingIndex !== undefined) {
      const idx = thinkingIndex
      thinkingIndex = undefined
      yield { kind: "thinking-stop" as const, index: idx }
    }
  }
  const closeText = function* () {
    if (textIndex !== undefined) {
      const idx = textIndex
      textIndex = undefined
      yield { kind: "text-stop" as const, index: idx }
    }
  }

  for await (const evt of parseSseStream(upstream)) {
    if (!evt.data) continue
    let chunk: GeminiStreamChunk
    try {
      chunk = JSON.parse(evt.data) as GeminiStreamChunk
    } catch (err) {
      log?.warn("upstream sse: invalid json", { err: String(err), preview: evt.data.slice(0, 200) })
      continue
    }
    if (stats) stats.chunkCount++

    if (chunk.error) {
      throw new UpstreamStreamError("failed", chunk.error.message || "Upstream error")
    }

    const response = chunk.response
    if (!response) continue
    if (response.usageMetadata) finalUsage = response.usageMetadata

    const candidate = response.candidates?.[0]
    if (!candidate) continue
    if (candidate.finishReason) finishReason = candidate.finishReason

    for (const part of candidate.content?.parts ?? []) {
      if ("text" in part && typeof part.text === "string" && part.text.length > 0) {
        if (part.thought) {
          if (thinkingIndex === undefined) {
            thinkingIndex = nextBlockIndex++
            yield { kind: "thinking-start", index: thinkingIndex }
          }
          yield { kind: "thinking-delta", index: thinkingIndex, text: part.text }
        } else {
          yield* closeThinking()
          if (textIndex === undefined) {
            textIndex = nextBlockIndex++
            yield { kind: "text-start", index: textIndex }
          }
          yield { kind: "text-delta", index: textIndex, text: part.text }
        }
      }

      if ("functionCall" in part && part.functionCall) {
        yield* closeThinking()
        yield* closeText()
        const name = part.functionCall.name ?? "tool"
        const fallbackText = invalidAskUserQuestionFallback(name, part.functionCall.args)
        if (fallbackText) {
          log?.warn("gemini upstream: dropping invalid AskUserQuestion tool call")
          const index = nextBlockIndex++
          yield { kind: "text-start", index }
          yield { kind: "text-delta", index, text: fallbackText }
          yield { kind: "text-stop", index }
          continue
        }

        const id = part.functionCall.id ?? `call_${crypto.randomUUID().replace(/-/g, "")}`
        cacheGeminiToolSignature(id, part.thoughtSignature)
        const args = JSON.stringify(part.functionCall.args ?? {})
        const index = nextBlockIndex++
        sawToolCalls = true
        yield { kind: "tool-start", index, id, name }
        if (args) yield { kind: "tool-delta", index, partialJson: args }
        yield { kind: "tool-stop", index }
      }
    }
  }

  yield* closeThinking()
  yield* closeText()

  const stopReason: StopReason =
    finishReason === "MAX_TOKENS"
      ? "max_tokens"
      : sawToolCalls
        ? "tool_use"
        : "end_turn"

  yield { kind: "finish", stopReason, usage: finalUsage }
}

export function mapUsageToAnthropic(u: GeminiUsage | undefined): {
  input_tokens: number
  output_tokens: number
  cache_creation_input_tokens: number
  cache_read_input_tokens: number
} {
  const cached = u?.cachedContentTokenCount ?? 0
  const totalPrompt = u?.promptTokenCount ?? 0
  return {
    input_tokens: Math.max(0, totalPrompt - cached),
    output_tokens: (u?.candidatesTokenCount ?? 0) + (u?.thoughtsTokenCount ?? 0),
    cache_creation_input_tokens: 0,
    cache_read_input_tokens: cached,
  }
}
