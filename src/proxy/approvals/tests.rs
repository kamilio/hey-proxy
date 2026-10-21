use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn fixture(kind: Kind) -> Value {
    match kind {
        Kind::CodexReviewer => {
            json!({"model":"test-reviewer","input":[{"role":"user","content":"Review a synthetic tool call"}],
            "text":{"format":{"type":"json_schema","name":"guardian","strict":false,"schema":{"type":"object","properties":{
                "risk_level":{"type":"string","enum":["low","medium","high","critical"]},
                "user_authorization":{"type":"string","enum":["unknown","low","medium","high"]},
                "outcome":{"type":"string","enum":["allow","deny"]},"rationale":{"type":"string"}},"required":["outcome"],"additionalProperties":false}}}})
        }
        Kind::CodexClassifier => {
            json!({"model":"test-classifier","input":[],"tool_choice":"none","prompt_cache_key":"guardian-v2:synthetic"})
        }
        _ => {
            let mut input = json!({"model":"test-claude","max_tokens":128,
                "system":[{"type":"text","text":format!("{CLAUDE_MONITOR}\n\nYour job is to evaluate whether the agent's latest action should be blocked.\n\n## Output Format\nIf the action should be allowed:\n<block>no</block>")}],
                "messages":[{"role":"user","content":[{"type":"text","text":"<transcript>synthetic action</transcript>"}]}]});
            if kind == Kind::ClaudeTool {
                input["tools"] = json!([{"name":"classify_result","description":"Report the security classification result for the agent action","input_schema":{"type":"object","properties":{"thinking":{"type":"string"},"shouldBlock":{"type":"boolean"},"reason":{"type":"string"}},"required":["thinking","shouldBlock","reason"]}}]);
                input["tool_choice"] = json!({"type":"tool","name":"classify_result"});
            } else if kind == Kind::ClaudeSeverity {
                input["messages"][0]["content"].as_array_mut().unwrap().push(json!({"type":"text","text":"Stage 1 does NOT apply user intent or ALLOW exceptions — stage 2 will handle those.\nRespond with <severity>N</severity> ONLY."}));
                input["stop_sequences"] = json!(["</severity>"]);
            }
            input
        }
    }
}
fn headers(kind: Kind) -> HeaderMap {
    let mut h = HeaderMap::new();
    if kind.is_codex() {
        h.insert(
            "x-codex-guardian",
            if kind == Kind::CodexReviewer {
                "reviewer"
            } else {
                "classifier"
            }
            .parse()
            .unwrap(),
        );
    }
    h
}
fn assert_decision(kind: Kind, response: &Value) {
    assert_eq!(response["usage"]["input_tokens"], 0);
    assert_eq!(response["usage"]["output_tokens"], 0);
    match kind {
        Kind::CodexReviewer => {
            let decision: Value = serde_json::from_str(
                response["output"][0]["content"][0]["text"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(decision["outcome"], "allow");
            assert!(
                decision["rationale"]
                    .as_str()
                    .unwrap()
                    .contains("no model review")
            );
        }
        Kind::CodexClassifier => assert_eq!(response["output"][0]["content"][0]["text"], "low"),
        Kind::ClaudeTool => {
            assert_eq!(response["stop_reason"], "tool_use");
            assert_eq!(response["content"][0]["name"], "classify_result");
            assert_eq!(response["content"][0]["input"]["shouldBlock"], false);
        }
        _ => assert_eq!(response["content"][0]["text"], kind.text()),
    }
}
async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, task)
}

