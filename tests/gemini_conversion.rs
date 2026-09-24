use hey_proxy::gemini::*;
use serde_json::{Value, json};

fn config() -> ProviderConfig {
    serde_json::from_value(json!({"api_key":"synthetic", "thinking":"budget"})).unwrap()
}
fn codec() -> ReasoningCodec {
    ReasoningCodec::new(&[42; 32])
}
fn request() -> Value {
    json!({"model":"gemini/models/gemini-3.1-pro-preview","input":"Sort an array","store":false,
    "tools":[{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"cmd":{"type":"string"}},"required":["cmd"]}}],
    "reasoning":{"effort":"high","summary":"auto"},"include":["reasoning.encrypted_content"]})
}
fn converted() -> ConvertedRequest {
    convert_request(&request(), &config(), &codec()).unwrap()
}
fn native() -> Value {
    let r = converted();
    let name = r.tools.keys().next().unwrap();
    json!({"responseId":"native-id","modelVersion":"test-version","candidates":[{"index":0,"content":{"role":"model","parts":[
        {"text":"I should inspect the workspace.","thought":true,"thoughtSignature":"thought-signature"},
        {"text":"Checking now.","thoughtSignature":"text-signature"},
        {"functionCall":{"name":name,"args":{"cmd":"ls"},"id":"native-call-1"},"thoughtSignature":"call-signature"},
        {"functionCall":{"name":name,"args":{"cmd":"pwd"},"id":"native-call-2"},"thoughtSignature":"parallel-signature"}
    ]},"finishReason":"STOP","safetyRatings":[{"category":"synthetic","probability":"NEGLIGIBLE"}],"groundingMetadata":{"test":"retained"}}],
    "usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20,"thoughtsTokenCount":30,"cachedContentTokenCount":40,"totalTokenCount":150}})
}

#[test]
fn vertex_model_and_developer_api_urls() {
    let c:ProviderConfig=serde_json::from_value(json!({"auth":"bearer","api_key":"synthetic","upstream_url":"https://aiplatform.googleapis.com/v1/projects/example-project/locations/global/publishers/google"})).unwrap();
    c.validate().unwrap();
    assert_eq!(
        c.endpoint("models/gemini-3.1-pro-preview", false).unwrap(),
        "https://aiplatform.googleapis.com/v1/projects/example-project/locations/global/publishers/google/models/gemini-3.1-pro-preview:generateContent"
    );
    assert!(
        c.endpoint("gemini-3.1-pro-preview", true)
            .unwrap()
            .ends_with(":streamGenerateContent?alt=sse")
    );
    assert!(
        config()
            .endpoint("gemini-2.5-pro", false)
            .unwrap()
            .starts_with("https://generativelanguage.googleapis.com/v1beta/models/")
    );
    for bad in [
        "",
        "../model",
        "foo?key=x",
        "models/a/b",
        "a:generateContent",
        "a%2fb",
    ] {
        assert!(c.endpoint(bad, false).is_err(), "{bad}");
    }
}
#[test]
fn auth_validation() {
    for value in [
        json!({"auth":"gcloud_adc"}),
        json!({"auth":"bearer","api_key":"synthetic"}),
        json!({"api_key":"synthetic"}),
    ] {
        serde_json::from_value::<ProviderConfig>(value)
            .unwrap()
            .validate()
            .unwrap();
    }
    for value in [
        json!({}),
        json!({"auth":"gcloud_adc","api_key":"synthetic"}),
        json!({"api_key":"\n"}),
        json!({"api_key":"synthetic","upstream_url":"https://user:password@example.com"}),
    ] {
        assert!(
            serde_json::from_value::<ProviderConfig>(value)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}
#[test]
fn instructions_reasoning_and_json_schema() {
    let mut r = request();
    r["instructions"] = json!("Follow the task");
    r["input"] = json!([
        {"role":"system","content":"Be precise"},{"role":"developer","content":[{"type":"input_text","text":"Use Python"}]},{"role":"user","content":"Sort"}]);
    r["temperature"] = json!(0.2);
    r["top_p"] = json!(0.8);
    r["max_output_tokens"] = json!(2000);
    r["text"] = json!({"format":{"type":"json_schema","name":"answer","schema":{"type":"object","properties":{"code":{"type":"string"}}}}});
    let r = convert_request(&r, &config(), &codec()).unwrap();
    assert_eq!(r.model, "gemini-3.1-pro-preview");
    assert_eq!(
        r.body["systemInstruction"]["parts"]
            .as_array()
            .unwrap()
            .len(),
        9
    );
    assert_eq!(
        r.body["generationConfig"]["thinkingConfig"],
        json!({"thinkingBudget":24576,"includeThoughts":true})
    );
    assert_eq!(
        r.body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(r.body["generationConfig"]["maxOutputTokens"], 2000);
}
#[test]
fn model_and_config_specific_thinking_modes() {
    for (effort, budget) in [
        ("none", 0),
        ("minimal", 128),
        ("low", 1024),
        ("medium", 8192),
        ("high", 24576),
        ("xhigh", -1),
        ("max", -1),
    ] {
        let mut r = request();
        r["reasoning"] = json!({"effort":effort,"summary":"none"});
        let r = convert_request(&r, &config(), &codec()).unwrap();
        assert_eq!(
            r.body
                .pointer("/generationConfig/thinkingConfig/thinkingBudget"),
            Some(&json!(budget))
        );
        assert_eq!(
            r.body
                .pointer("/generationConfig/thinkingConfig/includeThoughts"),
            Some(&json!(false))
        );
    }
    for effort in ["minimal", "low", "medium", "high", "xhigh", "max"] {
        let mut r = request();
        r["model"] = json!("gemini/gemini-3-pro-preview");
        r["reasoning"]["effort"] = json!(effort);
        let mut auto = config();
        auto.thinking = Thinking::Auto;
        let r = convert_request(&r, &auto, &codec()).unwrap();
        assert!(
            r.body
                .pointer("/generationConfig/thinkingConfig/thinkingLevel")
                .is_some()
        );
    }
    let mut c = config();
    c.thinking = Thinking::Level;
    assert!(
        convert_request(&request(), &c, &codec())
            .unwrap()
            .body
            .pointer("/generationConfig/thinkingConfig/thinkingLevel")
            .is_some()
    );
}
#[test]
fn exact_signed_native_turn_round_trip_and_native_call_ids() {
    let native = native();
    let response = convert_response(&native, &converted(), &codec(), "test").unwrap();
    let mut r = request();
    let mut input = vec![json!({"role":"user","content":"Inspect"})];
    input.extend(response["output"].as_array().unwrap().clone());
    input.extend([
        json!({"type":"function_call_output","call_id":"native-call-1","output":"file.py"}),
        json!({"type":"function_call_output","call_id":"native-call-2","output":"/workspace"}),
    ]);
    r["input"] = json!(input);
    let next = convert_request(&r, &config(), &codec()).unwrap();
    assert_eq!(next.body["contents"][1], native["candidates"][0]["content"]);
    assert_eq!(
        next.body["contents"][2]["parts"][0]["functionResponse"]["id"],
        "native-call-1"
    );
    assert_eq!(
        next.body["contents"][2]["parts"][1]["functionResponse"]["id"],
        "native-call-2"
    );
    assert_eq!(
        next.body["contents"][2]["parts"][0]["functionResponse"]["response"]["result"],
        "file.py"
    );
}
#[test]
fn generated_client_call_id_is_not_invented_in_native_replay() {
    let mut n = native();
    n["candidates"][0]["content"]["parts"][2]["functionCall"]
        .as_object_mut()
        .unwrap()
        .remove("id");
    let response = convert_response(&n, &converted(), &codec(), "test").unwrap();
    let mut r = request();
    let mut input = response["output"].as_array().unwrap().clone();
    input.push(json!({"type":"function_call_output","call_id":response["output"][2]["call_id"],"output":"ok"}));
    r["input"] = json!(input);
    let r = convert_request(&r, &config(), &codec()).unwrap();
    assert!(
        r.body["contents"][1]["parts"][0]["functionResponse"]
            .get("id")
            .is_none()
    );
}
#[test]
fn reasoning_cannot_be_tampered_lost_or_crossed_between_models() {
    let response = convert_response(&native(), &converted(), &codec(), "test").unwrap();
    let mut r = request();
    r["input"] = response["output"].clone();
    assert!(convert_request(&r, &config(), &ReasoningCodec::new(&[7; 32])).is_err());
    let mut different = r.clone();
    different["model"] = json!("gemini/gemini-2.5-pro");
    assert!(convert_request(&different, &config(), &codec()).is_err());
    let mut edited = r.clone();
    edited["input"][1]["content"][0]["text"] = json!("changed");
    assert!(convert_request(&edited, &config(), &codec()).is_err());
    let mut missing = r.clone();
    missing["input"].as_array_mut().unwrap().pop();
    assert!(convert_request(&missing, &config(), &codec()).is_err());
    let mut foreign = r.clone();
    foreign["input"][0]["encrypted_content"] = json!("OpenAI-encrypted-state");
    assert!(convert_request(&foreign, &config(), &codec()).is_err());
    let mut tampered = r.clone();
    let mut carrier = tampered["input"][0]["encrypted_content"]
        .as_str()
        .unwrap()
        .to_owned();
    carrier.replace_range(35..36, "!");
    tampered["input"][0]["encrypted_content"] = json!(carrier);
    assert!(convert_request(&tampered, &config(), &codec()).is_err());
    let opaque = response["output"][0]["encrypted_content"].as_str().unwrap();
    assert!(!opaque.contains("signature"));
    assert!(!opaque.contains("Checking"));
    // A fresh codec with the same persisted key can replay this after restart.
    assert!(convert_request(&r, &config(), &ReasoningCodec::new(&[42; 32])).is_ok());
}
#[test]
fn tool_names_are_stable_when_declarations_are_reordered() {
    let first = converted();
    let original = first.tools.keys().next().unwrap();
    let mut r = request();
    r["tools"].as_array_mut().unwrap().insert(
        0,
        json!({"type":"function","name":"another","parameters":{"type":"object"}}),
    );
    let second = convert_request(&r, &config(), &codec()).unwrap();
    assert_eq!(first.tools[original].name, second.tools[original].name);
    let response = convert_response(&native(), &first, &codec(), "test").unwrap();
    r["input"] = response["output"].clone();
    assert!(convert_request(&r, &config(), &codec()).is_ok());
}
#[test]
fn text_media_files_audio_and_native_video_parts() {
    let mut r = request();
    r["input"] = json!([{"role":"user","content":[
        {"type":"input_text","text":"inspect"},{"type":"input_image","image_url":"data:image/png;base64,YWJj"},
        {"type":"input_image","image_url":"https://example.com/image.png","mime_type":"image/png"},
        {"type":"input_file","file_data":"data:application/pdf;base64,YWJj"},
        {"type":"input_file","file_url":"gs://bucket/file.pdf"},
        {"type":"input_audio","input_audio":{"format":"wav","data":"YWJj"}},
        {"type":"gemini_part","part":{"fileData":{"mimeType":"video/mp4","fileUri":"gs://bucket/video.mp4"},"videoMetadata":{"fps":2}}}
    ]}]);
    let r = convert_request(&r, &config(), &codec()).unwrap();
    let p = &r.body["contents"][0]["parts"];
    assert_eq!(p.as_array().unwrap().len(), 7);
    assert_eq!(p[1]["inlineData"]["mimeType"], "image/png");
    assert_eq!(p[5]["inlineData"]["mimeType"], "audio/wav");
    assert_eq!(p[6]["videoMetadata"]["fps"], 2);
}
#[test]
fn custom_namespace_tools_and_choice() {
    let mut r = request();
    r["tools"] = json!([{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"patch","format":{"type":"text"}}]}]);
    r["tool_choice"] = json!({"type":"custom","name":"functions.patch"});
    let r = convert_request(&r, &config(), &codec()).unwrap();
    let name = r.tools.keys().next().unwrap();
    assert_eq!(
        r.body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"],
        json!([name])
    );
    let n = json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":name,"args":{"input":"*** patch"}}}]},"finishReason":"STOP"}]});
    let response = convert_response(&n, &r, &codec(), "custom").unwrap();
    assert_eq!(response["output"][1]["type"], "custom_tool_call");
    assert_eq!(response["output"][1]["input"], "*** patch");
    assert_eq!(response["output"][1]["name"], "patch");
    assert_eq!(response["output"][1]["namespace"], "functions");
}
#[test]
fn native_api_extensions_remain_available_without_conflicts() {
    let mut r = request();
    r["gemini"] = json!({"native_request":{"safetySettings":[{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"BLOCK_MEDIUM_AND_ABOVE"}],"cachedContent":"projects/p/locations/global/cachedContents/c","generationConfig":{"responseModalities":["TEXT","IMAGE"],"seed":42}}});
    let c = convert_request(&r, &config(), &codec()).unwrap();
    assert_eq!(c.body["generationConfig"]["seed"], 42);
    assert_eq!(
        c.body["cachedContent"],
        "projects/p/locations/global/cachedContents/c"
    );
    r["gemini"]["native_request"]["generationConfig"]["thinkingConfig"] = json!({});
    assert!(convert_request(&r, &config(), &codec()).is_err());
}
#[test]
fn usage_reasoning_cache_and_full_metadata_are_retained() {
    let n = native();
    let response = convert_response(&n, &converted(), &codec(), "usage").unwrap();
    assert_eq!(response["gemini"], n);
    assert_eq!(response["usage"]["input_tokens"], 100);
    assert_eq!(response["usage"]["output_tokens"], 50);
    assert_eq!(
        response["usage"]["output_tokens_details"]["reasoning_tokens"],
        30
    );
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        40
    );
    assert_eq!(response["usage"]["total_tokens"], 150);
    assert!(usage(&json!({})).is_none());
}
#[test]
fn finish_reasons_are_never_reported_as_false_success() {
    for (reason, status) in [
        ("STOP", "completed"),
        ("MAX_TOKENS", "incomplete"),
        ("SAFETY", "failed"),
        ("MALFORMED_FUNCTION_CALL", "failed"),
        ("RECITATION", "failed"),
        ("OTHER", "failed"),
    ] {
        let mut n = native();
        n["candidates"][0]["finishReason"] = json!(reason);
        assert_eq!(
            convert_response(&n, &converted(), &codec(), reason).unwrap()["status"],
            status
        );
    }
    let response = convert_response(
        &json!({"promptFeedback":{"blockReason":"SAFETY"}}),
        &converted(),
        &codec(),
        "blocked",
    )
    .unwrap();
    assert_eq!(response["status"], "failed");
    let mut n = native();
    n["candidates"][0]
        .as_object_mut()
        .unwrap()
        .remove("finishReason");
    assert_eq!(
        convert_response(&n, &converted(), &codec(), "truncated").unwrap()["status"],
        "failed"
    );
    let mut multiple = native();
    multiple["candidates"]
        .as_array_mut()
        .unwrap()
        .push(json!({"index":1}));
    assert!(convert_response(&multiple, &converted(), &codec(), "multi").is_err());
}
#[test]
fn native_nontext_outputs_are_retained_and_replayable() {
    let mut n = native();
    n["candidates"][0]["content"]["parts"] = json!([
        {"inlineData":{"mimeType":"image/png","data":"YWJj"},"thoughtSignature":"image-signature"},
        {"executableCode":{"language":"PYTHON","code":"print(1)"}},
        {"codeExecutionResult":{"outcome":"OUTCOME_OK","output":"1"}}
    ]);
    let response = convert_response(&n, &converted(), &codec(), "media").unwrap();
    assert_eq!(response["output"][1]["content"][0]["type"], "gemini_part");
    let mut r = request();
    r["input"] = response["output"].clone();
    assert_eq!(
        convert_request(&r, &config(), &codec()).unwrap().body["contents"][0],
        n["candidates"][0]["content"]
    );
}
#[test]
fn unsupported_semantics_fail_explicitly() {
    let unsupported = vec![
        ("previous_response_id", json!("resp_previous")),
        ("conversation", json!("conv")),
        ("background", json!(true)),
        ("store", json!(true)),
        ("tools", json!([{"type":"web_search"}])),
        (
            "tools",
            json!([{"type":"function","name":"strict","strict":true}]),
        ),
        (
            "tools",
            json!([{"type":"custom","name":"patch","format":{"type":"grammar","syntax":"lark","definition":"..."}}]),
        ),
        ("parallel_tool_calls", json!(false)),
        ("service_tier", json!("priority")),
        ("truncation", json!("auto")),
        ("include", json!(["message.output_text.logprobs"])),
        ("reasoning", json!({"effort":"unknown"})),
        ("reasoning", json!({"mode":"pro"})),
        ("input", json!([{"type":"item_reference","id":"item"}])),
        (
            "input",
            json!([{"type":"function_call_output","call_id":"missing","output":"ok"}]),
        ),
        (
            "input",
            json!([{"role":"user","content":[{"type":"input_file","file_id":"file_123"}]}]),
        ),
        (
            "input",
            json!([{"role":"user","content":[{"type":"input_image","image_url":"https://example.com/image.png","detail":"invalid"}]}]),
        ),
    ];
    for (key, value) in unsupported {
        let mut r = request();
        r[key] = value;
        let result = convert_request(&r, &config(), &codec());
        assert!(result.is_err(), "{key}: {result:?}");
    }
}

#[test]
fn streaming_emits_early_deltas_and_exact_signed_replay_and_late_usage() {
    let r = converted();
    let name = r.tools.keys().next().unwrap().clone();
    let mut stream = ResponseStream::new(r, "stream");
    let mut events = Vec::new();
    for part in [
        json!({"text":"Think ","thought":true}),
        json!({"text":"carefully","thought":true,"thoughtSignature":"reasoning"}),
        json!({"text":"Hello 🌍"}),
        json!({"text":"!","thoughtSignature":"text"}),
        json!({"functionCall":{"name":name,"args":{"cmd":"ls"}},"thoughtSignature":"tool"}),
    ] {
        let e = stream
            .feed(&json!({"candidates":[{"index":0,"content":{"role":"model","parts":[part]}}]}))
            .unwrap();
        events.extend(e);
    }
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "response.output_text.delta" && e["delta"] == "Hello 🌍")
    );
    assert!(!events.iter().any(|e| e["type"] == "response.completed"));
    stream
        .feed(&json!({"candidates":[{"index":0,"finishReason":"STOP"}]}))
        .unwrap();
    stream.feed(&json!({"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"thoughtsTokenCount":7,"totalTokenCount":22}})).unwrap();
    events.extend(stream.finish(&codec()).unwrap());
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], index as u64);
    }
    let done: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "response.output_item.done")
        .map(|event| event["item"].clone())
        .collect();
    assert_eq!(done[0]["type"], "reasoning");
    let mut done_request = request();
    done_request["input"] = json!(done);
    assert!(convert_request(&done_request, &config(), &codec()).is_ok());
    let final_response = &events.last().unwrap()["response"];
    assert_eq!(final_response["usage"]["output_tokens"], 12);
    assert_eq!(final_response["status"], "completed");
    let mut req = request();
    req["input"] = final_response["output"].clone();
    let next = convert_request(&req, &config(), &codec()).unwrap();
    assert_eq!(
        next.body["contents"][0]["parts"],
        final_response["gemini"]["candidates"][0]["content"]["parts"]
    );
    assert_eq!(
        next.body["contents"][0]["parts"][0]["text"],
        "Think carefully"
    );
    assert_eq!(next.body["contents"][0]["parts"][1]["text"], "Hello 🌍!");
}
#[test]
fn streaming_missing_completion_and_error_events() {
    let mut s = ResponseStream::new(converted(), "missing");
    s.feed(&json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]}))
        .unwrap();
    assert_eq!(
        s.finish(&codec()).unwrap().last().unwrap()["type"],
        "response.failed"
    );
    assert!(s.finish(&codec()).is_err());
    let mut s = ResponseStream::new(converted(), "error");
    let errors = s.feed(&json!({"error":{"code":503}})).unwrap();
    assert_eq!(errors[0]["type"], "response.failed");
    assert_eq!(errors[0]["response"]["error"]["code"], "server_error");
    assert!(s.is_finished());
}

