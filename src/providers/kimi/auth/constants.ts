export const CLIENT_ID = "17e5f671-d194-4dfb-9706-5516cb48c098"
export const OAUTH_HOST = process.env.KIMI_OAUTH_HOST ?? "https://auth.kimi.com"
export const API_BASE_URL = process.env.KIMI_BASE_URL ?? "https://api.kimi.com/coding/v1"
// Masquerade as kimi-cli so the server accepts us. Bumping this in lockstep
// with kimi-cli releases may become necessary.
export const KIMI_CLI_VERSION = "1.37.0"
export const REFRESH_MARGIN_MS = 5 * 60 * 1000
