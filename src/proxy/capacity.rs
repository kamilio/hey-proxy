use serde_json::{Value, json};

const LIMIT: usize = 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Prelude {
    Waiting,
    Started,
    Refused,
}

/// Only a definitive capacity refusal before output is safe to replay. Lifecycle
/// events are buffered so rejected response IDs never escape to the client.
pub(super) fn prelude_event(bytes: &[u8]) -> Prelude {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return Prelude::Started;
    };
    if ["/error", "/response/error"]
        .iter()
        .any(|p| value.pointer(p).is_some_and(permanent))
    {
        return Prelude::Started;
    }
    if value
        .pointer("/response/output")
        .and_then(Value::as_array)
        .is_some_and(|v| !v.is_empty())
        || value
            .pointer("/response/usage/output_tokens")
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0)
    {
        return Prelude::Started;
    }
    if event(bytes).is_some()
        || matches!(value["type"].as_str(), Some("error" | "response.failed"))
            && ["/error/code", "/response/error/code"]
                .iter()
                .any(|p| value.pointer(p).and_then(Value::as_str) == Some("rate_limit_exceeded"))
    {
        return Prelude::Refused;
    }
    if matches!(
        value["type"].as_str(),
        Some("response.created" | "response.in_progress")
    ) {
        Prelude::Waiting
    } else {
        Prelude::Started
    }
}

pub(super) fn prelude_sse(bytes: &[u8], complete: bool) -> Prelude {
    if bytes.len() >= LIMIT {
        return Prelude::Started;
    }
    let mut data = Vec::new();
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        if !line.ends_with(b"\n") && !complete {
            break;
        }
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            if !data.is_empty() {
                let result = prelude_event(&data);
                if result != Prelude::Waiting {
                    return result;
                }
                data.clear();
            }
        } else if let Some(part) = line.strip_prefix(b"data:") {
            if !data.is_empty() {
                data.push(b'\n');
            }
            data.extend_from_slice(part.strip_prefix(b" ").unwrap_or(part));
        }
    }
    if complete {
        if data.is_empty() {
            Prelude::Started
        } else {
            prelude_event(&data)
        }
    } else {
        Prelude::Waiting
    }
}

fn permanent(error: &Value) -> bool {
    ["code", "type"].iter().any(|key| {
        error[key]
            .as_str()
            .is_some_and(super::guidance::is_security_block)
            || matches!(
                error[key].as_str(),
                Some(
                    "insufficient_quota"
                        | "invalid_prompt"
                        | "billing_hard_limit_reached"
                        | "billing_not_active"
                        | "invalid_api_key"
                        | "credit_balance_exhausted"
                        | "organization_spend_limit_exceeded"
                        | "project_spend_limit_exceeded"
                )
            )
    })
}

fn is_capacity(error: &Value) -> bool {
    let code = error["code"].as_str().unwrap_or("");
    let kind = error["type"].as_str().unwrap_or("");
    if permanent(error)
        || code == "server_error"
            && error["message"]
                .as_str()
                .unwrap_or("")
                .starts_with("Upstream model is at capacity; hey-proxy translated")
    {
        return false;
    }
    [code, kind]
        .iter()
        .any(|v| matches!(*v, "server_is_overloaded" | "capacity_exceeded"))
        || capacity_message(error["message"].as_str().unwrap_or(""))
}

fn capacity_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "overloaded",
        "out of capacity",
        "at capacity",
        "capacity_exceeded",
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
}

fn rewrite_error(error: &mut Value) -> bool {
    if !error.is_object() || !is_capacity(error) {
        return false;
    }
    let original = error["message"].as_str().unwrap_or("").to_owned();
    if let Some(code) = error.get("code").cloned() {
        error["upstream_code"] = code;
    }
    error["code"] = json!("server_error");
    error["type"] = json!("server_error");
    error["message"] = json!(format!(
        "Upstream model is at capacity; hey-proxy translated this capacity failure to a retryable server error. Original upstream message: {}",
        if original.is_empty() {
            "(none provided)"
        } else {
            &original
        }
    ));
    true
}

/// Only error envelopes are changed; ordinary model output remains untouched.
pub(super) fn event(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(bytes).ok()?;
    let kind = value["type"].as_str().unwrap_or("").to_owned();
    let pointer = match kind.as_str() {
        "response.failed" => "/response/error",
        "error" => "/error",
        _ => return None,
    };
    if !rewrite_error(value.pointer_mut(pointer)?) {
        return None;
    }
    if kind == "error" {
        value["status"] = json!(500);
        for key in ["status", "status_code"] {
            if value.get(key).is_some() {
                value[key] = json!(500);
            }
        }
    }
    serde_json::to_vec(&value).ok()
}

pub(super) fn http(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(_) => {
            let message = std::str::from_utf8(bytes).ok()?;
            if !capacity_message(message) {
                return None;
            }
            json!({"error": {"message": message}})
        }
    };
    if !rewrite_error(value.get_mut("error")?) {
        return None;
    }
    serde_json::to_vec(&value).ok()
}

