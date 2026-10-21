use super::*;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

#[derive(Default)]
pub(super) struct Cache {
    identity: Option<[u8; 32]>,
    retry_at: Option<Instant>,
    updated_at: Option<u64>,
    data: Option<Value>,
    error: Option<String>,
}
impl Cache {
    fn value(&self) -> Value {
        json!({
            "state": if self.error.is_some() {
                if self.data.is_some() { "stale" } else { "error" }
            } else {
                "ok"
            },
            "updated_at": self.updated_at,
            "data": self.data,
            "error": self.error,
            "retry_after_seconds": self.retry_at.map(|t| t.saturating_duration_since(Instant::now()).as_secs())
        })
    }
}

fn reply(value: Value) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], axum::Json(value)).into_response()
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|n| n.is_finite() && *n >= 0.0)
}

fn bounded(value: &Value) -> Option<String> {
    value.as_str().map(|s| s.chars().take(160).collect())
}

fn reset_timestamp(window: &Value) -> Option<String> {
    for key in ["resets_at", "reset_at", "resetsAt", "resetAt"] {
        if let Some(raw) = window.get(key) {
            if let Some(secs) = raw
                .as_u64()
                .or_else(|| raw.as_i64().and_then(|n| u64::try_from(n).ok()))
                .filter(|&s| s > 0 && s < 253_402_300_800)
            {
                return Some(hey_proxy::usage::format_unix_iso8601(secs));
            }
            if let Some(text) = bounded(raw).filter(|s| !s.trim().is_empty()) {
                return Some(text);
            }
        }
    }
    None
}

fn window_seconds(window: &Value) -> Option<u64> {
    window["limit_window_seconds"]
        .as_u64()
        .or_else(|| window["limitWindowSeconds"].as_u64())
        .filter(|&s| s > 0)
}

fn slug(input: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_owned()
}

fn push_window(
    windows: &mut Vec<Value>,
    id: &str,
    label: &str,
    group: Option<&str>,
    snapshot: &Value,
) {
    if !snapshot.is_object() {
        return;
    }
    let used = number(&snapshot["used_percent"])
        .or_else(|| number(&snapshot["usedPercent"]))
        .or_else(|| number(&snapshot["utilization"]));
    let remaining = used.map(|u| (100.0 - u).max(0.0));
    let mut entry = json!({
        "id": id,
        "label": label,
        "used_percent": used,
        "remaining_percent": remaining,
        "resets_at": reset_timestamp(snapshot),
    });
    if let Some(group) = group {
        entry["group"] = json!(group);
    }
    windows.push(entry);
}

