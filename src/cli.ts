#!/usr/bin/env bun
import { startServer } from "./server.ts"
import { createLogger, logDir } from "./log.ts"
import { port as configPort, getConfig } from "./config.ts"
import { configDir } from "./paths.ts"
import { existsSync } from "node:fs"
import { join } from "node:path"
import {
  allProviders,
  allSupportedModels,
  getProvider,
  listProviders,
} from "./providers/registry.ts"
import type { CliHandlers } from "./providers/types.ts"

declare const BUILD_VERSION: string | undefined
const VERSION = typeof BUILD_VERSION === "string" ? BUILD_VERSION : "dev"

const log = createLogger("cli")

async function main() {
  const args = process.argv.slice(2)
  const [first, ...rest] = args

  if (first === "--version" || first === "-v" || first === "version") {
    console.log(`claude-code-proxy ${VERSION}`)
    return
  }

  if (!first || first === "serve") {
    const port = configPort()
    startServer({ port })
    console.log(`Proxy listening on http://localhost:${port}`)
    console.log(`Logs: ${logDir()}/proxy.log`)
    printConfigSummary()
    console.log()
    console.log("Providers are selected per-request by ANTHROPIC_MODEL:")
    for (const p of allProviders()) {
      const models = [...p.supportedModels].join(", ")
      console.log(`  ${p.name}: ${models}`)
    }
    console.log()
    console.log("Configure Claude Code (pick a model from above):")
    console.log(`  export ANTHROPIC_BASE_URL="http://localhost:${port}"`)
    console.log(`  export ANTHROPIC_AUTH_TOKEN="anything"`)
    console.log(`  export ANTHROPIC_MODEL="kimi-for-coding[1m]"              # or gpt-5.4[1m], etc.`)
    console.log(`  export ANTHROPIC_SMALL_FAST_MODEL="kimi-for-coding[1m]"   # background / title-gen`)
    console.log(`  export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1"`)
    return
  }

  if (listProviders().includes(first)) {
    const provider = getProvider(first)
    await runProviderCommand(provider.name, provider.cli, rest)
    return
  }

  usageAndExit()
}

async function runProviderCommand(name: string, cli: CliHandlers, args: string[]): Promise<void> {
  const [group, sub] = args
  if (group !== "auth") usageAndExit()

  switch (sub) {
    case "login":
      if (!cli.login) {
        console.error(`${name}: browser login not supported`)
        process.exit(2)
      }
      await cli.login()
      process.exit(0)
    case "device":
      if (!cli.device) {
        console.error(`${name}: device login not supported`)
        process.exit(2)
      }
      await cli.device()
      process.exit(0)
    case "status":
      await cli.status()
      return
    case "logout":
      await cli.logout()
      return
    default:
      usageAndExit()
  }
}

function usageAndExit(): never {
  const providers = listProviders().join("|")
  const models = allSupportedModels()
    .map((m) => `${m.model} (${m.provider})`)
    .join(", ")
  console.log(`Usage:
  claude-code-proxy serve                      Run proxy (PORT env or config.json port, default 18765)
                                               Upstream is chosen per-request from ANTHROPIC_MODEL.
  claude-code-proxy <provider> auth login      Browser OAuth
  claude-code-proxy <provider> auth device     Device-code OAuth
  claude-code-proxy <provider> auth status     Show current auth
  claude-code-proxy <provider> auth logout     Clear stored auth
  claude-code-proxy --version                  Show version

Providers: ${providers}
Models:    ${models}
`)
  process.exit(2)
}

