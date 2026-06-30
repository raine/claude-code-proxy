import { createSseStreamStats, parseSseStream, type SseStreamStats } from "../../../sse.ts";
import type { Logger } from "../../../log.ts";
import { logVerbose } from "../../../config.ts";
import type { TrafficCapture } from "../../types.ts";
import type { TextToolReducerEvent } from "../../translate/accumulate.ts";
import { mapCachedInputUsageToAnthropicUsage } from "../../translate/accumulate.ts";
import type { ResponsesInputItem } from "./request.ts";
import { serverToolUseIdFromCodexWebSearchId } from "./web-search-compat.ts";

export class UpstreamStreamError extends Error {
  constructor(
    public kind: "rate_limit" | "overloaded" | "transient" | "failed",
    message: string,
    public retryAfterSeconds?: number,
  ) {
    super(message);
    this.name = "UpstreamStreamError";
  }
}

export interface CodexUsage {
  input_tokens?: number;
  output_tokens?: number;
  input_tokens_details?: { cached_tokens?: number };
  output_tokens_details?: { reasoning_tokens?: number };
}

export type StopReason = "end_turn" | "tool_use" | "max_tokens";
export type TerminalType = "response.completed" | "response.incomplete" | "response.done";

export type ReducerEvent =
  | TextToolReducerEvent
  | { kind: "thinking-start"; index: number }
  | { kind: "thinking-delta"; index: number; text: string }
  | { kind: "thinking-stop"; index: number }
  | { kind: "tool-progress"; index: number }
  | { kind: "progress" }
  | {
      kind: "web-search";
      index: number;
      resultIndex: number;
      id: string;
      query: string;
    }
  | {
      kind: "finish";
      stopReason: StopReason;
      terminalType: TerminalType;
      continuationEligible: boolean;
      usage: CodexUsage | undefined;
      webSearchRequests: number;
      responseId?: string;
      outputItems: ResponsesInputItem[];
    };

interface TextState {
  kind: "text";
  index: number;
  textAccum: string;
}
interface ToolState {
  kind: "tool";
  index: number;
  outputIndex: number;
  callId: string;
  name: string;
  argsAccum: string;
  deltaCount: number;
  startedAt: number;
  lastProgressLogAt: number;
  largeArgsLogged: boolean;
  hadDelta: boolean;
  bufferUntilDone: boolean;
  emittedArgs: boolean;
}
type BlockState = TextState | ToolState;

const BUFFERED_TOOL_PROGRESS_LOG_INTERVAL_MS = 30_000;
const BUFFERED_TOOL_LARGE_ARGS_BYTES = 1_000_000;
const BUFFERED_TOOL_MAX_ARGS_BYTES = 5_000_000;
const BUFFERED_TOOL_MAX_DURATION_MS = 120_000;
const BUFFERED_READ_REPAIR_TRAILING_WHITESPACE_BYTES = 1_024;

function shouldBufferToolArgs(name: string): boolean {
  return name === "Read";
}

function sanitizeToolArgs(name: string, args: string): string {
  if (name !== "Read" || !args) return args;
  try {
    const parsed = JSON.parse(args);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return args;
    if (!("pages" in parsed) || parsed.pages !== "") return args;
    const sanitized = { ...parsed };
    delete sanitized.pages;
    return JSON.stringify(sanitized);
  } catch {
    return args;
  }
}

function toolArgSummary(args: string): Record<string, unknown> {
  const trimmed = args.trimEnd();
  return {
    length: args.length,
    trimmedLength: trimmed.length,
    trailingWhitespace: args.length - trimmed.length,
    prefix: args.slice(0, 120),
    suffix: args.slice(-120),
  };
}

