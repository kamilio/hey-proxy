use super::*;
use anyhow::{anyhow, bail, ensure};
use hey_proxy::gemini::ReasoningCodec;

pub(super) struct Converted {
    pub body: Value,
    pub stream: bool,
    pub include_usage: bool,
    pub exclude_reasoning: bool,
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{key} must be a string"))
}
fn content(value: &Value, assistant: bool) -> Result<Vec<Value>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let kind = if assistant {
        "output_text"
    } else {
        "input_text"
    };
    if let Some(text) = value.as_str() {
        return Ok(vec![json!({"type":kind,"text":text})]);
    }
    let mut out = Vec::new();
    for part in value
        .as_array()
        .ok_or_else(|| anyhow!("message content must be text or an array"))?
    {
        out.push(match string(part, "type")? {
            "text" => json!({"type":kind,"text":string(part,"text")?}),
            "image_url" if !assistant => {
                let image = &part["image_url"];
                let mut item = json!({"type":"input_image","image_url":string(image,"url")?});
                if let Some(detail) = image.get("detail") {
                    item["detail"] = detail.clone();
                }
                item
            }
            "file" if !assistant => {
                let file = part["file"]
                    .as_object()
                    .ok_or_else(|| anyhow!("file must be an object"))?;
                let mut item = json!({"type":"input_file"});
                for (key, value) in file {
                    ensure!(
                        ["file_id", "file_data", "filename"].contains(&key.as_str()),
                        "Unsupported file field {key}"
                    );
                    item[key] = value.clone();
                }
                item
            }
            "refusal" if assistant => json!({"type":"refusal","refusal":string(part,"refusal")?}),
            kind => bail!("Unsupported chat content type {kind}"),
        });
    }
    Ok(out)
}
fn visible_items(message: &Value) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut parts = content(&message["content"], true)?;
    if let Some(refusal) = message.get("refusal").filter(|v| !v.is_null()) {
        ensure!(refusal.is_string(), "refusal must be a string");
        parts.push(json!({"type":"refusal","refusal":refusal}));
    }
    if !parts.is_empty() {
        out.push(json!({"type":"message","role":"assistant","content":parts}));
    }
    if let Some(calls) = message.get("tool_calls").filter(|v| !v.is_null()) {
        for call in calls
            .as_array()
            .ok_or_else(|| anyhow!("tool_calls must be an array"))?
        {
            ensure!(
                call["type"] == "function",
                "Only function tool calls are supported"
            );
            out.push(json!({"type":"function_call","call_id":string(call,"id")?,"name":string(&call["function"],"name")?,"arguments":string(&call["function"],"arguments")?}));
        }
    }
    Ok(out)
}
fn assistant_items(message: &Value, model: &str, codec: &ReasoningCodec) -> Result<Vec<Value>> {
    let visible = visible_items(message)?;
    let Some(details) = message.get("reasoning_details").filter(|v| !v.is_null()) else {
        return Ok(visible);
    };
    let mut reasoning = Vec::new();
    let mut gemini_items = None;
    // Details are indexed so clients may either concatenate deltas or merge each
    // detail by index. Coalesce summaries and opaque data without changing bytes.
    let mut groups: Vec<Value> = Vec::new();
    for detail in details
        .as_array()
        .ok_or_else(|| anyhow!("reasoning_details must be an array"))?
    {
        let kind = string(detail, "type")?;
        let format = detail
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("openai-responses-v1");
        ensure!(
            ["openai-responses-v1", "google-gemini-v1"].contains(&format),
            "Unsupported reasoning format {format}; preserve details from this proxy"
        );
        let id = detail
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Reasoning details require an id for replay"))?;
        let position = groups
            .iter()
            .position(|item| item["id"] == id)
            .unwrap_or_else(|| {
                groups.push(json!({"type":"reasoning","id":id,"summary":[]}));
                groups.len() - 1
            });
        let item = &mut groups[position];
        match kind {
            "reasoning.summary" | "reasoning.text" => {
                ensure!(
                    detail.get("signature").is_none_or(Value::is_null),
                    "Signed reasoning text cannot be converted; use opaque reasoning.encrypted details"
                );
                let text = string(
                    detail,
                    if kind == "reasoning.summary" {
                        "summary"
                    } else {
                        "text"
                    },
                )?;
                let field = if kind == "reasoning.summary" {
                    "summary"
                } else {
                    "content"
                };
                let part_type = if kind == "reasoning.summary" {
                    "summary_text"
                } else {
                    "reasoning_text"
                };
                if item
                    .get(field)
                    .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
                {
                    item[field] = json!([{"type":part_type,"text":""}]);
                }
                let previous = item[field][0]["text"].as_str().unwrap().to_owned();
                item[field][0]["text"] = json!(previous + text);
            }
            "reasoning.encrypted" => {
                let data = string(detail, "data")?;
                if format == "google-gemini-v1" {
                    ensure!(
                        gemini_items.is_none(),
                        "Only one Gemini reasoning carrier per assistant turn is supported"
                    );
                    let model = model.strip_prefix("gemini/").ok_or_else(|| {
                        anyhow!("Gemini reasoning cannot be replayed to another provider")
                    })?;
                    gemini_items = Some(
                        codec.replay_items(model.strip_prefix("models/").unwrap_or(model), data)?,
                    );
                }
                let previous = item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                item["encrypted_content"] = json!(previous.to_owned() + data);
            }
            _ => bail!("Unsupported reasoning detail type {kind}"),
        }
    }
    reasoning.extend(groups);
    if let Some(items) = gemini_items {
        // Gemini signs output boundaries. Restore them after checking that the
        // flattened content and tool calls are still the ones the model returned.
        let expected = response::message(&items, true)?;
        let actual = response::message(&visible, true)?;
        for field in ["content", "tool_calls", "refusal"] {
            ensure!(
                expected.get(field) == actual.get(field),
                "Altered Gemini assistant {field}; preserve the original message and reasoning_details"
            );
        }
        reasoning.extend(items);
    } else {
        reasoning.extend(visible);
    }
    Ok(reasoning)
}

