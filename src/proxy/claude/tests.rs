use super::*;
use axum::routing::any;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}
fn config(dir: &Path, upstream: &str) -> Config {
    let path = dir.join("claude.json");
    crate::claude_auth::tests::fixture(&path, crate::claude_auth::now() + 3600);
    serde_json::from_value(json!({"listen":"127.0.0.1:8080","providers":{"claude":{"upstream_url":upstream,"credentials_file":path,"usage_cache_seconds":30}},"retry":{"max_retries":0}})).unwrap()
}
fn input(stream: bool) -> String {
    // Whitespace, signatures and tool names must survive byte-for-byte.
    format!(
        r#"{{ "model": "claude-test", "stream": {stream}, "max_tokens": 32, "system": "You are Claude Code, Anthropic's official CLI for Claude.", "messages": [{{"role":"assistant","content":[{{"type":"thinking","thinking":"private","signature":"unchanged"}},{{"type":"tool_use","id":"call_1","name":"Bash","input":{{"command":"true"}}}}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"call_1","content":"done"}}]}}] }}"#
    )
}

#[test]
fn claude_config_roundtrips_and_rejects_unsafe_destinations_and_client_credentials() {
    let config: Config =
        serde_json::from_value(json!({"listen":"127.0.0.1:8080","providers":{"claude":{}}}))
            .unwrap();
    config.validate().unwrap();
    let roundtrip: Config = serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
    assert!(roundtrip.claude.is_some());
    let p = roundtrip.claude.unwrap();
    assert_eq!(
        p.credentials_path(Some(Path::new("/tmp/example.json")))
            .unwrap(),
        Path::new("/tmp/example.claude.json")
    );
    for url in [
        "http://example.com",
        "https://user:secret@api.anthropic.com",
        "https://api.anthropic.com/path",
        "https://api.anthropic.com/?q=x",
    ] {
        let p = ProviderConfig {
            upstream_url: url.into(),
            ..Default::default()
        };
        assert!(p.validate(Mode::Standalone).is_err());
    }
    let mut client = config;
    client.mode = Mode::Client;
    client.connection = Some(crate::config::ClientConnection {
        url: "http://localhost:8080".into(),
        api_key: "local".into(),
    });
    assert!(client.validate().is_err());
}