function toolArgJsonState(args: string): Record<string, unknown> {
  const trimmed = args.trimEnd();
  try {
    const parsed = JSON.parse(trimmed);
    return {
      parseOk: true,
      parsedKeys:
        parsed && typeof parsed === "object" && !Array.isArray(parsed)
          ? Object.keys(parsed)
          : undefined,
      trimmedLength: trimmed.length,
      trailingWhitespace: args.length - trimmed.length,
    };
  } catch (err) {
    return {
      parseOk: false,
      parseError: err instanceof Error ? err.message : String(err),
      trimmedLength: trimmed.length,
      trailingWhitespace: args.length - trimmed.length,
    };
  }
}

function parseReadArgsCandidate(args: string): string | undefined {
  let parsed: unknown;
  try {
    parsed = JSON.parse(args);
  } catch {
    return undefined;
  }
  if (!isValidReadArgs(parsed)) return undefined;
  return sanitizeToolArgs("Read", JSON.stringify(parsed));
}

function isValidReadArgs(value: unknown): value is Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const allowed = new Set(["file_path", "offset", "limit", "pages"]);
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) return false;
  }
  const args = value as Record<string, unknown>;
  if (typeof args.file_path !== "string" || !args.file_path) return false;
  if (
    args.offset !== undefined &&
    (!Number.isSafeInteger(args.offset) || (args.offset as number) < 0)
  )
    return false;
  if (
    args.limit !== undefined &&
    (!Number.isSafeInteger(args.limit) || (args.limit as number) <= 0)
  )
    return false;
  if (args.pages !== undefined && typeof args.pages !== "string") return false;
  return true;
}

function repairWhitespaceStalledReadArgs(log: Logger, state: ToolState): string | undefined {
  if (!state.bufferUntilDone || state.name !== "Read") return undefined;
  const trimmed = state.argsAccum.trimEnd();
  const trailingWhitespace = state.argsAccum.length - trimmed.length;
  if (trailingWhitespace < BUFFERED_READ_REPAIR_TRAILING_WHITESPACE_BYTES) return undefined;

  const repaired = parseReadArgsCandidate(trimmed) ?? parseReadArgsCandidate(`${trimmed}}`);
  if (!repaired) return undefined;

  log.warn("repairing whitespace-stalled Read tool arguments", {
    outputIndex: state.outputIndex,
    index: state.index,
    callId: state.callId,
    name: state.name,
    deltaCount: state.deltaCount,
    elapsedMs: Date.now() - state.startedAt,
    args: toolArgSummary(state.argsAccum),
    repaired: toolArgJsonState(repaired),
  });
  return repaired;
}

function logBufferedToolProgress(log: Logger, state: ToolState, force = false): void {
  if (!state.bufferUntilDone) return;
  const now = Date.now();
  const large = state.argsAccum.length >= BUFFERED_TOOL_LARGE_ARGS_BYTES && !state.largeArgsLogged;
  const stale = now - state.lastProgressLogAt >= BUFFERED_TOOL_PROGRESS_LOG_INTERVAL_MS;
  if (!force && !large && !stale) return;
  if (large) state.largeArgsLogged = true;
  state.lastProgressLogAt = now;
  log.info("buffered tool arguments progress", {
    outputIndex: state.outputIndex,
    index: state.index,
    callId: state.callId,
    name: state.name,
    deltaCount: state.deltaCount,
    elapsedMs: now - state.startedAt,
    args: toolArgSummary(state.argsAccum),
  });
}

function throwIfBufferedToolExceeded(log: Logger, state: ToolState): void {
  if (!state.bufferUntilDone) return;
  const elapsedMs = Date.now() - state.startedAt;
  if (
    state.argsAccum.length <= BUFFERED_TOOL_MAX_ARGS_BYTES &&
    elapsedMs <= BUFFERED_TOOL_MAX_DURATION_MS
  )
    return;
  log.warn("buffered tool arguments exceeded safe limits", {
    outputIndex: state.outputIndex,
    index: state.index,
    callId: state.callId,
    name: state.name,
    deltaCount: state.deltaCount,
    elapsedMs,
    args: toolArgSummary(state.argsAccum),
    json: toolArgJsonState(state.argsAccum),
  });
  throw new UpstreamStreamError(
    "failed",
    `Buffered ${state.name} tool arguments exceeded safe limits`,
  );
}

