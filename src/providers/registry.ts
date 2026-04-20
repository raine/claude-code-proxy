import type { Provider } from "./types.ts"
import { codexProvider } from "./codex/index.ts"
import { kimiProvider } from "./kimi/index.ts"

const PROVIDERS: Record<string, Provider> = {
  codex: codexProvider,
  kimi: kimiProvider,
}

export function getProvider(name?: string): Provider {
  const key = name ?? process.env.CCP_PROVIDER ?? "codex"
  const p = PROVIDERS[key]
  if (!p) {
    throw new Error(
      `Unknown provider: ${key}. Available: ${Object.keys(PROVIDERS).join(", ")}`,
    )
  }
  return p
}

export function listProviders(): string[] {
  return Object.keys(PROVIDERS)
}
