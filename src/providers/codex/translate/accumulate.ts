import { mapRateLimitsSnapshot, type RateLimitsSidecarWriter } from "../rate-limits.ts"
import type { RateLimitsTracker } from "../client.ts"
import { mapUsageToAnthropic, reduceUpstream } from "./reducer.ts"
import type { CodexUsage } from "./reducer.ts"
import type { Logger } from "../../../log.ts"

export { UpstreamStreamError } from "./reducer.ts"

export interface AnthropicNonStreamResponse {
  id: string
  type: "message"
  role: "assistant"
  model: string
  content: Array<
    | { type: "text"; text: string }
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
  rawUsage?: CodexUsage
}

export async function accumulateResponse(
  upstream: ReadableStream<Uint8Array>,
  opts: {
    messageId: string
    model: string
    log: Logger
    accountId?: string
    rateLimitsWriter?: RateLimitsSidecarWriter
    rateLimitsTracker?: RateLimitsTracker
    signal?: AbortSignal
  },
): Promise<AccumulatedResponse> {
  const trackedUpstream = upstream.pipeThrough(
    new TransformStream<Uint8Array, Uint8Array>({
      transform(chunk, controller) {
        controller.enqueue(chunk)
      },
      async flush() {
        await opts.rateLimitsTracker?.refreshIfNeeded({
          success: true,
          accountId: opts.accountId,
          signal: opts.signal,
          rateLimitsWriter: opts.rateLimitsWriter,
          log: opts.log,
        })
      },
    }),
  )

  type Block =
    | { kind: "text"; text: string }
    | { kind: "tool"; id: string; name: string; args: string }

  const ordered: number[] = []
  const blocks = new Map<number, Block>()
  let stopReason: AnthropicNonStreamResponse["stop_reason"] = null
  let usage: ReturnType<typeof mapUsageToAnthropic> | undefined
  let rawUsage: CodexUsage | undefined

  for await (const e of reduceUpstream(trackedUpstream, opts.log)) {
    switch (e.kind) {
      case "text-start":
        blocks.set(e.index, { kind: "text", text: "" })
        ordered.push(e.index)
        break
      case "text-delta": {
        const b = blocks.get(e.index)
        if (b?.kind === "text") b.text += e.text
        break
      }
      case "tool-start":
        blocks.set(e.index, { kind: "tool", id: e.id, name: e.name, args: "" })
        ordered.push(e.index)
        break
      case "tool-delta": {
        const b = blocks.get(e.index)
        if (b?.kind === "tool") b.args += e.partialJson
        break
      }
      case "text-stop":
      case "tool-stop":
        break
      case "rate-limits": {
        opts.rateLimitsTracker?.markSeen()
        const snapshot = mapRateLimitsSnapshot(e.rateLimits, opts.accountId)
        if (snapshot) await opts.rateLimitsWriter?.write(snapshot)
        break
      }
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
    if (b.kind === "text") {
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
