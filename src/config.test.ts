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
  codexServiceTier,
  codexBaseUrl,
  aliasProvider,
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
    expect(codexServiceTier()).toBeUndefined()
    expect(codexBaseUrl("default-codex-url")).toBe("default-codex-url")
    expect(aliasProvider()).toBe("codex")
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

  it("codex.serviceTier from config.json", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { serviceTier: "fast" } }))
    setEnv({})
    expect(codexServiceTier()).toBe("fast")
  })

  it("codex.baseUrl from config.json", () => {
    writeFileSync(
      configPath,
      JSON.stringify({
        codex: { baseUrl: "http://127.0.0.1:2455/backend-api/codex/responses" },
      }),
    )
    setEnv({})
    expect(codexBaseUrl("default")).toBe(
      "http://127.0.0.1:2455/backend-api/codex/responses",
    )
  })

  it("aliasProvider from config.json", () => {
    writeFileSync(configPath, JSON.stringify({ aliasProvider: "codex" }))
    setEnv({})
    expect(aliasProvider()).toBe("codex")
  })

  it("kimi.oauthHost from config.json", () => {
    writeFileSync(
      configPath,
      JSON.stringify({ kimi: { oauthHost: "https://auth.example.com" } }),
    )
    setEnv({})
    expect(kimiOauthHost()).toBe("https://auth.example.com")
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

  it("CCP_CODEX_SERVICE_TIER env wins over config", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { serviceTier: "flex" } }))
    setEnv({ CCP_CODEX_SERVICE_TIER: "fast" })
    expect(codexServiceTier()).toBe("fast")
  })

  it("CCP_CODEX_BASE_URL env wins over config", () => {
    writeFileSync(
      configPath,
      JSON.stringify({ codex: { baseUrl: "http://127.0.0.1:2455/file" } }),
    )
    setEnv({ CCP_CODEX_BASE_URL: "http://127.0.0.1:2455/env" })
    expect(codexBaseUrl("default")).toBe("http://127.0.0.1:2455/env")
  })

  it("CCP_ALIAS_PROVIDER env wins over config", () => {
    writeFileSync(configPath, JSON.stringify({ aliasProvider: "kimi" }))
    setEnv({ CCP_ALIAS_PROVIDER: "codex" })
    expect(aliasProvider()).toBe("codex")
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

  it("empty CCP_CODEX_SERVICE_TIER env falls through to file value", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { serviceTier: "flex" } }))
    setEnv({ CCP_CODEX_SERVICE_TIER: "" })
    expect(codexServiceTier()).toBe("flex")
  })

  it("empty CCP_ALIAS_PROVIDER env falls through to file value", () => {
    writeFileSync(configPath, JSON.stringify({ aliasProvider: "codex" }))
    setEnv({ CCP_ALIAS_PROVIDER: "" })
    expect(aliasProvider()).toBe("codex")
  })

  it("invalid CCP_ALIAS_PROVIDER env falls through to file value", () => {
    writeFileSync(configPath, JSON.stringify({ aliasProvider: "codex" }))
    setEnv({ CCP_ALIAS_PROVIDER: "openai" })
    expect(aliasProvider()).toBe("codex")
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

  it("ignores invalid aliasProvider values", () => {
    writeFileSync(configPath, JSON.stringify({ aliasProvider: "openai" }))
    setEnv({})
    expect(aliasProvider()).toBe("codex")
  })

  it("returns defaults when file is missing entirely", () => {
    setEnv({})
    expect(port()).toBe(18765)
  })
})
