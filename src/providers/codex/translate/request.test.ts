import { afterEach, describe, expect, it } from "bun:test";
import { loadConfig } from "../../../config.ts";
import type { AnthropicRequest } from "../../../anthropic/schema.ts";
import { countTokens } from "../count-tokens.ts";
import {
  InvalidServiceTierError,
  toWebSocketRequest,
  toolResultToString,
  translateRequest,
} from "./request.ts";
const baseRequest: AnthropicRequest = {
  model: "claude-sonnet-4-6",
  messages: [{ role: "user", content: "hello" }],
};

afterEach(() => {
  loadConfig({ forceReload: true });
});

describe("translateRequest", () => {
  it("omits reasoning include when reasoning is not enabled", () => {
    const translated = translateRequest(baseRequest);

    expect(translated.reasoning).toBeUndefined();
    expect(translated.include).toBeUndefined();
  });

  it("includes encrypted reasoning content when reasoning is enabled", () => {
    const translated = translateRequest({
      ...baseRequest,
      output_config: { effort: "medium" },
    });

    expect(translated.reasoning).toEqual({ effort: "medium", summary: "auto" });
    expect(translated.include).toEqual(["reasoning.encrypted_content"]);
  });

  for (const value of ["off", "none"]) {
    it(`disables the reasoning summary when CCP_CODEX_REASONING_SUMMARY=${value}`, () => {
      loadConfig({ env: { CCP_CODEX_REASONING_SUMMARY: value }, forceReload: true });
      const translated = translateRequest({
        ...baseRequest,
        output_config: { effort: "medium" },
      });

      expect(translated.reasoning).toEqual({ effort: "medium" });
      expect(translated.include).toEqual(["reasoning.encrypted_content"]);
    });
  }

  it("keeps the summary on (auto) for any non-disabling override value", () => {
    loadConfig({ env: { CCP_CODEX_REASONING_SUMMARY: "detailed" }, forceReload: true });
    const translated = translateRequest({
      ...baseRequest,
      output_config: { effort: "medium" },
    });

    expect(translated.reasoning).toEqual({ effort: "medium", summary: "auto" });
  });

  it("produces byte-identical reasoning across calls so continuation stays stable", () => {
    const req = { ...baseRequest, output_config: { effort: "high" as const } };
    const a = translateRequest(req);
    const b = translateRequest(req);

    expect(JSON.stringify(a.reasoning)).toBe(JSON.stringify(b.reasoning));
  });

  it("translates Anthropic web search to the Codex hosted web_search tool", () => {
    const translated = translateRequest({
      ...baseRequest,
      tools: [
        {
          type: "web_search_20250305",
          name: "web_search",
          allowed_domains: [],
          blocked_domains: [],
          max_uses: 8,
        },
      ],
    });

    expect(translated.tools).toEqual([
      {
        type: "web_search",
        external_web_access: false,
        search_content_types: ["text", "image"],
      },
    ]);
  });

  it("maps forced Anthropic web search choice to Codex hosted search choice", () => {
    const translated = translateRequest({
      ...baseRequest,
      tools: [
        {
          type: "web_search_20250305",
          name: "web_search",
        },
      ],
      tool_choice: { type: "tool", name: "web_search" },
    });

    expect(translated.tools).toEqual([
      {
        type: "web_search",
        external_web_access: false,
        search_content_types: ["text", "image"],
      },
    ]);
    expect(translated.tool_choice).toEqual({ type: "web_search" });
  });

  it("preserves non-empty Anthropic web search domain filters", () => {
    const translated = translateRequest({
      ...baseRequest,
      tools: [
        {
          type: "web_search_20250305",
          name: "web_search",
          allowed_domains: ["github.com"],
          blocked_domains: ["example.com"],
        },
      ],
    });

    expect(translated.tools).toEqual([
      {
        type: "web_search",
        external_web_access: false,
        search_content_types: ["text", "image"],
        filters: {
          allowed_domains: ["github.com"],
          blocked_domains: ["example.com"],
        },
      },
    ]);
  });

  it("normalizes fast service tier to upstream priority", () => {
    loadConfig({ env: { CCP_CODEX_SERVICE_TIER: "fast" }, forceReload: true });

    const translated = translateRequest(baseRequest);

    expect(translated.service_tier).toBe("priority");
  });

  it("passes flex service tier through", () => {
    loadConfig({ env: { CCP_CODEX_SERVICE_TIER: "flex" }, forceReload: true });

    const translated = translateRequest(baseRequest);

    expect(translated.service_tier).toBe("flex");
  });

  it("uses model service tier when no override is set", () => {
    loadConfig({ env: {}, forceReload: true });

    const translated = translateRequest(baseRequest, { serviceTier: "priority" });

    expect(translated.service_tier).toBe("priority");
  });

  it("service tier override takes precedence over model service tier", () => {
    loadConfig({ env: { CCP_CODEX_SERVICE_TIER: "flex" }, forceReload: true });

    const translated = translateRequest(baseRequest, { serviceTier: "priority" });

    expect(translated.service_tier).toBe("flex");
  });

  it("rejects invalid service tier overrides", () => {
    loadConfig({ env: { CCP_CODEX_SERVICE_TIER: "standard" }, forceReload: true });

    expect(() => translateRequest(baseRequest)).toThrow(InvalidServiceTierError);
    expect(() => translateRequest(baseRequest)).toThrow(
      'Invalid service tier override: "standard"',
    );
  });

  it("translates mid-conversation system messages as developer input", () => {
    const translated = translateRequest({
      ...baseRequest,
      messages: [
        { role: "user", content: "hello" },
        { role: "system", content: "remember this runtime constraint" },
      ],
    });

    expect(translated.input).toEqual([
      {
        type: "message",
        role: "user",
        content: [{ type: "input_text", text: "hello" }],
      },
      {
        type: "message",
        role: "developer",
        content: [{ type: "input_text", text: "remember this runtime constraint" }],
      },
    ]);
  });

  it("translates unsupported tool result content blocks without throwing", () => {
    const translated = translateRequest({
      ...baseRequest,
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
    });

    expect(translated.input).toEqual([
      {
        type: "function_call_output",
        call_id: "toolu_1",
        output: "visible output\n[unsupported content block omitted: thinking]",
      },
    ]);
  });

  it("preserves text and image tool result stringification", () => {
    expect(
      toolResultToString([
        { type: "text", text: "caption" },
        {
          type: "image",
          source: { type: "base64", media_type: "image/png", data: "abc" },
        },
        { type: "image", source: { type: "url", url: "https://example.invalid/a.png" } },
      ]),
    ).toBe("caption\n[image omitted: image/png]\n[image omitted: url]");
  });

  it("counts unsupported tool result content blocks without throwing", () => {
    expect(
      countTokens({
        ...baseRequest,
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
      }),
    ).toBeGreaterThan(0);
  });

  it("treats malformed tool result content blocks as unsupported", () => {
    expect(toolResultToString([{ type: "text" }, { type: "image" }])).toBe(
      "[unsupported content block omitted: text]\n[unsupported content block omitted: image]",
    );
  });

  it("requires every JSON schema property for strict responses", () => {
    const translated = translateRequest({
      ...baseRequest,
      output_config: {
        format: {
          type: "json_schema",
          schema: {
            type: "object",
            properties: {
              ok: { type: "boolean" },
              reason: { type: "string" },
              impossible: { type: "boolean" },
            },
            required: ["ok", "reason"],
            additionalProperties: false,
          },
        },
      },
    });

    expect(translated.text?.format).toMatchObject({
      type: "json_schema",
      schema: {
        required: ["ok", "reason", "impossible"],
      },
      strict: true,
    });
  });

  it("builds a websocket request without mutating the HTTP request", () => {
    const translated = translateRequest(baseRequest);
    const delta = [
      {
        type: "message" as const,
        role: "user" as const,
        content: [{ type: "input_text" as const, text: "next" }],
      },
    ];

    const ws = toWebSocketRequest(translated, {
      previousResponseId: "resp_1",
      input: delta,
      generate: false,
    });

    expect(ws.previous_response_id).toBe("resp_1");
    expect(ws.generate).toBe(false);
    expect(ws.input).toEqual(delta);
    expect(translated.input).not.toEqual(delta);
    expect("previous_response_id" in translated).toBe(false);
  });

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
    });

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
    ]);
  });
});