pub(super) fn convert(
    input: &Value,
    routed_model: &str,
    codec: &ReasoningCodec,
) -> Result<Converted> {
    let object = input
        .as_object()
        .ok_or_else(|| anyhow!("Chat request must be an object"))?;
    ensure!(
        !string(input, "model")?.is_empty(),
        "model must not be empty"
    );
    let messages = input["messages"]
        .as_array()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("messages must be a nonempty array"))?;
    let mut items = Vec::new();
    for message in messages {
        let role = string(message, "role")?;
        for key in message.as_object().unwrap().keys() {
            ensure!(
                [
                    "role",
                    "content",
                    "name",
                    "tool_calls",
                    "tool_call_id",
                    "refusal",
                    "reasoning",
                    "reasoning_content",
                    "annotations",
                    "reasoning_details"
                ]
                .contains(&key.as_str()),
                "Unsupported chat message field {key}"
            );
        }
        match role {
            "assistant" => items.extend(assistant_items(message, routed_model, codec)?),
            "tool" => {
                let output = if let Some(text) = message["content"].as_str() {
                    json!(text)
                } else {
                    json!(content(&message["content"], false)?)
                };
                items.push(json!({"type":"function_call_output","call_id":string(message,"tool_call_id")?,"output":output}));
            }
            "system" | "developer" | "user" => {
                ensure!(
                    !message["content"].is_null(),
                    "{role} message requires content"
                );
                items.push(json!({"role":role,"content":content(&message["content"],false)?}));
            }
            _ => bail!("Unsupported chat role {role}"),
        }
    }
    let mut body = json!({"model":input["model"],"input":items,"include":["reasoning.encrypted_content"],"store":false});
    for (key, value) in object {
        match key.as_str() {
            "model"
            | "messages"
            | "stream_options"
            | "reasoning_effort"
            | "reasoning"
            | "include_reasoning"
            | "max_tokens"
            | "max_completion_tokens"
            | "response_format"
            | "tools"
            | "tool_choice" => {}
            "stream"
            | "temperature"
            | "top_p"
            | "parallel_tool_calls"
            | "metadata"
            | "store"
            | "user"
            | "service_tier"
            | "safety_identifier"
            | "prompt_cache_key"
            | "prompt_cache_retention"
            | "gemini" => {
                if !value.is_null() {
                    body[key] = value.clone();
                }
            }
            "n" => ensure!(
                value.is_null() || value.as_u64() == Some(1),
                "Custom Chat Completions supports n=1 only"
            ),
            "frequency_penalty" | "presence_penalty" => ensure!(
                value.is_null() || value.as_f64() == Some(0.0),
                "{key} has no Responses equivalent"
            ),
            "logprobs" => ensure!(
                value.is_null() || value == false,
                "logprobs is not supported by the chat shim"
            ),
            "stop" => ensure!(
                value.is_null() || value.as_array().is_some_and(Vec::is_empty),
                "stop sequences have no Responses equivalent"
            ),
            _ => {
                ensure!(value.is_null(), "Unsupported chat parameter {key}");
            }
        }
    }
    let stream = input
        .get("stream")
        .filter(|v| !v.is_null())
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| anyhow!("stream must be a boolean"))
        })
        .transpose()?
        .unwrap_or(false);
    let options = input.get("stream_options").filter(|v| !v.is_null());
    if let Some(options) = options {
        for key in options
            .as_object()
            .ok_or_else(|| anyhow!("stream_options must be an object"))?
            .keys()
        {
            ensure!(
                key == "include_usage",
                "Unsupported stream_options field {key}"
            );
        }
        ensure!(
            options.get("include_usage").is_none_or(Value::is_boolean),
            "include_usage must be a boolean"
        );
    }
    let include_usage = options.is_some_and(|v| v["include_usage"] == true);
    if let Some(tokens) = input
        .get("max_completion_tokens")
        .filter(|v| !v.is_null())
        .or_else(|| input.get("max_tokens").filter(|v| !v.is_null()))
    {
        ensure!(
            tokens.as_u64().is_some_and(|n| n > 0),
            "token limit must be a positive integer"
        );
        body["max_output_tokens"] = tokens.clone();
    }
    let mut reasoning = json!({});
    let mut exclude_reasoning = input["include_reasoning"] == false;
    if let Some(value) = input.get("reasoning").filter(|v| !v.is_null()) {
        for (key, value) in value
            .as_object()
            .ok_or_else(|| anyhow!("reasoning must be an object"))?
        {
            match key.as_str() {
                "effort" | "summary" => reasoning[key] = value.clone(),
                "enabled" => {
                    ensure!(value.is_boolean(), "reasoning.enabled must be boolean");
                    reasoning["effort"] = json!(if value == false { "none" } else { "medium" });
                }
                "exclude" => {
                    ensure!(value.is_boolean(), "reasoning.exclude must be boolean");
                    exclude_reasoning = value == true;
                }
                _ => bail!(
                    "Unsupported reasoning option {key}; use effort to select a reasoning budget"
                ),
            }
        }
    }
    if let Some(effort) = input.get("reasoning_effort").filter(|v| !v.is_null()) {
        ensure!(effort.is_string(), "reasoning_effort must be a string");
        reasoning["effort"] = effort.clone();
    }
    if !exclude_reasoning
        && reasoning["effort"] != "none"
        && (input["include_reasoning"] == true
            || input.get("reasoning").is_some_and(Value::is_object)
            || !reasoning.as_object().unwrap().is_empty())
        && reasoning.get("summary").is_none()
    {
        reasoning["summary"] = json!("auto");
    }
    if !reasoning.as_object().unwrap().is_empty() {
        body["reasoning"] = reasoning;
    }
    if let Some(format) = input.get("response_format").filter(|v| !v.is_null()) {
        body["text"] = match string(format, "type")? {
            "text" | "json_object" => json!({"format":format}),
            "json_schema" => {
                let mut schema = format["json_schema"]
                    .as_object()
                    .ok_or_else(|| anyhow!("json_schema must be an object"))?
                    .clone();
                schema.insert("type".into(), json!("json_schema"));
                json!({"format":schema})
            }
            kind => bail!("Unsupported response_format {kind}"),
        };
    }
    if let Some(tools) = input.get("tools").filter(|v| !v.is_null()) {
        let mut converted = Vec::new();
        for tool in tools
            .as_array()
            .ok_or_else(|| anyhow!("tools must be an array"))?
        {
            ensure!(
                tool["type"] == "function",
                "Only function tools are supported"
            );
            let mut function = tool["function"]
                .as_object()
                .ok_or_else(|| anyhow!("tool.function must be an object"))?
                .clone();
            function.insert("type".into(), json!("function"));
            // Chat's default schemas are non-strict; Responses otherwise normalizes
            // them to strict schemas and can change optional argument behavior.
            function.entry("strict").or_insert(json!(false));
            converted.push(json!(function));
        }
        body["tools"] = json!(converted);
    }
    if let Some(choice) = input.get("tool_choice").filter(|v| !v.is_null()) {
        body["tool_choice"] = if choice.is_string() {
            choice.clone()
        } else {
            ensure!(
                choice["type"] == "function",
                "Only function tool_choice is supported"
            );
            json!({"type":"function","name":string(&choice["function"],"name")?})
        };
    }
    Ok(Converted {
        body,
        stream,
        include_usage,
        exclude_reasoning,
    })
}
