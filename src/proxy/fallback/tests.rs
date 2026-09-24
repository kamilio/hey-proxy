use super::*;
use axum::routing::any;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
    )
}
fn configured(url: String) -> Config {
    let mut config = Config {
        upstream_url: url,
        ..Config::test_fixture()
    };
    config.fallbacks =
        serde_json::from_value(json!({"model-primary":["model-secondary"]})).unwrap();
    config
        .api_keys
        .insert("primary".into(), "primary-secret".into());
    config
        .api_keys
        .insert("default".into(), "secondary-secret".into());
    // A long legacy retry budget must NOT starve the fallback candidate.
    config.retry.recovery_timeout_ms = 290_000;
    config
}
async fn post(url: &str, payload: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/v1/responses"))
        .json(&payload)
        .send()
        .await
        .unwrap()
}
fn done(model: &str) -> Response {
    axum::Json(json!({"model":model,"status":"completed","output":[],"usage":{"input_tokens":3,"output_tokens":1}})).into_response()
}
fn sse(bytes: String) -> Response {
    ([(header::CONTENT_TYPE, "text/event-stream")], bytes).into_response()
}

#[tokio::test]
async fn transient_http_fallback_preserves_reasoning_history_and_uses_target_key() {
    for status in [408, 429, 500, 502, 503, 504, 529] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = seen.clone();
        let upstream = Router::new().fallback(any(
            move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
                captured
                    .lock()
                    .unwrap()
                    .push((headers.clone(), body.clone()));
                async move {
                    if body["model"] == "model-primary" {
                        (
                            StatusCode::from_u16(status).unwrap(),
                            [("retry-after", "120")],
                            axum::Json(json!({"error":{"code":"server_error"}})),
                        )
                            .into_response()
                    } else {
                        done("model-secondary")
                    }
                }
            },
        ));
        let (upstream, u) = serve(upstream).await;
        let (url, p) = serve(router(configured(upstream)).unwrap()).await;
        let payload = json!({"model":"model-primary","input":[{"type":"reasoning","encrypted_content":"opaque-original"},
            {"type":"function_call","call_id":"c","name":"test","arguments":"{}"},
            {"type":"function_call_output","call_id":"c","output":"result"}],
            "reasoning":{"effort":"high","summary":"auto"},"tools":[{"type":"function","name":"test","parameters":{"type":"object"}}],
            "include":["reasoning.encrypted_content"],"store":false});
        let start = Instant::now();
        let response = reqwest::Client::new()
            .post(format!("{url}/v1/responses"))
            .header("idempotency-key", "original")
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-hey-proxy-fallback-count"], "1");
        assert_eq!(
            response.json::<Value>().await.unwrap()["model"],
            "model-secondary"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0["authorization"], "Bearer primary-secret");
        assert_eq!(seen[1].0["authorization"], "Bearer secondary-secret");
        assert_eq!(seen[0].0["idempotency-key"], "original");
        assert!(
            seen[1].0["idempotency-key"]
                .to_str()
                .unwrap()
                .starts_with("hey-proxy-fallback-")
        );
        let mut expected = payload;
        expected["model"] = json!("model-secondary");
        assert_eq!(seen[1].1, expected, "history or reasoning changed");
        p.abort();
        u.abort();
    }
}

#[tokio::test]
async fn permanent_failures_do_not_switch_or_retry() {
    for (status, code) in [
        (400, "invalid_prompt"),
        (401, "invalid_api_key"),
        (403, "permission_error"),
        (404, "model_not_found"),
        (422, "invalid_request_error"),
        (429, "insufficient_quota"),
        (500, "content_policy_violation"),
        (503, "security_block"),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let n = calls.clone();
        let (upstream, u) = serve(Router::new().fallback(any(move || {
            n.fetch_add(1, Ordering::SeqCst);
            async move {
                (
                    StatusCode::from_u16(status).unwrap(),
                    axum::Json(json!({"error":{"code":code}})),
                )
            }
        })))
        .await;
        let (url, p) = serve(router(configured(upstream)).unwrap()).await;
        let response = post(&url, json!({"model":"model-primary"})).await;
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            code
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        p.abort();
        u.abort();
    }
}

