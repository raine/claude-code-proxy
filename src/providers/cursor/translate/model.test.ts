import { describe, expect, it } from "bun:test";
import { CURSOR_AGENT_MODEL_IDS } from "./catalog.ts";
import { CURSOR_SUPPORTED_MODELS, isCursorModel, resolveCursorModel } from "./model.ts";

describe("Cursor model selection", () => {
  it("maps cursor aliases to Cursor default", () => {
    expect(resolveCursorModel({ model: "cursor", metadata: undefined })).toEqual({
      mode: "AGENT_MODE_AGENT",
      requestedModel: { modelId: "default" },
    });
    expect(resolveCursorModel({ model: "cursor-agent", metadata: undefined })).toEqual({
      mode: "AGENT_MODE_AGENT",
      requestedModel: { modelId: "default" },
    });
  });

  it("maps composer aliases explicitly", () => {
    expect(resolveCursorModel({ model: "cursor-composer", metadata: undefined })).toEqual({
      mode: "AGENT_MODE_AGENT",
      requestedModel: {
        modelId: "composer-2.5",
        parameters: [{ id: "fast", value: "false" }],
      },
    });
    expect(resolveCursorModel({ model: "cursor-composer-fast", metadata: undefined })).toEqual({
      mode: "AGENT_MODE_AGENT",
      requestedModel: {
        modelId: "composer-2.5",
        parameters: [{ id: "fast", value: "true" }],
      },
    });
  });

  it("selects plan and ask modes from aliases", () => {
    expect(resolveCursorModel({ model: "cursor-plan", metadata: undefined })).toEqual({
      mode: "AGENT_MODE_PLAN",
      requestedModel: { modelId: "default" },
    });
    expect(resolveCursorModel({ model: "cursor-ask", metadata: undefined })).toEqual({
      mode: "AGENT_MODE_ASK",
      requestedModel: { modelId: "default" },
    });
  });

  it("supports raw cursor model prefix", () => {
    const selected = resolveCursorModel({
      model: "cursor:claude-sonnet-4-6-fast",
      metadata: { cursor_mode: "plan" },
    });

    expect(selected.mode).toBe("AGENT_MODE_PLAN");
    expect(selected.requestedModel).toEqual({
      modelId: "claude-sonnet-4-6",
      parameters: [{ id: "fast", value: "true" }],
    });

    expect(resolveCursorModel({
      model: "cursor:composer-2.5",
      metadata: undefined,
    }).requestedModel).toEqual({
      modelId: "composer-2.5",
      parameters: [{ id: "fast", value: "false" }],
    });
  });

  it("preserves exact Cursor catalog ids", () => {
    const selected = resolveCursorModel({
      model: "cursor:gpt-5.5-high-fast",
      metadata: undefined,
    });

    expect(selected.mode).toBe("AGENT_MODE_AGENT");
    expect(selected.requestedModel).toEqual({ modelId: "gpt-5.5-high-fast" });
  });

  it("maps Claude effort onto Cursor catalog effort variants", () => {
    expect(resolveCursorModel({
      model: "cursor:gpt-5.5",
      metadata: undefined,
      output_config: { effort: "low" },
    }).requestedModel).toEqual({ modelId: "gpt-5.5-low" });
    expect(resolveCursorModel({
      model: "cursor:gpt-5.5",
      metadata: undefined,
      output_config: { effort: "medium" },
    }).requestedModel).toEqual({ modelId: "gpt-5.5-medium" });
    expect(resolveCursorModel({
      model: "cursor:gpt-5.5",
      metadata: undefined,
      output_config: { effort: "high" },
    }).requestedModel).toEqual({ modelId: "gpt-5.5-high" });
    expect(resolveCursorModel({
      model: "cursor:gpt-5.5",
      metadata: undefined,
      output_config: { effort: "max" },
    }).requestedModel).toEqual({ modelId: "gpt-5.5-extra-high" });
  });

  it("preserves fast mode when applying Cursor effort variants", () => {
    expect(resolveCursorModel({
      model: "cursor:gpt-5.5-fast",
      metadata: undefined,
      output_config: { effort: "high" },
    }).requestedModel).toEqual({ modelId: "gpt-5.5-high-fast" });
  });

  it("maps max effort to Cursor xhigh when max is unavailable", () => {
    expect(resolveCursorModel({
      model: "cursor:gpt-5.2",
      metadata: undefined,
      output_config: { effort: "max" },
    }).requestedModel).toEqual({ modelId: "gpt-5.2-xhigh" });
  });

  it("applies effort independently of Cursor mode prefixes", () => {
    expect(resolveCursorModel({
      model: "cursor-plan:claude-opus-4-8",
      metadata: undefined,
      output_config: { effort: "max" },
    })).toEqual({
      mode: "AGENT_MODE_PLAN",
      requestedModel: { modelId: "claude-opus-4-8-max" },
    });
    expect(resolveCursorModel({
      model: "cursor-ask:claude-opus-4-8-thinking",
      metadata: undefined,
      output_config: { effort: "high" },
    })).toEqual({
      mode: "AGENT_MODE_ASK",
      requestedModel: { modelId: "claude-opus-4-8-thinking-high" },
    });
  });

  it("does not override explicit Cursor effort ids or models without effort variants", () => {
    expect(resolveCursorModel({
      model: "cursor:gpt-5.5-low",
      metadata: undefined,
      output_config: { effort: "max" },
    }).requestedModel).toEqual({ modelId: "gpt-5.5-low" });
    expect(resolveCursorModel({
      model: "cursor:gemini-3.1-pro",
      metadata: undefined,
      output_config: { effort: "high" },
    }).requestedModel).toEqual({ modelId: "gemini-3.1-pro" });
  });

  it("supports prefixed plan and ask variants for catalog ids", () => {
    expect(resolveCursorModel({ model: "cursor-plan:gpt-5.5-high", metadata: undefined })).toEqual({
      mode: "AGENT_MODE_PLAN",
      requestedModel: { modelId: "gpt-5.5-high" },
    });
    expect(resolveCursorModel({ model: "cursor-ask:claude-opus-4-8-thinking-high", metadata: undefined })).toEqual({
      mode: "AGENT_MODE_ASK",
      requestedModel: { modelId: "claude-opus-4-8-thinking-high" },
    });
  });

  it("advertises current Cursor Agent models with unambiguous prefixes", () => {
    expect(CURSOR_AGENT_MODEL_IDS.length).toBeGreaterThan(100);
    expect(CURSOR_SUPPORTED_MODELS.has("cursor:gpt-5.5-high")).toBe(true);
    expect(CURSOR_SUPPORTED_MODELS.has("cursor-plan:gpt-5.5-high")).toBe(true);
    expect(CURSOR_SUPPORTED_MODELS.has("cursor-ask:gpt-5.5-high")).toBe(true);
    expect(isCursorModel("cursor:kimi-k2.5")).toBe(true);
    expect(isCursorModel("gpt-5.2")).toBe(false);
  });
});
