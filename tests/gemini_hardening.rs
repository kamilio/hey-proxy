use hey_proxy::gemini::*;
use serde_json::{Value, json};

fn config() -> ProviderConfig {
    serde_json::from_value(json!({"api_key":"synthetic"})).unwrap()
}
fn codec() -> ReasoningCodec {
    ReasoningCodec::new(&[77; 32])
}
fn request(custom: bool) -> ConvertedRequest {
    let tool = if custom {
        json!({"type":"custom","name":"run"})
    } else {
        json!({"type":"function","name":"run","parameters":{"type":"object"}})
    };
    convert_request(
        &json!({"model":"gemini/test","input":"hi","tools":[tool]}),
        &config(),
        &codec(),
    )
    .unwrap()
}
fn chunk(parts: Value) -> Value {
    json!({"candidates":[{"content":{"role":"model","parts":parts}}]})
}
fn executable_done(events: &[Value]) -> bool {
    events.iter().any(|e| {
        matches!(
            e["type"].as_str(),
            Some("response.function_call_arguments.done" | "response.custom_tool_call_input.done")
        ) || (e["type"] == "response.output_item.done"
            && matches!(
                e["item"]["type"].as_str(),
                Some("function_call" | "custom_tool_call")
            ))
    })
}

#[test]
fn malformed_native_structures_fail_in_unary_and_streaming() {
    let mut cases = vec![
        Value::Null,
        json!([]),
        json!({"candidates":null}),
        json!({"candidates":{}}),
        json!({"candidates":[null]}),
        json!({"candidates":[{},{}]}),
        json!({"usageMetadata":[]}),
        json!({"usageMetadata":{"thoughtsTokenCount":-1}}),
        json!({"promptFeedback":{"blockReason":true}}),
    ];
    for bad in [
        json!(null),
        json!([]),
        json!({}),
        json!({"parts":null}),
        json!({"role":"user","parts":[]}),
    ] {
        cases.push(json!({"candidates":[{"content":bad,"finishReason":"STOP"}]}));
    }
    for bad in [json!(-1), json!(1), json!("0"), json!(null)] {
        cases.push(json!({"candidates":[{"index":bad,"finishReason":"STOP"}]}));
    }
    for part in [
        json!(null),
        json!("text"),
        json!({"text":null}),
        json!({"text":3}),
        json!({"thought":"true"}),
        json!({"thoughtSignature":[]}),
        json!({"functionCall":null}),
        json!({"text":"lost","functionCall":{"name":"run","args":{}}}),
        json!({"functionCall":{"name":"run","args":[]}}),
        json!({"functionCall":{"id":4}}),
        json!({"functionCall":{"willContinue":"true"}}),
    ] {
        cases.push(chunk(json!([part])));
    }
    for native in cases {
        assert!(
            convert_response(&native, &request(false), &codec(), "bad").is_err(),
            "{native}"
        );
        let mut stream = ResponseStream::new(request(false), "bad");
        assert!(stream.feed(&native).is_err(), "{native}");
        assert!(
            stream
                .feed(&json!({"candidates":[{"finishReason":"STOP"}]}))
                .is_err()
        );
        assert!(stream.finish(&codec()).is_err());
    }
}

#[test]
fn malformed_native_input_cannot_panic_when_merging_following_user_turn() {
    for content in [
        json!({"role":"user","parts":null}),
        json!({"role":"user"}),
        json!({"parts":[]}),
        json!({"role":"invalid","parts":[]}),
        json!({"role":"user","parts":[{"text":1}]}),
    ] {
        let input = json!({"model":"gemini/test","input":[{"type":"gemini_content","content":content},{"role":"user","content":"next"}]});
        assert!(convert_request(&input, &config(), &codec()).is_err());
    }
}

#[test]
fn tool_completion_requires_successful_finish_and_transport_eof() {
    for custom in [false, true] {
        for reason in [
            None,
            Some("STOP"),
            Some("MAX_TOKENS"),
            Some("SAFETY"),
            Some("MALFORMED_FUNCTION_CALL"),
        ] {
            let req = request(custom);
            let name = req.tools.keys().next().unwrap().clone();
            let mut native = chunk(
                json!([{"functionCall":{"name":name,"args":if custom {json!({"input":"echo hello"})} else {json!({"cmd":"echo hello"})}},"thoughtSignature":"signature"}]),
            );
            if let Some(reason) = reason {
                native["candidates"][0]["finishReason"] = json!(reason);
            }
            let unary = convert_response(&native, &req, &codec(), "tool").unwrap();
            assert_eq!(
                unary["output"][1]["status"],
                if reason == Some("STOP") {
                    "completed"
                } else {
                    "incomplete"
                }
            );
            let mut stream = ResponseStream::new(req, "tool");
            let early = stream.feed(&native).unwrap();
            assert!(!executable_done(&early));
            // Usage may arrive after the finish marker; retain it before completing.
            stream
                .feed(&json!({"usageMetadata":{"promptTokenCount":5,"thoughtsTokenCount":7}}))
                .unwrap();
            let final_events = stream.finish(&codec()).unwrap();
            assert_eq!(executable_done(&final_events), reason == Some("STOP"));
            assert_eq!(
                final_events.last().unwrap()["response"]["usage"]["output_tokens"],
                7
            );
            if reason == Some("STOP") {
                let carrier = final_events
                    .iter()
                    .position(|e| {
                        e["type"] == "response.output_item.done" && e["output_index"] == 0
                    })
                    .unwrap();
                let tool = final_events
                    .iter()
                    .position(|e| {
                        e["type"] == "response.function_call_arguments.done"
                            || e["type"] == "response.custom_tool_call_input.done"
                    })
                    .unwrap();
                assert!(carrier < tool);
            }
        }
    }
}