#[tokio::test]
async fn native_messages_preserve_wire_bytes_and_replace_all_client_credentials() {
    let expected = input(false);
    let compare = expected.clone();
    let app = Router::new().fallback(any(move |request: Request| {
        let compare = compare.clone();
        async move {
            let (parts, body) = request.into_parts();
            assert_eq!(parts.uri, "/v1/messages?beta=true");
            assert_eq!(parts.headers["authorization"], "Bearer sk-ant-oat01-synthetic-old");
            assert_eq!(parts.headers["user-agent"], "claude-code/synthetic");
            assert_eq!(parts.headers["anthropic-beta"], "future-feature,oauth-2025-04-20");
            for name in ["x-api-key","cookie","openai-project","x-hop"] { assert!(!parts.headers.contains_key(name)); }
            assert_eq!(axum::body::to_bytes(body, 65536).await.unwrap(), compare);
            ([("anthropic-ratelimit-unified-5h-utilization", "0.25"), ("request-id", "req-native")], axum::Json(json!({"type":"message","id":"msg_1","model":"claude-test","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":4,"cache_read_input_tokens":10,"cache_creation_input_tokens":2,"output_tokens":3}})))
        }
    }));
    let (upstream, upstream_task) = serve(app).await;
    let dir = tempfile::tempdir().unwrap();
    let logs = Arc::new(logs::Store::default());
    let (url, proxy_task) = serve(
        router_with(
            config(dir.path(), &upstream),
            Options {
                logs: Some(logs.clone()),
                ..Default::default()
            },
        )
        .unwrap(),
    )
    .await;
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/messages?beta=true"))
        .bearer_auth("local-placeholder")
        .header("x-api-key", "local-secret")
        .header("cookie", "session=local")
        .header("openai-project", "local-project")
        .header("connection", "x-hop")
        .header("x-hop", "drop")
        .header("user-agent", "claude-code/synthetic")
        .header("anthropic-beta", "future-feature")
        .body(expected)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["anthropic-ratelimit-unified-5h-utilization"],
        "0.25"
    );
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["content"][0]["text"], "ok");
    let entry = &logs.recent()[0];
    assert_eq!(entry.input_tokens, Some(16));
    assert_eq!(entry.output_tokens, Some(3));
    assert_eq!(entry.upstream_request_id.as_deref(), Some("req-native"));
    let serialized = serde_json::to_string(entry).unwrap();
    for private in ["local-placeholder", "sk-ant-oat", "unchanged", "private"] {
        assert!(!serialized.contains(private));
    }
    proxy_task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn native_stream_is_unchanged_and_counts_split_messages_usage() {
    let sse = concat!(
        "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_sse\",\"usage\":{\"input_tokens\":4,\"cache_read_input_tokens\":10,\"cache_creation_input_tokens\":2,\"output_tokens\":0}}}\r\n\r\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"héllo\"}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let (upstream, upstream_task) = serve(Router::new().fallback(any(move || async move {
        let chunks = sse
            .as_bytes()
            .iter()
            .map(|b| Ok::<_, std::io::Error>(Bytes::from(vec![*b])))
            .collect::<Vec<_>>();
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(futures_util::stream::iter(chunks)),
        )
    })))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let logs = Arc::new(logs::Store::default());
    let (url, proxy_task) = serve(
        router_with(
            config(dir.path(), &upstream),
            Options {
                logs: Some(logs.clone()),
                ..Default::default()
            },
        )
        .unwrap(),
    )
    .await;
    let reply = reqwest::Client::new()
        .post(format!("{url}/v1/messages"))
        .body(input(true))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(reply, sse);
    let entry = &logs.recent()[0];
    assert_eq!(entry.input_tokens, Some(16));
    assert_eq!(entry.output_tokens, Some(7));
    assert_eq!(entry.cached_input_tokens, Some(10));
    assert_eq!(entry.cache_write_tokens, Some(2));
    assert_eq!(entry.response_id.as_deref(), Some("msg_sse"));
    assert_eq!(entry.state, "succeeded");
    assert!(entry.first_output_ms.is_some());
    proxy_task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn native_count_and_rate_limits_are_forwarded_without_retries() {
    let count = Arc::new(AtomicUsize::new(0));
    let called = count.clone();
    let (upstream, upstream_task) = serve(Router::new().fallback(any(move |request: Request| {
        let called = called.clone();
        async move {
            called.fetch_add(1, Ordering::SeqCst);
            if request.uri().path().ends_with("count_tokens") { axum::Json(json!({"input_tokens":42})).into_response() }
            else { (StatusCode::TOO_MANY_REQUESTS, [("retry-after","90"),("anthropic-ratelimit-unified-status","rejected")], axum::Json(json!({"type":"error","error":{"type":"rate_limit_error","message":"Limit reached"}}))).into_response() }
        }
    }))).await;
    let dir = tempfile::tempdir().unwrap();
    let (url, proxy_task) = serve(router(config(dir.path(), &upstream)).unwrap()).await;
    let client = reqwest::Client::new();
    let value: Value = client
        .post(format!("{url}/v1/messages/count_tokens"))
        .body(input(false))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(value["input_tokens"], 42);
    let response = client
        .post(format!("{url}/v1/messages"))
        .body(input(false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "90");
    assert_eq!(count.load(Ordering::SeqCst), 2);
    proxy_task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn native_streams_continue_when_sqlite_is_locked_and_the_accounting_queue_overflows() {
    let sse = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":0}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let (upstream, upstream_task) = serve(Router::new().fallback(any(move || async move {
        let chunks = sse
            .as_bytes()
            .chunks(31)
            .map(|chunk| Ok::<_, std::io::Error>(Bytes::copy_from_slice(chunk)))
            .collect::<Vec<_>>();
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(futures_util::stream::iter(chunks)),
        )
    })))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path(), &upstream);
    config.logging.queue_capacity = 128;
    config.logging.batch_size = 32;
    config.logging.flush_interval_ms = 10;
    let logs = Arc::new(logs::Store::open(&config, &dir.path().join("config.json")).unwrap());
    logs.flush().await.unwrap();
    let db = logs.database.as_ref().unwrap();
    let locked = rusqlite::Connection::open(&db.path).unwrap();
    locked.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (url, proxy_task) = serve(
        router_with(
            config,
            Options {
                logs: Some(logs.clone()),
                ..Default::default()
            },
        )
        .unwrap(),
    )
    .await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        for _ in 0..10 {
            let requests = (0..10).map(|_| async {
                let response = client
                    .post(format!("{url}/v1/messages"))
                    .body(input(true))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.text().await.unwrap(), sse);
            });
            futures_util::future::join_all(requests).await;
        }
    })
    .await
    .expect("Native streaming waited for the locked accounting writer");
    assert!(db.health()["dropped_events"].as_u64().unwrap() > 0);
    assert_eq!(logs.rpm(), 100);
    locked.execute_batch("ROLLBACK").unwrap();
    logs.flush().await.unwrap();
    let dashboard: Value = client
        .get(format!("{url}/logs/api/dashboard"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dashboard["logging"]["status"], "gaps");
    assert!(dashboard["logging"]["dropped_events"].as_u64().unwrap() > 0);
    assert_eq!(dashboard["logging"]["pending_events"], 0);
    assert_eq!(dashboard["rpm"], 100);
    proxy_task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn usage_is_account_wide_allowlisted_cached_and_stale_on_rate_limit() {
    let count = Arc::new(AtomicUsize::new(0));
    let called = count.clone();
    let (upstream, upstream_task) = serve(Router::new().fallback(any(move |request: Request| {
        let called = called.clone();
        async move {
            assert_eq!(request.uri(), "/api/oauth/usage");
            assert_eq!(request.headers()["anthropic-beta"], OAUTH_BETA);
            assert_eq!(request.headers()["authorization"], "Bearer sk-ant-oat01-synthetic-old");
            if called.fetch_add(1, Ordering::SeqCst) > 0 {
                return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "120")], "private error body").into_response();
            }
            axum::Json(json!({"five_hour":{"utilization":25,"resets_at":"2026-10-01T01:00:00Z"},"seven_day":{"utilization":null,"resets_at":null},"limits":[{"kind":"weekly_scoped","group":"weekly","percent":70,"scope":{"model":{"display_name":"Example model"}},"resets_at":"2026-10-02T00:00:00Z"}],"extra_usage":{"is_enabled":false},"access_token":"PRIVATE_TOKEN","email":"PRIVATE_EMAIL"})).into_response()
        }
    }))).await;
    let dir = tempfile::tempdir().unwrap();
    let (url, proxy_task) = serve(router(config(dir.path(), &upstream)).unwrap()).await;
    let first: Value = reqwest::get(format!("{url}/claude/usage"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["state"], "ok");
    assert_eq!(first["data"]["windows"][0]["used_percent"], 25.0);
    assert!(first["data"]["windows"][1]["used_percent"].is_null());
    assert_eq!(first["data"]["windows"][2]["label"], "Example model");
    assert!(!first.to_string().contains("PRIVATE"));
    let mut jobs = Vec::new();
    for _ in 0..5 {
        let url = url.clone();
        jobs.push(tokio::spawn(async move {
            reqwest::get(format!("{url}/claude/usage")).await.unwrap()
        }));
    }
    for job in jobs {
        assert_eq!(
            job.await.unwrap().headers()[header::CACHE_CONTROL],
            "no-store"
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    let stale: Value = reqwest::get(format!("{url}/claude/usage"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stale["state"], "stale");
    assert_eq!(stale["updated_at"], first["updated_at"]);
    assert_eq!(stale["data"], first["data"]);
    assert!(!stale.to_string().contains("private error"));
    assert!(stale["retry_after_seconds"].as_u64().unwrap() >= 119);
    reqwest::get(format!("{url}/claude/usage")).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
    proxy_task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn usage_requires_host_auth_and_client_relay_uses_host_credentials() {
    let (upstream, upstream_task) = serve(Router::new().fallback(any(|| async {
        axum::Json(json!({"five_hour":{"utilization":12}}))
    })))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let access = crate::access::ensure(&path, &[]).unwrap();
    let mut host = config(dir.path(), &upstream);
    host.mode = Mode::Host;
    let (host_url, host_task) = serve(
        router_with(
            host,
            Options {
                access_config: Some(path),
                ..Default::default()
            },
        )
        .unwrap(),
    )
    .await;
    assert_eq!(
        reqwest::get(format!("{host_url}/claude/usage"))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    for path in ["/usage/v1/accounts", "/usage/v1/claude/default"] {
        assert_eq!(
            reqwest::get(format!("{host_url}{path}"))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let relay: Config = serde_json::from_value(json!({"listen":"127.0.0.1:8080","mode":"client","connection":{"url":host_url,"api_key":access.local}})).unwrap();
    let (relay_url, relay_task) = serve(router(relay).unwrap()).await;
    let value: Value = reqwest::get(format!("{relay_url}/claude/usage"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(value["data"]["windows"][0]["used_percent"], 12.0);
    let sdk =
        hey_proxy::usage::Client::new(&relay_url, Some("caller-key-never-forwarded")).unwrap();
    assert_eq!(sdk.accounts().await.unwrap().accounts[0].provider, "claude");
    assert_eq!(
        sdk.usage("claude", "default")
            .await
            .unwrap()
            .reading
            .data
            .unwrap()
            .windows[0]
            .remaining_percent,
        Some(88.0)
    );
    assert_eq!(
        sdk.usage("codex", "default").await.unwrap().reading.state,
        hey_proxy::usage::State::Disabled
    );
    assert_eq!(
        sdk.usage("unsupported", "default").await.unwrap_err(),
        hey_proxy::usage::Error::Http(501)
    );
    assert_eq!(
        sdk.usage("claude", "other").await.unwrap_err(),
        hey_proxy::usage::Error::Http(404)
    );
    let dashboard: Value = reqwest::get(format!("{host_url}/logs/api/dashboard"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(dashboard.get("rpm").is_none()); // Host authentication still applies.
    let dashboard: Value = reqwest::Client::new()
        .get(format!("{host_url}/logs/api/dashboard"))
        .bearer_auth(&access.local)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dashboard["rpm"], 0);
    relay_task.abort();
    host_task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn sdk_usage_shares_cache_and_preserves_provider_backoff() {
    use hey_proxy::usage::{Client, State as UsageState};
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let (upstream, upstream_task) = serve(Router::new().fallback(any(move || {
        let calls = calls.clone();
        async move {
            if calls.fetch_add(1, Ordering::SeqCst) > 0 {
                return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "120")], "PRIVATE").into_response();
            }
            axum::Json(json!({"five_hour":{"utilization":25},"seven_day":{"utilization":101},
                "extra_usage":{"is_enabled":true,"used_credits":250,"monthly_limit":1000},"token":"PRIVATE"})).into_response()
        }
    }))).await;
    let dir = tempfile::tempdir().unwrap();
    let (url, proxy_task) = serve(router(config(dir.path(), &upstream)).unwrap()).await;
    let client = Client::new(&format!("{url}/v1"), None).unwrap();
    let accounts = client.accounts().await.unwrap();
    assert_eq!(accounts.accounts.len(), 1);
    assert_eq!(accounts.accounts[0].id, "default");
    assert_eq!(count.load(Ordering::SeqCst), 0); // Startup/listing never query quota.
    let first = client.usage("claude", "default").await.unwrap();
    assert_eq!(first.reading.state, UsageState::Ok);
    let data = first.reading.data.as_ref().unwrap();
    assert_eq!(data.windows[0].remaining_percent, Some(75.0));
    assert_eq!(data.windows[1].remaining_percent, Some(0.0));
    assert_eq!(
        data.extra_usage
            .as_ref()
            .unwrap()
            .spend
            .as_ref()
            .unwrap()
            .remaining,
        Some(7.5)
    );
    let legacy: Value = reqwest::get(format!("{url}/claude/usage"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(legacy["updated_at"], first.reading.updated_at.unwrap());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    let stale = client.usage("claude", "default").await.unwrap();
    assert_eq!(stale.reading.state, UsageState::Stale);
    assert_eq!(stale.reading.updated_at, first.reading.updated_at);
    assert!(stale.reading.retry_after_seconds.unwrap() >= 119);
    assert_eq!(
        serde_json::to_value(stale.reading.data).unwrap(),
        serde_json::to_value(first.reading.data).unwrap()
    );
    assert!(
        !serde_json::to_string(&client.usage("claude", "default").await.unwrap())
            .unwrap()
            .contains("PRIVATE")
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(
        client
            .usage("codex", "default")
            .await
            .unwrap()
            .reading
            .state,
        UsageState::Disabled
    );
    assert_eq!(
        client.usage("unsupported", "default").await.unwrap_err(),
        hey_proxy::usage::Error::Http(501)
    );
    assert_eq!(
        client.usage("claude", "other").await.unwrap_err(),
        hey_proxy::usage::Error::Http(404)
    );
    let dashboard: Value = reqwest::get(format!("{url}/logs/api/dashboard"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dashboard["rpm"], 0);
    proxy_task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn pending_quota_fetch_does_not_block_native_requests() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let arrived = entered.clone();
    let released = release.clone();
    let (upstream, upstream_task) = serve(Router::new().fallback(any(move |request: Request| {
        let entered = arrived.clone();
        let release = released.clone();
        async move {
            if request.uri().path() == "/api/oauth/usage" {
                entered.notify_one();
                release.notified().await;
                return axum::Json(json!({"five_hour":{"utilization":25}}));
            }
            assert_eq!(request.uri().path(), "/v1/messages");
            axum::Json(
                json!({"type":"message","content":[],"usage":{"input_tokens":1,"output_tokens":1}}),
            )
        }
    })))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (url, proxy_task) = serve(router(config(dir.path(), &upstream)).unwrap()).await;
    let client = hey_proxy::usage::Client::new(&url, None).unwrap();
    let quota = tokio::spawn(async move { client.usage("claude", "default").await.unwrap() });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/messages"))
        .timeout(Duration::from_secs(2))
        .header("content-type", "application/json")
        .body(input(false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    assert!(!quota.is_finished());
    release.notify_one();
    quota.await.unwrap();
    proxy_task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn usage_without_a_configured_provider_is_disabled_not_zero() {
    let (url, task) = serve(router(Config::default()).unwrap()).await;
    let sdk = hey_proxy::usage::Client::new(&url, None).unwrap();
    assert!(sdk.accounts().await.unwrap().accounts.is_empty());
    let usage = sdk.usage("claude", "default").await.unwrap();
    assert_eq!(usage.reading.state, hey_proxy::usage::State::Disabled);
    assert!(usage.reading.data.is_none());
    assert!(usage.reading.updated_at.is_none());
    task.abort();
}