pub(crate) fn normalize(value: &Value) -> Result<Value> {
    ensure!(value.is_object(), "Codex usage response is invalid");
    let mut windows = Vec::new();
    let rate_limit = &value["rate_limit"];
    if rate_limit.is_object() {
        let primary = &rate_limit["primary_window"];
        let secondary = &rate_limit["secondary_window"];
        // Normalize session (300m / 18000s) vs weekly (10080m / 604800s) lanes like CodexBar.
        let primary_is_weekly = window_seconds(primary) == Some(604_800);
        let secondary_is_session = window_seconds(secondary) == Some(18_000);
        let (session, weekly) =
            if primary_is_weekly && (secondary_is_session || secondary.is_object()) {
                (secondary, primary)
            } else if primary_is_weekly && !secondary.is_object() {
                (&Value::Null, primary)
            } else if secondary_is_session && !primary.is_object() {
                (secondary, &Value::Null)
            } else {
                (primary, secondary)
            };
        push_window(
            &mut windows,
            "five_hour",
            "Session · 5 hours",
            None,
            session,
        );
        push_window(
            &mut windows,
            "seven_day",
            "Weekly · all models",
            None,
            weekly,
        );
    }

    if let Some(additional) = value["additional_rate_limits"].as_array() {
        for (index, entry) in additional.iter().take(64).enumerate() {
            if !entry.is_object() {
                continue;
            }
            let name = bounded(&entry["limit_name"])
                .or_else(|| bounded(&entry["metered_feature"]))
                .unwrap_or_else(|| format!("Codex limit {}", index + 1));
            let feature = bounded(&entry["metered_feature"]).unwrap_or_else(|| name.clone());
            let is_spark = name.to_ascii_lowercase().contains("spark")
                || feature.to_ascii_lowercase().contains("spark");
            let rl = &entry["rate_limit"];
            if is_spark {
                if rl["primary_window"].is_object() {
                    push_window(
                        &mut windows,
                        "codex-spark",
                        "Codex Spark · 5 hours",
                        Some("model"),
                        &rl["primary_window"],
                    );
                }
                if rl["secondary_window"].is_object() {
                    push_window(
                        &mut windows,
                        "codex-spark-weekly",
                        "Codex Spark · Weekly",
                        Some("model"),
                        &rl["secondary_window"],
                    );
                }
            } else {
                let target = if rl["primary_window"].is_object() {
                    &rl["primary_window"]
                } else {
                    &rl["secondary_window"]
                };
                if target.is_object() {
                    let s = slug(&feature);
                    let id = if s.is_empty() {
                        format!("codex_extra_{index}")
                    } else {
                        format!("codex-{s}")
                    };
                    push_window(&mut windows, &id, &name, Some("model"), target);
                }
            }
        }
    }

    let individual = [&value["individual_limit"], &rate_limit["individual_limit"]]
        .into_iter()
        .find(|v| v.is_object())
        .or_else(|| {
            value["spend_control"]["individual_limit"]
                .is_object()
                .then_some(&value["spend_control"]["individual_limit"])
        })
        .or_else(|| {
            value["spendControl"]["individualLimit"]
                .is_object()
                .then_some(&value["spendControl"]["individualLimit"])
        });
    let credits = &value["credits"];
    let extra = if individual.is_some() || credits.is_object() {
        let ind = individual.unwrap_or(&Value::Null);
        let limit = number(&ind["limit"]);
        let rem_pct_raw =
            number(&ind["remaining_percent"]).or_else(|| number(&ind["remainingPercent"]));
        let used = number(&ind["used"]).or_else(|| {
            limit
                .zip(rem_pct_raw)
                .map(|(l, rp)| l * (100.0 - rp.clamp(0.0, 100.0)) / 100.0)
        });
        let used_percent = used
            .zip(limit)
            .and_then(|(u, l)| (l > 0.0).then(|| (u / l * 100.0).max(0.0)))
            .or_else(|| rem_pct_raw.map(|rp| (100.0 - rp).max(0.0)));
        let remaining_percent = rem_pct_raw.or_else(|| used_percent.map(|u| (100.0 - u).max(0.0)));
        let balance = number(&credits["balance"]);
        let has_credits = credits["has_credits"].as_bool();
        let enabled = if limit.is_some_and(|l| l > 0.0)
            || has_credits == Some(true)
            || balance.is_some_and(|b| b > 0.0)
        {
            Some(true)
        } else if has_credits == Some(false) {
            Some(false)
        } else {
            None
        };
        let spend = if limit.is_some() || used.is_some() || balance.is_some() {
            let remaining = used.zip(limit).map(|(u, l)| (l - u).max(0.0)).or(balance);
            let over_limit = used.zip(limit).map(|(u, l)| (u - l).max(0.0));
            Some(hey_proxy::usage::SpendLimit {
                currency: "Credits".into(),
                period: "monthly".into(),
                used: used.or_else(|| balance.map(|_| 0.0)),
                limit,
                remaining,
                over_limit,
                resets_at: reset_timestamp(ind),
            })
        } else {
            None
        };
        Some(json!({
            "enabled": enabled,
            "used_percent": used_percent,
            "remaining_percent": remaining_percent,
            "spend": spend,
        }))
    } else {
        None
    };

    ensure!(
        !windows.is_empty() || extra.is_some(),
        "Codex did not report any subscription limits"
    );
    Ok(json!({
        "windows": windows,
        "extra_usage": extra,
    }))
}