function describeOpenBlock(outputIndex: number, state: BlockState): Record<string, unknown> {
  if (state.kind === "text") {
    return {
      outputIndex,
      index: state.index,
      kind: state.kind,
      textLength: state.textAccum.length,
    };
  }
  return {
    outputIndex,
    index: state.index,
    kind: state.kind,
    callId: state.callId,
    name: state.name,
    deltaCount: state.deltaCount,
    elapsedMs: Date.now() - state.startedAt,
    bufferUntilDone: state.bufferUntilDone,
    emittedArgs: state.emittedArgs,
    args: toolArgSummary(state.argsAccum),
    json: toolArgJsonState(state.argsAccum),
  };
}

/**
 * Single source of truth for translating Codex Responses SSE into a
 * stream of typed, downstream-agnostic ReducerEvents. Both the streaming
 * and non-streaming frontends consume this generator.
 *
 * Throws UpstreamStreamError on codex.rate_limits.limit_reached or
 * response.failed/response.error. Any usage that arrived before the
 * failure is discarded.
 */
export interface UpstreamStreamDiagnostics {
  stats: SseStreamStats;
  lastEventType?: string;
  sawTerminalEvent: boolean;
  traffic?: TrafficCapture;
}

export function createUpstreamStreamDiagnostics(): UpstreamStreamDiagnostics {
  return {
    stats: createSseStreamStats(),
    sawTerminalEvent: false,
  };
}

export function attachTrafficCapture(
  diagnostics: UpstreamStreamDiagnostics,
  traffic: TrafficCapture | undefined,
): UpstreamStreamDiagnostics {
  diagnostics.traffic = traffic;
  return diagnostics;
}

