import type { Provider } from "./types.ts"
import { codexProvider } from "./codex/index.ts"
import { geminiProvider } from "./gemini/index.ts"
import { kimiProvider } from "./kimi/index.ts"

const PROVIDERS: Record<string, Provider> = {
  codex: codexProvider,
  gemini: geminiProvider,
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

// Look up the single provider that claims the given model id. Returns
// undefined when no provider declares it (the caller should surface an
// error). Cross-provider aliases like `haiku` / `sonnet` are deliberately
// NOT resolvable here — users must use a concrete, provider-owned model id.
export function providerForModel(model: string): Provider | undefined {
  for (const p of allProviders()) {
    if (p.supportedModels.has(model)) return p
  }
  return undefined
}

export function allSupportedModels(): Array<{ model: string; provider: string }> {
  const out: Array<{ model: string; provider: string }> = []
  for (const p of allProviders()) {
    for (const m of p.supportedModels) out.push({ model: m, provider: p.name })
  }
  return out
}
