import { geminiModel } from "../../../config.ts"

export const GEMINI_DEFAULT_MODEL = "gemini-3.1-pro-preview"
export const GEMINI_SMALL_FAST_MODEL = "gemini-3-flash-preview"

export const ALLOWED_MODELS = new Set([
  GEMINI_DEFAULT_MODEL,
  "gemini-3-pro-preview",
  GEMINI_SMALL_FAST_MODEL,
  "gemini-2.5-pro",
  "gemini-2.5-flash",
  "gemini-2.5-flash-lite",
])

export const FALLBACK_MODELS: Record<string, string[]> = {
  "gemini-3.1-pro-preview": ["gemini-3-pro-preview", "gemini-2.5-pro"],
  "gemini-3-pro-preview": ["gemini-2.5-pro"],
  "gemini-3-flash-preview": ["gemini-2.5-flash"],
}

const GOOGLE_ONE_CREDIT_MODELS = new Set([
  GEMINI_DEFAULT_MODEL,
  "gemini-3-pro-preview",
  GEMINI_SMALL_FAST_MODEL,
])

export function resolveModel(model: string): string {
  return geminiModel() ?? model
}

export function fallbackChain(model: string): string[] {
  return [model, ...(FALLBACK_MODELS[model] ?? [])]
}

export function assertAllowedModel(model: string): void {
  if (!ALLOWED_MODELS.has(model)) {
    throw new ModelNotAllowedError(model)
  }
}

export function isGoogleOneCreditEligibleModel(model: string): boolean {
  return GOOGLE_ONE_CREDIT_MODELS.has(model)
}

export class ModelNotAllowedError extends Error {
  constructor(public model: string) {
    super(`Model not allowed: ${model}`)
    this.name = "ModelNotAllowedError"
  }
}
