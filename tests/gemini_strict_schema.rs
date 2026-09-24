use hey_proxy::gemini::*;
use serde_json::{Value, json};

fn config() -> ProviderConfig {
    serde_json::from_value(json!({"api_key":"synthetic","thinking":"level"})).unwrap()
}
fn codec() -> ReasoningCodec {
    ReasoningCodec::new(&[33; 32])
}
fn request(schema: Value) -> Value {
    json!({"model":"gemini/gemini-3.1-pro-preview","input":"Return JSON","reasoning":{"effort":"low"},
        "text":{"format":{"type":"json_schema","name":"answer","strict":true,"schema":schema}}})
}
fn schema() -> Value {
    json!({"type":"object","properties":{"answer":{"type":"integer","minimum":0,"maximum":10},"tag":{"enum":["✓"]}},"required":["answer","tag"],"additionalProperties":false})
}
fn native(text: &str, reason: &str) -> Value {
    json!({"candidates":[{"content":{"role":"model","parts":[{"text":"check constraints","thought":true,"thoughtSignature":"reasoning-signature"},{"text":text,"thoughtSignature":"answer-signature"}]},"finishReason":reason}]})
}

#[test]
fn strict_response_passes_schema_unchanged_and_validates_all_constraints() {
    let req = request(schema());
    let converted = convert_request(&req, &config(), &codec()).unwrap();
    assert_eq!(
        converted.body["generationConfig"]["responseJsonSchema"],
        schema()
    );
    for (text, valid) in [
        (r#"{"answer":3,"tag":"✓"}"#, true),
        (r#"{"answer":"3","tag":"✓"}"#, false),
        (r#"{"answer":11,"tag":"✓"}"#, false),
        (r#"{"answer":3,"tag":"wrong"}"#, false),
        (r#"{"answer":3,"tag":"✓","extra":true}"#, false),
        (r#"{"answer":3}"#, false),
        ("not json", false),
    ] {
        let original = native(text, "STOP");
        let response = convert_response(&original, &converted, &codec(), "strict").unwrap();
        assert_eq!(
            response["status"],
            if valid { "completed" } else { "failed" }
        );
        if !valid {
            assert_eq!(response["error"]["code"], "gemini_schema_mismatch");
        }
        assert_eq!(response["gemini"], original);
        let mut replay = request(schema());
        replay["input"] = response["output"].clone();
        let back = convert_request(&replay, &config(), &codec()).unwrap();
        assert_eq!(
            back.body["contents"][0]["parts"],
            original["candidates"][0]["content"]["parts"]
        );
    }
}

#[test]
fn strict_schema_refs_unions_and_no_external_retrieval() {
    let local = json!({"$defs":{"count":{"anyOf":[{"type":"integer","minimum":2},{"type":"null"}]}},"$ref":"#/$defs/count"});
    let converted = convert_request(&request(local), &config(), &codec()).unwrap();
    for (text, valid) in [("null", true), ("3", true), ("1", false), ("true", false)] {
        let response =
            convert_response(&native(text, "STOP"), &converted, &codec(), "ref").unwrap();
        assert_eq!(
            response["status"],
            if valid { "completed" } else { "failed" }
        );
    }
    for invalid in [
        json!({"type":"not-a-type"}),
        json!({"$ref":"https://example.invalid/schema.json"}),
        json!({"$ref":"file:///should-not-be-read.json"}),
    ] {
        assert!(convert_request(&request(invalid), &config(), &codec()).is_err());
    }
    let mut bad = request(schema());
    bad["text"]["format"]["strict"] = json!("yes");
    assert!(convert_request(&bad, &config(), &codec()).is_err());
}

#[test]
fn strict_stream_preserves_live_reasoning_and_defers_json_until_validated() {
    let text = r#"{"answer":3,"tag":"✓"}"#;
    for split in text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
    {
        let converted = convert_request(&request(schema()), &config(), &codec()).unwrap();
        let mut stream = ResponseStream::new(converted, "strict-stream");
        let mut events=stream.feed(&json!({"candidates":[{"content":{"parts":[{"text":"thinking","thought":true,"thoughtSignature":"thought"}]}}]})).unwrap();
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "response.reasoning_summary_text.delta")
        );
        for chunk in [&text[..split], &text[split..]] {
            let early = stream
                .feed(&json!({"candidates":[{"content":{"parts":[{"text":chunk}]}}]}))
                .unwrap();
            assert!(early.is_empty(), "JSON output escaped validation");
        }
        assert!(stream.feed(&json!({"candidates":[{"content":{"parts":[{"thoughtSignature":"answer"}]},"finishReason":"STOP"}]})).unwrap().is_empty());
        events.extend(stream.finish(&codec()).unwrap());
        assert_eq!(events.last().unwrap()["type"], "response.completed");
        for (seq, e) in events.iter().enumerate() {
            assert_eq!(e["sequence_number"], seq);
        }
        let recovered: String = events
            .iter()
            .filter(|e| e["type"] == "response.output_text.delta")
            .map(|e| e["delta"].as_str().unwrap())
            .collect();
        assert_eq!(recovered, text);
        let complete = &events.last().unwrap()["response"];
        let mut replay = request(schema());
        replay["input"] = complete["output"].clone();
        let back = convert_request(&replay, &config(), &codec()).unwrap();
        assert_eq!(
            back.body["contents"][0]["parts"],
            complete["gemini"]["candidates"][0]["content"]["parts"]
        );
    }
}

#[test]
fn strict_stream_never_publishes_invalid_output_as_a_delta_or_completion() {
    let converted = convert_request(&request(schema()), &config(), &codec()).unwrap();
    let mut stream = ResponseStream::new(converted, "invalid");
    let mut events = stream
        .feed(&native(r#"{"answer":200,"tag":"✓"}"#, "STOP"))
        .unwrap();
    events.extend(stream.finish(&codec()).unwrap());
    assert_eq!(events.last().unwrap()["type"], "response.failed");
    assert_eq!(
        events.last().unwrap()["response"]["error"]["code"],
        "gemini_schema_mismatch"
    );
    assert!(
        !events
            .iter()
            .any(|e| e["type"] == "response.output_text.delta"
                || e["type"] == "response.output_text.done"
                || e["type"] == "response.completed")
    );
    assert!(
        !events
            .iter()
            .any(|e| e["type"] == "response.output_item.done" && e["output_index"] != 0)
    );
    for (seq, e) in events.iter().enumerate() {
        assert_eq!(e["sequence_number"], seq);
    }
    let failed = &events.last().unwrap()["response"];
    assert!(failed["output"][0]["encrypted_content"].is_string());
    assert_eq!(failed["output"][1]["status"], "incomplete");
}

#[test]
fn strict_schema_retains_incomplete_and_tool_turn_semantics() {
    let mut req = request(schema());
    req["tools"] = json!([{"type":"function","name":"get_count"}]);
    req["tool_choice"] = json!({"type":"function","name":"get_count"});
    let converted = convert_request(&req, &config(), &codec()).unwrap();
    assert!(
        converted.body["generationConfig"]
            .get("responseMimeType")
            .is_none()
    );
    assert!(
        converted.body["generationConfig"]
            .get("responseJsonSchema")
            .is_none()
    );
    assert_eq!(
        converted.body["toolConfig"]["functionCallingConfig"]["mode"],
        "ANY"
    );
    assert_eq!(
        converted.response_fields["text"]["format"]["schema"],
        schema()
    );
    let partial = convert_response(
        &native(r#"{"answer":"#, "MAX_TOKENS"),
        &converted,
        &codec(),
        "partial",
    )
    .unwrap();
    assert_eq!(partial["status"], "incomplete");
    assert!(partial["error"].is_null());
    let name = converted.tools.keys().next().unwrap();
    let call = json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":name,"args":{}},"thoughtSignature":"call"}]},"finishReason":"STOP"}]});
    let response = convert_response(&call, &converted, &codec(), "tool").unwrap();
    assert_eq!(response["status"], "completed");
    let mut stream = ResponseStream::new(converted, "tool-stream");
    let mut events = stream.feed(&call).unwrap();
    events.extend(stream.finish(&codec()).unwrap());
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "response.function_call_arguments.done")
    );
    let completed: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "response.output_item.done")
        .collect();
    assert_eq!(completed[0]["item"]["type"], "reasoning");
    assert_eq!(completed[1]["item"]["type"], "function_call");
}

#[test]
fn strict_schema_enforces_formats_patterns_arrays_and_nontext_output() {
    let shape = json!({"type":"object","properties":{
        "email":{"type":"string","format":"email"},
        "code":{"type":"string","pattern":"^[A-Z]{2}$"},
        "values":{"type":"array","minItems":1,"maxItems":2,"items":{"type":"integer"}}
    },"required":["email","code","values"],"additionalProperties":false});
    let converted = convert_request(&request(shape), &config(), &codec()).unwrap();
    for (email, code, values, valid) in [
        ("a@example.com", "OK", json!([1]), true),
        ("not-email", "OK", json!([1]), false),
        ("a@example.com", "wrong", json!([1]), false),
        ("a@example.com", "OK", json!([]), false),
        ("a@example.com", "OK", json!([1, 2, 3]), false),
        ("a@example.com", "OK", json!(["1"]), false),
    ] {
        let text = json!({"email":email,"code":code,"values":values}).to_string();
        let response =
            convert_response(&native(&text, "STOP"), &converted, &codec(), "formats").unwrap();
        assert_eq!(
            response["status"],
            if valid { "completed" } else { "failed" }
        );
    }
    assert!(
        convert_request(
            &request(json!({"type":"string","format":"unrecognized-format"})),
            &config(),
            &codec()
        )
        .is_err()
    );
    let converted = convert_request(&request(schema()), &config(), &codec()).unwrap();
    let mut mixed = native(r#"{"answer":3,"tag":"✓"}"#, "STOP");
    mixed["candidates"][0]["content"]["parts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"inlineData":{"mimeType":"image/png","data":"YWJj"}}));
    let response = convert_response(&mixed, &converted, &codec(), "media").unwrap();
    assert_eq!(response["status"], "failed");
    assert_eq!(response["gemini"], mixed);
}
