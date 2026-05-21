import { codexModel } from "../../../config.ts"
import type { ServiceTier } from "./request.ts"

export const ALLOWED_MODELS = new Set([
  "gpt-5.2",
  "gpt-5.3-codex",
  "gpt-5.3-codex-spark",
  "gpt-5.4",
  "gpt-5.4-mini",
  "gpt-5.5",
])

export const FAST_MODEL_ALIASES = new Set(
  Array.from(ALLOWED_MODELS, (model) => `${model}-fast`),
)

export const MODEL_ALIASES = new Map<string, string>([
  ["haiku", "gpt-5.4-mini"],
  ["claude-haiku-4-5", "gpt-5.4-mini"],
  ["claude-haiku-4-5-20251001", "gpt-5.4-mini"],
  ["sonnet", "gpt-5.4"],
  ["claude-sonnet-4-6", "gpt-5.4"],
  ["opus", "gpt-5.5"],
  ["claude-opus-4-7", "gpt-5.5"],
])

export interface ResolvedModel {
  model: string
  serviceTier?: ServiceTier
}

export function resolveModel(model: string): string {
  return resolveModelRequest(model).model
}

export function resolveModelRequest(model: string): ResolvedModel {
  // CCP_CODEX_MODEL (env) or codex.model (config.json) overrides the model
  // so that regardless of whatever model is requested by the harness, the
  // provided model is always used. Empty values fall through to alias
  // resolution.
  const alias = MODEL_ALIASES.get(model) ?? model
  const isFastAlias = FAST_MODEL_ALIASES.has(alias)
  const modelWithoutTier = isFastAlias ? alias.slice(0, -"-fast".length) : alias
  const override = codexModel()

  return {
    model: override ?? modelWithoutTier,
    ...(isFastAlias ? { serviceTier: "priority" } : {}),
  }
}

export function assertAllowedModel(model: string): void {
  if (!ALLOWED_MODELS.has(model)) {
    throw new ModelNotAllowedError(model)
  }
}

export class ModelNotAllowedError extends Error {
  constructor(public model: string) {
    super(`Model not allowed: ${model}`)
    this.name = "ModelNotAllowedError"
  }
}
