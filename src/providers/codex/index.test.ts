import { afterEach, describe, expect, it } from "bun:test"
import type { RequestContext } from "../types.ts"
import { loadConfig } from "../../config.ts"
import { codexProvider } from "./index.ts"

const ctx: RequestContext = {
  reqId: "test-req",
  signal: new AbortController().signal,
  childLogger: () => ({
    debug() {},
    info() {},
    warn() {},
    error() {},
    child() {
      return this
    },
  }),
}

afterEach(() => {
  loadConfig({ forceReload: true })
})

describe("codexProvider", () => {
  it("returns 400 for invalid service tier config during token counting", async () => {
    loadConfig({ env: { CCP_CODEX_SERVICE_TIER: "standard" }, forceReload: true })

    const resp = await codexProvider.handleCountTokens(
      { model: "gpt-5.4", messages: [{ role: "user", content: "hello" }] },
      ctx,
    )

    expect(resp.status).toBe(400)
    expect(await resp.json()).toEqual({
      type: "error",
      error: {
        type: "invalid_request_error",
        message: 'Invalid service tier override: "standard". Must be one of: fast, priority, flex',
      },
    })
  })

  it("returns 400 for invalid forced model during token counting", async () => {
    loadConfig({ env: { CCP_CODEX_MODEL: "gpt-4.1" }, forceReload: true })

    const resp = await codexProvider.handleCountTokens(
      { model: "gpt-5.4", messages: [{ role: "user", content: "hello" }] },
      ctx,
    )

    expect(resp.status).toBe(400)
    expect(await resp.json()).toEqual({
      type: "error",
      error: {
        type: "invalid_request_error",
        message: 'Model "gpt-5.4" resolves to unsupported model "gpt-4.1"',
      },
    })
  })
})
