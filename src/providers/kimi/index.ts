import type { Provider, CliHandlers, RequestContext } from "../types.ts"
import type { AnthropicRequest } from "../../anthropic/schema.ts"
import { runDeviceLogin } from "./auth/login.ts"
import { persistInitialTokens } from "./auth/manager.ts"
import { loadAuth, clearAuth, authPath } from "./auth/token-store.ts"

function notImplemented(_body: AnthropicRequest, _ctx: RequestContext): Promise<Response> {
  return Promise.resolve(
    new Response(
      JSON.stringify({
        type: "error",
        error: { type: "api_error", message: "kimi provider: chat not implemented yet" },
      }),
      { status: 501, headers: { "content-type": "application/json" } },
    ),
  )
}

const cli: CliHandlers = {
  async login() {
    const tokens = await runDeviceLogin()
    const saved = await persistInitialTokens(tokens)
    console.log(`Auth saved in ${authPath()}`)
    if (saved.userId) console.log(`User: ${saved.userId}`)
    const secs = Math.floor((saved.expires - Date.now()) / 1000)
    console.log(`Expires in ${secs}s`)
  },
  async status() {
    const auth = await loadAuth()
    if (!auth) {
      console.log("Not authenticated")
      process.exit(1)
    }
    const ms = auth.expires - Date.now()
    console.log(`User: ${auth.userId ?? "(none)"}`)
    console.log(`Expires: ${new Date(auth.expires).toISOString()} (in ${Math.floor(ms / 1000)}s)`)
    console.log(`Scope: ${auth.scope ?? "(none)"}`)
    console.log(`Storage: ${authPath()}`)
  },
  async logout() {
    await clearAuth()
    console.log("Logged out")
  },
}

export const kimiProvider: Provider = {
  name: "kimi",
  handleMessages: notImplemented,
  handleCountTokens: notImplemented,
  cli,
}
