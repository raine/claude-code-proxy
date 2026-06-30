import { describe, expect, it } from "bun:test";
import { reduceUpstream } from "./reducer.ts";
import {
  sse,
  silentLog,
  upstreamFromChunks,
  upstreamThatErrorsAfterChunks,
} from "./test-helpers.ts";

async function events(chunks: string[]) {
  return collectEvents(upstreamFromChunks(chunks));
}

async function collectEvents(upstream: ReadableStream<Uint8Array>) {
  const out = [];
  for await (const event of reduceUpstream(upstream, silentLog)) out.push(event);
  return out;
}

describe("reduceUpstream finish metadata", () => {
  it("captures completed response id and assistant text output items", async () => {
    const out = await events([
      sse("response.output_item.added", {
        output_index: 0,
        item: { type: "message", id: "msg_upstream" },
      }),
      sse("response.output_text.delta", { output_index: 0, delta: "hello" }),
      sse("response.output_item.done", {
        output_index: 0,
        item: { type: "message", id: "msg_upstream" },
      }),
      sse("response.completed", { response: { id: "resp_1", usage: { input_tokens: 3 } } }),
    ]);

    expect(out.at(-1)).toEqual({
      kind: "finish",
      stopReason: "end_turn",
      terminalType: "response.completed",
      continuationEligible: true,
      usage: { input_tokens: 3 },
      webSearchRequests: 0,
      responseId: "resp_1",
      outputItems: [
        { type: "message", role: "assistant", content: [{ type: "output_text", text: "hello" }] },
      ],
    });
  });

  it("captures sanitized Read function call arguments", async () => {
    const out = await events([
      sse("response.output_item.added", {
        output_index: 0,
        item: { type: "function_call", call_id: "call_1", name: "Read" },
      }),
      sse("response.function_call_arguments.done", {
        output_index: 0,
        arguments: '{"file_path":"/tmp/a","pages":""}',
      }),
      sse("response.output_item.done", {
        output_index: 0,
        item: {
          type: "function_call",
          call_id: "call_1",
          name: "Read",
          arguments: '{"file_path":"/tmp/a","pages":""}',
        },
      }),
      sse("response.completed", { response: { id: "resp_1", usage: {} } }),
    ]);

    expect(out.at(-1)).toMatchObject({
      kind: "finish",
      stopReason: "tool_use",
      terminalType: "response.completed",
      continuationEligible: true,
      webSearchRequests: 0,
      responseId: "resp_1",
      outputItems: [
        {
          type: "function_call",
          call_id: "call_1",
          name: "Read",
          arguments: '{"file_path":"/tmp/a"}',
        },
      ],
    });
  });

  it("repairs whitespace-stalled Read function call arguments", async () => {
    const out = await events([
      sse("response.output_item.added", {
        output_index: 0,
        item: { type: "function_call", call_id: "call_1", name: "Read" },
      }),
      sse("response.function_call_arguments.delta", {
        output_index: 0,
        delta: '{"file_path":"/tmp/a","limit":2200',
      }),
      sse("response.function_call_arguments.delta", {
        output_index: 0,
        delta: " ".repeat(1024),
      }),
    ]);

    expect(out).toContainEqual({
      kind: "tool-delta",
      index: 0,
      partialJson: '{"file_path":"/tmp/a","limit":2200}',
    });
    expect(out).toContainEqual({ kind: "tool-stop", index: 0 });
    expect(out.at(-1)).toEqual({
      kind: "finish",
      stopReason: "tool_use",
      terminalType: "response.incomplete",
      continuationEligible: false,
      usage: undefined,
      webSearchRequests: 0,
      responseId: undefined,
      outputItems: [
        {
          type: "function_call",
          call_id: "call_1",
          name: "Read",
          arguments: '{"file_path":"/tmp/a","limit":2200}',
        },
      ],
    });
  });

  it("treats hosted web search response events as progress", async () => {
    const out = await events([
      sse("response.output_item.added", {
        output_index: 0,
        item: { type: "web_search_call", id: "ws_1", status: "in_progress" },
      }),
      sse("response.web_search_call.in_progress", { output_index: 0, item_id: "ws_1" }),
      sse("response.web_search_call.searching", { output_index: 0, item_id: "ws_1" }),
      sse("response.web_search_call.completed", { output_index: 0, item_id: "ws_1" }),
      sse("response.output_item.done", {
        output_index: 0,
        item: { type: "web_search_call", id: "ws_1", status: "completed" },
      }),
      sse("response.output_item.added", {
        output_index: 1,
        item: { type: "message", id: "msg_upstream" },
      }),
      sse("response.output_text.delta", { output_index: 1, delta: "result text" }),
      sse("response.output_item.done", {
        output_index: 1,
        item: { type: "message", id: "msg_upstream" },
      }),
      sse("response.completed", { response: { id: "resp_1", usage: { input_tokens: 3 } } }),
    ]);

    expect(out.filter((event) => event.kind === "progress").length).toBeGreaterThanOrEqual(3);
    expect(out).toContainEqual({
      kind: "web-search",
      index: 0,
      resultIndex: 1,
      id: "srvtoolu_ws_1",
      query: "",
    });
    expect(out.at(-1)).toEqual({
      kind: "finish",
      stopReason: "end_turn",
      terminalType: "response.completed",
      continuationEligible: true,
      usage: { input_tokens: 3 },
      webSearchRequests: 1,
      responseId: "resp_1",
      outputItems: [
        {
          type: "message",
          role: "assistant",
          content: [{ type: "output_text", text: "result text" }],
        },
      ],
    });
  });

  it("finishes completed tool calls when the Codex WebSocket closes before a terminal event", async () => {
    const out = await collectEvents(
      upstreamThatErrorsAfterChunks(
        [
          sse("response.output_item.added", {
            output_index: 0,
            item: { type: "function_call", call_id: "call_1", name: "WebSearch" },
          }),
          sse("response.function_call_arguments.done", {
            output_index: 0,
            arguments: '{"query":"claude-code-proxy github"}',
          }),
          sse("response.output_item.done", {
            output_index: 0,
            item: {
              type: "function_call",
              call_id: "call_1",
              name: "WebSearch",
              arguments: '{"query":"claude-code-proxy github"}',
            },
          }),
        ],
        new Error("Codex WebSocket connection closed"),
      ),
    );

    expect(out.at(-1)).toEqual({
      kind: "finish",
      stopReason: "tool_use",
      terminalType: "response.incomplete",
      continuationEligible: false,
      usage: undefined,
      webSearchRequests: 0,
      responseId: undefined,
      outputItems: [
        {
          type: "function_call",
          call_id: "call_1",
          name: "WebSearch",
          arguments: '{"query":"claude-code-proxy github"}',
        },
      ],
    });
  });

  it("marks response.done as continuation eligible when complete", async () => {
    const out = await events([
      sse("response.done", {
        response: {
          id: "resp_1",
          usage: {},
        },
      }),
    ]);

    expect(out.at(-1)).toMatchObject({
      kind: "finish",
      stopReason: "end_turn",
      terminalType: "response.done",
      continuationEligible: true,
      webSearchRequests: 0,
      responseId: "resp_1",
      outputItems: [],
    });
  });

  it("marks incomplete terminals as max tokens and preserves terminal type", async () => {
    const out = await events([
      sse("response.incomplete", {
        response: {
          id: "resp_1",
          status: "incomplete",
          incomplete_details: { reason: "max_output_tokens" },
          usage: {},
        },
      }),
    ]);

    expect(out.at(-1)).toMatchObject({
      kind: "finish",
      stopReason: "max_tokens",
      terminalType: "response.incomplete",
      continuationEligible: false,
      webSearchRequests: 0,
      responseId: "resp_1",
      outputItems: [],
    });
  });
});

