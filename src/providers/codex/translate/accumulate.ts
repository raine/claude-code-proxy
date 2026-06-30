import { mapUsageToAnthropic, reduceUpstream } from "./reducer.ts";
import type { CodexUsage, ReducerEvent } from "./reducer.ts";
import type { Logger } from "../../../log.ts";
import type { TrafficCapture } from "../../types.ts";
import { attachTrafficCapture, createUpstreamStreamDiagnostics } from "./reducer.ts";
import {
  AnthropicNonStreamResponse,
  type AnthropicNonStreamContent,
  createBlockAccumulator,
  defaultAnthropicNonStreamUsage,
  parseToolInputJsonOrRaw,
} from "../../translate/accumulate.ts";
import { buildWebSearchCompatBlocks } from "./web-search-compat.ts";

export { UpstreamStreamError } from "./reducer.ts";

type FinishEvent = Extract<ReducerEvent, { kind: "finish" }>;

export interface AccumulatedResponse {
  response: AnthropicNonStreamResponse;
  rawUsage?: CodexUsage;
  terminalType?: FinishEvent["terminalType"];
  continuationEligible: boolean;
  responseId?: string;
  outputItems: FinishEvent["outputItems"];
}

/**
 * Drive the Codex SSE stream to completion through the shared reducer
 * and fold the ReducerEvents into a single Anthropic non-streaming
 * response object. Throws UpstreamStreamError on rate_limit or failed
 * upstream; server translates to a proper HTTP status.
 */
export async function accumulateResponse(
  upstream: ReadableStream<Uint8Array>,
  opts: { messageId: string; model: string; log: Logger; traffic?: TrafficCapture },
): Promise<AccumulatedResponse> {
  const blockAccumulator = createBlockAccumulator({ includeThinking: true });
  let stopReason: AnthropicNonStreamResponse["stop_reason"] = null;
  let usage: ReturnType<typeof mapUsageToAnthropic> | undefined;
  let rawUsage: CodexUsage | undefined;
  let terminalType: FinishEvent["terminalType"] | undefined;
  let continuationEligible = false;
  let responseId: string | undefined;
  let outputItems: FinishEvent["outputItems"] = [];
  const webSearchEvents: Array<Extract<ReducerEvent, { kind: "web-search" }>> = [];

  const diagnostics = attachTrafficCapture(createUpstreamStreamDiagnostics(), opts.traffic);

  for await (const e of reduceUpstream(upstream, opts.log, diagnostics)) {
    switch (e.kind) {
      case "thinking-start":
        blockAccumulator.onThinkingStart(e.index);
        break;
      case "thinking-delta": {
        blockAccumulator.onThinkingDelta(e.index, e.text);
        break;
      }
      case "text-start":
        blockAccumulator.onTextStart(e.index);
        break;
      case "text-delta": {
        blockAccumulator.onTextDelta(e.index, e.text);
        break;
      }
      case "tool-start":
        blockAccumulator.onToolStart(e.index, e.id, e.name);
        break;
      case "tool-delta": {
        blockAccumulator.onToolDelta(e.index, e.partialJson);
        break;
      }
      case "web-search":
        webSearchEvents.push(e);
        break;
      case "thinking-stop":
      case "text-stop":
      case "tool-stop":
        break;
      case "finish":
        stopReason = e.stopReason;
        rawUsage = e.usage;
        usage = mapUsageToAnthropic(e.usage, { webSearchRequests: e.webSearchRequests });
        terminalType = e.terminalType;
        continuationEligible = e.continuationEligible;
        responseId = e.responseId;
        outputItems = e.outputItems;
        break;
    }
  }

  const accumulatedBlocks = blockAccumulator.orderedBlocks();
  const text = accumulatedBlocks
    .flatMap((block) => (block.kind === "text" ? [block.text] : []))
    .join("");
  const indexedContent: Array<{ index: number; content: AnthropicNonStreamContent }> = [
    ...buildWebSearchCompatBlocks(webSearchEvents, text),
  ];
  for (const block of accumulatedBlocks) {
    if (block.kind === "thinking") {
      if (block.text)
        indexedContent.push({
          index: block.index,
          content: { type: "thinking", thinking: block.text, signature: "" },
        });
    } else if (block.kind === "text") {
      if (block.text)
        indexedContent.push({ index: block.index, content: { type: "text", text: block.text } });
    } else if (block.kind === "tool") {
      indexedContent.push({
        index: block.index,
        content: {
          type: "tool_use",
          id: block.id,
          name: block.name,
          input: parseToolInputJsonOrRaw(block.args),
        },
      });
    }
  }
  const content = indexedContent.sort((a, b) => a.index - b.index).map((item) => item.content);

  return {
    rawUsage,
    terminalType,
    continuationEligible,
    responseId,
    outputItems,
    response: {
      id: opts.messageId,
      type: "message",
      role: "assistant",
      model: opts.model,
      content,
      stop_reason: stopReason,
      stop_sequence: null,
      usage: usage ?? defaultAnthropicNonStreamUsage(),
    },
  };
}
