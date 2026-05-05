const MIN_SIGNATURE_LENGTH = 50
const SIGNATURE_TTL_MS = 2 * 60 * 60 * 1000

interface SignatureEntry {
  signature: string
  expiresAt: number
}

const toolSignatures = new Map<string, SignatureEntry>()

export function cacheGeminiToolSignature(toolUseId: string, signature: unknown): void {
  if (typeof signature !== "string" || signature.length < MIN_SIGNATURE_LENGTH) return
  toolSignatures.set(toolUseId, {
    signature,
    expiresAt: Date.now() + SIGNATURE_TTL_MS,
  })
}

export function geminiToolSignature(toolUseId: string): string | undefined {
  const entry = toolSignatures.get(toolUseId)
  if (!entry) return undefined
  if (entry.expiresAt <= Date.now()) {
    toolSignatures.delete(toolUseId)
    return undefined
  }
  return entry.signature
}

export function clearGeminiToolSignatures(): void {
  toolSignatures.clear()
}
