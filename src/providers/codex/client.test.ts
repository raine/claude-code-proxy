import { afterEach, expect, test } from "bun:test"
import { mkdtempSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { loadConfig } from "../../config.ts"
import type { RequestContext } from "../types.ts"
import { postCodex } from "./client.ts"

const originalFetch = globalThis.fetch
let dir: string | undefined

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

const body = {
  model: "gpt-5.5",
  input: [{ type: "message" as const, role: "user" as const, content: [{ type: "input_text" as const, text: "hi" }] }],
  store: false as const,
  stream: true as const,
}

function configure(env: NodeJS.ProcessEnv) {
  dir = mkdtempSync(join(tmpdir(), "ccp-client-"))
  loadConfig({ configPath: join(dir, "config.json"), env, forceReload: true })
}

afterEach(() => {
  globalThis.fetch = originalFetch
  loadConfig({ forceReload: true })
  if (dir) rmSync(dir, { recursive: true, force: true })
  dir = undefined
})

test("postCodex uses OpenAI-compatible bearer auth without ChatGPT headers", async () => {
  configure({ CCP_CODEX_AUTH_MODE: "openai", CCP_CODEX_API_KEY: "sk-test", CCP_CODEX_BASE_URL: "https://api.q1ngyuan.top" })
  let seenUrl = ""
  let seenHeaders = new Headers()
  globalThis.fetch = (async (input: Request | URL | string, init?: RequestInit) => {
    seenUrl = String(input)
    seenHeaders = new Headers(init?.headers)
    return new Response(new ReadableStream<Uint8Array>({ start(controller) { controller.close() } }), {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    })
  }) as typeof fetch

  await postCodex(body, ctx)

  expect(seenUrl).toBe("https://api.q1ngyuan.top")
  expect(seenHeaders.get("authorization")).toBe("Bearer sk-test")
  expect(seenHeaders.has("openai-beta")).toBe(false)
  expect(seenHeaders.has("originator")).toBe(false)
  expect(seenHeaders.has("ChatGPT-Account-Id")).toBe(false)
})

test("postCodex auto-selects OpenAI auth for non-ChatGPT base URL with API key", async () => {
  configure({ OPENAI_API_KEY: "sk-openai", CCP_CODEX_BASE_URL: "https://api.q1ngyuan.top" })
  let seenHeaders = new Headers()
  globalThis.fetch = (async (_input: Request | URL | string, init?: RequestInit) => {
    seenHeaders = new Headers(init?.headers)
    return new Response(new ReadableStream<Uint8Array>({ start(controller) { controller.close() } }), { status: 200 })
  }) as typeof fetch

  await postCodex(body, ctx)

  expect(seenHeaders.get("authorization")).toBe("Bearer sk-openai")
  expect(seenHeaders.has("openai-beta")).toBe(false)
})

test("postCodex retries transient socket failures before headers", async () => {
  configure({ CCP_CODEX_AUTH_MODE: "openai", CCP_CODEX_API_KEY: "sk-test", CCP_CODEX_BASE_URL: "https://api.q1ngyuan.top" })
  let calls = 0
  globalThis.fetch = (async (_input: Request | URL | string, _init?: RequestInit) => {
    calls++
    if (calls === 1) throw new TypeError("The socket connection was closed unexpectedly")
    return new Response(new ReadableStream<Uint8Array>({ start(controller) { controller.close() } }), { status: 200 })
  }) as typeof fetch

  await postCodex(body, ctx)

  expect(calls).toBe(2)
})
