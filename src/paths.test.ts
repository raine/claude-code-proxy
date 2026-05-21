import { describe, expect, it } from "bun:test"
import {
  codexAuthFile,
  kimiAuthFile,
  kimiDeviceIdFile,
  legacyConfigDir,
  resolveConfigDir,
  resolveStateDir,
} from "./paths.ts"

describe("resolveConfigDir", () => {
  it("uses ~/.config on darwin even when XDG_CONFIG_HOME is set", () => {
    expect(
      resolveConfigDir({ platform: "darwin", env: { XDG_CONFIG_HOME: "/x" }, home: "/home/u" }),
    ).toBe("/home/u/.config/claude-code-proxy")
  })

  it("honors XDG_CONFIG_HOME on linux", () => {
    expect(
      resolveConfigDir({ platform: "linux", env: { XDG_CONFIG_HOME: "/x" }, home: "/home/u" }),
    ).toBe("/x/claude-code-proxy")
  })

  it("falls back to $HOME/.config on linux without XDG_CONFIG_HOME", () => {
    expect(resolveConfigDir({ platform: "linux", env: {}, home: "/home/u" })).toBe(
      "/home/u/.config/claude-code-proxy",
    )
  })

  it("uses APPDATA on windows", () => {
    expect(
      resolveConfigDir({
        platform: "win32",
        env: { APPDATA: "C:\\Users\\u\\AppData\\Roaming" },
        home: "C:\\Users\\u",
      }),
    ).toBe("C:\\Users\\u\\AppData\\Roaming\\claude-code-proxy")
  })

  it("falls back to $HOME/AppData/Roaming on windows without APPDATA", () => {
    expect(resolveConfigDir({ platform: "win32", env: {}, home: "C:\\Users\\u" })).toBe(
      "C:\\Users\\u\\AppData\\Roaming\\claude-code-proxy",
    )
  })
})

describe("resolveStateDir", () => {
  it("honors XDG_STATE_HOME on darwin (preserves pre-existing log.ts behavior)", () => {
    expect(
      resolveStateDir({ platform: "darwin", env: { XDG_STATE_HOME: "/x" }, home: "/home/u" }),
    ).toBe("/x/claude-code-proxy")
  })

  it("falls back to $HOME/.local/state on darwin without XDG_STATE_HOME", () => {
    expect(resolveStateDir({ platform: "darwin", env: {}, home: "/home/u" })).toBe(
      "/home/u/.local/state/claude-code-proxy",
    )
  })

  it("honors XDG_STATE_HOME on linux", () => {
    expect(
      resolveStateDir({ platform: "linux", env: { XDG_STATE_HOME: "/x" }, home: "/home/u" }),
    ).toBe("/x/claude-code-proxy")
  })

  it("uses LOCALAPPDATA on windows", () => {
    expect(
      resolveStateDir({
        platform: "win32",
        env: { LOCALAPPDATA: "C:\\Users\\u\\AppData\\Local" },
        home: "C:\\Users\\u",
      }),
    ).toBe("C:\\Users\\u\\AppData\\Local\\claude-code-proxy")
  })

  it("falls back to $HOME/AppData/Local on windows without LOCALAPPDATA", () => {
    expect(
      resolveStateDir({
        platform: "win32",
        env: { APPDATA: "C:\\Users\\u\\AppData\\Roaming" },
        home: "C:\\Users\\u",
      }),
    ).toBe("C:\\Users\\u\\AppData\\Local\\claude-code-proxy")
  })
})

describe("provider paths", () => {
  it("resolves provider files under the windows config directory", () => {
    const deps = {
      platform: "win32" as const,
      env: { APPDATA: "C:\\Users\\u\\AppData\\Roaming" },
      home: "C:\\Users\\u",
    }
    expect(codexAuthFile(deps)).toBe(
      "C:\\Users\\u\\AppData\\Roaming\\claude-code-proxy\\codex\\auth.json",
    )
    expect(kimiAuthFile(deps)).toBe(
      "C:\\Users\\u\\AppData\\Roaming\\claude-code-proxy\\kimi\\auth.json",
    )
    expect(kimiDeviceIdFile(deps)).toBe(
      "C:\\Users\\u\\AppData\\Roaming\\claude-code-proxy\\kimi\\device_id",
    )
  })

  it("keeps the legacy config directory independent of platform", () => {
    expect(legacyConfigDir({ platform: "win32", env: {}, home: "C:\\Users\\u" })).toBe(
      "C:\\Users\\u\\.config\\claude-code-proxy",
    )
  })
})
