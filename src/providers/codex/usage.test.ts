import { describe, expect, it, afterEach } from "bun:test"
import { normalizeCodexUsage, resetCodexUsageCacheForTests } from "./usage.ts"

afterEach(() => {
  resetCodexUsageCacheForTests()
})

describe("normalizeCodexUsage", () => {
  it("maps Codex usage windows to proxy status shape", () => {
    const usage = normalizeCodexUsage({
      plan_type: "pro",
      rate_limit: {
        limit_reached: false,
        primary_window: {
          used_percent: 3,
          limit_window_seconds: 18000,
          reset_at: 1777932010,
        },
        secondary_window: {
          used_percent: 39,
          limit_window_seconds: 604800,
          reset_at: 1777982406,
        },
      },
    })

    expect(usage.provider).toBe("codex")
    expect(usage.stale).toBe(false)
    expect(usage.plan_type).toBe("pro")
    expect(usage.rate_limit_reached).toBe(false)
    expect(usage.rate_limits.primary).toEqual({
      used_percent: 3,
      window_minutes: 300,
      resets_at: 1777932010,
    })
    expect(usage.rate_limits.secondary).toEqual({
      used_percent: 39,
      window_minutes: 10080,
      resets_at: 1777982406,
    })
  })

  it("falls back to reset_after_seconds when reset_at is missing", () => {
    const before = Math.floor(Date.now() / 1000)
    const usage = normalizeCodexUsage({
      rate_limit: {
        primary_window: {
          used_percent: 12,
          limit_window_seconds: 18000,
          reset_after_seconds: 60,
        },
      },
    })

    expect(usage.rate_limits.primary?.used_percent).toBe(12)
    expect(usage.rate_limits.primary?.window_minutes).toBe(300)
    expect(usage.rate_limits.primary?.resets_at).toBeGreaterThanOrEqual(before + 60)
    expect(usage.rate_limits.primary?.resets_at).toBeLessThanOrEqual(before + 61)
    expect(usage.rate_limits.secondary).toBeNull()
  })
})
