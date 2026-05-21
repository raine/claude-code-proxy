import { encodeSseEvent } from "../../../sse.ts"
import type { Logger } from "../../../log.ts"
import { mapRateLimitsSnapshot, type RateLimitsSidecarWriter } from "../rate-limits.ts"
import type { RateLimitsTracker } from "../client.ts"
import { mapUsageToAnthropic, reduceUpstream, UpstreamStreamError } from "./reducer.ts"

function isAbortError(err: unknown): boolean {
  return err instanceof Error && err.name === "AbortError"
}

export function translateStream(
  upstream: ReadableStream<Uint8Array>,
  opts: {
    messageId: string
    model: string
    log: Logger
    onFinish?: (finish: { stopReason: "end_turn" | "tool_use" | "max_tokens"; usage?: Parameters<typeof mapUsageToAnthropic>[0] }) => void
    accountId?: string
    rateLimitsWriter?: RateLimitsSidecarWriter
    rateLimitsTracker?: RateLimitsTracker
    signal?: AbortSignal
  },
): ReadableStream<Uint8Array> {
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
  const encoder = new TextEncoder()
  return new ReadableStream<Uint8Array>({
    async start(controller) {
      let controllerClosed = false
      const emit = (event: string, data: unknown): boolean => {
        if (controllerClosed) return false
        try {
          controller.enqueue(encoder.encode(encodeSseEvent(event, data)))
          return true
        } catch {
          controllerClosed = true
          return false
        }
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

      try {
        for await (const e of reduceUpstream(trackedUpstream, opts.log)) {
          switch (e.kind) {
            case "text-start":
              ensureMessageStart()
              emit("content_block_start", {
                type: "content_block_start",
                index: e.index,
                content_block: { type: "text", text: "" },
              })
              break
            case "text-delta":
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
            case "rate-limits": {
              opts.rateLimitsTracker?.markSeen()
              const snapshot = mapRateLimitsSnapshot(e.rateLimits, opts.accountId)
              if (snapshot) void opts.rateLimitsWriter?.write(snapshot)
              break
            }
            case "finish":
              ensureMessageStart()
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
        const activeToolNames = Array.from(activeTools.values(), (tool) => tool.name)
        const activeToolCalls = Array.from(activeTools.values())
        if (isAbortError(err) && opts.signal?.aborted) {
          opts.log.info("client disconnected")
        } else if (err instanceof UpstreamStreamError) {
          opts.log.warn("upstream stream error", {
            kind: err.kind,
            message: err.message,
            activeToolNames,
            activeToolCalls,
          })
          // If upstream fails before any content starts, emitting a synthetic
          // message_start without a matching final usage frame can trigger
          // Claude Code's internal usage accounting crash during /compact.
          // In that case, surface a plain SSE error event instead.
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
        if (!controllerClosed) {
          try {
            controller.close()
          } catch {
            // ignore if controller already errored or cancelled
          }
        }
      }
    },
  })
}
