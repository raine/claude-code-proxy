import { CLIENT_ID, OAUTH_HOST } from "./constants.ts"
import { commonHeaders } from "./headers.ts"

export interface TokenResponse {
  access_token: string
  refresh_token: string
  expires_in: number
  scope: string
  token_type: string
}

interface DeviceAuth {
  user_code: string
  device_code: string
  verification_uri?: string
  verification_uri_complete: string
  expires_in?: number
  interval: number
}

const GRANT_DEVICE_CODE = "urn:ietf:params:oauth:grant-type:device_code"
const POLL_SAFETY_MARGIN_MS = 500

export async function runDeviceLogin(): Promise<TokenResponse> {
  const headers = await commonHeaders()

  const initResp = await fetch(`${OAUTH_HOST}/api/oauth/device_authorization`, {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({ client_id: CLIENT_ID }).toString(),
  })
  if (!initResp.ok) {
    throw new Error(`Device authorization failed: ${initResp.status} ${await initResp.text()}`)
  }
  const auth = (await initResp.json()) as DeviceAuth
  const intervalMs = Math.max(auth.interval || 5, 1) * 1000

  console.log(`\nVisit: ${auth.verification_uri_complete}`)
  console.log(`Code:  ${auth.user_code}\n`)

  while (true) {
    const resp = await fetch(`${OAUTH_HOST}/api/oauth/token`, {
      method: "POST",
      headers: { ...headers, "Content-Type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({
        client_id: CLIENT_ID,
        device_code: auth.device_code,
        grant_type: GRANT_DEVICE_CODE,
      }).toString(),
    })

    if (resp.status === 200) {
      return (await resp.json()) as TokenResponse
    }

    // Pending / slow_down / expired come back as non-200 with a JSON error payload.
    const body = (await resp.json().catch(() => ({}))) as {
      error?: string
      error_description?: string
    }
    const error = body.error ?? `http_${resp.status}`

    if (error === "expired_token") {
      throw new Error("Device code expired. Run login again.")
    }
    if (error !== "authorization_pending" && error !== "slow_down") {
      throw new Error(
        `Device token poll failed (${resp.status}): ${error}${
          body.error_description ? ` — ${body.error_description}` : ""
        }`,
      )
    }
    await new Promise((r) => setTimeout(r, intervalMs + POLL_SAFETY_MARGIN_MS))
  }
}