function printConfigSummary(): void {
  const cfg = getConfig()
  const fromFile = cfg.file
  const overrides: string[] = []

  const configPath = join(configDir(), "config.json")
  if (existsSync(configPath)) {
    console.log(`Config: ${configPath}`)
  }

  if (cfg.env.CCP_CODEX_ORIGINATOR) overrides.push("CCP_CODEX_ORIGINATOR (env)")
  else if (fromFile.codex?.originator) overrides.push("codex.originator (config)")

  if (cfg.env.CCP_CODEX_USER_AGENT) overrides.push("CCP_CODEX_USER_AGENT (env)")
  else if (cfg.env.CCP_USER_AGENT) overrides.push("CCP_USER_AGENT (env)")
  else if (fromFile.codex?.userAgent) overrides.push("codex.userAgent (config)")

  if (cfg.env.CCP_KIMI_USER_AGENT) overrides.push("CCP_KIMI_USER_AGENT (env)")
  else if (fromFile.kimi?.userAgent) overrides.push("kimi.userAgent (config)")

  if (cfg.env.CCP_CLAUDE_ALIAS_PROVIDER) overrides.push("CCP_CLAUDE_ALIAS_PROVIDER (env)")
  else if (fromFile.routing?.claudeAliasProvider) overrides.push("routing.claudeAliasProvider (config)")

  if (cfg.env.CCP_CODEX_MODEL) overrides.push("CCP_CODEX_MODEL (env)")
  else if (fromFile.codex?.model) overrides.push("codex.model (config)")

  if (cfg.env.CCP_CODEX_EFFORT) overrides.push("CCP_CODEX_EFFORT (env)")
  else if (fromFile.codex?.effort) overrides.push("codex.effort (config)")

  if (cfg.env.CCP_GEMINI_MODEL) overrides.push("CCP_GEMINI_MODEL (env)")
  else if (fromFile.gemini?.model) overrides.push("gemini.model (config)")

  if (cfg.env.CCP_GEMINI_SMALL_FAST_MODEL) overrides.push("CCP_GEMINI_SMALL_FAST_MODEL (env)")
  else if (fromFile.gemini?.smallFastModel) overrides.push("gemini.smallFastModel (config)")

  if (cfg.env.CCP_GEMINI_OAUTH_CREDS_PATH) overrides.push("CCP_GEMINI_OAUTH_CREDS_PATH (env)")
  else if (fromFile.gemini?.oauthCredsPath) overrides.push("gemini.oauthCredsPath (config)")

  if (cfg.env.CCP_GEMINI_ENABLE_FALLBACK) overrides.push("CCP_GEMINI_ENABLE_FALLBACK (env)")
  else if (fromFile.gemini?.enableFallback !== undefined) overrides.push("gemini.enableFallback (config)")

  if (cfg.env.CCP_GEMINI_ENABLE_GOOGLE_ONE_CREDITS) {
    overrides.push("CCP_GEMINI_ENABLE_GOOGLE_ONE_CREDITS (env)")
  } else if (fromFile.gemini?.enableGoogleOneCredits !== undefined) {
    overrides.push("gemini.enableGoogleOneCredits (config)")
  }

  if (cfg.env.CCP_GEMINI_DEFAULT_EFFORT) overrides.push("CCP_GEMINI_DEFAULT_EFFORT (env)")
  else if (fromFile.gemini?.defaultEffort) overrides.push("gemini.defaultEffort (config)")

  if (cfg.env.CCP_GEMINI_CODE_ASSIST_ENDPOINT) overrides.push("CCP_GEMINI_CODE_ASSIST_ENDPOINT (env)")
  else if (fromFile.gemini?.endpoint) overrides.push("gemini.endpoint (config)")

  if (cfg.env.CCP_GEMINI_CODE_ASSIST_API_VERSION) overrides.push("CCP_GEMINI_CODE_ASSIST_API_VERSION (env)")
  else if (fromFile.gemini?.apiVersion) overrides.push("gemini.apiVersion (config)")

  if (cfg.env.CCP_LOG_VERBOSE !== undefined) overrides.push("CCP_LOG_VERBOSE (env)")
  else if (fromFile.log?.verbose) overrides.push("log.verbose (config)")

  if (cfg.env.CCP_LOG_STDERR !== undefined) overrides.push("CCP_LOG_STDERR (env)")
  else if (fromFile.log?.stderr) overrides.push("log.stderr (config)")

  if (cfg.env.CCP_KIMI_OAUTH_HOST) overrides.push("CCP_KIMI_OAUTH_HOST (env)")
  else if (fromFile.kimi?.oauthHost) overrides.push("kimi.oauthHost (config)")

  if (cfg.env.CCP_KIMI_BASE_URL) overrides.push("CCP_KIMI_BASE_URL (env)")
  else if (fromFile.kimi?.baseUrl) overrides.push("kimi.baseUrl (config)")

  if (overrides.length > 0) {
    console.log("Overrides:")
    for (const o of overrides) {
      console.log(`  ${o}`)
    }
  }
}

main().catch((err) => {
  log.error("cli fatal", { err: String(err), stack: (err as Error)?.stack })
  console.error(err)
  process.exit(1)
})
