import { CLIENT_ID, ISSUER, REFRESH_MARGIN_MS } from "./constants.ts"
import { extractAccountId, type TokenResponse } from "./jwt.ts"
import { loadAuth, saveAuth, type StoredAuth } from "./token-store.ts"

function validateTokenResponse(t: unknown): asserts t is TokenResponse {
  if (!t || typeof t !== "object") throw new Error("Invalid token response: not an object")
  const o = t as Record<string, unknown>
  if (typeof o.access_token !== "string" || !o.access_token)
    throw new Error("Invalid token response: missing access_token")
  if (typeof o.refresh_token !== "string" || !o.refresh_token)
    throw new Error("Invalid token response: missing refresh_token")
  if (o.expires_in !== undefined && (typeof o.expires_in !== "number" || !Number.isFinite(o.expires_in) || o.expires_in <= 0))
    throw new Error("Invalid token response: bad expires_in")
}

let cached: StoredAuth | undefined
let inflight: Promise<StoredAuth> | undefined

export async function getAuth(): Promise<StoredAuth> {
  if (!cached) {
    const stored = await loadAuth()
    if (!stored) throw new Error("Not authenticated. Run: claude-code-proxy codex auth login")
    cached = stored
  }
  if (cached.expires - REFRESH_MARGIN_MS > Date.now()) {
    return cached
  }
  if (inflight) return inflight
  inflight = refreshNow(cached).finally(() => {
    inflight = undefined
  })
  return inflight
}

export async function forceRefresh(): Promise<StoredAuth> {
  if (!cached) {
    const stored = await loadAuth()
    if (!stored) throw new Error("Not authenticated")
    cached = stored
  }
  if (inflight) return inflight
  inflight = refreshNow(cached).finally(() => {
    inflight = undefined
  })
  return inflight
}

async function refreshNow(current: StoredAuth): Promise<StoredAuth> {
  const resp = await fetch(`${ISSUER}/oauth/token`, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "refresh_token",
      refresh_token: current.refresh,
      client_id: CLIENT_ID,
    }).toString(),
  })
  if (!resp.ok) throw new Error(`Token refresh failed: ${resp.status}`)
  const tokens = await resp.json()
  validateTokenResponse(tokens)
  const accountId = extractAccountId(tokens) || current.accountId
  const next: StoredAuth = {
    access: tokens.access_token,
    refresh: tokens.refresh_token || current.refresh,
    expires: Date.now() + (tokens.expires_in ?? 3600) * 1000,
    accountId,
  }
  await saveAuth(next)
  cached = next
  return next
}

export async function persistInitialTokens(tokens: TokenResponse): Promise<StoredAuth> {
  validateTokenResponse(tokens)
  const auth: StoredAuth = {
    access: tokens.access_token,
    refresh: tokens.refresh_token,
    expires: Date.now() + (tokens.expires_in ?? 3600) * 1000,
    accountId: extractAccountId(tokens),
  }
  await saveAuth(auth)
  cached = auth
  return auth
}

export function resetCache(): void {
  cached = undefined
}