export async function* reduceUpstream(
  upstream: ReadableStream<Uint8Array>,
  log: Logger,
  diagnostics = createUpstreamStreamDiagnostics(),
): AsyncGenerator<ReducerEvent> {
  const blocksByOutputIndex = new Map<number, BlockState>();
  const outputItemsByIndex = new Map<number, ResponsesInputItem>();
  const itemIdToOutputIndex = new Map<string, number>();
  let anthropicIndex = 0;
  let thinkingIndex: number | undefined;
  let sawToolUse = false;
  let finalUsage: CodexUsage | undefined;
  let responseId: string | undefined;
  let terminalType: TerminalType | undefined;
  let continuationEligible = false;
  let incomplete = false;
  let webSearchRequests = 0;

  function canFinishAfterClosedCompletedToolCall(err: unknown): boolean {
    return (
      sawToolUse &&
      outputItemsByIndex.size > 0 &&
      blocksByOutputIndex.size === 0 &&
      isCodexWebSocketCloseError(err)
    );
  }

  function captureOutputItem(outputIndex: number, state: BlockState): void {
    if (state.kind === "text") {
      if (!state.textAccum) return;
      outputItemsByIndex.set(outputIndex, {
        type: "message",
        role: "assistant",
        content: [{ type: "output_text", text: state.textAccum }],
      });
      return;
    }
    outputItemsByIndex.set(outputIndex, {
      type: "function_call",
      call_id: state.callId,
      name: state.name,
      arguments: state.argsAccum,
    });
  }

  const closeThinking = function* () {
    if (thinkingIndex !== undefined) {
      const idx = thinkingIndex;
      thinkingIndex = undefined;
      yield { kind: "thinking-stop" as const, index: idx };
    }
  };

  const events = parseSseStream(upstream, diagnostics.stats);
  while (true) {
    let next: Awaited<ReturnType<typeof events.next>>;
    try {
      next = await events.next();
    } catch (err) {
      if (canFinishAfterClosedCompletedToolCall(err)) {
        diagnostics.sawTerminalEvent = true;
        terminalType = "response.incomplete";
        incomplete = false;
        continuationEligible = false;
        log.warn("upstream websocket closed after completed tool call", {
          err: err instanceof Error ? err.message : String(err),
          lastEventType: diagnostics.lastEventType,
          stats: diagnostics.stats,
        });
        break;
      }
      throw upstreamReadError(err);
    }
    if (next.done) break;
    const evt = next.value;
    if (!evt.data) continue;
    let p: any;
    try {
      p = JSON.parse(evt.data);
    } catch (err) {
      log.warn("upstream sse: invalid json", { err: String(err), preview: evt.data.slice(0, 200) });
      continue;
    }
    const t: string = p.type || evt.event || "";
    diagnostics.lastEventType = t;
    diagnostics.traffic?.writeJsonEvent("040-upstream-event", p);

    if (logVerbose())
      log.debug("upstream event", { type: t, output_index: p.output_index, item_id: p.item_id });

    if (t === "codex.rate_limits") {
      if (p.rate_limits?.limit_reached) {
        throw new UpstreamStreamError(
          "rate_limit",
          "rate limit reached",
          p.rate_limits?.primary?.reset_after_seconds,
        );
      }
      yield { kind: "progress" };
      continue;
    }
    if (t === "keepalive") {
      yield { kind: "progress" };
      continue;
    }
    if (
      t === "response.web_search_call.in_progress" ||
      t === "response.web_search_call.searching" ||
      t === "response.web_search_call.completed"
    ) {
      yield { kind: "progress" };
      continue;
    }
    if (t === "response.failed" || t === "response.error" || t === "error") {
      const message = p?.response?.error?.message || p?.error?.message || "Upstream error";
      throw new UpstreamStreamError(
        upstreamFailureKind(p, message),
        message,
        retryAfterSecondsFromPayload(p),
      );
    }

    if (t === "response.output_item.added") {
      const item = p.item;
      const outputIndex: number = p.output_index;
      if (!item) continue;
      if (item.type === "reasoning") continue;
      if (item.type === "web_search_call") {
        yield { kind: "progress" };
        continue;
      }
      if (item.type === "message") {
        yield* closeThinking();
        const idx = anthropicIndex++;
        blocksByOutputIndex.set(outputIndex, { kind: "text", index: idx, textAccum: "" });
        if (item.id) itemIdToOutputIndex.set(item.id, outputIndex);
        yield { kind: "text-start", index: idx };
        continue;
      }
      if (item.type === "function_call") {
        yield* closeThinking();
        sawToolUse = true;
        const idx = anthropicIndex++;
        const bufferUntilDone = shouldBufferToolArgs(item.name);
        blocksByOutputIndex.set(outputIndex, {
          kind: "tool",
          index: idx,
          outputIndex,
          callId: item.call_id,
          name: item.name,
          argsAccum: "",
          deltaCount: 0,
          startedAt: Date.now(),
          lastProgressLogAt: Date.now(),
          largeArgsLogged: false,
          hadDelta: false,
          bufferUntilDone,
          emittedArgs: false,
        });
        log.info("tool block started", {
          outputIndex,
          index: idx,
          callId: item.call_id,
          name: item.name,
          bufferUntilDone,
        });
        yield { kind: "tool-start", index: idx, id: item.call_id, name: item.name };
        continue;
      }

      continue;
    }

    if (t === "response.reasoning_summary_part.added") {
      if (thinkingIndex !== undefined) {
        yield { kind: "thinking-delta", index: thinkingIndex, text: "\n\n" };
      }
      continue;
    }
    if (t === "response.reasoning_summary_text.delta") {
      const delta: string = p.delta ?? "";
      if (!delta) continue;
      if (thinkingIndex === undefined) {
        thinkingIndex = anthropicIndex++;
        yield { kind: "thinking-start", index: thinkingIndex };
      }
      yield { kind: "thinking-delta", index: thinkingIndex, text: delta };
      continue;
    }

    if (t === "response.output_text.delta") {
      yield* closeThinking();
      const outputIndex: number | undefined = p.output_index;
      const itemId: string | undefined = p.item_id;
      let state: BlockState | undefined;
      if (typeof outputIndex === "number") state = blocksByOutputIndex.get(outputIndex);
      if (!state && itemId) {
        const mapped = itemIdToOutputIndex.get(itemId);
        if (mapped !== undefined) state = blocksByOutputIndex.get(mapped);
      }
      if (!state || state.kind !== "text") continue;
      const delta: string = p.delta ?? "";
      if (!delta) continue;
      state.textAccum += delta;
      yield { kind: "text-delta", index: state.index, text: delta };
      continue;
    }

    if (t === "response.function_call_arguments.delta") {
      const state = blocksByOutputIndex.get(p.output_index);
      if (!state || state.kind !== "tool") continue;
      const delta: string = p.delta ?? "";
      if (!delta) continue;
      state.argsAccum += delta;
      state.deltaCount += 1;
      state.hadDelta = true;
      logBufferedToolProgress(log, state);
      const repairedArgs = repairWhitespaceStalledReadArgs(log, state);
      if (repairedArgs) {
        state.argsAccum = repairedArgs;
        state.emittedArgs = true;
        captureOutputItem(p.output_index, state);
        yield { kind: "tool-delta", index: state.index, partialJson: state.argsAccum };
        yield { kind: "tool-stop", index: state.index };
        blocksByOutputIndex.delete(p.output_index);
        yield* closeThinking();
        const outputItems = Array.from(outputItemsByIndex.entries())
          .sort(([a], [b]) => a - b)
          .map(([, item]) => item);
        yield {
          kind: "finish",
          stopReason: "tool_use",
          terminalType: "response.incomplete",
          continuationEligible: false,
          usage: undefined,
          webSearchRequests,
          responseId: undefined,
          outputItems,
        };
        await events.return?.(undefined);
        return;
      }
      throwIfBufferedToolExceeded(log, state);
      if (!state.bufferUntilDone) {
        state.emittedArgs = true;
        yield { kind: "tool-delta", index: state.index, partialJson: delta };
      } else {
        yield { kind: "tool-progress", index: state.index };
      }
      continue;
    }

    if (t === "response.function_call_arguments.done") {
      const state = blocksByOutputIndex.get(p.output_index);
      if (!state || state.kind !== "tool") continue;
      if (typeof p.arguments === "string" && !state.argsAccum) {
        state.argsAccum = p.arguments;
      }
      log.info("tool arguments done", {
        outputIndex: p.output_index,
        index: state.index,
        callId: state.callId,
        name: state.name,
        deltaCount: state.deltaCount,
        elapsedMs: Date.now() - state.startedAt,
        args: toolArgSummary(state.argsAccum),
      });
      continue;
    }

    if (t === "response.output_item.done") {
      const item = p.item;
      if (item?.type === "reasoning") {
        yield* closeThinking();
        continue;
      }
      if (item?.type === "web_search_call") {
        const idx = anthropicIndex++;
        const resultIndex = anthropicIndex++;
        webSearchRequests += 1;
        yield {
          kind: "web-search",
          index: idx,
          resultIndex,
          id: serverToolUseIdFromCodexWebSearchId(item.id),
          query: webSearchQuery(item),
        };
        continue;
      }
      const state = blocksByOutputIndex.get(p.output_index);
      if (!state) continue;
      if (!item) {
        log.warn("output item done without item", {
          outputIndex: p.output_index,
          stateKind: state.kind,
        });
        if (state.kind === "text") yield { kind: "text-stop", index: state.index };
        else yield { kind: "tool-stop", index: state.index };
        blocksByOutputIndex.delete(p.output_index);
        continue;
      }
      if (state.kind === "tool") {
        const finalArgs =
          (typeof item.arguments === "string" && item.arguments.length
            ? item.arguments
            : state.argsAccum) || "";
        log.info("tool output item done", {
          outputIndex: p.output_index,
          index: state.index,
          callId: state.callId,
          name: state.name,
          itemType: item.type,
          deltaCount: state.deltaCount,
          elapsedMs: Date.now() - state.startedAt,
          finalArgs: toolArgSummary(finalArgs),
        });
        if (finalArgs.length) {
          state.argsAccum = sanitizeToolArgs(state.name, finalArgs);
          if (state.bufferUntilDone || !state.emittedArgs) {
            state.emittedArgs = true;
            yield { kind: "tool-delta", index: state.index, partialJson: state.argsAccum };
          }
        }
      }
      captureOutputItem(p.output_index, state);
      if (state.kind === "text") {
        log.debug("text block complete", { index: state.index, text: state.textAccum });
        yield { kind: "text-stop", index: state.index };
      } else {
        log.debug("tool block complete", {
          index: state.index,
          callId: state.callId,
          name: state.name,
          args: toolArgSummary(state.argsAccum),
        });
        yield { kind: "tool-stop", index: state.index };
      }
      blocksByOutputIndex.delete(p.output_index);
      continue;
    }

    if (t === "response.completed" || t === "response.incomplete" || t === "response.done") {
      diagnostics.sawTerminalEvent = true;
      terminalType = t;
      responseId = p.response?.id;
      finalUsage = p.response?.usage;
      const reason = p.response?.incomplete_details?.reason;
      if (
        t === "response.incomplete" ||
        reason !== undefined ||
        p.response?.status === "incomplete"
      ) {
        incomplete = true;
      }
      continuationEligible = (t === "response.completed" || t === "response.done") && !incomplete;
      continue;
    }
  }

  const openBlocks = Array.from(blocksByOutputIndex, ([outputIndex, state]) =>
    describeOpenBlock(outputIndex, state),
  );
  if (!diagnostics.sawTerminalEvent || openBlocks.length) {
    log.warn("upstream stream ended without complete response", {
      sawTerminalEvent: diagnostics.sawTerminalEvent,
      lastEventType: diagnostics.lastEventType,
      openBlocks,
      stats: diagnostics.stats,
    });
    throw new UpstreamStreamError(
      "failed",
      diagnostics.sawTerminalEvent
        ? "Upstream stream ended with open output blocks"
        : "Upstream stream ended without a terminal response event",
    );
  }

  yield* closeThinking();

  const stopReason: StopReason = incomplete ? "max_tokens" : sawToolUse ? "tool_use" : "end_turn";
  const outputItems = Array.from(outputItemsByIndex.entries())
    .sort(([a], [b]) => a - b)
    .map(([, item]) => item);
  yield {
    kind: "finish",
    stopReason,
    terminalType: terminalType ?? "response.incomplete",
    continuationEligible,
    usage: finalUsage,
    webSearchRequests,
    responseId,
    outputItems,
  };
}

