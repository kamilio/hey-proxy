use hey_proxy::gemini::*;
use serde_json::{Value, json};

fn config() -> ProviderConfig {
    serde_json::from_value(json!({
        "api_key": "synthetic-regression-key",
        "thinking": "budget"
    }))
    .unwrap()
}

fn codec() -> ReasoningCodec {
    ReasoningCodec::new(&[0x5a; 32])
}

fn base_request() -> Value {
    json!({
        "model": "gemini/models/gemini-2.5-pro",
        "input": "Run diagnostics",
        "store": false,
        "reasoning": {"effort": "medium", "summary": "concise"},
        "include": ["reasoning.encrypted_content"]
    })
}

#[test]
fn composed_multi_turn_signed_replay_across_reordered_tools_and_modalities() {
    let codec = codec();
    let cfg = config();

    let mut turn1_req = base_request();
    turn1_req["instructions"] = json!("Follow repository rules strictly.");
    turn1_req["tools"] = json!([
        {
            "type": "namespace",
            "name": "fs",
            "tools": [{
                "type": "function",
                "name": "read_file",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "encoding": {"type": "string"}
                    },
                    "required": ["path"]
                }
            }]
        },
        {
            "type": "function",
            "name": "run_shell",
            "parameters": {
                "type": "object",
                "properties": {"cmd": {"type": "string"}},
                "required": ["cmd"]
            }
        }
    ]);
    turn1_req["input"] = json!([
        {"role": "developer", "content": "Prefer read-only inspection first."},
        {"role": "user", "content": "Inspect Cargo.toml and current working directory."}
    ]);

    let converted_turn1 = convert_request(&turn1_req, &cfg, &codec).unwrap();
    let fs_read_native = converted_turn1
        .tools
        .iter()
        .find(|(_, t)| t.namespace.as_deref() == Some("fs") && t.name == "read_file")
        .unwrap()
        .0
        .clone();
    let shell_native = converted_turn1
        .tools
        .iter()
        .find(|(_, t)| t.namespace.is_none() && t.name == "run_shell")
        .unwrap()
        .0
        .clone();

    let native_turn1 = json!({
        "responseId": "resp-turn-1",
        "candidates": [{
            "index": 0,
            "content": {
                "role": "model",
                "parts": [
                    {"text": "Plan: read Cargo.toml and run pwd.", "thought": true, "thoughtSignature": "sig-turn1-thought"},
                    {"text": "", "thoughtSignature": "sig-turn1-empty-carrier"},
                    {"text": "Reading configuration and directory.", "thoughtSignature": "sig-turn1-text"},
                    {"functionCall": {"name": fs_read_native, "args": {"path": "Cargo.toml", "encoding": "utf-8"}, "id": "native-fs-call-1"}, "thoughtSignature": "sig-turn1-fs"},
                    {"functionCall": {"name": shell_native, "args": {"cmd": "pwd"}}, "thoughtSignature": "sig-turn1-shell"}
                ]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 40, "candidatesTokenCount": 15, "thoughtsTokenCount": 10}
    });

    let response_turn1 = convert_response(&native_turn1, &converted_turn1, &codec, "t1").unwrap();
    let mut turn1_outputs = response_turn1["output"].as_array().unwrap().clone();
    assert_eq!(turn1_outputs.len(), 4);

    turn1_outputs[1]["content"][0]["type"] = json!("input_text");
    turn1_outputs[1]["content"][0]
        .as_object_mut()
        .unwrap()
        .remove("annotations");
    turn1_outputs[2]["arguments"] =
        json!("{\n  \"encoding\": \"utf-8\",\n  \"path\": \"Cargo.toml\"\n}");
    let generated_shell_call_id = turn1_outputs[3]["call_id"].as_str().unwrap().to_owned();
    assert!(!generated_shell_call_id.is_empty());

    let mut turn2_req = turn1_req.clone();
    turn2_req["tools"] = json!([
        {
            "type": "namespace",
            "name": "editor",
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply unified patch",
                "format": {"type": "text"}
            }]
        },
        {
            "type": "function",
            "name": "run_shell",
            "parameters": {
                "type": "object",
                "properties": {"cmd": {"type": "string"}},
                "required": ["cmd"]
            }
        },
        {
            "type": "namespace",
            "name": "fs",
            "tools": [{
                "type": "function",
                "name": "read_file",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "encoding": {"type": "string"}
                    },
                    "required": ["path"]
                }
            }]
        }
    ]);

    let mut turn2_input = turn1_req["input"].as_array().unwrap().clone();
    turn2_input.extend(turn1_outputs.clone());
    turn2_input.push(json!({
        "type": "function_call_output",
        "call_id": "native-fs-call-1",
        "output": [
            {"type": "input_text", "text": "[package]\nname = \"hey-proxy\""},
            {"type": "input_image", "image_url": "data:image/png;base64,YWJj"}
        ]
    }));
    turn2_input.push(json!({
        "type": "function_call_output",
        "call_id": generated_shell_call_id,
        "output": "/workspace/hey-proxy"
    }));
    turn2_req["input"] = json!(turn2_input);

    let converted_turn2 = convert_request(&turn2_req, &cfg, &codec).unwrap();
    assert_eq!(
        converted_turn2.body["contents"][1],
        native_turn1["candidates"][0]["content"]
    );
    let user_tool_responses = &converted_turn2.body["contents"][2]["parts"];
    assert_eq!(
        user_tool_responses[0]["functionResponse"]["id"],
        "native-fs-call-1"
    );
    assert_eq!(
        user_tool_responses[0]["functionResponse"]["response"]["result"],
        json!(["[package]\nname = \"hey-proxy\""])
    );
    assert_eq!(
        user_tool_responses[0]["functionResponse"]["parts"],
        json!([{"inlineData": {"mimeType": "image/png", "data": "YWJj"}}])
    );
    assert!(
        user_tool_responses[1]["functionResponse"]
            .get("id")
            .is_none()
    );
    assert_eq!(
        user_tool_responses[1]["functionResponse"]["response"]["result"],
        "/workspace/hey-proxy"
    );

    let patch_native = converted_turn2
        .tools
        .iter()
        .find(|(_, t)| t.namespace.as_deref() == Some("editor") && t.name == "apply_patch")
        .unwrap()
        .0
        .clone();
    let mut stream_turn2 = ResponseStream::new(converted_turn2.clone(), "t2");
    let mut turn2_events = Vec::new();
    for chunk in [
        json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [
            {"text": "Need to patch Cargo.toml.", "thought": true, "thoughtSignature": "sig-turn2-thought"}
        ]}}]}),
        json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [
            {"functionCall": {"name": patch_native, "id": "native-patch-2", "willContinue": true, "partialArgs": [
                {"jsonPath": "$.input", "stringValue": "*** Begin Patch\n", "willContinue": true}
            ]}, "thoughtSignature": "sig-turn2-patch"}
        ]}}]}),
        json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [
            {"functionCall": {"id": "native-patch-2", "partialArgs": [
                {"jsonPath": "$.input", "stringValue": "*** End Patch"}
            ]}},
            {"text": "Applied the patch.", "thoughtSignature": "sig-turn2-text"}
        ]}, "finishReason": "STOP"}]}),
    ] {
        turn2_events.extend(stream_turn2.feed(&chunk).unwrap());
    }
    turn2_events.extend(stream_turn2.finish(&codec).unwrap());

    let turn2_done_items: Vec<Value> = turn2_events
        .iter()
        .filter(|e| e["type"] == "response.output_item.done")
        .map(|e| e["item"].clone())
        .collect();
    assert_eq!(turn2_done_items[0]["type"], "reasoning");
    assert_eq!(turn2_done_items[1]["type"], "custom_tool_call");
    assert_eq!(turn2_done_items[2]["type"], "message");

    let mut turn3_input = turn2_input;
    turn3_input.extend(turn2_done_items.clone());
    turn3_input.push(json!({
        "type": "custom_tool_call_output",
        "call_id": "native-patch-2",
        "output": "Patch applied cleanly"
    }));
    turn3_input.push(json!({"role": "user", "content": "Confirm everything succeeded."}));

    turn3_input.extend(turn2_done_items);
    turn3_input.extend(turn1_outputs);

    let mut turn3_req = turn2_req;
    turn3_req["input"] = json!(turn3_input);
    let converted_turn3 = convert_request(&turn3_req, &cfg, &codec).unwrap();
    let contents = converted_turn3.body["contents"].as_array().unwrap();

    let roles: Vec<&str> = contents
        .iter()
        .map(|c| c["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        vec![
            "user", "model", "user", "model", "user", "user", "model", "model", "user"
        ]
    );
    assert_eq!(
        contents[3]["parts"][1]["functionCall"]["args"],
        json!({"input": "*** Begin Patch\n*** End Patch"})
    );
    assert_eq!(
        contents[3]["parts"][1]["thoughtSignature"],
        "sig-turn2-patch"
    );
    assert_eq!(
        contents[4]["parts"][0]["functionResponse"]["id"],
        "native-patch-2"
    );
    assert_eq!(
        contents[5]["parts"][0]["text"],
        "Confirm everything succeeded."
    );
    assert_eq!(contents[4]["parts"].as_array().unwrap().len(), 1);
    assert_eq!(contents[6]["parts"].as_array().unwrap().len(), 3);
    assert_eq!(contents[7]["parts"].as_array().unwrap().len(), 5);
}

