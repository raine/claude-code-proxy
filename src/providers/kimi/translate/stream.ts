import { encodeSseEvent } from "../../../sse.ts"
import type { Logger } from "../../../log.ts"
import { mapUsageToAnthropic, reduceUpstream, UpstreamStreamError, type KimiUsage } from "./reducer.ts"
import { makeThinkingSignature } from "./signature.ts"

function isAbortError(err: unknown): boolean {
  return err instanceof Error && err.name === "AbortError"
}

/**
 * Translate a Kimi chat-completions SSE stream into Anthropic SSE events.
 * HTTP status 200 is already committed before the first upstream event is
 * read, so rate-limit and upstream-failed conditions surface here as SSE
 * `error` events instead of non-200 statuses.
 */
export function translateStream(
  upstream: ReadableStream<Uint8Array>,
  opts: {
    messageId: string
    model: string
    log: Logger
    requestStartTime?: number
    onFinish?: (finish: {
      stopReason: "end_turn" | "tool_use" | "max_tokens"
      usage?: Parameters<typeof mapUsageToAnthropic>[0]
    }) => void
  },
): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder()
  return new ReadableStream<Uint8Array>({
    async start(controller) {
      const emit = (event: string, data: unknown) => {
        controller.enqueue(encoder.encode(encodeSseEvent(event, data)))
      }
      const activeTools = new Map<number, { id: string; name: string }>()
      let messageStarted = false
      const ensureMessageStart = () => {
        if (messageStarted) return
        messageStarted = true
        emit("message_start", {
          type: "message_start",
          message: {
            id: opts.messageId,
            type: "message",
            role: "assistant",
            model: opts.model,
            content: [],
            stop_reason: null,
            stop_sequence: null,
            usage: {
              input_tokens: 0,
              output_tokens: 0,
              cache_creation_input_tokens: 0,
              cache_read_input_tokens: 0,
            },
          },
        })
        emit("ping", { type: "ping" })
      }

      const streamStart = Date.now()
      let firstChunkAt: number | undefined
      let reasoningChars = 0
      let contentChars = 0
      let toolCount = 0
      let finishUsage: KimiUsage | undefined
      let finishStopReason: string | undefined
      const stats = { chunkCount: 0 }

      try {
        for await (const e of reduceUpstream(upstream, stats, opts.log)) {
          if (firstChunkAt === undefined) firstChunkAt = Date.now()

          switch (e.kind) {
            case "thinking-start":
              ensureMessageStart()
              emit("content_block_start", {
                type: "content_block_start",
                index: e.index,
                content_block: { type: "thinking", thinking: "" },
              })
              break
            case "thinking-delta":
              reasoningChars += e.text.length
              emit("content_block_delta", {
                type: "content_block_delta",
                index: e.index,
                delta: { type: "thinking_delta", thinking: e.text },
              })
              break
            case "thinking-stop":
              // Emit the signature as a separate signature_delta right
              // before content_block_stop — matches Anthropic's native
              // wire format and keeps Claude Code's thinking-block parser
              // happy on round-trip.
              emit("content_block_delta", {
                type: "content_block_delta",
                index: e.index,
                delta: {
                  type: "signature_delta",
                  signature: makeThinkingSignature(opts.messageId, e.index),
                },
              })
              emit("content_block_stop", { type: "content_block_stop", index: e.index })
              break
            case "text-start":
              ensureMessageStart()
              emit("content_block_start", {
                type: "content_block_start",
                index: e.index,
                content_block: { type: "text", text: "" },
              })
              break
            case "text-delta":
              contentChars += e.text.length
              emit("content_block_delta", {
                type: "content_block_delta",
                index: e.index,
                delta: { type: "text_delta", text: e.text },
              })
              break
            case "text-stop":
              emit("content_block_stop", { type: "content_block_stop", index: e.index })
              break
            case "tool-start":
              toolCount++
              activeTools.set(e.index, { id: e.id, name: e.name })
              ensureMessageStart()
              emit("content_block_start", {
                type: "content_block_start",
                index: e.index,
                content_block: {
                  type: "tool_use",
                  id: e.id,
                  name: e.name,
                  input: {},
                },
              })
              break
            case "tool-delta":
              emit("content_block_delta", {
                type: "content_block_delta",
                index: e.index,
                delta: { type: "input_json_delta", partial_json: e.partialJson },
              })
              break
            case "tool-stop":
              activeTools.delete(e.index)
              emit("content_block_stop", { type: "content_block_stop", index: e.index })
              break
            case "finish":
              ensureMessageStart()
              finishUsage = e.usage
              finishStopReason = e.stopReason
              opts.onFinish?.({ stopReason: e.stopReason, usage: e.usage })
              emit("message_delta", {
                type: "message_delta",
                delta: { stop_reason: e.stopReason, stop_sequence: null },
                usage: mapUsageToAnthropic(e.usage),
              })
              emit("message_stop", { type: "message_stop" })
              break
          }
        }
      } catch (err) {
        const activeToolNames = Array.from(activeTools.values(), (t) => t.name)
        const activeToolCalls = Array.from(activeTools.values())
        if (isAbortError(err)) {
          opts.log.info("client disconnected")
        } else if (err instanceof UpstreamStreamError) {
          opts.log.warn("upstream stream error", {
            kind: err.kind,
            message: err.message,
            activeToolNames,
            activeToolCalls,
          })
          // Keep error-only streams as pure error events. Fabricating an empty
          // message_start before the error can leave Claude Code without a
          // final usage frame and trigger internal compact/accounting crashes.
          emit("error", {
            type: "error",
            error: {
              type: err.kind === "rate_limit" ? "rate_limit_error" : "api_error",
              message: err.message,
            },
          })
        } else {
          opts.log.error("stream translation error", {
            err: String(err),
            activeToolNames,
            activeToolCalls,
          })
          emit("error", {
            type: "error",
            error: { type: "api_error", message: String(err) },
          })
        }
      } finally {
        const now = Date.now()
        const timeToFirstChunkMs = opts.requestStartTime && firstChunkAt
          ? firstChunkAt - opts.requestStartTime
          : undefined
        opts.log.debug("stream summary", {
          chunkCount: stats.chunkCount,
          timeToFirstChunkMs,
          streamDurationMs: firstChunkAt ? now - firstChunkAt : undefined,
          totalMs: opts.requestStartTime ? now - opts.requestStartTime : now - streamStart,
          reasoningChars,
          contentChars,
          toolCount,
          stopReason: finishStopReason,
          usage: finishUsage,
        })
        try {
          controller.close()
        } catch {
          // ignore if controller already errored or cancelled
        }
      }
    },
  })
}
