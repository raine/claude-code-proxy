import { CLIENT_ID, OAUTH_HOST, REFRESH_MARGIN_MS } from "./constants.ts"
import { commonHeaders } from "./headers.ts"
import { extractUserId } from "./jwt.ts"
import type { TokenResponse } from "./login.ts"
import { clearAuth, loadAuth, saveAuth, type StoredAuth } from "./token-store.ts"
import { createLogger } from "../../../log.ts"

const log = createLogger("kimi.auth")

const RETRYABLE_STATUSES = new Set([429, 500, 502, 503, 504])
const MAX_REFRESH_ATTEMPTS = 3

let cached: StoredAuth | undefined
let inflight: Promise<StoredAuth> | undefined

export class KimiAuthUnauthorizedError extends Error {
  constructor(message: string) {
    super(message)
    this.name = "KimiAuthUnauthorizedError"
  }
}

export async function getAuth(): Promise<StoredAuth> {
  if (!cached) {
    const stored = await loadAuth()
    if (!stored) throw new Error("Not authenticated. Run: claude-code-proxy kimi auth login")
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

export async function persistInitialTokens(tokens: TokenResponse): Promise<StoredAuth> {
  const auth = tokensToStored(tokens)
  await saveAuth(auth)
  cached = auth
  return auth
}

export function resetCache(): void {
  cached = undefined
}

function tokensToStored(tokens: TokenResponse): StoredAuth {
  return {
    access: tokens.access_token,
    refresh: tokens.refresh_token,
    expires: Date.now() + (tokens.expires_in ?? 900) * 1000,
    scope: tokens.scope,
    userId: extractUserId(tokens.access_token),
  }
}

async function refreshNow(current: StoredAuth): Promise<StoredAuth> {
  if (!current.refresh) {
    throw new KimiAuthUnauthorizedError("No refresh token stored; re-authenticate")
  }
  const headers = await commonHeaders()

  let lastErr: unknown
  for (let attempt = 0; attempt < MAX_REFRESH_ATTEMPTS; attempt++) {
    let resp: Response
    try {
      resp = await fetch(`${OAUTH_HOST}/api/oauth/token`, {
        method: "POST",
        headers: { ...headers, "Content-Type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({
          client_id: CLIENT_ID,
          grant_type: "refresh_token",
          refresh_token: current.refresh,
        }).toString(),
      })
    } catch (err) {
      lastErr = err
      log.warn("refresh network error", { attempt, err: String(err) })
      await backoff(attempt)
      continue
    }

    if (resp.status === 200) {
      const tokens = (await resp.json()) as TokenResponse
      const next: StoredAuth = {
        ...tokensToStored(tokens),
        refresh: tokens.refresh_token || current.refresh,
        userId: extractUserId(tokens.access_token) || current.userId,
      }
      await saveAuth(next)
      cached = next
      return next
    }

    if (resp.status === 401 || resp.status === 403) {
      // Refresh token is dead; clear local state so the next login is clean.
      cached = undefined
      await clearAuth().catch(() => undefined)
      const body = (await resp.json().catch(() => ({}))) as { error_description?: string }
      throw new KimiAuthUnauthorizedError(
        body.error_description || `Token refresh unauthorized (${resp.status})`,
      )
    }

    if (!RETRYABLE_STATUSES.has(resp.status)) {
      const text = await resp.text().catch(() => "")
      throw new Error(`Token refresh failed: ${resp.status} ${text}`)
    }

    lastErr = new Error(`Token refresh failed: ${resp.status}`)
    log.warn("refresh retryable error", { attempt, status: resp.status })
    await backoff(attempt)
  }
  throw new Error(`Token refresh failed after ${MAX_REFRESH_ATTEMPTS} attempts: ${lastErr}`)
}

function backoff(attempt: number): Promise<void> {
  const ms = 2 ** attempt * 1000
  return new Promise((r) => setTimeout(r, ms))
}