#[test]
fn duplicate_tool_leaf_names_across_namespaces_and_nested_scopes() {
    let codec = codec();
    let cfg = config();

    let mut req = base_request();
    req["tools"] = json!([
        {
            "type": "function",
            "name": "query",
            "description": "Root query",
            "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}
        },
        {
            "type": "namespace",
            "name": "sql",
            "tools": [
                {
                    "type": "function",
                    "name": "query",
                    "description": "SQL query",
                    "parameters": {"type": "object", "properties": {"sql": {"type": "string"}}}
                },
                {
                    "type": "custom",
                    "name": "patch",
                    "description": "SQL migration patch",
                    "format": {"type": "text"}
                }
            ]
        },
        {
            "type": "namespace",
            "name": "vector",
            "tools": [
                {
                    "type": "function",
                    "name": "query",
                    "description": "Vector similarity query",
                    "parameters": {"type": "object", "properties": {"embedding": {"type": "array"}}}
                },
                {
                    "type": "custom",
                    "name": "patch",
                    "description": "Vector index patch",
                    "format": {"type": "text"}
                }
            ]
        },
        {
            "type": "namespace",
            "name": "cloud",
            "tools": [{
                "type": "namespace",
                "name": "sql",
                "tools": [{
                    "type": "function",
                    "name": "query",
                    "parameters": {"type": "object", "properties": {"instance": {"type": "string"}}}
                }]
            }]
        },
        {
            "type": "namespace",
            "name": "a_b",
            "tools": [{"type": "function", "name": "c"}]
        },
        {
            "type": "namespace",
            "name": "a",
            "tools": [{
                "type": "namespace",
                "name": "b",
                "tools": [{"type": "function", "name": "c"}]
            }]
        }
    ]);
    req["tool_choice"] = json!({"type": "function", "namespace": "vector", "name": "query"});

    let converted = convert_request(&req, &cfg, &codec).unwrap();
    assert_eq!(converted.tools.len(), 8);

    let find_native = |ns: Option<&str>, leaf: &str| -> String {
        converted
            .tools
            .iter()
            .find(|(_, t)| t.namespace.as_deref() == ns && t.name == leaf)
            .unwrap_or_else(|| panic!("missing tool {ns:?} {leaf}"))
            .0
            .clone()
    };

    let root_query = find_native(None, "query");
    let sql_query = find_native(Some("sql"), "query");
    let vector_query = find_native(Some("vector"), "query");
    let cloud_sql_query = find_native(Some("cloud.sql"), "query");
    let sql_patch = find_native(Some("sql"), "patch");
    let vector_patch = find_native(Some("vector"), "patch");
    let ab_c = find_native(Some("a_b"), "c");
    let a_b_c = find_native(Some("a.b"), "c");

    assert_ne!(ab_c, a_b_c);
    assert!(ab_c.starts_with("hey_a_b_c_"));
    assert!(a_b_c.starts_with("hey_a_b_c_"));

    assert_eq!(
        converted.body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"],
        json!([vector_query])
    );

    let mut dotted_choice_req = req.clone();
    dotted_choice_req["tool_choice"] = json!({"type": "custom", "name": "sql.patch"});
    let dotted_converted = convert_request(&dotted_choice_req, &cfg, &codec).unwrap();
    assert_eq!(
        dotted_converted.body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"],
        json!([sql_patch])
    );

    let native_response = json!({
        "candidates": [{
            "index": 0,
            "content": {
                "role": "model",
                "parts": [
                    {"functionCall": {"name": root_query, "args": {"q": "status"}, "id": "id-root"}, "thoughtSignature": "sig-1"},
                    {"functionCall": {"name": sql_query, "args": {"sql": "SELECT 1"}, "id": "id-sql"}, "thoughtSignature": "sig-2"},
                    {"functionCall": {"name": vector_query, "args": {"embedding": [0.1, 0.2]}, "id": "id-vec"}, "thoughtSignature": "sig-3"},
                    {"functionCall": {"name": cloud_sql_query, "args": {"instance": "prod"}, "id": "id-cloud"}, "thoughtSignature": "sig-4"},
                    {"functionCall": {"name": sql_patch, "args": {"input": "ALTER TABLE"}, "id": "id-sql-patch"}, "thoughtSignature": "sig-5"},
                    {"functionCall": {"name": vector_patch, "args": {"input": "REINDEX"}, "id": "id-vec-patch"}, "thoughtSignature": "sig-6"}
                ]
            },
            "finishReason": "STOP"
        }]
    });

    let resp = convert_response(&native_response, &converted, &codec, "dup").unwrap();
    let out = resp["output"].as_array().unwrap();
    assert_eq!(out[1]["name"], "query");
    assert!(out[1].get("namespace").is_none());
    assert_eq!(out[2]["name"], "query");
    assert_eq!(out[2]["namespace"], "sql");
    assert_eq!(out[3]["name"], "query");
    assert_eq!(out[3]["namespace"], "vector");
    assert_eq!(out[4]["name"], "query");
    assert_eq!(out[4]["namespace"], "cloud.sql");
    assert_eq!(out[5]["type"], "custom_tool_call");
    assert_eq!(out[5]["name"], "patch");
    assert_eq!(out[5]["namespace"], "sql");
    assert_eq!(out[6]["type"], "custom_tool_call");
    assert_eq!(out[6]["name"], "patch");
    assert_eq!(out[6]["namespace"], "vector");

    let mut replay_input = out.clone();
    for (call_id, kind, result) in [
        ("id-root", "function_call_output", "root-ok"),
        ("id-sql", "function_call_output", "sql-ok"),
        ("id-vec", "function_call_output", "vec-ok"),
        ("id-cloud", "function_call_output", "cloud-ok"),
        ("id-sql-patch", "custom_tool_call_output", "sql-patched"),
        ("id-vec-patch", "custom_tool_call_output", "vec-patched"),
    ] {
        replay_input.push(json!({"type": kind, "call_id": call_id, "output": result}));
    }
    replay_input.push(json!({
        "type": "function_call",
        "call_id": "manual-vec-call",
        "namespace": "vector",
        "name": "query",
        "arguments": "{\"embedding\":[0.9]}"
    }));
    replay_input.push(json!({
        "type": "function_call_output",
        "call_id": "manual-vec-call",
        "output": "manual-vec-ok"
    }));

    let mut replay_req = req.clone();
    replay_req["input"] = json!(replay_input);
    let replayed = convert_request(&replay_req, &cfg, &codec).unwrap();
    let response_parts = replayed.body["contents"][1]["parts"].as_array().unwrap();
    let response_names: Vec<&str> = response_parts
        .iter()
        .map(|p| p["functionResponse"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        response_names,
        vec![
            root_query.as_str(),
            sql_query.as_str(),
            vector_query.as_str(),
            cloud_sql_query.as_str(),
            sql_patch.as_str(),
            vector_patch.as_str()
        ]
    );
    assert_eq!(
        replayed.body["contents"][2]["parts"][0]["functionCall"]["name"],
        vector_query
    );
    assert_eq!(
        replayed.body["contents"][3]["parts"][0]["functionResponse"]["name"],
        vector_query
    );

    let mut bad_dup = req;
    bad_dup["tools"] = json!([{
        "type": "namespace",
        "name": "sql",
        "tools": [
            {"type": "function", "name": "query"},
            {"type": "custom", "name": "query"}
        ]
    }]);
    assert!(convert_request(&bad_dup, &cfg, &codec).is_err());
}

#[test]
fn custom_tools_streaming_lifecycle_multimodal_outputs_and_validation() {
    let codec = codec();
    let cfg = config();

    let mut req = base_request();
    req["tools"] = json!([
        {
            "type": "custom",
            "name": "raw_script",
            "description": "Execute raw bash script",
            "format": {"type": "text"}
        },
        {
            "type": "namespace",
            "name": "dev",
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply unified diff",
                "format": {"type": "text"}
            }]
        }
    ]);

    let converted = convert_request(&req, &cfg, &codec).unwrap();
    let declarations = converted.body["tools"][0]["functionDeclarations"]
        .as_array()
        .unwrap();
    assert_eq!(declarations.len(), 2);
    for decl in declarations {
        assert_eq!(
            decl["parametersJsonSchema"],
            json!({
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"]
            })
        );
    }

    let dev_patch_native = converted
        .tools
        .iter()
        .find(|(_, t)| t.namespace.as_deref() == Some("dev") && t.name == "apply_patch")
        .unwrap()
        .0
        .clone();

    let mut stream = ResponseStream::new(converted.clone(), "custom-stream");
    let mut events = Vec::new();
    events.extend(
        stream
            .feed(&json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [
                {"functionCall": {
                    "name": dev_patch_native,
                    "id": "custom-call-9",
                    "willContinue": true,
                    "partialArgs": [{"jsonPath": "$.input", "stringValue": "--- a/file.txt\n", "willContinue": true}]
                }, "thoughtSignature": "sig-custom-stream"}
            ]}}]}))
            .unwrap(),
    );
    assert!(
        !events
            .iter()
            .any(|e| e["type"] == "response.custom_tool_call_input.done")
    );

    events.extend(
        stream
            .feed(&json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [
                {"functionCall": {
                    "id": "custom-call-9",
                    "partialArgs": [{"jsonPath": "$.input", "stringValue": "+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new"}]
                }}
            ]}, "finishReason": "STOP"}]}))
            .unwrap(),
    );
    events.extend(stream.finish(&codec).unwrap());

    let added = events
        .iter()
        .find(|e| e["type"] == "response.output_item.added" && e["output_index"] == 1)
        .unwrap();
    assert_eq!(added["item"]["type"], "custom_tool_call");
    assert_eq!(added["item"]["status"], "in_progress");
    assert_eq!(added["item"]["input"], "");
    assert_eq!(added["item"]["name"], "apply_patch");
    assert_eq!(added["item"]["namespace"], "dev");

    let expected_patch = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new";
    let delta = events
        .iter()
        .find(|e| e["type"] == "response.custom_tool_call_input.delta")
        .unwrap();
    assert_eq!(delta["delta"], expected_patch);

    let input_done = events
        .iter()
        .find(|e| e["type"] == "response.custom_tool_call_input.done")
        .unwrap();
    assert_eq!(input_done["input"], expected_patch);

    let mut followup = req.clone();
    followup["input"] = json!([
        {
            "type": "custom_tool_call",
            "call_id": "unsigned-custom-1",
            "namespace": "dev",
            "name": "apply_patch",
            "input": expected_patch
        },
        {
            "type": "custom_tool_call_output",
            "call_id": "unsigned-custom-1",
            "output": [
                {"type": "input_text", "text": "Patched 1 file"},
                {"type": "input_file", "file_data": "YWJj", "mime_type": "application/pdf"},
                {"type": "input_audio", "format": "mp3", "data": "YWJj"}
            ]
        }
    ]);
    let followup_converted = convert_request(&followup, &cfg, &codec).unwrap();
    assert_eq!(
        followup_converted.body["contents"][0]["parts"][0]["functionCall"],
        json!({
            "name": dev_patch_native,
            "args": {"input": expected_patch},
            "id": "unsigned-custom-1"
        })
    );
    assert_eq!(
        followup_converted.body["contents"][1]["parts"][0]["functionResponse"],
        json!({
            "name": dev_patch_native,
            "id": "unsigned-custom-1",
            "response": {"result": ["Patched 1 file"]},
            "parts": [
                {"inlineData": {"mimeType": "application/pdf", "data": "YWJj"}},
                {"inlineData": {"mimeType": "audio/mpeg", "data": "YWJj"}}
            ]
        })
    );

    for invalid_args in [json!({}), json!({"input": 123}), json!({"input": null})] {
        let bad_native = json!({
            "candidates": [{
                "index": 0,
                "content": {"role": "model", "parts": [{"functionCall": {"name": dev_patch_native, "args": invalid_args}}]},
                "finishReason": "STOP"
            }]
        });
        assert!(convert_response(&bad_native, &converted, &codec, "bad-custom").is_err());
    }
}

