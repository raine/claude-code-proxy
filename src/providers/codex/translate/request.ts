import type {
  AnthropicContentBlock,
  AnthropicMessage,
  AnthropicRequest,
  AnthropicTool,
} from "../../../anthropic/schema.ts";
import { codexEffort, codexReasoningSummary, codexServiceTier } from "../../../config.ts";
import {
  assertValidEffort,
  mapToolChoice as mapAnthropicToolChoice,
  flattenSystemText,
  imageBlockToUrl,
  normalizeContent,
  toolResultToString,
} from "../../translate/anthropic-content.ts";

export type Effort = "none" | "low" | "medium" | "high" | "xhigh";
export type ServiceTier = "priority" | "flex";

export class InvalidServiceTierError extends Error {
  constructor(public serviceTier: string) {
    super(
      `Invalid service tier override: "${serviceTier}". Must be one of: ${Array.from(VALID_SERVICE_TIERS).join(", ")}`,
    );
    this.name = "InvalidServiceTierError";
  }
}

// Keep this aligned to the upstream Codex ResponsesApiRequest field set.
// Do not add plausible-looking top-level fields without source support or a confirmed live test.
export interface ResponsesRequest {
  model: string;
  instructions?: string;
  input: ResponsesInputItem[];
  tools?: ResponsesTool[];
  tool_choice?:
    | "auto"
    | "none"
    | "required"
    | { type: "function"; name: string }
    | { type: "web_search" };
  parallel_tool_calls?: boolean;
  reasoning?: { effort?: Effort; summary?: unknown };
  store: false;
  stream: true;
  include?: string[];
  service_tier?: ServiceTier;
  prompt_cache_key?: string;
  text?: {
    verbosity?: "low" | "medium" | "high";
    format?:
      | { type: "text" }
      | { type: "json_object" }
      | { type: "json_schema"; name: string; schema: unknown; strict?: boolean };
  };
  client_metadata?: Record<string, string>;
}

export interface ResponsesWebSocketRequest extends ResponsesRequest {
  previous_response_id?: string;
  generate?: boolean;
}

export function toWebSocketRequest(
  body: ResponsesRequest,
  opts: { previousResponseId?: string; input?: ResponsesInputItem[]; generate?: boolean } = {},
): ResponsesWebSocketRequest {
  return {
    ...body,
    ...(opts.input ? { input: opts.input } : {}),
    ...(opts.previousResponseId ? { previous_response_id: opts.previousResponseId } : {}),
    ...(opts.generate !== undefined ? { generate: opts.generate } : {}),
  };
}

export type ResponsesInputItem =
  | {
      type: "message";
      role: "user" | "assistant" | "developer" | "system";
      content: ResponsesContentPart[];
    }
  | {
      type: "function_call";
      call_id: string;
      name: string;
      arguments: string;
    }
  | {
      type: "function_call_output";
      call_id: string;
      output: string;
    };

export type ResponsesContentPart =
  | { type: "input_text"; text: string }
  | { type: "output_text"; text: string }
  | { type: "input_image"; image_url: string; detail?: "auto" | "low" | "high" };

export type ResponsesTool = ResponsesFunctionTool | ResponsesWebSearchTool;

export interface ResponsesFunctionTool {
  type: "function";
  name: string;
  description?: string;
  parameters: unknown;
  strict?: boolean;
}

export interface ResponsesWebSearchTool {
  type: "web_search";
  external_web_access: boolean;
  search_content_types: Array<"text" | "image">;
  filters?: {
    allowed_domains?: string[];
    blocked_domains?: string[];
  };
}

export interface TranslateOptions {
  sessionId?: string;
  serviceTier?: ServiceTier;
}

const VALID_EFFORTS = new Set<Effort>(["none", "low", "medium", "high", "xhigh"]);

const VALID_SERVICE_TIERS = new Set(["fast", "priority", "flex"]);

function toCodexEffort(
  effort: NonNullable<AnthropicRequest["output_config"]>["effort"],
): Effort | undefined {
  if (effort === "max") return "xhigh";
  return effort;
}

function resolveEffort(effort?: Effort): Effort | undefined {
  const override = codexEffort();
  if (override === undefined) {
    return effort;
  }
  if (!VALID_EFFORTS.has(override as Effort)) {
    throw new Error(
      `Invalid effort override: "${override}". Must be one of: ${Array.from(VALID_EFFORTS).join(", ")}`,
    );
  }
  return override as Effort;
}

function normalizeServiceTier(tier: string): ServiceTier {
  if (!VALID_SERVICE_TIERS.has(tier)) {
    throw new InvalidServiceTierError(tier);
  }
  return tier === "flex" ? "flex" : "priority";
}

function resolveServiceTier(modelServiceTier?: ServiceTier): ServiceTier | undefined {
  const tier = codexServiceTier();
  if (tier === undefined) return modelServiceTier;
  return normalizeServiceTier(tier);
}

export function normalizeStrictJsonSchema(schema: unknown): unknown {
  if (Array.isArray(schema)) return schema.map(normalizeStrictJsonSchema);
  if (!isRecord(schema)) return schema;

  const out: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(schema)) {
    out[key] = normalizeStrictJsonSchema(value);
  }

  if (isRecord(out.properties)) {
    out.required = Object.keys(out.properties);
  }

  return out;
}

