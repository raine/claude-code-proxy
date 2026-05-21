import { createServer } from "node:http"
import { CLIENT_ID, ISSUER, OAUTH_PORT, OAUTH_REDIRECT_URI, ORIGINATOR } from "./constants.ts"
import type { TokenResponse } from "./jwt.ts"

export interface PkceCodes {
  verifier: string
  challenge: string
}

export async function generatePKCE(): Promise<PkceCodes> {
  const verifier = base64UrlEncode(crypto.getRandomValues(new Uint8Array(32)).buffer)
  const hash = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier))
  return { verifier, challenge: base64UrlEncode(hash) }
}

function base64UrlEncode(buffer: ArrayBuffer): string {
  const bytes = new Uint8Array(buffer)
  let binary = ""
  for (const b of bytes) binary += String.fromCharCode(b)
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "")
}

export function generateState(): string {
  return base64UrlEncode(crypto.getRandomValues(new Uint8Array(32)).buffer)
}

export function buildAuthorizeUrl(pkce: PkceCodes, state: string): string {
  const params = new URLSearchParams({
    response_type: "code",
    client_id: CLIENT_ID,
    redirect_uri: OAUTH_REDIRECT_URI,
    scope: "openid profile email offline_access",
    code_challenge: pkce.challenge,
    code_challenge_method: "S256",
    id_token_add_organizations: "true",
    codex_cli_simplified_flow: "true",
    state,
    originator: ORIGINATOR,
  })
  return `${ISSUER}/oauth/authorize?${params.toString()}`
}

export async function exchangeCodeForTokens(code: string, pkce: PkceCodes): Promise<TokenResponse> {
  const response = await fetch(`${ISSUER}/oauth/token`, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "authorization_code",
      code,
      redirect_uri: OAUTH_REDIRECT_URI,
      client_id: CLIENT_ID,
      code_verifier: pkce.verifier,
    }).toString(),
  })
  if (!response.ok) throw new Error(`Token exchange failed: ${response.status} ${await response.text()}`)
  return (await response.json()) as TokenResponse
}

export async function runBrowserLogin(): Promise<TokenResponse> {
  const pkce = await generatePKCE()
  const state = generateState()
  const authUrl = buildAuthorizeUrl(pkce, state)

  return new Promise<TokenResponse>((resolve, reject) => {
    const cleanup = () => {
      clearTimeout(timeout)
      server.close()
      server.closeAllConnections?.()
    }
    const server = createServer((req, res) => {
      const url = new URL(req.url || "/", `http://localhost:${OAUTH_PORT}`)
      if (url.pathname !== "/auth/callback") {
        res.writeHead(404)
        res.end("Not found")
        return
      }
      const code = url.searchParams.get("code")
      const receivedState = url.searchParams.get("state")
      const error = url.searchParams.get("error")
      if (error || !code || receivedState !== state) {
        const msg = error || "Invalid callback"
        res.writeHead(400, { "Content-Type": "text/plain" })
        res.end(`Auth failed: ${msg}`)
        cleanup()
        reject(new Error(msg))
        return
      }
      exchangeCodeForTokens(code, pkce)
        .then((tokens) => {
          res.writeHead(200, { "Content-Type": "text/html" })
          res.end(
            "<html><body><h1>Authorization Successful</h1><p>You can close this window.</p></body></html>",
          )
          cleanup()
          resolve(tokens)
        })
        .catch((err) => {
          res.writeHead(500, { "Content-Type": "text/plain" })
          res.end(String(err))
          cleanup()
          reject(err)
        })
    })
    server.listen(OAUTH_PORT, "127.0.0.1", () => {
      console.log(`Open this URL in your browser to authorize:\n\n  ${authUrl}\n`)
    })
    server.on("error", reject)
    const timeout = setTimeout(
      () => {
        cleanup()
        reject(new Error("OAuth timeout"))
      },
      5 * 60 * 1000,
    )
  })
}
