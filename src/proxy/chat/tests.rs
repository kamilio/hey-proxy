use super::*;
use axum::routing::any;
use hey_proxy::gemini::{ProviderConfig, ReasoningCodec, convert_request, convert_response};

fn codec() -> ReasoningCodec {
    ReasoningCodec::new(&[7; 32])
}
fn chat() -> Value {
    json!({"model":"friendly","messages":[{"role":"user","content":"hello"}]})
}
fn reply(output: Value) -> Value {
    json!({"id":"resp_test","created_at":123,"status":"completed","model":"upstream-model","output":output,"usage":{"input_tokens":8,"output_tokens":5,"total_tokens":13,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":3}}})
}
fn text_item(text: &str) -> Value {
    json!({"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":text}]})
}
fn provider() -> ProviderConfig {
    serde_json::from_value(json!({"upstream_url":"http://localhost","api_key":"synthetic"}))
        .unwrap()
}
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
async fn collect_sse(response: Response) -> Vec<Value> {
    let bytes = axum::body::to_bytes(response.into_body(), LIMIT)
        .await
        .unwrap();
    assert!(bytes.ends_with(b"data: [DONE]\n\n"));
    super::super::sse::SseDecoder::default()
        .feed(&bytes, true)
        .unwrap()
}
fn event_stream(events: &[Value]) -> Response {
    let bytes: Vec<u8> = events
        .iter()
        .flat_map(|v| {
            format!(
                "event: {}\r\ndata: {v}\r\n\r\n",
                v["type"].as_str().unwrap_or("error")
            )
            .into_bytes()
        })
        .collect();
    // Force split UTF-8, JSON, and CRLF boundaries.
    let chunks = bytes
        .into_iter()
        .map(|b| Ok::<_, std::io::Error>(Bytes::from(vec![b])));
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(futures_util::stream::iter(chunks)),
    )
        .into_response()
}
fn merged_message(chunks: &[Value]) -> Value {
    let mut message = json!({"role":"assistant","content":""});
    let mut calls: std::collections::BTreeMap<usize, Value> = Default::default();
    let mut details: std::collections::BTreeMap<usize, Value> = Default::default();
    for chunk in chunks {
        let delta = &chunk["choices"][0]["delta"];
        for field in ["content", "reasoning", "refusal"] {
            if let Some(text) = delta[field].as_str() {
                let previous = message[field].as_str().unwrap_or("");
                message[field] = json!(previous.to_owned() + text);
            }
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let item = calls
                .entry(call["index"].as_u64().unwrap() as usize)
                .or_insert_with(|| json!({"type":"function","function":{"arguments":""}}));
            if call["id"].is_string() {
                item["id"] = call["id"].clone();
                item["function"]["name"] = call["function"]["name"].clone();
            }
            let args = item["function"]["arguments"].as_str().unwrap().to_owned();
            item["function"]["arguments"] =
                json!(args + call["function"]["arguments"].as_str().unwrap_or(""));
        }
        for detail in delta["reasoning_details"].as_array().into_iter().flatten() {
            let item = details
                .entry(detail["index"].as_u64().unwrap() as usize)
                .or_insert_with(|| {
                    let mut v = detail.clone();
                    v["summary"] = json!("");
                    v["data"] = json!("");
                    v
                });
            for field in ["summary", "data"] {
                if let Some(text) = detail[field].as_str() {
                    let prior = item[field].as_str().unwrap().to_owned();
                    item[field] = json!(prior + text);
                }
            }
        }
    }
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls.into_values().collect::<Vec<_>>());
    }
    if !details.is_empty() {
        message["reasoning_details"] = json!(details.into_values().collect::<Vec<_>>());
    }
    message
}

