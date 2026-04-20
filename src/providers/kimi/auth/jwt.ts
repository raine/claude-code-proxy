interface KimiClaims {
  user_id?: string
  device_id?: string
  scope?: string
  exp?: number
  iat?: number
}

export function decodeClaims(jwt: string): KimiClaims | undefined {
  const parts = jwt.split(".")
  if (parts.length < 2) return undefined
  try {
    const payload = parts[1]!.replace(/-/g, "+").replace(/_/g, "/")
    const padded = payload + "=".repeat((4 - (payload.length % 4)) % 4)
    return JSON.parse(Buffer.from(padded, "base64").toString("utf8")) as KimiClaims
  } catch {
    return undefined
  }
}

export function extractUserId(accessToken: string): string | undefined {
  return decodeClaims(accessToken)?.user_id
}
