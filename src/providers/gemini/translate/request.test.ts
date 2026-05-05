import { afterEach, describe, expect, it } from "bun:test"
import { loadConfig } from "../../../config.ts"
import { toCodeAssistGenerateRequest, translateRequest } from "./request.ts"
import type { AnthropicRequest } from "../../../anthropic/schema.ts"

afterEach(() => {
  loadConfig({ forceReload: true })
})

describe("translateRequest", () => {
  it("maps Claude messages, tools, and tool results to Gemini contents", () => {
    const req: AnthropicRequest = {
      model: "gemini-3.1-pro-preview",
      system: "Be concise.",
      messages: [
        { role: "user", content: "Read the file." },
        {
          role: "assistant",
          content: [
            { type: "tool_use", id: "toolu_1", name: "Read", input: { file_path: "a.ts" } },
          ],
        },
        {
          role: "user",
          content: [{ type: "tool_result", tool_use_id: "toolu_1", content: "hello" }],
        },
      ],
      tools: [{ name: "Read", input_schema: { type: "object" } }],
      max_tokens: 1000,
      stream: true,
    }

    const translated = translateRequest(req)

    expect(translated.config?.systemInstruction?.parts[0]).toEqual({ text: "Be concise." })
    expect(translated.config?.tools?.[0]?.functionDeclarations[0]).toMatchObject({
      name: "Read",
      parametersJsonSchema: { type: "object" },
    })
    expect(translated.contents[1]?.parts[0]).toEqual({
      functionCall: { id: "toolu_1", name: "Read", args: { file_path: "a.ts" } },
    })
    expect(translated.contents[2]?.parts[0]).toEqual({
      functionResponse: {
        id: "toolu_1",
        name: "Read",
        response: { output: "hello" },
      },
    })
  })

  it("maps max effort to Gemini 3 high thinking level", () => {
    const req: AnthropicRequest = {
      model: "gemini-3.1-pro-preview",
      messages: [{ role: "user", content: "think" }],
      output_config: { effort: "max" },
    }

    const translated = translateRequest(req)

    expect(translated.config?.generationConfig?.thinkingConfig).toEqual({
      includeThoughts: true,
      thinkingLevel: "HIGH",
    })
  })

  it("uses Gemini default effort when request effort is absent", () => {
    loadConfig({
      env: { CCP_GEMINI_DEFAULT_EFFORT: "medium" },
      configPath: "/tmp/claude-code-proxy-gemini-request-test-does-not-exist.json",
      forceReload: true,
    })
    const req: AnthropicRequest = {
      model: "gemini-3.1-pro-preview",
      messages: [{ role: "user", content: "think" }],
    }

    const translated = translateRequest(req)

    expect(translated.config?.generationConfig?.thinkingConfig).toEqual({
      includeThoughts: true,
      thinkingLevel: "MEDIUM",
    })
  })

  it("sets Code Assist enabled credit types only when provided", () => {
    const translated = translateRequest({
      model: "gemini-3.1-pro-preview",
      messages: [{ role: "user", content: "hello" }],
    })

    expect(
      toCodeAssistGenerateRequest(translated, {
        project: "project",
        userPromptId: "prompt",
        enabledCreditTypes: ["GOOGLE_ONE_AI"],
      }).enabled_credit_types,
    ).toEqual(["GOOGLE_ONE_AI"])

    expect(
      toCodeAssistGenerateRequest(translated, {
        project: "project",
        userPromptId: "prompt",
      }).enabled_credit_types,
    ).toBeUndefined()
  })
})
