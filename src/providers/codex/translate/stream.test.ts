import { expect, test } from "bun:test"
import type { Logger } from "../../../log.ts"
import { translateStream } from "./stream.ts"

const noopLogger: Logger = {
  debug() {},
  info() {},
  warn() {},
  error() {},
  child() {
    return noopLogger
  },
}

function erroringUpstream(message: string): ReadableStream<Uint8Array> {
  return new ReadableStream<Uint8Array>({
    start(controller) {
      controller.error(new Error(message))
    },
  })
}

function abortErroringUpstream(): ReadableStream<Uint8Array> {
  return new ReadableStream<Uint8Array>({
    start(controller) {
      controller.error(new DOMException("The connection was closed.", "AbortError"))
    },
  })
}

async function collect(stream: ReadableStream<Uint8Array>): Promise<string> {
  const reader = stream.getReader()
  const decoder = new TextDecoder()
  let out = ""
  while (true) {
    const { value, done } = await reader.read()
    if (done) break
    out += decoder.decode(value, { stream: true })
  }
  out += decoder.decode()
  return out
}

test("translateStream emits only an error event for pre-content upstream failures", async () => {
  const sse = await collect(
    translateStream(erroringUpstream("boom"), {
      messageId: "msg_test",
      model: "gpt-5.4",
      log: noopLogger,
    }),
  )

  expect(sse).toContain("event: error")
  expect(sse).not.toContain("event: message_start")
})


test("translateStream reports upstream aborts as SSE errors when the client is still connected", async () => {
  const sse = await collect(
    translateStream(abortErroringUpstream(), {
      messageId: "msg_test",
      model: "gpt-5.5",
      log: noopLogger,
      signal: new AbortController().signal,
    }),
  )

  expect(sse).toContain("event: error")
  expect(sse).toContain("AbortError")
})

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

function parseSseEvents(sse: string): Array<{ event?: string; data: any }> {
  return sse
    .trim()
    .split(/\n\n+/)
    .filter(Boolean)
    .map((block) => {
      let event: string | undefined
      const dataLines: string[] = []
      for (const line of block.split(/\n/)) {
        if (line.startsWith("event: ")) event = line.slice("event: ".length)
        if (line.startsWith("data: ")) dataLines.push(line.slice("data: ".length))
      }
      return { event, data: JSON.parse(dataLines.join("\n")) }
    })
}

test("translateStream emits complete tool blocks only after upstream output_item.done", async () => {
  const upstream = upstreamFromEvents([
    {
      event: "response.output_item.added",
      data: {
        type: "response.output_item.added",
        output_index: 0,
        item: { type: "function_call", call_id: "call_1", name: "Edit" },
      },
    },
    {
      event: "response.function_call_arguments.delta",
      data: {
        type: "response.function_call_arguments.delta",
        output_index: 0,
        delta: "{\"file_path\":\"/tmp/a\",",
      },
    },
    {
      event: "response.function_call_arguments.delta",
      data: {
        type: "response.function_call_arguments.delta",
        output_index: 0,
        delta: "\"old_string\":\"a\",\"new_string\":\"b\"}",
      },
    },
    {
      event: "response.output_item.done",
      data: {
        type: "response.output_item.done",
        output_index: 0,
        item: {
          type: "function_call",
          call_id: "call_1",
          name: "Edit",
          arguments: "{\"file_path\":\"/tmp/a\",\"old_string\":\"a\",\"new_string\":\"b\"}",
        },
      },
    },
    {
      event: "response.completed",
      data: {
        type: "response.completed",
        response: { usage: { input_tokens: 10, output_tokens: 4 } },
      },
    },
  ])

  const sse = await collect(
    translateStream(upstream, {
      messageId: "msg_test",
      model: "gpt-5.5",
      log: noopLogger,
    }),
  )
  const events = parseSseEvents(sse)
  const eventNames = events.map((e) => e.event)

  expect(eventNames).toEqual([
    "message_start",
    "ping",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "message_delta",
    "message_stop",
  ])
  const start = events[2]
  const delta = events[3]
  const messageDelta = events[5]
  expect(start).toBeDefined()
  expect(delta).toBeDefined()
  expect(messageDelta).toBeDefined()
  expect(start!.data.content_block).toEqual({
    type: "tool_use",
    id: "call_1",
    name: "Edit",
    input: {},
  })
  expect(delta!.data.delta).toEqual({
    type: "input_json_delta",
    partial_json: "{\"file_path\":\"/tmp/a\",\"old_string\":\"a\",\"new_string\":\"b\"}",
  })
  expect(messageDelta!.data.delta.stop_reason).toBe("tool_use")
})

test("translateStream does not expose a partial tool_use block when upstream fails mid-arguments", async () => {
  const encoder = new TextEncoder()
  const upstream = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(encoder.encode(sseEvent("response.output_item.added", {
        type: "response.output_item.added",
        output_index: 0,
        item: { type: "function_call", call_id: "call_1", name: "Edit" },
      })))
      controller.enqueue(encoder.encode(sseEvent("response.function_call_arguments.delta", {
        type: "response.function_call_arguments.delta",
        output_index: 0,
        delta: "{\"file_path\":",
      })))
      controller.enqueue(encoder.encode(sseEvent("response.failed", {
        type: "response.failed",
        response: { error: { message: "stream_read_error" } },
      })))
      controller.close()
    },
  })

  const sse = await collect(
    translateStream(upstream, {
      messageId: "msg_test",
      model: "gpt-5.5",
      log: noopLogger,
    }),
  )

  expect(sse).toContain("event: error")
  expect(sse).not.toContain("event: content_block_start")
  expect(sse).not.toContain("input_json_delta")
})
