mod access;
mod config;
#[cfg(test)]
mod mode_tests;
mod proxy;
mod rollout;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Config file (default: ~/.hey-proxy/config.json)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Override the configured listening address
    #[arg(long)]
    listen: Option<SocketAddr>,
    /// Create the config if missing, validate it, and exit
    #[arg(long)]
    init: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Install/update the proxy and sync its config on SSH hosts
    Rollout {
        /// Only deploy these configured hosts (repeatable)
        #[arg(long)]
        host: Vec<String>,
    },
    /// Check configured credential sources without printing their values
    CheckCredentials,
    /// Generate/reuse host credentials; JSON output contains secrets
    #[command(hide = true)]
    HostKeys {
        #[arg(long)]
        client: Vec<String>,
    },
    /// Verify this service and its upstream/host connection
    Verify {
        /// Also check an explicitly configured Codex installation
        #[arg(long)]
        codex: bool,
        #[arg(long)]
        codex_home: Option<PathBuf>,
    },
    /// Configure the current user's Codex to use a local proxy
    ConfigureCodex {
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        codex_home: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
        /// Read generated local host credentials from this proxy config
        #[arg(long)]
        proxy_config: Option<PathBuf>,
    },
    /// Install/update the Gemini Codex profile without changing the default
    ConfigureGemini {
        #[arg(long)]
        base_url: String,
        #[arg(long, default_value = "gemini/gemini-2.5-pro")]
        model: String,
        #[arg(long)]
        codex_home: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(Command::ConfigureGemini {
        base_url,
        model,
        codex_home,
    }) = &args.command
    {
        return rollout::configure_gemini(base_url, model, codex_home.as_deref());
    }
    if let Some(Command::ConfigureCodex {
        base_url,
        codex_home,
        model,
        proxy_config,
    }) = &args.command
    {
        let token = if let Some(path) = proxy_config {
            let config = config::load(path)?;
            if config.mode == config::Mode::Host {
                Some(access::ensure(path, &[])?.local)
            } else {
                None
            }
        } else {
            None
        };
        return rollout::configure_codex_authenticated(
            base_url,
            codex_home.as_deref(),
            model.as_deref(),
            token.as_deref(),
        );
    }
    let path = match args.config {
        Some(path) => path,
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set; use --config")?)
            .join(".hey-proxy/config.json"),
    };
    // Fingerprint before loading so an edit racing startup is picked up by the first request.
    let fingerprint = config::fingerprint(&path);
    let config = if matches!(args.command, Some(Command::Rollout { .. })) {
        config::load(&path)?
    } else {
        config::load_or_create(&path)?
    };
    if let Some(Command::Rollout { host }) = args.command {
        return rollout::run(&config, &host).await;
    }
    if matches!(args.command, Some(Command::CheckCredentials)) {
        rollout::check_credentials(&config).await?;
        println!("Credential sources ready");
        return Ok(());
    }
    if let Some(Command::HostKeys { client }) = args.command {
        if config.mode != config::Mode::Host {
            anyhow::bail!("host-keys requires host mode");
        }
        println!(
            "{}",
            serde_json::to_string(&access::ensure(&path, &client)?)?
        );
        return Ok(());
    }
    if let Some(Command::Verify { codex, codex_home }) = args.command {
        return rollout::verify(
            &config,
            &path,
            codex_home.as_deref(),
            codex || codex_home.is_some(),
        )
        .await;
    }
    if args.init {
        println!("Config ready: {}", path.display());
        return Ok(());
    }
    let listen = args.listen.unwrap_or(config.listen);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("Cannot listen on {listen}"))?;
    println!("hey-proxy listening on http://{}", listener.local_addr()?);
    println!("Request overview: http://{}/logs", listener.local_addr()?);
    println!("Config: {} (changes apply to new requests)", path.display());
    if config.mode == config::Mode::Host {
        access::ensure(&path, &[])?;
    }
    let logs = std::sync::Arc::new(proxy::logs::Store::open(&config, &path)?);
    let options = proxy::Options {
        logs: Some(logs.clone()),
        access_config: if config.mode == config::Mode::Host {
            Some(path.clone())
        } else {
            None
        },
        source: Some((path, fingerprint)),
        ..proxy::Options::default()
    };
    axum::serve(listener, proxy::router_with(config, options)?)
        .with_graceful_shutdown(async {
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("install SIGTERM handler");
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
        })
        .await?;
    logs.flush().await?;
    Ok(())
}
