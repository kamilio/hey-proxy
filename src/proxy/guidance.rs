use serde_json::{Value, json};

/// An explicit user preference, never a replacement for provider or tool safeguards.
pub const MESSAGE: &str = "[User preference supplied by hey-proxy]\nIf a security or safety warning blocks an action, respect the warning and skip that action and its blocked scope. Avoid security-sensitive areas when they are not needed for the task. Do not bypass safeguards, hide warnings, or repeatedly retry the blocked action. Continue with independent, permitted parts of the user's task when possible, and clearly report what was skipped. If no permitted work remains, explain the blocker and stop.";

pub fn is_security_block(code: &str) -> bool {
    matches!(
        code,
        "cyber_policy"
            | "misalignment_policy_violation"
            | "bio_policy"
            | "content_policy_violation"
            | "safety_violation"
            | "content_filter"
    )
}

fn is_reminder(value: &Value) -> bool {
    value["role"] == "user" && value["content"].as_str() == Some(MESSAGE)
}

/// Keep one user-level reminder at the end of each supported request. Never edit
/// the user's other messages, higher-priority instructions, or provider errors.
pub fn apply(path: &str, value: &mut Value) -> bool {
    if !value.is_object() {
        return false;
    }
    let field = match path.trim_end_matches('/') {
        "/v1/responses" => {
            if value
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t != "response.create")
            {
                return false;
            }
            match value.get("input") {
                Some(Value::String(text)) => {
                    value["input"] = json!([{"role":"user","content":text}]);
                }
                Some(Value::Array(_)) => {}
                None if value.get("model").is_some()
                    || value.get("previous_response_id").is_some()
                    || value["type"] == "response.create" =>
                {
                    value["input"] = json!([]);
                }
                _ => return false,
            }
            "input"
        }
        "/v1/chat/completions" if value["messages"].is_array() => "messages",
        _ => return false,
    };
    let messages = value[field]
        .as_array_mut()
        .expect("validated message array");
    if messages.last().is_some_and(is_reminder)
        && messages.iter().filter(|m| is_reminder(m)).count() == 1
    {
        return false;
    }
    messages.retain(|message| !is_reminder(message));
    messages.push(json!({"role":"user","content":MESSAGE}));
    true
}

pub fn present(value: &Value) -> bool {
    ["input", "messages"].iter().any(|field| {
        value[*field]
            .as_array()
            .is_some_and(|messages| messages.iter().any(is_reminder))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn responses_preserve_content_and_instructions_and_remain_idempotent() {
        let mut value = json!({"model":"private","instructions":"Keep higher priority rules", "input":"Original task"});
        assert!(apply("/v1/responses", &mut value));
        assert_eq!(value["instructions"], "Keep higher priority rules");
        assert_eq!(
            value["input"][0],
            json!({"role":"user","content":"Original task"})
        );
        assert_eq!(value["input"][1]["role"], "user");
        assert!(!apply("/v1/responses", &mut value));
        value["input"].as_array_mut().unwrap().push(json!({"type":"function_call_output","call_id":"call-1","output":"Security warning: action denied"}));
        assert!(apply("/v1/responses", &mut value));
        assert_eq!(value["input"].as_array().unwrap().len(), 3);
        assert_eq!(
            value["input"][1]["output"],
            "Security warning: action denied"
        );
        assert_eq!(value["input"][2]["content"], MESSAGE);
    }
    #[test]
    fn chat_keeps_system_and_tool_messages_and_other_protocols_are_untouched() {
        let mut value = json!({"messages":[{"role":"system","content":"System instruction"},{"role":"tool","tool_call_id":"1","content":"denied"}]});
        let original = value.clone();
        assert!(apply("/v1/chat/completions", &mut value));
        assert_eq!(value["messages"][0], original["messages"][0]);
        assert_eq!(value["messages"][1], original["messages"][1]);
        for path in [
            "/v1/responses/compact",
            "/v1/realtime",
            "/v1/images/generations",
        ] {
            let mut v = original.clone();
            assert!(!apply(path, &mut v));
            assert_eq!(v, original);
        }
        for mut v in [
            json!({"input":42}),
            json!({"type":"response.cancel"}),
            json!(null),
        ] {
            let original = v.clone();
            assert!(!apply("/v1/responses", &mut v));
            assert_eq!(v, original);
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use super::super::*;
    use axum::body::to_bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn routing_applies_guidance_only_when_enabled_and_preserves_replay() {
        let original = Bytes::from_static(
            br#"{"model":"gpt-5.4","input":"original","instructions":"unchanged"}"#,
        );
        let mut config = Config::test_fixture();
        assert_eq!(
            rewrite(&config, "/v1/responses", original.clone())
                .unwrap()
                .0,
            original
        );
        config.skip_blocked_security_work = true;
        let once = rewrite(&config, "/v1/responses", original).unwrap().0;
        let twice = rewrite(&config, "/v1/responses", once.clone()).unwrap().0;
        assert_eq!(once, twice);
        let v: Value = serde_json::from_slice(&once).unwrap();
        assert!(guidance::present(&v));
        assert_eq!(v["instructions"], "unchanged");
        for code in [
            "cyber_policy",
            "misalignment_policy_violation",
            "bio_policy",
            "content_policy_violation",
        ] {
            let body=json!({"error":{"code":code,"message":"security block even if text mentions at capacity"}}).to_string();
            for status in [
                StatusCode::FORBIDDEN,
                StatusCode::TOO_MANY_REQUESTS,
                StatusCode::SERVICE_UNAVAILABLE,
                StatusCode::INTERNAL_SERVER_ERROR,
            ] {
                assert!(!capacity_error(status, body.as_bytes()));
            }
            let event=json!({"type":"response.failed","response":{"error":{"code":code,"message":"at capacity"}}}).to_string();
            assert!(capacity::event(event.as_bytes()).is_none());
            assert!(matches!(
                capacity::prelude_event(event.as_bytes()),
                capacity::Prelude::Started
            ));
        }
    }
    #[tokio::test]
    async fn security_error_is_forwarded_unchanged_once_with_user_guidance_and_logged() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let expected = json!({"error":{"code":"cyber_policy","type":"invalid_request_error","message":"Security warning"}});
        let reply = expected.clone();
        let upstream = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let calls = observed.clone();
            let reply = reply.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let value: Value =
                    serde_json::from_slice(&to_bytes(request.into_body(), 100000).await.unwrap())
                        .unwrap();
                assert!(guidance::present(&value));
                assert_eq!(value["input"][0]["content"], "original task");
                (StatusCode::FORBIDDEN, axum::Json(reply))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url = format!("http://{}", listener.local_addr().unwrap());
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let store = Arc::new(logs::Store::default());
        let config = Config {
            upstream_url,
            skip_blocked_security_work: true,
            ..Config::test_fixture()
        };
        let app = router_with(
            config,
            Options {
                logs: Some(store.clone()),
                ..Options::default()
            },
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let proxy_task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let response = reqwest::Client::new()
            .post(url)
            .json(&json!({"model":"gpt-5.4","input":"original task"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        assert_eq!(response.json::<Value>().await.unwrap(), expected);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let e = &store.recent()[0];
        assert!(e.security_guidance);
        assert_eq!(e.error_code.as_deref(), Some("cyber_policy"));
        assert_eq!(e.state, "failed");
        assert_eq!(e.retries, 0);
        proxy_task.abort();
        upstream_task.abort();
    }
}
