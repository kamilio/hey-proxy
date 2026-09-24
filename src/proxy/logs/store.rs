use super::super::*;
use super::database::{Database, Event};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

pub const RECENT_LIMIT: usize = 500;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Entry {
    pub id: u64,
    pub request_id: String,
    pub session_id: String,
    pub timestamp_ms: u64,
    pub updated_ms: u64,
    pub ended_ms: Option<u64>,
    pub method: String,
    pub path: String,
    pub transport: String,
    pub mode: String,
    pub requested_model: Option<String>,
    pub routed_model: Option<String>,
    pub requested_reasoning: Option<String>,
    pub routed_reasoning: Option<String>,
    pub route_rule: Option<String>,
    pub project: Option<String>,
    pub status: Option<u16>,
    pub state: String,
    pub outcome_source: Option<String>,
    pub error_code: Option<String>,
    pub upstream_request_id: Option<String>,
    pub response_id: Option<String>,
    pub retries: u32,
    pub attempts: u32,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub duration_ms: Option<u64>,
    pub total_duration_ms: Option<u64>,
    pub first_byte_ms: Option<u64>,
    pub first_output_ms: Option<u64>,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub streaming: bool,
    #[serde(default)]
    pub security_guidance: bool,
    #[serde(skip)]
    pub started: Option<Instant>,
    #[serde(skip)]
    pub terminal: Option<String>,
}

impl Entry {
    pub fn is_final(&self) -> bool {
        self.ended_ms.is_some()
    }
    pub fn elapsed(&self) -> u64 {
        self.started
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or_else(|| now_ms().saturating_sub(self.timestamp_ms))
    }
}

#[derive(Default)]
struct Cache {
    next_id: u64,
    entries: HashMap<u64, Entry>,
    order: VecDeque<u64>,
}

pub struct Store {
    cache: Mutex<Cache>,
    pub(super) remote: Mutex<std::collections::BTreeMap<Option<u64>, super::RemoteCache>>,
    pub(super) window: Mutex<super::WindowCache>,
    pub database: Option<Arc<Database>>,
    session_id: String,
    mode: String,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            cache: Mutex::default(),
            remote: Mutex::default(),
            window: Mutex::default(),
            database: None,
            session_id: format!("{:032x}", rand::random::<u128>()),
            mode: "standalone".into(),
        }
    }
}

