use super::*;
use crate::proxy::*;
use axum::routing::any;
use serde_json::json;

#[test]
fn only_foreign_reasoning_at_the_responses_input_boundary_is_removed() {
    let input = json!([
        {"type":"reasoning","encrypted_content":"hey_gemini_v1.synthetic"},
        {"type":"reasoning","encrypted_content":"openai-opaque"},
        {"type":"reasoning","encrypted_content":"unknown-codec"},
        {"type":"compaction","encrypted_content":"hey_gemini_v1.not-a-reasoning-item"},
        {"type":"message","role":"user","content":"hey_gemini_v1.literal-instructions"},
        {"type":"function_call","call_id":"c","name":"exec_command","arguments":"{\"cmd\":\"git status --short\"}"},
        {"type":"function_call_output","call_id":"c","output":{"type":"reasoning","encrypted_content":"hey_gemini_v1.tool-data"}}
    ]);
    for (path, model, changes) in [
        ("/v1/responses", "reviewer", true),
        ("/v1/responses/", "reviewer", true),
        ("/responses", "reviewer", true),
        ("/v1/responses", "gemini/test", false),
        ("/v1/responses/compact", "reviewer", false),
        ("/v1/chat/completions", "reviewer", false),
        ("/v1/other", "reviewer", false),
    ] {
        let mut request = json!({"model":model,"input":input,"instructions":"Keep the complete action and policy."});
        let mut expected = request.clone();
        if changes {
            expected["input"].as_array_mut().unwrap().remove(0);
        }
        assert_eq!(for_openai(path, &mut request), changes);
        assert_eq!(request, expected);
        assert!(!for_openai(path, &mut request));
    }
    for mut request in [
        json!({"input":input}),
        json!({"model":"reviewer","input":"hello"}),
    ] {
        let expected = request.clone();
        assert!(!for_openai("/v1/responses", &mut request));
        assert_eq!(request, expected);
    }
}

#[test]
fn gemini_item_ids_are_dropped_when_a_thread_moves_to_openai() {
    let hex = "20f22b2775b9994b15d85c7f315f9aa8";
    let mut request = json!({"model":"ultima-alpha","input":[
        {"type":"custom_tool_call","id":format!("fc_{hex}_2"),"call_id":"call_1","name":"apply_patch","input":"*** Begin Patch"},
        {"type":"custom_tool_call_output","call_id":"call_1","output":"ok"},
        {"type":"function_call","id":format!("fc_{hex}_0_1"),"call_id":"call_2","name":"exec_command","arguments":"{}"},
        {"type":"message","id":format!("msg_{hex}_3"),"role":"assistant","content":[{"type":"output_text","text":"done"}]},
        {"type":"function_call","id":"fc_68d1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f7","call_id":"call_3","name":"exec_command","arguments":"{}"},
        {"type":"message","id":format!("msg_{hex}"),"role":"assistant","content":[]}
    ]});
    assert!(for_openai("/v1/responses", &mut request));
    let ids: Vec<_> = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.get("id").cloned())
        .collect();
    assert_eq!(
        ids,
        [
            None,
            None,
            None,
            None,
            Some(json!("fc_68d1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f7")),
            Some(json!(format!("msg_{hex}")))
        ]
    );
    assert!(!for_openai("/v1/responses", &mut request));
}

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(app: Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    Server {
        url,
        task: tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    }
}

