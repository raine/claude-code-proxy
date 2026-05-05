import { claudeAliasProvider, type ClaudeAliasProvider } from "../config.ts"

export interface RoutedModel {
  provider: "codex" | "gemini" | "kimi"
  model: string
  aliasProvider: ClaudeAliasProvider
}

type ClaudeAlias = "haiku" | "sonnet" | "opus"

const CODEX_ALIAS_MODELS: Record<ClaudeAlias, string> = {
  haiku: "gpt-5.4-mini",
  sonnet: "gpt-5.4",
  opus: "gpt-5.5",
}

const GEMINI_ALIAS_MODELS: Record<ClaudeAlias, string> = {
  haiku: "gemini-3-flash-preview",
  sonnet: "gemini-3-pro-preview",
  opus: "gemini-3.1-pro-preview",
}

export function resolveModelRoute(model: string): RoutedModel | undefined {
  const alias = claudeAlias(model)
  if (!alias) return undefined

  const aliasProvider = claudeAliasProvider()
  if (aliasProvider === "none") return undefined

  if (aliasProvider === "codex") {
    return { provider: "codex", model: CODEX_ALIAS_MODELS[alias], aliasProvider }
  }
  if (aliasProvider === "gemini") {
    return { provider: "gemini", model: GEMINI_ALIAS_MODELS[alias], aliasProvider }
  }
  return { provider: "kimi", model, aliasProvider }
}

function claudeAlias(model: string): ClaudeAlias | undefined {
  const normalized = model.toLowerCase()
  if (normalized === "haiku" || normalized.startsWith("claude-haiku-4-5")) {
    return "haiku"
  }
  if (normalized === "sonnet" || normalized.startsWith("claude-sonnet-4-6")) {
    return "sonnet"
  }
  if (normalized === "opus" || normalized.startsWith("claude-opus-4-7")) {
    return "opus"
  }
  return undefined
}
