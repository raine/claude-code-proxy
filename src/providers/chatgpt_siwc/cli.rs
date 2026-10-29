use super::{SiwcProvider, auth::Auth, client::Client};
use anyhow::Result;
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage official Sign in with ChatGPT accounts
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Fetch the selected account's current model catalog from the official API
    Models {
        #[arg(long, default_value = "default")]
        profile: String,
    },
    /// Start an isolated SIWC proxy on 127.0.0.1
    Serve {
        #[arg(long, default_value = "default")]
        profile: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value_t = 18765)]
        port: u16,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Continue with ChatGPT; repeat for reauthorization of the same registration
    Login {
        #[arg(long, default_value = "default")]
        profile: String,
        /// Explicitly request consent for ChatGPT plan usage
        #[arg(long)]
        consent: bool,
        /// Print a hint-free authorization URL instead of launching a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Show account registrations and permission/expiry status, without tokens
    Status {
        #[arg(long, default_value = "default")]
        profile: String,
    },
    /// Revoke the renewable session and clear tokens, retaining registration/host
    Logout {
        #[arg(long, default_value = "default")]
        profile: String,
    },
}

pub fn run(command: Command) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
        match command {
            Command::Auth { command } => match command {
                AuthCommand::Login { profile, consent, no_browser } => Auth::new(profile)?.login(consent, no_browser).await,
                AuthCommand::Status { profile } => { println!("{}", serde_json::to_string_pretty(&Auth::new(profile)?.status().await?)?); Ok(()) },
                AuthCommand::Logout { profile } => {
                    let confirmed = Auth::new(profile)?.logout().await?;
                    println!("Signed out locally; registration and host ID retained.");
                    if !confirmed { println!("Remote revocation was not confirmed. Disconnect the app in ChatGPT Settings: https://chatgpt.com/settings/usage"); }
                    Ok(())
                },
            },
            Command::Models { profile } => {
                let models = Client::new(Auth::new(profile)?).models().await?;
                println!("{}", serde_json::to_string_pretty(&models)?);
                Ok(())
            },
            Command::Serve { profile, model, port } => {
                let provider = SiwcProvider::new(profile.clone(), model.or_else(crate::config::siwc_model))?;
                // Authenticate once at startup; every request rereads credentials.
                provider.client.auth.access_token().await?;
                let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
                println!("SIWC proxy: http://{} | ChatGPT profile: {profile}", listener.local_addr()?);
                println!("Models: claude-code-proxy siwc models --profile {profile}");
                let registry = crate::registry::Registry::from_providers(crate::config::AliasProvider::Siwc, [std::sync::Arc::new(provider) as std::sync::Arc<dyn crate::Provider>]);
                axum::serve(listener, crate::server::app(std::sync::Arc::new(registry)).into_make_service_with_connect_info::<std::net::SocketAddr>())
                    .with_graceful_shutdown(async { shutdown_signal().await }).await?;
                Ok(())
            },
        }
    })
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