#[test]
fn interleaved_parallel_partial_calls_preserve_start_order_and_complex_json_paths() {
    let codec = codec();
    let cfg = config();

    let mut req = base_request();
    req["tools"] = json!([
        {
            "type": "function",
            "name": "search_logs",
            "parameters": {"type": "object"}
        },
        {
            "type": "namespace",
            "name": "ops",
            "tools": [{
                "type": "custom",
                "name": "hotfix",
                "format": {"type": "text"}
            }]
        },
        {
            "type": "function",
            "name": "quick_ping",
            "parameters": {"type": "object"}
        }
    ]);
    let converted = convert_request(&req, &cfg, &codec).unwrap();
    let search_native = converted
        .tools
        .iter()
        .find(|(_, t)| t.name == "search_logs")
        .unwrap()
        .0
        .clone();
    let hotfix_native = converted
        .tools
        .iter()
        .find(|(_, t)| t.name == "hotfix")
        .unwrap()
        .0
        .clone();
    let ping_native = converted
        .tools
        .iter()
        .find(|(_, t)| t.name == "quick_ping")
        .unwrap()
        .0
        .clone();

    let mut stream = ResponseStream::new(converted, "interleaved");
    let mut all_events = Vec::new();

    let e1 = stream
        .feed(&json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [{
            "functionCall": {
                "name": search_native,
                "id": "call_first",
                "willContinue": true,
                "partialArgs": [{"jsonPath": "$.query", "stringValue": "SELECT ", "willContinue": true}]
            },
            "thoughtSignature": "signature-one"
        }]}}]}))
        .unwrap();
    all_events.extend(e1);

    let e2 = stream
        .feed(&json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [{
            "functionCall": {
                "name": hotfix_native,
                "id": "call_beta",
                "willContinue": true,
                "partialArgs": [{"jsonPath": "$.input", "stringValue": "step 1; ", "willContinue": true}]
            },
            "thoughtSignature": "sig-beta"
        }]}}]}))
        .unwrap();
    all_events.extend(e2);

    let e3 = stream
        .feed(&json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [{
            "functionCall": {
                "id": "call_first",
                "partialArgs": [
                    {"jsonPath": "$['filter.meta']['single\\'quote']", "stringValue": "escaped-single"},
                    {"jsonPath": "$[\"double\\\"quote\\\\slash\"][0].enabled", "boolValue": true},
                    {"jsonPath": "$.buckets[2]", "numberValue": 42.5},
                    {"jsonPath": "$.cursor", "nullValue": null},
                    {"jsonPath": "$.query", "stringValue": "* FROM ", "willContinue": true}
                ]
            }
        }]}}]}))
        .unwrap();
    all_events.extend(e3);

    let e4 = stream
        .feed(&json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [
            {"functionCall": {"id": "call_beta", "willContinue": true, "partialArgs": [{"jsonPath": "$.input", "stringValue": "step 2;"}]}},
            {"functionCall": {"id": "call_beta"}, "thoughtSignature": "sig-beta"},
            {"functionCall": {"name": ping_native, "id": "call_gamma", "args": {"target": "127.0.0.1"}}, "thoughtSignature": "sig-gamma"},
            {"text": "All parallel calls dispatched.", "thoughtSignature": "sig-text"}
        ]}}]}))
        .unwrap();
    assert!(
        e4.is_empty(),
        "downstream completed items must wait for earlier active partial call: {e4:?}"
    );

    let e5 = stream
        .feed(
            &json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [{
            "functionCall": {
                "id": "call_first",
                "partialArgs": [{"jsonPath": "$.query", "stringValue": "events"}]
            },
            "thoughtSignature": "signature-one"
        }]}, "finishReason": "STOP"}]}),
        )
        .unwrap();
    all_events.extend(e5);
    all_events.extend(stream.finish(&codec).unwrap());

    let final_response = &all_events.last().unwrap()["response"];
    let outputs = final_response["output"].as_array().unwrap();
    assert_eq!(outputs.len(), 5);
    assert_eq!(outputs[1]["call_id"], "call_first");
    assert_eq!(outputs[1]["type"], "function_call");
    assert_eq!(outputs[2]["call_id"], "call_beta");
    assert_eq!(outputs[2]["type"], "custom_tool_call");
    assert_eq!(outputs[2]["input"], "step 1; step 2;");
    assert_eq!(outputs[3]["call_id"], "call_gamma");
    assert_eq!(outputs[3]["type"], "function_call");
    assert_eq!(outputs[4]["type"], "message");
    assert_eq!(
        outputs[4]["content"][0]["text"],
        "All parallel calls dispatched."
    );

    let first_args: Value =
        serde_json::from_str(outputs[1]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(
        first_args,
        json!({
            "query": "SELECT * FROM events",
            "filter.meta": {"single'quote": "escaped-single"},
            "double\"quote\\slash": [{"enabled": true}],
            "buckets": [null, null, 42.5],
            "cursor": null
        })
    );

    assert_eq!(
        final_response["gemini"]["streamFunctionCallParts"]
            .as_array()
            .unwrap()
            .len(),
        6
    );

    let mut replay_req = req;
    replay_req["input"] = final_response["output"].clone();
    let replayed = convert_request(&replay_req, &cfg, &codec).unwrap();
    let native_parts = replayed.body["contents"][0]["parts"].as_array().unwrap();
    assert_eq!(native_parts.len(), 4);
    assert_eq!(native_parts[0]["thoughtSignature"], "signature-one");
    assert_eq!(native_parts[0]["functionCall"]["args"], first_args);
    assert_eq!(native_parts[1]["thoughtSignature"], "sig-beta");
    assert_eq!(
        native_parts[1]["functionCall"]["args"],
        json!({"input": "step 1; step 2;"})
    );
    assert_eq!(native_parts[2]["thoughtSignature"], "sig-gamma");
    assert_eq!(native_parts[3]["thoughtSignature"], "sig-text");
}

