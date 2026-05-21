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
      model: "kimi-for-coding",
      log: noopLogger,
    }),
  )

  expect(sse).toContain("event: error")
  expect(sse).not.toContain("event: message_start")
})