function webSearchQuery(item: unknown): string {
  if (!item || typeof item !== "object") return "";
  const action = (item as { action?: unknown }).action;
  if (!action || typeof action !== "object") return "";
  const query = (action as { query?: unknown }).query;
  if (typeof query === "string") return query;
  const queries = (action as { queries?: unknown }).queries;
  if (Array.isArray(queries)) {
    const first = queries.find((value): value is string => typeof value === "string");
    return first ?? "";
  }
  return "";
}

function upstreamFailureKind(
  payload: {
    status?: unknown;
    status_code?: unknown;
    response?: { error?: { code?: unknown; type?: unknown } };
    error?: { code?: unknown; type?: unknown };
  },
  message: string,
): "overloaded" | "transient" | "failed" {
  const status = payload.status ?? payload.status_code;
  const code = payload.response?.error?.code ?? payload.error?.code;
  const type = payload.response?.error?.type ?? payload.error?.type;
  const lowerMessage = message.toLowerCase();
  if (
    status === 529 ||
    status === "529" ||
    code === "overloaded_error" ||
    type === "overloaded_error" ||
    lowerMessage.includes("overloaded")
  ) {
    return "overloaded";
  }
  if (
    is5xxStatus(status) ||
    code === "server_error" ||
    code === "internal_server_error" ||
    code === "internal_error" ||
    type === "server_error" ||
    type === "internal_server_error" ||
    type === "internal_error" ||
    isRetryableTransportOrServerMessage(lowerMessage)
  ) {
    return "transient";
  }
  return "failed";
}

