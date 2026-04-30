import type { Logger } from "../log.ts"

export const RETRY_INITIAL_DELAY_MS = 2000
export const RETRY_BACKOFF_FACTOR = 2
export const RETRY_MAX_DELAY_MS = 30_000
export const MAX_RATE_LIMIT_RETRIES = 3

const STRICT_NUMERIC = /^\s*\d+(?:\.\d+)?\s*$/

export interface BackoffOutcome {
  waitMs: number
  exceedsBudget: boolean
}

export function computeBackoffDelay(attempt: number, retryAfter?: string): BackoffOutcome {
  if (retryAfter) {
    if (STRICT_NUMERIC.test(retryAfter)) {
      const ms = Math.ceil(Number.parseFloat(retryAfter) * 1000)
      return { waitMs: Math.min(ms, RETRY_MAX_DELAY_MS), exceedsBudget: ms > RETRY_MAX_DELAY_MS }
    }
    const dateMs = Date.parse(retryAfter) - Date.now()
    if (!Number.isNaN(dateMs) && dateMs > 0) {
      return {
        waitMs: Math.min(Math.ceil(dateMs), RETRY_MAX_DELAY_MS),
        exceedsBudget: dateMs > RETRY_MAX_DELAY_MS,
      }
    }
  }
  const exp = RETRY_INITIAL_DELAY_MS * Math.pow(RETRY_BACKOFF_FACTOR, attempt)
  const capped = Math.min(exp, RETRY_MAX_DELAY_MS)
  // Equal jitter on the exponential fallback to avoid synchronized retries
  // across concurrent requests. Never jitter an explicit Retry-After.
  const jittered = capped / 2 + Math.random() * (capped / 2)
  return { waitMs: Math.round(jittered), exceedsBudget: false }
}

export function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(new DOMException("Aborted", "AbortError"))
      return
    }
    const onAbort = () => {
      clearTimeout(timer)
      reject(new DOMException("Aborted", "AbortError"))
    }
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort)
      resolve()
    }, ms)
    signal?.addEventListener("abort", onAbort, { once: true })
  })
}

export interface RateLimitInfo {
  retryAfter?: string
}

export interface RetryOptions {
  log: Logger
  signal?: AbortSignal
  classify: (err: unknown) => RateLimitInfo | undefined
}

export async function retryOn429<T>(run: () => Promise<T>, opts: RetryOptions): Promise<T> {
  for (let attempt = 0; ; attempt++) {
    try {
      return await run()
    } catch (err) {
      const info = opts.classify(err)
      if (!info || attempt >= MAX_RATE_LIMIT_RETRIES) throw err
      const { waitMs, exceedsBudget } = computeBackoffDelay(attempt, info.retryAfter)
      if (exceedsBudget) {
        opts.log.warn("upstream 429 retry-after exceeds budget; giving up", {
          retryAfter: info.retryAfter,
          maxDelayMs: RETRY_MAX_DELAY_MS,
        })
        throw err
      }
      opts.log.warn("upstream 429, retrying after backoff", {
        attempt: attempt + 1,
        maxRetries: MAX_RATE_LIMIT_RETRIES,
        waitMs,
        retryAfter: info.retryAfter,
      })
      await sleep(waitMs, opts.signal)
    }
  }
}
