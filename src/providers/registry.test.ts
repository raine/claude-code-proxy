import { afterEach, describe, expect, it } from "bun:test"
import { mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import {
  allSupportedModels,
  ANTHROPIC_STYLE_ALIASES,
  providerForModel,
} from "./registry.ts"
import { loadConfig } from "../config.ts"
import { normalizeIncomingModel } from "../server.ts"

afterEach(() => {
  loadConfig({ forceReload: true })
})

function providersFor(model: string): string[] {
  return allSupportedModels()
    .filter((entry) => entry.model === model)
    .map((entry) => entry.provider)
}

describe("provider routing", () => {
  it("routes fast Codex model aliases after context suffix normalization", () => {
    const model = normalizeIncomingModel("gpt-5.4-fast[1m]")

    expect(model).toBe("gpt-5.4-fast")
    expect(providerForModel(model)?.name).toBe("codex")
  })

  it("routes Anthropic-style aliases to Codex by default", () => {
    loadConfig({ env: {}, forceReload: true })

    for (const alias of ANTHROPIC_STYLE_ALIASES) {
      expect(providerForModel(alias)?.name).toBe("codex")
    }
  })

  it("routes every Anthropic-style alias to Kimi when env selects Kimi", () => {
    loadConfig({ env: { CCP_ALIAS_PROVIDER: "kimi" }, forceReload: true })

    for (const alias of ANTHROPIC_STYLE_ALIASES) {
      expect(providerForModel(alias)?.name).toBe("kimi")
    }
  })

  it("lets session affinity override the global alias provider", () => {
    loadConfig({ env: {}, forceReload: true })

    expect(providerForModel("sonnet", "kimi")?.name).toBe("kimi")
  })

  it("routes aliases to Codex when config file selects Codex", () => {
    const dir = mkdtempSync(join(tmpdir(), "ccp-alias-provider-"))
    const path = join(dir, "config.json")
    writeFileSync(path, JSON.stringify({ aliasProvider: "codex" }))
    try {
      loadConfig({ configPath: path, env: {}, forceReload: true })
      expect(providerForModel("claude-opus-4-7")?.name).toBe("codex")
    } finally {
      rmSync(dir, { recursive: true, force: true })
    }
  })

  it("lets env override config file alias provider", () => {
    const dir = mkdtempSync(join(tmpdir(), "ccp-alias-provider-"))
    const path = join(dir, "config.json")
    writeFileSync(path, JSON.stringify({ aliasProvider: "codex" }))
    try {
      loadConfig({ configPath: path, env: { CCP_ALIAS_PROVIDER: "kimi" }, forceReload: true })
      expect(providerForModel("haiku")?.name).toBe("kimi")
    } finally {
      rmSync(dir, { recursive: true, force: true })
    }
  })

  it("lists aliases only under the active alias provider", () => {
    loadConfig({ env: { CCP_ALIAS_PROVIDER: "codex" }, forceReload: true })

    expect(providersFor("sonnet")).toEqual(["codex"])
  })

  it("does not duplicate supported model entries", () => {
    for (const env of [{}, { CCP_ALIAS_PROVIDER: "codex" }]) {
      loadConfig({ env, forceReload: true })
      const models = allSupportedModels().map(({ model }) => model)
      expect(new Set(models).size).toBe(models.length)
    }
  })

  it("keeps concrete model routing unchanged", () => {
    loadConfig({ env: { CCP_ALIAS_PROVIDER: "codex" }, forceReload: true })

    expect(providerForModel("kimi-for-coding")?.name).toBe("kimi")
    expect(providerForModel("gpt-5.4-fast")?.name).toBe("codex")
  })
})