function is5xxStatus(status: unknown): boolean {
  const value =
    typeof status === "number" ? status : typeof status === "string" ? Number(status) : NaN;
  return Number.isInteger(value) && value >= 500 && value < 600;
}

function retryAfterSecondsFromPayload(payload: {
  retry_after_seconds?: unknown;
  headers?: Record<string, unknown>;
  response?: { error?: { retry_after_seconds?: unknown } };
  error?: { retry_after_seconds?: unknown };
}): number | undefined {
  const raw =
    payload.response?.error?.retry_after_seconds ??
    payload.error?.retry_after_seconds ??
    payload.retry_after_seconds ??
    payload.headers?.["retry-after"] ??
    payload.headers?.["Retry-After"];
  const value = typeof raw === "number" ? raw : typeof raw === "string" ? Number(raw) : NaN;
  return Number.isFinite(value) && value >= 0 ? value : undefined;
}

function isCodexWebSocketCloseError(err: unknown): boolean {
  const message = err instanceof Error ? err.message : String(err);
  return (
    message.includes("Codex WebSocket connection closed") ||
    message.includes("Codex WebSocket closed before terminal event")
  );
}

function upstreamReadError(err: unknown): Error {
  if (err instanceof UpstreamStreamError) return err;
  const message = err instanceof Error ? err.message : String(err);
  return new UpstreamStreamError(
    isRetryableTransportOrServerMessage(message.toLowerCase()) ? "transient" : "failed",
    `Upstream stream read failed: ${message}`,
  );
}

