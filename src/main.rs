use anyhow::{Context, Result};
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use claude_code_proxy::{
    config,
    diagnostics::{
        CollectOptions, UploadTarget, collect_bundle, inspect_bundle, upload_bundle_sftp,
    },
    logging,
    monitor::MonitorHandle,
    paths,
    registry::Registry,
    server::{self, ServerConfig},
    tui::{self, MonitorExit, MonitorUiConfig},
};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SESSION_END_HELPER_ARG: &str = "--ccproxy-session-end-hook";
const MAX_SESSION_END_INPUT_BYTES: u64 = 64 * 1024;
const MAX_DIAGNOSTIC_INPUT_BYTES: u64 = 64 * 1024 * 1024;
const CLAUDE_PROFILE_ROOT_DIR: &str = ".claude-ccproxy";
const CLAUDE_PROFILE_LOCK_FILE: &str = ".ccproxy-profile.lock";
const CLAUDE_PROFILE_INITIALIZED_FILE: &str = ".ccproxy-profile-initialized";
const CLAUDE_PROFILE_MANAGED_ENV: &str = "CCP_CLAUDE_PROFILE_MANAGED";
const CLAUDE_SHARED_CONFIG_ENTRIES: &[&str] = &[
    "CLAUDE.md",
    "agents",
    "channels",
    "commands",
    "hooks",
    "keybindings.json",
    "output-styles",
    "plugins",
    "rules",
    "settings.json",
    "settings.local.json",
    "skills",
    "themes",
    "workflows",
];
const EXPLORE_AGENT_DESCRIPTION: &str = "Fast, focused, read-only codebase exploration and search. Use proactively to locate files, trace code paths, understand architecture, and gather evidence before implementation.";
const EXPLORE_AGENT_PROMPT: &str = "You are a focused codebase exploration agent. Investigate the requested scope without modifying files. Prefer targeted searches, exact paths, and bounded line ranges over whole-file reads; trace the relevant control flow and return concise findings with file and line references. Clearly separate confirmed evidence from inference.";
const GENERAL_PURPOSE_AGENT_DESCRIPTION: &str = "General-purpose agent for complex, multi-step work that may require investigation, reasoning, implementation, and verification. Use proactively for substantial tasks that do not fit a narrower specialist.";
const GENERAL_PURPOSE_AGENT_PROMPT: &str = "You are a capable general-purpose engineering agent. Complete the delegated task end to end: inspect the relevant context, make focused changes when authorized, verify the result in proportion to risk, and return a concise evidence-backed summary. Preserve unrelated user changes and follow all applicable instructions.";
const PLAN_AGENT_DESCRIPTION: &str = "Read-only planning and research agent. Use proactively to investigate architecture, constraints, risks, and verification needs before implementation.";
const PLAN_AGENT_PROMPT: &str = "You are a read-only planning and research agent. Investigate the requested scope without modifying files, using targeted searches and bounded line ranges instead of ingesting whole large files. Identify the relevant architecture and constraints, surface risks and edge cases, and produce a decision-complete implementation and verification plan grounded in file and line evidence.";

