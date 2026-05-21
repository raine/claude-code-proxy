import { kimiBaseUrl, kimiOauthHost } from "../../../config.ts"

export const CLIENT_ID = "17e5f671-d194-4dfb-9706-5516cb48c098"
// Read lazily so config.json values apply. CCP_KIMI_OAUTH_HOST / CCP_KIMI_BASE_URL
// env vars still work (env wins over file in the getter).
export function oauthHost(): string {
  return kimiOauthHost()
}
export function apiBaseUrl(): string {
  return kimiBaseUrl()
}
// Masquerade as kimi-cli so the server accepts us. Bumping this in lockstep
// with kimi-cli releases may become necessary.
export const KIMI_CLI_VERSION = "1.37.0"
export const REFRESH_MARGIN_MS = 5 * 60 * 1000