#[test]
fn partial_call_validation_errors_and_json_path_limits() {
    let cfg = config();
    let mut req = base_request();
    req["tools"] = json!([{"type": "function", "name": "exec", "parameters": {"type": "object"}}]);
    let converted = convert_request(&req, &cfg, &codec()).unwrap();
    let native_name = converted.tools.keys().next().unwrap().clone();

    let mut s = ResponseStream::new(converted.clone(), "conflict-sig");
    s.feed(&json!({"candidates": [{"content": {"parts": [{
        "functionCall": {"name": native_name, "id": "c1", "willContinue": true},
        "thoughtSignature": "sig-one"
    }]}}]}))
    .unwrap();
    assert!(
        s.feed(&json!({"candidates": [{"content": {"parts": [{
            "functionCall": {"id": "c1", "partialArgs": [{"jsonPath": "$.a", "stringValue": "x"}]},
            "thoughtSignature": "sig-two"
        }]}}]}))
        .is_err()
    );

    let mut s = ResponseStream::new(converted.clone(), "unknown-id");
    s.feed(&json!({"candidates": [{"content": {"parts": [{
        "functionCall": {"name": native_name, "id": "c1", "willContinue": true}
    }]}}]}))
    .unwrap();
    assert!(
        s.feed(&json!({"candidates": [{"content": {"parts": [{
            "functionCall": {"id": "unknown", "partialArgs": [{"jsonPath": "$.a", "stringValue": "x"}]}
        }]}}]}))
        .is_err()
    );

    let mut s = ResponseStream::new(converted.clone(), "no-active");
    assert!(
        s.feed(&json!({"candidates": [{"content": {"parts": [{
            "functionCall": {"partialArgs": [{"jsonPath": "$.a", "stringValue": "x"}]}
        }]}}]}))
        .is_err()
    );

    let mut s = ResponseStream::new(converted.clone(), "overwrite");
    s.feed(&json!({"candidates": [{"content": {"parts": [{
        "functionCall": {
            "name": native_name,
            "id": "c1",
            "willContinue": true,
            "partialArgs": [{"jsonPath": "$.count", "numberValue": 1}]
        }
    }]}}]}))
    .unwrap();
    assert!(
        s.feed(&json!({"candidates": [{"content": {"parts": [{
            "functionCall": {
                "id": "c1",
                "partialArgs": [{"jsonPath": "$.count", "numberValue": 2}]
            }
        }]}}]}))
        .is_err()
    );

    let mut s_ok = ResponseStream::new(converted.clone(), "max-index-ok");
    assert!(
        s_ok.feed(&json!({"candidates": [{"content": {"parts": [{
            "functionCall": {
                "name": native_name,
                "partialArgs": [{"jsonPath": "$.arr[65535]", "boolValue": true}]
            }
        }]}}]}))
        .is_ok()
    );

    let deep_129 = format!("${}", ".a".repeat(129));
    for bad_path in [
        "no_dollar",
        "$",
        "$.",
        "$..empty",
        "$.arr[65536]",
        "$.arr[-1]",
        "$['unclosed]",
        "$['trailing\\",
        deep_129.as_str(),
    ] {
        let mut s_bad = ResponseStream::new(converted.clone(), "bad-path");
        assert!(
            s_bad
                .feed(&json!({"candidates": [{"content": {"parts": [{
                    "functionCall": {
                        "name": native_name,
                        "partialArgs": [{"jsonPath": bad_path, "stringValue": "v"}]
                    }
                }]}}]}))
                .is_err(),
            "path {bad_path} should fail"
        );
    }

    let mut s_type = ResponseStream::new(converted, "container-conflict");
    assert!(
        s_type
            .feed(&json!({"candidates": [{"content": {"parts": [{
                "functionCall": {
                    "name": native_name,
                    "partialArgs": [
                        {"jsonPath": "$.node.key", "stringValue": "obj"},
                        {"jsonPath": "$.node[0]", "stringValue": "arr"}
                    ]
                }
            }]}}]}))
            .is_err()
    );
}

#[test]
fn nontext_parts_streaming_transitions_and_multimodal_request_conversion() {
    let codec = codec();
    let cfg = config();

    let mut req = base_request();
    req["input"] = json!([{
        "role": "user",
        "content": [
            {"type": "input_text", "text": "Analyze media inputs"},
            {"type": "input_image", "image_url": {"url": "gs://bucket/diagram.jpg"}},
            {"type": "input_image", "image_url": "YWJj"},
            {"type": "input_file", "file_data": "YWJj"},
            {"type": "input_file", "file_url": "https://example.com/spec.pdf", "mime_type": "application/pdf"},
            {"type": "input_audio", "format": "mp3", "data": "YWJj"},
            {"type": "gemini_part", "part": {"executableCode": {"language": "PYTHON", "code": "print(7)"}}}
        ]
    }]);

    let converted = convert_request(&req, &cfg, &codec).unwrap();
    let req_parts = converted.body["contents"][0]["parts"].as_array().unwrap();
    assert_eq!(req_parts.len(), 7);
    assert_eq!(
        req_parts[1],
        json!({"fileData": {"mimeType": "image/jpeg", "fileUri": "gs://bucket/diagram.jpg"}})
    );
    assert_eq!(
        req_parts[2],
        json!({"inlineData": {"mimeType": "image/jpeg", "data": "YWJj"}})
    );
    assert_eq!(
        req_parts[3],
        json!({"inlineData": {"mimeType": "application/pdf", "data": "YWJj"}})
    );
    assert_eq!(
        req_parts[4],
        json!({"fileData": {"mimeType": "application/pdf", "fileUri": "https://example.com/spec.pdf"}})
    );
    assert_eq!(
        req_parts[5],
        json!({"inlineData": {"mimeType": "audio/mpeg", "data": "YWJj"}})
    );
    assert_eq!(
        req_parts[6],
        json!({"executableCode": {"language": "PYTHON", "code": "print(7)"}})
    );

    let mut stream = ResponseStream::new(converted, "nontext-stream");
    let mut events = Vec::new();
    for part in [
        json!({"text": "Running code: "}),
        json!({"text": "step 1."}),
        json!({"executableCode": {"language": "PYTHON", "code": "print(2 + 2)"}}),
        json!({"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": "4\n"}}),
        json!({"inlineData": {"mimeType": "image/png", "data": "YWJj"}, "thoughtSignature": "sig-inline-image"}),
        json!({"text": "Done rendering."}),
    ] {
        events.extend(
            stream
                .feed(&json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [part]}}]}))
                .unwrap(),
        );
    }
    events.extend(
        stream
            .feed(&json!({"candidates": [{"index": 0, "finishReason": "STOP"}]}))
            .unwrap(),
    );
    events.extend(stream.finish(&codec).unwrap());

    let first_text_done_seq = events
        .iter()
        .find(|e| e["type"] == "response.output_text.done" && e["output_index"] == 1)
        .unwrap()["sequence_number"]
        .as_u64()
        .unwrap();
    let exec_code_added_seq = events
        .iter()
        .find(|e| e["type"] == "response.output_item.added" && e["output_index"] == 2)
        .unwrap()["sequence_number"]
        .as_u64()
        .unwrap();
    assert!(first_text_done_seq < exec_code_added_seq);

    let done_items: Vec<Value> = events
        .iter()
        .filter(|e| e["type"] == "response.output_item.done")
        .map(|e| e["item"].clone())
        .collect();
    assert_eq!(done_items.len(), 6);
    assert_eq!(done_items[1]["content"][0]["text"], "Running code: step 1.");
    assert_eq!(done_items[2]["content"][0]["type"], "gemini_part");
    assert_eq!(done_items[3]["content"][0]["type"], "gemini_part");
    assert_eq!(
        done_items[4]["content"][0]["part"]["thoughtSignature"],
        "sig-inline-image"
    );
    assert_eq!(done_items[5]["content"][0]["text"], "Done rendering.");

    let mut replay_req = req;
    replay_req["input"] = json!(done_items);
    let replayed = convert_request(&replay_req, &cfg, &codec).unwrap();
    assert_eq!(
        replayed.body["contents"][0]["parts"],
        json!([
            {"text": "Running code: step 1."},
            {"executableCode": {"language": "PYTHON", "code": "print(2 + 2)"}},
            {"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": "4\n"}},
            {"inlineData": {"mimeType": "image/png", "data": "YWJj"}, "thoughtSignature": "sig-inline-image"},
            {"text": "Done rendering."}
        ])
    );
}

