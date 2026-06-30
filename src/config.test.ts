import { describe, expect, it, beforeEach, afterEach } from "bun:test";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  loadConfig,
  getConfig,
  configPath as resolveConfigPath,
  configOverrideSummaryLines,
  port,
  codexOriginator,
  codexUserAgent,
  codexModel,
  codexEffort,
  codexReasoningSummary,
  codexServiceTier,
  codexBaseUrl,
  codexTransport,
  codexPreviousResponseId,
  aliasProvider,
  kimiUserAgent,
  kimiOauthHost,
  kimiBaseUrl,
  cursorBaseUrl,
  cursorClientVersion,
  cursorAgentBundle,
  logVerbose,
  logStderr,
} from "./config.ts";

let dir: string;
let configPath: string;

function setEnv(env: NodeJS.ProcessEnv) {
  loadConfig({ configPath, env, forceReload: true });
}

function writeConfigSummaryFixture(overrides: { log?: { stderr?: boolean } } = {}) {
  writeFileSync(
    configPath,
    JSON.stringify({
      codex: {
        originator: "file-originator",
        userAgent: "file-codex-ua",
        model: "gpt-5.2",
        effort: "high",
        serviceTier: "flex",
        baseUrl: "https://codex-file.example.com",
        transport: "http",
        previousResponseId: false,
      },
      kimi: {
        userAgent: "file-kimi-ua",
        oauthHost: "https://kimi-auth-file.example.com",
        baseUrl: "https://kimi-file.example.com",
      },
      cursor: {
        baseUrl: "https://cursor-file.example.com",
        clientVersion: "cli-file",
        agentBundle: "/file/index.js",
      },
      aliasProvider: "kimi",
      log: {
        verbose: true,
        ...(overrides.log ? overrides.log : {}),
      },
    }),
  );
}

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "ccp-config-"));
  configPath = join(dir, "config.json");
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
  // Reset module-level cache to a clean process-env baseline so unrelated
  // tests that import config getters do not see leftover overrides.
  loadConfig({ forceReload: true });
});

describe("config defaults", () => {
  it("returns built-in defaults when no env and no file", () => {
    setEnv({});
    expect(port()).toBe(18765);
    expect(codexOriginator("default-orig")).toBe("default-orig");
    expect(codexUserAgent("default-ua")).toBe("default-ua");
    expect(codexModel()).toBeUndefined();
    expect(codexEffort()).toBeUndefined();
    expect(codexReasoningSummary()).toBeUndefined();
    expect(codexServiceTier()).toBeUndefined();
    expect(codexBaseUrl("default-codex-url")).toBe("default-codex-url");
    expect(codexTransport()).toBe("websocket");
    expect(codexPreviousResponseId()).toBe(false);
    expect(aliasProvider()).toBe("codex");
    expect(kimiUserAgent("default-kimi-ua")).toBe("default-kimi-ua");
    expect(kimiOauthHost()).toBe("https://auth.kimi.com");
    expect(kimiBaseUrl()).toBe("https://api.kimi.com/coding/v1");
    expect(cursorBaseUrl()).toBe("https://api2.cursor.sh");
    expect(cursorClientVersion()).toBe("cli-2026.06.04-5fd875e");
    expect(cursorAgentBundle()).toBeUndefined();
    expect(logVerbose()).toBe(false);
    expect(logStderr()).toBe(false);
  });
});

