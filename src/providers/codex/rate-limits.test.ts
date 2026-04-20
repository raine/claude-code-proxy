import { expect, test } from "bun:test"
import { mapRateLimitsSnapshot, mapUsageSnapshot } from "./rate-limits.ts"

test("mapRateLimitsSnapshot maps upstream codex payload", () => {
  const snapshot = mapRateLimitsSnapshot(
    {
      primary_window: { used_percentage: 12, reset_after_seconds: 1800 },
      secondary_window: { used_percentage: 34, reset_after_seconds: 7200 },
    },
    "acct_123",
    1_700_000_000_000,
  )

  expect(snapshot).toEqual({
    five_hour: { used_percentage: 12, resets_at: 1700001800 },
    seven_day: { used_percentage: 34, resets_at: 1700007200 },
    updated_at: "2023-11-14T22:13:20.000Z",
    account_id: "acct_123",
  })
})

test("mapUsageSnapshot maps wham usage payload", () => {
  const snapshot = mapUsageSnapshot(
    {
      rate_limit: {
        primary_window: { used_percent: 1, reset_after_seconds: 17255, reset_at: 1776708894 },
        secondary_window: { used_percent: 44, reset_after_seconds: 95841, reset_at: 1776787480 },
      },
    },
    "acct_456",
    1_700_000_000_000,
  )

  expect(snapshot).toEqual({
    five_hour: { used_percentage: 1, resets_at: 1776708894 },
    seven_day: { used_percentage: 44, resets_at: 1776787480 },
    updated_at: "2023-11-14T22:13:20.000Z",
    account_id: "acct_456",
  })
})