export function translateRequest(
  req: AnthropicRequest,
  opts: TranslateOptions = {},
): ResponsesRequest {
  const instructions = buildInstructions(req.system);
  const input = buildInput(req.messages);
  const tools = req.tools?.map(toResponsesTool);

  const text: ResponsesRequest["text"] = { verbosity: "low" };
  const fmt = req.output_config?.format;
  if (fmt?.type === "json_schema") {
    text.format = {
      type: "json_schema",
      name: fmt.name ?? "response",
      schema: normalizeStrictJsonSchema(fmt.schema),
      strict: true,
    };
  }

  const out: ResponsesRequest = {
    model: req.model,
    input,
    store: false,
    stream: true,
    parallel_tool_calls: true,
    tool_choice: mapToolChoice(req.tool_choice, req.tools),
    text,
  };
  if (instructions) out.instructions = instructions;
  if (tools && tools.length) out.tools = tools;
  if (opts.sessionId) out.prompt_cache_key = opts.sessionId;
  const serviceTier = resolveServiceTier(opts.serviceTier);
  if (serviceTier) out.service_tier = serviceTier;
  assertValidEffort(req.output_config?.effort);
  const effort = resolveEffort(toCodexEffort(req.output_config?.effort));
  if (effort) {
    const summary = codexReasoningSummary();
    out.reasoning =
      summary === "off" || summary === "none" ? { effort } : { effort, summary: "auto" };
    out.include = ["reasoning.encrypted_content"];
  }
  return out;
}

function mapToolChoice(
  choice: AnthropicRequest["tool_choice"],
  tools: AnthropicRequest["tools"],
): ResponsesRequest["tool_choice"] {
  const mapped = mapAnthropicToolChoice(choice);
  if (mapped === "auto" || mapped === "none" || mapped === "required") return mapped;
  if (isForcedHostedWebSearchChoice(choice, tools)) return { type: "web_search" };
  return { type: "function", name: mapped.name };
}

export const buildInstructions = flattenSystemText;

export { normalizeContent, toolResultToString };

function buildInput(messages: AnthropicMessage[]): ResponsesInputItem[] {
  const out: ResponsesInputItem[] = [];
  for (const msg of messages) {
    const blocks = normalizeContent(msg.content);
    if (msg.role === "user") {
      // Split into message parts vs function_call_output items
      const parts: ResponsesContentPart[] = [];
      for (const block of blocks) {
        if (block.type === "text") {
          parts.push({ type: "input_text", text: block.text });
        } else if (block.type === "image") {
          parts.push({ type: "input_image", image_url: imageBlockToUrl(block) });
        } else if (block.type === "tool_result") {
          if (parts.length) {
            out.push({ type: "message", role: "user", content: parts.splice(0) });
          }
          const body = toolResultToString(block.content);
          out.push({
            type: "function_call_output",
            call_id: block.tool_use_id,
            output: block.is_error ? `[tool execution error]\n${body}` : body,
          });
        }
      }
      if (parts.length) out.push({ type: "message", role: "user", content: parts });
    } else if (msg.role === "system") {
      const parts = blocks
        .filter((block): block is AnthropicContentBlock & { type: "text" } => block.type === "text")
        .map((block) => ({ type: "input_text" as const, text: block.text }));
      if (parts.length) out.push({ type: "message", role: "developer", content: parts });
    } else {
      // assistant: preserve interleaved order of text vs tool_use
      const textParts: ResponsesContentPart[] = [];
      const flushText = () => {
        if (textParts.length) {
          out.push({ type: "message", role: "assistant", content: textParts.splice(0) });
        }
      };
      for (const block of blocks) {
        if (block.type === "text") {
          textParts.push({ type: "output_text", text: block.text });
        } else if (block.type === "tool_use") {
          flushText();
          out.push({
            type: "function_call",
            call_id: block.id,
            name: block.name,
            arguments: JSON.stringify(block.input ?? {}),
          });
        }
      }
      flushText();
    }
  }
  return out;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object";
}

function isForcedHostedWebSearchChoice(
  choice: AnthropicRequest["tool_choice"],
  tools: AnthropicRequest["tools"],
): boolean {
  if (choice?.type !== "tool" || !choice.name) return false;
  return (
    tools?.some((tool) => tool.type === "web_search_20250305" && tool.name === choice.name) ?? false
  );
}

function toResponsesTool(tool: AnthropicTool): ResponsesTool {
  if (tool.type === "web_search_20250305") {
    const filters: ResponsesWebSearchTool["filters"] = {};
    if (tool.allowed_domains?.length) filters.allowed_domains = tool.allowed_domains;
    if (tool.blocked_domains?.length) filters.blocked_domains = tool.blocked_domains;
    return {
      type: "web_search",
      external_web_access: false,
      search_content_types: ["text", "image"],
      ...(Object.keys(filters).length ? { filters } : {}),
    };
  }
  return {
    type: "function",
    name: tool.name,
    description: tool.description,
    parameters: tool.input_schema,
  };
}
