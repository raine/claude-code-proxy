import { describe, expect, it } from "bun:test"
import type { AnthropicRequest } from "../anthropic/schema.ts"
import { translateRequest } from "./request.ts"

describe("translateRequest", () => {
  const baseRequest: AnthropicRequest = {
    model: "claude-sonnet-4-6",
    messages: [{ role: "user", content: "hello" }],
  }

  it("omits reasoning include when reasoning is not enabled", () => {
    const translated = translateRequest(baseRequest)

    expect(translated.reasoning).toBeUndefined()
    expect(translated.include).toBeUndefined()
  })

  it("includes encrypted reasoning content when reasoning is enabled", () => {
    const translated = translateRequest({
      ...baseRequest,
      output_config: { effort: "medium" },
    })

    expect(translated.reasoning).toEqual({ effort: "medium" })
    expect(translated.include).toEqual(["reasoning.encrypted_content"])
  })
})