#[tokio::test]
async fn compressed_upstream_errors_are_forwarded_without_guessing_their_category() {
    let calls = Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    let (upstream, u) = serve(Router::new().fallback(any(move || {
        n.fetch_add(1, Ordering::SeqCst);
        async {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [("content-encoding", "gzip")],
                "opaque-compressed-error",
            )
        }
    })))
    .await;
    let (url, p) = serve(router(configured(upstream)).unwrap()).await;
    let r = post(&url, json!({"model":"model-primary"})).await;
    assert_eq!(r.status(), 503);
    assert_eq!(r.text().await.unwrap(), "opaque-compressed-error");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    p.abort();
    u.abort();
}

#[tokio::test]
async fn slow_headers_and_streams_wait_for_primary_instead_of_switching() {
    for streaming in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let n = calls.clone();
        let release = Arc::new(tokio::sync::Notify::new());
        let wait = release.clone();
        let (upstream,u)=serve(Router::new().fallback(any(move || {
            n.fetch_add(1,Ordering::SeqCst);let wait=wait.clone();async move {
                if !streaming {wait.notified().await;return done("model-primary");}
                let body=Body::from_stream(async_stream::stream! {
                    yield Ok::<_,std::io::Error>(Bytes::from_static(b"data: {\"type\":\"response.created\"}\n\n"));
                    wait.notified().await;
                    yield Ok(Bytes::from_static(b"data: {\"type\":\"response.completed\",\"response\":{\"model\":\"model-primary\"}}\n\n"));
                });
                let mut r=Response::new(body);r.headers_mut().insert(header::CONTENT_TYPE,"text/event-stream".parse().unwrap());r
            }
        }))).await;
        let mut config = configured(upstream);
        // Even a tiny legacy recovery deadline must not time out fallback attempts.
        config.retry.recovery_timeout_ms = 1;
        let (url, p) = serve(router(config).unwrap()).await;
        let request = tokio::spawn(async move {
            post(&url, json!({"model":"model-primary","stream":streaming}))
                .await
                .text()
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(!request.is_finished(), "Slow request was timed out");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release.notify_one();
        assert!(request.await.unwrap().contains("model-primary"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        p.abort();
        u.abort();
    }
}

#[tokio::test]
async fn fragmented_pre_output_refusal_hides_failed_id_and_logs_winning_model() {
    let (upstream, u) = serve(Router::new().fallback(any(|axum::Json(body): axum::Json<Value>| async move {
        let text = if body["model"] == "model-primary" {
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"failed-id\",\"output\":[]}}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\"}}}\n\n"
        } else {
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"winning-id\",\"model\":\"model-secondary\",\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n"
        };
        let stream = futures_util::stream::iter(text.as_bytes().iter().map(|b| Ok::<_, std::io::Error>(Bytes::from(vec![*b]))).collect::<Vec<_>>());
        let mut r = Response::new(Body::from_stream(stream));
        r.headers_mut().insert(header::CONTENT_TYPE,"text/event-stream".parse().unwrap()); r
    }))).await;
    let (url, p) = serve(router(configured(upstream)).unwrap()).await;
    let response = post(&url, json!({"model":"model-primary","stream":true})).await;
    let text = response.text().await.unwrap();
    assert!(text.contains("winning-id"));
    assert!(!text.contains("failed-id"));
    let logs: Value = reqwest::get(format!("{url}/logs/api?local=true"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = &logs["entries"][0];
    assert_eq!(entry["requested_model"], "model-primary");
    assert_eq!(entry["routed_model"], "model-secondary");
    assert_eq!(entry["response_id"], "winning-id");
    assert_eq!(entry["output_tokens"], 2);
    assert_eq!(entry["state"], "succeeded");
    assert_eq!(entry["retries"], 1);
    p.abort();
    u.abort();
}

#[tokio::test]
async fn committed_reasoning_text_or_tool_never_switches() {
    for event in [
        json!({"type":"response.output_text.delta","delta":"answer"}),
        json!({"type":"response.reasoning_summary_text.delta","delta":"thinking"}),
        json!({"type":"response.output_item.added","item":{"type":"function_call","id":"tool","arguments":""}}),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let n = calls.clone();
        let event = Arc::new(event.to_string());
        let (upstream, u) = serve(Router::new().fallback(any(move || {
            n.fetch_add(1, Ordering::SeqCst); let event = event.clone();
            async move {
                let body = Body::from_stream(async_stream::stream! {
                    yield Ok::<_, std::io::Error>(Bytes::from(format!("data: {event}\n\n")));
                    tokio::time::sleep(Duration::from_millis(180)).await;
                    yield Ok(Bytes::from_static(b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\"}}}\n\n"));
                });
                let mut r=Response::new(body); r.headers_mut().insert(header::CONTENT_TYPE,"text/event-stream".parse().unwrap()); r
            }
        }))).await;
        let config = configured(upstream);
        let (url, p) = serve(router(config).unwrap()).await;
        let response = post(&url, json!({"model":"model-primary","stream":true})).await;
        assert_eq!(response.headers()["x-hey-proxy-fallback-count"], "0");
        assert!(response.text().await.unwrap().contains("response.failed"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        p.abort();
        u.abort();
    }
}

#[tokio::test]
async fn final_error_and_retry_after_survive_exhaustion_and_attempts_are_bounded() {
    let calls = Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    let (upstream, u) = serve(Router::new().fallback(any(move || {
        n.fetch_add(1, Ordering::SeqCst);
        async {
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", "9")],
                axum::Json(json!({"error":{"code":"rate_limit_exceeded","message":"last error"}})),
            )
        }
    })))
    .await;
    let config = configured(upstream);
    let (url, p) = serve(router(config).unwrap()).await;
    let r = post(&url, json!({"model":"model-primary"})).await;
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "9");
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"]["message"],
        "last error"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    p.abort();
    u.abort();
}

#[tokio::test]
async fn gemini_native_stream_refusal_and_http_errors_fallback_to_openai() {
    for streaming in [false, true] {
        let (google,g)=serve(Router::new().fallback(any(move || async move {
            if streaming { sse("data: {\"error\":{\"code\":503,\"status\":\"UNAVAILABLE\",\"message\":\"busy\"}}\n\n".into()) }
            else { (StatusCode::SERVICE_UNAVAILABLE,axum::Json(json!({"error":{"code":503,"status":"UNAVAILABLE"}}))).into_response() }
        }))).await;
        let (openai, o) =
            serve(Router::new().fallback(any(|| async { done("model-secondary") }))).await;
        let mut config = configured(openai);
        config.aliases[0].to = Some("gemini/test".into());
        config.aliases[0].api_key = None;
        config.fallbacks =
            serde_json::from_value(json!({"gemini/test":["model-secondary"]})).unwrap();
        config.gemini = Some(
            serde_json::from_value(
                json!({"upstream_url":google,"auth":"bearer","api_key":"synthetic"}),
            )
            .unwrap(),
        );
        let (url, p) = serve(router(config).unwrap()).await;
        let r = post(
            &url,
            json!({"model":"model-primary","input":"hello","stream":streaming}),
        )
        .await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["x-hey-proxy-fallback-count"], "1");
        assert_eq!(r.json::<Value>().await.unwrap()["model"], "model-secondary");
        p.abort();
        g.abort();
        o.abort();
    }
}

#[tokio::test]
async fn openai_to_gemini_preserves_full_request_and_conversion_errors_are_terminal() {
    let (openai, o) =
        serve(Router::new().fallback(any(|| async { (StatusCode::SERVICE_UNAVAILABLE, "busy") })))
            .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    let (google,g)=serve(Router::new().fallback(any(move |axum::Json(value):axum::Json<Value>| {
        n.fetch_add(1,Ordering::SeqCst);assert_eq!(value["contents"][0]["parts"][0]["text"],"hello");
        async {axum::Json(json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"answer"}]},"finishReason":"STOP"}]}))}
    }))).await;
    let mut config = configured(openai);
    config.aliases.push(
        serde_json::from_value(json!({"from":"model-secondary","to":"gemini/test"})).unwrap(),
    );
    config.gemini = Some(
        serde_json::from_value(
            json!({"upstream_url":google,"auth":"bearer","api_key":"synthetic"}),
        )
        .unwrap(),
    );
    let (url, p) = serve(router(config).unwrap()).await;
    let r = post(&url, json!({"model":"model-primary","input":"hello"})).await;
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("answer"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Never discard provider-bound opaque reasoning to make another provider work.
    let r=post(&url,json!({"model":"model-primary","input":[{"type":"reasoning","encrypted_content":"openai-opaque"}]})).await;
    assert_eq!(r.status(), 400);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    p.abort();
    o.abort();
    g.abort();
}

