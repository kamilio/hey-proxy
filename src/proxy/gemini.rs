//! Transport adapter; conversion lives in the reusable hey_proxy library.
use super::*;
use hey_proxy::gemini::{
    Auth, ProviderConfig, ReasoningCodec, ResponseError, ResponseStream, convert_request,
    convert_response,
};
use std::path::Path;

pub(super) struct GeminiState {
    pub codec: ReasoningCodec,
}
impl GeminiState {
    pub fn new(source: Option<&Path>) -> Result<Self> {
        let key = if let Some(source) = source {
            let path = source.with_extension("gemini-reasoning-key");
            // Publish a complete private key atomically. Concurrent startup must
            // never observe a newly created but still empty key file.
            let mut file = tempfile::NamedTempFile::new_in(
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?;
            use std::io::Write;
            let key: [u8; 32] = rand::random();
            file.write_all(&key)?;
            file.as_file().sync_all()?;
            match file.persist_noclobber(&path) {
                Ok(_) => key,
                Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let bytes = std::fs::read(&path)?;
                    bytes
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("Invalid Gemini reasoning key file"))?
                }
                Err(e) => return Err(e.error.into()),
            }
        } else {
            rand::random()
        };
        Ok(Self {
            codec: ReasoningCodec::new(&key),
        })
    }
}

fn sse_bytes(event: &Value) -> Bytes {
    Bytes::from(format!(
        "event: {}\ndata: {}\n\n",
        event["type"].as_str().unwrap_or("error"),
        event
    ))
}
/// Linear-time byte framing preserves split UTF-8 and LF/CRLF/CR delimiters.
/// The bound applies to complete frames too, including ignored comment fields.
const MAX_NATIVE_FRAME: usize = 16 * 1024 * 1024;
const MAX_NATIVE_RESPONSE: usize = 64 * 1024 * 1024;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

