import { describe, expect, it, beforeEach, afterEach } from "bun:test"
import { mkdtempSync, writeFileSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import {
  loadConfig,
  port,
  codexOriginator,
  codexUserAgent,
  codexModel,
  codexEffort,
  codexDefaultEffort,
  claudeAliasProvider,
  geminiModel,
  geminiSmallFastModel,
  geminiOauthCredsPath,
  geminiEnableFallback,
  geminiEnableGoogleOneCredits,
  geminiDefaultEffort,
  kimiUserAgent,
  kimiOauthHost,
  kimiBaseUrl,
  logVerbose,
  logStderr,
} from "./config.ts"

let dir: string
let configPath: string

function setEnv(env: NodeJS.ProcessEnv) {
  loadConfig({ configPath, env, forceReload: true })
}

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "ccp-config-"))
  configPath = join(dir, "config.json")
})

afterEach(() => {
  rmSync(dir, { recursive: true, force: true })
  // Reset module-level cache to a clean process-env baseline so unrelated
  // tests that import config getters do not see leftover overrides.
  loadConfig({ forceReload: true })
})

describe("config defaults", () => {
  it("returns built-in defaults when no env and no file", () => {
    setEnv({})
    expect(port()).toBe(18765)
    expect(codexOriginator("default-orig")).toBe("default-orig")
    expect(codexUserAgent("default-ua")).toBe("default-ua")
    expect(codexModel()).toBeUndefined()
    expect(codexEffort()).toBeUndefined()
    expect(codexDefaultEffort()).toBeUndefined()
    expect(claudeAliasProvider()).toBe("none")
    expect(geminiModel()).toBeUndefined()
    expect(geminiSmallFastModel()).toBeUndefined()
    expect(geminiOauthCredsPath()).toBeUndefined()
    expect(geminiEnableFallback()).toBe(true)
    expect(geminiEnableGoogleOneCredits()).toBe(false)
    expect(geminiDefaultEffort()).toBeUndefined()
    expect(kimiUserAgent("default-kimi-ua")).toBe("default-kimi-ua")
    expect(kimiOauthHost()).toBe("https://auth.kimi.com")
    expect(kimiBaseUrl()).toBe("https://api.kimi.com/coding/v1")
    expect(logVerbose()).toBe(false)
    expect(logStderr()).toBe(false)
  })
})

describe("file overrides default", () => {
  it("port from config.json", () => {
    writeFileSync(configPath, JSON.stringify({ port: 11111 }))
    setEnv({})
    expect(port()).toBe(11111)
  })

  it("codex.userAgent from config.json", () => {
    writeFileSync(
      configPath,
      JSON.stringify({ codex: { userAgent: "ccp/file" } }),
    )
    setEnv({})
    expect(codexUserAgent("default")).toBe("ccp/file")
  })

  it("kimi.oauthHost from config.json", () => {
    writeFileSync(
      configPath,
      JSON.stringify({ kimi: { oauthHost: "https://auth.example.com" } }),
    )
    setEnv({})
    expect(kimiOauthHost()).toBe("https://auth.example.com")
  })

  it("routing and gemini settings from config.json", () => {
    writeFileSync(
      configPath,
      JSON.stringify({
        routing: { claudeAliasProvider: "gemini" },
        gemini: {
          model: "gemini-3.1-pro-preview",
          smallFastModel: "gemini-3-flash-preview",
          oauthCredsPath: "/tmp/gemini-oauth.json",
          enableFallback: false,
          enableGoogleOneCredits: true,
          defaultEffort: "max",
        },
      }),
    )
    setEnv({})
    expect(claudeAliasProvider()).toBe("gemini")
    expect(geminiModel()).toBe("gemini-3.1-pro-preview")
    expect(geminiSmallFastModel()).toBe("gemini-3-flash-preview")
    expect(geminiOauthCredsPath()).toBe("/tmp/gemini-oauth.json")
    expect(geminiEnableFallback()).toBe(false)
    expect(geminiEnableGoogleOneCredits()).toBe(true)
    expect(geminiDefaultEffort()).toBe("max")
  })

  it("log.verbose from config.json", () => {
    writeFileSync(configPath, JSON.stringify({ log: { verbose: true } }))
    setEnv({})
    expect(logVerbose()).toBe(true)
  })
})

