import type { CursorAgentMode, CursorModelRequest } from "../client.ts";
import type { AnthropicRequest } from "../../../anthropic/schema.ts";
import { CURSOR_AGENT_MODEL_IDS, CURSOR_AGENT_MODEL_ID_SET } from "./catalog.ts";

export interface CursorModelSelection {
  requestedModel: CursorModelRequest;
  mode: CursorAgentMode;
}

type AnthropicEffort = NonNullable<AnthropicRequest["output_config"]>["effort"];

const COMPOSER_2_5_MODEL: CursorModelRequest = {
  modelId: "composer-2.5",
  parameters: [{ id: "fast", value: "false" }],
};

const LEGACY_CURSOR_MODELS = [
  "cursor",
  "cursor-agent",
  "cursor-composer",
  "cursor-composer-fast",
  "cursor-plan",
  "cursor-ask",
  "composer-2.5",
  "composer-2.5-fast",
] as const;

export const CURSOR_SUPPORTED_MODELS = new Set<string>([
  ...LEGACY_CURSOR_MODELS,
  ...CURSOR_AGENT_MODEL_IDS.map((id) => `cursor:${id}`),
  ...CURSOR_AGENT_MODEL_IDS.map((id) => `cursor-plan:${id}`),
  ...CURSOR_AGENT_MODEL_IDS.map((id) => `cursor-ask:${id}`),
]);

const CURSOR_MODEL_PREFIXES = [
  { prefix: "cursor-plan:", mode: "AGENT_MODE_PLAN" as const },
  { prefix: "cursor-ask:", mode: "AGENT_MODE_ASK" as const },
  { prefix: "cursor-agent:", mode: "AGENT_MODE_AGENT" as const },
  { prefix: "cursor:", mode: undefined },
];

export function isCursorModel(model: string): boolean {
  return CURSOR_SUPPORTED_MODELS.has(model) || CURSOR_MODEL_PREFIXES.some((p) => model.startsWith(p.prefix));
}

export function resolveCursorModel(req: Pick<AnthropicRequest, "model" | "metadata" | "output_config">): CursorModelSelection {
  let mode = modeFromMetadata(req.metadata);
  const effort = req.output_config?.effort;
  const prefixed = splitCursorModelPrefix(req.model);
  if (prefixed) {
    return {
      requestedModel: parseRawCursorModel(applyCursorEffort(prefixed.raw, effort)),
      mode: prefixed.mode ?? mode ?? "AGENT_MODE_AGENT",
    };
  }

  if (req.model === "cursor-plan") mode = "AGENT_MODE_PLAN";
  else if (req.model === "cursor-ask") mode = "AGENT_MODE_ASK";
  else mode ??= "AGENT_MODE_AGENT";

  switch (req.model) {
    case "cursor":
    case "cursor-agent":
    case "cursor-plan":
    case "cursor-ask":
      return { requestedModel: { modelId: "default" }, mode };
    case "cursor-composer":
    case "composer-2.5":
      return { requestedModel: COMPOSER_2_5_MODEL, mode };
    case "cursor-composer-fast":
    case "composer-2.5-fast":
      return {
        requestedModel: { modelId: "composer-2.5", parameters: [{ id: "fast", value: "true" }] },
        mode,
      };
    default:
      return { requestedModel: { modelId: req.model }, mode };
  }
}

function parseRawCursorModel(raw: string): CursorModelRequest {
  if (raw === "composer-2.5") return COMPOSER_2_5_MODEL;
  if (CURSOR_AGENT_MODEL_ID_SET.has(raw)) return { modelId: raw };
  if (raw.endsWith("-fast")) {
    return {
      modelId: raw.slice(0, -"-fast".length),
      parameters: [{ id: "fast", value: "true" }],
    };
  }
  return { modelId: raw };
}

const EFFORT_SUFFIXES = [
  "extra-high",
  "medium",
  "xhigh",
  "high",
  "low",
  "max",
  "none",
] as const;

function applyCursorEffort(raw: string, effort: AnthropicEffort | undefined): string {
  if (!effort) return raw;
  const { modelId, fast } = splitFastSuffix(raw);
  if (hasEffortSuffix(modelId)) return raw;

  const targetBase = findCursorEffortVariant(modelId, effort);
  if (!targetBase) return raw;
  if (!fast) return targetBase;

  const fastTarget = `${targetBase}-fast`;
  return CURSOR_AGENT_MODEL_ID_SET.has(fastTarget) ? fastTarget : `${targetBase}-fast`;
}

function findCursorEffortVariant(modelId: string, effort: AnthropicEffort): string | undefined {
  for (const suffix of cursorEffortSuffixCandidates(effort)) {
    const candidate = `${modelId}-${suffix}`;
    if (CURSOR_AGENT_MODEL_ID_SET.has(candidate)) return candidate;
  }
  return undefined;
}

function cursorEffortSuffixCandidates(effort: AnthropicEffort): readonly string[] {
  switch (effort) {
    case "low":
      return ["low"];
    case "medium":
      return ["medium"];
    case "high":
      return ["high", "extra-high", "xhigh"];
    case "max":
      return ["max", "xhigh", "extra-high", "high"];
    default:
      return [];
  }
}

function hasEffortSuffix(modelId: string): boolean {
  return EFFORT_SUFFIXES.some((suffix) => modelId.endsWith(`-${suffix}`));
}

function splitFastSuffix(raw: string): { modelId: string; fast: boolean } {
  return raw.endsWith("-fast")
    ? { modelId: raw.slice(0, -"-fast".length), fast: true }
    : { modelId: raw, fast: false };
}

function splitCursorModelPrefix(model: string): { raw: string; mode?: CursorAgentMode } | undefined {
  for (const p of CURSOR_MODEL_PREFIXES) {
    if (model.startsWith(p.prefix)) return { raw: model.slice(p.prefix.length), mode: p.mode };
  }
  return undefined;
}

function modeFromMetadata(metadata: unknown): CursorAgentMode | undefined {
  if (!metadata || typeof metadata !== "object" || Array.isArray(metadata)) return undefined;
  const value = (metadata as Record<string, unknown>).cursor_mode ?? (metadata as Record<string, unknown>).cursorMode;
  if (value === "plan" || value === "AGENT_MODE_PLAN") return "AGENT_MODE_PLAN";
  if (value === "ask" || value === "AGENT_MODE_ASK") return "AGENT_MODE_ASK";
  if (value === "agent" || value === "AGENT_MODE_AGENT") return "AGENT_MODE_AGENT";
  return undefined;
}
