import { mapUsageToAnthropic, reduceUpstream, type KimiUsage } from "./reducer.ts"
import { makeThinkingSignature } from "./signature.ts"

export { UpstreamStreamError } from "./reducer.ts"

export interface AnthropicNonStreamResponse {
  id: string
  type: "message"
  role: "assistant"
  model: string
  content: Array<
    | { type: "text"; text: string }
    | { type: "thinking"; thinking: string; signature: string }
    | { type: "tool_use"; id: string; name: string; input: unknown }
  >
  stop_reason: "end_turn" | "tool_use" | "max_tokens" | null
  stop_sequence: null
  usage: {
    input_tokens: number
    output_tokens: number
    cache_creation_input_tokens: number
    cache_read_input_tokens: number
  }
}

export interface AccumulatedResponse {
  response: AnthropicNonStreamResponse
  rawUsage?: KimiUsage
}

import type { Logger } from "../../../log.ts"

export async function accumulateResponse(
  upstream: ReadableStream<Uint8Array>,
  opts: { messageId: string; model: string; log: Logger },
): Promise<AccumulatedResponse> {
  type Block =
    | { kind: "thinking"; text: string }
    | { kind: "text"; text: string }
    | { kind: "tool"; id: string; name: string; args: string }

  const ordered: number[] = []
  const blocks = new Map<number, Block>()
  let stopReason: AnthropicNonStreamResponse["stop_reason"] = null
  let usage: ReturnType<typeof mapUsageToAnthropic> | undefined
  let rawUsage: KimiUsage | undefined
  let reasoningChars = 0
  let contentChars = 0
  let toolCount = 0
  const stats = { chunkCount: 0 }

  for await (const e of reduceUpstream(upstream, stats, opts.log)) {
    switch (e.kind) {
      case "thinking-start":
        blocks.set(e.index, { kind: "thinking", text: "" })
        ordered.push(e.index)
        break
      case "thinking-delta": {
        const b = blocks.get(e.index)
        if (b?.kind === "thinking") {
          b.text += e.text
          reasoningChars += e.text.length
        }
        break
      }
      case "text-start":
        blocks.set(e.index, { kind: "text", text: "" })
        ordered.push(e.index)
        break
      case "text-delta": {
        const b = blocks.get(e.index)
        if (b?.kind === "text") {
          b.text += e.text
          contentChars += e.text.length
        }
        break
      }
      case "tool-start":
        toolCount++
        blocks.set(e.index, { kind: "tool", id: e.id, name: e.name, args: "" })
        ordered.push(e.index)
        break
      case "tool-delta": {
        const b = blocks.get(e.index)
        if (b?.kind === "tool") b.args += e.partialJson
        break
      }
      case "thinking-stop":
      case "text-stop":
      case "tool-stop":
        break
      case "finish":
        stopReason = e.stopReason
        rawUsage = e.usage
        usage = mapUsageToAnthropic(e.usage)
        break
    }
  }

  const content: AnthropicNonStreamResponse["content"] = []
  for (const i of ordered) {
    const b = blocks.get(i)
    if (!b) continue
    if (b.kind === "thinking") {
      if (b.text)
        content.push({
          type: "thinking",
          thinking: b.text,
          signature: makeThinkingSignature(opts.messageId, i),
        })
    } else if (b.kind === "text") {
      if (b.text) content.push({ type: "text", text: b.text })
    } else {
      let input: unknown = {}
      try {
        input = b.args ? JSON.parse(b.args) : {}
      } catch {
        input = { _raw: b.args }
      }
      content.push({ type: "tool_use", id: b.id, name: b.name, input })
    }
  }

  opts.log.debug("accumulate summary", {
    chunkCount: stats.chunkCount,
    reasoningChars,
    contentChars,
    toolCount,
    stopReason,
    usage: rawUsage,
  })

  return {
    rawUsage,
    response: {
      id: opts.messageId,
      type: "message",
      role: "assistant",
      model: opts.model,
      content,
      stop_reason: stopReason,
      stop_sequence: null,
      usage: usage ?? {
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
      },
    },
  }
}
