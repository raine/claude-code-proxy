import { parseSseStream } from "../../../sse.ts"
import type { Logger } from "../../../log.ts"

const VERBOSE = !!process.env.CCP_LOG_VERBOSE

export class UpstreamStreamError extends Error {
  constructor(
    public kind: "rate_limit" | "failed",
    message: string,
    public retryAfterSeconds?: number,
  ) {
    super(message)
    this.name = "UpstreamStreamError"
  }
}

export interface CodexUsage {
  input_tokens?: number
  output_tokens?: number
  input_tokens_details?: { cached_tokens?: number }
  output_tokens_details?: { reasoning_tokens?: number }
}

export type StopReason = "end_turn" | "tool_use" | "max_tokens"

export type ReducerEvent =
  | { kind: "text-start"; index: number }
  | { kind: "text-delta"; index: number; text: string }
  | { kind: "text-stop"; index: number }
  | { kind: "tool-start"; index: number; id: string; name: string }
  | { kind: "tool-delta"; index: number; partialJson: string }
  | { kind: "tool-stop"; index: number }
  | { kind: "rate-limits"; rateLimits: unknown }
  | { kind: "finish"; stopReason: StopReason; usage: CodexUsage | undefined }

interface TextState {
  kind: "text"
  index: number
  textAccum: string
}
interface ToolState {
  kind: "tool"
  index: number
  callId: string
  name: string
  argsAccum: string
  hadDelta: boolean
  bufferUntilDone: boolean
  emittedArgs: boolean
}
type BlockState = TextState | ToolState

function shouldBufferToolArgs(name: string): boolean {
  return name === "Read"
}

