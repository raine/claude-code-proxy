import { expect, test } from "bun:test"
import type { Logger } from "../../../log.ts"
import { accumulateResponse } from "./accumulate.ts"

const noopLogger: Logger = {
  debug() {},
  info() {},
  warn() {},
  error() {},
  child() {
    return noopLogger
  },
}

function sseEvent(event: string, data: unknown): string {
  return `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`
}

function upstreamFromEvents(events: Array<{ event: string; data: unknown }>): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder()
  return new ReadableStream<Uint8Array>({
    start(controller) {
      for (const e of events) controller.enqueue(encoder.encode(sseEvent(e.event, e.data)))
      controller.close()
    },
  })
}

test("accumulateResponse maps compact responses to Anthropic compaction blocks", async () => {
  const upstream = upstreamFromEvents([
    {
      event: "response.output_item.added",
      data: {
        type: "response.output_item.added",
        output_index: 0,
        item: { type: "message", id: "msg_1" },
      },
    },
    {
      event: "response.output_text.delta",
      data: {
        type: "response.output_text.delta",
        output_index: 0,
        delta: "compact summary",
      },
    },
    {
      event: "response.output_item.done",
      data: {
        type: "response.output_item.done",
        output_index: 0,
        item: { type: "message" },
      },
    },
    {
      event: "response.completed",
      data: { type: "response.completed", response: { usage: { input_tokens: 10, output_tokens: 4 } } },
    },
  ])

  const result = await accumulateResponse(upstream, {
    messageId: "msg_test",
    model: "gpt-5.5",
    log: noopLogger,
    compactResponse: true,
  })

  expect(result.response.content).toEqual([{ type: "compaction", content: "compact summary" }])
  expect(result.response.stop_reason).toBe("compaction")
})
