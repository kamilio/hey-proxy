use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use hey_proxy::usage::{
    AccountUsage, Accounts, Client, Error as SdkError, Recommendation, SCHEMA_VERSION, State,
    format_duration_short,
};
use std::{
    fmt::Write,
    path::PathBuf,
    time::{Duration, UNIX_EPOCH},
};

#[derive(ClapArgs)]
pub(crate) struct Args {
    /// Provider ID (`claude` or `codex`); defaults to all configured providers
    #[arg(long)]
    provider: Option<String>,
    /// Proxy-local account alias
    #[arg(long, default_value = "default")]
    account: String,
    /// List configured accounts without fetching provider usage
    #[arg(long, conflicts_with_all = ["provider", "account", "recommend"])]
    accounts: bool,
    /// Recommend the best provider (`codex` or `claude`) based on earliest expiring usage and remaining quota
    #[arg(long, conflicts_with_all = ["provider", "account", "accounts", "spend"])]
    recommend: bool,
    /// Show API-equivalent spend by provider and model and subscription yield
    #[arg(long, conflicts_with_all = ["account", "accounts", "recommend"])]
    spend: bool,
    /// Print only the recommended provider ID (`codex` or `claude`) when recommending
    #[arg(long, short = 'q', alias = "quiet")]
    provider_only: bool,
    /// Print the versioned API response as JSON
    #[arg(long)]
    json: bool,
    /// Proxy root or /v1 URL; otherwise use the local address from --config
    #[arg(long)]
    base_url: Option<String>,
    /// Environment variable containing a proxy access key (never a provider OAuth token)
    #[arg(long, default_value = "HEY_PROXY_TOKEN")]
    token_env: String,
    /// Total request timeout, including the response body
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=300))]
    timeout_seconds: u64,
}

#[derive(ClapArgs)]
pub(crate) struct RecommendArgs {
    /// Print only the recommended provider ID (`codex` or `claude`)
    #[arg(long, short = 'q', visible_alias = "quiet", visible_short_alias = 'p')]
    pub provider_only: bool,
    /// Print the versioned recommendation response as JSON
    #[arg(long)]
    pub json: bool,
    /// Proxy root or /v1 URL; otherwise use the local address from --config
    #[arg(long)]
    pub base_url: Option<String>,
    /// Environment variable containing a proxy access key (never a provider OAuth token)
    #[arg(long, default_value = "HEY_PROXY_TOKEN")]
    pub token_env: String,
    /// Total request timeout, including the response body
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=300))]
    pub timeout_seconds: u64,
}

#[derive(ClapArgs)]
pub(crate) struct SpendArgs {
    /// Time window in days (default: 7 days to match weekly subscription cycle; use --all for all time)
    #[arg(long)]
    pub days: Option<u64>,
    /// Time window in hours (e.g. --hours 5 for the 5-hour session window or --hours 24)
    #[arg(long, conflicts_with = "days")]
    pub hours: Option<u64>,
    /// Include all recorded history instead of defaulting to the last 7 days
    #[arg(long, conflicts_with_all = ["days", "hours"])]
    pub all: bool,
    /// Filter breakdown to a specific provider (`codex`, `claude`, `gemini`, `openai`)
    #[arg(long)]
    pub provider: Option<String>,
    /// Skip incremental ingestion of local Codex/Claude CLI session JSONL logs
    #[arg(long)]
    pub no_sync: bool,
    /// Custom path to the spend SQLite database (default: ~/.hey-proxy/spend.sqlite3)
    #[arg(long)]
    pub db: Option<PathBuf>,
    /// Print the versioned spend & subscription yield report as JSON
    #[arg(long)]
    pub json: bool,
}

struct Target {
    client: Client,
    local_fallback: Option<(crate::config::Config, PathBuf)>,
}