#[test]
fn chat_request_translates_messages_tools_images_schema_and_reasoning() {
    let input = json!({"model":"friendly","messages":[
        {"role":"system","content":"Be useful"},
        {"role":"user","content":[{"type":"text","text":"Look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA==","detail":"low"}}]},
        {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}]},
        {"role":"tool","tool_call_id":"call_1","content":"sunny"}],
        "tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}],
        "tool_choice":{"type":"function","function":{"name":"weather"}},
        "response_format":{"type":"json_schema","json_schema":{"name":"result","strict":true,"schema":{"type":"object"}}},
        "max_tokens":50,"max_completion_tokens":100,"reasoning":{"effort":"low"},"stream":true,"stream_options":{"include_usage":true}});
    let converted = request::convert(&input, "upstream-model", &codec()).unwrap();
    assert!(converted.stream && converted.include_usage);
    let body = converted.body;
    assert_eq!(body["input"][0]["role"], "system");
    assert_eq!(body["input"][1]["content"][1]["type"], "input_image");
    assert_eq!(body["input"][2]["type"], "function_call");
    assert_eq!(body["input"][3]["type"], "function_call_output");
    assert_eq!(body["tools"][0]["strict"], false);
    assert_eq!(
        body["tool_choice"],
        json!({"type":"function","name":"weather"})
    );
    assert_eq!(body["text"]["format"]["type"], "json_schema");
    assert_eq!(body["reasoning"], json!({"effort":"low","summary":"auto"}));
    assert_eq!(body["max_output_tokens"], 100);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["store"], false);
    assert!(
        request::convert(&chat(), "gpt-4.1", &codec())
            .unwrap()
            .body
            .get("reasoning")
            .is_none()
    );
}

#[test]
fn chat_rejects_untranslatable_or_malformed_options() {
    for (key, value) in [
        ("n", json!(2)),
        ("stop", json!(["END"])),
        ("logprobs", json!(true)),
        ("frequency_penalty", json!(0.5)),
        ("stream", json!("true")),
        ("reasoning", json!({"max_tokens":100})),
        ("tools", json!({})),
        ("max_tokens", json!(-1)),
    ] {
        let mut input = chat();
        input[key] = value;
        assert!(request::convert(&input, "x", &codec()).is_err(), "{key}");
    }
    for input in [
        json!(null),
        json!([]),
        json!({"model":"x","messages":[null]}),
        json!({"model":"x","messages":[]}),
    ] {
        assert!(request::convert(&input, "x", &codec()).is_err());
    }
}

#[test]
fn chat_openai_reasoning_round_trips_opaque_data_and_tool_calls() {
    let reasoning = json!({"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Check the weather"}],"encrypted_content":"opaque-provider-bytes"});
    let call = json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"weather","arguments":"{}"});
    let value = reply(json!([reasoning, call]));
    let result = response::completion(&value, "friendly", false).unwrap();
    assert_eq!(result["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        result["usage"]["completion_tokens_details"]["reasoning_tokens"],
        3
    );
    let message = result["choices"][0]["message"].clone();
    assert_eq!(message["reasoning"], "Check the weather");
    let converted=request::convert(&json!({"model":"friendly","messages":[message,{"role":"tool","tool_call_id":"call_1","content":"sunny"}]}),"upstream-model",&codec()).unwrap();
    assert_eq!(converted.body["input"][0], reasoning);
    assert_eq!(converted.body["input"][1]["call_id"], "call_1");
    assert_eq!(converted.body["input"][2]["type"], "function_call_output");
}

#[test]
fn chat_gemini_replays_signed_boundaries_and_rejects_tampering() {
    let codec = codec();
    let provider = provider();
    let original=request::convert(&json!({"model":"gemini/test","messages":[{"role":"user","content":"weather?"}],"tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object","properties":{}}}}]}),"gemini/test",&codec).unwrap();
    let native_request = convert_request(&original.body, &provider, &codec).unwrap();
    let name = &native_request.body["tools"][0]["functionDeclarations"][0]["name"];
    let native = json!({"candidates":[{"content":{"role":"model","parts":[{"text":"Thinking","thought":true,"thoughtSignature":"signed-thought"},{"text":"I will check. ","thoughtSignature":"signed-text"},{"text":"One moment."},{"functionCall":{"name":name,"args":{},"id":"native_call"},"thoughtSignature":"signed-call"}]},"finishReason":"STOP"}]});
    let responses = convert_response(&native, &native_request, &codec, "signed").unwrap();
    let reply = response::completion(&responses, "gemini/test", false).unwrap();
    let message = reply["choices"][0]["message"].clone();
    assert_eq!(
        message["reasoning_details"][1]["format"],
        "google-gemini-v1"
    );
    let mut next = json!({"model":"gemini/test","messages":[{"role":"user","content":"weather?"},message,{"role":"tool","tool_call_id":"native_call","content":"sunny"}]});
    let converted = request::convert(&next, "gemini/test", &codec).unwrap();
    let native_replay = convert_request(&converted.body, &provider, &codec).unwrap();
    assert_eq!(
        native_replay.body["contents"][1]["parts"],
        native["candidates"][0]["content"]["parts"]
    );
    assert_eq!(
        native_replay.body["contents"][2]["parts"][0]["functionResponse"]["name"],
        *name
    );
    next["messages"][1]["content"] = json!("tampered");
    assert!(request::convert(&next, "gemini/test", &codec).is_err());
    next["messages"][1] = message;
    assert!(request::convert(&next, "gemini/another", &codec).is_err());
    assert!(request::convert(&next, "gpt-model", &codec).is_err());
}

