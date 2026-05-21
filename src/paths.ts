import { homedir } from "node:os"
import { posix, win32 } from "node:path"

export interface DirResolverEnv {
  platform: NodeJS.Platform
  env: NodeJS.ProcessEnv
  home: string
}

function defaults(): DirResolverEnv {
  return { platform: process.platform, env: process.env, home: homedir() }
}

function pathFor(platform: NodeJS.Platform) {
  return platform === "win32" ? win32 : posix
}

function windowsRoamingAppData(deps: DirResolverEnv): string {
  return deps.env.APPDATA || win32.join(deps.home, "AppData", "Roaming")
}

function windowsLocalAppData(deps: DirResolverEnv): string {
  return deps.env.LOCALAPPDATA || win32.join(deps.home, "AppData", "Local")
}

// macOS deliberately uses ~/.config/<app> rather than honoring XDG_CONFIG_HOME
// (which would redirect to ~/Library/Application Support). This matches where
// auth tokens have always been stored on macOS.
export function resolveConfigDir(deps: DirResolverEnv): string {
  const path = pathFor(deps.platform)
  if (deps.platform === "win32") {
    return path.join(windowsRoamingAppData(deps), "claude-code-proxy")
  }
  if (deps.platform === "darwin") {
    return path.join(deps.home, ".config", "claude-code-proxy")
  }
  const base = deps.env.XDG_CONFIG_HOME || path.join(deps.home, ".config")
  return path.join(base, "claude-code-proxy")
}

// XDG_STATE_HOME is honored on macOS and Unix platforms — that's the
// pre-config.json behavior of log.ts and is documented in the README.
export function resolveStateDir(deps: DirResolverEnv): string {
  const path = pathFor(deps.platform)
  if (deps.platform === "win32") {
    return path.join(windowsLocalAppData(deps), "claude-code-proxy")
  }
  const base = deps.env.XDG_STATE_HOME || path.join(deps.home, ".local", "state")
  return path.join(base, "claude-code-proxy")
}

// Legacy (pre-config.json) auth/device-id path. Always ~/.config regardless
// of XDG_CONFIG_HOME — this is the directory token stores hardcoded before
// configDir() existed. Used as a read-only fallback so existing logins keep
// working after upgrade.
export function legacyConfigDir(deps: DirResolverEnv = defaults()): string {
  return pathFor(deps.platform).join(deps.home, ".config", "claude-code-proxy")
}

export function codexAuthFile(deps: DirResolverEnv = defaults()): string {
  return pathFor(deps.platform).join(resolveConfigDir(deps), "codex", "auth.json")
}

export function kimiAuthFile(deps: DirResolverEnv = defaults()): string {
  return pathFor(deps.platform).join(resolveConfigDir(deps), "kimi", "auth.json")
}

export function kimiDeviceIdFile(deps: DirResolverEnv = defaults()): string {
  return pathFor(deps.platform).join(resolveConfigDir(deps), "kimi", "device_id")
}

export function logFile(deps: DirResolverEnv = defaults()): string {
  return pathFor(deps.platform).join(resolveStateDir(deps), "proxy.log")
}

export function configDir(): string {
  return resolveConfigDir(defaults())
}

export function stateDir(): string {
  return resolveStateDir(defaults())
}
