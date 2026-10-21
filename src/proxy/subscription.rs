//! On-demand management API. No startup hooks, inference accounting, or model-path locks.
use super::*;
use hey_proxy::usage::{Account, AccountUsage, Accounts, Reading, Recommendation, SCHEMA_VERSION};
use serde::{Serialize, de::DeserializeOwned};
use std::path::Path;

fn reply<T: Serialize>(value: T) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], axum::Json(value)).into_response()
}
fn failure(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(json!({"error":{"code":code,"message":message}})),
    )
        .into_response()
}

pub(crate) fn configured_accounts(config: &Config, source: Option<&Path>) -> Vec<Account> {
    let mut list = Vec::new();
    if claude::is_enabled(config, source) {
        list.push(Account {
            provider: "claude".into(),
            id: "default".into(),
        });
    }
    if codex::is_enabled(config, source) {
        list.push(Account {
            provider: "codex".into(),
            id: "default".into(),
        });
    }
    list
}

pub(super) async fn accounts(State(service): State<Arc<Service>>) -> Response {
    let proxy = service.snapshot();
    if proxy.config.mode == Mode::Client {
        return relay::<Accounts>(&proxy, "/usage/v1/accounts").await;
    }
    reply(Accounts {
        schema_version: SCHEMA_VERSION,
        accounts: configured_accounts(&proxy.config, proxy.service.source.as_deref()),
    })
}

pub(crate) async fn account_usage(
    proxy: &Proxy,
    provider: &str,
    account: &str,
) -> std::result::Result<AccountUsage, (StatusCode, &'static str, &'static str)> {
    if !matches!(provider, "claude" | "codex") {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            "unsupported_provider",
            "Subscription usage is not implemented for this provider",
        ));
    }
    if account != "default" {
        return Err((
            StatusCode::NOT_FOUND,
            "unknown_account",
            "Unknown subscription account",
        ));
    }
    let raw = match provider {
        "claude" => claude::reading(proxy).await,
        "codex" => codex::reading(proxy).await,
        _ => unreachable!(),
    };
    let reading: Reading = serde_json::from_value(raw).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_reading",
            "Cannot normalize subscription usage",
        )
    })?;
    let usage = AccountUsage {
        schema_version: SCHEMA_VERSION,
        account: Account {
            provider: provider.to_owned(),
            id: account.to_owned(),
        },
        reading,
    };
    if let Some(tracker) = crate::spend::global_tracker(proxy.service.source.as_deref()) {
        tracker.record_subscription(&usage);
    }
    Ok(usage)
}

pub(super) async fn usage(
    State(service): State<Arc<Service>>,
    axum::extract::Path((provider, account)): axum::extract::Path<(String, String)>,
) -> Response {
    if [&provider, &account].iter().any(|id| {
        id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    }) {
        return failure(
            StatusCode::BAD_REQUEST,
            "invalid_identifier",
            "Invalid provider or account ID",
        );
    }
    let proxy = service.snapshot();
    // Dispatch on the host so a relay can query providers added by a newer host.
    if proxy.config.mode == Mode::Client {
        return relay::<AccountUsage>(&proxy, &format!("/usage/v1/{provider}/{account}")).await;
    }
    match account_usage(&proxy, &provider, &account).await {
        Ok(usage) => reply(usage),
        Err((status, code, message)) => failure(status, code, message),
    }
}

pub(crate) async fn evaluate_recommendation(proxy: &Proxy) -> Recommendation {
    let source = proxy.service.source.as_deref();
    let claude_enabled = claude::is_enabled(&proxy.config, source);
    let codex_enabled = codex::is_enabled(&proxy.config, source);
    let (claude_res, codex_res) = tokio::join!(
        async {
            if claude_enabled {
                account_usage(proxy, "claude", "default").await.ok()
            } else {
                None
            }
        },
        async {
            if codex_enabled {
                account_usage(proxy, "codex", "default").await.ok()
            } else {
                None
            }
        }
    );
    let mut usages = Vec::new();
    if let Some(u) = claude_res {
        usages.push(u);
    }
    if let Some(u) = codex_res {
        usages.push(u);
    }
    hey_proxy::usage::recommend(&usages, crate::claude_auth::now())
}

pub(super) async fn recommend(State(service): State<Arc<Service>>) -> Response {
    let proxy = service.snapshot();
    if proxy.config.mode == Mode::Client {
        return relay::<Recommendation>(&proxy, "/usage/v1/recommend").await;
    }
    reply(evaluate_recommendation(&proxy).await)
}

// Independent of inference forwarding: no retries, body inspection or request-log writes.
// Deserialize through the public schema to retain the allowlist across client relays.
async fn relay<T: DeserializeOwned + Serialize>(proxy: &Proxy, path: &str) -> Response {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let source = proxy
            .config
            .api_keys
            .get(&proxy.config.default.api_key)
            .ok_or_else(|| anyhow::anyhow!("Host credential is missing"))?;
        let token = proxy
            .service
            .credentials
            .resolve(
                source,
                Duration::from_secs(proxy.config.credential_cache_seconds),
            )
            .await?;
        let mut response = proxy
            .client
            .get(format!(
                "{}{path}",
                proxy.config.upstream_url.trim_end_matches('/')
            ))
            .bearer_auth(token.to_str()?)
            .header("accept", "application/json")
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Ok::<_, anyhow::Error>(Err(status));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                bytes.len() + chunk.len() <= 1024 * 1024,
                "Host usage response is too large"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(Ok(serde_json::from_slice::<T>(&bytes)?))
    })
    .await;
    match result {
        Ok(Ok(Ok(value))) => reply(value),
        Ok(Ok(Err(
            status
            @ (StatusCode::NOT_FOUND | StatusCode::NOT_IMPLEMENTED | StatusCode::BAD_REQUEST),
        ))) => failure(
            status,
            "host_usage_rejected",
            "Host does not support this usage provider, account or endpoint",
        ),
        _ => failure(
            StatusCode::BAD_GATEWAY,
            "host_usage_unavailable",
            "Host usage unavailable; check the host connection and access key",
        ),
    }
}

pub(super) async fn spend(
    State(service): State<Arc<Service>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let proxy = service.snapshot();
    let db_path = crate::spend::default_db_path(proxy.service.source.as_deref());
    let days = params
        .get("days")
        .and_then(|d| d.parse::<i64>().ok())
        .filter(|d| *d > 0);
    let since_ms = days.map(|d| crate::spend::now_ms() - d * 86_400 * 1000);
    let label = days
        .map(|d| format!("Last {d}d"))
        .unwrap_or_else(|| "All time".to_string());
    let _ = crate::spend::sync_local_sources(&db_path, since_ms);
    let mut live_usages = Vec::new();
    for prov in ["codex", "claude"] {
        if let Ok(u) = account_usage(&proxy, prov, "default").await {
            live_usages.push(u);
        }
    }
    match crate::spend::generate_spend_report(&db_path, since_ms, &label, &live_usages) {
        Ok(report) => reply(report),
        Err(err) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "spend_report_failed",
            &err.to_string(),
        ),
    }
}