#[derive(Debug, Parser)]
#[command(
    name = "claude-code-proxy",
    version = VERSION,
    about = "Anthropic-compatible proxy for Claude Code provider backends",
    disable_version_flag = true
)]
struct Cli {
    #[arg(long = "version", short = 'v', action = ArgAction::SetTrue)]
    version_flag: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Version {
        #[arg(long, action = ArgAction::SetTrue)]
        json: bool,
    },
    Serve {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long = "no-monitor", action = ArgAction::SetTrue)]
        no_monitor: bool,
        /// Acknowledge that remote clients are not authenticated; require a firewall or authenticating reverse proxy
        #[arg(long = "allow-remote-unauthenticated", action = ArgAction::SetTrue)]
        allow_remote_unauthenticated: bool,
    },
    /// Open the monitor TUI with mock data and no proxy server
    Demo,
    Models,
    Codex {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    Grok {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    /// Collect or manually upload a metadata-only diagnostic bundle
    Diagnostics {
        #[command(subcommand)]
        command: DiagnosticsCommand,
    },
    /// Launch Claude Code with a model-specific proxy profile
    Claude {
        #[arg(value_enum)]
        profile: ClaudeProfile,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ClaudeProfile {
    Gpt,
    Grok,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeLaunchStyle {
    Shortcut,
    Subcommand,
}

#[derive(Debug, Clone, Copy)]
struct ClaudeProfileConfig {
    name: &'static str,
    main_model: &'static str,
    fable_model: &'static str,
    opus_model: &'static str,
    sonnet_model: &'static str,
    haiku_model: &'static str,
    compaction_model: Option<&'static str>,
    context_tokens: &'static str,
    file_read_max_output_tokens: Option<&'static str>,
    workflow_size_guideline: Option<&'static str>,
    effort_level: &'static str,
    ultracode: bool,
    explore_effort: &'static str,
    general_purpose_effort: &'static str,
    plan_effort: &'static str,
    available_models: &'static [&'static str],
}

impl ClaudeProfile {
    fn storage_name(self) -> &'static str {
        match self {
            Self::Gpt => "gpt",
            Self::Grok => "grok",
        }
    }

    fn config(self) -> ClaudeProfileConfig {
        match self {
            Self::Gpt => ClaudeProfileConfig {
                name: "GPT",
                main_model: "fable",
                fable_model: "gpt-5.6-sol",
                opus_model: "gpt-5.6-sol",
                sonnet_model: "gpt-5.6-terra",
                haiku_model: "gpt-5.6-luna",
                compaction_model: Some("gpt-5.6-terra"),
                context_tokens: "272000",
                file_read_max_output_tokens: Some("8000"),
                workflow_size_guideline: Some("small"),
                // Default to high (not ultracode/xhigh) so ordinary co sessions
                // start at a calmer effort; users can still raise /effort later.
                effort_level: "high",
                ultracode: false,
                explore_effort: "medium",
                general_purpose_effort: "high",
                plan_effort: "high",
                available_models: &[
                    "fable",
                    "gpt-5.6-sol",
                    "gpt-5.6-sol-fast",
                    "gpt-5.6-terra",
                    "gpt-5.6-terra-fast",
                    "gpt-5.6-luna",
                    "gpt-5.6-luna-fast",
                ],
            },
            Self::Grok => ClaudeProfileConfig {
                name: "Grok",
                // Keep high as the profile default, but do not bake it into the
                // model id: Claude Code's /effort setting must remain effective.
                main_model: "grok-4.5",
                fable_model: "grok-4.5",
                opus_model: "grok-4.5-high",
                sonnet_model: "grok-4.5-high",
                haiku_model: "grok-4.5-medium",
                compaction_model: None,
                context_tokens: "500000",
                file_read_max_output_tokens: None,
                workflow_size_guideline: None,
                effort_level: "high",
                ultracode: false,
                explore_effort: "medium",
                general_purpose_effort: "high",
                plan_effort: "high",
                available_models: &[
                    "grok-4.5",
                    "grok-4.5-high",
                    "grok-4.5-medium",
                    "grok-composer-2.5-fast",
                ],
            },
        }
    }
}

#[derive(Debug, Subcommand)]
enum ProviderGroup {
    Auth {
        #[command(subcommand)]
        command: claude_code_proxy::provider::AuthCommand,
    },
}

#[derive(Debug, Subcommand)]
enum DiagnosticsCommand {
    /// Collect sanitized proxy events into a local metadata-only archive
    Collect {
        /// Proxy state directory containing proxy.log and its rotations
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Destination .tar.gz path
        #[arg(long)]
        output: Option<PathBuf>,
        /// Include only events for this request id
        #[arg(long)]
        request_id: Option<String>,
        /// Optional non-identifying label such as wsl-laptop
        #[arg(long)]
        machine_label: Option<String>,
        /// Maximum number of sanitized events retained
        #[arg(long, default_value_t = 10_000)]
        last_lines: usize,
    },
    /// Upload a validated metadata-only bundle over OpenSSH SFTP
    Upload {
        /// Local diagnostic .tar.gz bundle
        bundle: PathBuf,
        /// SSH host or alias (port and key come from ~/.ssh/config)
        #[arg(long)]
        host: String,
        /// Optional SSH user
        #[arg(long)]
        user: Option<String>,
        /// Existing absolute directory on the development machine
        #[arg(long)]
        remote_dir: String,
        /// Skip the interactive upload confirmation
        #[arg(long, action = ArgAction::SetTrue)]
        yes: bool,
    },
}

fn main() -> Result<()> {
    let result = run();
    if !logging::flush(Duration::from_secs(2)) {
        eprintln!("warning: proxy logs could not be flushed completely");
    }
    result
}

fn run() -> Result<()> {
    let raw_args = std::env::args_os().collect::<Vec<_>>();
    if let Some(destination) = session_end_helper_destination_from_argv(&raw_args) {
        return run_session_end_helper(&destination?);
    }
    if let Some((profile, args)) = claude_profile_from_argv(raw_args) {
        return launch_claude(profile, ClaudeLaunchStyle::Shortcut, &args);
    }

    let cli = Cli::parse();

    if cli.version_flag {
        println!("claude-code-proxy {}", VERSION);
        return Ok(());
    }

    let commands = cli.command.unwrap_or(Commands::Serve {
        port: None,
        no_monitor: false,
        allow_remote_unauthenticated: false,
    });

    match commands {
        Commands::Version { json } => {
            if json {
                println!("{}", serde_json::to_string(&server::version_info())?);
            } else {
                println!("claude-code-proxy {}", VERSION);
            }
            Ok(())
        }
        Commands::Serve {
            port,
            no_monitor,
            allow_remote_unauthenticated,
        } => {
            // Cache the running inode before a deployment can atomically replace
            // the Cellar path while this process is still serving.
            server::initialize_process_identity();
            let bind_address = config::bind_address();
            let allow_remote_unauthenticated =
                allow_remote_unauthenticated || config::allow_remote_unauthenticated();
            let effective_port = port.unwrap_or_else(config::port);
            // Upgrade an existing Grok file credential before the long-lived
            // provider client is created. Migration is verified and safely
            // falls back to the file if Keychain access is unavailable.
            drop(
                claude_code_proxy::providers::grok::auth::token_store::file_store_with_migration(),
            );
            let registry = Registry::with_default_alias();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            match select_serve_mode(std::io::stdout().is_terminal(), no_monitor) {
                ServeMode::Plain => {
                    print_server_banner(&bind_address, effective_port, &registry);
                    runtime
                        .block_on(server::serve_with_shutdown(
                            ServerConfig {
                                bind_address,
                                port: effective_port,
                                monitor: None,
                                allow_remote_unauthenticated,
                            },
                            shutdown_signal(),
                        ))
                        .map_err(|err| anyhow::anyhow!(err))
                }
                ServeMode::Monitor => {
                    let monitor = MonitorHandle::default();
                    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                    let (shutdown_complete_tx, shutdown_complete_rx) = std::sync::mpsc::channel();
                    let listener = runtime.block_on(server::bind_proxy_listener_with_ack(
                        &bind_address,
                        effective_port,
                        allow_remote_unauthenticated,
                    ))?;
                    // Keep bind-time security warnings visible before the TUI
                    // takes ownership of the terminal.
                    let _stderr_guard = logging::suppress_stderr();
                    let local_addr = listener.local_addr()?;
                    let monitor_listen_url =
                        listen_url(&local_addr.ip().to_string(), local_addr.port());
                    let server_monitor = monitor.clone();
                    let server_task = runtime.spawn(async move {
                        let result =
                            server::serve_listener(listener, Some(server_monitor), async move {
                                tokio::select! {
                                    _ = async {
                                        let _ = shutdown_rx.await;
                                    } => {}
                                    _ = shutdown_signal() => {}
                                }
                            })
                            .await;
                        let _ = shutdown_complete_tx.send(());
                        result
                    });
                    let ui_result = tui::run_monitor(
                        monitor,
                        MonitorUiConfig {
                            listen_url: monitor_listen_url,
                            port: effective_port,
                            registry: &registry,
                            shutdown: Some(shutdown_tx),
                            shutdown_complete: Some(shutdown_complete_rx),
                        },
                    );
                    if matches!(&ui_result, Ok(MonitorExit::ForceQuit)) {
                        server_task.abort();
                        let _ = runtime.block_on(server_task);
                        exit_with_logs(130);
                    }
                    let server_result = runtime.block_on(server_task)?;
                    ui_result?;
                    server_result.map_err(|err| anyhow::anyhow!(err))
                }
            }
        }
        Commands::Demo => {
            let registry = Registry::with_default_alias();
            tui::run_mock_monitor(config::port(), &registry)
        }
        Commands::Models => {
            print_models(&Registry::with_default_alias());
            Ok(())
        }
        Commands::Codex { command } => run_provider_cli("codex", command),
        Commands::Grok { command } => run_provider_cli("grok", command),
        Commands::Diagnostics { command } => run_diagnostics(command),
        Commands::Claude { profile, args } => {
            launch_claude(profile, ClaudeLaunchStyle::Subcommand, &args)
        }
    }
}

fn run_diagnostics(command: DiagnosticsCommand) -> Result<()> {
    match command {
        DiagnosticsCommand::Collect {
            state_dir,
            output,
            request_id,
            machine_label,
            last_lines,
        } => {
            let state_dir = state_dir.unwrap_or_else(paths::state_dir);
            let output = output.unwrap_or_else(|| {
                state_dir.join("diagnostics").join(format!(
                    "ccproxy-diagnostics-{}.tar.gz",
                    uuid::Uuid::new_v4()
                ))
            });
            let report = collect_bundle(&CollectOptions {
                state_dir,
                output,
                max_input_bytes: MAX_DIAGNOSTIC_INPUT_BYTES,
                max_lines: last_lines,
                request_id,
                machine_label,
            })?;
            println!("Diagnostic bundle: {}", report.bundle_path.display());
            println!(
                "Sanitized events: {} (skipped: {}, truncated: {})",
                report.event_count, report.skipped_count, report.truncated
            );
            println!("Bundle bytes: {}", report.bundle_bytes);
            println!("Privacy: metadata-only; no raw traffic, prompts, or tool content");
            Ok(())
        }
        DiagnosticsCommand::Upload {
            bundle,
            host,
            user,
            remote_dir,
            yes,
        } => {
            if !yes && !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "refusing a non-interactive upload without --yes; inspect the bundle and retry explicitly"
                );
            }
            let summary = inspect_bundle(&bundle)?;
            let target = UploadTarget::new(host, user, remote_dir)?;
            if !yes {
                eprintln!(
                    "Validated bundle {}: {} events, {} bytes, privacy={}, members=3",
                    summary.bundle_id, summary.event_count, summary.bytes, summary.privacy_level,
                );
                let destination = target.user().map_or_else(
                    || target.host().to_string(),
                    |user| format!("{user}@{}", target.host()),
                );
                eprint!(
                    "Upload canonical metadata-only bundle to {destination}:{}? [y/N] ",
                    target.remote_dir(),
                );
                std::io::stderr()
                    .flush()
                    .context("failed to display upload confirmation")?;
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .context("failed to read upload confirmation")?;
                if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    anyhow::bail!("diagnostic upload cancelled");
                }
            }
            let report = upload_bundle_sftp(&bundle, &target)?;
            println!(
                "Diagnostic bundle uploaded as {} ({} bytes)",
                report.remote_name, report.size_bytes
            );
            Ok(())
        }
    }
}

fn claude_profile_from_argv(
    args: impl IntoIterator<Item = OsString>,
) -> Option<(ClaudeProfile, Vec<OsString>)> {
    let mut args = args.into_iter();
    let executable = args.next()?;
    let name = Path::new(&executable).file_name()?.to_str()?;
    let profile = match name {
        "co" | "co.exe" => ClaudeProfile::Gpt,
        "cg" | "cg.exe" => ClaudeProfile::Grok,
        _ => return None,
    };
    Some((profile, args.collect()))
}

fn session_end_helper_destination_from_argv(args: &[OsString]) -> Option<Result<PathBuf>> {
    if args.get(1).and_then(|argument| argument.to_str()) != Some(SESSION_END_HELPER_ARG) {
        return None;
    }
    Some(match args {
        [_, _, destination] => Ok(PathBuf::from(destination)),
        _ => Err(anyhow::anyhow!(
            "{SESSION_END_HELPER_ARG} requires exactly one destination path"
        )),
    })
}

fn run_session_end_helper(destination: &Path) -> Result<()> {
    let stdin = std::io::stdin();
    write_session_end_capture(destination, stdin.lock())
}

fn write_session_end_capture(destination: &Path, input: impl Read) -> Result<()> {
    let mut payload = Vec::new();
    input
        .take(MAX_SESSION_END_INPUT_BYTES + 1)
        .read_to_end(&mut payload)
        .context("failed to read the SessionEnd hook input")?;
    if payload.len() as u64 > MAX_SESSION_END_INPUT_BYTES {
        anyhow::bail!("SessionEnd hook input exceeds {MAX_SESSION_END_INPUT_BYTES} bytes");
    }

    let payload: serde_json::Value =
        serde_json::from_slice(&payload).context("SessionEnd hook input is not valid JSON")?;
    if payload
        .get("hook_event_name")
        .and_then(|value| value.as_str())
        != Some("SessionEnd")
    {
        anyhow::bail!("hook input is not a SessionEnd event");
    }
    let reason = payload
        .get("reason")
        .and_then(|value| value.as_str())
        .context("SessionEnd hook input is missing a string reason")?;
    if !matches!(reason, "prompt_input_exit" | "other") {
        anyhow::bail!("SessionEnd reason `{reason}` is not a resumable process exit");
    }
    let session_id = payload
        .get("session_id")
        .and_then(|value| value.as_str())
        .context("SessionEnd hook input is missing a string session_id")?;
    let session_id = uuid::Uuid::parse_str(session_id)
        .context("SessionEnd hook session_id is not a valid UUID")?
        .to_string();

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(destination)
        .with_context(|| format!("failed to create session capture {}", destination.display()))?;
    file.write_all(session_id.as_bytes())
        .with_context(|| format!("failed to write session capture {}", destination.display()))?;
    Ok(())
}

