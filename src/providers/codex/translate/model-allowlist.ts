export const ALLOWED_MODELS = new Set([
  "gpt-5.2",
  "gpt-5.3-codex",
  "gpt-5.4",
  "gpt-5.4-mini",
  "gpt-5.5",
])

export const MODEL_ALIASES = new Map<string, string>([
  ["haiku", "gpt-5.4-mini"],
  ["claude-haiku-4-5", "gpt-5.4-mini"],
  ["claude-haiku-4-5-20251001", "gpt-5.4-mini"],
  ["sonnet", "gpt-5.4"],
  ["claude-sonnet-4-6", "gpt-5.4"],
  ["opus", "gpt-5.5"],
  ["claude-opus-4-7", "gpt-5.5"],
])

export function resolveModel(model: string): string {
  // The CCP_CODEX_MODEL environment variable overrides the model so that
  // regardless of whatever model is requested by the harness, the provided
  // model is always used.
  if (
    process.env.CCP_CODEX_MODEL !== undefined &&
    process.env.CCP_CODEX_MODEL !== ""
  ) {
    return process.env.CCP_CODEX_MODEL
  }

  return MODEL_ALIASES.get(model) ?? model
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
