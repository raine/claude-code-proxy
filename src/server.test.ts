import { afterEach, describe, expect, it } from "bun:test"
import { loadConfig } from "./config.ts"
import { startServer } from "./server.ts"

const servers: Array<{ stop: () => void }> = []

afterEach(() => {
  for (const server of servers.splice(0)) server.stop()
  loadConfig({ forceReload: true })
})

function countTokens(port: number, model: string, sessionId?: string): Promise<Response> {
  return fetch(`http://127.0.0.1:${port}/v1/messages/count_tokens`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      ...(sessionId ? { "x-claude-code-session-id": sessionId } : {}),
    },
    body: JSON.stringify({ model, messages: [{ role: "user", content: "hello" }] }),
  })
}

describe("server session-aware alias routing", () => {
  it("routes aliases to the concrete provider used earlier in the session", async () => {
    loadConfig({ env: { CCP_CODEX_SERVICE_TIER: "standard" }, forceReload: true })
    const server = startServer({ port: 0 })
    servers.push(server)

    const sessionId = crypto.randomUUID()
    const fallback = await countTokens(server.port, "sonnet")
    expect(fallback.status).toBe(400)
    const fallbackBody = (await fallback.json()) as { error: { message: string } }
    expect(fallbackBody.error.message).toContain("Invalid service tier override")

    expect((await countTokens(server.port, "kimi-for-coding", sessionId)).status).toBe(200)
    expect((await countTokens(server.port, "sonnet", sessionId)).status).toBe(200)
  })
})