#[derive(Debug)]
struct SessionEndCapture {
    directory: PathBuf,
    destination: PathBuf,
}

impl SessionEndCapture {
    fn new() -> Result<Self> {
        let directory = create_private_session_directory()?;
        let destination = directory.join("session-id");
        Ok(Self {
            directory,
            destination,
        })
    }

    fn destination(&self) -> &Path {
        &self.destination
    }

    fn captured_session_id(&self) -> Option<String> {
        let value = std::fs::read_to_string(&self.destination).ok()?;
        uuid::Uuid::parse_str(value.trim())
            .ok()
            .map(|session_id| session_id.to_string())
    }
}

impl Drop for SessionEndCapture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn create_private_session_directory() -> Result<PathBuf> {
    for _ in 0..16 {
        let directory =
            std::env::temp_dir().join(format!("ccproxy-session-{}", uuid::Uuid::new_v4().simple()));
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt as _;

            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        match builder.create(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to create private session directory {}",
                        directory.display()
                    )
                });
            }
        }
    }
    anyhow::bail!("failed to allocate a unique private session directory")
}

fn should_capture_session_end(
    args: &[OsString],
    stdin_is_tty: bool,
    stdout_is_tty: bool,
    stderr_is_tty: bool,
) -> bool {
    stdin_is_tty
        && stdout_is_tty
        && stderr_is_tty
        && !args
            .iter()
            .take_while(|argument| argument.as_os_str() != "--")
            .any(|argument| {
                let Some(argument) = argument.to_str() else {
                    return false;
                };
                argument == "-p"
                    || [
                        "--print",
                        "--bg",
                        "--background",
                        "--bare",
                        "--safe-mode",
                        "--tmux",
                    ]
                    .into_iter()
                    .any(|option| {
                        argument == option
                            || argument
                                .strip_prefix(option)
                                .is_some_and(|suffix| suffix.starts_with('='))
                    })
            })
}

fn session_end_hook_settings(executable: &Path, destination: &Path) -> Result<serde_json::Value> {
    if !executable.is_absolute() {
        anyhow::bail!("SessionEnd hook executable must be an absolute path");
    }
    let executable = executable
        .to_str()
        .context("SessionEnd hook executable path must be valid UTF-8")?;
    let destination = destination
        .to_str()
        .context("SessionEnd hook destination path must be valid UTF-8")?;
    Ok(serde_json::json!({
        "SessionEnd": [{
            "matcher": "prompt_input_exit|other",
            "hooks": [{
                "type": "command",
                "command": executable,
                "args": [SESSION_END_HELPER_ARG, destination],
            }],
        }],
    }))
}

fn claude_resume_hint(
    profile: ClaudeProfile,
    launch_style: ClaudeLaunchStyle,
    session_id: &str,
) -> String {
    let profile_name = match profile {
        ClaudeProfile::Gpt => "gpt",
        ClaudeProfile::Grok => "grok",
    };
    let command = match launch_style {
        ClaudeLaunchStyle::Shortcut => match profile {
            ClaudeProfile::Gpt => format!("co --resume {session_id}"),
            ClaudeProfile::Grok => format!("cg --resume {session_id}"),
        },
        ClaudeLaunchStyle::Subcommand => {
            format!("claude-code-proxy claude {profile_name} -- --resume {session_id}")
        }
    };
    format!("Resume this session with: {command}")
}

fn claude_home_directory() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .context("HOME/USERPROFILE is required to isolate Claude Code profile history")?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        anyhow::bail!(
            "HOME/USERPROFILE must be absolute to isolate Claude Code profile history: {}",
            home.display()
        );
    }
    Ok(home)
}

fn claude_profile_config_directory(home: &Path, profile: ClaudeProfile) -> PathBuf {
    home.join(CLAUDE_PROFILE_ROOT_DIR)
        .join(profile.storage_name())
}

fn prepare_claude_profile_config(profile: ClaudeProfile) -> Result<PathBuf> {
    let home = claude_home_directory()?;
    let inherited_config = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty());
    let managed_parent =
        std::env::var_os(CLAUDE_PROFILE_MANAGED_ENV).as_deref() == Some(std::ffi::OsStr::new("1"));
    if let Some(inherited_config) = inherited_config {
        let inherited_config = PathBuf::from(inherited_config);
        let is_managed_profile = [ClaudeProfile::Gpt, ClaudeProfile::Grok]
            .into_iter()
            .map(|candidate| claude_profile_config_directory(&home, candidate))
            .any(|candidate| candidate == inherited_config);
        if !managed_parent || !is_managed_profile {
            anyhow::bail!(
                "CLAUDE_CONFIG_DIR={} cannot be combined with co/cg history isolation; unset it or run `claude` directly for that custom identity",
                inherited_config.display()
            );
        }
    }

    let shared_directory = home.join(".claude");
    let shared_state_file = home.join(".claude.json");
    let profile_root = home.join(CLAUDE_PROFILE_ROOT_DIR);
    let profile_directory = claude_profile_config_directory(&home, profile);

    create_private_directory(&profile_root)?;
    create_private_directory(&profile_directory)?;

    let lock_path = profile_directory.join(CLAUDE_PROFILE_LOCK_FILE);
    let lock = open_private_lock_file(&lock_path)?;
    fs2::FileExt::lock_exclusive(&lock).with_context(|| {
        format!(
            "failed to lock Claude Code profile directory {}",
            profile_directory.display()
        )
    })?;

    let result = prepare_claude_profile_config_locked(
        &shared_directory,
        &shared_state_file,
        &profile_directory,
    );
    let unlock_result = fs2::FileExt::unlock(&lock).with_context(|| {
        format!(
            "failed to unlock Claude Code profile directory {}",
            profile_directory.display()
        )
    });
    result?;
    unlock_result?;
    Ok(profile_directory)
}

fn prepare_claude_profile_config_locked(
    shared_directory: &Path,
    shared_state_file: &Path,
    profile_directory: &Path,
) -> Result<()> {
    validate_profile_snapshot_paths(profile_directory)?;
    let initialized = profile_directory.join(CLAUDE_PROFILE_INITIALIZED_FILE);
    if regular_file_exists(&initialized)? {
        return Ok(());
    }

    for entry in CLAUDE_SHARED_CONFIG_ENTRIES {
        let source = shared_directory.join(entry);
        let destination = profile_directory.join(entry);
        copy_claude_config_entry_if_present(&source, &destination)?;
    }

    if let Some(shared_state) = read_optional_json_file(shared_state_file)? {
        seed_claude_profile_state(&shared_state, &profile_directory.join(".claude.json"))?;
    }
    write_private_file_if_absent(&initialized, b"v1\n")
}

fn validate_profile_snapshot_paths(profile_directory: &Path) -> Result<()> {
    for entry in CLAUDE_SHARED_CONFIG_ENTRIES.iter().copied().chain([
        ".claude.json",
        "file-history",
        "history.jsonl",
        "jobs",
        "plans",
        "projects",
        "session-env",
        "sessions",
        "tasks",
    ]) {
        let path = profile_directory.join(entry);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "isolated Claude Code profile entry must not be a symlink: {}; move it aside and retry",
                    path.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
            }
        }
    }
    Ok(())
}

fn regular_file_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => anyhow::bail!("expected a regular file at {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn create_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!(
                "private Claude Code profile path must not be a symlink: {}",
                path.display()
            );
        }
        Ok(metadata) if !metadata.is_dir() => {
            anyhow::bail!(
                "private Claude Code profile path is not a directory: {}",
                path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create private directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure private directory {}", path.display()))?;
    }
    Ok(())
}

fn open_private_lock_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("failed to open profile lock {}", path.display()))
}

fn read_optional_json_file(path: &Path) -> Result<Option<serde_json::Value>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let value = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    Ok(Some(value))
}

fn seed_claude_profile_state(shared_state: &serde_json::Value, destination: &Path) -> Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_file() => return Ok(()),
        Ok(_) => anyhow::bail!(
            "isolated Claude Code state must be a regular file: {}",
            destination.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", destination.display()));
        }
    }

    let mut profile_state = shared_state.clone();
    remove_project_last_session_ids(&mut profile_state);
    let mut bytes = serde_json::to_vec_pretty(&profile_state)
        .context("failed to serialize the isolated Claude Code profile state")?;
    bytes.push(b'\n');
    write_private_file_if_absent(destination, &bytes)
}

