use super::*;
use anyhow::{anyhow, bail, ensure};
use std::collections::{BTreeMap, HashMap, HashSet};

struct State {
    id: String,
    created: u64,
    model: String,
    started: bool,
    ended: bool,
    include_usage: bool,
    exclude_reasoning: bool,
    gemini: bool,
    texts: HashMap<(usize, usize, String), String>,
    summaries: HashMap<(usize, bool), String>,
    tools: BTreeMap<usize, (usize, Value, String)>,
    encrypted: HashSet<usize>,
    retained: usize,
}
impl State {
    fn new(model: String, include_usage: bool, exclude_reasoning: bool) -> Self {
        Self {
            id: format!("chatcmpl-{:032x}", rand::random::<u128>()),
            created: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            model,
            started: false,
            ended: false,
            include_usage,
            exclude_reasoning,
            gemini: false,
            texts: Default::default(),
            summaries: Default::default(),
            tools: Default::default(),
            encrypted: Default::default(),
            retained: 0,
        }
    }
    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        let mut value = json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":finish,"logprobs":null}]});
        if self.include_usage {
            value["usage"] = Value::Null;
        }
        value
    }
    fn retain(&mut self, bytes: usize) -> Result<()> {
        self.retained = self.retained.saturating_add(bytes);
        ensure!(
            self.retained <= LIMIT,
            "Chat stream exceeds 64 MiB of translated output"
        );
        Ok(())
    }
    fn text(
        &mut self,
        output: usize,
        content: usize,
        field: &str,
        text: &str,
        full: bool,
        chunks: &mut Vec<Value>,
    ) -> Result<()> {
        let prior = self
            .texts
            .entry((output, content, field.into()))
            .or_default();
        let delta = if full {
            text.strip_prefix(prior.as_str())
                .ok_or_else(|| anyhow!("Response text changed after being streamed"))?
        } else {
            text
        };
        if !delta.is_empty() {
            prior.push_str(delta);
            self.retain(delta.len())?;
            chunks.push(self.chunk(json!({field:delta}), None));
        }
        Ok(())
    }
    fn summary(
        &mut self,
        index: usize,
        id: &Value,
        text: &str,
        full: bool,
        raw: bool,
        chunks: &mut Vec<Value>,
    ) -> Result<()> {
        if self.exclude_reasoning {
            return Ok(());
        }
        let prior = self.summaries.entry((index, raw)).or_default();
        let delta = if full {
            text.strip_prefix(prior.as_str())
                .ok_or_else(|| anyhow!("Reasoning summary changed after being streamed"))?
        } else {
            text
        };
        if !delta.is_empty() {
            prior.push_str(delta);
            self.retain(delta.len())?;
            chunks.push(self.chunk(json!({"reasoning":delta,"reasoning_details":[{"type":if raw {"reasoning.text"} else {"reasoning.summary"},(if raw {"text"} else {"summary"}):delta,"id":id,"format":if self.gemini {"google-gemini-v1"} else {"openai-responses-v1"},"index":index*3+usize::from(raw)}]}),None));
        }
        Ok(())
    }
    fn tool(
        &mut self,
        index: usize,
        item: &Value,
        full: bool,
        chunks: &mut Vec<Value>,
    ) -> Result<()> {
        if !self.tools.contains_key(&index) {
            ensure!(
                item["call_id"].is_string() && item["name"].is_string(),
                "Function call lacks identity"
            );
            let number = self.tools.len();
            self.tools
                .insert(index, (number, item.clone(), String::new()));
            self.retain(item.to_string().len())?;
            chunks.push(self.chunk(json!({"tool_calls":[{"index":number,"id":item["call_id"],"type":"function","function":{"name":item["name"],"arguments":""}}]}),None));
        }
        if full {
            self.arguments(
                index,
                item["arguments"].as_str().unwrap_or(""),
                true,
                chunks,
            )?;
        }
        Ok(())
    }
    fn arguments(
        &mut self,
        index: usize,
        args: &str,
        full: bool,
        chunks: &mut Vec<Value>,
    ) -> Result<()> {
        let (number, _, previous) = self
            .tools
            .get_mut(&index)
            .ok_or_else(|| anyhow!("Function arguments arrived before the function identity"))?;
        let delta = if full {
            args.strip_prefix(previous.as_str())
                .ok_or_else(|| anyhow!("Function arguments changed after being streamed"))?
        } else {
            args
        };
        if !delta.is_empty() {
            previous.push_str(delta);
            let number = *number;
            self.retain(delta.len())?;
            chunks.push(self.chunk(
                json!({"tool_calls":[{"index":number,"function":{"arguments":delta}}]}),
                None,
            ));
        }
        Ok(())
    }
    fn item(
        &mut self,
        index: usize,
        item: &Value,
        full: bool,
        chunks: &mut Vec<Value>,
    ) -> Result<()> {
        match item["type"].as_str() {
            Some("function_call") => self.tool(index, item, full, chunks)?,
            Some("message") if full => {
                for (content, part) in item["content"]
                    .as_array()
                    .ok_or_else(|| anyhow!("Message lacks content"))?
                    .iter()
                    .enumerate()
                {
                    match part["type"].as_str() {
                        Some("output_text") => self.text(
                            index,
                            content,
                            "content",
                            part["text"].as_str().unwrap_or(""),
                            true,
                            chunks,
                        )?,
                        Some("refusal") => self.text(
                            index,
                            content,
                            "refusal",
                            part["refusal"].as_str().unwrap_or(""),
                            true,
                            chunks,
                        )?,
                        _ => bail!("Unsupported response content in chat stream"),
                    }
                }
            }
            Some("reasoning") if full && !self.exclude_reasoning => {
                let summary: String = item["summary"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p["text"].as_str())
                    .collect();
                self.summary(index, &item["id"], &summary, true, false, chunks)?;
                let text: String = item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p["text"].as_str())
                    .collect();
                self.summary(index, &item["id"], &text, true, true, chunks)?;
                if item["encrypted_content"].is_string() && self.encrypted.insert(index) {
                    let detail = response::reasoning_details(item, index)
                        .into_iter()
                        .find(|v| v["type"] == "reasoning.encrypted")
                        .unwrap();
                    self.retain(detail.to_string().len())?;
                    chunks.push(self.chunk(json!({"reasoning_details":[detail]}), None));
                }
            }
            Some("message" | "reasoning") => {}
            _ => bail!("Unsupported response output in chat stream"),
        }
        Ok(())
    }
    fn event(&mut self, event: Value) -> Result<Vec<Value>> {
        if self.ended {
            return Ok(Vec::new());
        }
        let kind = event["type"]
            .as_str()
            .ok_or_else(|| anyhow!("Responses stream event lacks type"))?;
        if kind == "error" || kind == "response.failed" {
            self.ended = true;
            let error = event.get("error").or_else(|| event.pointer("/response/error")).cloned().unwrap_or_else(|| json!({"message":event.get("message").and_then(Value::as_str).unwrap_or("Responses stream failed"),"type":"server_error"}));
            return Ok(vec![json!({"error":error})]);
        }
        let mut chunks = Vec::new();
        if !self.started {
            if let Some(response) = event.get("response") {
                if let Some(id) = response["id"].as_str() {
                    self.id = format!("chatcmpl-{id}");
                }
                if let Some(created) = response["created_at"].as_u64() {
                    self.created = created;
                }
                self.gemini = response["model"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("gemini/"));
            }
            self.started = true;
            chunks.push(self.chunk(json!({"role":"assistant","content":""}), None));
        }
        let index = event["output_index"].as_u64().unwrap_or(0) as usize;
        let content = event["content_index"].as_u64().unwrap_or(0) as usize;
        let delta = event["delta"].as_str().unwrap_or("");
        match kind {
            "response.output_item.added" => self.item(index, &event["item"], false, &mut chunks)?,
            "response.output_item.done" => self.item(index, &event["item"], true, &mut chunks)?,
            "response.output_text.delta" => {
                self.text(index, content, "content", delta, false, &mut chunks)?
            }
            "response.refusal.delta" => {
                self.text(index, content, "refusal", delta, false, &mut chunks)?
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => self
                .summary(
                    index,
                    &event["item_id"],
                    delta,
                    false,
                    kind == "response.reasoning_text.delta",
                    &mut chunks,
                )?,
            "response.function_call_arguments.delta" => {
                self.arguments(index, delta, false, &mut chunks)?
            }
            "response.completed" | "response.incomplete" => {
                let response = &event["response"];
                let output = response["output"]
                    .as_array()
                    .ok_or_else(|| anyhow!("Terminal response lacks output"))?;
                for (index, item) in output.iter().enumerate() {
                    self.item(index, item, true, &mut chunks)?;
                }
                chunks.push(self.chunk(json!({}), Some(response::finish_reason(response)?)));
                if self.include_usage {
                    let mut usage = self.chunk(json!({}), None);
                    usage["choices"] = json!([]);
                    usage["usage"] = response::usage(&response["usage"]);
                    chunks.push(usage);
                }
                self.ended = true;
            }
            _ => {}
        }
        Ok(chunks)
    }
}
fn frame(value: Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}
type Log = Option<(Arc<logs::Store>, u64)>;
fn failure(message: &str, log: &Log) -> Bytes {
    if let Some((store, id)) = log {
        store.observe(*id, &json!({"error":{"code":"chat_stream_error"}}));
    }
    frame(json!({"error":{"type":"server_error","code":"chat_stream_error","message":message}}))
}