describe("file overrides default", () => {
  it("port from config.json", () => {
    writeFileSync(configPath, JSON.stringify({ port: 11111 }));
    setEnv({});
    expect(port()).toBe(11111);
  });

  it("codex.userAgent from config.json", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { userAgent: "ccp/file" } }));
    setEnv({});
    expect(codexUserAgent("default")).toBe("ccp/file");
  });

  it("codex.serviceTier from config.json", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { serviceTier: "fast" } }));
    setEnv({});
    expect(codexServiceTier()).toBe("fast");
  });

  it("codex.reasoningSummary from config.json", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { reasoningSummary: "off" } }));
    setEnv({});
    expect(codexReasoningSummary()).toBe("off");
  });

  it("codex.reasoningSummary env beats file and empty env falls through", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { reasoningSummary: "off" } }));
    setEnv({ CCP_CODEX_REASONING_SUMMARY: "auto" });
    expect(codexReasoningSummary()).toBe("auto");
    setEnv({ CCP_CODEX_REASONING_SUMMARY: "" });
    expect(codexReasoningSummary()).toBe("off");
  });

  it("codex.baseUrl from config.json", () => {
    writeFileSync(
      configPath,
      JSON.stringify({
        codex: { baseUrl: "http://127.0.0.1:2455/backend-api/codex/responses" },
      }),
    );
    setEnv({});
    expect(codexBaseUrl("default")).toBe("http://127.0.0.1:2455/backend-api/codex/responses");
  });

  it("codex transport and previous response id from config.json", () => {
    writeFileSync(
      configPath,
      JSON.stringify({ codex: { transport: "http", previousResponseId: true } }),
    );
    setEnv({});
    expect(codexTransport()).toBe("http");
    expect(codexPreviousResponseId()).toBe(true);
  });

  it("aliasProvider from config.json", () => {
    writeFileSync(configPath, JSON.stringify({ aliasProvider: "codex" }));
    setEnv({});
    expect(aliasProvider()).toBe("codex");
  });

  it("kimi.oauthHost from config.json", () => {
    writeFileSync(configPath, JSON.stringify({ kimi: { oauthHost: "https://auth.example.com" } }));
    setEnv({});
    expect(kimiOauthHost()).toBe("https://auth.example.com");
  });

  it("cursor config from config.json", () => {
    writeFileSync(
      configPath,
      JSON.stringify({
        cursor: {
          baseUrl: "https://cursor.example.com",
          clientVersion: "cli-test",
          agentBundle: "/tmp/cursor-agent/index.js",
        },
      }),
    );
    setEnv({});
    expect(cursorBaseUrl()).toBe("https://cursor.example.com");
    expect(cursorClientVersion()).toBe("cli-test");
    expect(cursorAgentBundle()).toBe("/tmp/cursor-agent/index.js");
  });

  it("log.verbose from config.json", () => {
    writeFileSync(configPath, JSON.stringify({ log: { verbose: true } }));
    setEnv({});
    expect(logVerbose()).toBe(true);
  });
});

describe("env overrides file", () => {
  it("PORT env wins over config port", () => {
    writeFileSync(configPath, JSON.stringify({ port: 11111 }));
    setEnv({ PORT: "22222" });
    expect(port()).toBe(22222);
  });

  it("CCP_CODEX_USER_AGENT env wins over config", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { userAgent: "from-file" } }));
    setEnv({ CCP_CODEX_USER_AGENT: "from-env" });
    expect(codexUserAgent("default")).toBe("from-env");
  });

  it("CCP_CODEX_SERVICE_TIER env wins over config", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { serviceTier: "flex" } }));
    setEnv({ CCP_CODEX_SERVICE_TIER: "fast" });
    expect(codexServiceTier()).toBe("fast");
  });

  it("CCP_CODEX_BASE_URL env wins over config", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { baseUrl: "http://127.0.0.1:2455/file" } }));
    setEnv({ CCP_CODEX_BASE_URL: "http://127.0.0.1:2455/env" });
    expect(codexBaseUrl("default")).toBe("http://127.0.0.1:2455/env");
  });

  it("Codex transport env wins over config", () => {
    writeFileSync(
      configPath,
      JSON.stringify({ codex: { transport: "http", previousResponseId: false } }),
    );
    setEnv({ CCP_CODEX_TRANSPORT: "auto", CCP_CODEX_PREVIOUS_RESPONSE_ID: "true" });
    expect(codexTransport()).toBe("auto");
    expect(codexPreviousResponseId()).toBe(true);
  });

  it("CCP_ALIAS_PROVIDER env wins over config", () => {
    writeFileSync(configPath, JSON.stringify({ aliasProvider: "kimi" }));
    setEnv({ CCP_ALIAS_PROVIDER: "codex" });
    expect(aliasProvider()).toBe("codex");
  });

  it("CCP_USER_AGENT env (generic fallback) is preferred over file", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { userAgent: "from-file" } }));
    setEnv({ CCP_USER_AGENT: "generic-env" });
    expect(codexUserAgent("default")).toBe("generic-env");
    expect(kimiUserAgent("default")).toBe("generic-env");
  });

  it("Cursor env vars win over config", () => {
    writeFileSync(
      configPath,
      JSON.stringify({
        cursor: {
          baseUrl: "https://cursor-file.example.com",
          clientVersion: "cli-file",
          agentBundle: "/file/index.js",
        },
      }),
    );
    setEnv({
      CCP_CURSOR_BASE_URL: "https://cursor-env.example.com",
      CCP_CURSOR_CLIENT_VERSION: "cli-env",
      CCP_CURSOR_AGENT_BUNDLE: "/env/index.js",
    });
    expect(cursorBaseUrl()).toBe("https://cursor-env.example.com");
    expect(cursorClientVersion()).toBe("cli-env");
    expect(cursorAgentBundle()).toBe("/env/index.js");
  });

  it("logStderr env-set forces true even when config sets false", () => {
    writeFileSync(configPath, JSON.stringify({ log: { stderr: false } }));
    setEnv({ CCP_LOG_STDERR: "1" });
    expect(logStderr()).toBe(true);
  });
});