fn build_target(
    base_url: Option<String>,
    token_env: &str,
    timeout_seconds: u64,
    config_path: Option<PathBuf>,
) -> Result<Target> {
    let supplied = match std::env::var(token_env) {
        Ok(token) => Some(token),
        Err(std::env::VarError::NotPresent) if token_env == "HEY_PROXY_TOKEN" => None,
        _ => bail!("Cannot read the requested proxy token environment variable"),
    };
    let (base, token, local_fallback) = if let Some(base) = base_url {
        // Never send a locally stored access key to an explicitly overridden host.
        (base, supplied, None)
    } else {
        let has_explicit_config = config_path.is_some();
        let path = crate::config_path(config_path)?;
        let config = if path.exists() || has_explicit_config {
            crate::config::load(&path)?
        } else {
            crate::config::Config::default()
        };
        let token = match supplied {
            Some(token) => Some(token),
            None if config.mode == crate::config::Mode::Host => {
                Some(crate::access::read(&path)?.local)
            }
            None => None,
        };
        let fallback = (config.mode != crate::config::Mode::Client).then(|| (config.clone(), path));
        (
            format!("http://{}", config.local_address()),
            token,
            fallback,
        )
    };
    let client =
        Client::new(&base, token.as_deref())?.with_timeout(Duration::from_secs(timeout_seconds))?;
    Ok(Target {
        client,
        local_fallback,
    })
}

async fn discover_accounts(target: &Target) -> Result<Accounts> {
    let mut accounts = match target.client.accounts().await {
        Ok(accounts) => accounts,
        Err(err) if should_fallback_locally(&err) && target.local_fallback.is_some() => {
            let (config, path) = target.local_fallback.as_ref().unwrap();
            Accounts {
                schema_version: SCHEMA_VERSION,
                accounts: crate::proxy::subscription::configured_accounts(
                    config,
                    Some(path.as_path()),
                ),
            }
        }
        Err(err) => return Err(err.into()),
    };
    if let Some((config, path)) = target.local_fallback.as_ref() {
        for local_account in
            crate::proxy::subscription::configured_accounts(config, Some(path.as_path()))
        {
            if !accounts.accounts.iter().any(|existing| {
                existing.provider == local_account.provider && existing.id == local_account.id
            }) {
                accounts.accounts.push(local_account);
            }
        }
    }
    Ok(accounts)
}

fn should_fallback_locally(error: &SdkError) -> bool {
    matches!(error, SdkError::Transport | SdkError::Http(404 | 501))
}

pub(crate) async fn run_spend(args: SpendArgs, config_path: Option<PathBuf>) -> Result<()> {
    let resolved_cfg = crate::config_path(config_path.clone()).ok();
    let db_path = args
        .db
        .clone()
        .unwrap_or_else(|| crate::spend::default_db_path(resolved_cfg.as_deref()));

    let now = crate::spend::now_ms();
    let (since_ms, window_label) = if let Some(hours) = args.hours {
        (
            Some(now - (hours as i64) * 3600 * 1000),
            format!("Last {hours}h"),
        )
    } else if let Some(days) = args.days {
        (
            Some(now - (days as i64) * 86_400 * 1000),
            format!("Last {days}d"),
        )
    } else {
        (None, "All-time".to_string())
    };

    if let Some(tracker) = crate::spend::global_tracker(resolved_cfg.as_deref()) {
        tracker.flush().await;
    }
    if !args.no_sync {
        let _ = crate::spend::sync_local_sources(&db_path, None);
    }

    let mut live_usages = Vec::new();
    if let Some(cfg_file) = resolved_cfg.as_ref() {
        let cfg = if cfg_file.exists() {
            crate::config::load(cfg_file).unwrap_or_default()
        } else {
            crate::config::Config::default()
        };
        if let Ok(snapshot) = crate::proxy::local_snapshot(cfg, Some(cfg_file.clone())) {
            let (codex_res, claude_res) = tokio::join!(
                crate::proxy::subscription::account_usage(&snapshot, "codex", "default"),
                crate::proxy::subscription::account_usage(&snapshot, "claude", "default"),
            );
            for u in [codex_res, claude_res].into_iter().flatten() {
                if u.reading.state == State::Ok {
                    live_usages.push(u);
                }
            }
        }
    }

    let mut report =
        crate::spend::generate_spend_report(&db_path, since_ms, &window_label, &live_usages)?;
    if let Some(prov_filter) = args.provider.as_deref() {
        report
            .by_provider
            .retain(|p| p.provider.eq_ignore_ascii_case(prov_filter));
        report
            .by_model
            .retain(|m| m.provider.eq_ignore_ascii_case(prov_filter));
        report
            .subscription_yield
            .retain(|s| s.provider.eq_ignore_ascii_case(prov_filter));
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("Cannot encode spend report")?
        );
    } else {
        print!("{}", crate::spend::render_spend_report(&report));
    }
    Ok(())
}