#[test]
fn codex_text_normalization_and_empty_signature_parts_preserve_exact_native_state() {
    let mut n = native();
    n["candidates"][0]["content"]["parts"]
        .as_array_mut()
        .unwrap()
        .insert(2, json!({"text":"","thoughtSignature":"empty-signature"}));
    let response = convert_response(&n, &converted(), &codec(), "codex").unwrap();
    assert_eq!(response["output"].as_array().unwrap().len(), 4);
    let mut output = response["output"].clone();
    let content = &mut output[1]["content"][0];
    content["type"] = json!("input_text");
    content.as_object_mut().unwrap().remove("annotations");
    let mut request = request();
    request["input"] = output;
    assert_eq!(
        convert_request(&request, &config(), &codec()).unwrap().body["contents"][0],
        n["candidates"][0]["content"]
    );
}

#[test]
fn gemini_three_auto_uses_levels_and_native_tools_can_be_combined() {
    let mut auto = config();
    auto.thinking = Thinking::Auto;
    let mut request = request();
    request["gemini"] = json!({"native_request":{"tools":[{"googleSearch":{}}]}});
    let result = convert_request(&request, &auto, &codec()).unwrap();
    assert_eq!(
        result.body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "high"
    );
    assert!(
        result.body["generationConfig"]["thinkingConfig"]
            .get("thinkingBudget")
            .is_none()
    );
    assert_eq!(result.body["tools"].as_array().unwrap().len(), 2);
    assert!(result.tools.keys().all(|name| name.len() <= 64));
}

