//! Google RPC errors expressed in the Responses error vocabulary. Native error
//! details remain available separately; classification never matches message text.
use serde_json::{Value, json};

pub struct ResponseError {
    pub error: Value,
    pub retryable: bool,
}

impl ResponseError {
    pub fn from_native(native: &Value, http_status: Option<u16>) -> Self {
        let status = native.pointer("/error/status").and_then(Value::as_str);
        let http_status = http_status.or_else(|| {
            native
                .pointer("/error/code")
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok())
        });
        let (kind, code, retryable) = match status {
            Some("RESOURCE_EXHAUSTED") => ("rate_limit_error", "rate_limit_exceeded", true),
            Some(
                "UNAVAILABLE" | "INTERNAL" | "DEADLINE_EXCEEDED" | "ABORTED" | "CANCELLED"
                | "UNKNOWN",
            ) => ("server_error", "server_error", true),
            // Codex recognizes invalid_prompt as a terminal stream error. Its
            // SSE parser retries unknown codes, including authentication_error.
            Some("UNAUTHENTICATED") => ("authentication_error", "invalid_prompt", false),
            Some("PERMISSION_DENIED") => ("permission_error", "invalid_prompt", false),
            Some(
                "INVALID_ARGUMENT"
                | "NOT_FOUND"
                | "FAILED_PRECONDITION"
                | "OUT_OF_RANGE"
                | "UNIMPLEMENTED"
                | "ALREADY_EXISTS",
            ) => ("invalid_request_error", "invalid_prompt", false),
            _ => match http_status {
                Some(429) => ("rate_limit_error", "rate_limit_exceeded", true),
                Some(400..=499) => ("invalid_request_error", "invalid_prompt", false),
                _ => ("server_error", "server_error", true),
            },
        };
        let mut message = native
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("Gemini request failed")
            .to_owned();
        let retry_seconds = native
            .pointer("/error/details")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|d| d["@type"] == "type.googleapis.com/google.rpc.RetryInfo")
            .and_then(|d| d["retryDelay"].as_str())
            .and_then(|d| d.strip_suffix('s'))
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s >= 0.0 && *s <= 86400.0);
        if code == "rate_limit_exceeded"
            && let Some(seconds) = retry_seconds
        {
            // Codex's Responses SSE reader takes its delay from this wording.
            message.push_str(&format!(" Please try again in {seconds}s."));
        }
        Self {
            error: json!({"type":kind,"code":code,"message":message,"param":null,
            "gemini_status":status,"gemini_http_status":http_status}),
            retryable,
        }
    }
}