fn transport_error(message: &str, proxy_code: &str) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        axum::Json(json!({"error":{
            "type":"server_error","code":"server_error","message":message,
            "param":null,"proxy_code":proxy_code
        }})),
    )
        .into_response()
}
#[derive(Default)]
struct NativeSse {
    line: Vec<u8>,
    data: Vec<u8>,
    frame_bytes: usize,
    after_cr: bool,
    done: bool,
}
impl NativeSse {
    fn dispatch(&mut self, events: &mut Vec<Value>) -> Result<()> {
        self.frame_bytes = 0;
        if self.data.is_empty() {
            return Ok(());
        }
        self.data.pop(); // Final data-line newline.
        if self.data.is_empty() {
            return Ok(());
        }
        if self.data == b"[DONE]" {
            self.done = true;
        } else {
            anyhow::ensure!(!self.done, "Gemini data arrived after [DONE]");
            events.push(serde_json::from_slice(&self.data)?);
        }
        self.data.clear();
        Ok(())
    }
    fn end_line(&mut self, events: &mut Vec<Value>) -> Result<()> {
        if self.line.is_empty() {
            self.dispatch(events)?;
        } else {
            // Validate ignored fields too; never repair malformed UTF-8.
            std::str::from_utf8(&self.line)?;
            if let Some(data) = self.line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                self.data.extend_from_slice(data);
                self.data.push(b'\n');
            }
            self.line.clear();
        }
        Ok(())
    }
    fn feed(&mut self, bytes: &[u8], eof: bool) -> Result<Vec<Value>> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            self.frame_bytes += 1;
            anyhow::ensure!(
                self.frame_bytes <= MAX_NATIVE_FRAME,
                "Gemini SSE frame exceeds 16 MiB"
            );
            if matches!(byte, b'\n' | b'\r') {
                self.end_line(&mut events)?;
            } else {
                self.line.push(byte);
            }
        }
        if eof {
            if !self.line.is_empty() {
                self.end_line(&mut events)?;
            }
            self.dispatch(&mut events)?;
        }
        Ok(events)
    }
}
async fn bounded_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            chunk.len() <= limit - bytes.len(),
            "Gemini response exceeds size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(super) async fn forward(
    proxy: Arc<Proxy>,
    parts: axum::http::request::Parts,
    request: Value,
) -> Response {
    if parts.method != axum::http::Method::POST
        || parts.uri.path().trim_end_matches('/') != "/v1/responses"
        || parts.uri.query().is_some()
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Gemini routes require POST /v1/responses without query parameters",
        );
    }
    let Some(config) = &proxy.config.gemini else {
        return error(
            StatusCode::BAD_REQUEST,
            "Configure gemini before using a gemini/model_name override",
        );
    };
    let converted = match convert_request(&request, config, &proxy.service.gemini.codec) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    proxy.service.logs.update(
        proxy.log_id,
        "provider_selected",
        json!({"provider":"gemini"}),
        |entry| {
            entry.routed_model = Some(converted.response_model.clone());
            entry.project = Some("gemini".into());
            entry.streaming = converted.stream;
            true
        },
    );
    let url = match config.endpoint(&converted.model, converted.stream) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let mut recovery = Recovery::for_request(&proxy);
    let mut upstream = loop {
        let credentials = match credential_headers(&proxy, config).await {
            Ok(v) => v,
            Err(e) => {
                proxy.service.logs.observe(
                    proxy.log_id,
                    &json!({"error":{"code":"gemini_credential_error"}}),
                );
                return transport_error(&e.to_string(), "gemini_credential_error");
            }
        };
        let mut attempt =
            logs::Attempt::new(proxy.service.logs.clone(), proxy.log_id, "gemini_http");
        proxy.service.logs.retries(proxy.log_id, recovery.retries);
        let response = recovery
            .run(
                proxy
                    .client
                    .post(&url)
                    .headers(credentials)
                    .json(&converted.body)
                    .send(),
            )
            .await;
        let response = match response {
            Ok(Ok(r)) => r,
            Err(()) => {
                attempt.finish("timeout", None, Some("proxy_recovery_timeout"));
                return recovery::timeout_response();
            }
            Ok(Err(e)) => {
                attempt.finish("connection_error", None, Some("gemini_connect_failed"));
                if proxy.fallback_attempt {
                    let mut response =
                        transport_error("Could not reach Gemini upstream", "gemini_connect_failed");
                    if recovery::connection_failure(&e) || e.is_timeout() {
                        response
                            .extensions_mut()
                            .insert(super::fallback::EligibleFailure);
                    }
                    return response;
                }
                if recovery::connection_failure(&e)
                    && recovery.retry(&proxy.config, &HeaderMap::new()).await
                {
                    continue;
                }
                return transport_error(
                    "Could not reach Gemini upstream; retry the request",
                    "gemini_connect_failed",
                );
            }
        };
        if !response.status().is_success() {
            let status = response.status();
            let headers = response.headers().clone();
            // Native status/code, rather than arbitrary error message matching.
            let bytes = match recovery.run(bounded_body(response, 1024 * 1024)).await {
                Ok(Ok(b)) => b,
                _ => {
                    return transport_error(
                        "Could not read Gemini error; retry the request",
                        "gemini_error_read_failed",
                    );
                }
            };
            let native: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let code = native
                .pointer("/error/status")
                .and_then(Value::as_str)
                .unwrap_or("gemini_http_error");
            let normalized = ResponseError::from_native(&native, Some(status.as_u16()));
            attempt.finish("http_error", Some(status.as_u16()), Some(code));
            if normalized.retryable && recovery.retry(&proxy.config, &headers).await {
                continue;
            }
            let mut response = (
                status,
                axum::Json(json!({"error":normalized.error,"gemini":native})),
            )
                .into_response();
            if let Some(delay) = headers.get(header::RETRY_AFTER) {
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, delay.clone());
            }
            response
                .extensions_mut()
                .insert(super::fallback::Upstream { gemini: true });
            return response;
        }
        attempt.finish("accepted", Some(response.status().as_u16()), None);
        break response;
    };
    let id = format!("{:032x}", rand::random::<u128>());
    if !converted.stream {
        let native = match bounded_body(upstream, MAX_NATIVE_RESPONSE)
            .await
            .and_then(|bytes| Ok(serde_json::from_slice::<Value>(&bytes)?))
        {
            Ok(v) => v,
            Err(_) => {
                return transport_error(
                    "Invalid Gemini JSON response; retry the request",
                    "gemini_response_read_failed",
                );
            }
        };
        return match convert_response(&native, &converted, &proxy.service.gemini.codec, &id) {
            Ok(response) => {
                let bytes = serde_json::to_vec(&response).unwrap();
                let mut usage = logs::UsageReader::new(false);
                if !proxy.fallback_attempt {
                    usage.feed(&bytes, &proxy.service.logs, proxy.log_id);
                    usage.finish(&proxy.service.logs, proxy.log_id);
                }
                axum::Json(response).into_response()
            }
            Err(e) => transport_error(&e.to_string(), "gemini_conversion_error"),
        };
    }
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.contains("text/event-stream") {
        return error(
            StatusCode::BAD_GATEWAY,
            "Gemini streaming endpoint did not return SSE",
        );
    }
    let body = Body::from_stream(async_stream::stream! {
        let mut parser=NativeSse::default();let mut converter=ResponseStream::new(converted,id);
        let mut usage=logs::UsageReader::new(true);
        let mut received = 0usize;
        let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // Keep the same read future alive across heartbeats. They must not
            // reset the upstream idle deadline or conceal a stalled Google stream.
            let read = upstream.chunk();
            tokio::pin!(read);
            let chunk = loop {
                tokio::select! {
                    result = &mut read => break result,
                    _ = heartbeat.tick() => yield Ok::<Bytes,std::io::Error>(Bytes::from_static(b": keepalive\n\n")),
                }
            };
            let (bytes,eof)=match chunk {
                Ok(Some(bytes))=>(bytes,false),Ok(None)=>(Bytes::new(),true),Err(_)=>{
                    proxy.service.logs.complete(proxy.log_id,"failed","transport",Some("gemini_stream_error"),0);
                    yield Ok::<Bytes,std::io::Error>(sse_bytes(&converter.fail("server_error", "Gemini stream interrupted; retry the request")));break;
                }
            };
            let converted=(||->Result<Vec<Value>> {
                received = received.saturating_add(bytes.len());
                anyhow::ensure!(received <= MAX_NATIVE_RESPONSE, "Gemini stream exceeds 64 MiB");
                let mut events=Vec::new();for native in parser.feed(&bytes,eof)?{
                    events.extend(converter.feed(&native)?);
                    if converter.is_finished() { break; }
                }
                if eof && !converter.is_finished(){events.extend(converter.finish(&proxy.service.gemini.codec)?);}Ok(events)
            })();
            match converted {
                Ok(events)=>for event in events {let bytes=sse_bytes(&event);if !proxy.fallback_attempt { usage.feed(&bytes,&proxy.service.logs,proxy.log_id); }yield Ok(bytes);},
                Err(cause)=>{
                    // Conversion errors contain structural diagnostics, never
                    // response bodies. Omit model-provided tool names as well.
                    let cause = cause.to_string();
                    let cause = if cause.starts_with("Gemini called undeclared tool ") {
                        "Gemini called an undeclared tool"
                    } else { &cause };
                    proxy.service.logs.update(proxy.log_id,"gemini_conversion_failed",json!({"reason":cause}), |_| true);
                    proxy.service.logs.complete(proxy.log_id,"failed","gemini_conversion",Some("gemini_conversion_error"),0);
                    yield Ok(sse_bytes(&converter.fail("server_error", &format!("Gemini stream conversion failed: {cause}; retry the request"))));
                    break;
                }
            }
            if eof || converter.is_finished(){if !proxy.fallback_attempt { usage.finish(&proxy.service.logs,proxy.log_id); }break;}
        }
    });
    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().unwrap());
    response
        .extensions_mut()
        .insert(super::fallback::Upstream { gemini: true });
    response
}