fn retry_delay(headers: &HeaderMap) -> Duration {
    let raw = headers
        .get(header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok());
    let seconds = raw
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| {
            raw.and_then(|v| httpdate::parse_http_date(v).ok())
                .map(|t| {
                    t.duration_since(SystemTime::now())
                        .unwrap_or_default()
                        .as_secs()
                })
        })
        .unwrap_or(60);
    Duration::from_secs(seconds.clamp(30, 86400))
}

pub(in crate::proxy) async fn usage(
    State(service): State<Arc<Service>>,
    request: Request,
) -> Response {
    let proxy = service.snapshot();
    if proxy.config.mode == Mode::Client {
        return super::super::forward(State(service), request).await;
    }
    reply(reading(&proxy).await)
}

pub(in crate::proxy) async fn reading(proxy: &Proxy) -> Value {
    let service = &proxy.service;
    let default_provider;
    let provider = if let Some(configured) = &proxy.config.codex {
        configured
    } else if service
        .source
        .as_deref()
        .is_some_and(|p| p.with_extension("codex.json").exists())
    {
        default_provider = ProviderConfig::default();
        &default_provider
    } else {
        return json!({"state":"disabled"});
    };
    let path = match service
        .codex
        .paths
        .resolve(provider, service.source.as_deref())
        .await
    {
        Ok(path) => path,
        Err(_) => {
            return json!({"state":"error","error":"Codex credential store is not configured"});
        }
    };
    let token_url = provider.token_url();
    let mut creds = match service
        .codex
        .tokens
        .credentials(&path, &proxy.client, &token_url)
        .await
    {
        Ok(creds) => creds,
        Err(error) => return json!({"state":"error","error":error.to_string()}),
    };
    let identity: [u8; 32] = Sha256::digest(
        format!(
            "{}\0{}\0{}\0{}",
            path.display(),
            provider.upstream_url,
            creds.access_token,
            creds.account_id.as_deref().unwrap_or("")
        )
        .as_bytes(),
    )
    .into();
    let mut cache = service.codex.usage.lock().await;
    if cache.identity != Some(identity) {
        *cache = Cache {
            identity: Some(identity),
            ..Default::default()
        };
    }
    if cache.retry_at.is_some_and(|t| t > Instant::now()) {
        return cache.value();
    }
    let mut delay = Duration::from_secs(provider.usage_cache_seconds);
    let usage_url = provider.usage_url();
    let result = async {
        for attempt in 0..2 {
            let mut request = proxy
                .client
                .get(&usage_url)
                .timeout(Duration::from_secs(20))
                .bearer_auth(&creds.access_token)
                .header("user-agent", "hey-proxy/0.1.0")
                .header("accept", "application/json");
            if let Some(account_id) = creds.account_id.as_deref() {
                request = request.header("ChatGPT-Account-Id", account_id);
            }
            let mut response = request
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("Cannot reach Codex usage endpoint"))?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && attempt == 0 {
                creds = service
                    .codex
                    .tokens
                    .rejected(&path, &proxy.client, &token_url, creds.access_token)
                    .await?;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS {
                delay = delay.max(retry_delay(response.headers()));
                anyhow::bail!("Codex usage is rate limited; waiting before retrying");
            }
            if matches!(status.as_u16(), 401 | 403) {
                anyhow::bail!(
                    "Codex usage authorization rejected; sign in again with hey-proxy codex-login"
                );
            }
            ensure!(
                status.is_success(),
                "Codex usage unavailable (HTTP {})",
                status.as_u16()
            );
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| anyhow::anyhow!("Cannot read Codex usage response"))?
            {
                ensure!(
                    bytes.len() + chunk.len() <= 1024 * 1024,
                    "Codex usage response is too large"
                );
                bytes.extend_from_slice(&chunk);
            }
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("Codex usage response is invalid"))?;
            return normalize(&value);
        }
        anyhow::bail!(
            "Codex usage authorization rejected; sign in again with hey-proxy codex-login"
        )
    }
    .await;
    cache.retry_at = Some(Instant::now() + delay);
    match result {
        Ok(data) => {
            cache.data = Some(data);
            cache.updated_at = Some(crate::codex_auth::now());
            cache.error = None;
        }
        Err(error) => {
            cache.error = Some(error.to_string());
        }
    }
    cache.value()
}