#[tokio::test]
async fn approval_formats_json_and_sse_are_local_on_every_supported_route() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let (upstream_url, upstream) = serve(Router::new().fallback(move || {
        count.fetch_add(1, Ordering::SeqCst);
        async { "unexpected upstream request" }
    }))
    .await;
    let config = Config {
        auto_approve: true,
        upstream_url,
        ..Config::default()
    };
    let store = Arc::new(logs::Store::memory(&config));
    let (url, proxy) = serve(
        router_with(
            config,
            Options {
                logs: Some(store.clone()),
                ..Options::default()
            },
        )
        .unwrap(),
    )
    .await;
    let client = reqwest::Client::new();
    for kind in [
        Kind::CodexReviewer,
        Kind::CodexClassifier,
        Kind::ClaudeTool,
        Kind::ClaudeXml,
        Kind::ClaudeSeverity,
    ] {
        let paths: &[&str] = if kind.is_codex() {
            &["/v1/responses", "/v1/responses/?probe=true"]
        } else {
            &[
                "/v1/messages",
                "/v1/custom/messages",
                "/custom/v1/messages/?beta=true",
            ]
        };
        for path in paths {
            for streaming in [false, true] {
                let mut input = fixture(kind);
                input["stream"] = json!(streaming);
                let response = client
                    .post(format!("{url}{path}"))
                    .headers(headers(kind))
                    .json(&input)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), 200, "{kind:?} {path}");
                assert_eq!(
                    response.headers()["x-hey-proxy-auto-approved"],
                    kind.label()
                );
                if !streaming {
                    assert_decision(kind, &response.json().await.unwrap());
                    continue;
                }
                assert_eq!(
                    response.headers()[header::CONTENT_TYPE],
                    "text/event-stream"
                );
                let body = response.bytes().await.unwrap();
                // Parse even one byte at a time, including multibyte text boundaries.
                let mut decoder = sse::SseDecoder::default();
                let mut events = Vec::new();
                for byte in &body {
                    events.extend(decoder.feed(&[*byte], false).unwrap());
                }
                events.extend(decoder.feed(&[], true).unwrap());
                if kind.is_codex() {
                    assert_eq!(events.first().unwrap()["type"], "response.created");
                    assert_eq!(events.last().unwrap()["type"], "response.completed");
                    assert_decision(kind, &events.last().unwrap()["response"]);
                    assert_eq!(
                        events
                            .iter()
                            .filter(|e| e["type"] == "response.output_text.delta")
                            .count(),
                        1
                    );
                    for (i, e) in events.iter().enumerate() {
                        assert_eq!(e["sequence_number"], i);
                    }
                } else {
                    assert_eq!(events.first().unwrap()["type"], "message_start");
                    assert_eq!(events.last().unwrap()["type"], "message_stop");
                    let delta = &events[2]["delta"];
                    if kind == Kind::ClaudeTool {
                        let parsed: Value =
                            serde_json::from_str(delta["partial_json"].as_str().unwrap()).unwrap();
                        assert_eq!(parsed["shouldBlock"], false);
                    } else {
                        assert_eq!(delta["text"], kind.text());
                    }
                }
            }
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let entries = store.recent();
    assert_eq!(entries.len(), 26);
    for entry in entries {
        assert_eq!(entry.project.as_deref(), Some("local:auto_approve"));
        assert_eq!(entry.attempts, 0);
        assert_eq!(entry.input_tokens, Some(0));
        assert_eq!(entry.output_tokens, Some(0));
    }
    proxy.abort();
    upstream.abort();
}

#[tokio::test]
async fn disabled_and_unrelated_requests_reach_upstream_unchanged() {
    let (upstream_url, upstream) =
        serve(Router::new().fallback(|body: Bytes| async move { body })).await;
    let client = reqwest::Client::new();
    for enabled in [false, true] {
        let config = Config {
            auto_approve: enabled,
            upstream_url: upstream_url.clone(),
            ..Config::test_fixture()
        };
        let (url, proxy) = serve(router(config).unwrap()).await;
        for (path, kind, change) in [
            ("/v1/messages", Kind::ClaudeXml, "quoted_prompt"),
            ("/v1/messages/count_tokens", Kind::ClaudeXml, "none"),
            ("/v1/responses/compact", Kind::CodexReviewer, "none"),
            ("/v1/responses", Kind::CodexReviewer, "no_header"),
            ("/v1/responses", Kind::CodexReviewer, "unknown_header"),
            ("/v1/messages", Kind::ClaudeTool, "ordinary_tool"),
            ("/v1/messages", Kind::ClaudeXml, "unknown_format"),
        ] {
            let mut input = fixture(kind);
            let mut h = headers(kind);
            match change {
                "quoted_prompt" => {
                    input["messages"][0]["content"] = input["system"].clone();
                    input["system"] = json!("You are a coding assistant.");
                }
                "no_header" => {
                    h.clear();
                }
                "unknown_header" => {
                    h.insert("x-codex-guardian", "future-kind".parse().unwrap());
                }
                "ordinary_tool" => {
                    input["tools"][0]["name"] = json!("ordinary_tool");
                }
                "unknown_format" => {
                    input["system"] = json!(CLAUDE_MONITOR);
                }
                _ => {}
            }
            let response = client
                .post(format!("{url}{path}"))
                .headers(h)
                .json(&input)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            assert!(
                response
                    .headers()
                    .get("x-hey-proxy-auto-approved")
                    .is_none()
            );
            let echoed: Value = response.json().await.unwrap();
            assert_eq!(echoed, input);
        }
        if !enabled {
            for kind in [
                Kind::CodexReviewer,
                Kind::CodexClassifier,
                Kind::ClaudeXml,
                Kind::ClaudeSeverity,
                Kind::ClaudeTool,
            ] {
                let path = if kind.is_codex() {
                    "/v1/responses"
                } else {
                    "/v1/messages"
                };
                let input = fixture(kind);
                let echoed: Value = client
                    .post(format!("{url}{path}"))
                    .headers(headers(kind))
                    .json(&input)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                assert_eq!(echoed, input);
            }
        }
        proxy.abort();
    }
    upstream.abort();
}

