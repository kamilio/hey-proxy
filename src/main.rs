mod access;
mod claude_auth;
mod codex_auth;
mod config;
mod gemini_cli;
#[cfg(test)]
mod mode_tests;
mod model_registry;
mod proxy;
mod rollout;
mod spend;
mod usage_cli;

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
    /// Check remaining subscription quota and provider-reported extra spend
    Usage(usage_cli::Args),
    /// Recommend the best subscription provider (codex or claude) based on earliest expiring usage and remaining quota
    Recommend(usage_cli::RecommendArgs),
    /// Monitor API-equivalent spend across providers and models and estimate subscription yield
    Spend(usage_cli::SpendArgs),
    /// Install/update the proxy and sync its config on SSH hosts
    Rollout {
        /// Only deploy these configured hosts (repeatable)
        #[arg(long)]
        host: Vec<String>,
    },
    /// Check configured credential sources without printing their values
    CheckCredentials,
    /// Sign in to Claude; the proxy stores and refreshes its own OAuth tokens
    ClaudeLogin {
        /// Print the authorization URL without opening a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Sign in with a Codex (ChatGPT) subscription and store proxy-owned OAuth credentials
    CodexLogin {
        /// Print the authorization URL without opening a browser
        #[arg(long)]
        no_browser: bool,
        /// Sign in using a one-time device code instead of a localhost browser callback
        #[arg(long)]
        device_code: bool,
        /// Import existing OAuth credentials from CODEX_HOME (~/.codex/auth.json) into hey-proxy
        #[arg(long)]
        import_codex_home: bool,
    },
    /// Encrypt literal API keys in this config using a private local key file
    EncryptConfig,
    /// Internal rollout transport; stdout contains plaintext credentials
    #[command(hide = true)]
    ExportConfig,
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
        /// Read generated local host credentials from this proxy config
        #[arg(long)]
        proxy_config: Option<PathBuf>,
    },
    /// Configure the current user's Pi with this proxy's providers and models
    ConfigurePi,
    /// Configure Gemini CLI to use this proxy's native Gemini API
    ConfigureGeminiCli {
        /// Native model or alias (default: first configured Gemini destination)
        #[arg(long)]
        model: Option<String>,
        /// Gemini configuration directory (default: ~/.gemini)
        #[arg(long)]
        gemini_home: Option<PathBuf>,
    },
}

fn config_path(config: Option<PathBuf>) -> Result<PathBuf> {
    match config {
        Some(path) => Ok(path),
        None => Ok(PathBuf::from(
            std::env::var_os("HOME").context("HOME is not set; use --config")?,
        )
        .join(".hey-proxy/config.json")),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(Command::Usage(usage)) = args.command {
        return usage_cli::run(usage, args.config).await;
    }
    if let Some(Command::Recommend(recommend)) = args.command {
        return usage_cli::run_recommend(recommend, args.config).await;
    }
    if let Some(Command::Spend(spend_args)) = args.command {
        return usage_cli::run_spend(spend_args, args.config).await;
    }
    if let Some(Command::ConfigureGemini {
        base_url,
        model,
        codex_home,
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
        return rollout::configure_gemini_authenticated(
            base_url,
            model,
            codex_home.as_deref(),
            token.as_deref(),
        );
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
    if let Some(Command::ConfigureGeminiCli { model, gemini_home }) = &args.command {
        let path = config_path(args.config.clone())?;
        let config = config::load(&path)?;
        let api_key = if config.mode == config::Mode::Host {
            access::ensure(&path, &[])?.local
        } else {
            "hey-proxy".to_owned()
        };
        return gemini_cli::configure(&config, &api_key, model.as_deref(), gemini_home.as_deref());
    }
    if matches!(args.command, Some(Command::ConfigurePi)) {
        // Everything comes from the proxy's own config: where it listens, and
        // the models it serves.
        let path = config_path(args.config)?;
        let config = config::load(&path)?;
        let api_key = if config.mode == config::Mode::Host {
            access::ensure(&path, &[])?.local
        } else {
            // Loopback mode ignores the client key; Pi needs one to offer models.
            "hey-proxy".to_owned()
        };
        return rollout::configure_pi(&config, &api_key, None);
    }
    let path = config_path(args.config)?;
    // Fingerprint before loading so an edit racing startup is picked up by the first request.
    let fingerprint = config::fingerprint(&path);
    let config = if matches!(args.command, Some(Command::Rollout { .. })) {
        config::load(&path)?
    } else {
        config::load_or_create(&path)?
    };
    if let Some(Command::ClaudeLogin { no_browser }) = &args.command {
        return claude_auth::login(&config, &path, *no_browser).await;
    }
    if let Some(Command::CodexLogin {
        no_browser,
        device_code,
        import_codex_home,
    }) = &args.command
    {
        return codex_auth::login(
            &config,
            &path,
            codex_auth::LoginOptions {
                no_browser: *no_browser,
                device_code: *device_code,
                import_codex_home: *import_codex_home,
            },
        )
        .await;
    }
    if matches!(args.command, Some(Command::EncryptConfig)) {
        config::protect(&path)?;
        println!("Config credentials encrypted");
        return Ok(());
    }
    if matches!(args.command, Some(Command::ExportConfig)) {
        println!("{}", serde_json::to_string(&config)?);
        return Ok(());
    }
    if let Some(Command::Rollout { host }) = args.command {
        return rollout::run(&config, &host).await;
    }
    if matches!(args.command, Some(Command::CheckCredentials)) {
        rollout::check_credentials(&config).await?;
        if let Some(provider) = &config.claude {
            claude_auth::TokenManager::default()
                .token(
                    &provider.credentials_path(Some(&path))?,
                    &proxy::build_client(&config)?,
                )
                .await?;
        }
        if let Some(provider) = &config.codex {
            codex_auth::TokenManager::default()
                .token(
                    &provider.credentials_path(Some(&path))?,
                    &proxy::build_client(&config)?,
                    &provider.token_url(),
                )
                .await?;
        }
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
    let config = config::protect(&path)?;
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
    #[cfg(target_os = "macos")]
    let mut keep_awake = if config.mode == config::Mode::Host {
        std::process::Command::new("/usr/bin/caffeinate")
            .args(["-i", "-s", "-w", &std::process::id().to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()
    } else {
        None
    };
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
    #[cfg(target_os = "macos")]
    if let Some(mut child) = keep_awake.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    Ok(())
}
