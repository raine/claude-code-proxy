import { CLIENT_ID, ISSUER, REFRESH_MARGIN_MS } from "./constants.ts"
import { extractAccountId, type TokenResponse } from "./jwt.ts"
import { clearAuth, loadAuth, saveAuth, type StoredAuth } from "./token-store.ts"

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
  if (resp.status === 401 || resp.status === 403) {
    cached = undefined
    await clearAuth().catch(() => undefined)
    throw new Error(`Token refresh unauthorized (${resp.status})`)
  }
  if (!resp.ok) throw new Error(`Token refresh failed: ${resp.status} ${await resp.text()}`)
  const tokens = (await resp.json()) as TokenResponse
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
