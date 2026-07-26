use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::env;
use std::io::Read;
use tempfile::TempDir;

#[test]
fn version_aliases_print_expected_version() -> Result<(), Box<dyn std::error::Error>> {
    let expected = format!("claude-code-proxy {}", env!("CARGO_PKG_VERSION"));

    for arg in ["--version", "-v", "version"] {
        let mut cmd = Command::cargo_bin("claude-code-proxy")?;
        cmd.arg(arg)
            .assert()
            .success()
            .stdout(contains(expected.clone()));
    }
    Ok(())
}

#[test]
fn version_json_is_machine_readable() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    let output = cmd.args(["version", "--json"]).output()?;
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert!(value["binarySha256"].as_str().is_some());
    Ok(())
}

#[cfg(unix)]
#[test]
fn version_json_ignores_unrelated_non_utf8_environment_values()
-> Result<(), Box<dyn std::error::Error>> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    let output = cmd
        .args(["version", "--json"])
        .env("CCP_NON_UTF8_TEST", OsString::from_vec(vec![0xff, 0xfe]))
        .output()?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    Ok(())
}

#[test]
fn models_prints_only_active_route_providers() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.arg("models");
    let out = String::from_utf8(cmd.output()?.stdout)?;
    assert!(out.contains("codex:"));
    assert!(out.contains("grok:"));
    assert_eq!(out.lines().count(), 2);
    Ok(())
}

#[test]
fn help_discovers_serverless_tui_demo() -> Result<(), Box<dyn std::error::Error>> {
    Command::cargo_bin("claude-code-proxy")?
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("demo"))
        .stdout(contains("mock data and no proxy server"));
    Ok(())
}

#[test]
fn help_discovers_metadata_only_diagnostics() -> Result<(), Box<dyn std::error::Error>> {
    Command::cargo_bin("claude-code-proxy")?
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("diagnostics"))
        .stdout(contains("metadata-only"));

    Command::cargo_bin("claude-code-proxy")?
        .args(["diagnostics", "collect", "--help"])
        .assert()
        .success()
        .stdout(contains("--last-lines"))
        .stdout(contains("--request-id"))
        .stdout(contains("--machine-label"))
        .stdout(contains("traffic").not());
    Ok(())
}

#[test]
fn diagnostics_collect_writes_only_fixed_metadata_members() -> Result<(), Box<dyn std::error::Error>>
{
    let temp = TempDir::new()?;
    let state_dir = temp.path().join("state");
    std::fs::create_dir_all(&state_dir)?;
    let output = temp.path().join("bundle.tar.gz");
    let secret = "PROMPT_AND_TOOL_ARGUMENT_MUST_NOT_ESCAPE";
    std::fs::write(
        state_dir.join("proxy.log"),
        format!(
            concat!(
                "{{\"t\":\"2026-07-22T10:00:00Z\",\"level\":\"info\",",
                "\"service\":\"server\",\"msg\":\"request_completed\",",
                "\"fields\":{{\"reqId\":\"req-safe\",\"provider\":\"codex\",",
                "\"model\":\"gpt-5.6-sol\",\"status\":200,\"ms\":42,",
                "\"prompt\":\"{}\"}}}}\n"
            ),
            secret
        ),
    )?;

    Command::cargo_bin("claude-code-proxy")?
        .args([
            "diagnostics",
            "collect",
            "--state-dir",
            state_dir.to_str().ok_or("non-UTF-8 state path")?,
            "--output",
            output.to_str().ok_or("non-UTF-8 output path")?,
            "--request-id",
            "req-safe",
            "--machine-label",
            "wsl-test",
            "--last-lines",
            "100",
        ])
        .assert()
        .success()
        .stdout(contains("metadata-only"));

    let archive_file = std::fs::File::open(&output)?;
    let decoder = flate2::read::GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    let mut members = Vec::new();
    let mut contents = String::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        members.push(entry.path()?.to_string_lossy().into_owned());
        entry.read_to_string(&mut contents)?;
    }
    members.sort();
    assert_eq!(
        members,
        ["environment.json", "events.jsonl", "manifest.json"]
    );
    assert!(contents.contains("reqIdHash"));
    assert!(!contents.contains("req-safe"));
    assert!(contents.contains("wsl-test"));
    assert!(!contents.contains(secret));
    Ok(())
}

