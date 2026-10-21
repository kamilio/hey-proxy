//! Public SDK and CLI contract tests; all credentials and HTTP servers are synthetic.
use axum::{Router, extract::Request, response::IntoResponse, routing::any};
use hey_proxy::usage::{Client, Error};
use serde_json::{Value, json};
use std::time::Duration;

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}
fn reading(state: &str) -> Value {
    json!({"schema_version":1,"account":{"provider":"claude","id":"default"},"state":state,
        "updated_at":1790800000,"retry_after_seconds":60,
        "data":{"windows":[{"id":"five_hour","label":"Session","used_percent":25,"remaining_percent":75}],
            "extra_usage":{"enabled":true,"spend":{"currency":"USD","period":"monthly","used":2.5,"limit":10,"remaining":7.5,"over_limit":0}}}})
}

#[tokio::test]
async fn sdk_rejects_redirects_bad_versions_mismatched_accounts_and_oversized_bodies() {
    for (payload, expected) in [
        (
            json!({"schema_version":2,"accounts":[]}),
            Error::UnsupportedSchema(2),
        ),
        (json!({"schema_version":1}), Error::InvalidResponse),
    ] {
        let (url, task) = serve(Router::new().fallback(any(move || {
            let payload = payload.clone();
            async move { axum::Json(payload) }
        })))
        .await;
        assert_eq!(
            Client::new(&url, None)
                .unwrap()
                .accounts()
                .await
                .unwrap_err(),
            expected
        );
        task.abort();
    }
    let (url, task) = serve(Router::new().fallback(any(|| async {
        (
            axum::http::StatusCode::FOUND,
            [("location", "/target-with-private-token")],
            "PRIVATE BODY",
        )
    })))
    .await;
    let error = Client::new(&url, Some("synthetic-private-token"))
        .unwrap()
        .accounts()
        .await
        .unwrap_err();
    assert_eq!(error, Error::Http(302));
    assert!(!format!("{error:?} {error}").contains("private"));
    task.abort();
    let (url, task) = serve(Router::new().fallback(any(|| async {
        let mut payload = reading("ok");
        payload["account"]["id"] = json!("other");
        axum::Json(payload)
    })))
    .await;
    assert_eq!(
        Client::new(&url, None)
            .unwrap()
            .usage("claude", "default")
            .await
            .unwrap_err(),
        Error::InvalidResponse
    );
    task.abort();
    let (url, task) =
        serve(Router::new().fallback(any(|| async { "x".repeat(1024 * 1024 + 1) }))).await;
    assert_eq!(
        Client::new(&url, None)
            .unwrap()
            .accounts()
            .await
            .unwrap_err(),
        Error::ResponseTooLarge
    );
    task.abort();
}

#[tokio::test]
async fn sdk_timeout_covers_a_stalled_response_body_and_preserves_unknown_states() {
    let (url, task) = serve(Router::new().fallback(any(|| async {
        let stream = async_stream::stream! {
            yield Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"{"));
            std::future::pending::<()>().await;
        };
        axum::body::Body::from_stream(stream).into_response()
    })))
    .await;
    assert_eq!(
        Client::new(&url, None)
            .unwrap()
            .with_timeout(Duration::from_millis(50))
            .unwrap()
            .accounts()
            .await
            .unwrap_err(),
        Error::Transport
    );
    task.abort();
    let parsed: hey_proxy::usage::AccountUsage =
        serde_json::from_value(reading("new_state")).unwrap();
    assert_eq!(parsed.reading.state, hey_proxy::usage::State::Unknown);
    for url in [
        "https://u:SECRET@example.com",
        "http://example.com?token=SECRET",
        "http://example.com/api",
        "file:///tmp/key",
    ] {
        assert!(matches!(Client::new(url, None), Err(Error::InvalidBaseUrl)));
    }
    assert!(matches!(
        Client::new("http://localhost", Some("token\nsecret")),
        Err(Error::InvalidToken)
    ));
    let client = Client::new("http://127.0.0.1:1/v1/", None).unwrap();
    assert_eq!(
        client.usage("claude", "../other").await.unwrap_err(),
        Error::InvalidIdentifier
    );
}

fn command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_hey-proxy"));
    command.env_remove("HEY_PROXY_TOKEN");
    command
}
#[tokio::test]
async fn cli_json_remote_url_skips_config_and_returns_nonzero_for_stale_readings() {
    for state in ["ok", "stale", "disabled", "error"] {
        let payload = reading(state);
        let (url, task) = serve(Router::new().fallback(any(move |request: Request| {
            assert_eq!(request.uri(), "/usage/v1/claude/default");
            assert_eq!(
                request.headers()["authorization"],
                "Bearer synthetic-host-key"
            );
            let payload = payload.clone();
            async move { axum::Json(payload) }
        })))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never-created.json");
        let output = command()
            .arg("--config")
            .arg(&path)
            .args([
                "usage",
                "--base-url",
                &url,
                "--json",
                "--token-env",
                "TEST_USAGE_KEY",
            ])
            .env("TEST_USAGE_KEY", "synthetic-host-key")
            .output()
            .await
            .unwrap();
        assert_eq!(
            output.status.success(),
            state == "ok",
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(payload["state"], state);
        assert_eq!(payload["data"]["extra_usage"]["spend"]["remaining"], 7.5);
        assert!(!path.exists());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-host-key"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-host-key"));
        task.abort();
    }
}