describe("env overrides file", () => {
  it("PORT env wins over config port", () => {
    writeFileSync(configPath, JSON.stringify({ port: 11111 }))
    setEnv({ PORT: "22222" })
    expect(port()).toBe(22222)
  })

  it("CCP_CODEX_USER_AGENT env wins over config", () => {
    writeFileSync(
      configPath,
      JSON.stringify({ codex: { userAgent: "from-file" } }),
    )
    setEnv({ CCP_CODEX_USER_AGENT: "from-env" })
    expect(codexUserAgent("default")).toBe("from-env")
  })

  it("CCP_CODEX_DEFAULT_EFFORT env wins over config", () => {
    writeFileSync(
      configPath,
      JSON.stringify({ codex: { defaultEffort: "medium" } }),
    )
    setEnv({ CCP_CODEX_DEFAULT_EFFORT: "high" })
    expect(codexDefaultEffort()).toBe("high")
  })

  it("CCP_CLAUDE_ALIAS_PROVIDER env wins over config", () => {
    writeFileSync(
      configPath,
      JSON.stringify({ routing: { claudeAliasProvider: "gemini" } }),
    )
    setEnv({ CCP_CLAUDE_ALIAS_PROVIDER: "codex" })
    expect(claudeAliasProvider()).toBe("codex")
  })

  it("CCP_GEMINI_ENABLE_FALLBACK env can disable fallback", () => {
    writeFileSync(configPath, JSON.stringify({ gemini: { enableFallback: true } }))
    setEnv({ CCP_GEMINI_ENABLE_FALLBACK: "false" })
    expect(geminiEnableFallback()).toBe(false)
  })

  it("CCP_GEMINI_ENABLE_GOOGLE_ONE_CREDITS env can enable credit usage", () => {
    writeFileSync(configPath, JSON.stringify({ gemini: { enableGoogleOneCredits: false } }))
    setEnv({ CCP_GEMINI_ENABLE_GOOGLE_ONE_CREDITS: "1" })
    expect(geminiEnableGoogleOneCredits()).toBe(true)
  })

  it("CCP_USER_AGENT env (generic fallback) is preferred over file", () => {
    writeFileSync(
      configPath,
      JSON.stringify({ codex: { userAgent: "from-file" } }),
    )
    setEnv({ CCP_USER_AGENT: "generic-env" })
    expect(codexUserAgent("default")).toBe("generic-env")
    expect(kimiUserAgent("default")).toBe("generic-env")
  })

  it("logStderr env-set forces true even when config sets false", () => {
    writeFileSync(configPath, JSON.stringify({ log: { stderr: false } }))
    setEnv({ CCP_LOG_STDERR: "1" })
    expect(logStderr()).toBe(true)
  })
})

describe("empty-string semantics", () => {
  it("empty CCP_CODEX_MODEL env falls through to file value", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { model: "gpt-5.2" } }))
    setEnv({ CCP_CODEX_MODEL: "" })
    expect(codexModel()).toBe("gpt-5.2")
  })

  it("empty CCP_CODEX_MODEL env with no file value returns undefined", () => {
    setEnv({ CCP_CODEX_MODEL: "" })
    expect(codexModel()).toBeUndefined()
  })

  it("empty CCP_CODEX_DEFAULT_EFFORT env falls through to file value", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { defaultEffort: "low" } }))
    setEnv({ CCP_CODEX_DEFAULT_EFFORT: "" })
    expect(codexDefaultEffort()).toBe("low")
  })

  it("empty PORT env falls through to file value", () => {
    writeFileSync(configPath, JSON.stringify({ port: 33333 }))
    setEnv({ PORT: "" })
    expect(port()).toBe(33333)
  })
})

describe("empty env-string compatibility", () => {
  it("empty CCP_CODEX_USER_AGENT env is a valid value (legacy ?? semantics)", () => {
    setEnv({ CCP_CODEX_USER_AGENT: "" })
    expect(codexUserAgent("default-ua")).toBe("")
  })

  it("empty CCP_KIMI_OAUTH_HOST env is a valid value (legacy ?? semantics)", () => {
    setEnv({ CCP_KIMI_OAUTH_HOST: "" })
    expect(kimiOauthHost()).toBe("")
  })

  it("CCP_LOG_STDERR set to empty string still enables stderr (legacy !! semantics)", () => {
    setEnv({ CCP_LOG_STDERR: "" })
    expect(logStderr()).toBe(true)
  })
})

describe("malformed config", () => {
  it("returns defaults when JSON is invalid", () => {
    writeFileSync(configPath, "{not valid json")
    setEnv({})
    expect(port()).toBe(18765)
  })

  it("ignores wrong-typed values with a warning, keeps other valid ones", () => {
    writeFileSync(
      configPath,
      JSON.stringify({ port: "not-a-number", codex: { userAgent: "good" } }),
    )
    setEnv({})
    expect(port()).toBe(18765)
    expect(codexUserAgent("default")).toBe("good")
  })

  it("returns defaults when file is missing entirely", () => {
    setEnv({})
    expect(port()).toBe(18765)
  })
})
