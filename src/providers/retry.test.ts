import { describe, expect, it } from "bun:test"
import {
  computeBackoffDelay,
  MAX_RATE_LIMIT_RETRIES,
  RETRY_INITIAL_DELAY_MS,
  RETRY_MAX_DELAY_MS,
  retryOn429,
  sleep,
} from "./retry.ts"

const silentLog = {
  debug: () => {},
  info: () => {},
  warn: () => {},
  error: () => {},
  child: () => silentLog,
}

describe("computeBackoffDelay", () => {
  it("uses jittered exponential backoff without retry-after", () => {
    // equal jitter: result is in [cap/2, cap]
    for (const attempt of [0, 1, 2]) {
      const cap = RETRY_INITIAL_DELAY_MS * 2 ** attempt
      const { waitMs } = computeBackoffDelay(attempt)
      expect(waitMs).toBeGreaterThanOrEqual(cap / 2)
      expect(waitMs).toBeLessThanOrEqual(cap)
    }
  })

  it("caps exponential backoff at max delay", () => {
    const { waitMs } = computeBackoffDelay(20)
    expect(waitMs).toBeGreaterThanOrEqual(RETRY_MAX_DELAY_MS / 2)
    expect(waitMs).toBeLessThanOrEqual(RETRY_MAX_DELAY_MS)
  })

  it("respects numeric retry-after as seconds", () => {
    expect(computeBackoffDelay(0, "5").waitMs).toBe(5000)
  })

  it("flags retry-after that exceeds budget", () => {
    const out = computeBackoffDelay(0, "120")
    expect(out.waitMs).toBe(RETRY_MAX_DELAY_MS)
    expect(out.exceedsBudget).toBe(true)
  })

  it("rejects non-numeric retry-after garbage", () => {
    const { waitMs } = computeBackoffDelay(0, "1abc")
    expect(waitMs).toBeGreaterThanOrEqual(RETRY_INITIAL_DELAY_MS / 2)
    expect(waitMs).toBeLessThanOrEqual(RETRY_INITIAL_DELAY_MS)
  })

  it("parses HTTP-date retry-after", () => {
    const date = new Date(Date.now() + 5000).toUTCString()
    const out = computeBackoffDelay(0, date)
    expect(out.waitMs).toBeGreaterThan(3500)
    expect(out.waitMs).toBeLessThanOrEqual(5000)
  })
})

describe("sleep", () => {
  it("rejects if signal already aborted", async () => {
    const c = new AbortController()
    c.abort()
    await expect(sleep(1000, c.signal)).rejects.toThrow()
  })

  it("rejects when aborted mid-sleep", async () => {
    const c = new AbortController()
    const p = sleep(1000, c.signal)
    setTimeout(() => c.abort(), 10)
    await expect(p).rejects.toThrow()
  })
})

describe("retryOn429", () => {
  class FakeRateLimit extends Error {
    constructor(public retryAfter?: string) {
      super("rate limited")
    }
  }

  it("returns successful result without retry", async () => {
    let calls = 0
    const result = await retryOn429(
      async () => {
        calls++
        return "ok"
      },
      {
        log: silentLog,
        classify: () => undefined,
      },
    )
    expect(result).toBe("ok")
    expect(calls).toBe(1)
  })

  it("retries up to MAX_RATE_LIMIT_RETRIES then throws", async () => {
    let calls = 0
    const start = Date.now()
    await expect(
      retryOn429(
        async () => {
          calls++
          throw new FakeRateLimit("0")
        },
        {
          log: silentLog,
          classify: (err) =>
            err instanceof FakeRateLimit ? { retryAfter: err.retryAfter } : undefined,
        },
      ),
    ).rejects.toBeInstanceOf(FakeRateLimit)
    expect(calls).toBe(MAX_RATE_LIMIT_RETRIES + 1)
    expect(Date.now() - start).toBeLessThan(2000)
  })

  it("gives up immediately when retry-after exceeds budget", async () => {
    let calls = 0
    await expect(
      retryOn429(
        async () => {
          calls++
          throw new FakeRateLimit("120")
        },
        {
          log: silentLog,
          classify: (err) =>
            err instanceof FakeRateLimit ? { retryAfter: err.retryAfter } : undefined,
        },
      ),
    ).rejects.toBeInstanceOf(FakeRateLimit)
    expect(calls).toBe(1)
  })

  it("does not retry non-rate-limit errors", async () => {
    let calls = 0
    await expect(
      retryOn429(
        async () => {
          calls++
          throw new Error("other")
        },
        {
          log: silentLog,
          classify: () => undefined,
        },
      ),
    ).rejects.toThrow("other")
    expect(calls).toBe(1)
  })

  it("succeeds after a transient 429", async () => {
    let calls = 0
    const result = await retryOn429(
      async () => {
        calls++
        if (calls === 1) throw new FakeRateLimit("0")
        return "recovered"
      },
      {
        log: silentLog,
        classify: (err) =>
          err instanceof FakeRateLimit ? { retryAfter: err.retryAfter } : undefined,
      },
    )
    expect(result).toBe("recovered")
    expect(calls).toBe(2)
  })
})