#[test]
fn streaming_partial_function_arguments_preserve_nested_values_and_signatures() {
    let r = converted();
    let name = r.tools.keys().next().unwrap().clone();
    let mut s = ResponseStream::new(r, "partial");
    let chunks = [
        json!({"functionCall":{"name":name,"id":"native-partial","willContinue":true,"partialArgs":[{"jsonPath":"$.cmd","stringValue":"echo ","willContinue":true}]},"thoughtSignature":"partial-signature"}),
        json!({"functionCall":{"id":"native-partial","willContinue":true,"partialArgs":[{"jsonPath":"$.cmd","stringValue":"\"🌍\""},{"jsonPath":"$.items[0]['some-key']","numberValue":3},{"jsonPath":"$.items[1].flag","boolValue":false}]}}),
        json!({"functionCall":{"id":"native-partial","partialArgs":[{"jsonPath":"$.optional","nullValue":null}]}}),
    ];
    for part in &chunks {
        s.feed(&json!({"candidates":[{"content":{"parts":[part]}}]}))
            .unwrap();
    }
    s.feed(&json!({"candidates":[{"finishReason":"STOP"}]}))
        .unwrap();
    let events = s.finish(&codec()).unwrap();
    let response = &events.last().unwrap()["response"];
    let args: Value =
        serde_json::from_str(response["output"][1]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(
        args,
        json!({"cmd":"echo \"🌍\"","items":[{"some-key":3},{"flag":false}],"optional":null})
    );
    assert_eq!(response["gemini"]["streamFunctionCallParts"], json!(chunks));
    let mut request = request();
    request["input"] = response["output"].clone();
    let next = convert_request(&request, &config(), &codec()).unwrap();
    assert_eq!(
        next.body["contents"][0]["parts"][0]["thoughtSignature"],
        "partial-signature"
    );
    assert_eq!(
        next.body["contents"][0]["parts"][0]["functionCall"]["args"],
        args
    );
}

#[test]
fn partial_parallel_calls_keep_start_order_even_when_completion_is_reversed() {
    let r = converted();
    let name = r.tools.keys().next().unwrap().clone();
    let mut s = ResponseStream::new(r, "parallel-partial");
    for part in [
        json!({"functionCall":{"name":name,"id":"first","willContinue":true}}),
        json!({"functionCall":{"name":name,"id":"second","willContinue":true}}),
        json!({"functionCall":{"id":"second","partialArgs":[{"jsonPath":"$.cmd","stringValue":"pwd"}]},"thoughtSignature":"second-sig"}),
        json!({"functionCall":{"id":"first","partialArgs":[{"jsonPath":"$.cmd","stringValue":"ls"}]},"thoughtSignature":"first-sig"}),
    ] {
        s.feed(&json!({"candidates":[{"content":{"parts":[part]}}]}))
            .unwrap();
    }
    s.feed(&json!({"candidates":[{"finishReason":"STOP"}]}))
        .unwrap();
    let events = s.finish(&codec()).unwrap();
    let response = &events.last().unwrap()["response"];
    assert_eq!(response["output"][1]["call_id"], "first");
    assert_eq!(response["output"][2]["call_id"], "second");
}

#[test]
fn incomplete_or_invalid_partial_arguments_are_never_executed() {
    let r = converted();
    let name = r.tools.keys().next().unwrap().clone();
    let mut s = ResponseStream::new(r, "unfinished");
    let events=s.feed(&json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":name,"willContinue":true}}]}}]})).unwrap();
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "response.function_call_arguments.done")
    );
    assert!(s.finish(&codec()).is_err());
    let r = converted();
    let name = r.tools.keys().next().unwrap().clone();
    let mut s = ResponseStream::new(r, "invalid");
    assert!(s.feed(&json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":name,"partialArgs":[{"jsonPath":"$.items[999999999]","numberValue":1}]}}]}}]})).is_err());
}

#[test]
fn incremental_text_retains_native_content_metadata_and_signed_replay() {
    let mut stream = ResponseStream::new(converted(), "text-fragments");
    let mut events = Vec::new();
    for fragment in ["a", "🌍", "b"] {
        events.extend(stream.feed(&json!({"candidates":[{"content":{"role":"model","nativeContentMetadata":{"future":true},"parts":[{"text":fragment}]}}]})).unwrap());
    }
    events.extend(stream.feed(&json!({"candidates":[{"content":{"parts":[{"thoughtSignature":"signed-fragments"}]},"finishReason":"STOP"}]})).unwrap());
    events.extend(stream.finish(&codec()).unwrap());
    assert_eq!(
        events
            .iter()
            .filter(
                |event| event["type"] == "response.output_item.added" && event["output_index"] == 1
            )
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "response.output_text.delta")
            .map(|event| event["delta"].as_str().unwrap())
            .collect::<String>(),
        "a🌍b"
    );
    let response = &events.last().unwrap()["response"];
    assert_eq!(
        response["gemini"]["candidates"][0]["content"]["nativeContentMetadata"],
        json!({"future":true})
    );
    let mut request = request();
    request["input"] = response["output"].clone();
    let replay = convert_request(&request, &config(), &codec()).unwrap();
    assert_eq!(
        replay.body["contents"][0]["parts"],
        json!([{"text":"a🌍b","thoughtSignature":"signed-fragments"}])
    );
}