type Seen = Arc<Mutex<Vec<(String, HeaderMap, Value)>>>;
async fn fixture(fallback: bool) -> (Server, Server, Seen) {
    let seen: Seen = Default::default();
    let captured = seen.clone();
    let upstream = serve(Router::new().fallback(any(move |request: Request| {
        let captured = captured.clone();
        async move {
            let (parts, body) = request.into_parts();
            let body: Value = serde_json::from_slice(&axum::body::to_bytes(body, 1024 * 1024).await.unwrap()).unwrap();
            captured.lock().unwrap().push((parts.uri.path().into(), parts.headers, body.clone()));
            if parts.uri.path().contains(":generateContent") {
                return axum::Json(json!({"candidates":[{"content":{"role":"model","parts":[
                    {"text":"Inspect the authorized repository only.","thought":true,"thoughtSignature":"synthetic-thought"},
                    {"text":"I will inspect git status.","thoughtSignature":"synthetic-text"}
                ]},"finishReason":"STOP"}]})).into_response();
            }
            if body["input"].as_array().unwrap().iter().any(|item| item["encrypted_content"].is_string()) {
                return (StatusCode::BAD_REQUEST, axum::Json(json!({"error":{"code":"invalid_encrypted_content","type":"invalid_request_error"}}))).into_response();
            }
            if fallback {
                return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({"error":{"code":"server_error"}}))).into_response();
            }
            // Synthetic reviewer: assert the policy and full action survived routing.
            assert_eq!(body["instructions"], "Review only. Allow the authorized status inspection; reject destructive changes. Never execute the action.");
            let action = body["input"].as_array().unwrap().last().unwrap();
            let decision = if action["content"] == "Proposed action: git status --short" { "allow" } else { "deny" };
            let response = json!({"id":"review","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":decision}]}]});
            if body["stream"] == true {
                return ([(header::CONTENT_TYPE, "text/event-stream")], format!("event: response.completed\ndata: {}\n\n", json!({"type":"response.completed","response":response}))).into_response();
            }
            axum::Json(response).into_response()
        }
    }))).await;
    let mut config: Config = serde_json::from_value(json!({
        "listen":"127.0.0.1:0",
        "providers":{
            "openai":{"upstream_url":upstream.url,"api_keys":{"alpha":"synthetic-alpha","codex":"synthetic-codex"},"default":{"api_key":"alpha"}},
            "gemini":{"upstream_url":upstream.url,"api_key":"synthetic-gemini"}
        },
        "aliases":[{"from":"parent","to":"gemini/test"},{"from":"reviewer","to":"review-model","api_key":"codex"}]
    })).unwrap();
    if fallback {
        config
            .fallbacks
            .insert("review-model".into(), vec!["gemini/test".into()]);
    }
    let proxy = serve(
        router_with(
            config,
            Options {
                connectivity_probes: vec![],
                ..Options::default()
            },
        )
        .unwrap(),
    )
    .await;
    (proxy, upstream, seen)
}
async fn post(proxy: &Server, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/responses", proxy.url))
        .header("x-codex-guardian", "reviewer")
        .json(body)
        .send()
        .await
        .unwrap()
}
async fn review_history(proxy: &Server) -> Value {
    let user =
        json!({"role":"user","content":"Inspect git status only; do not change or delete files."});
    let response = post(
        proxy,
        &json!({"model":"parent","input":[user],"store":false}),
    )
    .await;
    assert_eq!(response.status(), 200);
    let response: Value = response.json().await.unwrap();
    assert!(
        response["output"][0]["encrypted_content"]
            .as_str()
            .unwrap()
            .starts_with(CARRIER_PREFIX)
    );
    let mut input = vec![user];
    input.extend(response["output"].as_array().unwrap().iter().cloned());
    input.push(json!({"role":"user","content":"Proposed action: git status --short"}));
    json!({"model":"reviewer","store":false,"reasoning":{"effort":"low"},
        "instructions":"Review only. Allow the authorized status inspection; reject destructive changes. Never execute the action.",
        "input":input})
}

#[tokio::test]
async fn review_preserves_policy_action_and_routing_for_allow_and_deny() {
    let (proxy, _upstream, seen) = fixture(false).await;
    let mut request = review_history(&proxy).await;
    for (stream, action, decision) in [
        (false, "git status --short", "allow"),
        (true, "git reset --hard && git clean -fd", "deny"),
    ] {
        request["stream"] = json!(stream);
        request["input"].as_array_mut().unwrap().last_mut().unwrap()["content"] =
            json!(format!("Proposed action: {action}"));
        let before = request.clone();
        let response = post(&proxy, &request).await;
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        assert!(text.contains(decision));
        let records = seen.lock().unwrap();
        let (_, headers, actual) = records.last().unwrap();
        let mut expected = request.clone();
        expected["model"] = json!("review-model");
        expected["input"].as_array_mut().unwrap().remove(1);
        expected["input"][1].as_object_mut().unwrap().remove("id");
        assert_eq!(*actual, expected);
        assert_eq!(headers["authorization"], "Bearer synthetic-codex");
        assert_eq!(headers["x-codex-guardian"], "reviewer");
        assert_eq!(request, before);
    }
}

