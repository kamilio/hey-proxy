use super::response::output_items;
use super::{ConvertedRequest, ReasoningCodec, convert_response};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

/// Incremental native GenerateContent SSE -> typed Responses events. Transport
/// framing and JSON decoding remain separate, so this can also power WebSockets.
pub struct ResponseStream {
    request: ConvertedRequest,
    id: String,
    native: Value,
    sequence: u64,
    started: bool,
    ended: bool,
    active: Option<usize>,
    done_items: Vec<usize>,
    partial_calls: super::partial::PartialCalls,
    summary_open: bool,
    summary: String,
    deferred: Vec<Value>,
    wire_sequence: u64,
}
impl ResponseStream {
    pub fn new(request: ConvertedRequest, id: impl Into<String>) -> Self {
        Self {
            request,
            id: id.into(),
            native: json!({"candidates":[{"index":0,"content":{"role":"model","parts":[]}}]}),
            sequence: 0,
            started: false,
            ended: false,
            active: None,
            done_items: Vec::new(),
            partial_calls: Default::default(),
            summary_open: false,
            summary: String::new(),
            deferred: Vec::new(),
            wire_sequence: 0,
        }
    }
    fn emit(&mut self, kind: &str, mut event: Value) -> Value {
        event["type"] = json!(kind);
        event["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        event
    }
    fn item_event(&self, index: usize) -> Value {
        json!({"output_index":index,"item_id":format!("msg_{}_{index}",self.id),"content_index":0})
    }
    fn wire_events(&mut self, mut events: Vec<Value>) -> Vec<Value> {
        if self.request.output_schema.is_some() {
            for event in &mut events {
                event["sequence_number"] = json!(self.wire_sequence);
                self.wire_sequence += 1;
            }
        }
        events
    }
    fn close_active(&mut self, events: &mut Vec<Value>) -> Result<()> {
        if let Some(index) = self.active.take() {
            let parts = self.native["candidates"][0]["content"]["parts"]
                .as_array()
                .unwrap();
            let items = output_items(parts, &self.request, &self.id)?;
            let item = items
                .get(index - 1)
                .ok_or_else(|| anyhow!("Invalid active stream item"))?
                .clone();
            let mut event = self.item_event(index);
            event["text"] = item["content"][0]["text"].clone();
            events.push(self.emit("response.output_text.done", event));
            let mut event = self.item_event(index);
            event["part"] = item["content"][0].clone();
            events.push(self.emit("response.content_part.done", event));
            self.done_items.push(index);
        }
        Ok(())
    }
    pub fn feed(&mut self, chunk: &Value) -> Result<Vec<Value>> {
        if self.ended {
            bail!("Gemini event arrived after stream completion");
        }
        if chunk.get("error").is_some() {
            let error = super::ResponseError::from_native(chunk, None);
            self.native["error"] = chunk["error"].clone();
            let mut event = self.fail("server_error", "Gemini request failed");
            event["response"]["error"] = error.error;
            return Ok(vec![event]);
        }
        let result = self.feed_inner(chunk).map(|events| {
            if self.request.output_schema.is_none() {
                return events;
            }
            let mut immediate = Vec::new();
            for event in events {
                let kind = event["type"].as_str().unwrap_or("");
                // Reasoning and lifecycle events remain live. Structured output
                // is released only after the complete JSON passes validation.
                if matches!(kind, "response.created" | "response.in_progress")
                    || kind.starts_with("response.reasoning_")
                    || (kind == "response.output_item.added" && event["output_index"] == 0)
                {
                    immediate.push(event);
                } else {
                    self.deferred.push(event);
                }
            }
            self.wire_events(immediate)
        });
        if result.is_err() {
            self.ended = true;
        }
        result
    }
    fn feed_inner(&mut self, chunk: &Value) -> Result<Vec<Value>> {
        super::validate::response(chunk)?;
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            let response = json!({"id":format!("resp_{}",self.id),"object":"response","model":self.request.response_model,"status":"in_progress","output":[],"usage":null});
            events.push(self.emit("response.created", json!({"response":response})));
            events.push(self.emit("response.in_progress", json!({"response":response})));
            events.push(self.emit("response.output_item.added",json!({"output_index":0,"item":{"id":format!("rs_{}",self.id),"type":"reasoning","summary":[]}})));
        }
        if let Some(candidates) = chunk.get("candidates").and_then(Value::as_array) {
            if candidates.len() > 1
                || candidates
                    .first()
                    .and_then(|c| c.get("index"))
                    .and_then(Value::as_u64)
                    .is_some_and(|i| i != 0)
            {
                bail!("Cannot discard multiple Gemini candidates");
            }
            if let Some(candidate) = candidates.first() {
                if let Some(parts) = candidate
                    .pointer("/content/parts")
                    .and_then(Value::as_array)
                {
                    let mut normalized = Vec::new();
                    for part in parts {
                        normalized.extend(self.partial_calls.part(part)?);
                    }
                    for part in &normalized {
                        let thought = part["thought"] == true;
                        let text = part.get("text").and_then(Value::as_str);
                        let prior = self.native["candidates"][0]["content"]["parts"]
                            .as_array()
                            .unwrap()
                            .last();
                        let merge = text.is_some()
                            && prior.is_some_and(|p| {
                                p.get("text").is_some()
                                    && (p["thought"] == true) == thought
                                    && p.get("thoughtSignature").is_none()
                            });
                        let signature_only = part
                            .as_object()
                            .is_some_and(|p| p.len() == 1 && p.contains_key("thoughtSignature"));
                        if !thought && !merge && !signature_only {
                            self.close_active(&mut events)?;
                        }
                        let native_parts = self.native["candidates"][0]["content"]["parts"]
                            .as_array_mut()
                            .unwrap();
                        if signature_only {
                            let last = native_parts.last_mut().ok_or_else(|| {
                                anyhow!("Detached signature has no preceding part")
                            })?;
                            if last.get("thoughtSignature").is_some() {
                                bail!("Conflicting Gemini signatures");
                            }
                            last["thoughtSignature"] = part["thoughtSignature"].clone();
                        } else if merge {
                            let last = native_parts.last_mut().unwrap();
                            if let Value::String(previous) = &mut last["text"] {
                                previous.push_str(text.unwrap());
                            }
                            for (key, value) in part.as_object().unwrap() {
                                if key != "text" {
                                    last[key] = value.clone();
                                }
                            }
                        } else {
                            native_parts.push(part.clone());
                        }
                        if thought {
                            if let Some(text) = text {
                                if !self.summary_open {
                                    self.summary_open = true;
                                    events.push(self.emit("response.reasoning_summary_part.added",json!({"output_index":0,"item_id":format!("rs_{}",self.id),"summary_index":0,"part":{"type":"summary_text","text":""}})));
                                }
                                self.summary.push_str(text);
                                events.push(self.emit("response.reasoning_summary_text.delta",json!({"output_index":0,"item_id":format!("rs_{}",self.id),"summary_index":0,"delta":text})));
                            }
                        } else if let Some(text) = text {
                            if text.is_empty() {
                                continue;
                            }
                            if !merge || self.active.is_none() {
                                let parts = self.native["candidates"][0]["content"]["parts"]
                                    .as_array()
                                    .unwrap();
                                let items = output_items(parts, &self.request, &self.id)?;
                                let index = items.len();
                                self.active = Some(index);
                                let mut item = items.last().unwrap().clone();
                                item["status"] = json!("in_progress");
                                item["content"] = json!([]);
                                events.push(self.emit(
                                    "response.output_item.added",
                                    json!({"output_index":index,"item":item}),
                                ));
                                let mut event = self.item_event(index);
                                event["part"] =
                                    json!({"type":"output_text","text":"","annotations":[]});
                                events.push(self.emit("response.content_part.added", event));
                            }
                            let index = self.active.unwrap();
                            let mut event = self.item_event(index);
                            event["delta"] = json!(text);
                            events.push(self.emit("response.output_text.delta", event));
                        } else if !signature_only {
                            let parts = self.native["candidates"][0]["content"]["parts"]
                                .as_array()
                                .unwrap();
                            let items = output_items(parts, &self.request, &self.id)?;
                            let index = items.len();
                            let item = items.last().unwrap().clone();
                            let mut initial = item.clone();
                            initial["status"] = json!("in_progress");
                            if item["type"] == "function_call" {
                                initial["arguments"] = json!("");
                            }
                            if item["type"] == "custom_tool_call" {
                                initial["input"] = json!("");
                            }
                            events.push(self.emit(
                                "response.output_item.added",
                                json!({"output_index":index,"item":initial}),
                            ));
                            for (kind, field) in [
                                ("function_call", "arguments"),
                                ("custom_tool_call", "input"),
                            ] {
                                if item["type"] == kind {
                                    let event = json!({"output_index":index,"item_id":item["id"],"delta":item[field]});
                                    let event_type = if kind == "function_call" {
                                        "response.function_call_arguments.delta"
                                    } else {
                                        "response.custom_tool_call_input.delta"
                                    };
                                    events.push(self.emit(event_type, event));
                                }
                            }
                            self.done_items.push(index);
                        }
                    }
                }
                if let Some(content) = candidate.get("content").and_then(Value::as_object) {
                    for (key, value) in content {
                        if key != "parts" {
                            self.native["candidates"][0]["content"][key] = value.clone();
                        }
                    }
                }
                for (key, value) in candidate
                    .as_object()
                    .ok_or_else(|| anyhow!("Invalid Gemini candidate"))?
                {
                    if key != "content" {
                        self.native["candidates"][0][key] = value.clone();
                    }
                }
            }
        }
        for (key, value) in chunk
            .as_object()
            .ok_or_else(|| anyhow!("Gemini stream event must be an object"))?
        {
            if key != "candidates" {
                self.native[key] = value.clone();
            }
        }
        Ok(events)
    }
    /// Terminal transport/parser failure. Never completes executable output.
    /// Native data already accepted is retained for diagnostics.
    pub fn fail(&mut self, code: &str, message: &str) -> Value {
        self.ended = true;
        self.deferred.clear();
        let mut native = std::mem::take(&mut self.native);
        if !self.partial_calls.trace.is_empty() {
            native["streamFunctionCallParts"] =
                json!(std::mem::take(&mut self.partial_calls.trace));
        }
        let event = self.emit(
            "response.failed",
            json!({"response":{
                "id":format!("resp_{}",self.id),"object":"response",
                "model":self.request.response_model,"status":"failed","output":[],
                "usage":super::usage(&native),"gemini":native,
                "error":{"type":"server_error","code":code,"message":message,"param":null}
            }}),
        );
        self.wire_events(vec![event]).remove(0)
    }
    pub fn is_finished(&self) -> bool {
        self.ended
    }
    /// Wait for transport EOF: Gemini can report usage in a final event after
    /// finishReason. Never lose that event by finalizing at the finish marker.
    pub fn finish(&mut self, codec: &ReasoningCodec) -> Result<Vec<Value>> {
        if self.ended {
            bail!("Gemini stream already completed");
        }
        self.ended = true;
        if self.partial_calls.pending() {
            bail!("Gemini stream ended with incomplete function arguments");
        }
        if !self.partial_calls.trace.is_empty() {
            self.native["streamFunctionCallParts"] = json!(self.partial_calls.trace);
        }
        let mut events = Vec::new();
        let mut response = convert_response(&self.native, &self.request, codec, &self.id)?;
        if response["status"] == "failed" {
            // Codex commits output_item.done to history before it receives the
            // terminal event. A failed attempt must not poison its next retry
            // with an unfinished model turn or executable partial tool call.
            self.deferred.clear();
            let event = self.emit("response.failed", json!({"response":response}));
            return Ok(self.wire_events(vec![event]));
        }
        self.close_active(&mut events)?;
        if self.request.output_schema.is_some() {
            let mut deferred = std::mem::take(&mut self.deferred);
            deferred.extend(events);
            events = deferred;
        }
        if self.summary_open {
            let event = json!({"output_index":0,"item_id":format!("rs_{}",self.id),"summary_index":0,"text":self.summary});
            events.push(self.emit("response.reasoning_summary_text.done", event));
            let part = json!({"type":"summary_text","text":self.summary});
            events.push(self.emit("response.reasoning_summary_part.done",json!({"output_index":0,"item_id":format!("rs_{}",self.id),"summary_index":0,"part":part})));
            response["output"][0]["summary"] = json!([part]);
        }
        events.push(self.emit(
            "response.output_item.done",
            json!({"output_index":0,"item":response["output"][0]}),
        ));
        // Codex replays completed items in event order, not by output_index.
        // Finalize the carrier before its associated outputs so native signed
        // parts remain available when the next tool-result request is converted.
        let mut completed = std::mem::take(&mut self.done_items);
        completed.sort_unstable();
        for index in completed {
            // A detached signature can arrive after a nontext item was added.
            // Complete with the final projection authenticated by the carrier,
            // rather than an arrival-time snapshot that would fail replay.
            let item = response["output"]
                .get(index)
                .ok_or_else(|| anyhow!("Invalid completed stream item"))?;
            if matches!(
                item["type"].as_str(),
                Some("function_call" | "custom_tool_call")
            ) {
                if response["status"] != "completed" {
                    continue;
                }
                let (kind, field) = if item["type"] == "function_call" {
                    ("response.function_call_arguments.done", "arguments")
                } else {
                    ("response.custom_tool_call_input.done", "input")
                };
                let mut event = json!({"output_index":index,"item_id":item["id"]});
                event[field] = item[field].clone();
                events.push(self.emit(kind, event));
            }
            let event = json!({"output_index":index,"item":item});
            events.push(self.emit("response.output_item.done", event));
        }
        let kind = match response["status"].as_str() {
            Some("completed") => "response.completed",
            Some("incomplete") => "response.incomplete",
            _ => "response.failed",
        };
        events.push(self.emit(kind, json!({"response":response})));
        Ok(self.wire_events(events))
    }
}
