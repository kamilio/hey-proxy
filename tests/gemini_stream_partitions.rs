use hey_proxy::gemini::{ProviderConfig, ReasoningCodec, ResponseStream, convert_request};
use serde_json::{Value, json};

fn partitions(text: &str, mask: usize) -> Vec<String> {
    let chars: Vec<_> = text.chars().collect();
    let mut fragments = vec![String::new()];
    for (index, character) in chars.iter().enumerate() {
        fragments.last_mut().unwrap().push(*character);
        if index + 1 < chars.len() && mask & (1 << index) != 0 {
            fragments.push(String::new());
        }
    }
    fragments
}

#[test]
fn every_unicode_text_partition_preserves_signed_turn_and_codex_completion_order() {
    // Exhaust all 256 pairs of fragment boundaries rather than relying on one
    // hand-picked network partition. The strings include UTF-8, JSON escaping,
    // and a newline, followed by an executable call and usage after STOP.
    let thought = "plan✓";
    let answer = "A🌍B\"\n";
    let config: ProviderConfig = serde_json::from_value(json!({"api_key":"synthetic"})).unwrap();
    let codec = ReasoningCodec::new(&[19; 32]);
    let request = json!({"model":"gemini/gemini-3.1-pro-preview","input":"Check the boundary matrix",
        "tools":[{"type":"function","name":"execute","parameters":{"type":"object","properties":{"cmd":{"type":"string"}}}}]});
    let converted = convert_request(&request, &config, &codec).unwrap();
    let native_name = converted.tools.keys().next().unwrap();
    let call = json!({"functionCall":{"name":native_name,"args":{"cmd":"printf '\"🌍\"\\n'"},"id":"boundary-call"},"thoughtSignature":"signed-call"});
    let expected = json!([
        {"text":thought,"thought":true,"thoughtSignature":"signed-thought"},
        {"text":answer,"thoughtSignature":"signed-answer"},
        call
    ]);

    for thought_mask in 0..(1 << (thought.chars().count() - 1)) {
        for answer_mask in 0..(1 << (answer.chars().count() - 1)) {
            let context = format!("thought mask {thought_mask}, answer mask {answer_mask}");
            let mut stream = ResponseStream::new(converted.clone(), "boundary-matrix");
            let mut events = Vec::new();
            for fragment in partitions(thought, thought_mask) {
                events.extend(stream.feed(&json!({"candidates":[{"content":{"parts":[{"text":fragment,"thought":true}]}}]})).unwrap());
            }
            events.extend(stream.feed(&json!({"candidates":[{"content":{"parts":[{"thoughtSignature":"signed-thought"}]}}]})).unwrap());
            for fragment in partitions(answer, answer_mask) {
                events.extend(
                    stream
                        .feed(&json!({"candidates":[{"content":{"parts":[{"text":fragment}]}}]}))
                        .unwrap(),
                );
            }
            events.extend(stream.feed(&json!({"candidates":[{"content":{"parts":[{"thoughtSignature":"signed-answer"},call]}}]})).unwrap());
            assert!(
                !events
                    .iter()
                    .any(|event| event["type"] == "response.output_item.done"),
                "{context}: signed outputs finished before their carrier"
            );
            events.extend(
                stream
                    .feed(&json!({"candidates":[{"finishReason":"STOP"}]}))
                    .unwrap(),
            );
            events.extend(stream.feed(&json!({"usageMetadata":{"promptTokenCount":51,"cachedContentTokenCount":4,"candidatesTokenCount":12,"thoughtsTokenCount":7,"totalTokenCount":70}})).unwrap());
            events.extend(stream.finish(&codec).unwrap());

            for (sequence, event) in events.iter().enumerate() {
                assert_eq!(event["sequence_number"], sequence, "{context}");
            }
            let text: String = events
                .iter()
                .filter(|event| event["type"] == "response.output_text.delta")
                .map(|event| event["delta"].as_str().unwrap())
                .collect();
            assert_eq!(text, answer, "{context}");
            let summary: String = events
                .iter()
                .filter(|event| event["type"] == "response.reasoning_summary_text.delta")
                .map(|event| event["delta"].as_str().unwrap())
                .collect();
            assert_eq!(summary, thought, "{context}");

            let completed: Vec<Value> = events
                .iter()
                .filter(|event| event["type"] == "response.output_item.done")
                .map(|event| event["item"].clone())
                .collect();
            assert_eq!(completed.len(), 3, "{context}");
            assert_eq!(completed[0]["type"], "reasoning", "{context}");
            assert_eq!(
                events.last().unwrap()["type"],
                "response.completed",
                "{context}"
            );
            let response = &events.last().unwrap()["response"];
            assert_eq!(response["usage"]["output_tokens"], 19, "{context}");
            assert_eq!(
                response["usage"]["input_tokens_details"]["cached_tokens"], 4,
                "{context}"
            );
            assert_eq!(
                response["gemini"]["candidates"][0]["content"]["parts"], expected,
                "{context}"
            );

            // Match how Codex builds its next request from completed events.
            let mut replay = request.clone();
            replay["input"] = json!(completed);
            replay["input"].as_array_mut().unwrap().push(json!({"type":"function_call_output","call_id":"boundary-call","output":"verified"}));
            let native = convert_request(&replay, &config, &codec).unwrap().body;
            assert_eq!(native["contents"][0]["parts"], expected, "{context}");
            assert_eq!(
                native["contents"][1]["parts"][0]["functionResponse"]["id"], "boundary-call",
                "{context}"
            );
        }
    }
}