#[cfg(test)]
pub(super) fn adapt(
    upstream: Response,
    model: String,
    include_usage: bool,
    exclude_reasoning: bool,
) -> Response {
    adapt_with_logs(upstream, model, include_usage, exclude_reasoning, None)
}

pub(super) fn adapt_with_logs(
    upstream: Response,
    model: String,
    include_usage: bool,
    exclude_reasoning: bool,
    log: Log,
) -> Response {
    if !upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"))
    {
        return error(StatusCode::BAD_GATEWAY, "Expected a Responses event stream");
    }
    let (mut parts, body) = upstream.into_parts();
    response::clean_headers(&mut parts.headers);
    parts.headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    parts.headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    parts
        .headers
        .insert("x-accel-buffering", header::HeaderValue::from_static("no"));
    let mut source = body.into_data_stream();
    let mut decoder = super::super::sse::SseDecoder::default();
    let mut state = State::new(model, include_usage, exclude_reasoning);
    let body = Body::from_stream(async_stream::stream! {
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        heartbeat.tick().await;
        'read: loop {
            let chunk = tokio::select! {
                chunk = source.next() => chunk,
                _ = heartbeat.tick() => { yield Ok::<Bytes,std::io::Error>(Bytes::from_static(b": keep-alive\n\n")); continue; }
            };
            let eof = chunk.is_none();
            let events = match chunk {
                Some(Ok(bytes)) => decoder.feed(&bytes,false),
                Some(Err(_)) => { yield Ok(failure("Responses stream disconnected",&log)); break; }
                None => decoder.feed(&[],true),
            };
            match events {
                Ok(events) => for event in events {
                    match state.event(event) {
                        Ok(chunks) => for chunk in chunks {
                            if let Some((store,id)) = &log { store.observe(*id,&chunk); }
                            yield Ok(frame(chunk));
                        },
                        Err(e) => { yield Ok(failure(&e.to_string(),&log)); break 'read; }
                    }
                    if state.ended { break 'read; }
                },
                Err(_) => { yield Ok(failure("Invalid or oversized Responses SSE frame",&log)); break; }
            }
            if eof {
                if !state.ended { yield Ok(failure("Responses stream ended without a terminal event",&log)); }
                break;
            }
        }
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    });
    Response::from_parts(parts, body)
}
