export const ALLOWED_MODELS = new Set([
  "gpt-5.2",
  "gpt-5.3-codex",
  "gpt-5.4",
  "gpt-5.4-mini",
])

export const MODEL_ALIASES = new Map<string, string>([
  ["haiku", "gpt-5.4-mini"],
  ["claude-haiku-4-5", "gpt-5.4-mini"],
  ["claude-haiku-4-5-20251001", "gpt-5.4-mini"],
  ["sonnet", "gpt-5.4"],
  ["claude-sonnet-4-6", "gpt-5.4"],
  ["opus", "gpt-5.4"],
  ["claude-opus-4-7", "gpt-5.4"],
])

export function resolveModel(model: string): string {
  // The CLAUDE_CODEX_PROXY_OPEN_AI_MODEL_OVERRIDE environment variable allows
  // for the model that's used to be overridden so that regardless of whatever
  // model is being requested by the harness, the model which is provided in
  // that env var is always returned.
  //
  // This is useful in cases where you just want the claude-code harness to use
  // a specific model all across the way.
  if (
    process.env.CLAUDE_CODEX_PROXY_OPEN_AI_MODEL_OVERRIDE !== undefined &&
    process.env.CLAUDE_CODEX_PROXY_OPEN_AI_MODEL_OVERRIDE !== ""
  ) {
    return process.env.CLAUDE_CODEX_PROXY_OPEN_AI_MODEL_OVERRIDE
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