const reasoningSummaryDelta = (delta: string, summaryIndex = 0) =>
  sse("response.reasoning_summary_text.delta", {
    output_index: 0,
    summary_index: summaryIndex,
    delta,
  });
const reasoningItemDone = sse("response.output_item.done", {
  output_index: 0,
  item: { type: "reasoning", summary: [], encrypted_content: "enc" },
});
const messageBlock = [
  sse("response.output_item.added", { output_index: 1, item: { type: "message", id: "m" } }),
  sse("response.output_text.delta", { output_index: 1, delta: "answer" }),
  sse("response.output_item.done", { output_index: 1, item: { type: "message", id: "m" } }),
];
const completed = sse("response.completed", { response: { id: "resp_1", usage: {} } });
const thinkingOf = (out: Awaited<ReturnType<typeof events>>) =>
  out.filter(
    (e) => e.kind === "thinking-start" || e.kind === "thinking-delta" || e.kind === "thinking-stop",
  );

describe("reduceUpstream reasoning summaries", () => {
  it("emits thinking events from summary deltas, before and below the text block", async () => {
    const out = await events([
      sse("response.reasoning_summary_part.added", {
        output_index: 0,
        summary_index: 0,
        part: { type: "summary_text", text: "" },
      }),
      reasoningSummaryDelta("Plan"),
      reasoningSummaryDelta("ning"),
      reasoningItemDone,
      ...messageBlock,
      completed,
    ]);

    expect(thinkingOf(out)).toEqual([
      { kind: "thinking-start", index: 0 },
      { kind: "thinking-delta", index: 0, text: "Plan" },
      { kind: "thinking-delta", index: 0, text: "ning" },
      { kind: "thinking-stop", index: 0 },
    ]);
    const stopAt = out.findIndex((e) => e.kind === "thinking-stop");
    const textStartAt = out.findIndex((e) => e.kind === "text-start");
    expect(stopAt).toBeGreaterThanOrEqual(0);
    expect(stopAt).toBeLessThan(textStartAt);
    expect(out.find((e) => e.kind === "text-start")).toEqual({ kind: "text-start", index: 1 });
  });

  it("emits no thinking for a reasoning item with an empty summary (trivial turn)", async () => {
    const out = await events([
      sse("response.output_item.added", {
        output_index: 0,
        item: { type: "reasoning", summary: [], encrypted_content: "enc" },
      }),
      reasoningItemDone,
      ...messageBlock,
      completed,
    ]);

    expect(thinkingOf(out)).toEqual([]);
  });

  it("closes thinking before text even if the summary text.done never arrives", async () => {
    const out = await events([reasoningSummaryDelta("hmm"), ...messageBlock, completed]);

    const stopAt = out.findIndex((e) => e.kind === "thinking-stop");
    const textStartAt = out.findIndex((e) => e.kind === "text-start");
    expect(stopAt).toBeGreaterThanOrEqual(0);
    expect(stopAt).toBeLessThan(textStartAt);
  });

  it("defensively closes an open thinking block at stream end", async () => {
    const out = await events([reasoningSummaryDelta("dangling"), completed]);

    expect(thinkingOf(out)).toEqual([
      { kind: "thinking-start", index: 0 },
      { kind: "thinking-delta", index: 0, text: "dangling" },
      { kind: "thinking-stop", index: 0 },
    ]);
    expect(out.filter((e) => e.kind === "thinking-stop")).toHaveLength(1);
  });

  it("concatenates multiple summary parts into one thinking block", async () => {
    const out = await events([
      reasoningSummaryDelta("part one", 0),
      sse("response.reasoning_summary_part.added", {
        output_index: 0,
        summary_index: 1,
        part: { type: "summary_text", text: "" },
      }),
      reasoningSummaryDelta("part two", 1),
      reasoningItemDone,
      ...messageBlock,
      completed,
    ]);

    expect(thinkingOf(out)).toEqual([
      { kind: "thinking-start", index: 0 },
      { kind: "thinking-delta", index: 0, text: "part one" },
      { kind: "thinking-delta", index: 0, text: "\n\n" },
      { kind: "thinking-delta", index: 0, text: "part two" },
      { kind: "thinking-stop", index: 0 },
    ]);
  });

  it("handles two reasoning items in one response", async () => {
    const out = await events([
      reasoningSummaryDelta("first"),
      reasoningItemDone,
      sse("response.output_item.added", {
        output_index: 1,
        item: { type: "function_call", call_id: "call_1", name: "Read" },
      }),
      sse("response.function_call_arguments.done", {
        output_index: 1,
        arguments: '{"file_path":"/tmp/a"}',
      }),
      sse("response.output_item.done", {
        output_index: 1,
        item: { type: "function_call", call_id: "call_1", name: "Read" },
      }),
      sse("response.reasoning_summary_text.delta", {
        output_index: 2,
        summary_index: 0,
        delta: "second",
      }),
      sse("response.output_item.done", {
        output_index: 2,
        item: { type: "reasoning", summary: [], encrypted_content: "enc" },
      }),
      completed,
    ]);

    expect(out.filter((e) => e.kind === "thinking-start")).toHaveLength(2);
    expect(out.filter((e) => e.kind === "thinking-stop")).toHaveLength(2);
    expect(
      out.filter((e) => e.kind === "thinking-delta").map((e) => (e as { text: string }).text),
    ).toEqual(["first", "second"]);
  });

  it("closes thinking before the Read-repair synthetic finish", async () => {
    const out = await events([
      reasoningSummaryDelta("deciding"),
      sse("response.output_item.added", {
        output_index: 1,
        item: { type: "function_call", call_id: "call_1", name: "Read" },
      }),
      sse("response.function_call_arguments.delta", {
        output_index: 1,
        delta: '{"file_path":"/tmp/a","limit":2200',
      }),
      sse("response.function_call_arguments.delta", { output_index: 1, delta: " ".repeat(1024) }),
    ]);

    const stopAt = out.findIndex((e) => e.kind === "thinking-stop");
    const finishAt = out.findIndex((e) => e.kind === "finish");
    const toolStartAt = out.findIndex((e) => e.kind === "tool-start");
    expect(stopAt).toBeGreaterThanOrEqual(0);
    expect(stopAt).toBeLessThan(toolStartAt);
    expect(stopAt).toBeLessThan(finishAt);
    expect(out.at(-1)).toMatchObject({ kind: "finish", stopReason: "tool_use" });
  });
});
