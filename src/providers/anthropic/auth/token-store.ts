import { keychainGet } from "../../../keychain.ts"
import { userInfo } from "node:os"

const KEYCHAIN_SERVICE = "Claude Code-credentials"

interface ClaudeOAuthToken {
  accessToken: string
  refreshToken: string
  expiresAt?: number
}

interface ClaudeCredentials {
  claudeAiOauth: ClaudeOAuthToken
}

export function loadToken(): string | undefined {
  const account = userInfo().username
  let raw: string | undefined
  try {
    raw = keychainGet(KEYCHAIN_SERVICE, account)
  } catch {
    return undefined
  }
  if (!raw) return undefined
  try {
    const creds = JSON.parse(raw) as ClaudeCredentials
    return creds.claudeAiOauth?.accessToken
  } catch {
    return undefined
  }
}

export function tokenStatus(): { found: boolean; account: string } {
  const account = userInfo().username
  const token = loadToken()
  return { found: !!token, account }
}