/// Buffer one SSE event, preserving all untouched bytes and bounding memory.
/// Oversized events pass through unchanged until their terminating blank line.
#[derive(Default)]
pub(super) struct Sse {
    buffer: Vec<u8>,
    overflow: bool,
    line_nonempty: bool,
}

impl Sse {
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        for &byte in bytes {
            if self.overflow {
                output.push(byte);
            } else {
                self.buffer.push(byte);
            }
            if byte == b'\n' {
                if !self.line_nonempty {
                    if !self.overflow {
                        output.extend(self.finish());
                    }
                    self.overflow = false;
                }
                self.line_nonempty = false;
            } else if byte != b'\r' {
                self.line_nonempty = true;
            }
            if self.buffer.len() >= LIMIT {
                output.append(&mut self.buffer);
                self.overflow = true;
            }
        }
        output
    }

    pub(super) fn finish(&mut self) -> Vec<u8> {
        let original = std::mem::take(&mut self.buffer);
        let mut data = Vec::new();
        for line in original.split(|b| *b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some(part) = line.strip_prefix(b"data:") {
                if !data.is_empty() {
                    data.push(b'\n');
                }
                data.extend_from_slice(part.strip_prefix(b" ").unwrap_or(part));
            }
        }
        let Some(rewritten) = event(&data) else {
            return original;
        };
        let mut output = Vec::new();
        let mut inserted = false;
        for line in original.split_inclusive(|b| *b == b'\n') {
            if line.starts_with(b"data:") {
                if !inserted {
                    output.extend_from_slice(b"data: ");
                    output.extend_from_slice(&rewritten);
                    if line.ends_with(b"\r\n") {
                        output.extend_from_slice(b"\r\n");
                    } else if line.ends_with(b"\n") {
                        output.push(b'\n');
                    }
                    inserted = true;
                }
            } else {
                output.extend_from_slice(line);
            }
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_envelopes_preserve_cause_and_become_server_errors() {
        for original in [
            json!({"type":"response.failed","response":{"id":"r1","error":{"code":"server_is_overloaded","message":"Selected model is at capacity"}}}),
            json!({"type":"error","status":503,"error":{"code":"server_is_overloaded","message":"busy"}}),
        ] {
            let rewritten: Value =
                serde_json::from_slice(&event(&serde_json::to_vec(&original).unwrap()).unwrap())
                    .unwrap();
            let error = rewritten
                .pointer("/response/error")
                .unwrap_or(&rewritten["error"]);
            assert_eq!(error["code"], "server_error");
            assert_eq!(error["upstream_code"], "server_is_overloaded");
            assert!(
                error["message"]
                    .as_str()
                    .unwrap()
                    .contains("Upstream model is at capacity")
            );
            if original.get("status").is_some() {
                assert_eq!(rewritten["status"], 500);
            }
            assert!(event(&serde_json::to_vec(&rewritten).unwrap()).is_none());
        }
        assert!(http(br#"{"error":{"message":"Model is at capacity"}}"#).is_some());
        assert!(http(b"Model is out of capacity").is_some());
        for code in [
            "insufficient_quota",
            "invalid_api_key",
            "billing_not_active",
        ] {
            assert!(
                http(
                    &serde_json::to_vec(&json!({"error":{"code":code,"message":"at capacity"}}))
                        .unwrap()
                )
                .is_none()
            );
        }
        assert!(event(br#"{"type":"response.output_text.delta","delta":"at capacity"}"#).is_none());
        assert!(
            http(br#"{"error":{"code":"rate_limit_exceeded","message":"Too many requests"}}"#)
                .is_none()
        );
    }

    #[test]
    fn sse_handles_split_multiline_events_and_preserves_other_events() {
        let input = b": keepalive\r\n\r\nevent: response.failed\r\ndata: {\r\ndata: \"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_is_overloaded\",\"message\":\"busy\"}}}\r\n\r\ndata: [DONE]\n\n";
        for size in [1, 7, input.len()] {
            let mut sse = Sse::default();
            let mut output = Vec::new();
            for chunk in input.chunks(size) {
                output.extend(sse.feed(chunk));
            }
            output.extend(sse.finish());
            let text = String::from_utf8(output).unwrap();
            assert!(text.starts_with(": keepalive\r\n\r\nevent: response.failed\r\n"));
            assert!(text.contains("\"code\":\"server_error\""));
            assert!(text.ends_with("data: [DONE]\n\n"));
        }
        let oversized = format!("data: {}\n\n", "x".repeat(LIMIT + 10));
        let mut sse = Sse::default();
        let mut output = sse.feed(oversized.as_bytes());
        output.extend(sse.finish());
        assert_eq!(output, oversized.as_bytes());
    }
}