fn remove_project_last_session_ids(value: &mut serde_json::Value) {
    let Some(projects) = value
        .get_mut("projects")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    for project in projects.values_mut() {
        if let Some(project) = project.as_object_mut() {
            project.remove("lastSessionId");
        }
    }
}

fn write_private_file_if_absent(path: &Path, bytes: &[u8]) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => return Ok(()),
        Ok(_) => anyhow::bail!(
            "private file path is not a regular file: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    let temporary = private_temporary_path(path)?;
    write_private_file(&temporary, bytes)?;
    let linked = match fs::hard_link(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to atomically initialize Claude Code state {}",
                path.display()
            )
        }),
    };
    let _ = fs::remove_file(&temporary);
    linked
}

fn private_temporary_path(destination: &Path) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .with_context(|| format!("{} has no parent directory", destination.display()))?;
    Ok(parent.join(format!(".ccproxy-{}.tmp", uuid::Uuid::new_v4().simple())))
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create private file {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write private file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync private file {}", path.display()))?;
    Ok(())
}

fn copy_claude_config_entry_if_present(source: &Path, destination: &Path) -> Result<()> {
    let metadata = match fs::metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", source.display()));
        }
    };
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!(
                "private Claude Code config snapshot must not be a symlink: {}",
                destination.display()
            );
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", destination.display()));
        }
    }
    if metadata.is_dir() {
        let temporary = private_temporary_path(destination)?;
        if let Err(error) = copy_claude_config_directory(source, &temporary, &mut Vec::new()) {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error);
        }
        publish_config_snapshot(&temporary, destination, true)
    } else {
        let temporary = private_temporary_path(destination)?;
        fs::copy(source, &temporary)
            .with_context(|| format!("failed to copy Claude Code config {}", source.display()))?;
        publish_config_snapshot(&temporary, destination, false)
    }
}

fn publish_config_snapshot(temporary: &Path, destination: &Path, is_directory: bool) -> Result<()> {
    let result = if is_directory {
        match fs::rename(temporary, destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to publish Claude Code config snapshot {}",
                    destination.display()
                )
            }),
        }
    } else {
        match fs::hard_link(temporary, destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to publish Claude Code config snapshot {}",
                    destination.display()
                )
            }),
        }
    };
    if temporary.exists() {
        if is_directory {
            let _ = fs::remove_dir_all(temporary);
        } else {
            let _ = fs::remove_file(temporary);
        }
    }
    result
}

fn copy_claude_config_directory(
    source: &Path,
    destination: &Path,
    ancestors: &mut Vec<PathBuf>,
) -> Result<()> {
    let canonical = fs::canonicalize(source)
        .with_context(|| format!("failed to resolve Claude Code config {}", source.display()))?;
    if ancestors.contains(&canonical) {
        anyhow::bail!(
            "Claude Code config contains a directory symlink cycle at {}",
            source.display()
        );
    }
    ancestors.push(canonical);
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", source.display()))?;
        let child_source = entry.path();
        let child_destination = destination.join(entry.file_name());
        let metadata = fs::metadata(&child_source)
            .with_context(|| format!("failed to inspect {}", child_source.display()))?;
        if metadata.is_dir() {
            copy_claude_config_directory(&child_source, &child_destination, ancestors)?;
        } else {
            fs::copy(&child_source, &child_destination).with_context(|| {
                format!(
                    "failed to copy Claude Code config {}",
                    child_source.display()
                )
            })?;
        }
    }
    ancestors.pop();
    Ok(())
}

fn launch_claude(
    profile: ClaudeProfile,
    launch_style: ClaudeLaunchStyle,
    args: &[OsString],
) -> Result<()> {
    validate_claude_profile_args(profile.config(), args)?;
    let profile_config = prepare_claude_profile_config(profile)?;
    let loaded = config::load_config();
    let base_url = proxy_client_url(&loaded.bind_address, loaded.port);
    let capture = should_capture_session_end(
        args,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        std::io::stderr().is_terminal(),
    )
    .then(SessionEndCapture::new)
    .transpose()?;
    let Some(capture) = capture else {
        let command = build_claude_command_for_profile_config(
            profile,
            args,
            &base_url,
            &profile_config,
            None,
        )?;
        return launch_claude_without_capture(command);
    };

    let executable = std::env::current_exe().context("failed to locate the ccproxy executable")?;
    let hook = Some((executable.as_path(), capture.destination()));
    let mut command =
        build_claude_command_for_profile_config(profile, args, &base_url, &profile_config, hook)?;
    #[cfg(unix)]
    let status = wait_for_claude_with_signal_forwarding(&mut command)?;
    #[cfg(not(unix))]
    let status = command
        .status()
        .context("failed to launch local Claude Code")?;

    if let Some(session_id) = capture.captured_session_id() {
        eprintln!(
            "\n{}",
            claude_resume_hint(profile, launch_style, &session_id)
        );
    }

    // process::exit does not run destructors. Remove the private capture
    // directory explicitly before preserving Claude Code's exit status.
    drop(executable);
    drop(capture);
    exit_with_child_status(status);
}

fn launch_claude_without_capture(mut command: Command) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        let error = command.exec();
        Err(error).context("failed to exec local Claude Code")
    }

    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .context("failed to launch local Claude Code")?;
        exit_with_child_status(status);
    }
}

#[cfg(unix)]
fn wait_for_claude_with_signal_forwarding(command: &mut Command) -> Result<ExitStatus> {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    use signal_hook::iterator::{SignalsInfo, exfiltrator::WithOrigin};

    // Keep the wrapper alive long enough to collect SessionEnd while relaying
    // externally targeted termination signals to Claude Code. Terminal INT and
    // QUIT signals already reach both members of the foreground process group,
    // so origin metadata prevents turning one keypress into two signals. A
    // process-targeted signal still has sender metadata and is forwarded.
    let mut signals = SignalsInfo::<WithOrigin>::new([SIGHUP, SIGINT, SIGQUIT, SIGTERM])
        .context("failed to install Claude Code signal forwarding")?;
    let signal_handle = signals.handle();
    let mut child = command
        .spawn()
        .context("failed to launch local Claude Code")?;
    let child_pid = child.id() as libc::pid_t;
    let forwarder = std::thread::spawn(move || {
        for origin in signals.forever() {
            if !should_forward_signal_to_child(origin.signal, origin.process.is_some()) {
                continue;
            }
            // The PID came directly from Child and remains valid until wait
            // reaps it. A failed forward means the child already exited.
            unsafe {
                libc::kill(child_pid, origin.signal);
            }
        }
    });
    let status = child.wait();
    signal_handle.close();
    let _ = forwarder.join();
    status.context("failed while waiting for local Claude Code")
}

