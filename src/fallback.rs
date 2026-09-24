//! Ordered model fallbacks and conservative, provider-independent failure policy.
//! No HTTP client, credentials, sleeps, or global state live in this module.
use anyhow::{Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

pub type Fallbacks = BTreeMap<String, Vec<String>>;
pub const MAX_ATTEMPTS: usize = 16;

/// Config keys identify the rewritten upstream model; openai/ is optional.
pub fn canonical(model: &str) -> &str {
    model.strip_prefix("openai/").unwrap_or(model)
}
pub fn targets<'a>(fallbacks: &'a Fallbacks, model: &str) -> &'a [String] {
    let model = canonical(model);
    fallbacks
        .get(model)
        .or_else(|| fallbacks.get(&format!("openai/{model}")))
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
pub fn validate(fallbacks: &Fallbacks) -> Result<()> {
    if fallbacks.len() > 1024 {
        bail!("At most 1024 fallback rules are supported");
    }
    let mut sources = HashSet::new();
    let mut depths = BTreeMap::new();
    for (source, next) in fallbacks {
        if !valid_name(source) || !sources.insert(canonical(source)) {
            bail!("Fallback sources must be valid unique upstream model names");
        }
        if next.len() >= MAX_ATTEMPTS {
            bail!("At most 15 fallback targets per rule");
        }
        let mut seen = HashSet::new();
        for target in next {
            if !valid_name(target) || !seen.insert(canonical(target)) {
                bail!("Fallback targets must be valid unique model names");
            }
        }
        visit(fallbacks, source, &mut HashSet::new(), &mut depths)?;
    }
    Ok(())
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !canonical(name).is_empty()
        && name.len() <= 256
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}
fn visit<'a>(
    fallbacks: &'a Fallbacks,
    name: &'a str,
    active: &mut HashSet<&'a str>,
    depths: &mut BTreeMap<&'a str, usize>,
) -> Result<usize> {
    let name = canonical(name);
    if let Some(depth) = depths.get(name) {
        return Ok(*depth);
    }
    if !active.insert(name) {
        bail!("Fallback cycle at {name}");
    }
    if active.len() > MAX_ATTEMPTS {
        bail!("Fallback chain exceeds 16 models");
    }
    let mut depth = 1;
    for next in targets(fallbacks, name) {
        depth = depth.max(1 + visit(fallbacks, next, active, depths)?);
    }
    if depth > MAX_ATTEMPTS {
        bail!("Fallback chain exceeds 16 models");
    }
    active.remove(name);
    depths.insert(name, depth);
    Ok(depth)
}

/// Stateful continuation and hosted tools cannot safely be replayed on another
/// deployment after an ambiguous failure. Never drop these fields to enable fallback.
pub fn replay_blocker(request: &Value) -> Option<&'static str> {
    for key in ["previous_response_id", "conversation"] {
        if request.get(key).is_some_and(|v| !v.is_null()) {
            return Some("server_side_conversation");
        }
    }
    if request["background"] == true {
        return Some("background_request");
    }
    fn local_tool(tool: &Value) -> bool {
        match tool["type"].as_str() {
            Some("function" | "custom") => true,
            Some("namespace") => tool["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().all(local_tool)),
            _ => false,
        }
    }
    if request["tools"]
        .as_array()
        .is_some_and(|tools| !tools.iter().all(local_tool))
    {
        return Some("hosted_tools");
    }
    None
}

fn permanent(error: &Value) -> bool {
    ["code", "type", "gemini_status"]
        .iter()
        .filter_map(|key| error[*key].as_str())
        .any(|code| {
            matches!(
                code,
                "insufficient_quota"
                    | "invalid_prompt"
                    | "invalid_request_error"
                    | "billing_hard_limit_reached"
                    | "billing_not_active"
                    | "invalid_api_key"
                    | "authentication_error"
                    | "permission_error"
                    | "credit_balance_exhausted"
                    | "organization_spend_limit_exceeded"
                    | "project_spend_limit_exceeded"
                    | "context_length_exceeded"
                    | "content_policy_violation"
                    | "cyber_policy"
                    | "bio_policy"
                    | "misalignment_policy_violation"
                    | "content_filter"
                    | "UNAUTHENTICATED"
                    | "PERMISSION_DENIED"
                    | "INVALID_ARGUMENT"
                    | "NOT_FOUND"
                    | "FAILED_PRECONDITION"
                    | "OUT_OF_RANGE"
                    | "UNIMPLEMENTED"
                    | "ALREADY_EXISTS"
            ) || code.to_ascii_lowercase().contains("security")
                || code.to_ascii_lowercase().contains("safety")
        })
}

