import { describe, expect, it } from "bun:test"
import type { AnthropicRequest } from "../../../anthropic/schema.ts"
import { countTokens } from "../count-tokens.ts"
import { translateRequest } from "./request.ts"

describe("translateRequest", () => {
  it("translates unsupported tool result content blocks as text parts", () => {
    const req: AnthropicRequest = {
      model: "kimi-k2",
      messages: [
        {
          role: "user",
          content: [
            {
              type: "tool_result",
              tool_use_id: "toolu_1",
              content: [
                { type: "text", text: "visible output" },
                { type: "thinking", thinking: "hidden thought" },
              ],
            },
          ],
        },
      ],
    }

    expect(translateRequest(req).messages).toEqual([
      {
        role: "tool",
        tool_call_id: "toolu_1",
        content: [
          { type: "text", text: "visible output" },
          { type: "text", text: "[unsupported content block omitted: thinking]" },
        ],
      },
    ])
  })

  it("preserves image tool result content parts", () => {
    const req: AnthropicRequest = {
      model: "kimi-k2",
      messages: [
        {
          role: "user",
          content: [
            {
              type: "tool_result",
              tool_use_id: "toolu_1",
              content: [
                { type: "text", text: "caption" },
                {
                  type: "image",
                  source: { type: "base64", media_type: "image/png", data: "abc" },
                },
              ],
            },
          ],
        },
      ],
    }

    expect(translateRequest(req).messages).toEqual([
      {
        role: "tool",
        tool_call_id: "toolu_1",
        content: [
          { type: "text", text: "caption" },
          { type: "image_url", image_url: { url: "data:image/png;base64,abc" } },
        ],
      },
    ])
  })

  it("counts unsupported tool result content blocks without throwing", () => {
    const req: AnthropicRequest = {
      model: "kimi-k2",
      messages: [
        {
          role: "user",
          content: [
            {
              type: "tool_result",
              tool_use_id: "toolu_1",
              content: [{ type: "thinking", thinking: "hidden thought" }],
            },
          ],
        },
      ],
    }

    expect(countTokens(req)).toBeGreaterThan(0)
  })
})
