import { beforeEach, describe, expect, it } from "bun:test"
import { encodeSseEvent } from "../../../sse.ts"
import { reduceUpstream, type ReducerEvent } from "./reducer.ts"
import { translateRequest } from "./request.ts"
import { clearGeminiToolSignatures } from "./signature-cache.ts"

const encoder = new TextEncoder()

function streamFromEvents(events: unknown[]): ReadableStream<Uint8Array> {
  return new ReadableStream({
    start(controller) {
      for (const event of events) controller.enqueue(encoder.encode(encodeSseEvent("message", event)))
      controller.close()
    },
  })
}

async function collect(events: unknown[]): Promise<ReducerEvent[]> {
  const collected: ReducerEvent[] = []
  for await (const event of reduceUpstream(streamFromEvents(events))) collected.push(event)
  return collected
}

describe("reduceUpstream", () => {
  beforeEach(() => {
    clearGeminiToolSignatures()
  })

  it("passes valid function calls through as tool calls", async () => {
    const events = await collect([
      {
        response: {
          candidates: [
            {
              content: {
                parts: [
                  {
                    functionCall: {
                      id: "call_1",
                      name: "Read",
                      args: { file_path: "a.ts" },
                    },
                  },
                ],
              },
              finishReason: "STOP",
            },
          ],
          usageMetadata: { promptTokenCount: 5, candidatesTokenCount: 2 },
        },
      },
    ])

    expect(events).toEqual([
      { kind: "tool-start", index: 0, id: "call_1", name: "Read" },
      { kind: "tool-delta", index: 0, partialJson: '{"file_path":"a.ts"}' },
      { kind: "tool-stop", index: 0 },
      {
        kind: "finish",
        stopReason: "tool_use",
        usage: { promptTokenCount: 5, candidatesTokenCount: 2 },
      },
    ])
  })

  it("preserves Gemini tool call thought signatures for the next request", async () => {
    const thoughtSignature = "x".repeat(64)
    await collect([
      {
        response: {
          candidates: [
            {
              content: {
                parts: [
                  {
                    thoughtSignature,
                    functionCall: {
                      id: "call_1",
                      name: "Read",
                      args: { file_path: "a.ts" },
                    },
                  },
                ],
              },
              finishReason: "STOP",
            },
          ],
        },
      },
    ])

    const translated = translateRequest({
      model: "gemini-3.1-pro-preview",
      messages: [
        {
          role: "assistant",
          content: [
            {
              type: "tool_use",
              id: "call_1",
              name: "Read",
              input: { file_path: "a.ts" },
            },
          ],
        },
      ],
    })

    expect(translated.contents[0]?.parts[0]).toEqual({
      functionCall: {
        id: "call_1",
        name: "Read",
        args: { file_path: "a.ts" },
      },
      thoughtSignature,
    })
  })

  it("turns invalid AskUserQuestion calls into assistant text", async () => {
    const question = "Hello! It looks like you are testing the models. What would you like to plan today?"
    const events = await collect([
      {
        response: {
          candidates: [
            {
              content: {
                parts: [
                  {
                    functionCall: {
                      id: "call_2",
                      name: "AskUserQuestion",
                      args: {
                        questions: [
                          {
                            question,
                            multiSelect: false,
                            options: [{ label: "Other", description: "Provide details about the task to plan." }],
                            header: "Goal",
                          },
                        ],
                      },
                    },
                  },
                ],
              },
              finishReason: "STOP",
            },
          ],
        },
      },
    ])

    expect(events).toEqual([
      { kind: "text-start", index: 0 },
      { kind: "text-delta", index: 0, text: question },
      { kind: "text-stop", index: 0 },
      { kind: "finish", stopReason: "end_turn", usage: undefined },
    ])
  })

  it("does not report tool_use when Gemini reports a malformed function call without an emitted tool", async () => {
    const events = await collect([
      {
        response: {
          candidates: [{ finishReason: "MALFORMED_FUNCTION_CALL" }],
        },
      },
    ])

    expect(events).toEqual([{ kind: "finish", stopReason: "end_turn", usage: undefined }])
  })
})