#[tokio::test]
async fn websocket_upgrade_downgrades_to_http_only_when_fallbacks_enabled() {
    let config = configured("http://127.0.0.1:1".into());
    let (url, p) = serve(router(config).unwrap()).await;
    let r = reqwest::Client::new()
        .get(format!("{url}/v1/responses"))
        .header("upgrade", "websocket")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 426);
    p.abort();
}

#[tokio::test]
async fn shared_host_client_propagates_websocket_downgrade_before_local_upgrade() {
    let (host, h) = serve(router(configured("http://127.0.0.1:1".into())).unwrap()).await;
    let client: Config = serde_json::from_value(json!({"mode":"client","listen":"127.0.0.1:8080",
        "connection":{"url":host,"api_key":"host-access"}}))
    .unwrap();
    let (url, p) = serve(router(client).unwrap()).await;
    let r = reqwest::Client::new()
        .get(format!("{url}/v1/responses"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 426);
    p.abort();
    h.abort();
}

#[tokio::test]
async fn abrupt_pre_output_disconnect_switches_but_partial_tool_disconnect_does_not() {
    for committed in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let n = calls.clone();
        let (upstream,u)=serve(Router::new().fallback(any(move |axum::Json(body):axum::Json<Value>| {
            n.fetch_add(1,Ordering::SeqCst);async move {
                if body["model"]=="model-secondary" {return done("model-secondary");}
                let body=Body::from_stream(async_stream::stream! {
                    let initial=if committed {b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"arguments\":\"\"}}\n\n".as_slice()}
                        else {b"data: {\"type\":\"response.created\"}\n\n".as_slice()};
                    yield Ok::<_,std::io::Error>(Bytes::from_static(initial));
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    yield Err(std::io::Error::other("synthetic transport interruption"));
                });
                let mut r=Response::new(body);r.headers_mut().insert(header::CONTENT_TYPE,"text/event-stream".parse().unwrap());r
            }
        }))).await;
        let (url, p) = serve(router(configured(upstream)).unwrap()).await;
        let r = post(&url, json!({"model":"model-primary","stream":true})).await;
        if committed {
            assert!(r.bytes().await.is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(r.json::<Value>().await.unwrap()["model"], "model-secondary");
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }
        p.abort();
        u.abort();
    }
}

#[test]
fn fallback_config_roundtrip_alias_reasoning_and_client_ownership() {
    let mut config = configured("http://127.0.0.1:1".into());
    config.aliases.push(
        serde_json::from_value(
            json!({"from":"logical","to":"model-primary","reasoning":"high","api_key":"primary"}),
        )
        .unwrap(),
    );
    let value = serde_json::to_value(&config).unwrap();
    let loaded: Config = serde_json::from_value(value.clone()).unwrap();
    loaded.validate().unwrap();
    assert_eq!(serde_json::to_value(&loaded).unwrap(), value);
    let request = json!({"model":"logical","reasoning":{"effort":"low","summary":"auto"}});
    let (bytes, key) = rewrite(&loaded, "/v1/responses", Bytes::from(request.to_string())).unwrap();
    assert_eq!(key, "primary");
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["reasoning"],
        json!({"effort":"high","summary":"auto"})
    );
    let rewritten = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        plan(&loaded, "/v1/responses", &request, &rewritten).unwrap()[0].actual,
        "model-primary"
    );
    assert!(value.get("models").is_none());
    assert!(value.get("fallback").is_none());
    assert_eq!(
        value["fallbacks"],
        json!({"model-primary":["model-secondary"]})
    );
    let mut client:Config=serde_json::from_value(json!({"mode":"client","listen":"127.0.0.1:8080","connection":{"url":"http://host:8080","api_key":"host-token"}})).unwrap();
    client.fallbacks = config.fallbacks;
    assert!(client.validate().is_err());
    assert!(client.effective().fallbacks.is_empty());
}

