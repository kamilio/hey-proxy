use super::*;
mod database;
mod lifecycle;
mod pricing;
mod query;
pub(super) mod report;
mod store;
mod websocket;
pub(super) use lifecycle::{Attempt, RequestGuard};
use store::RECENT_LIMIT as LIMIT;
pub(crate) use store::Store;
pub(super) use websocket::Tracker as WebSocketTracker;

#[derive(Default)]
struct RemoteCache {
    updated: Option<Instant>,
    fetching: bool,
    machines: Vec<Value>,
    entries: Vec<Value>,
}
#[derive(Default)]
struct WindowCache {
    minutes: Option<u64>,
    updated: Option<Instant>,
    value: Value,
}

// Explicit allowlist: this must never serialize Config or actual key values.
fn routing_config(config: &Config) -> Value {
    json!({"default_project":config.default.api_key,"aliases":config.aliases,"fallbacks":config.fallbacks,"mode":config.mode})
}
// Inspect a bounded copy while forwarding original chunks without waiting for completion.
// Oversized payloads/events remain opaque; no payload is retained in the store.
pub(super) struct UsageReader {
    sse: bool,
    buffer: Vec<u8>,
    event: Vec<u8>,
    overflow: bool,
    first_output: bool,
    line_nonempty: bool,
}
impl UsageReader {
    pub(super) fn new(sse: bool) -> Self {
        Self {
            sse,
            buffer: Vec::new(),
            event: Vec::new(),
            overflow: false,
            first_output: false,
            line_nonempty: false,
        }
    }
    fn parse(&mut self, bytes: &[u8], store: &Store, id: u64) {
        if bytes.trim_ascii() == b"[DONE]" {
            store.observe(id, &json!({"type":"proxy.stream.done"}));
        } else if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
            if !self.first_output
                && (value["type"].as_str().is_some_and(|kind| {
                    kind.ends_with(".delta") || kind == "response.output_item.added"
                }) || value
                    .get("choices")
                    .and_then(Value::as_array)
                    .is_some_and(|items| !items.is_empty()))
            {
                self.first_output = true;
                store.first_output(id);
            }
            store.observe(id, &value);
        }
    }
    pub(super) fn feed(&mut self, bytes: &[u8], store: &Store, id: u64) {
        const MAX: usize = 8 * 1024 * 1024;
        let sse = self.sse;
        for part in bytes.split_inclusive(|byte| sse && *byte == b'\n') {
            if part.iter().any(|byte| !matches!(byte, b'\r' | b'\n')) {
                self.line_nonempty = true;
            }
            if !self.overflow {
                if self.buffer.len() + self.event.len() + part.len() > MAX {
                    self.buffer.clear();
                    self.event.clear();
                    self.overflow = true;
                } else {
                    self.buffer.extend_from_slice(part);
                }
            }
            if self.sse && part.last() == Some(&b'\n') {
                let line = std::mem::take(&mut self.buffer);
                let line = line.strip_suffix(b"\n").unwrap_or(&line);
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                let blank = !self.line_nonempty;
                self.line_nonempty = false;
                if blank {
                    if !self.overflow {
                        let event = std::mem::take(&mut self.event);
                        self.parse(&event, store, id);
                    }
                    self.overflow = false;
                } else if !self.overflow
                    && let Some(data) = line.strip_prefix(b"data:")
                {
                    if !self.event.is_empty() {
                        self.event.push(b'\n');
                    }
                    self.event
                        .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
                }
            }
        }
        if !self.sse
            && !self.overflow
            && self.buffer.iter().rfind(|b| !b.is_ascii_whitespace()) == Some(&b'}')
        {
            let bytes = std::mem::take(&mut self.buffer);
            self.parse(&bytes, store, id);
            self.buffer = bytes;
        }
    }
    pub(super) fn finish(&mut self, store: &Store, id: u64) {
        if self.overflow {
            return;
        }
        if self.sse {
            if let Some(data) = self.buffer.strip_prefix(b"data:") {
                if !self.event.is_empty() {
                    self.event.push(b'\n');
                }
                self.event
                    .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
            }
            let event = std::mem::take(&mut self.event);
            self.parse(&event, store, id);
        } else {
            let bytes = std::mem::take(&mut self.buffer);
            self.parse(&bytes, store, id);
        }
    }
}
pub(super) fn json_model(bytes: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    ["/model", "/session/model", "/response/model"]
        .iter()
        .find_map(|pointer| {
            value
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}
pub(super) fn uri_model(uri: &axum::http::Uri) -> Option<String> {
    if let Some(model) = uri
        .path()
        .strip_prefix("/v1/models/")
        .filter(|v| !v.contains('/'))
    {
        return Some(
            percent_encoding::percent_decode_str(model)
                .decode_utf8_lossy()
                .into_owned(),
        );
    }
    let url = reqwest::Url::parse(&format!("http://localhost{uri}")).ok()?;
    url.query_pairs()
        .find(|(key, _)| key == "model")
        .map(|(_, value)| value.into_owned())
}
pub(super) async fn page() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::response::Html(include_str!("logs.html")),
    )
}
pub(super) async fn script() -> impl IntoResponse {
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
        ],
        include_str!("dashboard.js"),
    )
}
#[derive(serde::Deserialize, Default)]
pub(super) struct Query {
    #[serde(default)]
    local: bool,
    minutes: Option<u64>,
}