pub(crate) async fn run_recommend(args: RecommendArgs, config_path: Option<PathBuf>) -> Result<()> {
    let target = build_target(
        args.base_url,
        &args.token_env,
        args.timeout_seconds,
        config_path,
    )?;
    let recommendation = match target.client.recommend().await {
        Ok(rec) => rec,
        Err(err) if should_fallback_locally(&err) && target.local_fallback.is_some() => {
            let (config, path) = target.local_fallback.unwrap();
            let snapshot = crate::proxy::local_snapshot(config, Some(path))?;
            crate::proxy::subscription::evaluate_recommendation(&snapshot).await
        }
        Err(err) => return Err(err.into()),
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&recommendation)
                .context("Cannot encode recommendation")?
        );
    } else if args.provider_only {
        if let Some(provider) = &recommendation.recommended_provider {
            println!("{}", plain(provider));
        }
    } else {
        print!("{}", render_recommendation(&recommendation));
    }
    if recommendation.recommended_provider.is_none() {
        bail!("{}", plain(&recommendation.summary));
    }
    Ok(())
}

pub(crate) async fn run(args: Args, config_path: Option<PathBuf>) -> Result<()> {
    if args.spend {
        return run_spend(
            SpendArgs {
                days: None,
                hours: None,
                all: false,
                provider: args.provider,
                no_sync: false,
                db: None,
                json: args.json,
            },
            config_path,
        )
        .await;
    }
    if args.recommend {
        return run_recommend(
            RecommendArgs {
                provider_only: args.provider_only,
                json: args.json,
                base_url: args.base_url,
                token_env: args.token_env,
                timeout_seconds: args.timeout_seconds,
            },
            config_path,
        )
        .await;
    }
    let target = build_target(
        args.base_url,
        &args.token_env,
        args.timeout_seconds,
        config_path,
    )?;
    if args.accounts {
        let accounts = discover_accounts(&target).await?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&accounts)?);
        } else if accounts.accounts.is_empty() {
            println!("No subscription accounts configured.");
        } else {
            for account in accounts.accounts {
                println!("{}/{}", plain(&account.provider), plain(&account.id));
            }
        }
        return Ok(());
    }
    let providers: Vec<String> = if let Some(provider) = args.provider {
        vec![provider]
    } else {
        let accounts = discover_accounts(&target).await.unwrap_or(Accounts {
            schema_version: SCHEMA_VERSION,
            accounts: Vec::new(),
        });
        let configured: Vec<String> = accounts
            .accounts
            .into_iter()
            .filter(|a| a.id == args.account)
            .map(|a| a.provider)
            .collect();
        if configured.is_empty() {
            vec!["claude".to_string()]
        } else {
            configured
        }
    };
    let mut usages = Vec::new();
    for provider in &providers {
        let usage = match target.client.usage(provider, &args.account).await {
            Ok(usage)
                if usage.reading.state != State::Disabled || target.local_fallback.is_none() =>
            {
                usage
            }
            Ok(remote_usage) => {
                if let Some((config, path)) = target.local_fallback.as_ref() {
                    let snapshot =
                        crate::proxy::local_snapshot(config.clone(), Some(path.clone()))?;
                    match crate::proxy::subscription::account_usage(
                        &snapshot,
                        provider,
                        &args.account,
                    )
                    .await
                    {
                        Ok(usage) => usage,
                        Err(_) => remote_usage,
                    }
                } else {
                    remote_usage
                }
            }
            Err(err) if should_fallback_locally(&err) && target.local_fallback.is_some() => {
                let (config, path) = target.local_fallback.as_ref().unwrap();
                let snapshot = crate::proxy::local_snapshot(config.clone(), Some(path.clone()))?;
                match crate::proxy::subscription::account_usage(&snapshot, provider, &args.account)
                    .await
                {
                    Ok(usage) => usage,
                    Err((status, _, _)) => return Err(SdkError::Http(status.as_u16()).into()),
                }
            }
            Err(err) => return Err(err.into()),
        };
        usages.push(usage);
    }
    if args.json {
        if usages.len() == 1 {
            println!(
                "{}",
                serde_json::to_string_pretty(&usages[0]).context("Cannot encode usage")?
            );
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&usages).context("Cannot encode usage")?
            );
        }
    } else {
        for (idx, usage) in usages.iter().enumerate() {
            if idx > 0 {
                println!();
            }
            print!("{}", render(usage));
        }
    }
    if usages.iter().all(|u| u.reading.state != State::Ok) {
        bail!("No fresh subscription reading; see the reported state and retry delay");
    }
    Ok(())
}

