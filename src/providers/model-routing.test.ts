import { afterEach, beforeEach, describe, expect, it } from "bun:test"
import { loadConfig } from "../config.ts"
import { resolveModelRoute } from "./model-routing.ts"

function setAliasProvider(provider?: string) {
  loadConfig({
    env: provider ? { CCP_CLAUDE_ALIAS_PROVIDER: provider } : {},
    configPath: "/tmp/claude-code-proxy-model-routing-test-does-not-exist.json",
    forceReload: true,
  })
}

beforeEach(() => {
  setAliasProvider(undefined)
})

afterEach(() => {
  loadConfig({ forceReload: true })
})

describe("resolveModelRoute", () => {
  it("does not route Claude aliases when alias routing is disabled", () => {
    expect(resolveModelRoute("claude-opus-4-7")).toBeUndefined()
  })

  it("routes Claude aliases to Codex models", () => {
    setAliasProvider("codex")
    expect(resolveModelRoute("opus")).toMatchObject({
      provider: "codex",
      model: "gpt-5.5",
    })
    expect(resolveModelRoute("claude-sonnet-4-6")).toMatchObject({
      provider: "codex",
      model: "gpt-5.4",
    })
    expect(resolveModelRoute("claude-haiku-4-5-20251001")).toMatchObject({
      provider: "codex",
      model: "gpt-5.4-mini",
    })
  })

  it("routes Claude aliases to Gemini models", () => {
    setAliasProvider("gemini")
    expect(resolveModelRoute("claude-opus-4-7")).toMatchObject({
      provider: "gemini",
      model: "gemini-3.1-pro-preview",
    })
    expect(resolveModelRoute("claude-sonnet-4-6")).toMatchObject({
      provider: "gemini",
      model: "gemini-3-pro-preview",
    })
    expect(resolveModelRoute("haiku")).toMatchObject({
      provider: "gemini",
      model: "gemini-3-flash-preview",
    })
  })

  it("routes Claude aliases to Kimi without changing the requested model", () => {
    setAliasProvider("kimi")
    expect(resolveModelRoute("claude-opus-4-7")).toMatchObject({
      provider: "kimi",
      model: "claude-opus-4-7",
    })
  })

  it("leaves direct provider model ids alone", () => {
    setAliasProvider("gemini")
    expect(resolveModelRoute("gemini-3.1-pro-preview")).toBeUndefined()
    expect(resolveModelRoute("gpt-5.5")).toBeUndefined()
  })
})