describe("empty-string semantics", () => {
  it("empty CCP_CODEX_MODEL env falls through to file value", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { model: "gpt-5.2" } }));
    setEnv({ CCP_CODEX_MODEL: "" });
    expect(codexModel()).toBe("gpt-5.2");
  });

  it("empty CCP_CODEX_MODEL env with no file value returns undefined", () => {
    setEnv({ CCP_CODEX_MODEL: "" });
    expect(codexModel()).toBeUndefined();
  });

  it("empty Codex transport env falls through to file value", () => {
    writeFileSync(
      configPath,
      JSON.stringify({ codex: { transport: "http", previousResponseId: true } }),
    );
    setEnv({ CCP_CODEX_TRANSPORT: "", CCP_CODEX_PREVIOUS_RESPONSE_ID: "" });
    expect(codexTransport()).toBe("http");
    expect(codexPreviousResponseId()).toBe(true);
  });

  it("empty CCP_CODEX_SERVICE_TIER env falls through to file value", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { serviceTier: "flex" } }));
    setEnv({ CCP_CODEX_SERVICE_TIER: "" });
    expect(codexServiceTier()).toBe("flex");
  });

  it("empty CCP_ALIAS_PROVIDER env falls through to file value", () => {
    writeFileSync(configPath, JSON.stringify({ aliasProvider: "codex" }));
    setEnv({ CCP_ALIAS_PROVIDER: "" });
    expect(aliasProvider()).toBe("codex");
  });

  it("invalid CCP_ALIAS_PROVIDER env falls through to file value", () => {
    writeFileSync(configPath, JSON.stringify({ aliasProvider: "codex" }));
    setEnv({ CCP_ALIAS_PROVIDER: "openai" });
    expect(aliasProvider()).toBe("codex");
  });

  it("empty PORT env falls through to file value", () => {
    writeFileSync(configPath, JSON.stringify({ port: 33333 }));
    setEnv({ PORT: "" });
    expect(port()).toBe(33333);
  });
});

describe("empty env-string compatibility", () => {
  it("empty CCP_CODEX_USER_AGENT env is a valid value (legacy ?? semantics)", () => {
    setEnv({ CCP_CODEX_USER_AGENT: "" });
    expect(codexUserAgent("default-ua")).toBe("");
  });

  it("empty CCP_KIMI_OAUTH_HOST env is a valid value (legacy ?? semantics)", () => {
    setEnv({ CCP_KIMI_OAUTH_HOST: "" });
    expect(kimiOauthHost()).toBe("");
  });

  it("CCP_LOG_STDERR set to empty string still enables stderr (legacy !! semantics)", () => {
    setEnv({ CCP_LOG_STDERR: "" });
    expect(logStderr()).toBe(true);
  });
});

describe("malformed config", () => {
  it("returns defaults when JSON is invalid", () => {
    writeFileSync(configPath, "{not valid json");
    setEnv({});
    expect(port()).toBe(18765);
  });

  it("ignores wrong-typed values with a warning, keeps other valid ones", () => {
    writeFileSync(
      configPath,
      JSON.stringify({ port: "not-a-number", codex: { userAgent: "good" } }),
    );
    setEnv({});
    expect(port()).toBe(18765);
    expect(codexUserAgent("default")).toBe("good");
  });

  it("ignores invalid aliasProvider values", () => {
    writeFileSync(configPath, JSON.stringify({ aliasProvider: "openai" }));
    setEnv({});
    expect(aliasProvider()).toBe("codex");
  });

  it("ignores invalid Codex transport values", () => {
    writeFileSync(configPath, JSON.stringify({ codex: { transport: "websockets" } }));
    setEnv({});
    expect(codexTransport()).toBe("websocket");
  });

  it("returns defaults when file is missing entirely", () => {
    setEnv({});
    expect(port()).toBe(18765);
  });
});