#[test]
fn stream_failure_modes_state_transitions_and_signature_boundaries() {
    let codec = codec();
    let cfg = config();
    let converted = convert_request(&base_request(), &cfg, &codec).unwrap();

    let mut max_tokens_stream = ResponseStream::new(converted.clone(), "max-tok");
    max_tokens_stream
        .feed(&json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": "Cut off"}]}, "finishReason": "MAX_TOKENS"}]}))
        .unwrap();
    let max_events = max_tokens_stream.finish(&codec).unwrap();
    let terminal = max_events.last().unwrap();
    assert_eq!(terminal["type"], "response.incomplete");
    assert_eq!(terminal["response"]["status"], "incomplete");
    assert_eq!(
        terminal["response"]["incomplete_details"],
        json!({"reason": "max_output_tokens"})
    );
    assert!(
        max_tokens_stream
            .feed(&json!({"candidates": [{"index": 0}]}))
            .is_err()
    );
    assert!(max_tokens_stream.finish(&codec).is_err());

    let mut blocked_stream = ResponseStream::new(converted.clone(), "blocked");
    blocked_stream
        .feed(&json!({
            "candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": "Blocked"}]}, "finishReason": "STOP"}],
            "promptFeedback": {"blockReason": "PROHIBITED_CONTENT"}
        }))
        .unwrap();
    let blocked_events = blocked_stream.finish(&codec).unwrap();
    let blocked_terminal = blocked_events.last().unwrap();
    assert_eq!(blocked_terminal["type"], "response.failed");
    assert_eq!(blocked_terminal["response"]["status"], "failed");
    assert_eq!(
        blocked_terminal["response"]["error"]["code"],
        "invalid_prompt"
    );

    let mut bad_cand = ResponseStream::new(converted.clone(), "bad-cand");
    assert!(
        bad_cand
            .feed(&json!({"candidates": [{"index": 1, "content": {"parts": [{"text": "alt"}]}}]}))
            .is_err()
    );
    let mut multi_cand = ResponseStream::new(converted.clone(), "multi-cand");
    assert!(
        multi_cand
            .feed(&json!({"candidates": [{"index": 0}, {"index": 1}]}))
            .is_err()
    );

    let mut orphan_sig = ResponseStream::new(converted.clone(), "orphan-sig");
    assert!(
        orphan_sig
            .feed(&json!({"candidates": [{"index": 0, "content": {"parts": [{"thoughtSignature": "orphan"}]}}]}))
            .is_err()
    );
    let mut conflict_sig = ResponseStream::new(converted.clone(), "conflict-detached");
    conflict_sig
        .feed(&json!({"candidates": [{"index": 0, "content": {"parts": [{"text": "signed", "thoughtSignature": "s1"}]}}]}))
        .unwrap();
    assert!(
        conflict_sig
            .feed(&json!({"candidates": [{"index": 0, "content": {"parts": [{"thoughtSignature": "s2"}]}}]}))
            .is_err()
    );

    let mut split_stream = ResponseStream::new(converted, "split-sig");
    let mut split_events = Vec::new();
    for chunk in [
        json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": "First segment"}]}}]}),
        json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [{"thoughtSignature": "sig-boundary"}]}}]}),
        json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": "Second segment"}]}, "finishReason": "STOP"}]}),
    ] {
        split_events.extend(split_stream.feed(&chunk).unwrap());
    }
    split_events.extend(split_stream.finish(&codec).unwrap());

    let final_resp = &split_events.last().unwrap()["response"];
    let out = final_resp["output"].as_array().unwrap();
    assert_eq!(out.len(), 3);
    assert_eq!(out[1]["content"][0]["text"], "First segment");
    assert_eq!(out[2]["content"][0]["text"], "Second segment");

    let mut replay_req = base_request();
    replay_req["input"] = final_resp["output"].clone();
    let replayed = convert_request(&replay_req, &cfg, &codec).unwrap();
    assert_eq!(
        replayed.body["contents"][0]["parts"],
        json!([
            {"text": "First segment", "thoughtSignature": "sig-boundary"},
            {"text": "Second segment"}
        ])
    );
}