/// Explicit transient HTTP errors only; auth, request and policy failures win over status.
pub fn transient_http(status: u16, body: &[u8]) -> bool {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    !has_output(&value)
        && !["/error", "/response/error"]
            .iter()
            .any(|p| value.pointer(p).is_some_and(permanent))
        && matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

fn has_output(value: &Value) -> bool {
    ["/response/output", "/output"].iter().any(|p| {
        value
            .pointer(p)
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty())
    }) || ["/response/usage/output_tokens", "/usage/output_tokens"]
        .iter()
        .any(|p| {
            value
                .pointer(p)
                .and_then(Value::as_u64)
                .is_some_and(|n| n > 0)
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prelude {
    Waiting,
    Committed,
    Refused,
}

/// Unknown events, output, reasoning and tool calls commit the selected model.
/// Gemini transport/conversion errors are ambiguous; only native RPC refusals switch.
pub fn prelude_event(bytes: &[u8], gemini: bool) -> Prelude {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return Prelude::Committed;
    };
    if has_output(&value) {
        return Prelude::Committed;
    }
    let error = value
        .pointer("/response/error")
        .or_else(|| value.get("error"));
    if let Some(error) = error.filter(|error| !error.is_null()) {
        if permanent(error) {
            return Prelude::Committed;
        }
        let transient = if gemini {
            matches!(
                error["gemini_status"].as_str(),
                Some(
                    "RESOURCE_EXHAUSTED"
                        | "UNAVAILABLE"
                        | "INTERNAL"
                        | "DEADLINE_EXCEEDED"
                        | "ABORTED"
                        | "UNKNOWN"
                )
            )
        } else {
            matches!(
                error["code"].as_str(),
                Some(
                    "server_error"
                        | "server_is_overloaded"
                        | "capacity_exceeded"
                        | "rate_limit_exceeded"
                        | "request_timeout"
                )
            )
        };
        if transient
            && (matches!(value["type"].as_str(), Some("response.failed" | "error"))
                || value.get("type").is_none())
        {
            return Prelude::Refused;
        }
        return Prelude::Committed;
    }
    if matches!(
        value["type"].as_str(),
        Some("response.created" | "response.in_progress")
    ) {
        return Prelude::Waiting;
    }
    Prelude::Committed
}

/// Incremental SSE scanner: linear in received bytes, bounded independently of chunking.
/// The caller retains chunks only while Waiting, up to MAX_PRELUDE bytes.
pub const MAX_PRELUDE: usize = 256 * 1024;
pub struct SsePrelude {
    line: Vec<u8>,
    data: Vec<u8>,
    total: usize,
    gemini: bool,
    decision: Prelude,
}
impl SsePrelude {
    pub fn new(gemini: bool) -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            total: 0,
            gemini,
            decision: Prelude::Waiting,
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Prelude {
        for &byte in bytes {
            if self.decision != Prelude::Waiting {
                break;
            }
            self.total += 1;
            if self.total >= MAX_PRELUDE {
                self.decision = Prelude::Committed;
                break;
            }
            if byte != b'\n' {
                self.line.push(byte);
                continue;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            if self.line.is_empty() {
                if !self.data.is_empty() {
                    self.decision = prelude_event(&self.data, self.gemini);
                    self.data.clear();
                }
            } else if let Some(data) = self.line.strip_prefix(b"data:") {
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data
                    .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
            }
            self.line.clear();
        }
        self.decision
    }
}