fn plain(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(256)
        .collect()
}
fn number(value: Option<f64>, suffix: &str) -> String {
    value
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| format!("{v:.1}{suffix}"))
        .unwrap_or_else(|| "unknown".into())
}
fn amount(value: Option<f64>, currency: &str) -> String {
    value
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| format!("{} {v:.2}", plain(currency)))
        .unwrap_or_else(|| "unknown".into())
}

pub(crate) fn render_recommendation(rec: &Recommendation) -> String {
    let mut out = String::new();
    match &rec.recommended_provider {
        Some(provider) => {
            let _ = writeln!(out, "Recommended: {}", plain(provider));
        }
        None => {
            let _ = writeln!(out, "Recommended: none");
        }
    }
    let _ = writeln!(out, "Reason: {}", plain(&rec.summary));
    if !rec.candidates.is_empty() {
        let _ = writeln!(out, "Candidates:");
        for (idx, candidate) in rec.candidates.iter().enumerate() {
            let marker = if idx == 0 && candidate.available {
                "*"
            } else {
                " "
            };
            let status = if candidate.exhausted && !candidate.available {
                "OUT OF QUOTA".to_owned()
            } else if candidate.using_extra_usage {
                "EXTRA USAGE".to_owned()
            } else if candidate.available {
                "AVAILABLE".to_owned()
            } else {
                format!("{:?}", candidate.state).to_ascii_uppercase()
            };
            let rem = number(candidate.effective_remaining_percent, "% left");
            let win_detail = if let Some(win) = &candidate.earliest_expiring_window {
                let reset_part = match (win.resets_in_seconds, win.resets_at.as_deref()) {
                    (Some(secs), Some(at)) => {
                        format!("resets in {} ({})", format_duration_short(secs), plain(at))
                    }
                    (Some(secs), None) => format!("resets in {}", format_duration_short(secs)),
                    (None, Some(at)) => format!("resets {}", plain(at)),
                    (None, None) => "reset time unknown".into(),
                };
                format!(
                    " · {}: {} left, {reset_part}",
                    plain(&win.label),
                    number(win.remaining_percent, "%")
                )
            } else if let Some(err) = &candidate.error {
                format!(" · {}", plain(err))
            } else {
                String::new()
            };
            let _ = writeln!(
                out,
                "{marker} {}/{} · {status} · effective {rem}{win_detail}",
                plain(&candidate.account.provider),
                plain(&candidate.account.id)
            );
        }
    }
    out
}

