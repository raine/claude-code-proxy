import { parseSseStream } from "../../../sse.ts"
import type { Logger } from "../../../log.ts"

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

export interface KimiUsage {
  prompt_tokens?: number
  completion_tokens?: number
  total_tokens?: number
  cached_tokens?: number
  prompt_tokens_details?: { cached_tokens?: number }
  completion_tokens_details?: { reasoning_tokens?: number }
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
  | { kind: "finish"; stopReason: StopReason; usage: KimiUsage | undefined }

interface StreamChunk {
  choices?: Array<{
    delta?: {
      role?: string
      content?: string | null
      reasoning_content?: string | null
      tool_calls?: Array<{
        index: number
        id?: string
        type?: string
        function?: { name?: string; arguments?: string }
      }>
    }
    finish_reason?: string | null
  }>
  usage?: KimiUsage
  error?: { message?: string; type?: string }
}

interface ToolSlot {
  blockIndex: number
  id: string
  name: string
}

/**
 * Translate a Kimi/OpenAI-style chat-completions SSE stream into a stream
 * of typed, downstream-agnostic ReducerEvents consumed by both the
 * streaming and non-streaming frontends.
 */
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
  const toolSlots = new Map<number, ToolSlot>() // keyed by upstream tool_calls[].index

  let sawToolCalls = false
  let finishReason: string | null | undefined
  let finalUsage: KimiUsage | undefined

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
  const closeAllTools = function* () {
    for (const slot of toolSlots.values()) {
      yield { kind: "tool-stop" as const, index: slot.blockIndex }
    }
    toolSlots.clear()
  }

  for await (const evt of parseSseStream(upstream)) {
    if (!evt.data) continue
    const data = evt.data.trim()
    if (data === "[DONE]") continue

    let chunk: StreamChunk
    try {
      chunk = JSON.parse(data) as StreamChunk
    } catch (err) {
      log?.warn("upstream sse: invalid json", { err: String(err), preview: data.slice(0, 200) })
      continue
    }

    if (stats) stats.chunkCount++

    if (chunk.error) {
      throw new UpstreamStreamError("failed", chunk.error.message || "Upstream error")
    }

    if (chunk.usage && !chunk.choices?.length) {
      finalUsage = chunk.usage
      continue
    }

    const choice = chunk.choices?.[0]
    if (!choice) continue
    const delta = choice.delta ?? {}

    if (typeof delta.reasoning_content === "string" && delta.reasoning_content.length > 0) {
      if (thinkingIndex === undefined) {
        thinkingIndex = nextBlockIndex++
        yield { kind: "thinking-start", index: thinkingIndex }
      }
      yield { kind: "thinking-delta", index: thinkingIndex, text: delta.reasoning_content }
    }

    if (typeof delta.content === "string" && delta.content.length > 0) {
      yield* closeThinking()
      if (textIndex === undefined) {
        textIndex = nextBlockIndex++
        yield { kind: "text-start", index: textIndex }
      }
      yield { kind: "text-delta", index: textIndex, text: delta.content }
    }

    if (delta.tool_calls?.length) {
      yield* closeThinking()
      yield* closeText()
      for (const tc of delta.tool_calls) {
        const upstreamIdx = tc.index
        let slot = toolSlots.get(upstreamIdx)
        if (!slot) {
          // First fragment of this tool call: must carry id + function.name.
          const id = tc.id ?? ""
          const name = tc.function?.name ?? ""
          if (!id || !name) {
            // Defensive: out-of-order fragment before the initial one. Skip.
            continue
          }
          sawToolCalls = true
          slot = { blockIndex: nextBlockIndex++, id, name }
          toolSlots.set(upstreamIdx, slot)
          yield { kind: "tool-start", index: slot.blockIndex, id, name }
        }
        const argsDelta = tc.function?.arguments
        if (typeof argsDelta === "string" && argsDelta.length > 0) {
          yield { kind: "tool-delta", index: slot.blockIndex, partialJson: argsDelta }
        }
      }
    }

    if (choice.finish_reason) {
      finishReason = choice.finish_reason
      if (chunk.usage) finalUsage = chunk.usage
    }
  }

  yield* closeThinking()
  yield* closeText()
  yield* closeAllTools()

  const stopReason: StopReason =
    finishReason === "length"
      ? "max_tokens"
      : finishReason === "tool_calls" || sawToolCalls
        ? "tool_use"
        : "end_turn"

  yield { kind: "finish", stopReason, usage: finalUsage }
}

export function mapUsageToAnthropic(u: KimiUsage | undefined): {
  input_tokens: number
  output_tokens: number
  cache_creation_input_tokens: number
  cache_read_input_tokens: number
} {
  const cached = u?.prompt_tokens_details?.cached_tokens ?? u?.cached_tokens ?? 0
  const totalPrompt = u?.prompt_tokens ?? 0
  return {
    input_tokens: Math.max(0, totalPrompt - cached),
    output_tokens: u?.completion_tokens ?? 0,
    cache_creation_input_tokens: 0,
    cache_read_input_tokens: cached,
  }
}