async fn credential_headers(proxy: &Proxy, config: &ProviderConfig) -> Result<HeaderMap> {
    if config.auth == Auth::Adc {
        return proxy.service.credentials.adc_headers().await;
    }
    let source = if config.auth == Auth::GcloudAdc {
        "sh://gcloud auth application-default print-access-token"
    } else {
        config.api_key.as_deref().unwrap_or("")
    };
    let value = proxy
        .service
        .credentials
        .resolve(source, Duration::from_secs(config.credential_cache_seconds))
        .await?;
    let mut headers = HeaderMap::new();
    if config.auth == Auth::ApiKey {
        headers.insert("x-goog-api-key", value);
    } else {
        let mut bearer = header::HeaderValue::from_str(&format!("Bearer {}", value.to_str()?))?;
        bearer.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, bearer);
    }
    Ok(headers)
}

pub(super) fn native_path(path: &str) -> bool {
    path == "/v1beta"
        || path.starts_with("/v1beta/")
        || path.starts_with("/v1/projects/")
        || path.starts_with("/v1beta1/")
        || (path.starts_with("/v1/models/") && path.contains(':'))
}

/// Native Gemini requests and responses remain byte-for-byte native. This
/// includes countTokens, cached content, files, media generation and future APIs.
pub(super) async fn forward_native(
    proxy: Arc<Proxy>,
    parts: axum::http::request::Parts,
    body: Body,
) -> Response {
    let Some(config) = &proxy.config.gemini else {
        return error(
            StatusCode::BAD_REQUEST,
            "Configure providers.gemini to proxy native Gemini requests",
        );
    };
    let credentials = match credential_headers(&proxy, config).await {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_GATEWAY, &e.to_string()),
    };
    let incoming = parts.uri.path();
    // Config can name either the Developer API version root or a complete
    // Vertex publisher root. Incoming full Vertex resource paths remain valid.
    let root = match reqwest::Url::parse(&config.upstream_url) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid Gemini root"),
    };
    let base = root.path().trim_end_matches('/');
    let relative = if let Some(rest) = incoming
        .strip_prefix(base)
        .filter(|r| !base.is_empty() && r.starts_with('/'))
    {
        rest.to_owned()
    } else if let Some((_, rest)) = incoming.split_once("/models/") {
        format!("/models/{rest}")
    } else if let Some(rest) = incoming.strip_prefix("/v1beta") {
        rest.to_owned()
    } else {
        return error(
            StatusCode::BAD_REQUEST,
            "Native Gemini path does not match the configured provider root",
        );
    };
    let mut path = relative;
    if let Some(resource) = path.strip_prefix("/models/") {
        let (model, suffix) = resource
            .split_once(':')
            .map(|(m, s)| (m, format!(":{s}")))
            .unwrap_or((resource, String::new()));
        let model = model.to_owned();
        let alias = proxy
            .config
            .aliases
            .iter()
            .find(|a| a.from == model || a.from == format!("models/{model}"));
        if let Some(target) = alias.and_then(|a| a.to.as_deref()) {
            let Some(target) = target.strip_prefix("gemini/") else {
                return error(
                    StatusCode::BAD_REQUEST,
                    "Native Gemini cross-provider rewrites require a gemini/model target",
                );
            };
            let target = target.strip_prefix("models/").unwrap_or(target);
            if config.endpoint(target, false).is_err() {
                return error(
                    StatusCode::BAD_REQUEST,
                    "Invalid native Gemini alias target",
                );
            }
            path = format!("/models/{target}{suffix}");
        }
        proxy.service.logs.route(
            proxy.log_id,
            Some(format!("gemini/{model}")),
            Some(format!(
                "gemini/{}",
                path.trim_start_matches("/models/")
                    .split(':')
                    .next()
                    .unwrap_or(&model)
            )),
            "gemini",
        );
    }
    let mut url = format!("{}{}", config.upstream_url.trim_end_matches('/'), path);
    if let Some(query) = parts.uri.query() {
        let retained: Vec<_> = url::form_urlencoded::parse(query.as_bytes())
            .filter(|(k, _)| k != "key" && k != "access_token")
            .collect();
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(retained)
            .finish();
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query);
        }
    }
    let mut headers = parts.headers;
    clean_headers(&mut headers);
    for name in [
        "host",
        "authorization",
        "x-goog-api-key",
        "x-api-key",
        "api-key",
        "cookie",
        "content-length",
        "openai-project",
        "openai-organization",
    ] {
        headers.remove(name);
    }
    headers.extend(credentials);
    let upstream = match proxy
        .client
        .request(parts.method, &url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await
    {
        Ok(v) => v,
        Err(_) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "Could not reach native Gemini upstream",
            );
        }
    };
    let status = upstream.status();
    let mut headers = upstream.headers().clone();
    clean_headers(&mut headers);
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, routing::post};
    use std::sync::atomic::{AtomicUsize, Ordering};
    pub(super) async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        (
            url,
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
        )
    }
    pub(super) fn provider(url: &str, auth: &str) -> ProviderConfig {
        serde_json::from_value(json!({"upstream_url":url,"auth":auth,"api_key":"synthetic-gemini"}))
            .unwrap()
    }
    #[test]
    fn native_sse_handles_every_byte_boundary_unicode_crlf_and_multiline() {
        let bytes="event: chunk\r\ndata: {\"candidates\":\r\ndata: [{\"content\":{\"parts\":[{\"text\":\"🌍\"}]}}]}\r\n\r\ndata: {\"usageMetadata\":{\"promptTokenCount\":2}}\n\n".as_bytes();
        let mut parser = NativeSse::default();
        let mut events = Vec::new();
        for byte in bytes {
            events.extend(parser.feed(&[*byte], false).unwrap());
        }
        events.extend(parser.feed(&[], true).unwrap());
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0]["candidates"][0]["content"]["parts"][0]["text"],
            "🌍"
        );
        assert!(
            NativeSse::default()
                .feed(b"data: invalid\n\n", false)
                .is_err()
        );
    }
    #[test]
    fn codec_key_survives_restart_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        let first = GeminiState::new(Some(&config)).unwrap();
        let provider = provider("http://localhost", "bearer");
        let request = convert_request(
            &json!({"model":"gemini/test","input":"hi"}),
            &provider,
            &first.codec,
        )
        .unwrap();
        let response=convert_response(&json!({"candidates":[{"content":{"role":"model","parts":[{"text":"hello","thoughtSignature":"synthetic"}]},"finishReason":"STOP"}]}),&request,&first.codec,"restart").unwrap();
        let second = GeminiState::new(Some(&config)).unwrap();
        assert!(
            convert_request(
                &json!({"model":"gemini/test","input":response["output"]}),
                &provider,
                &second.codec
            )
            .is_ok()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(config.with_extension("gemini-reasoning-key"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[tokio::test]
    async fn converts_aliases_strips_client_credentials_and_retries_capacity() {
        async fn upstream(
            State(count): State<Arc<AtomicUsize>>,
            headers: HeaderMap,
            axum::Json(body): axum::Json<Value>,
        ) -> Response {
            assert_eq!(headers["authorization"], "Bearer synthetic-gemini");
            assert!(!headers.contains_key("cookie"));
            assert!(!headers.contains_key("x-goog-api-key"));
            assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(
                        json!({"error":{"status":"UNAVAILABLE","message":"synthetic capacity"}}),
                    ),
                )
                    .into_response();
            }
            axum::Json(json!({"candidates":[{"content":{"role":"model","parts":[{"text":"hello","thoughtSignature":"signed-text"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":3,"thoughtsTokenCount":4,"totalTokenCount":9}})).into_response()
        }
        let count = Arc::new(AtomicUsize::new(0));
        let (url, task) = serve(
            Router::new()
                .route(
                    "/models/gemini-3.1-pro-preview:generateContent",
                    post(upstream),
                )
                .with_state(count.clone()),
        )
        .await;
        let mut config = Config::test_fixture();
        config.gemini = Some(provider(&url, "bearer"));
        config.aliases = vec![crate::config::Alias {
            from: "test-alias".into(),
            to: Some("gemini/models/gemini-3.1-pro-preview".into()),
            reasoning: None,
            api_key: None,
            reasoning_routes: Default::default(),
        }];
        config.retry.initial_delay_ms = 0;
        config.retry.max_delay_ms = 0;
        let (url, proxy) = serve(super::super::router(config).unwrap()).await;
        let response = reqwest::Client::new()
            .post(format!("{url}/v1/responses"))
            .bearer_auth("client-secret")
            .header("x-goog-api-key", "client-key")
            .header("cookie", "client-cookie")
            .json(&json!({"model":"test-alias","input":"hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["model"], "gemini/models/gemini-3.1-pro-preview");
        assert_eq!(body["usage"]["output_tokens"], 7);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert!(
            body["output"][0]["encrypted_content"]
                .as_str()
                .unwrap()
                .starts_with("hey_gemini_v1.")
        );
        task.abort();
        proxy.abort();
    }
    #[tokio::test]
    async fn native_gemini_passthrough_preserves_body_and_strips_query_keys() {
        async fn upstream(request: Request) -> Response {
            assert_eq!(
                request.uri().path(),
                "/models/gemini-3.1-pro-preview:generateContent"
            );
            assert_eq!(request.uri().query(), Some("alt=sse"));
            assert_eq!(request.headers()["x-goog-api-key"], "synthetic-gemini");
            assert!(!request.headers().contains_key("authorization"));
            let bytes = axum::body::to_bytes(request.into_body(), 1024)
                .await
                .unwrap();
            assert_eq!(bytes, b"{ \"contents\": [] }"[..]);
            (
                [(header::CONTENT_TYPE, "application/json")],
                "{ \"native\": true }",
            )
                .into_response()
        }
        let (upstream_url, task) = serve(Router::new().fallback(upstream)).await;
        let config = Config {
            gemini: Some(provider(&upstream_url, "api_key")),
            ..Config::test_fixture()
        };
        let (url, proxy) = serve(super::super::router(config).unwrap()).await;
        let response=reqwest::Client::new().post(format!("{url}/v1beta/models/gemini-3.1-pro-preview:generateContent?key=CLIENT_SECRET&alt=sse&access_token=CLIENT_TOKEN")).header("authorization","Bearer client").body("{ \"contents\": [] }").send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "{ \"native\": true }");
        task.abort();
        proxy.abort();
    }
    #[tokio::test]
    async fn openai_stays_default_with_both_providers_configured() {
        async fn upstream(headers: HeaderMap, axum::Json(body): axum::Json<Value>) -> Response {
            assert_eq!(headers["authorization"], "Bearer something");
            assert_eq!(body["model"], "ordinary-model");
            axum::Json(json!({"id":"openai-response","output":[]})).into_response()
        }
        let (upstream_url, task) =
            serve(Router::new().route("/v1/responses", post(upstream))).await;
        let config = Config {
            upstream_url,
            gemini: Some(provider("http://127.0.0.1:1", "bearer")),
            ..Config::test_fixture()
        };
        let (url, proxy) = serve(super::super::router(config).unwrap()).await;
        for model in ["ordinary-model", "openai/ordinary-model"] {
            let response = reqwest::Client::new()
                .post(format!("{url}/v1/responses"))
                .json(&json!({"model":model,"input":"hi"}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.json::<Value>().await.unwrap()["id"],
                "openai-response"
            );
        }
        task.abort();
        proxy.abort();
    }
    #[tokio::test]
    async fn stream_is_incremental_and_records_terminal_usage() {
        async fn upstream() -> Response {
            let body = Body::from_stream(async_stream::stream! {
                yield Ok::<_,std::io::Error>(Bytes::from_static(b"data: {\"candidates\":[{\"index\":0,\"content\":{\"parts\":[{\"text\":\"early\"}]}}]}\n\n"));
                tokio::time::sleep(Duration::from_millis(150)).await;
                yield Ok(Bytes::from_static(b"data: {\"candidates\":[{\"index\":0,\"finishReason\":\"STOP\"}]}\n\ndata: {\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":2,\"thoughtsTokenCount\":3,\"totalTokenCount\":6}}\n\n"));
            });
            let mut response = Response::new(body);
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
            response
        }
        let (upstream_url, task) = serve(Router::new().fallback(upstream)).await;
        let config = Config {
            gemini: Some(provider(&upstream_url, "bearer")),
            ..Config::test_fixture()
        };
        let (url, proxy) = serve(super::super::router(config).unwrap()).await;
        let mut response = reqwest::Client::new()
            .post(format!("{url}/v1/responses"))
            .json(&json!({"model":"gemini/test","input":"hi","stream":true}))
            .send()
            .await
            .unwrap();
        let early = tokio::time::timeout(Duration::from_millis(100), response.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let mut text = String::from_utf8(early.to_vec()).unwrap();
        assert!(text.contains("response.created"));
        while let Some(bytes) = response.chunk().await.unwrap() {
            text.push_str(std::str::from_utf8(&bytes).unwrap());
        }
        assert!(text.contains("response.output_text.delta"));
        assert!(text.contains("response.completed"));
        assert!(text.contains("\"output_tokens\":5"));
        let logs = reqwest::get(format!("{url}/logs/api"))
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(logs["entries"][0]["output_tokens"], 5);
        task.abort();
        proxy.abort();
    }
}

#[cfg(test)]
mod hardening_tests;