#[test]
fn tampered_projections_are_rejected_while_benign_normalizations_succeed() {
    let codec = codec();
    let cfg = config();

    let mut req = base_request();
    req["tools"] = json!([
        {
            "type": "namespace",
            "name": "fs",
            "tools": [
                {
                    "type": "function",
                    "name": "exec",
                    "parameters": {"type": "object"}
                },
                {
                    "type": "custom",
                    "name": "patch",
                    "format": {"type": "text"}
                }
            ]
        },
        {
            "type": "namespace",
            "name": "net",
            "tools": [{
                "type": "function",
                "name": "exec",
                "parameters": {"type": "object"}
            }]
        }
    ]);
    let converted = convert_request(&req, &cfg, &codec).unwrap();
    let fs_exec_native = converted
        .tools
        .iter()
        .find(|(_, t)| t.namespace.as_deref() == Some("fs") && t.name == "exec")
        .unwrap()
        .0
        .clone();
    let fs_patch_native = converted
        .tools
        .iter()
        .find(|(_, t)| t.namespace.as_deref() == Some("fs") && t.name == "patch")
        .unwrap()
        .0
        .clone();

    let native = json!({
        "candidates": [{
            "index": 0,
            "content": {
                "role": "model",
                "parts": [
                    {"text": "Thinking...", "thought": true, "thoughtSignature": "sig-t"},
                    {"text": "Status report", "thoughtSignature": "sig-m"},
                    {"inlineData": {"mimeType": "image/png", "data": "YWJj"}, "thoughtSignature": "sig-img"},
                    {"functionCall": {"name": fs_exec_native, "args": {"cmd": "ls", "flags": ["-a", "-l"]}, "id": "call-exec"}, "thoughtSignature": "sig-fc"},
                    {"functionCall": {"name": fs_patch_native, "args": {"input": "patch-body"}, "id": "call-patch"}, "thoughtSignature": "sig-custom"}
                ]
            },
            "finishReason": "STOP"
        }]
    });
    let response = convert_response(&native, &converted, &codec, "tamper").unwrap();
    let base_output = response["output"].as_array().unwrap().clone();
    assert_eq!(base_output.len(), 5);

    let mut benign = base_output.clone();
    benign[0]["summary"] = json!([{"type": "summary_text", "text": "Edited client summary"}]);
    benign[1]["id"] = json!("custom_client_msg_id");
    benign[1]["content"][0]["type"] = json!("text");
    benign[1]["content"][0]
        .as_object_mut()
        .unwrap()
        .remove("annotations");
    benign[3]["arguments"] = json!("{\n  \"flags\": [\"-a\", \"-l\"],\n  \"cmd\": \"ls\"\n}");
    let mut benign_req = req.clone();
    benign_req["input"] = json!(benign);
    assert!(convert_request(&benign_req, &cfg, &codec).is_ok());

    let mut cases: Vec<(&str, Vec<Value>)> = Vec::new();

    let mut c_type = base_output.clone();
    c_type[3]["type"] = json!("custom_tool_call");
    cases.push(("item type", c_type));

    let mut c_role = base_output.clone();
    c_role[1]["role"] = json!("user");
    cases.push(("message role", c_role));

    let mut c_text = base_output.clone();
    c_text[1]["content"][0]["text"] = json!("Tampered status report");
    cases.push(("message text", c_text));

    let mut c_annotations = base_output.clone();
    c_annotations[1]["content"][0]["annotations"] = json!([{"type": "file_citation"}]);
    cases.push(("non-empty annotations", c_annotations));

    let mut c_media = base_output.clone();
    c_media[2]["content"][0]["part"]["inlineData"]["data"] = json!("ZGVm");
    cases.push(("nontext gemini_part data", c_media));

    let mut c_call_id = base_output.clone();
    c_call_id[3]["call_id"] = json!("call-exec-altered");
    cases.push(("function call_id", c_call_id));

    let mut c_name = base_output.clone();
    c_name[3]["name"] = json!("other_exec");
    cases.push(("function name", c_name));

    let mut c_namespace = base_output.clone();
    c_namespace[3]["namespace"] = json!("net");
    cases.push(("cross-namespace swap", c_namespace));

    let mut c_drop_ns = base_output.clone();
    c_drop_ns[3].as_object_mut().unwrap().remove("namespace");
    cases.push(("dropped namespace", c_drop_ns));

    let mut c_args = base_output.clone();
    c_args[3]["arguments"] = json!("{\"cmd\":\"rm -rf /\",\"flags\":[\"-a\",\"-l\"]}");
    cases.push(("function arguments value", c_args));

    let mut c_custom_input = base_output.clone();
    c_custom_input[4]["input"] = json!("tampered-patch-body");
    cases.push(("custom_tool_call input", c_custom_input));

    let mut c_swapped = base_output.clone();
    c_swapped.swap(3, 4);
    cases.push(("swapped output item order", c_swapped));

    let mut c_truncated = base_output.clone();
    c_truncated.pop();
    cases.push(("truncated trailing output item", c_truncated));

    for (label, tampered_input) in cases {
        let mut bad_req = req.clone();
        bad_req["input"] = json!(tampered_input);
        assert!(
            convert_request(&bad_req, &cfg, &codec).is_err(),
            "expected tampered projection ({label}) to be rejected"
        );
    }

    let mut undeclared_req = req;
    undeclared_req["tools"] = json!([{
        "type": "namespace",
        "name": "fs",
        "tools": [{"type": "function", "name": "exec", "parameters": {"type": "object"}}]
    }]);
    undeclared_req["input"] = json!(base_output);
    // Removing a declaration is legitimate; the authenticated historical turn
    // stays intact, while the removed tool is unavailable for new calls.
    let replayed = convert_request(&undeclared_req, &cfg, &codec).unwrap();
    assert_eq!(replayed.tools.len(), 1);
}

#[test]
fn retired_tools_replay_exactly_without_becoming_callable_again() {
    let codec = codec();
    let cfg = config();
    for custom in [false, true] {
        for namespaced in [false, true] {
            let tool = if custom {
                json!({"type":"custom","name":"get_goal","format":{"type":"text"}})
            } else {
                json!({"type":"function","name":"get_goal","parameters":{"type":"object"}})
            };
            let mut req = base_request();
            req["tools"] = if namespaced {
                json!([{"type":"namespace","name":"functions","tools":[tool]}])
            } else {
                json!([tool])
            };
            let first = convert_request(&req, &cfg, &codec).unwrap();
            let name = first.tools.keys().next().unwrap();
            for native_id in [None, Some("native-goal-id")] {
                let mut call = json!({"name":name,"args":if custom { json!({"input":"goal\nstatus"}) } else { json!({}) }});
                if let Some(id) = native_id {
                    call["id"] = json!(id);
                }
                let parts = json!([
                    {"thought":true,"text":"Check the current goal.","thoughtSignature":"reasoning-signature"},
                    {"functionCall":call,"thoughtSignature":"tool-signature"}
                ]);
                let native = json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}]});
                let response = convert_response(&native, &first, &codec, "retired-tool").unwrap();
                let mut history = response["output"].as_array().unwrap().clone();
                let call_id = history[1]["call_id"].clone();
                history.push(json!({"type":if custom {"custom_tool_call_output"} else {"function_call_output"},"call_id":call_id,"output":"complete"}));
                for current_tools in [
                    None,
                    Some(json!([])),
                    Some(
                        json!([{"type":"function","name":"run_shell","parameters":{"type":"object"}}]),
                    ),
                ] {
                    let mut next = req.clone();
                    next.as_object_mut().unwrap().remove("tools");
                    if let Some(tools) = current_tools {
                        next["tools"] = tools;
                    }
                    next["input"] = json!(history);
                    let replayed = convert_request(&next, &cfg, &codec).unwrap();
                    assert_eq!(replayed.body["contents"][0]["parts"], parts);
                    let result = &replayed.body["contents"][1]["parts"][0]["functionResponse"];
                    assert_eq!(result["name"], *name);
                    assert_eq!(result.get("id").and_then(Value::as_str), native_id);
                    assert_eq!(result["response"]["result"], "complete");
                    assert!(!replayed.tools.contains_key(name));
                    assert!(convert_response(&native, &replayed, &codec, "new-call").is_err());
                    let mut tampered = next;
                    tampered["input"][1]["name"] = json!("different_tool");
                    assert!(convert_request(&tampered, &cfg, &codec).is_err());
                }
                // Unsigned history (e.g. imported sessions) also describes past
                // calls, independently of which tools are now available.
                history.remove(0);
                req["tools"] = json!([]);
                req["input"] = json!(history);
                let unsigned = convert_request(&req, &cfg, &codec).unwrap();
                assert_eq!(
                    unsigned.body["contents"][0]["parts"][0]["functionCall"]["name"],
                    *name
                );
                assert_eq!(
                    unsigned.body["contents"][1]["parts"][0]["functionResponse"]["name"],
                    *name
                );
                assert!(unsigned.tools.is_empty());
                assert!(unsigned.body.get("tools").is_none());
            }
        }
    }
}

#[test]
fn late_usage_metadata_progression_and_token_calculation_fallbacks() {
    let codec = codec();
    let cfg = config();
    let converted = convert_request(&base_request(), &cfg, &codec).unwrap();

    let mut stream = ResponseStream::new(converted, "late-usage");
    let mut events = Vec::new();

    events.extend(
        stream
            .feed(&json!({
                "candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": "Reasoning step...", "thought": true}]}}],
                "usageMetadata": {"promptTokenCount": 120}
            }))
            .unwrap(),
    );

    events.extend(
        stream
            .feed(&json!({
                "candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": "Final answer."}]}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 120, "candidatesTokenCount": 10}
            }))
            .unwrap(),
    );

    events.extend(
        stream
            .feed(&json!({
                "responseId": "late-resp-id",
                "modelVersion": "gemini-2.5-pro-late",
                "usageMetadata": {
                    "promptTokenCount": 120,
                    "candidatesTokenCount": 35,
                    "thoughtsTokenCount": 45,
                    "cachedContentTokenCount": 80
                }
            }))
            .unwrap(),
    );

    events.extend(stream.finish(&codec).unwrap());
    let completed = &events.last().unwrap()["response"];
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["gemini"]["responseId"], "late-resp-id");
    assert_eq!(completed["gemini"]["modelVersion"], "gemini-2.5-pro-late");
    assert_eq!(
        completed["usage"],
        json!({
            "input_tokens": 120,
            "input_tokens_details": {"cached_tokens": 80},
            "output_tokens": 80,
            "output_tokens_details": {"reasoning_tokens": 45},
            "total_tokens": 200,
            "gemini": {
                "promptTokenCount": 120,
                "candidatesTokenCount": 35,
                "thoughtsTokenCount": 45,
                "cachedContentTokenCount": 80
            }
        })
    );

    assert_eq!(
        usage(
            &json!({"usageMetadata": {"promptTokenCount": 50, "thoughtsTokenCount": 12, "totalTokenCount": 70}})
        ),
        Some(json!({
            "input_tokens": 50,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 12,
            "output_tokens_details": {"reasoning_tokens": 12},
            "total_tokens": 70,
            "gemini": {"promptTokenCount": 50, "thoughtsTokenCount": 12, "totalTokenCount": 70}
        }))
    );
    assert!(usage(&json!({"usageMetadata": {"candidatesTokenCount": 10}})).is_none());
}