#[tokio::test]
async fn chat_stream_preserves_unicode_parallel_tools_reasoning_usage_and_finish() {
    let reasoning = json!({"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Think 🌍"}],"encrypted_content":"opaque"});
    let call = |id: &str, name: &str, args: &str| json!({"type":"function_call","id":format!("fc_{id}"),"call_id":id,"name":name,"arguments":args});
    let response = reply(json!([
        reasoning,
        text_item("Hello 🌍"),
        call("a", "first", "{\"x\":1}"),
        call("b", "second", "{}")
    ]));
    let events = vec![
        json!({"type":"response.created","response":{"id":"resp_test","model":"upstream-model","created_at":123}}),
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"item_id":"rs_1","delta":"Think 🌍"}),
        json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"Hello "}),
        json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"🌍"}),
        json!({"type":"response.output_item.added","output_index":2,"item":call("a","first","")}),
        json!({"type":"response.output_item.added","output_index":3,"item":call("b","second","")}),
        json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"x\":"}),
        json!({"type":"response.function_call_arguments.delta","output_index":3,"delta":"{}"}),
        json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"1}"}),
        json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
        json!({"type":"response.completed","response":response}),
    ];
    let chunks = collect_sse(stream::adapt(
        event_stream(&events),
        "friendly".into(),
        true,
        false,
    ))
    .await;
    assert!(
        chunks.iter().all(|v| v.get("error").is_none()),
        "{chunks:?}"
    );
    let merged = merged_message(&chunks);
    assert_eq!(merged["content"], "Hello 🌍");
    assert_eq!(merged["reasoning"], "Think 🌍");
    assert_eq!(merged["reasoning_details"][1]["data"], "opaque");
    assert_eq!(
        merged["tool_calls"][0]["function"]["arguments"],
        "{\"x\":1}"
    );
    assert_eq!(merged["tool_calls"][1]["function"]["name"], "second");
    assert_eq!(
        chunks[chunks.len() - 2]["choices"][0]["finish_reason"],
        "tool_calls"
    );
    assert_eq!(chunks.last().unwrap()["usage"]["total_tokens"], 13);
    assert!(chunks.iter().all(|v| v["id"] == "chatcmpl-resp_test"));
}

#[tokio::test]
async fn chat_stream_gemini_signed_tool_round_trip() {
    let codec = codec();
    let provider = provider();
    let request=convert_request(&json!({"model":"gemini/test","input":"weather?","stream":true,"tools":[{"type":"function","name":"weather","parameters":{"type":"object","properties":{}}}]}),&provider,&codec).unwrap();
    let name = request.body["tools"][0]["functionDeclarations"][0]["name"].clone();
    let native = json!({"candidates":[{"content":{"role":"model","parts":[{"text":"Check weather","thought":true},{"functionCall":{"name":name,"args":{}},"thoughtSignature":"signed"}]},"finishReason":"STOP"}]});
    let mut converter = hey_proxy::gemini::ResponseStream::new(request, "stream");
    let mut events = converter.feed(&native).unwrap();
    events.extend(converter.finish(&codec).unwrap());
    let chunks = collect_sse(stream::adapt(
        event_stream(&events),
        "gemini/test".into(),
        false,
        false,
    ))
    .await;
    assert!(
        chunks.iter().all(|v| v.get("error").is_none()),
        "{chunks:?}"
    );
    let message = merged_message(&chunks);
    let id = message["tool_calls"][0]["id"].clone();
    let next=request::convert(&json!({"model":"gemini/test","messages":[message,{"role":"tool","tool_call_id":id,"content":"sunny"}]}),"gemini/test",&codec).unwrap();
    let replay = convert_request(&next.body, &provider, &codec).unwrap();
    assert_eq!(
        replay.body["contents"][0]["parts"],
        native["candidates"][0]["content"]["parts"]
    );
}