#[cfg(unix)]
fn should_forward_signal_to_child(signal: i32, has_sending_process: bool) -> bool {
    use signal_hook::consts::signal::{SIGINT, SIGQUIT};

    has_sending_process || !matches!(signal, SIGINT | SIGQUIT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildTermination {
    Exit(i32),
    #[cfg(unix)]
    Signal(i32),
    Unknown,
}

fn child_termination(status: &ExitStatus) -> ChildTermination {
    if let Some(code) = status.code() {
        return ChildTermination::Exit(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;

        if let Some(signal) = status.signal() {
            return ChildTermination::Signal(signal);
        }
    }
    ChildTermination::Unknown
}

fn exit_with_child_status(status: ExitStatus) -> ! {
    match child_termination(&status) {
        ChildTermination::Exit(code) => exit_with_logs(code),
        #[cfg(unix)]
        ChildTermination::Signal(signal) => {
            if !logging::flush(Duration::from_secs(2)) {
                eprintln!("warning: proxy logs could not be flushed completely");
            }
            // Re-raise the child's terminating signal so callers observe a
            // signaled wrapper, not a lossy synthetic 128+signal exit code.
            unsafe {
                libc::signal(signal, libc::SIG_DFL);
                libc::raise(signal);
            }
            std::process::exit(128 + signal);
        }
        ChildTermination::Unknown => exit_with_logs(1),
    }
}

#[cfg(test)]
fn build_claude_command(
    profile: ClaudeProfile,
    args: &[OsString],
    base_url: &str,
) -> Result<Command> {
    build_claude_command_with_session_end_hook(profile, args, base_url, None)
}

#[cfg(test)]
fn build_claude_command_with_session_end_hook(
    profile: ClaudeProfile,
    args: &[OsString],
    base_url: &str,
    session_end_hook: Option<(&Path, &Path)>,
) -> Result<Command> {
    let home = claude_home_directory()?;
    let profile_config = claude_profile_config_directory(&home, profile);
    build_claude_command_for_profile_config(
        profile,
        args,
        base_url,
        &profile_config,
        session_end_hook,
    )
}

fn build_claude_command_for_profile_config(
    profile: ClaudeProfile,
    args: &[OsString],
    base_url: &str,
    profile_config_directory: &Path,
    session_end_hook: Option<(&Path, &Path)>,
) -> Result<Command> {
    let profile = profile.config();
    validate_claude_profile_args(profile, args)?;
    let environment = claude_profile_environment(profile, base_url, profile_config_directory);
    let settings_environment = environment
        .iter()
        .map(|(name, value)| (name.to_string(), serde_json::Value::String(value.clone())))
        .collect::<serde_json::Map<_, _>>();
    let mut inline_settings = serde_json::json!({
        "env": settings_environment,
        "model": profile.main_model,
        "effortLevel": profile.effort_level,
        "ultracode": profile.ultracode,
        "availableModels": profile.available_models,
        "enforceAvailableModels": true,
        "fallbackModel": [],
    });
    if let Some(guideline) = profile.workflow_size_guideline {
        inline_settings["workflowSizeGuideline"] = serde_json::json!(guideline);
    }
    if let Some((executable, destination)) = session_end_hook {
        inline_settings["hooks"] = session_end_hook_settings(executable, destination)?;
    }
    let inline_settings = inline_settings.to_string();
    // CLI agent definitions are replacements, not partial overrides: Claude
    // Code requires both description and prompt. Keep its private built-in
    // `claude` agent untouched because public definitions cannot preserve that
    // agent's appendSystemPrompt/FleetView completion protocol.
    let inline_agents = serde_json::json!({
        "Explore": {
            "description": EXPLORE_AGENT_DESCRIPTION,
            "prompt": EXPLORE_AGENT_PROMPT,
            "tools": ["Read", "Glob", "Grep"],
            "permissionMode": "plan",
            "model": profile.haiku_model,
            "effort": profile.explore_effort,
        },
        "general-purpose": {
            "description": GENERAL_PURPOSE_AGENT_DESCRIPTION,
            "prompt": GENERAL_PURPOSE_AGENT_PROMPT,
            "model": profile.opus_model,
            "effort": profile.general_purpose_effort,
        },
        "Plan": {
            "description": PLAN_AGENT_DESCRIPTION,
            "prompt": PLAN_AGENT_PROMPT,
            "tools": ["Read", "Glob", "Grep"],
            "permissionMode": "plan",
            "model": profile.sonnet_model,
            "effort": profile.plan_effort,
        },
    })
    .to_string();

    let mut command = Command::new("claude");
    command
        .arg("--settings")
        .arg(inline_settings)
        .arg("--agents")
        .arg(inline_agents);
    command.args(args).envs(environment);
    if profile.file_read_max_output_tokens.is_none() {
        // Keep the Grok profile independent from a GPT-specific cap inherited
        // from the parent shell or from a previously configured launcher.
        command.env_remove("CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS");
    }
    Ok(command)
}

fn validate_claude_profile_args(profile: ClaudeProfileConfig, args: &[OsString]) -> Result<()> {
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        let Some(argument) = argument.to_str() else {
            continue;
        };
        if argument == "--" {
            break;
        }
        if let Some(option) = blocked_claude_profile_option(argument) {
            anyhow::bail!(
                "{option} is disabled for the {} launch profile because it can override its model, agent, or context isolation; run `claude` directly for custom settings",
                profile.name
            );
        }

        if argument == "--model" || argument == "-m" {
            let value = args
                .next()
                .context("--model/-m requires a model name after it")?;
            let model = value
                .to_str()
                .context("--model/-m must be valid UTF-8 in a co/cg launch profile")?;
            validate_profile_model(profile, model, "--model")?;
            continue;
        }
        if let Some(model) = argument.strip_prefix("--model=") {
            validate_profile_model(profile, model, "--model")?;
            continue;
        }
        if let Some(model) = argument.strip_prefix("-m=") {
            validate_profile_model(profile, model, "--model")?;
            continue;
        }
        if let Some(model) = argument.strip_prefix("-m") {
            validate_profile_model(profile, model, "--model")?;
            continue;
        }

        let fallback_models = if argument == "--fallback-model" {
            let value = args
                .next()
                .context("--fallback-model requires a comma-separated model list after it")?;
            Some(
                value
                    .to_str()
                    .context("--fallback-model must be valid UTF-8 in a co/cg launch profile")?,
            )
        } else {
            argument.strip_prefix("--fallback-model=")
        };
        if let Some(fallback_models) = fallback_models {
            for model in fallback_models.split(',').map(str::trim) {
                validate_profile_model(profile, model, "--fallback-model")?;
            }
        }
    }
    Ok(())
}

fn blocked_claude_profile_option(argument: &str) -> Option<&'static str> {
    [
        "--settings",
        "--managed-settings",
        "--agents",
        "--autocompact",
        "--advisor",
    ]
    .into_iter()
    .find(|option| {
        argument == *option
            || argument
                .strip_prefix(*option)
                .is_some_and(|suffix| suffix.starts_with('='))
    })
}

fn validate_profile_model(profile: ClaudeProfileConfig, model: &str, option: &str) -> Result<()> {
    if model.is_empty() || !profile.available_models.contains(&model) {
        anyhow::bail!(
            "{option} model `{model}` is outside the {} launch profile; use `co` for GPT models and `cg` for Grok models",
            profile.name
        );
    }
    Ok(())
}

fn claude_profile_environment(
    profile: ClaudeProfileConfig,
    base_url: &str,
    profile_config_directory: &Path,
) -> Vec<(&'static str, String)> {
    let mut environment = vec![
        (CLAUDE_PROFILE_MANAGED_ENV, "1".to_string()),
        (
            "CLAUDE_CONFIG_DIR",
            profile_config_directory.to_string_lossy().into_owned(),
        ),
        ("ANTHROPIC_BASE_URL", base_url.to_string()),
        ("ANTHROPIC_AUTH_TOKEN", "unused".to_string()),
        ("ANTHROPIC_MODEL", profile.main_model.to_string()),
        (
            "ANTHROPIC_DEFAULT_FABLE_MODEL",
            profile.fable_model.to_string(),
        ),
        (
            "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
            profile.fable_model.to_string(),
        ),
        (
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            profile.opus_model.to_string(),
        ),
        (
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
            profile.opus_model.to_string(),
        ),
        (
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            profile.sonnet_model.to_string(),
        ),
        (
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
            profile.sonnet_model.to_string(),
        ),
        (
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            profile.haiku_model.to_string(),
        ),
        (
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
            profile.haiku_model.to_string(),
        ),
        (
            "ANTHROPIC_SMALL_FAST_MODEL",
            profile.haiku_model.to_string(),
        ),
        (
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
            profile.context_tokens.to_string(),
        ),
        (
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
            profile.context_tokens.to_string(),
        ),
        ("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", "90".to_string()),
        ("CLAUDE_CODE_DISABLE_1M_CONTEXT", "1".to_string()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_string()),
        ("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1".to_string()),
        ("CLAUDE_CODE_MAX_RETRIES", "1".to_string()),
        ("CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY", "10".to_string()),
        ("ENABLE_TOOL_SEARCH", "true".to_string()),
    ];
    if let Some(model) = profile.compaction_model {
        environment.push((
            "ANTHROPIC_CUSTOM_HEADERS",
            format!("x-ccproxy-compaction-model: {model}"),
        ));
    }
    if let Some(tokens) = profile.file_read_max_output_tokens {
        environment.push((
            "CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS",
            tokens.to_string(),
        ));
    }
    environment
}

fn proxy_client_url(bind_address: &str, port: u16) -> String {
    let client_address = match bind_address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(address)) if address.is_unspecified() => "127.0.0.1".to_string(),
        Ok(std::net::IpAddr::V6(address)) if address.is_unspecified() => "::1".to_string(),
        _ => bind_address.to_string(),
    };
    listen_url(&client_address, port)
}