#[test]
fn model_prefix_normalization_and_persisted_codec_replay_with_unsigned_history() {
    let key = [0x9c; 32];
    let codec_initial = ReasoningCodec::new(&key);
    let codec_restarted = ReasoningCodec::new(&key);
    let cfg = config();

    // Turn 1 uses "gemini/models/gemini-2.5-pro"
    let mut req1 = base_request();
    req1["model"] = json!("gemini/models/gemini-2.5-pro");
    req1["tools"] = json!([
        {
            "type": "namespace",
            "name": "repo",
            "tools": [{
                "type": "function",
                "name": "status",
                "parameters": {"type": "object"}
            }]
        }
    ]);
    let conv1 = convert_request(&req1, &cfg, &codec_initial).unwrap();
    assert_eq!(conv1.model, "gemini-2.5-pro");
    assert_eq!(conv1.response_model, "gemini/models/gemini-2.5-pro");
    let status_native = conv1.tools.keys().next().unwrap().clone();

    let native1 = json!({
        "candidates": [{
            "index": 0,
            "content": {
                "role": "model",
                "parts": [
                    {"text": "Checking repo status.", "thought": true, "thoughtSignature": "sig-restart-1"},
                    {"functionCall": {"name": status_native, "args": {"short": true}, "id": "status-1"}, "thoughtSignature": "sig-restart-2"}
                ]
            },
            "finishReason": "STOP"
        }]
    });
    let resp1 = convert_response(&native1, &conv1, &codec_initial, "r1").unwrap();
    assert_eq!(resp1["model"], "gemini/models/gemini-2.5-pro");

    // Turn 2 uses "models/gemini-2.5-pro" (which normalizes to the same canonical "gemini-2.5-pro" AAD)
    // and a freshly instantiated ReasoningCodec with the same 32-byte key.
    let mut req2 = req1.clone();
    req2["model"] = json!("models/gemini-2.5-pro");
    let mut input2 = vec![
        json!({"role": "user", "content": "Initial question"}),
        json!({"role": "assistant", "content": "Prior unsigned assistant note"}),
    ];
    input2.extend(resp1["output"].as_array().unwrap().clone());
    input2.push(json!({
        "type": "function_call_output",
        "call_id": "status-1",
        "output": {"clean": true}
    }));
    input2.push(json!({
        "type": "gemini_content",
        "content": {
            "role": "user",
            "parts": [{"text": "Injected native user turn"}]
        }
    }));
    req2["input"] = json!(input2);

    let conv2 = convert_request(&req2, &cfg, &codec_restarted).unwrap();
    assert_eq!(conv2.model, "gemini-2.5-pro");
    let contents = conv2.body["contents"].as_array().unwrap();
    // [0] user ("Initial question")
    // [1] model ("Prior unsigned assistant note")
    // [2] model (signed Turn 1 - kept separate from prior model turn [1])
    // [3] user (functionResponse with structured JSON object result)
    // [4] user (gemini_content native turn)
    assert_eq!(contents.len(), 5);
    assert_eq!(
        contents[1]["parts"],
        json!([{"text": "Prior unsigned assistant note"}])
    );
    assert_eq!(contents[2], native1["candidates"][0]["content"]);
    assert_eq!(
        contents[3]["parts"][0]["functionResponse"]["response"]["result"],
        json!({"clean": true})
    );
    assert_eq!(
        contents[4]["parts"],
        json!([{"text": "Injected native user turn"}])
    );
}

#[test]
fn implicit_continuation_after_model_turn_preserves_signed_history() {
    let cfg = config();
    let codec = codec();
    let req = base_request();
    let first = convert_request(&req, &cfg, &codec).unwrap();
    let parts = json!([
        {"thought":true,"text":"Resume the active goal.","thoughtSignature":"continuation-thought"},
        {"text":"The initial check is complete.","thoughtSignature":"continuation-text"}
    ]);
    let response = convert_response(
        &json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}]}),
        &first,
        &codec,
        "continuation",
    )
    .unwrap();
    let mut next = req.clone();
    next["input"] = response["output"].clone();
    let replayed = convert_request(&next, &cfg, &codec).unwrap();
    assert_eq!(
        replayed.body["contents"],
        json!([
            {"role":"model","parts":parts},
            {"role":"user","parts":[{"text":"Continue."}]}
        ])
    );
    for history in [
        json!([{"role":"user","content":"Start"},{"role":"assistant","content":"One step done."}]),
        json!([{"type":"gemini_content","content":{"role":"model","parts":[{"text":"One step done."}]}}]),
    ] {
        next["input"] = history;
        let converted = convert_request(&next, &cfg, &codec).unwrap();
        let contents = converted.body["contents"].as_array().unwrap();
        assert_eq!(
            contents.last().unwrap(),
            &json!({"role":"user","parts":[{"text":"Continue."}]})
        );
        assert_eq!(
            contents[contents.len() - 2]["parts"][0]["text"],
            "One step done."
        );
    }
    // Explicit user turns already carry their own continuation instruction.
    next["input"] = response["output"].clone();
    next["input"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"user","content":"Next check"}));
    let explicit = convert_request(&next, &cfg, &codec).unwrap();
    assert_eq!(explicit.body["contents"].as_array().unwrap().len(), 2);
    assert_eq!(
        explicit.body["contents"][1]["parts"],
        json!([{"text":"Next check"}])
    );
}

#[test]
fn tool_results_and_following_user_messages_keep_separate_turns() {
    let cfg = config();
    let codec = codec();
    let mut req = base_request();
    req["tools"] =
        json!([{"type":"function","name":"exec_command","parameters":{"type":"object"}}]);
    let first = convert_request(&req, &cfg, &codec).unwrap();
    let name = first.tools.keys().next().unwrap();
    let preserved = json!([
        {"text":"", "thoughtSignature":"empty-signature"},
        {"text":"Inspecting the repository."},
        {"functionCall":{"name":name,"args":{},"id":"exec-1"},"thoughtSignature":"tool-signature"}
    ]);
    let mut native_parts = preserved.as_array().unwrap().clone();
    native_parts.extend([json!({"text":""}), json!({"text":""})]);
    let native = json!({"candidates":[{"content":{"role":"model","parts":native_parts},"finishReason":"STOP"}]});
    let response = convert_response(&native, &first, &codec, "model-empty").unwrap();
    assert_eq!(
        response["gemini"]["candidates"][0]["content"]["parts"],
        json!(native_parts)
    );
    let mut input = response["output"].as_array().unwrap().clone();
    input.push(json!({"type":"function_call_output","call_id":"exec-1","output":"done"}));
    input.push(json!({"role":"user","content":"Continue the goal."}));
    req["input"] = json!(input);
    req["tools"] = json!([]);
    let converted = convert_request(&req, &cfg, &codec).unwrap();
    assert_eq!(converted.body["contents"][0]["parts"], json!(native_parts));
    assert_eq!(
        converted.body["contents"][1]["parts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        converted.body["contents"][2]["parts"],
        json!([{"text":"Continue the goal."}])
    );
    assert_eq!(
        converted.body["contents"][1]["parts"][0]["functionResponse"]["id"],
        "exec-1"
    );
    for retained in [
        json!({"text":"","thoughtSignature":"signature"}),
        json!({"text":"","thought":true}),
        json!({"text":"","partMetadata":{"test":true}}),
    ] {
        let mut parts = preserved.as_array().unwrap().clone();
        parts.push(retained);
        // Native replay must retain empty signature carriers and all metadata.
        req["input"] = json!([{"type":"gemini_content","content":{"role":"model","parts":parts}},{"role":"user","content":"Continue"}]);
        let converted = convert_request(&req, &cfg, &codec).unwrap();
        assert_eq!(converted.body["contents"][0]["parts"], json!(parts));
    }
}

#[test]
fn late_instructions_keep_priority_order_and_history_boundaries() {
    let cfg = config();
    let codec = codec();
    let first = convert_request(&base_request(), &cfg, &codec).unwrap();
    let parts = json!([{"text":"A prior answer.","thoughtSignature":"prior-signature"}]);
    let response = convert_response(
        &json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}]}),
        &first,
        &codec,
        "instructions",
    )
    .unwrap();
    let mut input = vec![
        json!({"role":"developer","content":"Initial instructions."}),
        json!({"role":"user","content":"Start"}),
    ];
    input.extend(response["output"].as_array().unwrap().clone());
    input.extend([
        json!({"role":"developer","content":"Use the updated repository rules from now on."}),
        json!({"role":"user","content":"Continue"}),
        json!({"role":"system","content":[{"type":"input_text","text":"Retain the new safety constraint."}]}),
    ]);
    let mut req = base_request();
    req["input"] = json!(input);
    let converted = convert_request(&req, &cfg, &codec).unwrap();
    assert_eq!(converted.body["contents"][1]["parts"], parts);
    let system = converted.body["systemInstruction"]["parts"]
        .as_array()
        .unwrap();
    assert!(
        system[0]["text"]
            .as_str()
            .unwrap()
            .contains("role=developer")
    );
    assert_eq!(system[1]["text"], "Initial instructions.");
    assert!(
        system[3]["text"]
            .as_str()
            .unwrap()
            .contains("role=developer")
    );
    assert_eq!(
        system[4]["text"],
        "Use the updated repository rules from now on."
    );
    assert!(system[6]["text"].as_str().unwrap().contains("role=system"));
    assert_eq!(system[7]["text"], "Retain the new safety constraint.");
    let user = converted.body["contents"][2]["parts"].as_array().unwrap();
    assert!(
        user[0]["text"]
            .as_str()
            .unwrap()
            .contains("Instruction update takes effect here")
    );
    assert_eq!(user[1]["text"], "Continue");
    assert_ne!(user[0], user[2]);
    assert_eq!(
        converted.body,
        convert_request(&req, &cfg, &codec).unwrap().body
    );
}