fn render(usage: &AccountUsage) -> String {
    let mut out = String::new();
    let reading = &usage.reading;
    let state = match reading.state {
        State::Ok => "ok",
        State::Stale => "STALE",
        State::Error => "ERROR",
        State::Disabled => "DISABLED",
        State::Unknown => "UNKNOWN",
    };
    let _ = writeln!(
        out,
        "{}/{} · {state}",
        plain(&usage.account.provider),
        plain(&usage.account.id)
    );
    if let Some(at) = reading
        .updated_at
        .and_then(|s| UNIX_EPOCH.checked_add(Duration::from_secs(s)))
    {
        // httpdate supports years through 9999. Guard untrusted server timestamps.
        if reading.updated_at.is_some_and(|s| s < 253_402_300_800) {
            let _ = writeln!(
                out,
                "Last successful reading: {}",
                httpdate::fmt_http_date(at)
            );
        }
    }
    if let Some(data) = &reading.data {
        for window in &data.windows {
            let _ = writeln!(
                out,
                "{}: {} left ({} used) · resets {}",
                plain(&window.label),
                number(window.remaining_percent, "%"),
                number(window.used_percent, "%"),
                window
                    .resets_at
                    .as_deref()
                    .map(plain)
                    .unwrap_or_else(|| "unknown".into())
            );
        }
        if data.windows.is_empty() {
            out.push_str("Subscription quota windows: not reported\n");
        }
        match &data.extra_usage {
            Some(extra) => {
                let _ = writeln!(
                    out,
                    "Extra usage (beyond included subscription): {}",
                    match extra.enabled {
                        Some(true) => "enabled",
                        Some(false) => "disabled",
                        None => "status unknown",
                    }
                );
                let _ = writeln!(
                    out,
                    "Extra budget: {} left ({} used)",
                    number(extra.remaining_percent, "%"),
                    number(extra.used_percent, "%")
                );
                if let Some(spend) = &extra.spend {
                    let _ = writeln!(
                        out,
                        "Provider-reported {} extra spend: {}",
                        plain(&spend.period),
                        amount(spend.used, &spend.currency)
                    );
                    let _ = writeln!(
                        out,
                        "Extra spend cap: {} · remaining: {} · above cap: {}",
                        amount(spend.limit, &spend.currency),
                        amount(spend.remaining, &spend.currency),
                        amount(spend.over_limit, &spend.currency)
                    );
                } else {
                    out.push_str("Extra spend amounts: not reported or units unknown\n");
                }
            }
            None => out.push_str("Extra usage: not reported\n"),
        }
    }
    if let Some(error) = &reading.error {
        let _ = writeln!(out, "Notice: {}", plain(error));
    }
    if let Some(delay) = reading.retry_after_seconds {
        let _ = writeln!(
            out,
            "Next provider refresh in {delay}s (cached until then)."
        );
    }
    out.push_str(
        "Account-wide usage; extra spend is separate from estimated API-equivalent spend.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn cli_defaults_and_account_discovery_parse() {
        assert!(crate::Args::try_parse_from(["hey-proxy", "usage"]).is_ok());
        assert!(crate::Args::try_parse_from(["hey-proxy", "usage", "--provider", "codex"]).is_ok());
        assert!(crate::Args::try_parse_from(["hey-proxy", "usage", "--recommend", "-q"]).is_ok());
        assert!(crate::Args::try_parse_from(["hey-proxy", "recommend"]).is_ok());
        assert!(crate::Args::try_parse_from(["hey-proxy", "recommend", "--provider-only"]).is_ok());
        assert!(crate::Args::try_parse_from(["hey-proxy", "codex-login", "--device-code"]).is_ok());
        assert!(
            crate::Args::try_parse_from(["hey-proxy", "usage", "--accounts", "--json"]).is_ok()
        );
        assert!(
            crate::Args::try_parse_from([
                "hey-proxy",
                "usage",
                "--accounts",
                "--provider",
                "claude"
            ])
            .is_err()
        );
        assert!(
            crate::Args::try_parse_from(["hey-proxy", "usage", "--timeout-seconds", "0"]).is_err()
        );
    }
    #[test]
    fn output_marks_stale_unknown_and_disabled_spend_without_terminal_controls() {
        let reading = serde_json::from_value(serde_json::json!({
            "schema_version":1,"account":{"provider":"claude","id":"default"},"state":"stale",
            "updated_at":1790800000,"retry_after_seconds":60,"error":"wait\u{1b}[31m",
            "data":{"windows":[{"id":"session","label":"Session","remaining_percent":null,"used_percent":null}],
            "extra_usage":{"enabled":false,"spend":{"currency":"USD","period":"monthly","used":12.5,"limit":10,"remaining":0,"over_limit":2.5}}}
        })).unwrap();
        let output = render(&reading);
        assert!(output.contains("STALE"));
        assert!(output.contains("Session: unknown left (unknown used)"));
        assert!(output.contains("subscription): disabled"));
        assert!(output.contains("extra spend: USD 12.50"));
        assert!(output.contains("remaining: USD 0.00 · above cap: USD 2.50"));
        assert!(!output.contains('\u{1b}'));
    }
}
