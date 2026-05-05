import { describe, expect, it } from "bun:test"
import type { AnthropicRequest } from "../../../anthropic/schema.ts"
import { loadConfig } from "../../../config.ts"
import { translateRequest } from "./request.ts"

const baseRequest: AnthropicRequest = {
  model: "claude-sonnet-4-6",
  messages: [{ role: "user", content: "hello" }],
}

describe("translateRequest", () => {
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

  it("maps Claude max effort to Codex xhigh reasoning effort", () => {
    const translated = translateRequest({
      ...baseRequest,
      output_config: { effort: "max" },
    })

    expect(translated.reasoning).toEqual({ effort: "xhigh" })
    expect(translated.include).toEqual(["reasoning.encrypted_content"])
  })

  it("maps max effort override to Codex xhigh reasoning effort", () => {
    loadConfig({ env: { CCP_CODEX_EFFORT: "max" }, forceReload: true })
    try {
      const translated = translateRequest(baseRequest)

      expect(translated.reasoning).toEqual({ effort: "xhigh" })
      expect(translated.include).toEqual(["reasoning.encrypted_content"])
    } finally {
      loadConfig({ env: {}, forceReload: true })
    }
  })

  it("uses the default effort only when Claude Code does not send effort", () => {
    loadConfig({ env: { CCP_CODEX_DEFAULT_EFFORT: "high" }, forceReload: true })
    try {
      const translated = translateRequest(baseRequest)

      expect(translated.reasoning).toEqual({ effort: "high" })
      expect(translated.include).toEqual(["reasoning.encrypted_content"])
    } finally {
      loadConfig({ env: {}, forceReload: true })
    }
  })

  it("lets Claude Code request effort win over the default effort", () => {
    loadConfig({ env: { CCP_CODEX_DEFAULT_EFFORT: "high" }, forceReload: true })
    try {
      const translated = translateRequest({
        ...baseRequest,
        output_config: { effort: "low" },
      })

      expect(translated.reasoning).toEqual({ effort: "low" })
    } finally {
      loadConfig({ env: {}, forceReload: true })
    }
  })

  it("returns only the expected top-level upstream request fields", () => {
    const translated = translateRequest({
      ...baseRequest,
      system: "follow instructions",
      tools: [
        {
          name: "lookup_weather",
          description: "Look up the weather",
          input_schema: {
            type: "object",
            properties: { city: { type: "string" } },
            required: ["city"],
          },
        },
      ],
      tool_choice: { type: "tool", name: "lookup_weather" },
      output_config: {
        effort: "high",
        format: {
          type: "json_schema",
          name: "weather_response",
          schema: {
            type: "object",
            properties: { forecast: { type: "string" } },
            required: ["forecast"],
          },
        },
      },
    })

    expect(Object.keys(translated).sort()).toEqual([
      "include",
      "input",
      "instructions",
      "model",
      "parallel_tool_calls",
      "reasoning",
      "store",
      "stream",
      "text",
      "tool_choice",
      "tools",
    ])
  })
})