fn exit_with_logs(code: i32) -> ! {
    if !logging::flush(Duration::from_secs(2)) {
        eprintln!("warning: proxy logs could not be flushed completely");
    }
    std::process::exit(code)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServeMode {
    Monitor,
    Plain,
}

fn select_serve_mode(stdout_is_tty: bool, no_monitor: bool) -> ServeMode {
    if stdout_is_tty && !no_monitor {
        ServeMode::Monitor
    } else {
        ServeMode::Plain
    }
}

fn run_provider_cli(name: &str, command: ProviderGroup) -> Result<()> {
    let registry = Registry::with_default_alias();
    let provider = registry
        .provider(name)
        .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
    let handlers = provider.cli();
    match command {
        ProviderGroup::Auth { command } => match command {
            claude_code_proxy::provider::AuthCommand::Login => {
                if let Err(err) = handlers.login() {
                    eprintln!("{err}");
                    exit_with_logs(2);
                }
                Ok(())
            }
            claude_code_proxy::provider::AuthCommand::Device => {
                if let Err(err) = handlers.device() {
                    eprintln!("{err}");
                    exit_with_logs(2);
                }
                Ok(())
            }
            claude_code_proxy::provider::AuthCommand::Status => {
                if let Err(err) = handlers.status() {
                    println!("{err}");
                    if err.to_string() == "Not authenticated" {
                        exit_with_logs(1);
                    }
                    exit_with_logs(2);
                }
                Ok(())
            }
            claude_code_proxy::provider::AuthCommand::Logout => {
                handlers.logout()?;
                Ok(())
            }
        },
    }
}

fn print_models(registry: &Registry) {
    let grouped = registry.grouped_models();
    for provider in ["codex", "grok"] {
        let Some(models) = grouped.get(provider) else {
            continue;
        };
        println!("{provider}: {}", models.join(", "));
    }
}

fn listen_url(bind_address: &str, port: u16) -> String {
    match bind_address.parse::<std::net::IpAddr>() {
        Ok(ip) => format!("http://{}", std::net::SocketAddr::new(ip, port)),
        Err(_) => format!("http://{bind_address}:{port}"),
    }
}

fn print_server_banner(bind_address: &str, port: u16, registry: &Registry) {
    println!("Proxy listening on {}", listen_url(bind_address, port));
    println!("Logs: {}", paths::log_file().display());
    let cfg = paths::config_dir();
    if cfg.exists() {
        println!("Config: {}", cfg.display());
    }
    print_models(registry);
    println!();
    println!("Configure Claude Code (pick a model from above):");
    println!("  export ANTHROPIC_BASE_URL=\"http://localhost:{port}\"");
    println!("  export ANTHROPIC_AUTH_TOKEN=\"anything\"");
    println!("  export ANTHROPIC_MODEL=\"gpt-5.6-sol\"");
    println!("  export ANTHROPIC_SMALL_FAST_MODEL=\"gpt-5.6-luna\"");
    println!("  export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn default_serve_selects_monitor_on_tty() {
        assert_eq!(select_serve_mode(true, false), ServeMode::Monitor);
    }

    #[test]
    fn no_monitor_selects_plain_mode() {
        assert_eq!(select_serve_mode(true, true), ServeMode::Plain);
    }

    #[test]
    fn non_tty_stdout_selects_plain_mode() {
        assert_eq!(select_serve_mode(false, false), ServeMode::Plain);
    }

    #[test]
    fn demo_command_parses_without_server_options() {
        let cli = Cli::try_parse_from(["claude-code-proxy", "demo"]).unwrap();

        assert!(matches!(cli.command, Some(Commands::Demo)));
    }

    #[test]
    fn listen_url_brackets_ipv6_addresses() {
        assert_eq!(listen_url("::1", 18765), "http://[::1]:18765");
    }

    #[test]
    fn claude_profile_forwards_trailing_arguments() {
        let cli = Cli::try_parse_from([
            "claude-code-proxy",
            "claude",
            "gpt",
            "--",
            "--effort",
            "max",
            "hello world",
        ])
        .unwrap();

        let Some(Commands::Claude { profile, args }) = cli.command else {
            panic!("expected Claude profile command");
        };
        assert_eq!(profile, ClaudeProfile::Gpt);
        assert_eq!(args, ["--effort", "max", "hello world"].map(OsString::from));
    }

    #[test]
    fn argv_zero_shortcuts_select_profile_and_preserve_arguments() {
        let (profile, args) = claude_profile_from_argv([
            OsString::from("/home/user/.local/bin/co"),
            OsString::from("--effort"),
            OsString::from("max"),
        ])
        .expect("co should select a profile");
        assert_eq!(profile, ClaudeProfile::Gpt);
        assert_eq!(args, ["--effort", "max"].map(OsString::from));

        let (profile, args) =
            claude_profile_from_argv([OsString::from("cg"), OsString::from("--continue")])
                .expect("cg should select a profile");
        assert_eq!(profile, ClaudeProfile::Grok);
        assert_eq!(args, ["--continue"].map(OsString::from));
    }

    #[test]
    fn claude_profiles_resolve_distinct_history_directories() {
        let home = Path::new("/home/tester");

        assert_eq!(
            claude_profile_config_directory(home, ClaudeProfile::Gpt),
            PathBuf::from("/home/tester/.claude-ccproxy/gpt")
        );
        assert_eq!(
            claude_profile_config_directory(home, ClaudeProfile::Grok),
            PathBuf::from("/home/tester/.claude-ccproxy/grok")
        );
    }

    #[test]
    fn profile_initialization_copies_config_but_not_history() {
        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join(".claude");
        let profile = temp.path().join(".claude-ccproxy/gpt");
        fs::create_dir_all(shared.join("skills/example")).unwrap();
        fs::create_dir_all(shared.join("projects/example")).unwrap();
        fs::write(shared.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();
        fs::write(
            shared.join("skills/example/SKILL.md"),
            "shared skill content",
        )
        .unwrap();
        fs::write(shared.join("history.jsonl"), "legacy prompt\n").unwrap();
        fs::write(
            shared.join("projects/example/legacy-session.jsonl"),
            "legacy transcript\n",
        )
        .unwrap();
        let shared_state_file = temp.path().join(".claude.json");
        fs::write(
            &shared_state_file,
            serde_json::json!({
                "hasCompletedOnboarding": true,
                "mcpServers": {
                    "brave-search": {
                        "command": "/usr/bin/true",
                        "env": {"lastSessionId": "mcp-value-must-survive"}
                    }
                },
                "projects": {
                    "/workspace": {
                        "hasTrustDialogAccepted": true,
                        "lastSessionId": "57c7c914-ada4-4f40-9672-985f950fbb66"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        create_private_directory(&profile).unwrap();

        prepare_claude_profile_config_locked(&shared, &shared_state_file, &profile).unwrap();

        assert_eq!(
            fs::read_to_string(profile.join("settings.json")).unwrap(),
            r#"{"theme":"dark"}"#
        );
        assert_eq!(
            fs::read_to_string(profile.join("skills/example/SKILL.md")).unwrap(),
            "shared skill content"
        );
        #[cfg(unix)]
        {
            assert!(
                !fs::symlink_metadata(profile.join("settings.json"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(
                !fs::symlink_metadata(profile.join("skills"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        assert!(!profile.join("history.jsonl").exists());
        assert!(!profile.join("projects").exists());

        let profile_state: serde_json::Value =
            serde_json::from_slice(&fs::read(profile.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(profile_state["hasCompletedOnboarding"], true);
        assert_eq!(
            profile_state["projects"]["/workspace"]["hasTrustDialogAccepted"],
            true
        );
        assert_eq!(
            profile_state["mcpServers"]["brave-search"]["command"],
            "/usr/bin/true"
        );
        assert_eq!(
            profile_state["mcpServers"]["brave-search"]["env"]["lastSessionId"],
            "mcp-value-must-survive"
        );
        assert!(
            profile_state["projects"]["/workspace"]
                .get("lastSessionId")
                .is_none()
        );

        fs::write(shared.join("settings.json"), r#"{"theme":"light"}"#).unwrap();
        fs::write(&shared_state_file, "not valid JSON").unwrap();
        fs::write(
            profile.join(".claude.json"),
            r#"{"profileLocalState":"keep"}"#,
        )
        .unwrap();
        fs::write(profile.join("history.jsonl"), "gpt-only prompt\n").unwrap();
        prepare_claude_profile_config_locked(&shared, &shared_state_file, &profile).unwrap();
        assert_eq!(
            fs::read_to_string(profile.join(".claude.json")).unwrap(),
            r#"{"profileLocalState":"keep"}"#
        );
        assert_eq!(
            fs::read_to_string(profile.join("history.jsonl")).unwrap(),
            "gpt-only prompt\n"
        );
        assert_eq!(
            fs::read_to_string(profile.join("settings.json")).unwrap(),
            r#"{"theme":"dark"}"#
        );
    }

    #[cfg(unix)]
    #[test]
    fn profile_snapshot_dereferences_source_symlinks_and_rejects_destination_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join(".claude");
        let profile = temp.path().join(".claude-ccproxy/gpt");
        let external_settings = temp.path().join("shared-settings.json");
        fs::create_dir_all(&shared).unwrap();
        create_private_directory(&profile).unwrap();
        fs::write(&external_settings, r#"{"theme":"dark"}"#).unwrap();
        symlink(&external_settings, shared.join("settings.json")).unwrap();

        prepare_claude_profile_config_locked(
            &shared,
            &temp.path().join("missing-state.json"),
            &profile,
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(profile.join("settings.json")).unwrap(),
            r#"{"theme":"dark"}"#
        );
        assert!(
            !fs::symlink_metadata(profile.join("settings.json"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let other_profile = temp.path().join(".claude-ccproxy/grok");
        create_private_directory(&other_profile).unwrap();
        symlink(&external_settings, other_profile.join("settings.json")).unwrap();
        let error = prepare_claude_profile_config_locked(
            &shared,
            &temp.path().join("missing-state.json"),
            &other_profile,
        )
        .unwrap_err();
        assert!(error.to_string().contains("must not be a symlink"));
    }

    #[test]
    fn normal_binary_name_does_not_select_profile_shortcut() {
        assert!(
            claude_profile_from_argv([
                OsString::from("claude-code-proxy"),
                OsString::from("serve")
            ])
            .is_none()
        );
    }

    #[test]
    fn hidden_session_end_helper_is_detected_for_shortcut_argv() {
        let destination = std::env::temp_dir().join("ccproxy-session-id");
        let args = [
            OsString::from("co"),
            OsString::from(SESSION_END_HELPER_ARG),
            destination.as_os_str().to_owned(),
        ];

        assert_eq!(
            session_end_helper_destination_from_argv(&args)
                .expect("helper flag should be recognized")
                .unwrap(),
            destination
        );
    }

    #[test]
    fn session_end_helper_validates_and_exclusively_writes_uuid() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("session-id");
        let session_id = "57C7C914-ADA4-4F40-9672-985F950FBB66";
        let payload = serde_json::json!({
            "hook_event_name": "SessionEnd",
            "session_id": session_id,
            "reason": "prompt_input_exit",
        })
        .to_string();

        write_session_end_capture(&destination, payload.as_bytes()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "57c7c914-ada4-4f40-9672-985f950fbb66"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&destination)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        let error = write_session_end_capture(&destination, payload.as_bytes()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to create session capture")
        );
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "57c7c914-ada4-4f40-9672-985f950fbb66"
        );
    }

    #[test]
    fn session_end_helper_rejects_wrong_event_and_invalid_uuid() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("session-id");
        let wrong_event = serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": "57c7c914-ada4-4f40-9672-985f950fbb66",
        })
        .to_string();
        let error = write_session_end_capture(&destination, wrong_event.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("not a SessionEnd event"));
        assert!(!destination.exists());

        let invalid_uuid = serde_json::json!({
            "hook_event_name": "SessionEnd",
            "session_id": "not-a-session-id",
            "reason": "other",
        })
        .to_string();
        let error = write_session_end_capture(&destination, invalid_uuid.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("not a valid UUID"));
        assert!(!destination.exists());

        let cleared_session = serde_json::json!({
            "hook_event_name": "SessionEnd",
            "session_id": "57c7c914-ada4-4f40-9672-985f950fbb66",
            "reason": "clear",
        })
        .to_string();
        let error =
            write_session_end_capture(&destination, cleared_session.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("not a resumable process exit"));
        assert!(!destination.exists());
    }

    #[test]
    fn session_end_capture_uses_private_directory_and_cleans_it_up() {
        let directory = {
            let capture = SessionEndCapture::new().unwrap();
            let directory = capture.directory.clone();
            assert!(directory.is_dir());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                assert_eq!(
                    std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
            directory
        };
        assert!(!directory.exists());
    }

    #[test]
    fn session_capture_only_runs_for_interactive_non_print_mode() {
        assert!(should_capture_session_end(&[], true, true, true));
        for option in [
            "-p",
            "--print",
            "--bg",
            "--background",
            "--bare",
            "--safe-mode",
            "--tmux",
            "--print=true",
            "--bg=true",
            "--background=true",
            "--bare=true",
            "--safe-mode=true",
            "--tmux=true",
        ] {
            let args = [OsString::from(option), OsString::from("hello")];
            assert!(!should_capture_session_end(&args, true, true, true));
        }
        assert!(should_capture_session_end(
            &[OsString::from("--backgrounding")],
            true,
            true,
            true
        ));
        assert!(!should_capture_session_end(&[], false, true, true));
        assert!(!should_capture_session_end(&[], true, false, true));
        assert!(!should_capture_session_end(&[], true, true, false));
        assert!(should_capture_session_end(
            &[OsString::from("--"), OsString::from("--print")],
            true,
            true,
            true
        ));
    }

    #[test]
    fn session_end_hook_uses_exec_form_with_absolute_current_executable() {
        let executable = std::env::current_exe().unwrap();
        assert!(executable.is_absolute());
        let destination = std::env::temp_dir().join("ccproxy-session-id");
        let command = build_claude_command_with_session_end_hook(
            ClaudeProfile::Gpt,
            &[],
            "http://127.0.0.1:18765",
            Some((&executable, &destination)),
        )
        .unwrap();
        let settings = command_inline_settings(&command);
        let session_end = &settings["hooks"]["SessionEnd"][0];
        let hook = &session_end["hooks"][0];

        assert_eq!(session_end["matcher"], "prompt_input_exit|other");
        assert_eq!(hook["type"], "command");
        assert_eq!(hook["command"], executable.to_str().unwrap());
        assert_eq!(
            hook["args"],
            serde_json::json!([SESSION_END_HELPER_ARG, destination.to_str().unwrap()])
        );
    }

    #[test]
    fn resume_hints_use_the_exact_launch_style() {
        let session_id = "57c7c914-ada4-4f40-9672-985f950fbb66";
        assert_eq!(
            claude_resume_hint(ClaudeProfile::Gpt, ClaudeLaunchStyle::Shortcut, session_id),
            format!("Resume this session with: co --resume {session_id}")
        );
        assert_eq!(
            claude_resume_hint(ClaudeProfile::Grok, ClaudeLaunchStyle::Shortcut, session_id),
            format!("Resume this session with: cg --resume {session_id}")
        );
        assert_eq!(
            claude_resume_hint(
                ClaudeProfile::Gpt,
                ClaudeLaunchStyle::Subcommand,
                session_id
            ),
            format!(
                "Resume this session with: claude-code-proxy claude gpt -- --resume {session_id}"
            )
        );
        assert_eq!(
            claude_resume_hint(
                ClaudeProfile::Grok,
                ClaudeLaunchStyle::Subcommand,
                session_id
            ),
            format!(
                "Resume this session with: claude-code-proxy claude grok -- --resume {session_id}"
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn signaled_child_remains_a_signal_termination() {
        use std::os::unix::process::ExitStatusExt as _;

        let status = ExitStatus::from_raw(15);
        assert_eq!(child_termination(&status), ChildTermination::Signal(15));
    }

    #[cfg(unix)]
    #[test]
    fn terminal_group_signals_are_not_forwarded_twice() {
        use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};

        assert!(!should_forward_signal_to_child(SIGINT, false));
        assert!(!should_forward_signal_to_child(SIGQUIT, false));
        assert!(should_forward_signal_to_child(SIGINT, true));
        assert!(should_forward_signal_to_child(SIGQUIT, true));
        assert!(should_forward_signal_to_child(SIGTERM, false));
        assert!(should_forward_signal_to_child(SIGHUP, false));
    }

    #[test]
    fn gpt_profile_builds_codex_model_environment() {
        let command = build_claude_command(
            ClaudeProfile::Gpt,
            &[OsString::from("--effort"), OsString::from("max")],
            "http://127.0.0.1:18765",
        )
        .unwrap();

        assert_eq!(command.get_program(), OsStr::new("claude"));
        let command_args = command.get_args().collect::<Vec<_>>();
        assert_eq!(command_args[0], OsStr::new("--settings"));
        assert_eq!(command_args[2], OsStr::new("--agents"));
        assert_eq!(
            &command_args[4..],
            [OsStr::new("--effort"), OsStr::new("max")]
        );
        let settings = command_inline_settings(&command);
        let agents = command_inline_agents(&command);
        assert_complete_inline_agents(&agents);
        assert_eq!(settings["model"], "fable");
        assert_eq!(settings["effortLevel"], "high");
        assert_eq!(settings["ultracode"], false);
        assert_eq!(settings["workflowSizeGuideline"], "small");
        assert_eq!(settings["enforceAvailableModels"], true);
        assert_eq!(settings["fallbackModel"], serde_json::json!([]));
        assert!(
            PathBuf::from(command_env(&command, "CLAUDE_CONFIG_DIR"))
                .ends_with(".claude-ccproxy/gpt")
        );
        assert_eq!(
            settings["env"]["CLAUDE_CONFIG_DIR"],
            command_env(&command, "CLAUDE_CONFIG_DIR")
        );
        assert_eq!(command_env(&command, CLAUDE_PROFILE_MANAGED_ENV), "1");
        assert_eq!(agents["Explore"]["model"], "gpt-5.6-luna");
        assert_eq!(agents["Explore"]["effort"], "medium");
        assert_eq!(agents["Plan"]["model"], "gpt-5.6-terra");
        assert_eq!(agents["Plan"]["effort"], "high");
        assert_eq!(agents["general-purpose"]["model"], "gpt-5.6-sol");
        assert_eq!(agents["general-purpose"]["effort"], "high");
        assert!(
            settings["availableModels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model == "gpt-5.6-terra")
        );
        assert!(
            settings["availableModels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model == "fable")
        );
        assert!(
            !settings["availableModels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model == "opus")
        );
        assert_eq!(settings["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "272000");
        assert_eq!(command_env(&command, "ANTHROPIC_MODEL"), "fable");
        assert_eq!(
            command_env(&command, "ANTHROPIC_DEFAULT_SONNET_MODEL"),
            "gpt-5.6-terra"
        );
        assert_eq!(
            command_env(&command, "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            "gpt-5.6-luna"
        );
        assert_eq!(
            command_env(&command, "CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
            "272000"
        );
        assert_eq!(
            command_env(&command, "CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
            "272000"
        );
        assert_eq!(
            command_env(&command, "CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS"),
            "8000"
        );
        assert_eq!(
            command_env(&command, "ANTHROPIC_CUSTOM_HEADERS"),
            "x-ccproxy-compaction-model: gpt-5.6-terra"
        );
        assert_eq!(
            settings["env"]["ANTHROPIC_CUSTOM_HEADERS"],
            "x-ccproxy-compaction-model: gpt-5.6-terra"
        );
    }

    #[test]
    fn grok_profile_builds_grok_model_environment() {
        let command =
            build_claude_command(ClaudeProfile::Grok, &[], "http://127.0.0.1:18765").unwrap();

        let settings = command_inline_settings(&command);
        let agents = command_inline_agents(&command);
        assert_complete_inline_agents(&agents);
        assert_eq!(settings["model"], "grok-4.5");
        assert_eq!(settings["effortLevel"], "high");
        assert_eq!(settings["ultracode"], false);
        assert!(settings.get("workflowSizeGuideline").is_none());
        assert!(
            PathBuf::from(command_env(&command, "CLAUDE_CONFIG_DIR"))
                .ends_with(".claude-ccproxy/grok")
        );
        assert_eq!(agents["Explore"]["model"], "grok-4.5-medium");
        assert_eq!(agents["Explore"]["effort"], "medium");
        assert_eq!(agents["Plan"]["model"], "grok-4.5-high");
        assert_eq!(agents["Plan"]["effort"], "high");
        assert_eq!(agents["general-purpose"]["effort"], "high");
        assert!(
            settings["availableModels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model == "grok-4.5-medium")
        );
        assert!(
            settings["availableModels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model == "grok-composer-2.5-fast")
        );
        assert_eq!(settings["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "500000");
        assert_eq!(command_env(&command, "ANTHROPIC_MODEL"), "grok-4.5");
        assert_eq!(
            command_env(&command, "ANTHROPIC_DEFAULT_FABLE_MODEL"),
            "grok-4.5"
        );
        assert_eq!(
            command_env(&command, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
            "grok-4.5-high"
        );
        assert_eq!(
            command_env(&command, "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            "grok-4.5-medium"
        );
        assert_eq!(
            command_env(&command, "ANTHROPIC_SMALL_FAST_MODEL"),
            "grok-4.5-medium"
        );
        assert_eq!(
            command_env(&command, "CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
            "500000"
        );
        assert_eq!(
            command_env(&command, "CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
            "500000"
        );
        assert_eq!(command_env(&command, "CLAUDE_CODE_DISABLE_1M_CONTEXT"), "1");
        assert_eq!(
            command_env(&command, "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"),
            "90"
        );
        assert!(
            command_env_optional(&command, "CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS").is_none()
        );
        assert!(command_env_optional(&command, "ANTHROPIC_CUSTOM_HEADERS").is_none());
    }

    #[test]
    fn claude_profiles_reject_cross_family_models() {
        let error = build_claude_command(
            ClaudeProfile::Grok,
            &[OsString::from("--model=gpt-5.6-sol")],
            "http://127.0.0.1:18765",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside the Grok launch profile")
        );

        build_claude_command(
            ClaudeProfile::Gpt,
            &[OsString::from("--model"), OsString::from("gpt-5.6-terra")],
            "http://127.0.0.1:18765",
        )
        .expect("same-family model should remain selectable");

        let error = build_claude_command(
            ClaudeProfile::Gpt,
            &[
                OsString::from("--fallback-model"),
                OsString::from("gpt-5.6-luna,grok-4.5-medium"),
            ],
            "http://127.0.0.1:18765",
        )
        .unwrap_err();
        assert!(error.to_string().contains("--fallback-model model"));

        let error = build_claude_command(
            ClaudeProfile::Gpt,
            &[OsString::from("--model=GPT-5.6-SOL")],
            "http://127.0.0.1:18765",
        )
        .unwrap_err();
        assert!(error.to_string().contains("outside the GPT launch profile"));

        for args in [
            vec![OsString::from("-m"), OsString::from("grok-4.5-high")],
            vec![OsString::from("-m=grok-4.5-high")],
            vec![OsString::from("-mgrok-4.5-high")],
        ] {
            let error = build_claude_command(ClaudeProfile::Gpt, &args, "http://127.0.0.1:18765")
                .unwrap_err();
            assert!(error.to_string().contains("outside the GPT launch profile"));
        }
    }

    #[test]
    fn claude_profiles_reject_settings_override() {
        let error = build_claude_command(
            ClaudeProfile::Gpt,
            &[OsString::from("--settings"), OsString::from("{}")],
            "http://127.0.0.1:18765",
        )
        .unwrap_err();
        assert!(error.to_string().contains("--settings is disabled"));

        let error = build_claude_command(
            ClaudeProfile::Grok,
            &[OsString::from("--managed-settings"), OsString::from("{}")],
            "http://127.0.0.1:18765",
        )
        .unwrap_err();
        assert!(error.to_string().contains("--managed-settings"));

        for option in ["--agents", "--autocompact", "--advisor"] {
            let error = build_claude_command(
                ClaudeProfile::Gpt,
                &[OsString::from(option), OsString::from("override")],
                "http://127.0.0.1:18765",
            )
            .unwrap_err();
            assert!(error.to_string().contains(option));
        }
    }

    #[test]
    fn proxy_client_url_uses_loopback_for_unspecified_listener() {
        assert_eq!(proxy_client_url("0.0.0.0", 18765), "http://127.0.0.1:18765");
        assert_eq!(proxy_client_url("::", 18765), "http://[::1]:18765");
    }

    fn command_env(command: &Command, key: &str) -> String {
        command_env_optional(command, key)
            .unwrap_or_else(|| panic!("missing UTF-8 command environment variable: {key}"))
    }

    fn command_env_optional(command: &Command, key: &str) -> Option<String> {
        command
            .get_envs()
            .find_map(|(name, value)| {
                (name == OsStr::new(key)).then(|| value.and_then(OsStr::to_str))
            })
            .flatten()
            .map(str::to_string)
    }

    fn command_inline_settings(command: &Command) -> serde_json::Value {
        let args = command.get_args().collect::<Vec<_>>();
        assert_eq!(args.first().copied(), Some(OsStr::new("--settings")));
        serde_json::from_str(
            args.get(1)
                .and_then(|value| value.to_str())
                .expect("inline settings must be a UTF-8 argument"),
        )
        .expect("inline settings must be valid JSON")
    }

    fn command_inline_agents(command: &Command) -> serde_json::Value {
        let args = command.get_args().collect::<Vec<_>>();
        assert_eq!(args.get(2).copied(), Some(OsStr::new("--agents")));
        serde_json::from_str(
            args.get(3)
                .and_then(|value| value.to_str())
                .expect("inline agents must be a UTF-8 argument"),
        )
        .expect("inline agents must be valid JSON")
    }

    fn assert_complete_inline_agents(agents: &serde_json::Value) {
        assert!(
            agents.get("claude").is_none(),
            "the private built-in claude agent must not be overridden"
        );
        for name in ["Explore", "general-purpose", "Plan"] {
            assert!(
                agents[name]["description"]
                    .as_str()
                    .is_some_and(|value| !value.trim().is_empty()),
                "{name} must have a nonempty description"
            );
            assert!(
                agents[name]["prompt"]
                    .as_str()
                    .is_some_and(|value| !value.trim().is_empty()),
                "{name} must have a nonempty prompt"
            );
        }

        let read_only_tools = serde_json::json!(["Read", "Glob", "Grep"]);
        for name in ["Explore", "Plan"] {
            assert_eq!(agents[name]["tools"], read_only_tools);
            assert_eq!(agents[name]["permissionMode"], "plan");
        }
        assert!(agents["general-purpose"].get("tools").is_none());
        assert!(agents["general-purpose"].get("permissionMode").is_none());
    }
}
