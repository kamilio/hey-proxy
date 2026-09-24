use super::{RequestGuard, Store};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

/// Keeps request lifetimes independent of the bounded recent-history view.
pub struct Tracker {
    store: Arc<Store>,
    pending: VecDeque<u64>,
    responses: HashMap<String, u64>,
    guards: HashMap<u64, RequestGuard>,
    output_seen: HashSet<u64>,
}
impl Tracker {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            pending: VecDeque::new(),
            responses: HashMap::new(),
            guards: HashMap::new(),
            output_seen: HashSet::new(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.guards.is_empty()
    }
    pub fn begin(&mut self, guard: RequestGuard) {
        let id = guard.id();
        self.pending.push_back(id);
        self.guards.insert(id, guard);
    }
    pub fn observe(&mut self, value: &Value, bytes: usize) {
        let kind = value["type"].as_str().unwrap_or("");
        let terminal = matches!(
            kind,
            "response.completed" | "response.failed" | "response.incomplete" | "error"
        );
        let response_id = value
            .pointer("/response/id")
            .or_else(|| value.pointer("/response_id"))
            .and_then(Value::as_str);
        if let Some(response_id) = response_id
            && !self.responses.contains_key(response_id)
            && (kind == "response.created" || (terminal && self.guards.len() == 1))
            && let Some(id) = self.pending.pop_front()
        {
            self.responses.insert(response_id.to_owned(), id);
        }
        let id = response_id
            .and_then(|id| self.responses.get(id).copied())
            .or_else(|| {
                if self.guards.len() == 1 && response_id.is_none() {
                    self.guards.keys().next().copied()
                } else {
                    None
                }
            });
        let Some(id) = id else {
            return;
        };
        if let Some(guard) = self.guards.get_mut(&id) {
            guard.add_bytes(bytes);
        }
        if (kind.ends_with(".delta") || kind == "response.output_item.added")
            && self.output_seen.insert(id)
        {
            self.store.first_output(id);
        }
        self.store.observe(id, value);
        if terminal {
            if let Some(mut guard) = self.guards.remove(&id) {
                guard.finish(
                    if kind == "response.completed" {
                        "succeeded"
                    } else {
                        "failed"
                    },
                    "responses_event",
                    None,
                );
            }
            self.responses.retain(|_, value| *value != id);
            self.pending.retain(|value| *value != id);
            self.output_seen.remove(&id);
        }
    }
    pub fn fail_all(&mut self, code: &str) {
        for (_, mut guard) in self.guards.drain() {
            guard.finish("failed", "transport", Some(code));
        }
        self.pending.clear();
        self.responses.clear();
        self.output_seen.clear();
    }
}