#[test]
fn diagnostics_upload_requires_yes_when_non_interactive() -> Result<(), Box<dyn std::error::Error>>
{
    let temp = TempDir::new()?;
    let bundle = temp.path().join("bundle.tar.gz");
    std::fs::write(&bundle, b"not inspected after confirmation guard")?;

    Command::cargo_bin("claude-code-proxy")?
        .args([
            "diagnostics",
            "upload",
            bundle.to_str().ok_or("non-UTF-8 bundle path")?,
            "--host",
            "ccproxy-dev",
            "--remote-dir",
            "/srv/ccproxy-diagnostics",
        ])
        .assert()
        .failure()
        .stderr(contains("non-interactive upload without --yes"));
    Ok(())
}

#[test]
fn serve_help_documents_remote_bind_acknowledgement() -> Result<(), Box<dyn std::error::Error>> {
    Command::cargo_bin("claude-code-proxy")?
        .args(["serve", "--help"])
        .assert()
        .success()
        .stdout(contains("--allow-remote-unauthenticated"))
        .stdout(contains("remote clients are not authenticated"))
        .stdout(contains("authenticating reverse proxy"));
    Ok(())
}

#[test]
fn serve_rejects_non_loopback_without_acknowledgement() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    Command::cargo_bin("claude-code-proxy")?
        .args(["serve", "--no-monitor"])
        .env("CCP_CONFIG_DIR", temp.path())
        .env("CCP_BIND_ADDRESS", "0.0.0.0")
        .env("PORT", "0")
        .env_remove("CCP_ALLOW_REMOTE_UNAUTHENTICATED")
        .assert()
        .failure()
        .stderr(contains(
            "refusing unauthenticated non-loopback bind address",
        ));
    Ok(())
}

#[test]
fn invalid_command_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    Command::cargo_bin("claude-code-proxy")?
        .arg("definitely-not-a-command")
        .assert()
        .failure()
        .code(2);
    Ok(())
}

#[test]
fn provider_logout_without_auth_is_success() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.args(["grok", "auth", "logout"]);
    cmd.env("CCP_CONFIG_DIR", temp.path());
    cmd.assert().success();
    Ok(())
}

#[test]
fn models_output_is_stable_order() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.arg("models");
    let output = cmd.output()?;
    let out = String::from_utf8(output.stdout)?;
    let codex_pos = out.find("codex:").expect("codex models");
    let grok_pos = out.find("grok:").expect("grok models");
    assert!(codex_pos < grok_pos);
    Ok(())
}

