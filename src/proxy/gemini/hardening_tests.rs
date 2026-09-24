use super::tests::{provider, serve};
use super::*;
use axum::{extract::State, routing::post};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[test]
fn concurrent_key_publication_uses_one_complete_private_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let barrier = Arc::new(std::sync::Barrier::new(32));
    let workers: Vec<_> = (0..32)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                GeminiState::new(Some(&path)).unwrap()
            })
        })
        .collect();
    let states: Vec<_> = workers.into_iter().map(|t| t.join().unwrap()).collect();
    let provider = provider("http://localhost", "bearer");
    let req = convert_request(
        &json!({"model":"gemini/test","input":"hi"}),
        &provider,
        &states[0].codec,
    )
    .unwrap();
    let response = convert_response(&json!({"candidates":[{"content":{"parts":[{"text":"hello","thoughtSignature":"same-key"}]},"finishReason":"STOP"}]}),&req,&states[0].codec,"atomic").unwrap();
    for state in states {
        convert_request(
            &json!({"model":"gemini/test","input":response["output"]}),
            &provider,
            &state.codec,
        )
        .unwrap();
    }
    assert_eq!(
        std::fs::read(path.with_extension("gemini-reasoning-key"))
            .unwrap()
            .len(),
        32
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn sse_limits_apply_to_complete_and_fragmented_frames() {
    let oversized = format!(
        "data: {{\"text\":\"{}\"}}\n\n",
        "x".repeat(MAX_NATIVE_FRAME)
    );
    assert!(
        NativeSse::default()
            .feed(oversized.as_bytes(), false)
            .unwrap_err()
            .to_string()
            .contains("16 MiB")
    );
    let mut parser = NativeSse::default();
    let mut rejected = false;
    for chunk in oversized.as_bytes().chunks(1023) {
        if parser.feed(chunk, false).is_err() {
            rejected = true;
            break;
        }
    }
    assert!(rejected);
    let mut parser = NativeSse::default();
    // The bound is per frame, not a lifetime limit on the parser.
    let legal = format!(":{}\n\n", "x".repeat(MAX_NATIVE_FRAME / 2));
    for _ in 0..3 {
        assert!(parser.feed(legal.as_bytes(), false).unwrap().is_empty());
    }
}

#[test]
fn sse_handles_mixed_delimiters_eof_and_rejects_data_after_done() {
    assert!(
        NativeSse::default()
            .feed(b"data:\n\n", false)
            .unwrap()
            .is_empty()
    );
    let bytes = b":comment\rdata: {\"a\":\r\ndata: 1}\n\rdata: {\"b\":2}";
    for split in 0..=bytes.len() {
        let mut parser = NativeSse::default();
        let mut events = parser.feed(&bytes[..split], false).unwrap();
        events.extend(parser.feed(&bytes[split..], true).unwrap());
        assert_eq!(events, vec![json!({"a":1}), json!({"b":2})]);
    }
    assert!(
        NativeSse::default()
            .feed(b"data: [DONE]\n\ndata: {}\n\n", true)
            .is_err()
    );
    assert!(NativeSse::default().feed(b"data: \xff\n\n", true).is_err());
}

fn stream_response(body: Body) -> Response {
    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    response
}
fn native_frame(value: Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}
fn events(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

#[tokio::test]
async fn downstream_cancellation_drops_upstream_without_retry() {
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    async fn upstream(
        State((dropped, calls)): State<(Arc<AtomicBool>, Arc<AtomicUsize>)>,
    ) -> Response {
        calls.fetch_add(1, Ordering::SeqCst);
        let guard = Dropped(dropped);
        stream_response(Body::from_stream(async_stream::stream! {
            let _guard=guard;
            yield Ok::<_,std::io::Error>(native_frame(json!({"candidates":[{"content":{"parts":[{"text":"first"}]}}]})));
            std::future::pending::<()>().await;
        }))
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let (upstream_url, task) = serve(
        Router::new()
            .fallback(upstream)
            .with_state((dropped.clone(), calls.clone())),
    )
    .await;
    let (url, proxy) = serve(
        super::super::router(Config {
            gemini: Some(provider(&upstream_url, "bearer")),
            ..Config::default()
        })
        .unwrap(),
    )
    .await;
    let client = reqwest::Client::new();
    let mut response = client
        .post(format!("{url}/v1/responses"))
        .json(&json!({"model":"gemini/test","input":"hi","stream":true}))
        .send()
        .await
        .unwrap();
    response.chunk().await.unwrap().unwrap();
    drop(response);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("downstream cancellation did not release upstream");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    task.abort();
    proxy.abort();
}

#[tokio::test]
async fn malformed_and_truncated_upstreams_never_complete_tool_calls() {
    async fn upstream(axum::Json(body): axum::Json<Value>) -> Response {
        let scenario = body["contents"][0]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        if scenario == "unary" {
            return axum::Json(json!({"candidates":"bad"})).into_response();
        }
        let name = body["tools"][0]["functionDeclarations"][0]["name"].clone();
        let prefix = json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":name,"args":{"cmd":"prefix"}}}]}}]});
        stream_response(Body::from_stream(async_stream::stream! {
            yield Ok::<_,std::io::Error>(native_frame(prefix));
            tokio::time::sleep(Duration::from_millis(10)).await;
            if scenario=="malformed" {
                yield Ok(native_frame(json!({"candidates":[{"finishReason":"STOP"}]})));
                yield Ok(Bytes::from_static(b"data: broken-json\n\n"));
            } else if scenario=="transport" {
                yield Err(std::io::Error::other("synthetic transport failure"));
            }
        }))
    }
    let (upstream_url, task) = serve(Router::new().fallback(upstream)).await;
    let (url, proxy) = serve(
        super::super::router(Config {
            gemini: Some(provider(&upstream_url, "bearer")),
            ..Config::default()
        })
        .unwrap(),
    )
    .await;
    let client = reqwest::Client::new();
    for scenario in ["unary", "truncated", "malformed", "transport"] {
        let response=client.post(format!("{url}/v1/responses")).json(&json!({"model":"gemini/test","input":scenario,"stream":scenario!="unary","tools":[{"type":"function","name":"run","parameters":{"type":"object"}}]})).send().await.unwrap();
        if scenario == "unary" {
            assert_eq!(response.status(), 502);
            continue;
        }
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        let events = events(&text);
        assert_eq!(
            events.last().unwrap()["type"],
            "response.failed",
            "{scenario}: {text}"
        );
        assert!(!text.contains("response.function_call_arguments.done"));
        assert!(
            !events
                .iter()
                .any(|e| e["type"] == "response.output_item.done"
                    && e["item"]["type"] == "function_call")
        );
        for pair in events.windows(2) {
            assert!(
                pair[0]["sequence_number"].as_u64().unwrap()
                    < pair[1]["sequence_number"].as_u64().unwrap()
            );
        }
    }
    let response=client.post(format!("{url}/v1/responses")).json(&json!({"model":"gemini/test","input":[{"type":"gemini_content","content":{"role":"user","parts":null}},{"role":"user","content":"hi"}]})).send().await.unwrap();
    assert_eq!(response.status(), 400);
    task.abort();
    proxy.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_signed_conversations_remain_isolated() {
    async fn upstream(
        State(count): State<Arc<AtomicUsize>>,
        axum::Json(body): axum::Json<Value>,
    ) -> Response {
        count.fetch_add(1, Ordering::SeqCst);
        let identity = body["contents"][0]["parts"][0]["text"].as_str().unwrap();
        let parts = if body["contents"].as_array().unwrap().len() == 1 {
            let name = body["tools"][0]["functionDeclarations"][0]["name"].clone();
            json!([
                {"text":format!("thinking-{identity}"),"thought":true,"thoughtSignature":format!("thought-{identity}")},
                {"functionCall":{"name":name,"id":identity,"args":{"identity":identity}},"thoughtSignature":format!("call-{identity}")}
            ])
        } else {
            assert_eq!(
                body["contents"][1]["parts"][0]["thoughtSignature"],
                format!("thought-{identity}")
            );
            assert_eq!(
                body["contents"][1]["parts"][1]["thoughtSignature"],
                format!("call-{identity}")
            );
            assert_eq!(
                body["contents"][2]["parts"][0]["functionResponse"]["id"],
                identity
            );
            json!([{"text":format!("done-{identity}")}])
        };
        let frame = native_frame(
            json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}]}),
        );
        stream_response(Body::from_stream(async_stream::stream! {
            // Exercise fragmentation across JSON, signatures and stream delimiters.
            for chunk in frame.chunks(37) { yield Ok::<_,std::io::Error>(Bytes::copy_from_slice(chunk)); }
            yield Ok(native_frame(json!({"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"thoughtsTokenCount":3}})));
        }))
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let (upstream_url, task) = serve(
        Router::new()
            .route("/models/test:streamGenerateContent", post(upstream))
            .with_state(calls.clone()),
    )
    .await;
    let (url, proxy) = serve(
        super::super::router(Config {
            gemini: Some(provider(&upstream_url, "bearer")),
            ..Config::default()
        })
        .unwrap(),
    )
    .await;
    let start = std::time::Instant::now();
    let client = reqwest::Client::new();
    let mut jobs = tokio::task::JoinSet::new();
    for worker in 0..32 {
        let client = client.clone();
        let url = url.clone();
        jobs.spawn(async move {
            let mut durations=Vec::new();
            for round in 0..8 {
                let started=std::time::Instant::now();
                let identity=format!("client-{worker}-round-{round}");
                let mut input=vec![json!({"role":"user","content":identity})];
                for turn in 0..2 {
                    let response=client.post(format!("{url}/v1/responses")).json(&json!({"model":"gemini/test","input":input,"stream":true,"tools":[{"type":"function","name":"run","parameters":{"type":"object"}}]})).send().await.unwrap();
                    assert_eq!(response.status(),200);
                    let text=response.text().await.unwrap();let events=events(&text);
                    let final_event=events.last().unwrap();assert_eq!(final_event["type"],"response.completed","{text}");
                    assert_eq!(final_event["response"]["usage"]["output_tokens"],5);
                    if turn==0 {
                        // Replay the exact completion order Codex sees, including the carrier.
                        input.extend(events.iter().filter(|e|e["type"]=="response.output_item.done").map(|e|e["item"].clone()));
                        input.push(json!({"type":"function_call_output","call_id":identity,"output":"ok"}));
                    } else {
                        assert_eq!(final_event["response"]["output"][1]["content"][0]["text"],format!("done-{identity}"));
                    }
                }
                durations.push(started.elapsed());
            }
            durations
        });
    }
    let mut durations = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(result) = jobs.join_next().await {
            durations.extend(result.unwrap());
        }
    })
    .await
    .unwrap();
    durations.sort_unstable();
    assert_eq!(calls.load(Ordering::SeqCst), 512);
    eprintln!(
        "debug loopback: 32 concurrent clients, 256 signed two-turn conversations, 512 requests; elapsed {:?}, conversation p50 {:?}, p95 {:?}",
        start.elapsed(),
        durations[128],
        durations[243]
    );
    task.abort();
    proxy.abort();
}

#[tokio::test]
async fn bounded_native_body_rejects_complete_and_chunked_oversize() {
    async fn upstream(axum::extract::Path(kind): axum::extract::Path<String>) -> Response {
        if kind == "complete" {
            return "x".repeat(65).into_response();
        }
        Response::new(Body::from_stream(async_stream::stream! {
            yield Ok::<_,std::io::Error>(Bytes::from(vec![b'x'; 32]));
            tokio::time::sleep(Duration::from_millis(5)).await;
            yield Ok(Bytes::from(vec![b'x'; 33]));
        }))
    }
    let (url, task) = serve(Router::new().route("/{kind}", axum::routing::get(upstream))).await;
    for kind in ["complete", "chunked"] {
        let response = reqwest::get(format!("{url}/{kind}")).await.unwrap();
        assert!(
            bounded_body(response, 64)
                .await
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
        let response = reqwest::get(format!("{url}/{kind}")).await.unwrap();
        assert_eq!(bounded_body(response, 65).await.unwrap(), vec![b'x'; 65]);
    }
    task.abort();
}

#[tokio::test]
async fn native_error_is_terminal_without_waiting_for_upstream_eof() {
    async fn upstream() -> Response {
        stream_response(Body::from_stream(async_stream::stream! {
            yield Ok::<_,std::io::Error>(native_frame(json!({"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Busy"}})));
            std::future::pending::<()>().await;
        }))
    }
    let (native, task) = serve(Router::new().fallback(upstream)).await;
    let (url, proxy) = serve(
        super::super::router(Config {
            gemini: Some(provider(&native, "bearer")),
            ..Config::default()
        })
        .unwrap(),
    )
    .await;
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/responses"))
        .json(&json!({"model":"gemini/test","input":"hi","stream":true}))
        .send()
        .await
        .unwrap();
    let text = tokio::time::timeout(Duration::from_secs(2), response.text())
        .await
        .unwrap()
        .unwrap();
    let events = events(&text);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "response.failed");
    assert_eq!(
        events[0]["response"]["error"]["code"],
        "rate_limit_exceeded"
    );
    task.abort();
    proxy.abort();
}

#[tokio::test]
async fn http_errors_use_responses_envelope_and_preserve_retry_after() {
    async fn upstream() -> Response {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "7")],
            axum::Json(
                json!({"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Busy"}}),
            ),
        )
            .into_response()
    }
    let (native, task) = serve(Router::new().fallback(upstream)).await;
    let mut config = Config {
        gemini: Some(provider(&native, "bearer")),
        ..Config::default()
    };
    config.retry.max_retries = 0;
    let (url, proxy) = serve(super::super::router(config).unwrap()).await;
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/responses"))
        .json(&json!({"model":"gemini/test","input":"hi","stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()[header::RETRY_AFTER], "7");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "rate_limit_exceeded");
    assert_eq!(body["gemini"]["error"]["status"], "RESOURCE_EXHAUSTED");
    task.abort();
    proxy.abort();
}

#[tokio::test]
async fn idle_stream_sends_heartbeat_then_preserves_final_usage() {
    async fn upstream(State(release): State<Arc<tokio::sync::Notify>>) -> Response {
        stream_response(Body::from_stream(async_stream::stream! {
            yield Ok::<_,std::io::Error>(native_frame(json!({"candidates":[{"content":{"parts":[{"text":"thinking","thought":true}]}}]})));
            release.notified().await;
            yield Ok(native_frame(json!({"candidates":[{"content":{"parts":[{"text":"done"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2}})));
        }))
    }
    let release = Arc::new(tokio::sync::Notify::new());
    let (native, task) = serve(Router::new().fallback(upstream).with_state(release.clone())).await;
    let (url, proxy) = serve(
        super::super::router(Config {
            gemini: Some(provider(&native, "bearer")),
            ..Config::default()
        })
        .unwrap(),
    )
    .await;
    let mut response = reqwest::Client::new()
        .post(format!("{url}/v1/responses"))
        .json(&json!({"model":"gemini/test","input":"hi","stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["x-accel-buffering"], "no");
    let mut bytes = Vec::new();
    tokio::time::timeout(HEARTBEAT_INTERVAL + Duration::from_secs(5), async {
        loop {
            bytes.extend_from_slice(&response.chunk().await.unwrap().unwrap());
            if bytes
                .windows(b": keepalive\n\n".len())
                .any(|w| w == b": keepalive\n\n")
            {
                break;
            }
        }
    })
    .await
    .expect("idle stream did not emit a heartbeat");
    release.notify_one();
    bytes.extend_from_slice(&response.bytes().await.unwrap());
    let text = String::from_utf8(bytes).unwrap();
    let events = events(&text);
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    assert_eq!(
        events.last().unwrap()["response"]["usage"]["output_tokens"],
        2
    );
    task.abort();
    proxy.abort();
}
