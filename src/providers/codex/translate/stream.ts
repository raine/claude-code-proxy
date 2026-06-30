import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { stateDir } from "../../../paths.ts";
import { encodeSseEvent } from "../../../sse.ts";
import type { Logger } from "../../../log.ts";
import type { TrafficCapture } from "../../types.ts";
import type { CodexRequestSizeSummary } from "../request-summary.ts";
import {
  attachTrafficCapture,
  createUpstreamStreamDiagnostics,
  mapUsageToAnthropic,
  reduceUpstream,
  UpstreamStreamError,
  type ReducerEvent,
} from "./reducer.ts";
import { emitMessageStart } from "../../shared/anthropic-sse.ts";
import { buildWebSearchCompatBlocks } from "./web-search-compat.ts";
import { computeBackoffDelay, sleep, type BackoffOutcome } from "../../retry.ts";

/**
 * Translate a Codex Responses SSE stream into Anthropic SSE events.
 * Returns a ReadableStream<Uint8Array> ready to pipe to the client.
 *
 * The HTTP status has already been flushed (200) before the first
 * upstream event is consumed, so rate-limit and upstream-failed cases
 * surface as SSE error events rather than non-200 statuses.
 */
const KEEPALIVE_INTERVAL_MS = 15_000;
const STREAM_WATCHDOG_INTERVAL_MS = 30_000;
const MAX_RETRYABLE_STREAM_RETRIES = 10;

interface RetryableUpstream {
  body: ReadableStream<Uint8Array>;
  headers?: Headers;
  requestSize?: CodexRequestSizeSummary;
}