#[tokio::test]
async fn preserves_alias_reasoning_override_on_fallback_and_idempotency_key_is_stable() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let captured = received.clone();
    let (upstream, u) = serve(Router::new().fallback(any(
        move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
            captured.lock().unwrap().push((headers, body.clone()));
            async move {
                if body["model"] == "model-primary" {
                    (StatusCode::SERVICE_UNAVAILABLE, "busy").into_response()
                } else {
                    done("model-secondary")
                }
            }
        },
    )))
    .await;
    let mut config = configured(upstream);
    config.aliases.push(
        serde_json::from_value(
            json!({"from":"logical","to":"model-primary","reasoning":"high","api_key":"primary"}),
        )
        .unwrap(),
    );
    let (url, p) = serve(router(config).unwrap()).await;
    for _ in 0..2 {
        let r = reqwest::Client::new()
            .post(format!("{url}/v1/responses"))
            .header("idempotency-key", "same-key")
            .json(&json!({"model":"logical","reasoning":{"effort":"low","summary":"auto"}}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.bytes().await.unwrap();
    }
    let got = received.lock().unwrap();
    assert_eq!(got.len(), 4);
    assert_eq!(
        got[1].1["reasoning"],
        json!({"effort":"high","summary":"auto"})
    );
    assert_eq!(got[1].0["idempotency-key"], got[3].0["idempotency-key"]);
    p.abort();
    u.abort();
}

#[tokio::test]
async fn client_disconnect_cancels_pending_fallback_attempts() {
    let calls = Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    let (upstream, u) = serve(Router::new().fallback(any(move || {
        n.fetch_add(1, Ordering::SeqCst);
        async {
            std::future::pending::<()>().await;
            done("never")
        }
    })))
    .await;
    let config = configured(upstream);
    let (url, p) = serve(router(config).unwrap()).await;
    let request = tokio::spawn(async move { post(&url, json!({"model":"model-primary"})).await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    request.abort();
    let _ = request.await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "Cancelled request launched a fallback"
    );
    p.abort();
    u.abort();
}

#[tokio::test]
async fn chain_is_snapshotted_during_reload_and_new_requests_use_edited_models() {
    let reached = Arc::new(tokio::sync::Notify::new());
    let notify = reached.clone();
    let release = Arc::new(tokio::sync::Notify::new());
    let wait = release.clone();
    let (upstream, u) = serve(Router::new().fallback(any(
        move |axum::Json(body): axum::Json<Value>| {
            let notify = notify.clone();
            let wait = wait.clone();
            async move {
                if body["model"] == "model-primary" {
                    notify.notify_one();
                    wait.notified().await;
                    (StatusCode::SERVICE_UNAVAILABLE, "busy").into_response()
                } else {
                    done(body["model"].as_str().unwrap())
                }
            }
        },
    )))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let mut config = configured(upstream);
    config.logging.enabled = false;
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let options = Options {
        source: Some((path.clone(), config::fingerprint(&path))),
        ..Default::default()
    };
    let (url, p) = serve(router_with(config.clone(), options).unwrap()).await;
    let request_url = url.clone();
    let first = tokio::spawn(async move {
        post(&request_url, json!({"model":"model-primary"}))
            .await
            .json::<Value>()
            .await
            .unwrap()
    });
    reached.notified().await;
    config.aliases.push(
        serde_json::from_value(json!({"from":"model-secondary","to":"openai/edited-secondary"}))
            .unwrap(),
    );
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    release.notify_one();
    assert_eq!(first.await.unwrap()["model"], "model-secondary");
    release.notify_one();
    assert_eq!(
        post(&url, json!({"model":"model-primary"}))
            .await
            .json::<Value>()
            .await
            .unwrap()["model"],
        "edited-secondary"
    );
    // An invalid edit is ignored, retaining the last valid fallback configuration.
    config
        .fallbacks
        .insert("model-secondary".into(), vec!["model-primary".into()]);
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    release.notify_one();
    assert_eq!(
        post(&url, json!({"model":"model-primary"}))
            .await
            .json::<Value>()
            .await
            .unwrap()["model"],
        "edited-secondary"
    );
    p.abort();
    u.abort();
}

#[tokio::test]
async fn stateful_and_hosted_tool_requests_and_unconfigured_models_use_existing_path() {
    let calls = Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    let (upstream, u) = serve(Router::new().fallback(any(move || {
        n.fetch_add(1, Ordering::SeqCst);
        async {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({"error":{"code":"server_error"}})),
            )
        }
    })))
    .await;
    let mut config = configured(upstream);
    config.retry.max_retries = 0;
    let (url, p) = serve(router(config).unwrap()).await;
    for body in [
        json!({"model":"model-primary","previous_response_id":"r"}),
        json!({"model":"model-primary","conversation":"c"}),
        json!({"model":"model-primary","background":true}),
        json!({"model":"model-primary","tools":[{"type":"mcp"}]}),
        json!({"model":"unconfigured"}),
    ] {
        let r = post(&url, body).await;
        assert_eq!(r.status(), 503);
        assert!(!r.headers().contains_key("x-hey-proxy-fallback-count"));
        r.bytes().await.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    p.abort();
    u.abort();
}

#[tokio::test]
async fn fallback_lookup_uses_rewritten_model_and_reasoning_route_on_the_wire() {
    for path in ["/v1/responses", "/v1/chat/completions"] {
        for effort in [None, Some("high"), Some("low")] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let captured = seen.clone();
            let (upstream, u) = serve(Router::new().fallback(any(
                move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
                    captured.lock().unwrap().push((headers, body));
                    async { (StatusCode::SERVICE_UNAVAILABLE, "busy") }
                },
            )))
            .await;
            let mut config = configured(upstream);
            config.retry.max_retries = 0;
            config.aliases.push(
                serde_json::from_value(json!({
                    "from":"gpt-5.4", "to":"openai/model-primary", "api_key":"primary",
                    "reasoning_routes":{"low":{"to":"model-secondary","api_key":"default"}}
                }))
                .unwrap(),
            );
            config
                .fallbacks
                .insert("gpt-5.4".into(), vec!["must-not-run".into()]);
            let (url, p) = serve(router(config).unwrap()).await;
            let mut payload = json!({"model":"gpt-5.4"});
            if let Some(effort) = effort {
                if path.ends_with("responses") {
                    payload["reasoning"] = json!({"effort":effort});
                } else {
                    payload["reasoning_effort"] = json!(effort);
                }
            }
            let r = reqwest::Client::new()
                .post(format!("{url}{path}"))
                .json(&payload)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 503);
            r.bytes().await.unwrap();
            let seen = seen.lock().unwrap();
            let models: Vec<_> = seen
                .iter()
                .map(|(_, b)| b["model"].as_str().unwrap())
                .collect();
            assert_eq!(
                models,
                if effort == Some("low") {
                    vec!["model-secondary"]
                } else {
                    vec!["model-primary", "model-secondary"]
                }
            );
            assert_eq!(
                seen.last().unwrap().0["authorization"],
                "Bearer secondary-secret"
            );
            p.abort();
            u.abort();
        }
    }
}

