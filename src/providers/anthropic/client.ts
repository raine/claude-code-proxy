import { loadToken } from "./auth/token-store.ts"

export const ANTHROPIC_BASE_URL = "https://api.anthropic.com"

export class AnthropicError extends Error {
  constructor(
    public readonly status: number,
    public readonly detail: string,
  ) {
    super(`Anthropic ${status}: ${detail}`)
  }
}

export async function postAnthropic(
  path: string,
  body: unknown,
  signal?: AbortSignal,
): Promise<Response> {
  const token = loadToken()
  if (!token) {
    throw new AnthropicError(401, "No Claude Code credentials found in keychain. Run Claude Code at least once to authenticate.")
  }

  return fetch(`${ANTHROPIC_BASE_URL}${path}`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${token}`,
      "content-type": "application/json",
      "anthropic-version": "2023-06-01",
    },
    body: JSON.stringify(body),
    signal,
  })
}