// Credentials are read and used on the remote machine, never returned over SSH.
const REMOTE_READ: &str = r#"import json, pathlib, urllib.request, sys
p = pathlib.Path.home() / '.hey-proxy/config.json'
c = json.loads(p.read_text())
address = c.get('listen', '127.0.0.1:8080')
if address.startswith('0.0.0.0:'): address = address.replace('0.0.0.0:', '127.0.0.1:', 1)
if address.startswith('[::]:'): address = address.replace('[::]:', '[::1]:', 1)
headers = {}
if c.get('mode') == 'host':
    keys = json.loads(p.with_suffix('.access-keys.json').read_text())
    headers['Authorization'] = 'Bearer ' + keys['local']
r = urllib.request.Request('http://' + address + '/logs/api?local=true' + sys.argv[1], headers=headers)
with urllib.request.urlopen(r, timeout=12) as response:
    data = response.read(48 * 1024 * 1024 + 1)
    if len(data) > 48 * 1024 * 1024: raise ValueError('Dashboard response too large')
    value = json.loads(data)
    machine = value.get('machines', [{}])[0]
    print(json.dumps({'entries': value['entries'], 'mode': c.get('mode', 'standalone'), 'coverage': machine.get('coverage'), 'routing': machine.get('routing')}))
"#;
async fn remote_entries(host: String, minutes: Option<u64>) -> (String, Option<Value>) {
    let argument = minutes.map(|m| format!("&minutes={m}")).unwrap_or_default();
    let script = format!(
        "python3 -c '{}' '{argument}'",
        REMOTE_READ.replace('\'', "'\\''")
    );
    let mut command = tokio::process::Command::new("ssh");
    command.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=4",
        "--",
        &host,
        &script,
    ]);
    command.kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(16), command.output()).await;
    let value = match result {
        Ok(Ok(output)) if output.status.success() => {
            serde_json::from_slice::<Value>(&output.stdout).ok()
        }
        _ => None,
    };
    (host, value)
}
pub(super) async fn entries(
    State(service): State<Arc<Service>>,
    axum::extract::Query(query): axum::extract::Query<Query>,
) -> Response {
    if query.minutes.is_some_and(|m| ![15, 60, 1440].contains(&m)) {
        return (StatusCode::BAD_REQUEST, "minutes must be 15, 60, or 1440").into_response();
    }
    let config = service.snapshot().config;
    let mut entries: Vec<Value> = service
        .logs
        .recent()
        .iter()
        .map(|entry| {
            let mut value = serde_json::to_value(entry).unwrap();
            value["machine"] = json!("local");
            value["mode"] = json!(config.mode);
            value
        })
        .collect();
    let mut coverage = json!({"source":"memory","limit":LIMIT,"truncated":entries.len()>=LIMIT});
    if let (Some(minutes), Some(database)) = (query.minutes, &service.logs.database) {
        let cached = {
            let cache = service
                .logs
                .window
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (cache.minutes == Some(minutes)
                && cache
                    .updated
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(5)))
            .then(|| cache.value.clone())
        };
        let value = if let Some(value) = cached {
            value
        } else {
            let end = store::now_ms().saturating_add(1);
            let start = end.saturating_sub(minutes * 60_000);
            match database
                .read(move |connection| query::window(connection, start, end))
                .await
            {
                Ok(value) => {
                    let mut cache = service
                        .logs
                        .window
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    *cache = WindowCache {
                        minutes: Some(minutes),
                        updated: Some(Instant::now()),
                        value: value.clone(),
                    };
                    value
                }
                Err(_) => {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Dashboard history unavailable",
                    )
                        .into_response();
                }
            }
        };
        coverage = value["coverage"].clone();
        let mut merged = std::collections::BTreeMap::new();
        for entry in value["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(entries.iter())
        {
            merged.insert(
                entry["request_id"].as_str().unwrap_or_default().to_owned(),
                entry.clone(),
            );
        }
        entries = merged
            .into_values()
            .map(|mut entry| {
                entry["machine"] = json!("local");
                entry
            })
            .collect();
    }
    if let Some(minutes) = query.minutes {
        let end = store::now_ms().saturating_add(1);
        let start = end.saturating_sub(minutes * 60_000);
        entries.retain(|e| {
            e["timestamp_ms"]
                .as_u64()
                .is_some_and(|t| t >= start && t < end)
        });
        entries.sort_by_key(|e| {
            std::cmp::Reverse((
                e["timestamp_ms"].as_u64().unwrap_or(0),
                e["request_id"].as_str().unwrap_or_default().to_owned(),
            ))
        });
        if entries.len() > 50_000 {
            entries.truncate(50_000);
            coverage["truncated"] = json!(true);
        }
    }
    let mut machines = vec![
        json!({"name":"local", "status":"live", "mode":config.mode,"routing":routing_config(&config),"coverage":coverage}),
    ];
    if !query.local {
        let hosts: Vec<String> = config
            .ssh_hosts
            .iter()
            .map(|host| host.host().to_owned())
            .collect();
        let mut cache = service
            .logs
            .remote
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cache = cache.entry(query.minutes).or_default();
        machines.extend(hosts.iter().map(|host| {
            cache
                .machines
                .iter()
                .find(|m| m["name"] == *host)
                .cloned()
                .unwrap_or_else(|| json!({"name":host,"status":"connecting"}))
        }));
        entries.extend(
            cache
                .entries
                .iter()
                .filter(|e| hosts.iter().any(|h| e["machine"] == *h))
                .cloned(),
        );
        if !cache.fetching
            && cache
                .updated
                .is_none_or(|time| time.elapsed() >= Duration::from_secs(10))
        {
            cache.fetching = true;
            let service = service.clone();
            tokio::spawn(async move {
                let mut tasks = tokio::task::JoinSet::new();
                for host in hosts {
                    tasks.spawn(remote_entries(host, query.minutes));
                }
                let mut machines = Vec::new();
                let mut entries = Vec::new();
                while let Some(Ok((host, value))) = tasks.join_next().await {
                    let mode = value
                        .as_ref()
                        .and_then(|v| v.get("mode"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    let remote = value.as_ref().and_then(|v| v["entries"].as_array());
                    machines.push(json!({"name":host,"status":if remote.is_some(){"live"}else{"unavailable"},"mode":mode,"routing":value.as_ref().and_then(|v|v.get("routing")),"coverage":value.as_ref().and_then(|v|v.get("coverage"))}));
                    if let Some(remote) = remote {
                        for entry in remote {
                            let mut entry = entry.clone();
                            entry["machine"] = json!(host);
                            entry["mode"] = mode.clone();
                            entries.push(entry);
                        }
                    }
                }
                let mut cache = service
                    .logs
                    .remote
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let cache = cache.entry(query.minutes).or_default();
                cache.machines = machines;
                cache.entries = entries;
                cache.updated = Some(Instant::now());
                cache.fetching = false;
            });
        }
    }
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(
            json!({"entries":entries,"machines":machines,"limit":LIMIT,"logging":service.logs.database.as_ref().map(|db|db.health()).unwrap_or_else(||json!({"enabled":false,"storage":"memory","status":"disabled"}))}),
        ),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn dashboard_reports_actual_routing_without_payloads_or_keys() {
        let upstream = Router::new().fallback(|| async {
            axum::Json(json!({"usage":{"input_tokens":120,"output_tokens":35,
                "input_tokens_details":{"cached_tokens":80,"cache_write_tokens":20}}}))
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url = format!("http://{}", listener.local_addr().unwrap());
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let config = Config {
            upstream_url,
            ..Config::test_fixture()
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router(config).unwrap())
                .await
                .unwrap();
        });
        let client = reqwest::Client::new();
        assert!(
            client
                .get(format!("{url}/logs"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
                .contains("Model traffic")
        );
        let script = client
            .get(format!("{url}/logs/dashboard.js"))
            .send()
            .await
            .unwrap();
        assert_eq!(script.status(), 200);
        assert_eq!(
            script.headers()[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        assert!(script.text().await.unwrap().contains("function estimate"));
        client
            .post(format!("{url}/v1/responses"))
            .json(&json!({"model":"gpt-4.1","input":"private prompt"}))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let text = client
            .get(format!("{url}/logs/api"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!text.contains("private prompt"));
        assert!(!text.contains("something"));
        assert!(!text.contains("replace-with-primary-api-key"));
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["entries"].as_array().unwrap().len(), 1);
        let entry = &value["entries"][0];
        assert_eq!(entry["requested_model"], "gpt-4.1");
        assert_eq!(entry["routed_model"], "gpt-4.1-mini");
        assert_eq!(entry["project"], "default");
        assert_eq!(entry["requested_reasoning"], Value::Null);
        assert_eq!(entry["routed_reasoning"], "low");
        assert_eq!(entry["route_rule"], "alias");
        assert_eq!(
            value["machines"][0]["routing"]["default_project"],
            "default"
        );
        assert_eq!(entry["status"], 200);
        assert_eq!(entry["input_tokens"], 120);
        assert_eq!(entry["output_tokens"], 35);
        assert_eq!(entry["cached_input_tokens"], 80);
        assert_eq!(entry["cache_write_tokens"], 20);
        task.abort();
        upstream_task.abort();
    }
    #[test]
    fn usage_handles_split_streams_and_snapshot_updates() {
        let store = Store::default();
        let id = store.begin("POST", "/v1/responses", "HTTP");
        let mut reader = UsageReader::new(true);
        let stream = b"event: response.completed\r\ndata: {\"response\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":8,\"input_tokens_details\":{\"cached_tokens\":20,\"cache_write_tokens\":10}}}}\r\n\ndata: [DONE]\n";
        for chunk in stream.chunks(3) {
            reader.feed(chunk, &store, id);
        }
        reader.finish(&store, id);
        {
            let state = store.recent();
            assert_eq!(state[0].cached_input_tokens, Some(20));
            assert_eq!(state[0].cache_write_tokens, Some(10));
        }
        store.usage(
            id,
            &json!({"usage":{"prompt_tokens":42,"completion_tokens":9,
                "prompt_tokens_details":{"cached_tokens":30,"cache_write_tokens":5}}}),
        );
        let state = store.recent();
        let entry = &state[0];
        assert_eq!(entry.input_tokens, Some(42));
        assert_eq!(entry.output_tokens, Some(9));
        assert_eq!(entry.cached_input_tokens, Some(30));
        assert_eq!(entry.cache_write_tokens, Some(5));
    }
    #[test]
    fn remote_reader_script_is_valid_python_and_uses_local_only_endpoint() {
        let status = std::process::Command::new("python3")
            .args(["-c", "import ast,sys; ast.parse(sys.argv[1])", REMOTE_READ])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(REMOTE_READ.contains("/logs/api?local=true"));
    }
    #[test]
    fn history_is_bounded_and_newest_first() {
        let store = Store::default();
        for _ in 0..LIMIT + 2 {
            store.begin("GET", "/v1/models", "HTTP");
        }
        let state = store.recent();
        assert_eq!(state.len(), LIMIT);
        assert_eq!(state.last().unwrap().id, 3);
    }
}

#[cfg(test)]
mod persistence_tests;