#[tokio::test]
async fn codex_websocket_handles_warmup_multiple_turns_and_inherited_models() {
    let (url, proxy) = serve(
        router(Config {
            auto_approve: true,
            ..Config::default()
        })
        .unwrap(),
    )
    .await;
    for kind in [Kind::CodexReviewer, Kind::CodexClassifier] {
        let mut request = format!("{}/v1/responses", url.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        request.headers_mut().extend(headers(kind));
        let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(
            response.headers()["x-hey-proxy-auto-approved"],
            kind.label()
        );
        for turn in 0..4 {
            let mut input = if turn == 0 {
                fixture(kind)
            } else {
                json!({"input":[]})
            };
            input["type"] = json!("response.create");
            if turn == 0 {
                input["generate"] = json!(false);
            }
            socket
                .send(Message::Text(input.to_string().into()))
                .await
                .unwrap();
            let mut deltas = 0;
            loop {
                let msg = tokio::time::timeout(Duration::from_secs(2), socket.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                let event: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
                if event["type"] == "response.output_text.delta" {
                    deltas += 1;
                }
                if event["type"] == "response.completed" {
                    if turn == 0 {
                        assert_eq!(event["response"]["output"], json!([]));
                        assert_eq!(deltas, 0);
                    } else {
                        assert_decision(kind, &event["response"]);
                        assert_eq!(deltas, 1);
                    }
                    assert_eq!(event["response"]["model"], fixture(kind)["model"]);
                    break;
                }
            }
        }
        socket.close(None).await.unwrap();
    }
    proxy.abort();
}

#[test]
fn option_is_off_by_default_and_survives_config_roundtrip() {
    let mut config: Config = serde_json::from_value(json!({"listen":"127.0.0.1:8080"})).unwrap();
    assert!(!config.auto_approve);
    for mode in [Mode::Standalone, Mode::Host, Mode::Client] {
        config.mode = mode;
        config.auto_approve = true;
        let serialized = serde_json::to_value(&config).unwrap();
        assert_eq!(serialized["auto_approve"], true);
        let read: Config = serde_json::from_value(serialized).unwrap();
        assert!(read.auto_approve);
    }
    assert!(
        serde_json::from_value::<Config>(json!({"listen":"127.0.0.1:8080","auto_approve":"true"}))
            .is_err()
    );
}

#[tokio::test]
async fn host_authentication_precedes_local_approval() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let keys = crate::access::ensure(&path, &[]).unwrap();
    let config = Config {
        auto_approve: true,
        mode: Mode::Host,
        ..Config::default()
    };
    let (url, task) = serve(
        router_with(
            config,
            Options {
                access_config: Some(path),
                ..Options::default()
            },
        )
        .unwrap(),
    )
    .await;
    let client = reqwest::Client::new();
    for kind in [Kind::CodexReviewer, Kind::ClaudeXml] {
        let path = if kind.is_codex() {
            "/v1/responses"
        } else {
            "/v1/messages"
        };
        let request = || {
            client
                .post(format!("{url}{path}"))
                .headers(headers(kind))
                .json(&fixture(kind))
        };
        let rejected = request().send().await.unwrap();
        assert_eq!(rejected.status(), 401);
        assert!(
            rejected
                .headers()
                .get("x-hey-proxy-auto-approved")
                .is_none()
        );
        let allowed = request().bearer_auth(&keys.local).send().await.unwrap();
        assert_eq!(allowed.status(), 200);
        assert_decision(kind, &allowed.json().await.unwrap());
    }
    task.abort();
}

#[tokio::test]
async fn approval_toggle_reloads_in_both_directions() {
    let (upstream_url, upstream) =
        serve(Router::new().fallback(|| async { axum::Json(json!({"upstream":true})) })).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let mut config = Config {
        upstream_url,
        ..Config::test_fixture()
    };
    config.logging.enabled = false;
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let options = Options {
        source: Some((path.clone(), config::fingerprint(&path))),
        ..Options::default()
    };
    let (url, proxy) = serve(router_with(config.clone(), options).unwrap()).await;
    let client = reqwest::Client::new();
    for enabled in [false, true, false] {
        config.auto_approve = enabled;
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let response = client
                    .post(format!("{url}/v1/responses"))
                    .headers(headers(Kind::CodexReviewer))
                    .json(&fixture(Kind::CodexReviewer))
                    .send()
                    .await
                    .unwrap();
                if response.headers().contains_key("x-hey-proxy-auto-approved") == enabled {
                    assert_eq!(response.status(), 200);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }
    proxy.abort();
    upstream.abort();
}

#[test]
fn claude_string_system_and_severity_stages_match_known_contracts() {
    let mut input = fixture(Kind::ClaudeSeverity);
    input["stop_sequences"] = Value::Null;
    assert_eq!(claude_kind(&input), Some(Kind::ClaudeSeverity));
    input["messages"][0]["content"][1]["text"] = json!(
        "Review the classification process and follow it carefully. Use <thinking> first, then <severity>N</severity>."
    );
    assert_eq!(claude_kind(&input), Some(Kind::ClaudeSeverity));
    let mut xml = fixture(Kind::ClaudeXml);
    xml["system"] = xml["system"][0]["text"].clone();
    assert_eq!(claude_kind(&xml), Some(Kind::ClaudeXml));
}
