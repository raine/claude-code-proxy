import { describe, expect, it } from "bun:test"
import { geminiRetryAfter } from "./client.ts"

describe("geminiRetryAfter", () => {
  it("extracts reset seconds from Code Assist JSON errors", () => {
    expect(
      geminiRetryAfter(
        JSON.stringify({
          error: {
            message: "You have exhausted your capacity on this model. Your quota will reset after 45s.",
          },
        }),
      ),
    ).toBe("45")
  })

  it("extracts reset minutes from plain text", () => {
    expect(geminiRetryAfter("Usage limit reached. Your quota will reset after 2 minutes.")).toBe(
      "120",
    )
  })

  it("returns undefined when there is no reset hint", () => {
    expect(geminiRetryAfter("rate limited")).toBeUndefined()
  })
})
