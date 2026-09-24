use hey_proxy::gemini::{
    ProviderConfig, ReasoningCodec, ResponseError, ResponseStream, convert_request,
    convert_response,
};
use serde_json::{Value, json};

fn stream(strict: bool) -> (ResponseStream, ReasoningCodec) {
    let provider: ProviderConfig = serde_json::from_value(json!({"api_key":"synthetic"})).unwrap();
    let codec = ReasoningCodec::new(&[17; 32]);
    let mut input = json!({"model":"gemini/test","input":"hi","stream":true});
    if strict {
        input["text"] = json!({"format":{"type":"json_schema","name":"answer","strict":true,"schema":{"type":"object"}}});
    }
    (
        ResponseStream::new(convert_request(&input, &provider, &codec).unwrap(), "error"),
        codec,
    )
}

#[test]
fn native_errors_keep_retry_classification_and_details_in_unary_and_streams() {
    for (status, http, code, retryable) in [
        ("UNAVAILABLE", 503, "server_error", true),
        ("INTERNAL", 500, "server_error", true),
        ("DEADLINE_EXCEEDED", 504, "server_error", true),
        ("RESOURCE_EXHAUSTED", 429, "rate_limit_exceeded", true),
        ("INVALID_ARGUMENT", 400, "invalid_prompt", false),
        ("UNAUTHENTICATED", 401, "invalid_prompt", false),
        ("PERMISSION_DENIED", 403, "invalid_prompt", false),
        ("NOT_FOUND", 404, "invalid_prompt", false),
        ("FAILED_PRECONDITION", 400, "invalid_prompt", false),
    ] {
        let native = json!({"error":{"status":status,"code":http,"message":"synthetic error","details":[{"reason":"retained"}]}});
        let normalized = ResponseError::from_native(&native, Some(http));
        assert_eq!(normalized.retryable, retryable);
        assert_eq!(normalized.error["code"], code);
        assert_eq!(normalized.error["gemini_status"], status);
        for strict in [false, true] {
            let (mut stream, codec) = stream(strict);
            let prior = stream.feed(&json!({"candidates":[{"content":{"parts":[{"text":"thinking","thought":true,"thoughtSignature":"retained-signature"},{"text":"partial"}]}}]})).unwrap();
            let events = stream.feed(&native).unwrap();
            assert_eq!(events.len(), 1);
            let event = &events[0];
            assert_eq!(event["type"], "response.failed");
            assert_eq!(event["response"]["error"], normalized.error);
            assert_eq!(event["response"]["gemini"]["error"], native["error"]);
            assert_eq!(
                event["response"]["gemini"]["candidates"][0]["content"]["parts"][0]["thoughtSignature"],
                "retained-signature"
            );
            assert_eq!(event["response"]["output"], json!([]));
            assert_eq!(
                event["sequence_number"].as_u64().unwrap(),
                prior.last().unwrap()["sequence_number"].as_u64().unwrap() + 1
            );
            assert!(stream.is_finished());
            assert!(stream.finish(&codec).is_err());
            assert!(stream.feed(&json!({})).is_err());
        }
        let provider = serde_json::from_value(json!({"api_key":"synthetic"})).unwrap();
        let codec = ReasoningCodec::new(&[17; 32]);
        let request = convert_request(
            &json!({"model":"gemini/test","input":"hi"}),
            &provider,
            &codec,
        )
        .unwrap();
        let unary = convert_response(&native, &request, &codec, "unary").unwrap();
        assert_eq!(unary["error"], normalized.error);
        assert_eq!(unary["gemini"], native);
    }
}

#[test]
fn google_retry_info_becomes_codex_retry_delay_without_guessing_from_message() {
    let native = json!({"error":{"status":"RESOURCE_EXHAUSTED","message":"Busy","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"0.25s"}]}});
    let result = ResponseError::from_native(&native, None);
    assert_eq!(result.error["message"], "Busy Please try again in 0.25s.");
    // A scary message cannot make transient capacity into permanent quota failure.
    let result = ResponseError::from_native(
        &json!({"error":{"status":"RESOURCE_EXHAUSTED","message":"quota permission invalid"}}),
        None,
    );
    assert!(result.retryable);
    for delay in ["NaNs", "infs", "-1s", "999999999999999s", "garbage"] {
        let mut n = native.clone();
        n["error"]["details"][0]["retryDelay"] = json!(delay);
        assert_eq!(
            ResponseError::from_native(&n, None).error["message"],
            "Busy"
        );
    }
    assert!(ResponseError::from_native(&Value::Null, Some(503)).retryable);
    assert!(!ResponseError::from_native(&Value::Null, Some(403)).retryable);
}

#[test]
fn failed_eof_never_commits_partial_history_before_codex_retries() {
    for strict in [false, true] {
        let (mut stream, codec) = stream(strict);
        stream.feed(&json!({"candidates":[{"content":{"parts":[{"text":"reasoning","thought":true,"thoughtSignature":"keep"},{"text":"unfinished"}]}}]})).unwrap();
        let events = stream.finish(&codec).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "response.failed");
        assert_eq!(events[0]["response"]["error"]["code"], "server_error");
        assert!(events[0]["response"]["output"][0]["encrypted_content"].is_string());
    }
}
