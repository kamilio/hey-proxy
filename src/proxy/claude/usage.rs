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
        json!({"state":if self.error.is_some() { if self.data.is_some() {"stale"} else {"error"} } else {"ok"},
            "updated_at":self.updated_at,"data":self.data,"error":self.error,
            "retry_after_seconds":self.retry_at.map(|t| t.saturating_duration_since(Instant::now()).as_secs())})
    }
}
fn reply(value: Value) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], axum::Json(value)).into_response()
}
fn percent(value: &Value) -> Option<f64> {
    value.as_f64().filter(|n| n.is_finite() && *n >= 0.0)
}
fn bounded(value: &Value) -> Option<String> {
    value.as_str().map(|s| s.chars().take(160).collect())
}

// Only quota fields leave this endpoint, never arbitrary upstream metadata or account details.
fn normalize(value: &Value) -> Result<Value> {
    ensure!(value.is_object(), "Claude usage response is invalid");
    let mut windows = Vec::new();
    for (key, label) in [
        ("five_hour", "Session · 5 hours"),
        ("seven_day", "Weekly · all models"),
        ("seven_day_sonnet", "Weekly · Sonnet"),
        ("seven_day_opus", "Weekly · Opus"),
        ("seven_day_oauth_apps", "Weekly · OAuth apps"),
        ("seven_day_routines", "Weekly · Routines"),
    ] {
        if value[key].is_object() {
            windows.push(json!({"id":key,"label":label,"used_percent":percent(&value[key]["utilization"]),"resets_at":bounded(&value[key]["resets_at"])}));
        }
    }
    if let Some(limits) = value["limits"].as_array() {
        for (index, limit) in limits.iter().take(64).enumerate() {
            if !limit.is_object() || limit["is_active"] == false {
                continue;
            }
            let label = bounded(&limit["scope"]["model"]["display_name"])
                .or_else(|| bounded(&limit["scope"]["model"]["id"]))
                .or_else(|| bounded(&limit["kind"]))
                .unwrap_or_else(|| "Subscription limit".into());
            windows.push(json!({"id":format!("limit_{index}"),"label":label,"group":bounded(&limit["group"]),"used_percent":percent(&limit["percent"]),"resets_at":bounded(&limit["resets_at"])}));
        }
    }
    for window in &mut windows {
        window["remaining_percent"] =
            json!(percent(&window["used_percent"]).map(|used| (100.0 - used).max(0.0)));
    }
    let extra = &value["extra_usage"];
    let extra = extra.is_object().then(|| json!({"enabled":extra["is_enabled"].as_bool(),"used_percent":percent(&extra["utilization"]),
        "used_credits":percent(&extra["used_credits"]),"monthly_limit":percent(&extra["monthly_limit"]),"currency":bounded(&extra["currency"]),
        "remaining_percent":percent(&extra["utilization"]).map(|used| (100.0 - used).max(0.0)),
        "spend":extra_spend(extra)}));
    ensure!(
        !windows.is_empty() || extra.is_some(),
        "Claude did not report any subscription limits"
    );
    Ok(json!({"windows":windows,"extra_usage":extra}))
}
// OAuth extra_usage uses hundredths of the currency unit, USD when omitted.
// Keep legacy raw fields above; expose explicit major-unit amounts for all clients.
fn extra_spend(extra: &Value) -> Option<hey_proxy::usage::SpendLimit> {
    let currency = match &extra["currency"] {
        Value::Null => "USD".to_owned(),
        Value::String(code) if code.trim().is_empty() => "USD".to_owned(),
        Value::String(code)
            if code.trim().len() == 3 && code.trim().bytes().all(|c| c.is_ascii_alphabetic()) =>
        {
            code.trim().to_ascii_uppercase()
        }
        _ => return None, // Unrecognized units must never silently become dollars.
    };
    let used = percent(&extra["used_credits"]).map(|n| n / 100.0);
    let limit = percent(&extra["monthly_limit"]).map(|n| n / 100.0);
    Some(hey_proxy::usage::SpendLimit {
        currency,
        period: "monthly".into(),
        used,
        limit,
        remaining: used.zip(limit).map(|(used, limit)| (limit - used).max(0.0)),
        over_limit: used.zip(limit).map(|(used, limit)| (used - limit).max(0.0)),
        resets_at: None,
    })
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
    let provider = if let Some(configured) = &proxy.config.claude {
        configured
    } else if service
        .source
        .as_deref()
        .is_some_and(|p| p.with_extension("claude.json").exists())
    {
        default_provider = ProviderConfig::default();
        &default_provider
    } else {
        return json!({"state":"disabled"});
    };
    let path = match service
        .claude
        .paths
        .resolve(provider, service.source.as_deref())
        .await
    {
        Ok(path) => path,
        Err(_) => {
            return json!({"state":"error","error":"Claude credential store is not configured"});
        }
    };
    let token = match service.claude.tokens.token(&path, &proxy.client).await {
        Ok(token) => token,
        Err(error) => return json!({"state":"error","error":error.to_string()}),
    };
    let identity: [u8; 32] = Sha256::digest(
        format!("{}\0{}\0{}", path.display(), provider.upstream_url, token).as_bytes(),
    )
    .into();
    // This mutex coalesces polling across tabs. It never blocks model requests.
    let mut cache = service.claude.usage.lock().await;
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
    let result = async {
        let mut response = proxy.client.get(format!("{}/api/oauth/usage", provider.upstream_url.trim_end_matches('/')))
            .timeout(Duration::from_secs(20)).bearer_auth(token).header("anthropic-beta", OAUTH_BETA)
            .header("user-agent", "hey-proxy/0.1.0").header("accept", "application/json").send().await
            .map_err(|_| anyhow::anyhow!("Cannot reach Claude usage endpoint"))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            delay = delay.max(retry_delay(response.headers()));
            anyhow::bail!("Claude usage is rate limited; waiting before retrying");
        }
        if matches!(status.as_u16(), 401 | 403) {
            anyhow::bail!("Claude usage authorization rejected; sign in again with hey-proxy claude-login (profile scope is required)");
        }
        ensure!(status.is_success(), "Claude usage unavailable (HTTP {})", status.as_u16());
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| anyhow::anyhow!("Cannot read Claude usage response"))? {
            ensure!(bytes.len() + chunk.len() <= 1024 * 1024, "Claude usage response is too large");
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Claude usage response is invalid"))?;
        normalize(&value)
    }.await;
    cache.retry_at = Some(Instant::now() + delay);
    match result {
        Ok(data) => {
            cache.data = Some(data);
            cache.updated_at = Some(crate::claude_auth::now());
            cache.error = None;
        }
        Err(error) => {
            cache.error = Some(error.to_string());
        }
    }
    cache.value()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_and_extra_spend_have_explicit_units_and_preserve_unknowns() {
        let data = normalize(&json!({
            "five_hour":{"utilization":112},"seven_day":{"utilization":null},
            "extra_usage":{"is_enabled":false,"used_credits":1250,"monthly_limit":1000,"utilization":125}
        })).unwrap();
        assert_eq!(data["windows"][0]["remaining_percent"], 0.0);
        assert!(data["windows"][1]["remaining_percent"].is_null());
        assert_eq!(data["extra_usage"]["used_credits"], 1250.0); // Legacy contract.
        assert_eq!(
            data["extra_usage"]["spend"],
            json!({"currency":"USD","period":"monthly","used":12.5,"limit":10.0,"remaining":0.0,"over_limit":2.5,"resets_at":null})
        );
        assert_eq!(data["extra_usage"]["remaining_percent"], 0.0);
        let data = normalize(
            &json!({"extra_usage":{"is_enabled":true,"used_credits":0,"monthly_limit":null}}),
        )
        .unwrap();
        assert_eq!(data["extra_usage"]["spend"]["used"], 0.0);
        assert!(data["extra_usage"]["spend"]["remaining"].is_null());
        assert!(data["extra_usage"]["remaining_percent"].is_null());
        for bad in [json!(-1), json!("1250"), Value::Null] {
            let spend =
                extra_spend(&json!({"used_credits":bad,"monthly_limit":1000,"currency":" eur "}))
                    .unwrap();
            assert_eq!(spend.currency, "EUR");
            assert_eq!(spend.used, None);
            assert_eq!(spend.remaining, None);
        }
        assert!(extra_spend(&json!({"currency":"not money","used_credits":100})).is_none());
        assert!(extra_spend(&json!({"currency":12,"used_credits":100})).is_none());
        let zero_cap = extra_spend(&json!({"used_credits":100,"monthly_limit":0})).unwrap();
        assert_eq!(zero_cap.remaining, Some(0.0));
        assert_eq!(zero_cap.over_limit, Some(1.0));
        let available = extra_spend(&json!({"used_credits":250,"monthly_limit":1000})).unwrap();
        assert_eq!(available.remaining, Some(7.5));
        assert_eq!(available.over_limit, Some(0.0));
    }
}
