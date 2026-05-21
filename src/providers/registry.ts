import { aliasProvider, type AliasProvider } from "../config.ts"
import type { Provider } from "./types.ts"
import { codexProvider } from "./codex/index.ts"
import { kimiProvider } from "./kimi/index.ts"

export const ANTHROPIC_STYLE_ALIASES = new Set([
  "haiku",
  "claude-haiku-4-5",
  "claude-haiku-4-5-20251001",
  "sonnet",
  "claude-sonnet-4-6",
  "opus",
  "claude-opus-4-7",
])

const PROVIDERS: Record<string, Provider> = {
  codex: codexProvider,
  kimi: kimiProvider,
}

export function getProvider(name: string): Provider {
  const p = PROVIDERS[name]
  if (!p) {
    throw new Error(
      `Unknown provider: ${name}. Available: ${Object.keys(PROVIDERS).join(", ")}`,
    )
  }
  return p
}

export function listProviders(): string[] {
  return Object.keys(PROVIDERS)
}

export function allProviders(): Provider[] {
  return Object.values(PROVIDERS)
}

export function providerForModel(
  model: string,
  aliasProviderOverride?: AliasProvider,
): Provider | undefined {
  if (ANTHROPIC_STYLE_ALIASES.has(model)) return getProvider(aliasProviderOverride ?? aliasProvider())
  for (const p of allProviders()) {
    if (p.supportedModels.has(model)) return p
  }
  return undefined
}

export function allSupportedModels(): Array<{ model: string; provider: string }> {
  const out: Array<{ model: string; provider: string }> = []
  const activeAliasProvider = aliasProvider()
  for (const p of allProviders()) {
    for (const m of p.supportedModels) out.push({ model: m, provider: p.name })
    if (p.name === activeAliasProvider) {
      for (const m of ANTHROPIC_STYLE_ALIASES) out.push({ model: m, provider: p.name })
    }
  }
  return out
}