#[tokio::test]
async fn native_replay_stays_authenticated_and_fallback_keeps_the_original_carrier() {
    let (proxy, _upstream, seen) = fixture(true).await;
    let request = review_history(&proxy).await;
    // The OpenAI attempt fails transiently. Gemini must still receive its own
    // exact signed turn from the original history, not the filtered HTTP body.
    let response = post(&proxy, &request).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-hey-proxy-fallback-count"], "1");
    let _: Value = response.json().await.unwrap();
    let records = seen.lock().unwrap().clone();
    assert_eq!(records.len(), 3);
    let parts = records[2].2["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|content| content["parts"].as_array().unwrap())
        .collect::<Vec<_>>();
    assert!(
        parts
            .iter()
            .any(|p| p["thoughtSignature"] == "synthetic-thought")
    );
    assert!(
        parts
            .iter()
            .any(|p| p["thoughtSignature"] == "synthetic-text")
    );
    for alteration in ["carrier", "visible"] {
        let mut invalid = request.clone();
        invalid["model"] = json!("gemini/test");
        match alteration {
            "carrier" => invalid["input"][1]["encrypted_content"] = json!("hey_gemini_v1.altered"),
            "visible" => invalid["input"][2]["content"][0]["text"] = json!("Changed signed output"),
            _ => unreachable!(),
        }
        let response = post(&proxy, &invalid).await;
        assert_eq!(response.status(), 400, "{alteration}");
        let _ = response.bytes().await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 3);
    }
    // Model changes intentionally import visible history without reusing the old
    // model's signed native turn. This differs from altered authenticated output.
    let mut switched = request.clone();
    switched["model"] = json!("gemini/another");
    let response = post(&proxy, &switched).await;
    assert_eq!(response.status(), 200);
    let _: Value = response.json().await.unwrap();
    let records = seen.lock().unwrap();
    assert_eq!(records.len(), 4);
    assert!(records[3].0.contains("/models/another:"));
    let history = records[3].2["contents"].to_string();
    assert!(history.contains("I will inspect git status."));
    assert!(history.contains("Proposed action: git status --short"));
    assert!(!history.contains("synthetic-thought"));
    assert!(!history.contains("synthetic-text"));
}

#[tokio::test]
async fn native_openai_errors_are_preserved_without_retry_or_fallback() {
    let (proxy, _upstream, seen) = fixture(true).await;
    let request = review_history(&proxy).await;
    for kind in ["reasoning", "compaction"] {
        let mut invalid = request.clone();
        invalid["input"][1] =
            json!({"type":kind,"id":"opaque-id","encrypted_content":"opaque-native-content"});
        let before = seen.lock().unwrap().len();
        let response = post(&proxy, &invalid).await;
        assert_eq!(response.status(), 400);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "invalid_encrypted_content"
        );
        let records = seen.lock().unwrap();
        assert_eq!(records.len(), before + 1);
        let mut expected = invalid["input"].clone();
        expected[2].as_object_mut().unwrap().remove("id");
        assert_eq!(records.last().unwrap().2["input"], expected);
    }
}

#[tokio::test]
async fn client_relay_does_not_guess_the_hosts_provider_route() {
    let seen: Seen = Default::default();
    let captured = seen.clone();
    let upstream = serve(Router::new().fallback(any(
        move |axum::Json(body): axum::Json<Value>| {
            captured
                .lock()
                .unwrap()
                .push((String::new(), HeaderMap::new(), body));
            async { axum::Json(json!({"status":"completed","output":[]})) }
        },
    )))
    .await;
    let config: Config = serde_json::from_value(
        json!({"listen":"127.0.0.1:0","mode":"client","connection":{"url":upstream.url,"api_key":"synthetic"}}),
    )
    .unwrap();
    let proxy = serve(router_with(config, Options::default()).unwrap()).await;
    let request = json!({"model":"host-alias","input":[{"type":"reasoning","encrypted_content":"hey_gemini_v1.opaque"}]});
    assert_eq!(post(&proxy, &request).await.status(), 200);
    assert_eq!(seen.lock().unwrap()[0].2, request);
}