function isRetryableTransportOrServerMessage(lowerMessage: string): boolean {
  return (
    lowerMessage.includes("you can retry your request") ||
    lowerMessage.includes("socket connection was closed unexpectedly") ||
    lowerMessage.includes("connection closed unexpectedly") ||
    lowerMessage.includes("connection reset") ||
    lowerMessage.includes("operation timed out") ||
    lowerMessage.includes("econnreset") ||
    lowerMessage.includes("epipe") ||
    lowerMessage.includes("etimedout") ||
    lowerMessage.includes("und_err_socket") ||
    lowerMessage.includes("fetch failed")
  );
}

export function mapUsageToAnthropic(
  u: CodexUsage | undefined,
  opts: { webSearchRequests?: number } = {},
): {
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number;
  cache_read_input_tokens: number;
  server_tool_use?: { web_search_requests?: number };
} {
  // OpenAI-style usage reports cached prompt tokens inside input_tokens.
  // Anthropic-style usage reports cache reads separately, and Claude Code
  // sums input_tokens + cache_read_input_tokens when deciding context size.
  // Subtract cached reads here so the downstream total matches the real
  // prompt window instead of double-counting cached context.
  const usage = mapCachedInputUsageToAnthropicUsage({
    inputTokens: u?.input_tokens,
    outputTokens: u?.output_tokens,
    cachedInputTokens: u?.input_tokens_details?.cached_tokens,
  });
  if (opts.webSearchRequests && opts.webSearchRequests > 0) {
    usage.server_tool_use = { web_search_requests: opts.webSearchRequests };
  }
  return usage;
}