#[tokio::test]
async fn cli_uses_local_host_key_for_config_and_lists_accounts_without_oauth_store() {
    let (url, task) = serve(Router::new().fallback(any(|request: Request| async move {
        assert_eq!(request.uri(), "/usage/v1/accounts");
        assert_eq!(
            request.headers()["authorization"],
            "Bearer synthetic-local-key"
        );
        axum::Json(json!({"schema_version":1,"accounts":[{"provider":"claude","id":"default"}]}))
    })))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"listen":url.trim_start_matches("http://"),"mode":"host"}))
            .unwrap(),
    )
    .unwrap();
    std::fs::write(
        path.with_extension("access-keys.json"),
        br#"{"local":"synthetic-local-key","clients":{}}"#,
    )
    .unwrap();
    let output = command()
        .arg("--config")
        .arg(&path)
        .args(["usage", "--accounts"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "claude/default\n"
    );
    assert!(!path.with_extension("claude.json").exists());
    task.abort();
}

#[tokio::test]
async fn cli_recommend_and_codex_usage_work_for_both_earliest_expiring_and_exhausted_states() {
    let rec_payload = json!({
        "schema_version": 1,
        "recommended_provider": "codex",
        "recommended_account": "default",
        "reason": "earliest_expiring_window",
        "summary": "Recommended codex: Session · 5 hours expires earliest in 30m (60.0% left) vs claude in 2h (75.0% left).",
        "candidates": [
            {
                "account": {"provider": "codex", "id": "default"},
                "state": "ok",
                "available": true,
                "exhausted": false,
                "using_extra_usage": false,
                "effective_remaining_percent": 60.0,
                "earliest_expiring_window": {
                    "id": "five_hour",
                    "label": "Session · 5 hours",
                    "used_percent": 40.0,
                    "remaining_percent": 60.0,
                    "resets_at": "2026-10-02T19:00:00Z",
                    "resets_in_seconds": 1800
                },
                "next_reset_at": "2026-10-02T19:00:00Z",
                "next_reset_in_seconds": 1800,
                "error": null
            },
            {
                "account": {"provider": "claude", "id": "default"},
                "state": "ok",
                "available": true,
                "exhausted": false,
                "using_extra_usage": false,
                "effective_remaining_percent": 75.0,
                "earliest_expiring_window": {
                    "id": "five_hour",
                    "label": "Session · 5 hours",
                    "used_percent": 25.0,
                    "remaining_percent": 75.0,
                    "resets_at": "2026-10-02T20:30:00Z",
                    "resets_in_seconds": 7200
                },
                "next_reset_at": "2026-10-02T20:30:00Z",
                "next_reset_in_seconds": 7200,
                "error": null
            }
        ]
    });
    let (url, task) = serve(Router::new().fallback(any(move |request: Request| {
        let rec_payload = rec_payload.clone();
        async move {
            match request.uri().path() {
                "/usage/v1/recommend" => axum::Json(rec_payload),
                "/usage/v1/codex/default" => {
                    let mut p = reading("ok");
                    p["account"]["provider"] = json!("codex");
                    axum::Json(p)
                }
                other => panic!("unexpected path {other}"),
            }
        }
    })))
    .await;

    let out_quiet = command()
        .args(["recommend", "--base-url", &url, "--provider-only"])
        .output()
        .await
        .unwrap();
    assert!(out_quiet.status.success());
    assert_eq!(String::from_utf8_lossy(&out_quiet.stdout).trim(), "codex");

    let out_human = command()
        .args(["recommend", "--base-url", &url])
        .output()
        .await
        .unwrap();
    assert!(out_human.status.success());
    let text = String::from_utf8_lossy(&out_human.stdout);
    assert!(text.contains("Recommended: codex"));
    assert!(text.contains("* codex/default · AVAILABLE"));

    let out_usage_rec = command()
        .args(["usage", "--recommend", "--json", "--base-url", &url])
        .output()
        .await
        .unwrap();
    assert!(out_usage_rec.status.success());
    let parsed: Value = serde_json::from_slice(&out_usage_rec.stdout).unwrap();
    assert_eq!(parsed["recommended_provider"], "codex");

    let out_codex = command()
        .args(["usage", "--provider", "codex", "--base-url", &url])
        .output()
        .await
        .unwrap();
    assert!(out_codex.status.success());
    assert!(String::from_utf8_lossy(&out_codex.stdout).contains("codex/default · ok"));

    task.abort();
}