#[test]
fn image_detail_maps_per_image_without_changing_bytes_or_urls() {
    let mut req = base_request();
    let cases = [
        (Value::Null, Value::Null),
        (json!("auto"), Value::Null),
        (json!("low"), json!({"level":"MEDIA_RESOLUTION_LOW"})),
        (json!("high"), json!({"level":"MEDIA_RESOLUTION_HIGH"})),
        (
            json!("original"),
            json!({"level":"MEDIA_RESOLUTION_ULTRA_HIGH"}),
        ),
    ];
    for (detail, expected) in cases {
        for nested in [false, true] {
            let mut image = json!({"type":"input_image","image_url":"data:image/png;base64,YWJj"});
            if nested {
                image["image_url"] = json!({"url":"data:image/png;base64,YWJj","detail":detail});
            } else {
                image["detail"] = detail.clone();
            }
            req["input"] = json!([{"role":"user","content":[image]}]);
            let out = convert_request(&req, &config(), &codec()).unwrap();
            let part = &out.body["contents"][0]["parts"][0];
            assert_eq!(part["mediaResolution"], expected);
            assert_eq!(
                part["inlineData"],
                json!({"mimeType":"image/png","data":"YWJj"})
            );
        }
    }
    req["input"] = json!([{"role":"user","content":[
        {"type":"image","image_url":"gs://bucket/screenshot.png","mime_type":"image/png","detail":"low"},
        {"type":"input_image","image_url":"https://example.com/detail.png","mime_type":"image/png","detail":"original"}
    ]}]);
    req["gemini"] = json!({"native_request":{"generationConfig":{"mediaResolution":"MEDIA_RESOLUTION_MEDIUM"}}});
    let out = convert_request(&req, &config(), &codec()).unwrap();
    assert_eq!(
        out.body["generationConfig"]["mediaResolution"],
        "MEDIA_RESOLUTION_MEDIUM"
    );
    assert_eq!(
        out.body["contents"][0]["parts"][0]["mediaResolution"]["level"],
        "MEDIA_RESOLUTION_LOW"
    );
    assert_eq!(
        out.body["contents"][0]["parts"][1]["mediaResolution"]["level"],
        "MEDIA_RESOLUTION_ULTRA_HIGH"
    );
    assert_eq!(
        out.body["contents"][0]["parts"][1]["fileData"]["fileUri"],
        "https://example.com/detail.png"
    );
}

#[test]
fn malformed_and_conflicting_image_details_are_rejected() {
    for detail in [
        json!("invalid"),
        json!(2),
        json!({}),
        json!([]),
        json!(false),
    ] {
        for nested in [false, true] {
            let mut req = base_request();
            let image = if nested {
                json!({"type":"input_image","image_url":{"url":"YWJj","detail":detail}})
            } else {
                json!({"type":"input_image","image_url":"YWJj","detail":detail})
            };
            req["input"] = json!([{"role":"user","content":[image]}]);
            assert!(
                convert_request(&req, &config(), &codec())
                    .unwrap_err()
                    .to_string()
                    .contains("Image detail")
            );
        }
    }
    let mut req = base_request();
    req["input"] = json!([{"role":"user","content":[{"type":"input_image","image_url":{"url":"YWJj","detail":"low"},"detail":"high"}]}]);
    assert!(
        convert_request(&req, &config(), &codec())
            .unwrap_err()
            .to_string()
            .contains("Conflicting image detail")
    );
    req["input"][0]["content"][0]["detail"] = json!("low");
    assert!(convert_request(&req, &config(), &codec()).is_ok());
}

#[test]
fn signed_tool_image_results_keep_resolution_and_parallel_call_association() {
    for kind in ["function", "custom"] {
        let cfg = config();
        let codec = codec();
        let mut req = base_request();
        req["tools"] = json!([{"type":kind,"name":"screenshot"}]);
        let first = convert_request(&req, &cfg, &codec).unwrap();
        let name = first.tools.keys().next().unwrap();
        let args = if kind == "custom" {
            json!({"input":"capture"})
        } else {
            json!({})
        };
        let signed = json!([
            {"functionCall":{"name":name,"args":args,"id":"shot1"},"thoughtSignature":"signature-1"},
            {"functionCall":{"name":name,"args":args,"id":"shot2"},"thoughtSignature":"signature-2"}
        ]);
        let response = convert_response(&json!({"candidates":[{"content":{"role":"model","parts":signed},"finishReason":"STOP"}]}), &first, &codec, "image-test").unwrap();
        let mut input = vec![json!({"role":"user","content":"Inspect screenshots"})];
        input.extend(response["output"].as_array().unwrap().clone());
        let output_type = if kind == "custom" {
            "custom_tool_call_output"
        } else {
            "function_call_output"
        };
        input.push(json!({"type":output_type,"call_id":"shot1","output":[
            {"type":"input_text","text":"Screenshot results"},
            {"type":"input_image","image_url":"data:image/png;base64,YWJj","detail":"low"},
            {"type":"input_image","image_url":"data:image/png;base64,ZGVm","detail":"original"}
        ]}));
        input.push(json!({"type":output_type,"call_id":"shot2","output":[
            {"type":"input_image","image_url":"data:image/png;base64,Z2hp","detail":"high"}
        ]}));
        req["input"] = json!(input);
        req["tools"] = json!([]);
        let out = convert_request(&req, &cfg, &codec).unwrap();
        assert_eq!(out.body["contents"][1]["parts"], signed);
        let parts = &out.body["contents"][2]["parts"];
        assert_eq!(parts.as_array().unwrap().len(), 2);
        assert_eq!(parts[0]["functionResponse"]["id"], "shot1");
        assert_eq!(parts[1]["functionResponse"]["id"], "shot2");
        assert_eq!(
            parts[0]["mediaResolution"]["level"],
            "MEDIA_RESOLUTION_ULTRA_HIGH"
        );
        assert_eq!(
            parts[1]["mediaResolution"]["level"],
            "MEDIA_RESOLUTION_HIGH"
        );
        assert_eq!(
            parts[0]["functionResponse"]["response"]["result"],
            json!(["Screenshot results"])
        );
        assert_eq!(
            parts[0]["functionResponse"]["response"]["requestedMediaResolutions"],
            json!([{"level":"MEDIA_RESOLUTION_LOW"},{"level":"MEDIA_RESOLUTION_ULTRA_HIGH"}])
        );
        assert_eq!(
            parts[0]["functionResponse"]["parts"],
            json!([
                {"inlineData":{"mimeType":"image/png","data":"YWJj"}},
                {"inlineData":{"mimeType":"image/png","data":"ZGVm"}}
            ])
        );
        assert_eq!(
            parts[1]["functionResponse"]["parts"][0]["inlineData"]["data"],
            "Z2hp"
        );
    }
}