#[test]
fn large_patch_tool_payload_is_byte_exact_through_streaming_and_signed_replay() {
    // Separate model-authored patch syntax failures from transport corruption:
    // neither large payloads nor Unicode/escaping may alter the tool's input.
    let mut patch = String::from("*** Begin Patch\n*** Add File: regression.rs\n");
    for index in 0..2048 {
        patch.push_str(&format!("+// {index}: 🌍 \\\"quoted\\\" \\n\n"));
    }
    patch.push_str("*** End Patch\n");
    let config: ProviderConfig = serde_json::from_value(json!({"api_key":"synthetic"})).unwrap();
    let codec = ReasoningCodec::new(&[29; 32]);
    for custom in [false, true] {
        let tool = if custom {
            json!({"type":"custom","name":"apply_patch","format":{"type":"text"}})
        } else {
            json!({"type":"function","name":"apply_patch","parameters":{"type":"object","properties":{"input":{"type":"string"}}}})
        };
        let request = json!({"model":"gemini/gemini-3.1-pro-preview","input":"Add a regression test","tools":[tool]});
        let converted = convert_request(&request, &config, &codec).unwrap();
        let name = converted.tools.keys().next().unwrap().clone();
        let native_call = json!({"functionCall":{"name":name,"args":{"input":patch},"id":"patch-call"},"thoughtSignature":"patch-signature"});
        let mut stream = ResponseStream::new(converted, "large-patch");
        let mut events = stream
            .feed(
                &json!({"candidates":[{"content":{"parts":[native_call]},"finishReason":"STOP"}]}),
            )
            .unwrap();
        events.extend(stream.finish(&codec).unwrap());
        let done: Vec<_> = events
            .iter()
            .filter(|event| event["type"] == "response.output_item.done")
            .map(|event| event["item"].clone())
            .collect();
        let client_input = if custom {
            done[1]["input"].as_str().unwrap().to_owned()
        } else {
            serde_json::from_str::<Value>(done[1]["arguments"].as_str().unwrap()).unwrap()["input"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(client_input.as_bytes(), patch.as_bytes());
        let mut replay = request;
        replay["input"] = json!(done);
        replay["input"].as_array_mut().unwrap().push(json!({"type":if custom {"custom_tool_call_output"} else {"function_call_output"},"call_id":"patch-call","output":"applied"}));
        let native = convert_request(&replay, &config, &codec).unwrap().body;
        assert_eq!(native["contents"][0]["parts"][0], native_call);
        assert_eq!(
            native["contents"][0]["parts"][0]["functionCall"]["args"]["input"]
                .as_str()
                .unwrap()
                .as_bytes(),
            patch.as_bytes()
        );
    }
}

#[test]
fn detached_signatures_on_nontext_outputs_are_present_in_completed_items_and_replay() {
    let config: ProviderConfig = serde_json::from_value(json!({"api_key":"synthetic"})).unwrap();
    let codec = ReasoningCodec::new(&[39; 32]);
    let request = json!({"model":"gemini/gemini-3.1-pro-preview","input":"Render the result"});
    for part in [
        json!({"inlineData":{"mimeType":"image/png","data":"YWJj"}}),
        json!({"executableCode":{"language":"PYTHON","code":"print(7)"}}),
        json!({"codeExecutionResult":{"outcome":"OUTCOME_OK","output":"7\n"}}),
    ] {
        let converted = convert_request(&request, &config, &codec).unwrap();
        let mut stream = ResponseStream::new(converted, "detached-nontext");
        let mut events = stream
            .feed(&json!({"candidates":[{"content":{"parts":[part]}}]}))
            .unwrap();
        events.extend(stream.feed(&json!({"candidates":[{"content":{"parts":[{"thoughtSignature":"late-nontext-signature"}]},"finishReason":"STOP"}]})).unwrap());
        events.extend(stream.finish(&codec).unwrap());
        let completed: Vec<_> = events
            .iter()
            .filter(|event| event["type"] == "response.output_item.done")
            .map(|event| event["item"].clone())
            .collect();
        let final_output = &events.last().unwrap()["response"]["output"];
        assert_eq!(
            json!(completed),
            *final_output,
            "completed events must agree with the signed final projections"
        );
        assert_eq!(
            completed[1]["content"][0]["part"]["thoughtSignature"],
            "late-nontext-signature"
        );
        let mut replay = request.clone();
        replay["input"] = json!(completed);
        let native = convert_request(&replay, &config, &codec).unwrap().body;
        let mut signed_part = part;
        signed_part["thoughtSignature"] = json!("late-nontext-signature");
        assert_eq!(native["contents"][0]["parts"], json!([signed_part]));
    }
}