#[cfg(unix)]
#[test]
fn co_execs_claude_with_gpt_profile_and_forwards_arguments()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = ClaudeLauncherFixture::new("co")?;
    let expected_config_dir = fixture.home_dir.join(".claude-ccproxy/gpt");
    let mut cmd = Command::new(&fixture.shortcut);
    cmd.args(["--effort", "max", "hello world"])
        .env("PATH", fixture.path_env())
        .env("HOME", &fixture.home_dir)
        .env("PORT", "19876")
        .env("CCP_BIND_ADDRESS", "0.0.0.0")
        .env(
            "ANTHROPIC_CUSTOM_HEADERS",
            "x-existing: keep\nX-CCPROXY-CODEX-XHIGH-AS-MAX: 0",
        );

    cmd.assert()
        .success()
        .stdout(contains("base=http://127.0.0.1:19876"))
        .stdout(contains(format!(
            "config_dir={}",
            expected_config_dir.display()
        )))
        .stdout(contains("profile_managed=1"))
        .stdout(contains("main=gpt-5.6-sol"))
        .stdout(contains("fable=gpt-5.6-sol"))
        .stdout(contains("sonnet=gpt-5.6-terra"))
        .stdout(contains("haiku=gpt-5.6-luna"))
        .stdout(contains("small=gpt-5.6-luna"))
        .stdout(contains("max_context=272000"))
        .stdout(contains("compact_window=272000"))
        .stdout(contains("compact_pct=90"))
        .stdout(contains("disable_1m=1"))
        .stdout(contains("max_retries=1"))
        .stdout(contains("tool_concurrency=10"))
        .stdout(contains("tool_search=true"))
        .stdout(contains("arg=<--settings>"))
        .stdout(contains("arg=<--agents>"))
        .stdout(contains(
            "\"Explore\":{\"description\":\"Fast, focused, read-only codebase exploration and search.",
        ))
        .stdout(contains(
            "\"effort\":\"medium\",\"model\":\"gpt-5.6-luna\"",
        ))
        .stdout(contains("\"claude\":").not())
        .stdout(contains(
            "\"general-purpose\":{\"description\":\"General-purpose agent",
        ))
        .stdout(contains(
            "\"Plan\":{\"description\":\"Read-only planning and research agent.",
        ))
        .stdout(contains(
            "\"permissionMode\":\"plan\",\"prompt\":\"You are a focused codebase exploration agent.",
        ))
        .stdout(contains(
            "\"tools\":[\"Read\",\"Glob\",\"Grep\"]",
        ))
        .stdout(contains("\"effortLevel\":\"high\""))
        .stdout(contains("\"model\":\"gpt-5.6-sol\""))
        .stdout(contains("\"ultracode\":false"))
        .stdout(contains("arg=<--effort>"))
        .stdout(contains("arg=<max>"))
        .stdout(contains("arg=<hello world>"))
        .stdout(contains("custom_headers=x-existing: keep"))
        .stdout(contains("x-ccproxy-codex-xhigh-as-max: 1"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn cg_execs_claude_with_grok_profile_and_preserves_exit_code()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = ClaudeLauncherFixture::new("cg")?;
    let expected_config_dir = fixture.home_dir.join(".claude-ccproxy/grok");
    let mut cmd = Command::new(&fixture.shortcut);
    cmd.args(["--continue"])
        .env("PATH", fixture.path_env())
        .env("HOME", &fixture.home_dir)
        .env("PORT", "19877")
        .env("FAKE_CLAUDE_EXIT_CODE", "37");

    cmd.assert()
        .failure()
        .code(37)
        .stdout(contains("base=http://127.0.0.1:19877"))
        .stdout(contains(format!(
            "config_dir={}",
            expected_config_dir.display()
        )))
        .stdout(contains("profile_managed=1"))
        .stdout(contains("main=grok-4.5\n"))
        .stdout(contains("fable=grok-4.5\n"))
        .stdout(contains("opus=grok-4.5-high"))
        .stdout(contains("sonnet=grok-4.5-high"))
        .stdout(contains("haiku=grok-4.5-medium"))
        .stdout(contains("small=grok-4.5-medium"))
        .stdout(contains("max_context=500000"))
        .stdout(contains("compact_window=500000"))
        .stdout(contains("compact_pct=90"))
        .stdout(contains("disable_1m=1"))
        .stdout(contains("max_retries=1"))
        .stdout(contains("tool_concurrency=10"))
        .stdout(contains("tool_search=true"))
        .stdout(contains("arg=<--settings>"))
        .stdout(contains("arg=<--agents>"))
        .stdout(contains(
            "\"Explore\":{\"description\":\"Fast, focused, read-only codebase exploration and search.",
        ))
        .stdout(contains(
            "\"effort\":\"medium\",\"model\":\"grok-4.5-medium\"",
        ))
        .stdout(contains("\"claude\":").not())
        .stdout(contains(
            "\"general-purpose\":{\"description\":\"General-purpose agent",
        ))
        .stdout(contains(
            "\"Plan\":{\"description\":\"Read-only planning and research agent.",
        ))
        .stdout(contains(
            "\"permissionMode\":\"plan\",\"prompt\":\"You are a focused codebase exploration agent.",
        ))
        .stdout(contains(
            "\"tools\":[\"Read\",\"Glob\",\"Grep\"]",
        ))
        .stdout(contains("\"effortLevel\":\"high\""))
        .stdout(contains("\"model\":\"grok-4.5\""))
        .stdout(contains("grok-composer-2.5-fast"))
        .stdout(contains("\"ultracode\":false"))
        .stdout(contains("arg=<--continue>"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn profile_shortcuts_reject_cross_family_models_and_settings_overrides()
-> Result<(), Box<dyn std::error::Error>> {
    let grok = ClaudeLauncherFixture::new("cg")?;
    Command::new(&grok.shortcut)
        .args(["--model", "gpt-5.6-sol"])
        .env("PATH", grok.path_env())
        .env("HOME", &grok.home_dir)
        .assert()
        .failure()
        .stderr(contains("outside the Grok launch profile"));

    let gpt = ClaudeLauncherFixture::new("co")?;
    Command::new(&gpt.shortcut)
        .args(["--settings", "{}"])
        .env("PATH", gpt.path_env())
        .env("HOME", &gpt.home_dir)
        .assert()
        .failure()
        .stderr(contains("--settings is disabled"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn profile_shortcuts_reject_external_claude_config_dir() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = ClaudeLauncherFixture::new("co")?;
    let external_config = fixture.home_dir.join("custom-claude");

    Command::new(&fixture.shortcut)
        .env("PATH", fixture.path_env())
        .env("HOME", &fixture.home_dir)
        .env("CLAUDE_CONFIG_DIR", &external_config)
        .env_remove("CCP_CLAUDE_PROFILE_MANAGED")
        .assert()
        .failure()
        .stderr(contains("cannot be combined with co/cg history isolation"))
        .stderr(contains(external_config.display().to_string()));

    Command::new(&fixture.shortcut)
        .env("PATH", fixture.path_env())
        .env("HOME", &fixture.home_dir)
        .env("CLAUDE_CONFIG_DIR", &external_config)
        .env("CCP_CLAUDE_PROFILE_MANAGED", "1")
        .assert()
        .failure()
        .stderr(contains("cannot be combined with co/cg history isolation"));

    Command::new(&fixture.shortcut)
        .env("PATH", fixture.path_env())
        .env("HOME", &fixture.home_dir)
        .env(
            "CLAUDE_CONFIG_DIR",
            fixture.home_dir.join(".claude-ccproxy/grok"),
        )
        .env("CCP_CLAUDE_PROFILE_MANAGED", "1")
        .assert()
        .success()
        .stdout(contains(
            fixture
                .home_dir
                .join(".claude-ccproxy/gpt")
                .display()
                .to_string(),
        ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn co_and_cg_initialize_independent_profiles_in_parallel() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = ClaudeLauncherFixture::new("co")?;
    let cg_shortcut = fixture.add_shortcut("cg")?;
    let shared_config = fixture.home_dir.join(".claude");
    std::fs::create_dir_all(&shared_config)?;
    std::fs::write(
        shared_config.join("settings.json"),
        r#"{"permissions":{"deny":["WebSearch"]}}"#,
    )?;
    std::fs::write(
        fixture.home_dir.join(".claude.json"),
        serde_json::json!({
            "mcpServers": {"brave-search": {"command": "/usr/bin/true"}},
            "projects": {
                "/workspace": {
                    "hasTrustDialogAccepted": true,
                    "lastSessionId": "57c7c914-ada4-4f40-9672-985f950fbb66"
                }
            }
        })
        .to_string(),
    )?;

    let mut co = std::process::Command::new(&fixture.shortcut);
    co.env("PATH", fixture.path_env())
        .env("HOME", &fixture.home_dir)
        .stdout(std::process::Stdio::null());
    let mut cg = std::process::Command::new(&cg_shortcut);
    cg.env("PATH", fixture.path_env())
        .env("HOME", &fixture.home_dir)
        .stdout(std::process::Stdio::null());
    let co_child = co.spawn()?;
    let cg_child = cg.spawn()?;
    assert!(co_child.wait_with_output()?.status.success());
    assert!(cg_child.wait_with_output()?.status.success());

    for profile in ["gpt", "grok"] {
        let directory = fixture.home_dir.join(".claude-ccproxy").join(profile);
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.join(".claude.json"))?)?;
        assert_eq!(
            state["mcpServers"]["brave-search"]["command"],
            "/usr/bin/true"
        );
        assert!(
            state["projects"]["/workspace"]
                .get("lastSessionId")
                .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(directory.join("settings.json"))?,
            r#"{"permissions":{"deny":["WebSearch"]}}"#
        );
        assert!(!directory.join("history.jsonl").exists());
        assert!(!directory.join("projects").exists());
    }
    Ok(())
}

#[cfg(unix)]
struct ClaudeLauncherFixture {
    _temp: TempDir,
    shortcut: std::path::PathBuf,
    bin_dir: std::path::PathBuf,
    home_dir: std::path::PathBuf,
}

#[cfg(unix)]
impl ClaudeLauncherFixture {
    fn new(shortcut_name: &str) -> Result<Self, Box<dyn std::error::Error>> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = TempDir::new()?;
        let bin_dir = temp.path().to_path_buf();
        let home_dir = temp.path().join("home");
        std::fs::create_dir_all(&home_dir)?;
        let fake_claude = bin_dir.join("claude");
        std::fs::write(
            &fake_claude,
            r#"#!/bin/sh
printf 'base=%s\n' "$ANTHROPIC_BASE_URL"
printf 'config_dir=%s\n' "$CLAUDE_CONFIG_DIR"
printf 'profile_managed=%s\n' "$CCP_CLAUDE_PROFILE_MANAGED"
printf 'main=%s\n' "$ANTHROPIC_MODEL"
printf 'fable=%s\n' "$ANTHROPIC_DEFAULT_FABLE_MODEL"
printf 'opus=%s\n' "$ANTHROPIC_DEFAULT_OPUS_MODEL"
printf 'sonnet=%s\n' "$ANTHROPIC_DEFAULT_SONNET_MODEL"
printf 'haiku=%s\n' "$ANTHROPIC_DEFAULT_HAIKU_MODEL"
printf 'small=%s\n' "$ANTHROPIC_SMALL_FAST_MODEL"
printf 'max_context=%s\n' "$CLAUDE_CODE_MAX_CONTEXT_TOKENS"
printf 'compact_window=%s\n' "$CLAUDE_CODE_AUTO_COMPACT_WINDOW"
printf 'compact_pct=%s\n' "$CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"
printf 'disable_1m=%s\n' "$CLAUDE_CODE_DISABLE_1M_CONTEXT"
printf 'max_retries=%s\n' "$CLAUDE_CODE_MAX_RETRIES"
printf 'tool_concurrency=%s\n' "$CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY"
printf 'tool_search=%s\n' "$ENABLE_TOOL_SEARCH"
printf 'custom_headers=%s\n' "$ANTHROPIC_CUSTOM_HEADERS"
for arg in "$@"; do
  printf 'arg=<%s>\n' "$arg"
done
exit "${FAKE_CLAUDE_EXIT_CODE:-0}"
"#,
        )?;
        let mut permissions = std::fs::metadata(&fake_claude)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_claude, permissions)?;

        let shortcut = bin_dir.join(shortcut_name);
        symlink(
            assert_cmd::cargo::cargo_bin!("claude-code-proxy"),
            &shortcut,
        )?;
        Ok(Self {
            _temp: temp,
            shortcut,
            bin_dir,
            home_dir,
        })
    }

    fn path_env(&self) -> std::ffi::OsString {
        let mut paths = vec![self.bin_dir.clone()];
        if let Some(existing) = env::var_os("PATH") {
            paths.extend(env::split_paths(&existing));
        }
        env::join_paths(paths).expect("test PATH must be valid")
    }

    fn add_shortcut(
        &self,
        shortcut_name: &str,
    ) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
        use std::os::unix::fs::symlink;

        let shortcut = self.bin_dir.join(shortcut_name);
        symlink(
            assert_cmd::cargo::cargo_bin!("claude-code-proxy"),
            &shortcut,
        )?;
        Ok(shortcut)
    }
}