impl Store {
    pub fn open(config: &Config, config_path: &std::path::Path) -> anyhow::Result<Self> {
        let mut store = Self {
            mode: serde_json::to_value(config.mode)?.as_str().unwrap().into(),
            ..Self::default()
        };
        if config.logging.enabled {
            let directory = config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let path = config
                .logging
                .database
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("requests.sqlite3"));
            let path = if path.is_absolute() {
                path
            } else {
                directory.join(path)
            };
            store.database = Some(Arc::new(Database::open(
                path,
                &config.logging,
                &store.session_id,
            )?));
        }
        Ok(store)
    }
    pub fn begin(&self, method: &str, path: &str, transport: &str) -> u64 {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.next_id += 1;
        let id = cache.next_id;
        if cache.order.len() == RECENT_LIMIT
            && let Some(old) = cache.order.pop_front()
            && cache.entries.get(&old).is_some_and(Entry::is_final)
        {
            cache.entries.remove(&old);
        }
        let timestamp = now_ms();
        let entry = Entry {
            id,
            request_id: format!("{}-{id}", self.session_id),
            session_id: self.session_id.clone(),
            timestamp_ms: timestamp,
            updated_ms: timestamp,
            method: method.into(),
            path: path.chars().take(2048).collect(),
            transport: transport.into(),
            mode: self.mode.clone(),
            state: "pending".into(),
            started: Some(Instant::now()),
            ..Entry::default()
        };
        self.persist(&entry, "received", json!({}));
        cache.entries.insert(id, entry);
        cache.order.push_back(id);
        id
    }
    pub fn recent(&self) -> Vec<Entry> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .order
            .iter()
            .rev()
            .filter_map(|id| cache.entries.get(id).cloned())
            .collect()
    }
    fn persist(&self, entry: &Entry, kind: &str, mut details: Value) {
        if kind == "attempt_started" {
            details["attempt"] = json!(entry.attempts);
        }
        if kind == "usage" {
            details = json!({"input_tokens":entry.input_tokens,"output_tokens":entry.output_tokens,"cached_input_tokens":entry.cached_input_tokens,"cache_write_tokens":entry.cache_write_tokens,"reasoning_tokens":entry.reasoning_tokens});
        }
        if kind == "routed" {
            details = json!({"requested_model":entry.requested_model,"routed_model":entry.routed_model,"project":entry.project});
        }
        if kind == "finished" {
            details["state"] = json!(entry.state);
            details["error_code"] = json!(entry.error_code);
            details["source"] = json!(entry.outcome_source);
        }
        if kind == "response_event" {
            details["error_code"] = json!(entry.error_code);
        }
        if let Some(database) = &self.database {
            database.enqueue(Event {
                entry: entry.clone(),
                kind: kind.into(),
                details,
                timestamp_ms: now_ms(),
            });
        }
    }
    pub fn update(
        &self,
        id: u64,
        kind: &str,
        details: Value,
        change: impl FnOnce(&mut Entry) -> bool,
    ) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.entries.get_mut(&id)
            && change(entry)
        {
            for value in [
                &mut entry.requested_model,
                &mut entry.routed_model,
                &mut entry.requested_reasoning,
                &mut entry.routed_reasoning,
                &mut entry.route_rule,
                &mut entry.project,
                &mut entry.upstream_request_id,
                &mut entry.response_id,
                &mut entry.error_code,
            ]
            .into_iter()
            .flatten()
            {
                if value.len() > 256 {
                    *value = value.chars().take(256).collect();
                }
            }
            entry.updated_ms = now_ms();
            self.persist(entry, kind, details);
            if entry.is_final() && id <= cache.next_id.saturating_sub(RECENT_LIMIT as u64) {
                cache.entries.remove(&id);
            }
        }
    }
    pub fn route(
        &self,
        id: u64,
        incoming: Option<String>,
        outgoing: Option<String>,
        project: &str,
    ) {
        self.update(id, "routed", json!({}), |entry| {
            entry.requested_model = incoming;
            entry.routed_model = outgoing;
            entry.project = Some(project.into());
            true
        });
    }
    /// Record only the routing inputs, never retain a body or nested user content.
    pub fn routing_decision(&self, id: u64, config: &Config, path: &str, body: &[u8]) {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return;
        };
        let envelope = if value.get("model").and_then(Value::as_str).is_some() {
            &value
        } else if value.pointer("/response/model").is_some() {
            &value["response"]
        } else if value.pointer("/session/model").is_some() {
            &value["session"]
        } else {
            &value
        };
        let effort = requested_effort(path, envelope);
        let alias = envelope
            .get("model")
            .and_then(Value::as_str)
            .and_then(|model| config.aliases.iter().find(|a| a.from == model));
        let matched =
            alias.is_some_and(|a| effort.is_some_and(|e| a.reasoning_routes.contains_key(e)));
        let rule = if matched {
            "reasoning"
        } else if alias.is_some() {
            "alias"
        } else if envelope.get("model").is_some() {
            "passthrough"
        } else {
            "inherited"
        };
        let outgoing = if path.trim_end_matches('/').ends_with("/responses")
            || path.trim_end_matches('/').ends_with("/chat/completions")
        {
            alias.and_then(|a| a.reasoning.as_deref()).or(effort)
        } else {
            effort
        };
        let bounded = |v: Option<&str>| v.map(|s| s.chars().take(256).collect::<String>());
        let incoming = bounded(effort);
        let outgoing = bounded(outgoing);
        self.update(
            id,
            "routing_decision",
            json!({"requested_reasoning":incoming,"routed_reasoning":outgoing,"route_rule":rule}),
            |entry| {
                entry.requested_reasoning = incoming;
                entry.routed_reasoning = outgoing;
                entry.route_rule = Some(rule.into());
                true
            },
        );
    }
    pub fn usage(&self, id: u64, value: &Value) {
        let usage = value
            .pointer("/usage")
            .or_else(|| value.pointer("/response/usage"));
        let Some(usage) = usage else { return };
        let input = usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))
            .and_then(Value::as_u64);
        let output = usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))
            .and_then(Value::as_u64);
        if input.is_none() && output.is_none() {
            return;
        }
        let details = usage
            .get("input_tokens_details")
            .or_else(|| usage.get("prompt_tokens_details"));
        let cached = details
            .and_then(|v| v.get("cached_tokens"))
            .and_then(Value::as_u64);
        let writes = details
            .and_then(|v| v.get("cache_write_tokens"))
            .and_then(Value::as_u64);
        let reasoning = usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .or_else(|| usage.pointer("/completion_tokens_details/reasoning_tokens"))
            .and_then(Value::as_u64);
        self.update(id, "usage", json!({}), |entry| {
            let before = (
                entry.input_tokens,
                entry.output_tokens,
                entry.cached_input_tokens,
                entry.cache_write_tokens,
                entry.reasoning_tokens,
            );
            if input.is_some() {
                entry.input_tokens = input;
                if cached.is_some() {
                    entry.cached_input_tokens = cached;
                }
                if writes.is_some() {
                    entry.cache_write_tokens = writes;
                }
            }
            if output.is_some() {
                entry.output_tokens = output;
            }
            if reasoning.is_some() {
                entry.reasoning_tokens = reasoning;
            }
            before
                != (
                    entry.input_tokens,
                    entry.output_tokens,
                    entry.cached_input_tokens,
                    entry.cache_write_tokens,
                    entry.reasoning_tokens,
                )
        });
    }
    pub fn retries(&self, id: u64, count: u32) {
        self.update(id, "retry", json!({"retry":count}), |entry| {
            if entry.retries == count {
                return false;
            }
            entry.retries = count;
            true
        });
    }
    /// HTTP headers or the first WebSocket send. Completion is recorded separately.
    pub fn finish(&self, id: u64, status: u16) {
        self.update(id, "headers", json!({"status":status}), |entry| {
            entry.status = Some(status);
            let elapsed = entry.elapsed();
            entry.duration_ms.get_or_insert(elapsed);
            if !entry.is_final() {
                entry.state = "streaming".into();
            }
            true
        });
    }
    pub fn complete(&self, id: u64, outcome: &str, source: &str, code: Option<&str>, bytes: u64) {
        self.update(id, "finished", json!({"source":source}), |entry| {
            if entry.is_final() {
                return false;
            }
            let missing_completed = source == "stream_end"
                && entry.streaming
                && entry.path.trim_end_matches('/').ends_with("/responses")
                && entry.terminal.is_none();
            // Codex closes SSE after response.completed, without waiting for HTTP EOF.
            let completed_disconnect = source == "client_disconnect"
                && entry.streaming
                && entry.terminal.as_deref() == Some("succeeded")
                && entry.status.is_none_or(|status| status < 400);
            entry.state = if entry.terminal.as_deref() == Some("failed")
                || missing_completed
                || (outcome == "succeeded" && entry.status.is_some_and(|s| s >= 400))
            {
                "failed".into()
            } else if completed_disconnect {
                "succeeded".into()
            } else {
                outcome.into()
            };
            entry.outcome_source = Some(
                if completed_disconnect {
                    "responses_event"
                } else {
                    source
                }
                .into(),
            );
            if !completed_disconnect
                && entry.error_code.is_none()
                && let Some(code) = code
            {
                entry.error_code = Some(code.chars().take(128).collect());
            }
            if missing_completed {
                entry.error_code = Some("stream_missing_completed".into());
            }
            entry.ended_ms = Some(now_ms());
            entry.total_duration_ms = Some(entry.elapsed());
            entry.response_bytes = bytes;
            true
        });
    }
    pub fn observe(&self, id: u64, value: &Value) {
        self.usage(id, value);
        let kind = value["type"].as_str().unwrap_or("");
        let has_error = value.get("error").is_some_and(|v| !v.is_null())
            || value
                .pointer("/response/error")
                .is_some_and(|v| !v.is_null());
        let response_status = value
            .get("status")
            .or_else(|| value.pointer("/response/status"))
            .and_then(Value::as_str);
        if !matches!(
            kind,
            "response.created"
                | "response.completed"
                | "response.failed"
                | "response.incomplete"
                | "error"
                | "proxy.stream.done"
        ) && !has_error
            && !matches!(response_status, Some("completed" | "failed" | "incomplete"))
        {
            return;
        }
        self.update(id, "response_event", json!({"type":kind}), |entry| {
            if let Some(response_id) = value
                .pointer("/response/id")
                .or_else(|| value.get("id"))
                .and_then(Value::as_str)
            {
                entry.response_id = Some(response_id.chars().take(128).collect());
            }
            if (matches!(kind, "response.completed" | "proxy.stream.done")
                || response_status == Some("completed"))
                && entry.terminal.as_deref() != Some("failed")
            {
                entry.terminal = Some("succeeded".into());
            }
            if matches!(kind, "response.failed" | "response.incomplete" | "error")
                || has_error
                || matches!(response_status, Some("failed" | "incomplete"))
            {
                entry.terminal = Some("failed".into());
                entry.error_code = value
                    .pointer("/response/error/code")
                    .or_else(|| value.pointer("/error/code"))
                    .or_else(|| value.pointer("/response/incomplete_details/reason"))
                    .or_else(|| value.pointer("/incomplete_details/reason"))
                    .and_then(Value::as_str)
                    .map(|v| v.chars().take(128).collect());
            }
            true
        });
    }
    pub fn first_output(&self, id: u64) {
        self.update(id, "first_output", json!({}), |entry| {
            if entry.first_output_ms.is_some() {
                return false;
            }
            entry.first_output_ms = Some(entry.elapsed());
            true
        });
    }
    pub fn first_byte(&self, id: u64) {
        self.update(id, "first_byte", json!({}), |entry| {
            if entry.first_byte_ms.is_some() {
                return false;
            }
            entry.first_byte_ms = Some(entry.elapsed());
            true
        });
    }
    pub async fn flush(&self) -> anyhow::Result<()> {
        if let Some(database) = &self.database {
            database.flush().await?;
        }
        Ok(())
    }
}