#[test]
fn error_after_tool_prefix_is_terminal_and_preserves_diagnostics() {
    let req = request(false);
    let name = req.tools.keys().next().unwrap().clone();
    let mut stream = ResponseStream::new(req, "broken");
    let prefix = chunk(json!([{"functionCall":{"name":name,"args":{"cmd":"unsafe-if-partial"}}}]));
    let early = stream.feed(&prefix).unwrap();
    assert!(!executable_done(&early));
    let failed = stream
        .feed(&json!({"error":{"message":"failure"}}))
        .unwrap()
        .remove(0);
    assert_eq!(failed["type"], "response.failed");
    assert_eq!(failed["response"]["output"], json!([]));
    assert_eq!(failed["response"]["error"]["code"], "server_error");
    assert_eq!(
        failed["response"]["gemini"]["candidates"][0]["content"]["parts"],
        prefix["candidates"][0]["content"]["parts"]
    );
    assert!(
        failed["sequence_number"].as_u64().unwrap()
            > early.last().unwrap()["sequence_number"].as_u64().unwrap()
    );
    assert!(stream.finish(&codec()).is_err());
}

#[test]
fn sparse_partial_arguments_have_a_cumulative_allocation_bound() {
    let req = request(false);
    let name = req.tools.keys().next().unwrap().clone();
    let mut stream = ResponseStream::new(req, "sparse");
    let mut path = String::from("$.x");
    for _ in 0..5 {
        path.push_str("[65535]");
    }
    let result = stream.feed(&chunk(json!([{"functionCall":{"name":name,"partialArgs":[{"jsonPath":path,"stringValue":"small input, huge allocation"}]}}])));
    assert!(result.unwrap_err().to_string().contains("allocated slots"));
    assert!(stream.finish(&codec()).is_err());
}

#[test]
fn ambiguous_partial_calls_are_rejected() {
    for mixed in [true, false] {
        let req = request(false);
        let name = req.tools.keys().next().unwrap().clone();
        let mut stream = ResponseStream::new(req, "ambiguous");
        let mut part = json!({"functionCall":{"name":name,"id":"duplicate","willContinue":true}});
        if mixed {
            part["functionCall"]["args"] = json!({"lost":"value"});
            assert!(stream.feed(&chunk(json!([part]))).is_err());
        } else {
            stream.feed(&chunk(json!([part.clone()]))).unwrap();
            assert!(stream.feed(&chunk(json!([part]))).is_err());
        }
    }
}

#[test]
fn many_small_text_fragments_preserve_full_output_and_signatures() {
    let mut stream = ResponseStream::new(request(false), "large");
    let fragment = "🌍λ".repeat(20);
    let part = chunk(json!([{"text":fragment}]));
    let start = std::time::Instant::now();
    for _ in 0..8192 {
        stream.feed(&part).unwrap();
    }
    stream.feed(&json!({"candidates":[{"content":{"parts":[{"thoughtSignature":"late-signature"}]},"finishReason":"STOP"}]})).unwrap();
    let events = stream.finish(&codec()).unwrap();
    let response = &events.last().unwrap()["response"];
    assert_eq!(
        response["output"][1]["content"][0]["text"],
        fragment.repeat(8192)
    );
    assert_eq!(
        response["gemini"]["candidates"][0]["content"]["parts"][0]["thoughtSignature"],
        "late-signature"
    );
    eprintln!(
        "8192 text chunks / {} bytes incl final encryption: {:?}",
        fragment.len() * 8192,
        start.elapsed()
    );
}

#[test]
fn interrupted_partial_calls_retain_the_exact_native_trace() {
    let req = request(false);
    let name = req.tools.keys().next().unwrap().clone();
    let part = json!({"functionCall":{"name":name,"id":"partial","willContinue":true,
        "partialArgs":[{"jsonPath":"$.command","stringValue":"unfinished"}]},
        "thoughtSignature":"signed-partial"});
    let mut stream = ResponseStream::new(req, "partial");
    stream.feed(&chunk(json!([part]))).unwrap();
    assert!(stream.finish(&codec()).is_err());
    let failed = stream.fail("interrupted", "incomplete arguments");
    assert_eq!(
        failed["response"]["gemini"]["streamFunctionCallParts"],
        json!([part])
    );
    assert_eq!(failed["response"]["output"], json!([]));
}
