use super::*;
use anyhow::{anyhow, bail, ensure};

pub(super) fn clean_headers(headers: &mut HeaderMap) {
    for key in [
        "content-length",
        "content-encoding",
        "etag",
        "content-md5",
        "digest",
    ] {
        headers.remove(key);
    }
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
}
pub(super) fn usage(value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    json!({"prompt_tokens":value["input_tokens"],"completion_tokens":value["output_tokens"],
        "total_tokens":value["total_tokens"],"prompt_tokens_details":value["input_tokens_details"],
        "completion_tokens_details":value["output_tokens_details"]})
}
pub(super) fn reasoning_details(item: &Value, index: usize) -> Vec<Value> {
    let mut details = Vec::new();
    let data = item.get("encrypted_content").and_then(Value::as_str);
    let format = if data.is_some_and(|s| s.starts_with("hey_gemini_v1.")) {
        "google-gemini-v1"
    } else {
        "openai-responses-v1"
    };
    let summary: String = item["summary"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["text"].as_str())
        .collect();
    if !summary.is_empty() {
        details.push(json!({"type":"reasoning.summary","summary":summary,"id":item["id"],"format":format,"index":index*3}));
    }
    let text: String = item["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["text"].as_str())
        .collect();
    if !text.is_empty() {
        details.push(json!({"type":"reasoning.text","text":text,"signature":null,"id":item["id"],"format":format,"index":index*3+1}));
    }
    if let Some(data) = data {
        details.push(json!({"type":"reasoning.encrypted","data":data,"id":item["id"],"format":format,"index":index*3+2}));
    }
    details
}
pub(super) fn message(output: &[Value], exclude_reasoning: bool) -> Result<Value> {
    let mut text = String::new();
    let mut refusal = String::new();
    let mut calls = Vec::new();
    let mut details = Vec::new();
    let mut annotations = Vec::new();
    for (index, item) in output.iter().enumerate() {
        match item["type"].as_str() {
            Some("message") => {
                for part in item["content"].as_array().ok_or_else(|| anyhow!("Response message lacks content"))? {
                    match part["type"].as_str() {
                        Some("output_text" | "input_text") => {
                            text.push_str(part["text"].as_str().ok_or_else(|| anyhow!("Response text must be a string"))?);
                            if let Some(values) = part["annotations"].as_array() { annotations.extend(values.iter().cloned()); }
                        }
                        Some("refusal") => refusal.push_str(part["refusal"].as_str().unwrap_or("")),
                        _ => bail!("Response content cannot be represented as a chat message"),
                    }
                }
            }
            Some("function_call") => calls.push(json!({"id":item["call_id"],"type":"function","function":{"name":item["name"],"arguments":item["arguments"]}})),
            Some("reasoning") if !exclude_reasoning => details.extend(reasoning_details(item,index)),
            Some("reasoning") => {},
            _ => bail!("Response output type cannot be represented by the chat shim"),
        }
    }
    let mut result = json!({"role":"assistant","content":if text.is_empty() {Value::Null} else {json!(text)},"refusal":if refusal.is_empty() {Value::Null} else {json!(refusal)}});
    if !calls.is_empty() {
        result["tool_calls"] = json!(calls);
    }
    if !annotations.is_empty() {
        result["annotations"] = json!(annotations);
    }
    if !details.is_empty() {
        let reasoning: String = details
            .iter()
            .filter_map(|d| d["summary"].as_str().or_else(|| d["text"].as_str()))
            .collect();
        if !reasoning.is_empty() {
            result["reasoning"] = json!(reasoning);
        }
        result["reasoning_details"] = json!(details);
    }
    Ok(result)
}
pub(super) fn finish_reason(value: &Value) -> Result<&'static str> {
    match value["status"].as_str() {
        Some("incomplete") => match value
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") => Ok("length"),
            Some("content_filter") => Ok("content_filter"),
            _ => bail!("Response ended incomplete without a supported finish reason"),
        },
        Some("completed") => Ok(
            if value["output"]
                .as_array()
                .is_some_and(|items| items.iter().any(|i| i["type"] == "function_call"))
            {
                "tool_calls"
            } else {
                "stop"
            },
        ),
        _ => bail!("Response did not complete"),
    }
}
pub(super) fn completion(value: &Value, model: &str, exclude_reasoning: bool) -> Result<Value> {
    let output = value["output"]
        .as_array()
        .ok_or_else(|| anyhow!("Response lacks output"))?;
    ensure!(value["id"].is_string(), "Response lacks id");
    Ok(
        json!({"id":format!("chatcmpl-{}",value["id"].as_str().unwrap()),"object":"chat.completion",
        "created":value["created_at"],"model":model,
        "choices":[{"index":0,"message":message(output,exclude_reasoning)?,"finish_reason":finish_reason(value)?,"logprobs":null}],
        "usage":usage(&value["usage"])}),
    )
}