describe("config summary", () => {
  it("resolves config.json under the configured directory", () => {
    expect(resolveConfigPath("/tmp/ccp-config")).toBe(join("/tmp/ccp-config", "config.json"));
  });

  it("returns no override lines for defaults", () => {
    setEnv({});
    expect(configOverrideSummaryLines(getConfig())).toEqual([]);
  });

  it("summarizes file override lines with existing labels", () => {
    writeConfigSummaryFixture({ log: { stderr: true } });
    setEnv({});

    expect(configOverrideSummaryLines(getConfig())).toEqual([
      "codex.originator (config)",
      "codex.userAgent (config)",
      "kimi.userAgent (config)",
      "codex.model (config)",
      "codex.effort (config)",
      "codex.serviceTier (config)",
      "codex.baseUrl (config)",
      "codex.transport=http (config)",
      "codex.previousResponseId=false (config)",
      "aliasProvider=kimi (config)",
      "log.verbose (config)",
      "log.stderr (config)",
      "kimi.oauthHost (config)",
      "kimi.baseUrl (config)",
      "cursor.baseUrl (config)",
      "cursor.clientVersion (config)",
      "cursor.agentBundle (config)",
    ]);
  });

  it("summarizes env override lines with current precedence and display values", () => {
    writeConfigSummaryFixture({ log: { stderr: false } });
    setEnv({
      CCP_CODEX_ORIGINATOR: "env-originator",
      CCP_CODEX_USER_AGENT: "env-codex-ua",
      CCP_KIMI_USER_AGENT: "env-kimi-ua",
      CCP_CODEX_MODEL: "gpt-5.5",
      CCP_CODEX_EFFORT: "medium",
      CCP_CODEX_REASONING_SUMMARY: "off",
      CCP_CODEX_SERVICE_TIER: "fast",
      CCP_CODEX_BASE_URL: "https://codex-env.example.com",
      CCP_CODEX_TRANSPORT: "auto",
      CCP_CODEX_PREVIOUS_RESPONSE_ID: "true",
      CCP_ALIAS_PROVIDER: "codex",
      CCP_LOG_VERBOSE: "0",
      CCP_LOG_STDERR: "",
      CCP_KIMI_OAUTH_HOST: "https://kimi-auth-env.example.com",
      CCP_KIMI_BASE_URL: "https://kimi-env.example.com",
      CCP_CURSOR_BASE_URL: "https://cursor-env.example.com",
      CCP_CURSOR_CLIENT_VERSION: "cli-env",
      CCP_CURSOR_AGENT_BUNDLE: "/env/index.js",
    });

    expect(configOverrideSummaryLines(getConfig())).toEqual([
      "CCP_CODEX_ORIGINATOR (env)",
      "CCP_CODEX_USER_AGENT (env)",
      "CCP_KIMI_USER_AGENT (env)",
      "CCP_CODEX_MODEL (env)",
      "CCP_CODEX_EFFORT (env)",
      "CCP_CODEX_REASONING_SUMMARY (env)",
      "CCP_CODEX_SERVICE_TIER (env)",
      "CCP_CODEX_BASE_URL (env)",
      "CCP_CODEX_TRANSPORT=auto (env)",
      "CCP_CODEX_PREVIOUS_RESPONSE_ID=true (env)",
      "CCP_ALIAS_PROVIDER=codex (env)",
      "CCP_LOG_VERBOSE (env)",
      "CCP_LOG_STDERR (env)",
      "CCP_KIMI_OAUTH_HOST (env)",
      "CCP_KIMI_BASE_URL (env)",
      "CCP_CURSOR_BASE_URL=https://cursor-env.example.com (env)",
      "CCP_CURSOR_CLIENT_VERSION=cli-env (env)",
      "CCP_CURSOR_AGENT_BUNDLE (env)",
    ]);
  });

  it("preserves current empty-string display semantics", () => {
    writeFileSync(
      configPath,
      JSON.stringify({
        codex: { model: "gpt-5.2", transport: "http", previousResponseId: true },
        aliasProvider: "kimi",
        cursor: { baseUrl: "https://cursor-file.example.com" },
      }),
    );
    setEnv({
      CCP_CODEX_MODEL: "",
      CCP_CODEX_TRANSPORT: "",
      CCP_CODEX_PREVIOUS_RESPONSE_ID: "",
      CCP_ALIAS_PROVIDER: "",
      CCP_LOG_STDERR: "",
      CCP_CURSOR_BASE_URL: "",
    });

    expect(configOverrideSummaryLines(getConfig())).toEqual([
      "codex.model (config)",
      "codex.transport=http (config)",
      "CCP_CODEX_PREVIOUS_RESPONSE_ID=true (env)",
      "aliasProvider=kimi (config)",
      "CCP_LOG_STDERR (env)",
      "cursor.baseUrl (config)",
    ]);
  });

  it("summarizes generic CCP_USER_AGENT fallback once in the existing position", () => {
    setEnv({ CCP_USER_AGENT: "generic-env" });

    expect(configOverrideSummaryLines(getConfig())).toEqual(["CCP_USER_AGENT (env)"]);
  });

  it("uses the supplied loaded config instead of the module cache", () => {
    setEnv({ CCP_CODEX_TRANSPORT: "http", CCP_CURSOR_CLIENT_VERSION: "cached" });

    const supplied = loadConfig({
      configPath,
      env: { CCP_CODEX_TRANSPORT: "auto", CCP_CURSOR_CLIENT_VERSION: "supplied" },
    });

    expect(configOverrideSummaryLines(supplied)).toEqual([
      "CCP_CODEX_TRANSPORT=auto (env)",
      "CCP_CURSOR_CLIENT_VERSION=supplied (env)",
    ]);
  });
});
