use super::*;
use crate::proxy::router_with;
use axum::{Router, extract::Request, routing::any};
use hey_proxy::usage::{Client, State as UsageState};
use std::sync::atomic::{AtomicUsize, Ordering};

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}

#[test]
fn wham_usage_normalizes_session_weekly_spark_and_spend_limits() {
    let data = usage::normalize(&json!({
        "plan_type": "pro",
        "rate_limit": {
            "primary_window": {
                "used_percent": 65,
                "reset_at": 1790803600,
                "limit_window_seconds": 604800
            },
            "secondary_window": {
                "used_percent": 30,
                "reset_at": 1790801800,
                "limit_window_seconds": 18000
            },
            "individual_limit": {
                "limit": 100.0,
                "used": 25.0,
                "remaining_percent": 75.0,
                "reset_at": 1791500000
            }
        },
        "additional_rate_limits": [
            {
                "limit_name": "GPT-5.3-Codex-Spark",
                "metered_feature": "codex-spark",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 15,
                        "reset_at": 1790801800,
                        "limit_window_seconds": 18000
                    }
                }
            }
        ],
        "credits": {
            "has_credits": true,
            "unlimited": false,
            "balance": "12.5"
        }
    }))
    .unwrap();
    let windows = data["windows"].as_array().unwrap();
    // Primary/secondary swapped so 5-hour session is first and 7-day weekly is second.
    assert_eq!(windows[0]["id"], "five_hour");
    assert_eq!(windows[0]["used_percent"], 30.0);
    assert_eq!(windows[0]["remaining_percent"], 70.0);
    assert_eq!(windows[1]["id"], "seven_day");
    assert_eq!(windows[1]["used_percent"], 65.0);
    assert_eq!(windows[1]["remaining_percent"], 35.0);
    assert_eq!(windows[2]["id"], "codex-spark");
    assert_eq!(windows[2]["group"], "model");
    assert_eq!(windows[2]["remaining_percent"], 85.0);
    assert_eq!(data["extra_usage"]["enabled"], true);
    assert_eq!(data["extra_usage"]["spend"]["remaining"], 75.0);
}

#[tokio::test]
async fn codex_usage_and_recommender_work_without_affecting_proxy_routing() {
    let now = crate::codex_auth::now();
    let codex_hits = Arc::new(AtomicUsize::new(0));
    let claude_hits = Arc::new(AtomicUsize::new(0));
    let openai_hits = Arc::new(AtomicUsize::new(0));
    let (codex_counter, claude_counter, openai_counter) =
        (codex_hits.clone(), claude_hits.clone(), openai_hits.clone());
    let (upstream, upstream_task) = serve(Router::new().fallback(any(move |request: Request| {
        let codex_counter = codex_counter.clone();
        let claude_counter = claude_counter.clone();
        let openai_counter = openai_counter.clone();
        async move {
            match request.uri().path() {
                "/backend-api/wham/usage" => {
                    codex_counter.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(
                        request.headers()["authorization"],
                        "Bearer codex-oat-synthetic-old"
                    );
                    assert_eq!(
                        request.headers()["ChatGPT-Account-Id"],
                        "acct_synthetic_123"
                    );
                    axum::Json(json!({
                        "rate_limit": {
                            "primary_window": {
                                "used_percent": 40,
                                "reset_at": now + 1800,
                                "limit_window_seconds": 18000
                            },
                            "secondary_window": {
                                "used_percent": 20,
                                "reset_at": now + 300000,
                                "limit_window_seconds": 604800
                            }
                        }
                    }))
                }
                "/api/oauth/usage" => {
                    claude_counter.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({
                        "five_hour": {
                            "utilization": 25.0,
                            "resets_at": hey_proxy::usage::format_unix_iso8601(now + 7200)
                        },
                        "seven_day": {
                            "utilization": 30.0,
                            "resets_at": hey_proxy::usage::format_unix_iso8601(now + 400000)
                        }
                    }))
                }
                "/v1/responses" => {
                    openai_counter.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(
                        request.headers()["authorization"],
                        "Bearer openai-api-key-only"
                    );
                    axum::Json(json!({
                        "id": "resp_1",
                        "object": "response",
                        "model": "gpt-4.1",
                        "output": []
                    }))
                }
                other => panic!("unexpected path {other}"),
            }
        }
    })))
    .await;

    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.json");
    crate::codex_auth::tests::fixture(&config_path.with_extension("codex.json"), now + 3600);
    crate::claude_auth::tests::fixture(&config_path.with_extension("claude.json"), now + 3600);

    let cfg: Config = serde_json::from_value(json!({
        "listen": "127.0.0.1:0",
        "providers": {
            "openai": {
                "upstream_url": upstream,
                "api_keys": { "default": "openai-api-key-only" }
            },
            "claude": {
                "upstream_url": upstream,
                "routing": false
            },
            "codex": {
                "upstream_url": upstream
            }
        }
    }))
    .unwrap();
    std::fs::write(&config_path, serde_json::to_vec(&cfg).unwrap()).unwrap();

    let (proxy_url, proxy_task) = serve(
        router_with(
            cfg,
            crate::proxy::Options {
                source: Some((
                    config_path.clone(),
                    crate::config::fingerprint(&config_path),
                )),
                ..Default::default()
            },
        )
        .unwrap(),
    )
    .await;

    let sdk = Client::new(&proxy_url, None).unwrap();
    let accounts = sdk.accounts().await.unwrap();
    assert_eq!(accounts.accounts.len(), 2);

    let codex_usage = sdk.usage("codex", "default").await.unwrap();
    assert_eq!(codex_usage.reading.state, UsageState::Ok);
    assert_eq!(
        codex_usage.reading.data.as_ref().unwrap().windows[0].remaining_percent,
        Some(60.0)
    );

    // Codex resets in 1800s (30m) while Claude resets in 7200s (2h); recommender picks codex!
    let rec = sdk.recommend().await.unwrap();
    assert_eq!(rec.recommended_provider.as_deref(), Some("codex"));
    assert_eq!(rec.reason, "earliest_expiring_window");

    // Proxy routing for /v1/responses still uses providers.openai API key, completely unaffected by codex OAuth!
    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/responses"))
        .json(&json!({"model": "gpt-4.1", "input": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(openai_hits.load(Ordering::SeqCst), 1);

    proxy_task.abort();
    upstream_task.abort();
}