#[test]
fn fallback_target_rewrites_determine_subsequent_rules_and_cycles_are_bounded() {
    let mut config = configured("http://127.0.0.1:1".into());
    config.aliases.push(
        serde_json::from_value(json!({"from":"model-secondary","to":"openai/actual-secondary"}))
            .unwrap(),
    );
    config
        .fallbacks
        .insert("model-secondary".into(), vec!["must-not-run".into()]);
    config
        .fallbacks
        .insert("actual-secondary".into(), vec!["last".into()]);
    config
        .fallbacks
        .insert("last".into(), vec!["loop-alias".into()]);
    config.aliases.push(
        serde_json::from_value(
            json!({"from":"loop-alias","to":"model-primary","api_key":"primary"}),
        )
        .unwrap(),
    );
    config.validate().unwrap();
    let original = json!({"model":"model-primary"});
    let candidates = plan(&config, "/v1/responses", &original, &original).unwrap();
    assert_eq!(
        candidates
            .iter()
            .map(|c| c.actual.as_str())
            .collect::<Vec<_>>(),
        ["model-primary", "actual-secondary", "last"]
    );
}

#[tokio::test]
async fn model_wait_survives_ten_minutes_without_read_or_recovery_timeout() {
    let reached = Arc::new(tokio::sync::Notify::new());
    let notify = reached.clone();
    let release = Arc::new(tokio::sync::Notify::new());
    let wait = release.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let (upstream, u) = serve(Router::new().fallback(any(move || {
        let notify = notify.clone();
        let wait = wait.clone();
        count.fetch_add(1, Ordering::SeqCst);
        async move {
            notify.notify_one();
            wait.notified().await;
            done("model-primary")
        }
    })))
    .await;
    let (url, p) = serve(router(configured(upstream)).unwrap()).await;
    let task = tokio::spawn(async move {
        post(&url, json!({"model":"model-primary"}))
            .await
            .text()
            .await
            .unwrap()
    });
    reached.notified().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(601)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        !task.is_finished(),
        "Proxy timed out a pending model response"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::resume();
    release.notify_one();
    assert!(task.await.unwrap().contains("model-primary"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    p.abort();
    u.abort();
}