export function translateStream(
  upstream: ReadableStream<Uint8Array>,
  opts: {
    messageId: string;
    model: string;
    log: Logger;
    reqId?: string;
    signal?: AbortSignal;
    upstreamHeaders?: Headers;
    traffic?: TrafficCapture;
    requestSize?: CodexRequestSizeSummary;
    onFinish?: (finish: Extract<ReducerEvent, { kind: "finish" }>) => void;
    onInvalidateContinuation?: () => void;
    retryUpstream?: () => Promise<RetryableUpstream>;
    maxRetryableStreamRetries?: number;
    computeRetryDelay?: (attempt: number, retryAfter?: string) => BackoffOutcome;
  },
): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  return new ReadableStream<Uint8Array>({
    async start(controller) {
      const openBlocks = new Map<
        number,
        { type: "text" | "tool" | "thinking"; id?: string; name?: string }
      >();
      let diagnostics = attachTrafficCapture(createUpstreamStreamDiagnostics(), opts.traffic);
      let closed = false;
      let messageStarted = false;
      let lastEmitAt = 0;
      let lastEmitEvent: string | undefined;
      let lastReducerEvent: string | undefined;
      const webSearchEvents: Array<Extract<ReducerEvent, { kind: "web-search" }>> = [];
      const deferredContentEvents: ReducerEvent[] = [];
      const startedAt = Date.now();
      const safeClose = () => {
        if (closed) return;
        closed = true;
        try {
          controller.close();
        } catch {
          // The stream can be closed by cancellation before the producer observes it.
        }
      };
      const emit = (event: string, data: unknown) => {
        if (closed || opts.signal?.aborted || controller.desiredSize === null) return false;
        try {
          opts.traffic?.writeJsonEvent("050-downstream-event", { event, data });
          controller.enqueue(encoder.encode(encodeSseEvent(event, data)));
          lastEmitAt = Date.now();
          lastEmitEvent = event;
          return true;
        } catch (err) {
          if (opts.signal?.aborted || isClosedControllerError(err)) {
            closed = true;
            return false;
          }
          throw err;
        }
      };
      const emitPingIfStale = () => {
        if (!messageStarted || Date.now() - lastEmitAt < KEEPALIVE_INTERVAL_MS) return;
        emit("ping", { type: "ping" });
      };
      const activeToolCalls = () =>
        Array.from(openBlocks.values()).filter(
          (block): block is { type: "tool"; id: string; name: string } =>
            block.type === "tool" && typeof block.id === "string" && typeof block.name === "string",
        );
      const closeOpenBlocks = () => {
        for (const [index] of openBlocks) {
          emit("content_block_stop", { type: "content_block_stop", index });
        }
        openBlocks.clear();
      };
      const resetBeforeRetry = () => {
        openBlocks.clear();
        webSearchEvents.length = 0;
        deferredContentEvents.length = 0;
        lastReducerEvent = undefined;
      };
      const logWatchdog = () => {
        const tools = activeToolCalls();
        opts.log.info("codex stream watchdog", {
          elapsedMs: Date.now() - startedAt,
          activeToolNames: tools.map((tool) => tool.name),
          activeToolCalls: tools,
          openBlocks: Array.from(openBlocks.entries(), ([index, block]) => ({ index, ...block })),
          messageStarted,
          lastReducerEvent,
          lastEmitEvent,
          msSinceLastEmit: lastEmitAt ? Date.now() - lastEmitAt : undefined,
          diagnostics: describeDiagnostics(diagnostics),
        });
      };
      const watchdog = setInterval(logWatchdog, STREAM_WATCHDOG_INTERVAL_MS);
      const ensureMessageStart = () => {
        if (messageStarted) return;
        messageStarted = true;
        emitMessageStart(emit, { messageId: opts.messageId, model: opts.model });
      };
      const textFromDeferredContent = () =>
        deferredContentEvents
          .flatMap((event) => (event.kind === "text-delta" ? [event.text] : []))
          .join("");
      const emitWebSearchCompatBlocks = () => {
        if (!webSearchEvents.length) return;
        ensureMessageStart();
        for (const block of buildWebSearchCompatBlocks(
          webSearchEvents,
          textFromDeferredContent(),
        )) {
          if (block.content.type === "server_tool_use") {
            emit("content_block_start", {
              type: "content_block_start",
              index: block.index,
              content_block: {
                type: "server_tool_use",
                id: block.content.id,
                name: block.content.name,
                input: {},
              },
            });
            emit("content_block_delta", {
              type: "content_block_delta",
              index: block.index,
              delta: {
                type: "input_json_delta",
                partial_json: JSON.stringify(block.content.input),
              },
            });
            emit("content_block_stop", { type: "content_block_stop", index: block.index });
            continue;
          }
          emit("content_block_start", {
            type: "content_block_start",
            index: block.index,
            content_block: block.content,
          });
          emit("content_block_stop", { type: "content_block_stop", index: block.index });
        }
      };
      const isContentEvent = (event: ReducerEvent): boolean =>
        event.kind === "thinking-start" ||
        event.kind === "thinking-delta" ||
        event.kind === "thinking-stop" ||
        event.kind === "text-start" ||
        event.kind === "text-delta" ||
        event.kind === "text-stop" ||
        event.kind === "tool-start" ||
        event.kind === "tool-delta" ||
        event.kind === "tool-stop";
      const emitContentEvent = (e: ReducerEvent) => {
        switch (e.kind) {
          case "thinking-start":
            openBlocks.set(e.index, { type: "thinking" });
            ensureMessageStart();
            emit("content_block_start", {
              type: "content_block_start",
              index: e.index,
              content_block: { type: "thinking", thinking: "", signature: "" },
            });
            break;
          case "thinking-delta":
            emit("content_block_delta", {
              type: "content_block_delta",
              index: e.index,
              delta: { type: "thinking_delta", thinking: e.text },
            });
            break;
          case "thinking-stop":
            openBlocks.delete(e.index);
            emit("content_block_stop", { type: "content_block_stop", index: e.index });
            break;
          case "text-start":
            openBlocks.set(e.index, { type: "text" });
            ensureMessageStart();
            emit("content_block_start", {
              type: "content_block_start",
              index: e.index,
              content_block: { type: "text", text: "" },
            });
            break;
          case "text-delta":
            emit("content_block_delta", {
              type: "content_block_delta",
              index: e.index,
              delta: { type: "text_delta", text: e.text },
            });
            break;
          case "text-stop":
            openBlocks.delete(e.index);
            emit("content_block_stop", { type: "content_block_stop", index: e.index });
            break;
          case "tool-start":
            openBlocks.set(e.index, { type: "tool", id: e.id, name: e.name });
            ensureMessageStart();
            emit("content_block_start", {
              type: "content_block_start",
              index: e.index,
              content_block: {
                type: "tool_use",
                id: e.id,
                name: e.name,
                input: {},
              },
            });
            break;
          case "tool-delta":
            emit("content_block_delta", {
              type: "content_block_delta",
              index: e.index,
              delta: { type: "input_json_delta", partial_json: e.partialJson },
            });
            break;
          case "tool-stop":
            openBlocks.delete(e.index);
            emit("content_block_stop", { type: "content_block_stop", index: e.index });
            break;
        }
      };

      let currentUpstream = upstream;
      let currentUpstreamHeaders = opts.upstreamHeaders;
      let currentRequestSize = opts.requestSize;
      let retryAttempt = 0;
      let terminalError: unknown;
      try {
        for (;;) {
          diagnostics = attachTrafficCapture(createUpstreamStreamDiagnostics(), opts.traffic);
          try {
            for await (const e of reduceUpstream(currentUpstream, opts.log, diagnostics)) {
              lastReducerEvent = e.kind;
              if (e.kind === "web-search") {
                webSearchEvents.push(e);
                continue;
              }
              if (webSearchEvents.length && isContentEvent(e)) {
                deferredContentEvents.push(e);
                continue;
              }
              switch (e.kind) {
                case "thinking-start":
                case "thinking-delta":
                case "thinking-stop":
                case "text-start":
                case "text-delta":
                case "text-stop":
                case "tool-start":
                case "tool-delta":
                case "tool-stop":
                  emitContentEvent(e);
                  break;
                case "tool-progress":
                case "progress":
                  emitPingIfStale();
                  break;
                case "finish":
                  if (openBlocks.size) {
                    throw new UpstreamStreamError(
                      "failed",
                      "Stream finished with open content blocks",
                    );
                  }
                  emitWebSearchCompatBlocks();
                  for (const event of deferredContentEvents) {
                    emitContentEvent(event);
                  }
                  if (openBlocks.size) {
                    throw new UpstreamStreamError(
                      "failed",
                      "Stream finished with open content blocks",
                    );
                  }
                  ensureMessageStart();
                  opts.onFinish?.(e);
                  emit("message_delta", {
                    type: "message_delta",
                    delta: { stop_reason: e.stopReason, stop_sequence: null },
                    usage: mapUsageToAnthropic(e.usage, {
                      webSearchRequests: e.webSearchRequests,
                    }),
                  });
                  emit("message_stop", { type: "message_stop" });
                  break;
              }
            }
            break;
          } catch (err) {
            if (!(await retryPreMessageStreamError(err))) {
              terminalError ??= err;
              break;
            }
          }
        }
        if (terminalError) throw terminalError;
      } catch (err) {
        const activeToolCalls = Array.from(openBlocks.values()).filter(
          (block): block is { type: "tool"; id: string; name: string } =>
            block.type === "tool" && typeof block.id === "string" && typeof block.name === "string",
        );
        const activeToolNames = activeToolCalls.map((tool) => tool.name);
        const openBlockDetails = Array.from(openBlocks.entries(), ([index, block]) => ({
          index,
          ...block,
        }));
        opts.onInvalidateContinuation?.();
        if (opts.signal?.aborted) {
          opts.log.info("stream cancelled", {
            err: describeError(err),
            activeToolNames,
            activeToolCalls,
            openBlocks: openBlockDetails,
            diagnostics: describeDiagnostics(diagnostics),
          });
          return;
        }
        if (err instanceof UpstreamStreamError) {
          const detail = {
            kind: err.kind,
            message: err.message,
            activeToolNames,
            activeToolCalls,
            openBlocks: openBlockDetails,
            clientAborted: opts.signal?.aborted ?? false,
            diagnostics: describeDiagnostics(diagnostics),
            upstreamHeaders: describeHeaders(currentUpstreamHeaders),
            requestSize: currentRequestSize,
          };
          const diagnosticFile = await writeDiagnosticFile(
            opts.reqId,
            "upstream-stream-error",
            detail,
          );
          opts.log.warn("upstream stream error", { ...detail, diagnosticFile });
          ensureMessageStart();
          closeOpenBlocks();
          emit("error", {
            type: "error",
            error: {
              type: anthropicStreamErrorType(err),
              message: err.message,
            },
          });
        } else {
          const detail = {
            err: describeError(err),
            activeToolNames,
            activeToolCalls,
            openBlocks: openBlockDetails,
            clientAborted: opts.signal?.aborted ?? false,
            diagnostics: describeDiagnostics(diagnostics),
            upstreamHeaders: describeHeaders(currentUpstreamHeaders),
            requestSize: currentRequestSize,
          };
          const diagnosticFile = await writeDiagnosticFile(
            opts.reqId,
            "stream-translation-error",
            detail,
          );
          opts.log.error("stream translation error", { ...detail, diagnosticFile });
          ensureMessageStart();
          closeOpenBlocks();
          emit("error", {
            type: "error",
            error: { type: "api_error", message: String(err) },
          });
        }
      } finally {
        clearInterval(watchdog);
        safeClose();
      }

      async function retryPreMessageStreamError(err: unknown): Promise<boolean> {
        const retryInfo = retryableStreamErrorInfo(err);
        const maxRetries = opts.maxRetryableStreamRetries ?? MAX_RETRYABLE_STREAM_RETRIES;
        if (
          !retryInfo ||
          !opts.retryUpstream ||
          messageStarted ||
          retryAttempt >= maxRetries ||
          opts.signal?.aborted
        ) {
          return false;
        }
        const retryDelay = opts.computeRetryDelay ?? computeBackoffDelay;
        const { waitMs, exceedsBudget } = retryDelay(retryAttempt, retryInfo.retryAfter);
        if (exceedsBudget) {
          opts.log.warn("upstream stream retry-after exceeds budget; giving up", {
            kind: retryInfo.kind,
            retryAfter: retryInfo.retryAfter,
            maxDelayMs: waitMs,
          });
          return false;
        }
        const nextAttempt = retryAttempt + 1;
        opts.log.warn("upstream stream error before downstream output, retrying", {
          kind: retryInfo.kind,
          attempt: nextAttempt,
          maxRetries,
          waitMs,
          retryAfter: retryInfo.retryAfter,
          message: retryInfo.message,
          diagnostics: describeDiagnostics(diagnostics),
          requestSize: currentRequestSize,
        });
        resetBeforeRetry();
        retryAttempt = nextAttempt;
        try {
          await sleep(waitMs, opts.signal);
          const retry = await opts.retryUpstream();
          currentUpstream = retry.body;
          currentUpstreamHeaders = retry.headers ?? currentUpstreamHeaders;
          currentRequestSize = retry.requestSize ?? currentRequestSize;
          return true;
        } catch (retryErr) {
          terminalError = retryErr;
          return false;
        }
      }
    },
  });
}