#[tokio::test]
async fn chat_stream_reports_failure_and_truncation_without_success() {
    for events in [
        vec![json!({"type":"response.output_text.delta","delta":"partial","output_index":0})],
        vec![
            json!({"type":"response.failed","response":{"error":{"message":"upstream failed","code":"test_failure"}}}),
        ],
    ] {
        let chunks = collect_sse(stream::adapt(
            event_stream(&events),
            "x".into(),
            true,
            false,
        ))
        .await;
        assert!(chunks.last().unwrap().get("error").is_some());
        assert!(
            !chunks
                .iter()
                .any(|c| c["choices"][0]["finish_reason"].is_string())
        );
    }
    let mut incomplete = reply(json!([text_item("partial")]));
    incomplete["status"] = json!("incomplete");
    incomplete["incomplete_details"] = json!({"reason":"max_output_tokens"});
    let chunks = collect_sse(stream::adapt(
        event_stream(&[json!({"type":"response.incomplete","response":incomplete})]),
        "x".into(),
        false,
        false,
    ))
    .await;
    assert_eq!(
        chunks.last().unwrap()["choices"][0]["finish_reason"],
        "length"
    );
}

#[tokio::test]
async fn chat_custom_route_uses_chat_alias_shape_and_leaves_standard_route_unchanged() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let seen = captured.clone();
    let upstream = Router::new().fallback(any(move |request: Request| {
        let seen = seen.clone();
        async move {
            let (parts, body) = request.into_parts();
            if parts.uri.path() == "/v1/responses" {
                assert_eq!(parts.headers["accept-encoding"], "identity");
                assert!(!parts.headers.contains_key("digest"));
            }
            let input: Value =
                serde_json::from_slice(&axum::body::to_bytes(body, LIMIT).await.unwrap()).unwrap();
            seen.lock().unwrap().push((
                parts.uri.to_string(),
                parts.headers["authorization"].to_str().unwrap().to_owned(),
                input,
            ));
            axum::Json(reply(json!([text_item("OK")])))
        }
    }));
    let (upstream, upstream_task) = serve(upstream).await;
    let config=Config { upstream_url:upstream, aliases:serde_json::from_value(json!([
        {"from":"friendly","to":"responses-only","api_shape":"responses"},
        {"from":"friendly","to":"chat-target","api_shape":"chat_completions","api_key":"primary","reasoning":"low"}
    ])).unwrap(),..Config::test_fixture() };
    let (url, task) = serve(router(config).unwrap()).await;
    let client = reqwest::Client::new();
    let result = client
        .post(format!("{url}/v1/custom/chat/completions"))
        .header("accept-encoding", "gzip, br")
        .header("digest", "original-body-checksum")
        .json(&chat())
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 200);
    let result: Value = result.json().await.unwrap();
    assert_eq!(result["choices"][0]["message"]["content"], "OK");
    assert_eq!(result["model"], "friendly");
    client
        .post(format!("{url}/v1/chat/completions"))
        .json(&chat())
        .send()
        .await
        .unwrap();
    let seen = captured.lock().unwrap();
    assert_eq!(seen[0].0, "/v1/responses");
    assert_eq!(seen[0].1, "Bearer replace-with-primary-api-key");
    assert_eq!(seen[0].2["model"], "chat-target");
    assert_eq!(seen[0].2["reasoning"]["effort"], "low");
    assert!(seen[0].2.get("messages").is_none());
    assert_eq!(seen[1].0, "/v1/chat/completions");
    assert!(seen[1].2.get("messages").is_some());
    task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn chat_custom_route_runs_through_gemini_backend() {
    let upstream=Router::new().fallback(any(|request:Request| async move {
        assert_eq!(request.uri().path(),"/models/test:generateContent");
        let input:Value=serde_json::from_slice(&axum::body::to_bytes(request.into_body(),LIMIT).await.unwrap()).unwrap();
        assert_eq!(input["contents"][0]["parts"][0]["text"],"hello");
        axum::Json(json!({"candidates":[{"content":{"role":"model","parts":[{"text":"OK","thoughtSignature":"signed"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":1,"totalTokenCount":5}}))
    }));
    let (upstream, upstream_task) = serve(upstream).await;
    let config = Config {
        gemini: Some(ProviderConfig {
            upstream_url: upstream,
            ..provider()
        }),
        ..Config::test_fixture()
    };
    let (url, task) = serve(router(config).unwrap()).await;
    let mut input = chat();
    input["model"] = json!("gemini/test");
    let result = reqwest::Client::new()
        .post(format!("{url}/v1/custom/chat/completions"))
        .json(&input)
        .send()
        .await
        .unwrap();
    let status = result.status();
    let body: Value = result.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["choices"][0]["message"]["content"], "OK");
    assert_eq!(
        body["choices"][0]["message"]["reasoning_details"][0]["format"],
        "google-gemini-v1"
    );
    assert_eq!(body["usage"]["prompt_tokens"], 4);
    task.abort();
    upstream_task.abort();
}

#[test]
fn api_shape_gates_all_overrides_url_routing_and_log_evidence() {
    let config = Config {aliases: serde_json::from_value(json!([
        {"from":"friendly","to":"primary","api_key":"primary","reasoning":"high","api_shape":"responses","reasoning_routes":{"low":{"to":"low-target"}}}
    ])).unwrap(),..Config::test_fixture()};
    for path in [
        "/v1/chat/completions",
        "/v1/completions",
        "/v1/realtime",
        "/v1/models/friendly",
        "/v1/audio/transcriptions",
        "/v1/custom/chat/completions",
    ] {
        let bytes = Bytes::from_static(
            br#"{"model":"friendly","reasoning_effort":"low","reasoning":{"effort":"low"}}"#,
        );
        let (result, key) = rewrite(&config, path, bytes.clone()).unwrap();
        assert_eq!(result, bytes, "{path}");
        assert_eq!(key, "default");
        let mut project = "default".to_owned();
        let uri = format!("{path}?model=friendly").parse().unwrap();
        assert_eq!(
            rewrite_uri(&config, &uri, &mut project),
            format!("{path}?model=friendly")
        );
        assert_eq!(project, "default");
        let store = logs::Store::default();
        let id = store.begin("POST", path, "HTTP");
        store.routing_decision(id, &config, path, &bytes);
        assert_eq!(store.recent()[0].route_rule.as_deref(), Some("passthrough"));
    }
    let (bytes, key) = rewrite(
        &config,
        "/v1/responses",
        Bytes::from_static(br#"{"model":"friendly","reasoning":{"effort":"low"}}"#),
    )
    .unwrap();
    let result: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(result["model"], "low-target");
    assert_eq!(result["reasoning"]["effort"], "high");
    assert_eq!(key, "primary");
}

#[tokio::test]
async fn chat_custom_fallback_keeps_incoming_shape_and_propagates_http_errors() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let upstream = Router::new().fallback(any(move |axum::Json(body): axum::Json<Value>| {
        let captured = captured.clone();
        async move {
            captured.lock().unwrap().push(body["model"].clone());
            if body["model"] == "first" {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(json!({"error":{"code":"overloaded","message":"busy"}})),
                )
                    .into_response();
            }
            if body["model"] == "denied" {
                return (
                    StatusCode::UNAUTHORIZED,
                    [("x-request-id", "req-denied")],
                    axum::Json(json!({"error":{"message":"insufficient permissions"}})),
                )
                    .into_response();
            }
            axum::Json(reply(json!([text_item("fallback OK")]))).into_response()
        }
    }));
    let (upstream, upstream_task) = serve(upstream).await;
    let config = Config {
        upstream_url: upstream,
        aliases: serde_json::from_value(json!([
            {"from":"friendly","to":"first","api_shape":"chat_completions"},
            {"from":"next","to":"wrong-shape","api_shape":"responses"},
            {"from":"next","to":"second","api_shape":"chat_completions"}
        ]))
        .unwrap(),
        fallbacks: serde_json::from_value(json!({"first":["next"]})).unwrap(),
        ..Config::test_fixture()
    };
    let (url, task) = serve(router(config).unwrap()).await;
    let client = reqwest::Client::new();
    let result = client
        .post(format!("{url}/v1/custom/chat/completions"))
        .json(&chat())
        .send()
        .await
        .unwrap();
    let status = result.status();
    let body: Value = result.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["choices"][0]["message"]["content"], "fallback OK");
    assert_eq!(*seen.lock().unwrap(), vec![json!("first"), json!("second")]);
    let mut input = chat();
    input["model"] = json!("denied");
    let result = client
        .post(format!("{url}/v1/custom/chat/completions"))
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 401);
    assert_eq!(result.headers()["x-request-id"], "req-denied");
    for (suffix, body, status) in [
        ("", json!({"model":"x","messages":[]}), 400),
        ("?stream=true", chat(), 400),
    ] {
        assert_eq!(
            client
                .post(format!("{url}/v1/custom/chat/completions{suffix}"))
                .json(&body)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            status
        );
    }
    assert_eq!(
        client
            .get(format!("{url}/v1/custom/chat/completions"))
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn chat_reasoning_text_stream_and_encrypted_item_order_survive_replay() {
    let reasoning = json!({"id":"rs_z","type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"Visible thought"}],"encrypted_content":"opaque-z"});
    let later = json!({"id":"rs_a","type":"reasoning","summary":[],"encrypted_content":"opaque-a"});
    let value = reply(json!([reasoning, later, text_item("OK")]));
    let events = vec![
        json!({"type":"response.created","response":{"id":"resp_test"}}),
        json!({"type":"response.reasoning_text.delta","output_index":0,"item_id":"rs_z","delta":"Visible thought"}),
        json!({"type":"response.completed","response":value}),
    ];
    let chunks = collect_sse(stream::adapt(
        event_stream(&events),
        "friendly".into(),
        false,
        false,
    ))
    .await;
    assert!(
        chunks.iter().all(|v| v.get("error").is_none()),
        "{chunks:?}"
    );
    let details: Vec<Value> = chunks
        .iter()
        .flat_map(|v| {
            v["choices"][0]["delta"]["reasoning_details"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned()
        })
        .collect();
    assert_eq!(details[0]["type"], "reasoning.text");
    assert_eq!(details[0]["text"], "Visible thought");
    let input = json!({"model":"friendly","messages":[{"role":"assistant","content":"OK","reasoning_details":details}]});
    let converted = request::convert(&input, "upstream", &codec()).unwrap();
    assert_eq!(converted.body["input"][0], reasoning);
    assert_eq!(converted.body["input"][1], later);
}

#[tokio::test]
async fn chat_stream_conversion_failure_is_recorded_as_failed() {
    let store = Arc::new(logs::Store::default());
    let id = store.begin("POST", "/v1/custom/chat/completions", "HTTP");
    // The upstream completed successfully, but its output cannot become chat text.
    let value = reply(json!([{"type":"image_generation_call","result":"opaque-image"}]));
    store.observe(id, &value);
    let upstream = event_stream(&[json!({"type":"response.completed","response":value})]);
    let chunks = collect_sse(stream::adapt_with_logs(
        upstream,
        "friendly".into(),
        false,
        false,
        Some((store.clone(), id)),
    ))
    .await;
    assert!(chunks.last().unwrap().get("error").is_some());
    store.finish(id, 200);
    store.complete(id, "succeeded", "stream_end", None, 100);
    assert_eq!(store.recent()[0].state, "failed");
    assert_eq!(
        store.recent()[0].error_code.as_deref(),
        Some("chat_stream_error")
    );
}

#[test]
fn chat_openrouter_enabled_setting_routes_like_its_converted_effort() {
    let config=Config {aliases:serde_json::from_value(json!([
        {"from":"friendly","to":"base","reasoning_routes":{"medium":{"to":"thinking"},"none":{"to":"quick"}}}
    ])).unwrap(),..Config::test_fixture()};
    for (enabled, expected) in [(true, "thinking"), (false, "quick")] {
        let mut input = chat();
        input["reasoning"] = json!({"enabled":enabled});
        let (routed, _) = rewrite(
            &config,
            "/v1/custom/chat/completions",
            Bytes::from(input.to_string()),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&routed).unwrap();
        assert_eq!(value["model"], expected);
        let converted = request::convert(&input, expected, &codec()).unwrap();
        let (routed, _) = rewrite(
            &config,
            "/v1/responses",
            Bytes::from(converted.body.to_string()),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&routed).unwrap();
        assert_eq!(value["model"], expected);
    }
}
