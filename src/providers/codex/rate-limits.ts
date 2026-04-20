import { mkdir, rename, writeFile } from "node:fs/promises"
import { join } from "node:path"
import { createLogger, stateDir } from "../../log.ts"

const log = createLogger("codex.rate-limits")

interface UpstreamRateLimitWindow {
  used_percentage?: unknown
  used_percent?: unknown
  reset_after_seconds?: unknown
  reset_at?: unknown
}

interface UpstreamRateLimits {
  primary?: UpstreamRateLimitWindow | null
  secondary?: UpstreamRateLimitWindow | null
  primary_window?: UpstreamRateLimitWindow | null
  secondary_window?: UpstreamRateLimitWindow | null
}

interface UsageResponse {
  rate_limit?: UpstreamRateLimits | null
}

export interface RateLimitWindowSnapshot {
  used_percentage: number | null
  resets_at: number | null
}

export interface RateLimitsSnapshot {
  five_hour: RateLimitWindowSnapshot | null
  seven_day: RateLimitWindowSnapshot | null
  updated_at: string
  account_id?: string
}

export function rateLimitsPath(): string {
  return join(stateDir(), "rate_limits.json")
}

export function mapRateLimitsSnapshot(
  rateLimits: unknown,
  accountId?: string,
  now = Date.now(),
): RateLimitsSnapshot | null {
  if (!rateLimits || typeof rateLimits !== "object" || Array.isArray(rateLimits)) return null
  const data = rateLimits as UpstreamRateLimits
  const fiveHour = mapWindow(data.primary_window ?? data.primary, now)
  const sevenDay = mapWindow(data.secondary_window ?? data.secondary, now)
  if (!fiveHour && !sevenDay) return null
  return {
    five_hour: fiveHour,
    seven_day: sevenDay,
    updated_at: new Date(now).toISOString(),
    ...(accountId ? { account_id: accountId } : {}),
  }
}

export function mapUsageSnapshot(
  usage: unknown,
  accountId?: string,
  now = Date.now(),
): RateLimitsSnapshot | null {
  if (!usage || typeof usage !== "object" || Array.isArray(usage)) return null
  return mapRateLimitsSnapshot((usage as UsageResponse).rate_limit, accountId, now)
}

function mapWindow(window: UpstreamRateLimitWindow | null | undefined, now: number): RateLimitWindowSnapshot | null {
  if (!window || typeof window !== "object" || Array.isArray(window)) return null
  const used_percentage = parseFiniteNumber(window.used_percentage ?? window.used_percent)
  const resetAt = parseFiniteNumber(window.reset_at)
  const resetAfterSeconds = parseFiniteNumber(window.reset_after_seconds)
  const resets_at =
    resetAt !== null && resetAt >= 0
      ? Math.floor(resetAt)
      : resetAfterSeconds !== null && resetAfterSeconds >= 0
        ? Math.floor((now + resetAfterSeconds * 1000) / 1000)
        : null
  if (used_percentage === null && resets_at === null) return null
  return { used_percentage, resets_at }
}

function parseFiniteNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null
}

export class RateLimitsSidecarWriter {
  readonly path = rateLimitsPath()

  async write(snapshot: RateLimitsSnapshot): Promise<void> {
    const dir = stateDir()
    const tmp = `${this.path}.${process.pid}.${Date.now()}.tmp`
    try {
      await mkdir(dir, { recursive: true })
      await writeFile(tmp, JSON.stringify(snapshot, null, 2), "utf8")
      await rename(tmp, this.path)
    } catch (err) {
      log.warn("failed to write rate limits snapshot", { err: String(err), path: this.path })
    }
  }
}