function retryableStreamErrorInfo(err: unknown):
  | {
      kind: "rate_limit" | "overloaded" | "transient";
      retryAfter?: string;
      message: string;
    }
  | undefined {
  if (!(err instanceof UpstreamStreamError)) return undefined;
  if (err.kind !== "rate_limit" && err.kind !== "overloaded" && err.kind !== "transient")
    return undefined;
  return {
    kind: err.kind,
    retryAfter: err.retryAfterSeconds !== undefined ? String(err.retryAfterSeconds) : undefined,
    message: err.message,
  };
}

function anthropicStreamErrorType(err: UpstreamStreamError): string {
  if (err.kind === "rate_limit") return "rate_limit_error";
  if (err.kind === "overloaded") return "overloaded_error";
  return "api_error";
}

function isClosedControllerError(err: unknown): boolean {
  return err instanceof TypeError && err.message.includes("Controller is already closed");
}

function describeDiagnostics(diagnostics: ReturnType<typeof createUpstreamStreamDiagnostics>) {
  const now = Date.now();
  return {
    bytesRead: diagnostics.stats.bytesRead,
    chunkCount: diagnostics.stats.chunkCount,
    eventCount: diagnostics.stats.eventCount,
    durationMs: now - diagnostics.stats.startedAt,
    msSinceLastChunk: diagnostics.stats.lastChunkAt
      ? now - diagnostics.stats.lastChunkAt
      : undefined,
    msSinceLastEvent: diagnostics.stats.lastEventAt
      ? now - diagnostics.stats.lastEventAt
      : undefined,
    lastEventType: diagnostics.lastEventType,
    sawTerminalEvent: diagnostics.sawTerminalEvent,
  };
}

