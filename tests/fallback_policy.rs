use hey_proxy::fallback::*;
use serde_json::{Value, json};

fn rules(value: Value) -> Fallbacks {
    serde_json::from_value(value).unwrap()
}

#[test]
fn validates_simple_fallbacks_without_model_definitions() {
    let config = rules(json!({"model-primary":["model-secondary"]}));
    validate(&config).unwrap();
    assert_eq!(
        targets(&config, "openai/model-primary"),
        ["model-secondary"]
    );
    assert!(targets(&config, "model-secondary").is_empty());
    let config = rules(json!({"openai/model-primary":["gemini/test"]}));
    validate(&config).unwrap();
    assert_eq!(targets(&config, "model-primary"), ["gemini/test"]);
    for value in [
        json!({"a":["a"]}),
        json!({"a":["b"],"b":["openai/a"]}),
        json!({"a":["b","openai/b"]}),
        json!({"a":[],"openai/a":[]}),
        json!({" ":[]}),
        json!({"a":["b\nc"]}),
        json!({"openai/":[]}),
    ] {
        assert!(validate(&rules(value)).is_err());
    }
    assert!(serde_json::from_value::<Fallbacks>(json!({"a":{"fallbacks":["b"]}})).is_err());
}

#[test]
fn validates_diamonds_and_bounds_depth_and_width() {
    validate(&rules(json!({"a":["b","c"],"b":["d"],"c":["d"]}))).unwrap();
    let mut config = Fallbacks::new();
    for n in 0..15 {
        config.insert(format!("m{n}"), vec![format!("m{}", n + 1)]);
    }
    validate(&config).unwrap();
    config.insert("m15".into(), vec!["m16".into()]);
    assert!(validate(&config).is_err());
    assert!(
        validate(&Fallbacks::from([(
            "a".into(),
            (0..16).map(|n| format!("m{n}")).collect()
        )]))
        .is_err()
    );
}

#[test]
fn only_transient_statuses_switch_and_error_codes_override_status() {
    for code in [408, 429, 500, 502, 503, 504, 529] {
        assert!(transient_http(code, b"unavailable"));
    }
    for code in [200, 400, 401, 403, 404, 409, 422, 501] {
        assert!(!transient_http(code, b"capacity"));
    }
    for code in [
        "insufficient_quota",
        "invalid_api_key",
        "billing_not_active",
        "context_length_exceeded",
        "content_policy_violation",
        "cyber_policy",
        "bio_policy",
        "misalignment_policy_violation",
        "security_policy",
        "safety_block",
        "invalid_prompt",
    ] {
        let bytes = serde_json::to_vec(&json!({"error":{"code":code}})).unwrap();
        for status in [429, 500, 503] {
            assert!(!transient_http(status, &bytes), "{code}");
        }
    }
}

#[test]
fn server_state_and_hosted_tools_are_never_replayed() {
    for body in [
        json!({"previous_response_id":"r"}),
        json!({"conversation":"c"}),
        json!({"background":true}),
        json!({"tools":[{"type":"mcp"}]}),
        json!({"tools":[{"type":"namespace","tools":[{"type":"computer"}]}]}),
    ] {
        assert!(replay_blocker(&body).is_some());
    }
    assert!(
        replay_blocker(&json!({"background":false,"previous_response_id":null,
        "tools":[{"type":"namespace","tools":[{"type":"function"},{"type":"custom"}]}]}))
        .is_none()
    );
}

#[test]
fn fragmented_sse_refusals_are_recognized_without_rescanning() {
    let bytes = b": ping\r\n\r\nevent: response.created\r\ndata: {\"type\":\"response.created\",\"response\":{\"output\":[]}}\r\n\r\nevent: response.failed\r\ndata: {\r\ndata: \"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\"}}}\r\n\r\n";
    for split in 1..=bytes.len() {
        let mut scanner = SsePrelude::new(false);
        let mut decision = Prelude::Waiting;
        for chunk in bytes.chunks(split) {
            decision = scanner.feed(chunk);
        }
        assert_eq!(decision, Prelude::Refused, "split {split}");
    }
}

#[test]
fn output_reasoning_tools_and_unknown_events_commit_even_if_failure_follows() {
    for event in [
        json!({"type":"response.output_text.delta","delta":"x"}),
        json!({"type":"response.reasoning_summary_text.delta","delta":"thinking"}),
        json!({"type":"response.output_item.added","item":{"type":"function_call","arguments":""}}),
        json!({"type":"future_event"}),
        json!({"type":"response.failed","response":{"output":[{"type":"reasoning","encrypted_content":"signed"}],"error":{"code":"server_error"}}}),
        json!({"type":"response.failed","response":{"usage":{"output_tokens":1},"error":{"code":"server_error"}}}),
    ] {
        let mut scanner = SsePrelude::new(false);
        let bytes =
            format!("data: {event}\n\ndata: {{\"error\":{{\"code\":\"server_error\"}}}}\n\n");
        assert_eq!(scanner.feed(bytes.as_bytes()), Prelude::Committed);
    }
}

#[test]
fn gemini_only_explicit_native_errors_are_refusals() {
    for (status, result) in [
        ("UNAVAILABLE", Prelude::Refused),
        ("RESOURCE_EXHAUSTED", Prelude::Refused),
        ("PERMISSION_DENIED", Prelude::Committed),
        ("INVALID_ARGUMENT", Prelude::Committed),
    ] {
        let bytes = serde_json::to_vec(&json!({"type":"response.failed","response":{"error":{"code":"server_error","gemini_status":status}}})).unwrap();
        assert_eq!(prelude_event(&bytes, true), result);
    }
    assert_eq!(prelude_event(br#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"conversion failed"}}}"#, true), Prelude::Committed);
}

#[test]
fn oversized_preludes_release_unchanged_without_switching() {
    let mut scanner = SsePrelude::new(false);
    assert_eq!(scanner.feed(&vec![b':'; MAX_PRELUDE]), Prelude::Committed);
    assert_eq!(
        scanner.feed(b"data: {\"error\":{\"code\":\"server_error\"}}\n\n"),
        Prelude::Committed
    );
}

#[test]
fn errors_on_lifecycle_events_are_not_hidden_and_nested_policy_errors_win() {
    for error in [
        json!({"code":"invalid_prompt"}),
        json!({"code":"unknown_error"}),
    ] {
        let value = json!({"type":"response.created","response":{"error":error}});
        assert_eq!(
            prelude_event(value.to_string().as_bytes(), false),
            Prelude::Committed
        );
    }
    assert!(!transient_http(
        503,
        br#"{"response":{"error":{"code":"cyber_policy"}}}"#
    ));
    assert_eq!(
        prelude_event(
            br#"{"type":"response.created","response":{"error":null}}"#,
            false
        ),
        Prelude::Waiting
    );
}

#[test]
fn nonstream_failed_responses_with_partial_output_are_preserved() {
    for fields in [
        json!({"output":[{"type":"reasoning","encrypted_content":"original"}]}),
        json!({"usage":{"output_tokens":1}}),
    ] {
        let mut value = fields;
        value["error"] = json!({"code":"server_error"});
        value["status"] = json!("failed");
        let bytes = value.to_string();
        assert!(!transient_http(503, bytes.as_bytes()));
        assert_eq!(prelude_event(bytes.as_bytes(), false), Prelude::Committed);
    }
}