function sanitizeToolArgs(name: string, args: string): string {
  if (name !== "Read" || !args) return args
  try {
    const parsed = JSON.parse(args)
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return args
    if (!("pages" in parsed) || parsed.pages !== "") return args
    const sanitized = { ...parsed }
    delete sanitized.pages
    return JSON.stringify(sanitized)
  } catch {
    return args
  }
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
export async function* reduceUpstream(
  upstream: ReadableStream<Uint8Array>,
  log: Logger,
): AsyncGenerator<ReducerEvent> {
  const blocksByOutputIndex = new Map<number, BlockState>()
  const itemIdToOutputIndex = new Map<string, number>()
  let anthropicIndex = 0
  let sawToolUse = false
  let finalUsage: CodexUsage | undefined
  let incomplete = false

  for await (const evt of parseSseStream(upstream)) {
    if (!evt.data) continue
    let p: any
    try {
      p = JSON.parse(evt.data)
    } catch {
      continue
    }
    const t: string = p.type || evt.event || ""

    if (VERBOSE) log.debug("upstream event", { type: t, output_index: p.output_index, item_id: p.item_id })

    if (t === "codex.rate_limits") {
      if (p.rate_limits?.limit_reached) {
        throw new UpstreamStreamError(
          "rate_limit",
          "rate limit reached",
          p.rate_limits?.primary?.reset_after_seconds,
        )
      }
      yield { kind: "rate-limits", rateLimits: p.rate_limits }
      continue
    }
    if (t === "response.failed" || t === "response.error" || t === "error") {
      const message = p?.response?.error?.message || p?.error?.message || "Upstream error"
      throw new UpstreamStreamError("failed", message)
    }

    if (t === "response.output_item.added") {
      const item = p.item
      const outputIndex: number = p.output_index
      if (!item) continue
      if (item.type === "reasoning") continue
      if (item.type === "message") {
        const idx = anthropicIndex++
        blocksByOutputIndex.set(outputIndex, { kind: "text", index: idx, textAccum: "" })
        if (item.id) itemIdToOutputIndex.set(item.id, outputIndex)
        yield { kind: "text-start", index: idx }
        continue
      }
      if (item.type === "function_call") {
        sawToolUse = true
        const idx = anthropicIndex++
        blocksByOutputIndex.set(outputIndex, {
          kind: "tool",
          index: idx,
          callId: item.call_id,
          name: item.name,
          argsAccum: "",
          hadDelta: false,
          bufferUntilDone: shouldBufferToolArgs(item.name),
          emittedArgs: false,
        })
        yield { kind: "tool-start", index: idx, id: item.call_id, name: item.name }
        continue
      }

      continue
    }

    if (t === "response.output_text.delta") {
      const outputIndex: number | undefined = p.output_index
      const itemId: string | undefined = p.item_id
      let state: BlockState | undefined
      if (typeof outputIndex === "number") state = blocksByOutputIndex.get(outputIndex)
      if (!state && itemId) {
        const mapped = itemIdToOutputIndex.get(itemId)
        if (mapped !== undefined) state = blocksByOutputIndex.get(mapped)
      }
      if (!state || state.kind !== "text") continue
      const delta: string = p.delta ?? ""
      if (!delta) continue
      state.textAccum += delta
      yield { kind: "text-delta", index: state.index, text: delta }
      continue
    }

    if (t === "response.function_call_arguments.delta") {
      const state = blocksByOutputIndex.get(p.output_index)
      if (!state || state.kind !== "tool") continue
      const delta: string = p.delta ?? ""
      if (!delta) continue
      state.argsAccum += delta
      state.hadDelta = true
      if (!state.bufferUntilDone) {
        state.emittedArgs = true
        yield { kind: "tool-delta", index: state.index, partialJson: delta }
      }
      continue
    }

    if (t === "response.function_call_arguments.done") {
      const state = blocksByOutputIndex.get(p.output_index)
      if (!state || state.kind !== "tool") continue
      if (typeof p.arguments === "string" && !state.argsAccum) {
        state.argsAccum = p.arguments
      }
      continue
    }

    if (t === "response.output_item.done") {
      const item = p.item
      const state = blocksByOutputIndex.get(p.output_index)
      if (!state) continue
      if (!item) {
        // defensive
        if (state.kind === "text") yield { kind: "text-stop", index: state.index }
        else yield { kind: "tool-stop", index: state.index }
        blocksByOutputIndex.delete(p.output_index)
        continue
      }
      if (item.type === "reasoning") continue
      if (state.kind === "tool") {
        const finalArgs =
          (typeof item.arguments === "string" && item.arguments.length
            ? item.arguments
            : state.argsAccum) || ""
        if (finalArgs.length) {
          state.argsAccum = sanitizeToolArgs(state.name, finalArgs)
          if (state.bufferUntilDone || !state.emittedArgs) {
            state.emittedArgs = true
            yield { kind: "tool-delta", index: state.index, partialJson: state.argsAccum }
          }
        }
      }
      if (state.kind === "text") {
        log.debug("text block complete", { index: state.index, text: state.textAccum })
        yield { kind: "text-stop", index: state.index }
      } else {
        log.debug("tool block complete", {
          index: state.index,
          callId: state.callId,
          name: state.name,
          args: state.argsAccum,
        })
        yield { kind: "tool-stop", index: state.index }
      }
      blocksByOutputIndex.delete(p.output_index)
      continue
    }

    if (t === "response.completed" || t === "response.incomplete") {
      finalUsage = p.response?.usage
      const reason = p.response?.incomplete_details?.reason
      if (
        t === "response.incomplete" ||
        reason === "max_output_tokens" ||
        p.response?.status === "incomplete"
      ) {
        incomplete = true
      }
      continue
    }
  }

  const stopReason: StopReason = incomplete ? "max_tokens" : sawToolUse ? "tool_use" : "end_turn"
  yield { kind: "finish", stopReason, usage: finalUsage }
}

export function mapUsageToAnthropic(u: CodexUsage | undefined): {
  input_tokens: number
  output_tokens: number
  cache_creation_input_tokens: number
  cache_read_input_tokens: number
} {
  const cachedTokens = u?.input_tokens_details?.cached_tokens ?? 0
  const totalInputTokens = u?.input_tokens ?? 0
  return {
    // OpenAI-style usage reports cached prompt tokens inside input_tokens.
    // Anthropic-style usage reports cache reads separately, and Claude Code
    // sums input_tokens + cache_read_input_tokens when deciding context size.
    // Subtract cached reads here so the downstream total matches the real
    // prompt window instead of double-counting cached context.
    input_tokens: Math.max(0, totalInputTokens - cachedTokens),
    output_tokens: u?.output_tokens ?? 0,
    cache_creation_input_tokens: 0,
    cache_read_input_tokens: cachedTokens,
  }
}