function describeHeaders(headers: Headers | undefined): Record<string, string> | undefined {
  if (!headers) return undefined;
  const out: Record<string, string> = {};
  for (const [key, value] of headers) {
    out[key] = value;
  }
  return out;
}

async function writeDiagnosticFile(
  reqId: string | undefined,
  kind: string,
  detail: Record<string, unknown>,
): Promise<string | undefined> {
  try {
    const dir = join(stateDir(), "diagnostics");
    await mkdir(dir, { recursive: true });
    const safeReqId = reqId?.replace(/[^a-zA-Z0-9._-]/g, "_") || "unknown";
    const file = join(
      dir,
      `${new Date().toISOString().replace(/[:.]/g, "-")}-${safeReqId}-${kind}.json`,
    );
    await writeFile(
      file,
      JSON.stringify({ t: new Date().toISOString(), kind, reqId, ...detail }, null, 2),
    );
    return file;
  } catch {
    return undefined;
  }
}

function describeError(err: unknown) {
  if (!(err instanceof Error)) return { message: String(err) };
  const detail = err as Error & {
    code?: unknown;
    errno?: unknown;
    cause?: unknown;
  };
  return {
    name: err.name,
    message: err.message,
    code: detail.code,
    errno: detail.errno,
    cause: describeCause(detail.cause),
    stack: err.stack,
  };
}

function describeCause(cause: unknown): unknown {
  if (!(cause instanceof Error)) return cause;
  const detail = cause as Error & { code?: unknown; errno?: unknown };
  return {
    name: cause.name,
    message: cause.message,
    code: detail.code,
    errno: detail.errno,
    stack: cause.stack,
  };
}
