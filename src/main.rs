use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand};
use claude_code_proxy::{
    config, logging,
    monitor::MonitorHandle,
    paths,
    registry::{ANTHROPIC_STYLE_ALIASES, Registry},
    server::{self, ServerConfig},
    tui::{self, MonitorUiConfig},
};
use std::io::IsTerminal;

const VERSION: &str = env!("CARGO_PKG_VERSION");

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
    Version,
    Serve {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long = "no-monitor", action = ArgAction::SetTrue)]
        no_monitor: bool,
    },
    Models {
        #[arg(long)]
        full: bool,
    },
    Codex {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    Kimi {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    Cursor {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    Grok {
        #[command(subcommand)]
        command: ProviderGroup,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderGroup {
    Auth {
        #[command(subcommand)]
        command: claude_code_proxy::provider::AuthCommand,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.version_flag {
        println!("claude-code-proxy {}", VERSION);
        return Ok(());
    }

    let commands = cli.command.unwrap_or(Commands::Serve {
        port: None,
        no_monitor: false,
    });

    match commands {
        Commands::Version => {
            println!("claude-code-proxy {}", VERSION);
            Ok(())
        }
        Commands::Serve { port, no_monitor } => {
            let bind_address = config::bind_address();
            let effective_port = port.unwrap_or_else(config::port);
            let registry = Registry::with_default_alias();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            match select_serve_mode(std::io::stdout().is_terminal(), no_monitor) {
                ServeMode::Plain => {
                    print_server_banner(&bind_address, effective_port, &registry);
                    runtime
                        .block_on(server::serve(ServerConfig {
                            bind_address,
                            port: effective_port,
                            monitor: None,
                        }))
                        .map_err(|err| anyhow::anyhow!(err))
                }
                ServeMode::Monitor => {
                    let _stderr_guard = logging::suppress_stderr();
                    let monitor = MonitorHandle::default();
                    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                    let listener = runtime
                        .block_on(server::bind_proxy_listener(&bind_address, effective_port))?;
                    let local_addr = listener.local_addr()?;
                    let monitor_listen_url =
                        listen_url(&local_addr.ip().to_string(), local_addr.port());
                    let server_monitor = monitor.clone();
                    let server_task = runtime.spawn(server::serve_listener(
                        listener,
                        Some(server_monitor),
                        async move {
                            let _ = shutdown_rx.await;
                        },
                    ));
                    let ui_result = tui::run_monitor(
                        monitor,
                        MonitorUiConfig {
                            listen_url: monitor_listen_url,
                            port: effective_port,
                            registry: &registry,
                            shutdown: Some(shutdown_tx),
                        },
                    );
                    let server_result = runtime.block_on(server_task)?;
                    ui_result?;
                    server_result.map_err(|err| anyhow::anyhow!(err))
                }
            }
        }
        Commands::Models { full } => {
            print_models(&Registry::with_default_alias(), full);
            Ok(())
        }
        Commands::Codex { command } => run_provider_cli("codex", command),
        Commands::Kimi { command } => run_provider_cli("kimi", command),
        Commands::Cursor { command } => run_provider_cli("cursor", command),
        Commands::Grok { command } => run_provider_cli("grok", command),
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
                    std::process::exit(2);
                }
                Ok(())
            }
            claude_code_proxy::provider::AuthCommand::Device => {
                if let Err(err) = handlers.device() {
                    eprintln!("{err}");
                    std::process::exit(2);
                }
                Ok(())
            }
            claude_code_proxy::provider::AuthCommand::Status => {
                if let Err(err) = handlers.status() {
                    println!("{err}");
                    if err.to_string() == "Not authenticated" {
                        std::process::exit(1);
                    }
                    std::process::exit(2);
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

fn print_models(registry: &Registry, full: bool) {
    let grouped = registry.grouped_models();
    for provider in ["codex", "kimi", "grok", "cursor"] {
        let Some(models) = grouped.get(provider) else {
            continue;
        };
        if full || provider != "cursor" {
            println!("{provider}: {}", models.join(", "));
        } else {
            println!("{provider}: {}", compact_cursor_list(models));
        }
    }
}

fn compact_cursor_list(models: &[String]) -> String {
    let mut legacy = Vec::new();
    let mut dynamic = Vec::new();
    for model in models {
        if !model.contains(':') {
            legacy.push(model.clone());
        } else {
            dynamic.push(model.clone());
        }
    }
    let mut out = String::new();
    if !legacy.is_empty() {
        out.push_str(&legacy.join(", "));
        out.push_str("; ");
    }
    out.push_str(&format!("{} cursor model aliases", dynamic.len()));
    if !dynamic.is_empty() {
        out.push_str(", example: cursor:gpt-5.5");
    }
    out.push_str(" run `claude-code-proxy models --full` for all aliases");
    out
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
    print_models(registry, false);
    println!();
    println!("Configure Claude Code (pick a model from above):");
    println!("  export ANTHROPIC_BASE_URL=\"http://localhost:{port}\"");
    println!("  export ANTHROPIC_AUTH_TOKEN=\"anything\"");
    println!("  export ANTHROPIC_MODEL=\"gpt-5.6-sol\"");
    println!("  export ANTHROPIC_SMALL_FAST_MODEL=\"gpt-5.6-luna\"");
    println!("  export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1");
}

#[allow(dead_code)]
fn alias_names() -> usize {
    ANTHROPIC_STYLE_ALIASES.len()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn listen_url_brackets_ipv6_addresses() {
        assert_eq!(listen_url("::1", 18765), "http://[::1]:18765");
    }
}
