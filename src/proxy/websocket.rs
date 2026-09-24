use super::*;
use futures_util::SinkExt;
use std::collections::VecDeque;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::server::create_response,
        protocol::{CloseFrame, Role, WebSocketConfig, frame::coding::CloseCode},
    },
};

type ClientSocket = WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>;
type UpstreamSocket = WebSocketStream<reqwest::Upgraded>;
const QUEUE_BYTES: usize = 1024 * 1024;
const QUEUE_MESSAGES: usize = 64;

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(16 * 1024 * 1024))
        .max_frame_size(Some(16 * 1024 * 1024))
}

// Only model-bearing API envelopes are inspected, never arbitrary input/tool data.
fn rewrite_message(
    config: &Config,
    path: &str,
    message: Message,
) -> Result<(Message, Option<String>), &'static str> {
    let Message::Text(text) = &message else {
        return Ok((message, None));
    };
    let Ok(mut value) = serde_json::from_str::<Value>(text) else {
        return Ok((message, None));
    };
    let pointer = if value.get("model").and_then(Value::as_str).is_some() {
        ""
    } else if value
        .pointer("/session/model")
        .and_then(Value::as_str)
        .is_some()
    {
        "/session"
    } else if value
        .pointer("/response/model")
        .and_then(Value::as_str)
        .is_some()
    {
        "/response"
    } else {
        // Follow-up response.create messages can inherit the model from the socket.
        if config.skip_blocked_security_work
            && value["type"] == "response.create"
            && path.trim_end_matches('/') == "/v1/responses"
        {
            let envelope = if value["response"].is_object() {
                &mut value["response"]
            } else {
                &mut value
            };
            if envelope.get("input").is_none() {
                envelope["input"] = json!([]);
            }
            if guidance::apply(path, envelope) {
                return Ok((Message::Text(value.to_string().into()), None));
            }
        }
        return Ok((message, None));
    };
    let envelope = value.pointer(pointer).unwrap();
    let (rewritten, project) = rewrite(
        config,
        path,
        Bytes::from(serde_json::to_vec(envelope).unwrap()),
    )?;
    *value.pointer_mut(pointer).unwrap() = serde_json::from_slice(&rewritten).unwrap();
    Ok((
        Message::Text(value.to_string().into()),
        Some(project.to_owned()),
    ))
}

async fn event(client: &mut ClientSocket, code: &str, message: &str) {
    let _ = client
        .send(Message::Text(
            json!({"type":"error","status":503,"error":{"type":"proxy_error","code":code,"message":message}})
                .to_string()
                .into(),
        ))
        .await;
}

async fn fail(client: &mut ClientSocket, code: &str, message: &str) {
    event(client, code, message).await;
    let _ = client
        .close(Some(CloseFrame {
            code: CloseCode::Error,
            reason: code.to_owned().into(),
        }))
        .await;
}

struct Queued {
    message: Message,
    guard: Option<logs::RequestGuard>,
}
struct PendingQueue {
    items: VecDeque<Queued>,
    store: Arc<logs::Store>,
    config: Arc<Config>,
    path: String,
    last_requested: Option<String>,
    last_routed: Option<String>,
    last_project: Option<String>,
}
impl PendingQueue {
    fn new(store: Arc<logs::Store>, config: Arc<Config>, path: String) -> Self {
        Self {
            items: VecDeque::new(),
            store,
            config,
            path,
            last_requested: None,
            last_routed: None,
            last_project: None,
        }
    }
    fn pop_front(&mut self) -> Option<Queued> {
        self.items.pop_front()
    }
    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    fn fail_all(&mut self, code: &str) {
        for item in &mut self.items {
            if let Some(guard) = &mut item.guard {
                guard.finish("failed", "websocket_queue", Some(code));
            }
        }
    }
}
fn queue_message(queue: &mut PendingQueue, message: Message) -> Result<(), &'static str> {
    let value = message
        .to_text()
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let incoming = message
        .to_text()
        .ok()
        .and_then(|text| logs::json_model(text.as_bytes()));
    let create = value
        .as_ref()
        .is_some_and(|v| v["type"] == "response.create");
    let mut guard = if incoming.is_some() || create {
        let id = queue.store.begin("SEND", &queue.path, "WebSocket");
        if let Ok(text) = message.to_text() {
            queue
                .store
                .routing_decision(id, &queue.config, &queue.path, text.as_bytes());
        }
        let requested = incoming.or_else(|| queue.last_requested.clone());
        let (outgoing, project) = rewrite_message(&queue.config, &queue.path, message.clone())
            .map(|(rewritten, project)| {
                (
                    rewritten
                        .to_text()
                        .ok()
                        .and_then(|text| logs::json_model(text.as_bytes()))
                        .or_else(|| queue.last_routed.clone()),
                    project,
                )
            })
            .unwrap_or((None, None));
        let project = project.or_else(|| queue.last_project.clone());
        queue.last_project = project.clone();
        queue.last_requested = requested.clone();
        queue.last_routed = outgoing.clone();
        queue.store.route(
            id,
            requested,
            outgoing,
            project.as_deref().unwrap_or(&queue.config.default.api_key),
        );
        queue.store.update(id, "queued", json!({}), |entry| {
            entry.request_bytes = message.len() as u64;
            entry.streaming = create;
            true
        });
        Some(logs::RequestGuard::new(queue.store.clone(), id))
    } else {
        None
    };
    if queue.items.len() >= QUEUE_MESSAGES
        || queue
            .items
            .iter()
            .map(|item| item.message.len())
            .sum::<usize>()
            + message.len()
            > QUEUE_BYTES
    {
        if let Some(guard) = &mut guard {
            guard.finish("failed", "websocket_queue", Some("proxy_buffer_limit"));
        }
        return Err("WebSocket pending messages exceed 64 messages or 1 MiB");
    }
    queue.items.push_back(Queued { message, guard });
    Ok(())
}

#[allow(clippy::result_large_err)] // Preserve an upstream HTTP error response for eager handshakes.
async fn connect_once(
    proxy: &Proxy,
    url: &str,
    headers: &HeaderMap,
    project: &str,
) -> Result<reqwest::Response, Response> {
    let mut headers = headers.clone();
    let source = proxy.config.api_keys.get(project).ok_or_else(|| error(
        StatusCode::SERVICE_UNAVAILABLE,
        "No OpenAI credential configured; add providers.openai.api_keys.default to your proxy config",
    ))?;
    let resolved = proxy
        .service
        .credentials
        .resolve(
            source,
            Duration::from_secs(proxy.config.credential_cache_seconds),
        )
        .await
        .map_err(|e| error(StatusCode::BAD_GATEWAY, &e.to_string()))?;
    let mut key = header::HeaderValue::from_str(&format!(
        "Bearer {}",
        resolved.to_str().expect("validated credential")
    ))
    .expect("validated credential");
    key.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, key);
    match tokio::time::timeout(
        Duration::from_secs(20),
        proxy.client.get(url).headers(headers).send(),
    )
    .await
    {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(err)) => {
            let cause = describe_send_error(&err);
            let message = diagnose_unreachable(proxy, url, &cause).await;
            Err(error(StatusCode::BAD_GATEWAY, &message))
        }
        Err(_) => {
            let message = diagnose_unreachable(proxy, url, "handshake timed out").await;
            Err(error(StatusCode::BAD_GATEWAY, &message))
        }
    }
}

#[allow(clippy::result_large_err)] // Preserve an upstream HTTP error response for eager handshakes.
async fn connect(
    proxy: &Proxy,
    url: &str,
    headers: &HeaderMap,
    project: &str,
) -> Result<reqwest::Response, Response> {
    let mut recovery = Recovery::new(&proxy.config);
    loop {
        let mut attempt = logs::Attempt::new(
            proxy.service.logs.clone(),
            proxy.log_id,
            "websocket_handshake",
        );
        let mut response = match recovery
            .run(connect_once(proxy, url, headers, project))
            .await
        {
            Ok(Ok(response)) => response,
            Err(()) => {
                attempt.finish("timeout", None, Some("proxy_recovery_timeout"));
                return Err(recovery::timeout_response());
            }
            Ok(Err(response)) => {
                attempt.finish(
                    "connection_error",
                    Some(response.status().as_u16()),
                    Some("handshake_failed"),
                );
                if recovery.retry(&proxy.config, &HeaderMap::new()).await {
                    continue;
                }
                return Err(response);
            }
        };
        attempt.finish(
            if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                "connected"
            } else {
                "refused"
            },
            Some(response.status().as_u16()),
            None,
        );
        if response.status() == StatusCode::SWITCHING_PROTOCOLS {
            let expected = tokio_tungstenite::tungstenite::handshake::derive_accept_key(
                headers["sec-websocket-key"].as_bytes(),
            );
            if response
                .headers()
                .get("sec-websocket-accept")
                .and_then(|v| v.to_str().ok())
                != Some(expected.as_str())
            {
                return Err(error(
                    StatusCode::BAD_GATEWAY,
                    "Invalid upstream WebSocket handshake",
                ));
            }
            if response.headers().contains_key("sec-websocket-extensions") {
                return Err(error(
                    StatusCode::BAD_GATEWAY,
                    "Unrequested WebSocket extension",
                ));
            }
            if let Some(selected) = response.headers().get("sec-websocket-protocol") {
                let offered = headers
                    .get("sec-websocket-protocol")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if !offered
                    .split(',')
                    .any(|p| p.trim().as_bytes() == selected.as_bytes())
                {
                    return Err(error(
                        StatusCode::BAD_GATEWAY,
                        "Unrequested WebSocket subprotocol",
                    ));
                }
            }
            return Ok(response);
        }
        let mut status = response.status();
        let peer = response.remote_addr();
        let mut response_headers = response.headers().clone();
        let mut prefix = Vec::new();
        let mut complete = false;
        while prefix.len() < 64 * 1024 {
            match recovery.run(response.chunk()).await {
                Ok(Ok(Some(chunk))) => prefix.extend_from_slice(&chunk),
                Ok(Ok(None)) => {
                    complete = true;
                    break;
                }
                Err(()) => return Err(recovery::timeout_response()),
                Ok(Err(_)) => break,
            }
        }
        if capacity_error(status, &prefix) && recovery.retry(&proxy.config, &response_headers).await
        {
            continue;
        }
        if complete && let Some(rewritten) = capacity::http(&prefix) {
            prefix = rewritten;
            status = StatusCode::INTERNAL_SERVER_ERROR;
            response_headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            response_headers.insert(header::CONTENT_LENGTH, prefix.len().into());
        }
        clean_headers(&mut response_headers);
        if ip_not_authorized(status, &prefix) {
            prefix = annotate_ip_error(proxy, url, peer, prefix, complete).await;
            if complete {
                response_headers.insert(header::CONTENT_LENGTH, prefix.len().into());
            }
        }
        let stream = async_stream::stream! {
            yield Ok::<_, reqwest::Error>(Bytes::from(prefix));
            while let Some(chunk) = response.chunk().await? { yield Ok(chunk); }
        };
        let mut result = Response::new(Body::from_stream(stream));
        *result.status_mut() = status;
        *result.headers_mut() = response_headers;
        return Err(result);
    }
}

// Read/control the local socket during reconnect rather than accumulating unbounded data.
struct WsRoute<'a> {
    url: &'a str,
    headers: &'a HeaderMap,
    project: &'a str,
    protocol: Option<&'a header::HeaderValue>,
}

async fn connect_queued(
    proxy: &Proxy,
    route: WsRoute<'_>,
    client: &mut ClientSocket,
    queue: &mut PendingQueue,
    delay: Duration,
    recovery: Recovery,
) -> Result<UpstreamSocket, ()> {
    let connection = async {
        // Include the initial reconnect delay in the same elapsed-time ceiling.
        match recovery
            .run(async {
                tokio::time::sleep(delay).await;
                connect(proxy, route.url, route.headers, route.project).await
            })
            .await
        {
            Ok(result) => result,
            Err(()) => Err(recovery::timeout_response()),
        }
    };
    tokio::pin!(connection);
    loop {
        tokio::select! {
            result = &mut connection => {
                match result {
                    Ok(response) if response.headers().get("sec-websocket-protocol") != route.protocol => {
                        queue.fail_all("proxy_subprotocol_changed");
                        fail(client, "proxy_subprotocol_changed", "Upstream selected a different WebSocket subprotocol; open a new connection").await;
                        return Err(());
                    }
                    Ok(response) => match response.upgrade().await {
                        Ok(socket) => return Ok(WebSocketStream::from_raw_socket(socket, Role::Client, Some(ws_config())).await),
                        Err(_) => { queue.fail_all("proxy_connect_failed"); fail(client, "proxy_connect_failed", "Could not upgrade upstream connection").await; return Err(()); }
                    },
                    Err(response) if response.extensions().get::<recovery::Timeout>().is_some() => {
                        queue.fail_all("proxy_recovery_timeout");
                        recovery_timeout(client).await;
                        return Err(());
                    }
                    Err(response) => {
                        queue.fail_all("proxy_connect_failed");
                        fail(client, "proxy_connect_failed", &format!("Upstream handshake failed with HTTP {}", response.status().as_u16())).await;
                        return Err(());
                    }
                }
            }
            message = client.next() => {
                match message {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => { let _ = client.flush().await; return Err(()); }
                    Some(Ok(Message::Ping(_))) => { let _ = client.flush().await; }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(message)) => if let Err(message) = queue_message(queue, message) {
                        fail(client, "proxy_buffer_limit", message).await; return Err(());
                    }
                }
            }
        }
    }
}

// While waiting for a rejected request to recover, keep servicing local control frames
// and stop immediately when the client cancels. Only unsent messages enter this queue.
async fn while_connected<T>(
    future: impl std::future::Future<Output = T>,
    client: &mut ClientSocket,
    queue: &mut PendingQueue,
) -> Result<T, ()> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return Ok(result),
            message = client.next() => match message {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                    let _ = client.flush().await; return Err(());
                }
                Some(Ok(Message::Ping(_))) => { let _ = client.flush().await; }
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(message)) => if let Err(message) = queue_message(queue, message) {
                    fail(client, "proxy_buffer_limit", message).await; return Err(());
                }
            }
        }
    }
}

enum PreludeExit {
    Closed,
    Disconnected,
}

async fn recovery_timeout(client: &mut ClientSocket) {
    let _ = client.send(Message::Text(json!({"type":"error","status":503,"error":{
        "type":"server_error","code":"server_error","proxy_code":"proxy_recovery_timeout",
        "message":"hey-proxy internal recovery time budget exhausted before output; retry the request."
    }}).to_string().into())).await;
    let _ = client
        .close(Some(CloseFrame {
            code: CloseCode::Error,
            reason: "proxy_recovery_timeout".into(),
        }))
        .await;
}

// Serialize the initial prelude only when there is a single in-flight Responses
// request. Rejections are retried on the SAME socket to preserve connection context.
// Nothing is replayed after output, an ambiguous disconnect, or a failed write.
async fn recover_response(
    proxy: &Proxy,
    client: &mut ClientSocket,
    upstream: &mut UpstreamSocket,
    queue: &mut PendingQueue,
    sent: Message,
    id: u64,
    mut recovery: Recovery,
) -> Result<VecDeque<Message>, PreludeExit> {
    let mut buffered = VecDeque::new();
    let mut bytes = 0;
    let mut response_id: Option<String> = None;
    loop {
        let message = match while_connected(recovery.run(upstream.next()), client, queue).await {
            Ok(Ok(Some(Ok(message)))) => message,
            Err(()) => return Err(PreludeExit::Closed),
            Ok(Err(())) => {
                proxy.service.logs.complete(
                    id,
                    "failed",
                    "recovery",
                    Some("proxy_recovery_timeout"),
                    bytes as u64,
                );
                recovery_timeout(client).await;
                return Err(PreludeExit::Closed);
            }
            _ => return Err(PreludeExit::Disconnected),
        };
        match message {
            Message::Ping(_) => {
                let _ = upstream.flush().await;
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => {
                buffered.push_back(message);
                return Ok(buffered);
            }
            _ => {}
        }
        let mut decision = message
            .to_text()
            .ok()
            .map(|s| capacity::prelude_event(s.as_bytes()))
            .unwrap_or(capacity::Prelude::Started);
        if let Some(value) = message
            .to_text()
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            && let Some(incoming_id) = value.pointer("/response/id").and_then(Value::as_str)
        {
            if response_id.as_ref().is_some_and(|id| id != incoming_id) {
                decision = capacity::Prelude::Started;
            } else {
                response_id = Some(incoming_id.to_owned());
            }
        }
        bytes += message.len();
        buffered.push_back(message);
        // Release a large prelude unchanged instead of retaining unbounded messages.
        if bytes >= QUEUE_BYTES || buffered.len() >= QUEUE_MESSAGES {
            return Ok(buffered);
        }
        match decision {
            capacity::Prelude::Started => return Ok(buffered),
            capacity::Prelude::Waiting => continue,
            capacity::Prelude::Refused => {}
        }
        proxy.service.logs.update(
            id,
            "capacity_refused",
            json!({"response_id":response_id}),
            |_| true,
        );
        match while_connected(
            recovery.retry(&proxy.config, &HeaderMap::new()),
            client,
            queue,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => return Ok(buffered), // forward the final refusal using normal translation
            Err(()) => return Err(PreludeExit::Closed),
        }
        proxy.service.logs.retries(id, recovery.retries);
        let mut attempt =
            logs::Attempt::new(proxy.service.logs.clone(), id, "websocket_recovery_send");
        match while_connected(recovery.run(upstream.send(sent.clone())), client, queue).await {
            Ok(Ok(Ok(()))) => {
                attempt.finish("sent", Some(101), None);
            }
            Ok(Err(())) => {
                proxy.service.logs.complete(
                    id,
                    "failed",
                    "recovery",
                    Some("proxy_recovery_timeout"),
                    bytes as u64,
                );
                recovery_timeout(client).await;
                return Err(PreludeExit::Closed);
            }
            Err(()) => return Err(PreludeExit::Closed),
            _ => return Err(PreludeExit::Disconnected),
        }
        buffered.clear();
        bytes = 0;
        response_id = None;
    }
}

pub(super) async fn forward(proxy: Arc<Proxy>, mut request: Request) -> Response {
    let mut handshake = axum::http::Request::new(());
    *handshake.method_mut() = request.method().clone();
    *handshake.uri_mut() = request.uri().clone();
    *handshake.version_mut() = request.version();
    *handshake.headers_mut() = request.headers().clone();
    let mut response = match create_response(&handshake) {
        Ok(response) => response.map(|_| Body::empty()),
        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid WebSocket handshake"),
    };
    let mut project = proxy.config.default.api_key.clone();
    let path = request.uri().path().to_owned();
    let path_query = rewrite_uri(&proxy.config, request.uri(), &mut project);
    proxy.service.logs.route(
        proxy.log_id,
        logs::uri_model(request.uri()),
        logs::uri_model(&path_query.parse().unwrap()),
        &project,
    );
    // Forward a host's 426 before upgrading the local client. A deferred client
    // handshake would hide the host's HTTP/SSE fallback requirement behind 101.
    let deferred = proxy.config.mode != Mode::Client
        && path.trim_end_matches('/').ends_with("/responses")
        && !request
            .uri()
            .query()
            .unwrap_or("")
            .split('&')
            .any(|p| p.split('=').next() == Some("model"));
    let url = format!(
        "{}{}",
        proxy.config.upstream_url.trim_end_matches('/'),
        path_query
    );
    let mut headers = request.headers().clone();
    clean_headers(&mut headers);
    for name in [
        "host",
        "content-length",
        "authorization",
        "x-api-key",
        "api-key",
        "cookie",
        "openai-organization",
        "openai-project",
        "sec-websocket-extensions",
    ] {
        headers.remove(name);
    }
    let protocols = headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|p| !p.is_empty() && !p.starts_with("openai-insecure-api-key."))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    headers.remove("sec-websocket-protocol");
    if let Some(protocol) = protocols.first() {
        headers.insert(
            "sec-websocket-protocol",
            protocols.join(", ").parse().unwrap(),
        );
        response
            .headers_mut()
            .insert("sec-websocket-protocol", protocol.parse().unwrap());
    }
    headers.insert(header::CONNECTION, "Upgrade".parse().unwrap());
    headers.insert(header::UPGRADE, "websocket".parse().unwrap());
    let upstream = if deferred {
        None
    } else {
        match connect(&proxy, &url, &headers, &project).await {
            Ok(upstream) => {
                response.headers_mut().remove("sec-websocket-protocol");
                if let Some(protocol) = upstream.headers().get("sec-websocket-protocol") {
                    response
                        .headers_mut()
                        .insert("sec-websocket-protocol", protocol.clone());
                }
                Some(upstream)
            }
            Err(response) => return response,
        }
    };
    let negotiated_protocol = response.headers().get("sec-websocket-protocol").cloned();
    let upgrade = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let Ok(socket) = upgrade.await else {
            return;
        };
        let mut client = WebSocketStream::from_raw_socket(
            hyper_util::rt::TokioIo::new(socket),
            Role::Server,
            Some(ws_config()),
        )
        .await;
        let mut queue = PendingQueue::new(
            proxy.service.logs.clone(),
            proxy.config.clone(),
            path.clone(),
        );
        if deferred {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                match tokio::time::timeout_at(deadline, client.next()).await {
                    Ok(Some(Ok(Message::Ping(_)))) => {
                        let _ = client.flush().await;
                    }
                    Ok(Some(Ok(Message::Pong(_)))) => {}
                    Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => {
                        let _ = client.flush().await;
                        return;
                    }
                    Err(_) => {
                        fail(
                            &mut client,
                            "proxy_model_timeout",
                            "Send a model-bearing message within 30 seconds",
                        )
                        .await;
                        return;
                    }
                    Ok(Some(Ok(message))) => {
                        if let Err(message) = queue_message(&mut queue, message.clone()) {
                            fail(&mut client, "proxy_buffer_limit", message).await;
                            return;
                        }
                        let selected = match rewrite_message(&proxy.config, &path, message.clone())
                        {
                            Ok((_, selected)) => selected,
                            Err(message) => {
                                queue.fail_all("proxy_invalid_message");
                                fail(&mut client, "proxy_invalid_message", message).await;
                                return;
                            }
                        };
                        if let Some(selected) = selected {
                            project = selected;
                            break;
                        }
                    }
                }
            }
        }
        // The first queued request's deadline starts before its deferred handshake,
        // and is carried into prelude recovery rather than starting another window.
        let mut queued_recovery = deferred.then(|| Recovery::new(&proxy.config));
        let mut upstream = match upstream {
            Some(response) => match response.upgrade().await {
                Ok(socket) => {
                    WebSocketStream::from_raw_socket(socket, Role::Client, Some(ws_config())).await
                }
                Err(_) => {
                    fail(
                        &mut client,
                        "proxy_connect_failed",
                        "Could not upgrade upstream connection",
                    )
                    .await;
                    return;
                }
            },
            None => match connect_queued(
                &proxy,
                WsRoute {
                    url: &url,
                    headers: &headers,
                    project: &project,
                    protocol: negotiated_protocol.as_ref(),
                },
                &mut client,
                &mut queue,
                Duration::ZERO,
                queued_recovery.as_ref().unwrap().clone(),
            )
            .await
            {
                Ok(socket) => socket,
                Err(()) => return,
            },
        };
        // Count mode retains the lifetime reconnect cap; timed mode budgets each outage.
        let mut recoveries = 0;
        let mut tracking = logs::WebSocketTracker::new(proxy.service.logs.clone());
        let mut upstream_queue = VecDeque::new();
        loop {
            let disconnected = loop {
                if upstream_queue.is_empty()
                    && let Some(Queued { message, mut guard }) = queue.pop_front()
                {
                    let (message, selected) = match rewrite_message(&proxy.config, &path, message) {
                        Ok(result) => result,
                        Err(message) => {
                            if let Some(guard) = &mut guard {
                                guard.finish("failed", "validation", Some("proxy_invalid_message"));
                            }
                            fail(&mut client, "proxy_invalid_message", message).await;
                            return;
                        }
                    };
                    if selected.is_some_and(|selected| {
                        proxy.config.api_keys.get(&selected) != proxy.config.api_keys.get(&project)
                    }) {
                        if let Some(guard) = &mut guard {
                            guard.finish("failed", "validation", Some("proxy_api_key_change"));
                        }
                        fail(&mut client, "proxy_api_key_change", "This model requires a different API key; open a new WebSocket connection").await;
                        let _ = upstream.close(None).await;
                        return;
                    }
                    let request_value = message
                        .to_text()
                        .ok()
                        .and_then(|text| serde_json::from_str::<Value>(text).ok());
                    let response_create = request_value
                        .as_ref()
                        .is_some_and(|v| v["type"] == "response.create");
                    if let Some(mut send_guard) = guard {
                        let id = send_guard.id();
                        if request_value.as_ref().is_some_and(|value| {
                            guidance::present(value) || guidance::present(&value["response"])
                        }) {
                            proxy.service.logs.update(
                                id,
                                "guidance_applied",
                                json!({"policy":"skip_blocked_security_work"}),
                                |entry| {
                                    entry.security_guidance = true;
                                    true
                                },
                            );
                        }
                        let request_recovery = queued_recovery
                            .take()
                            .unwrap_or_else(|| Recovery::new(&proxy.config));
                        let recover = response_create
                            && path.trim_end_matches('/').ends_with("/responses")
                            && request_recovery.timed()
                            && message.len() <= QUEUE_BYTES
                            && tracking.is_empty();
                        let mut attempt =
                            logs::Attempt::new(proxy.service.logs.clone(), id, "websocket_send");
                        match request_recovery.run(upstream.send(message.clone())).await {
                            Ok(Ok(())) => {}
                            Err(()) => {
                                attempt.finish("timeout", None, Some("proxy_recovery_timeout"));
                                send_guard.finish(
                                    "failed",
                                    "recovery",
                                    Some("proxy_recovery_timeout"),
                                );
                                recovery_timeout(&mut client).await;
                                return;
                            }
                            Ok(Err(_)) => {
                                proxy.service.logs.finish(id, 502);
                                attempt.finish(
                                    "send_failed",
                                    Some(502),
                                    Some("upstream_send_failed"),
                                );
                                send_guard.finish(
                                    "failed",
                                    "transport",
                                    Some("upstream_send_failed"),
                                );
                                break true;
                            }
                        }
                        proxy.service.logs.finish(id, 101);
                        attempt.finish("sent", Some(101), None);
                        if response_create {
                            tracking.begin(send_guard);
                        } else {
                            send_guard.finish("succeeded", "websocket_send", None);
                        }
                        if recover {
                            match recover_response(
                                &proxy,
                                &mut client,
                                &mut upstream,
                                &mut queue,
                                message,
                                id,
                                request_recovery,
                            )
                            .await
                            {
                                Ok(messages) => upstream_queue.extend(messages),
                                Err(PreludeExit::Closed) => return,
                                Err(PreludeExit::Disconnected) => break true,
                            }
                        }
                        continue;
                    }
                    // A failed write may have partially reached upstream: never replay it.
                    if upstream.send(message).await.is_err() {
                        break true;
                    }
                    continue;
                }
                tokio::select! {
                    message = client.next() => match message {
                        Some(Ok(Message::Close(frame))) => { let _ = client.flush().await; let _ = upstream.close(frame).await; return; }
                        None | Some(Err(_)) => { let _ = upstream.close(None).await; return; }
                        Some(Ok(Message::Ping(_))) => { let _ = client.flush().await; }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(message)) => if let Err(message)=queue_message(&mut queue,message) {fail(&mut client,"proxy_buffer_limit",message).await;return;},
                    },
                    message = async {
                        if let Some(message) = upstream_queue.pop_front() { Some(Ok(message)) }
                        else { upstream.next().await }
                    } => match message {
                        Some(Ok(Message::Close(frame))) if frame.as_ref().is_none_or(|f| matches!(f.code, CloseCode::Normal)) => {
                            tracking.fail_all("upstream_closed_before_completion"); let _ = upstream.flush().await; let _ = client.close(frame).await; return;
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break true,
                        Some(Ok(Message::Ping(_))) => { let _ = upstream.flush().await; }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(message)) => {
                            if let Some(value) = message.to_text().ok().and_then(|text| serde_json::from_str::<Value>(text).ok()) {
                                tracking.observe(&value,message.len());
                            }
                            let message = match &message {
                                Message::Text(text) => capacity::event(text.as_bytes())
                                    .map(|bytes| Message::Text(String::from_utf8(bytes).expect("serialized JSON").into()))
                                    .unwrap_or(message),
                                _ => message,
                            };
                            if client.send(message).await.is_err() { return; }
                        },
                    }
                }
            };
            if disconnected {
                tracking.fail_all("upstream_disconnected");
                upstream_queue.clear();
                event(&mut client, "proxy_upstream_interrupted", "Upstream disconnected. Sent messages were not replayed; in-flight work may be incomplete.").await;
                if proxy.config.retry.max_retries == 0
                    || (proxy.config.retry.recovery_timeout_ms == 0
                        && recoveries >= proxy.config.retry.max_retries)
                {
                    fail(
                        &mut client,
                        "proxy_recovery_exhausted",
                        "WebSocket recovery budget exhausted; open a new connection",
                    )
                    .await;
                    return;
                }
                let delay = retry_delay(&proxy.config, recoveries, &HeaderMap::new());
                recoveries = recoveries.saturating_add(1);
                let reconnect_recovery = Recovery::new(&proxy.config);
                upstream = match connect_queued(
                    &proxy,
                    WsRoute {
                        url: &url,
                        headers: &headers,
                        project: &project,
                        protocol: negotiated_protocol.as_ref(),
                    },
                    &mut client,
                    &mut queue,
                    delay,
                    reconnect_recovery.clone(),
                )
                .await
                {
                    Ok(socket) => socket,
                    Err(()) => return,
                };
                queued_recovery = (!queue.is_empty()).then_some(reconnect_recovery);
                if client.send(Message::Text(json!({"type":"proxy.reconnected","fresh_session":true,"replayed_sent_messages":false,"message":"Upstream connection restored. Resubmit required context; prior session state is not restored."}).to_string().into())).await.is_err() { return; }
            }
        }
    });
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::handshake::server::{
        Request as WsRequest, Response as WsResponse,
    };

    async fn start(config: Config) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/v1/responses", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router(config).unwrap())
                .await
                .unwrap();
        });
        (url, task)
    }

    async fn next(
        socket: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    ) -> Message {
        tokio::time::timeout(Duration::from_secs(3), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    #[test]
    fn guidance_covers_flat_nested_and_model_less_creates_but_not_control_frames() {
        let config = Config {
            skip_blocked_security_work: true,
            ..Config::test_fixture()
        };
        for value in [
            json!({"type":"response.create","model":"unknown","input":"original"}),
            json!({"type":"response.create","input":[],"previous_response_id":"r_previous"}),
            json!({"type":"response.create","response":{"model":"unknown","input":[]}}),
            json!({"type":"response.create","response":{"input":[]}}),
        ] {
            let (message, project) = rewrite_message(
                &config,
                "/v1/responses",
                Message::Text(value.to_string().into()),
            )
            .unwrap();
            let rewritten: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            let envelope = rewritten.get("response").unwrap_or(&rewritten);
            assert!(guidance::present(envelope));
            if value.get("model").is_none() && value.pointer("/response/model").is_none() {
                assert!(project.is_none());
            }
            let again = rewrite_message(&config, "/v1/responses", message.clone())
                .unwrap()
                .0;
            assert_eq!(message, again);
        }
        let control = Message::Text(
            json!({"type":"response.cancel","response_id":"r_previous"})
                .to_string()
                .into(),
        );
        assert_eq!(
            rewrite_message(&config, "/v1/responses", control.clone())
                .unwrap()
                .0,
            control
        );
    }

    #[test]
    fn queued_requests_are_recorded_on_receipt_with_inherited_route_and_cancellation() {
        let store = Arc::new(logs::Store::default());
        let mut config = Config::test_fixture();
        config.aliases[1].api_key = Some("primary".into());
        let mut queue = PendingQueue::new(store.clone(), Arc::new(config), "/v1/responses".into());
        queue_message(
            &mut queue,
            Message::Text(
                json!({"type":"response.create","model":"gpt-4.1"})
                    .to_string()
                    .into(),
            ),
        )
        .unwrap();
        queue_message(
            &mut queue,
            Message::Text(json!({"type":"response.create"}).to_string().into()),
        )
        .unwrap();
        let records = store.recent();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].requested_model.as_deref(), Some("gpt-4.1"));
        assert_eq!(records[0].project.as_deref(), Some("primary"));
        assert_eq!(records[0].state, "pending");
        assert_eq!(records[0].attempts, 0);
        drop(queue);
        assert!(store.recent().iter().all(|e| e.state == "cancelled"));
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn first_frame_routes_key_and_rewrites_then_rejects_key_change() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config {
            upstream_url: format!("http://{}", listener.local_addr().unwrap()),
            ..Config::test_fixture()
        };
        config.aliases[1].api_key = Some("primary".into());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                |request: &WsRequest, response: WsResponse| {
                    assert_eq!(
                        request.headers()["authorization"],
                        "Bearer replace-with-primary-api-key"
                    );
                    Ok(response)
                },
            )
            .await
            .unwrap();
            for _ in 0..2 {
                let value: Value =
                    serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(value["model"], "gpt-4.1-mini");
                assert_eq!(
                    value["reasoning"],
                    json!({"effort":"low", "summary":"auto"})
                );
                socket
                    .send(Message::Text(value.to_string().into()))
                    .await
                    .unwrap();
            }
            assert!(matches!(
                socket.next().await,
                Some(Ok(Message::Close(_))) | None
            ));
        });
        let (url, task) = start(config).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        socket.send(Message::Ping(vec![7].into())).await.unwrap();
        assert!(next(&mut socket).await.is_pong());
        for _ in 0..2 {
            socket.send(Message::Text(json!({"type":"response.create","model":"gpt-4.1","reasoning":{"summary":"auto"},"input":"hi"}).to_string().into())).await.unwrap();
            let value: Value =
                serde_json::from_str(next(&mut socket).await.to_text().unwrap()).unwrap();
            assert_eq!(value["model"], "gpt-4.1-mini");
        }
        socket
            .send(Message::Text(
                json!({"type":"response.create","model":"unknown"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let value: Value =
            serde_json::from_str(next(&mut socket).await.to_text().unwrap()).unwrap();
        assert_eq!(value["error"]["code"], "proxy_api_key_change");
        assert!(next(&mut socket).await.is_close());
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn capacity_events_reach_client_as_explicit_server_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config {
            upstream_url: format!("http://{}", listener.local_addr().unwrap()),
            ..Config::test_fixture()
        };
        config.retry.recovery_timeout_ms = 0;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.next().await.unwrap().unwrap();
            socket.send(Message::Text(json!({"type":"response.failed","response":{"id":"r1","error":{"code":"server_is_overloaded","message":"busy"}}}).to_string().into())).await.unwrap();
            socket.next().await;
        });
        let (url, task) = start(config).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        socket
            .send(Message::Text(
                json!({"type":"response.create","model":"unknown","input":"hi"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let value: Value =
            serde_json::from_str(next(&mut socket).await.to_text().unwrap()).unwrap();
        assert_eq!(value["response"]["error"]["code"], "server_error");
        assert_eq!(
            value["response"]["error"]["upstream_code"],
            "server_is_overloaded"
        );
        assert!(
            value["response"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Original upstream message: busy")
        );
        socket.close(None).await.unwrap();
        server.await.unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn reconnects_without_replaying_sent_work_and_accepts_next_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config {
            upstream_url: format!("http://{}", listener.local_addr().unwrap()),
            ..Config::test_fixture()
        };
        config.retry.initial_delay_ms = 1;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut first = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert!(first.next().await.unwrap().unwrap().is_text());
            first
                .send(Message::Text(
                    json!({"type":"response.output_text.delta","delta":"partial"})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            drop(first);
            let (stream, _) = listener.accept().await.unwrap();
            let mut second = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), second.next())
                    .await
                    .is_err(),
                "sent work must not replay"
            );
            second.send(Message::Text("ready".into())).await.unwrap();
            let message = second.next().await.unwrap().unwrap();
            let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            assert_eq!(value["input"], "second");
            assert_eq!(value["model"], "gpt-4.1-mini");
            second.send(message).await.unwrap();
            second
                .close(Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: "done".into(),
                }))
                .await
                .unwrap();
        });
        let (url, task) = start(config).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        socket
            .send(Message::Text(
                json!({"type":"response.create","model":"gpt-4.1","input":"first"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        assert!(
            next(&mut socket)
                .await
                .to_text()
                .unwrap()
                .contains("partial")
        );
        assert!(
            next(&mut socket)
                .await
                .to_text()
                .unwrap()
                .contains("proxy_upstream_interrupted")
        );
        assert!(
            next(&mut socket)
                .await
                .to_text()
                .unwrap()
                .contains("proxy.reconnected")
        );
        assert_eq!(next(&mut socket).await.to_text().unwrap(), "ready");
        socket
            .send(Message::Text(
                json!({"type":"response.create","model":"gpt-4.1","input":"second"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        assert!(
            next(&mut socket)
                .await
                .to_text()
                .unwrap()
                .contains("second")
        );
        assert!(next(&mut socket).await.is_close());
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn capacity_handshake_retries_before_sending_first_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config {
            upstream_url: format!("http://{}", listener.local_addr().unwrap()),
            ..Config::test_fixture()
        };
        config.retry.initial_delay_ms = 1;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
                assert!(request.len() < 8192);
            }
            stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 8\r\nConnection: close\r\n\r\ncapacity").await.unwrap();
            drop(stream);
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let message = socket.next().await.unwrap().unwrap();
            assert!(message.to_text().unwrap().contains("gpt-4.1-mini"));
            socket.send(message).await.unwrap();
        });
        let (url, task) = start(config).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        socket
            .send(Message::Text(
                json!({"type":"response.create","model":"gpt-4.1"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        assert!(
            next(&mut socket)
                .await
                .to_text()
                .unwrap()
                .contains("gpt-4.1-mini")
        );
        socket.close(None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        task.abort();
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn rejects_changed_subprotocol_after_deferred_handshake() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = Config {
            upstream_url: format!("http://{}", listener.local_addr().unwrap()),
            ..Config::test_fixture()
        };
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _socket = tokio_tungstenite::accept_hdr_async(
                stream,
                |request: &WsRequest, mut response: WsResponse| {
                    assert_eq!(request.headers()["sec-websocket-protocol"], "first, second");
                    response
                        .headers_mut()
                        .insert("sec-websocket-protocol", "second".parse().unwrap());
                    Ok(response)
                },
            )
            .await
            .unwrap();
        });
        let (url, task) = start(config).await;
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("sec-websocket-protocol", "first, second".parse().unwrap());
        let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(response.headers()["sec-websocket-protocol"], "first");
        socket
            .send(Message::Text(
                json!({"type":"response.create","model":"gpt-4.1"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        assert!(
            next(&mut socket)
                .await
                .to_text()
                .unwrap()
                .contains("proxy_subprotocol_changed")
        );
        assert!(next(&mut socket).await.is_close());
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        task.abort();
    }

    #[test]
    fn responses_websocket_routes_by_effort() {
        let config = Config {
            aliases: vec![
                serde_json::from_value(json!({
                    "from": "gpt-5.4", "to": "model-primary", "api_key": "primary",
                    "reasoning_routes": {"low": {"to": "model-secondary", "api_key": "default"}}
                }))
                .unwrap(),
            ],
            ..Config::test_fixture()
        };
        for nested in [false, true] {
            let envelope = json!({"model":"gpt-5.4", "reasoning":{"effort":"low"}});
            let input = if nested {
                json!({"type":"response.create", "response":envelope})
            } else {
                envelope
            };
            let (message, project) = rewrite_message(
                &config,
                "/v1/responses",
                Message::Text(input.to_string().into()),
            )
            .unwrap();
            let output: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            let envelope = if nested { &output["response"] } else { &output };
            assert_eq!(envelope["model"], "model-secondary");
            assert_eq!(envelope["reasoning"]["effort"], "low");
            assert_eq!(project.as_deref(), Some("default"));
            let store = Arc::new(logs::Store::default());
            let mut queue = PendingQueue::new(
                store.clone(),
                Arc::new(config.clone()),
                "/v1/responses".into(),
            );
            queue_message(&mut queue, Message::Text(input.to_string().into())).unwrap();
            let recorded = &store.recent()[0];
            assert_eq!(recorded.requested_reasoning.as_deref(), Some("low"));
            assert_eq!(recorded.routed_reasoning.as_deref(), Some("low"));
            assert_eq!(recorded.route_rule.as_deref(), Some("reasoning"));
            assert_eq!(recorded.routed_model.as_deref(), Some("model-secondary"));
        }
    }

    #[test]
    fn rewrites_nested_models_and_bounds_unsent_queue() {
        let config = Config::test_fixture();
        for (path, input, pointer) in [
            (
                "/v1/realtime",
                json!({"type":"session.update","session":{"model":"gpt-4.1"}}),
                "/session/model",
            ),
            (
                "/v1/realtime",
                json!({"type":"response.create","response":{"model":"gpt-4.1"}}),
                "/response/model",
            ),
        ] {
            let (message, _) =
                rewrite_message(&config, path, Message::Text(input.to_string().into())).unwrap();
            let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            assert_eq!(value.pointer(pointer).unwrap(), "gpt-4.1-mini");
        }
        let mut queue = PendingQueue::new(
            Arc::new(logs::Store::default()),
            Arc::new(Config::test_fixture()),
            "/v1/responses".into(),
        );
        queue_message(&mut queue, Message::Binary(vec![0u8; QUEUE_BYTES].into())).unwrap();
        assert!(queue_message(&mut queue, Message::Text("x".into())).is_err());
    }
    #[tokio::test]
    async fn refused_responses_retry_on_same_socket_and_keep_one_usage_entry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config {
            upstream_url: format!("http://{}", listener.local_addr().unwrap()),
            ..Config::test_fixture()
        };
        config.retry.max_retries = 1;
        config.retry.recovery_timeout_ms = 500;
        config.retry.initial_delay_ms = 1;
        config.retry.max_delay_ms = 1;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for n in 0..4 {
                let request = socket.next().await.unwrap().unwrap();
                let value: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
                assert_eq!(value["model"], "gpt-4.1-mini");
                assert_eq!(value["input"], "same context");
                socket.send(Message::Text(json!({"type":"response.created","response":{"id":format!("r{n}"),"output":[]}}).to_string().into())).await.unwrap();
                if n < 3 {
                    socket.send(Message::Text(json!({"type":"response.failed","response":{"id":format!("r{n}"),"error":{"code":"server_is_overloaded","message":"busy"}}}).to_string().into())).await.unwrap();
                } else {
                    socket
                        .send(Message::Text(
                            json!({"type":"response.output_text.delta","delta":"recovered"})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                    socket.send(Message::Text(json!({"type":"response.completed","response":{"id":"r3","usage":{"input_tokens":20,"output_tokens":3}}}).to_string().into())).await.unwrap();
                }
            }
            socket.next().await;
        });
        let (url, task) = start(config).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        socket
            .send(Message::Text(
                json!({"type":"response.create","model":"gpt-4.1","input":"same context"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let created: Value =
            serde_json::from_str(next(&mut socket).await.to_text().unwrap()).unwrap();
        assert_eq!(created["response"]["id"], "r3");
        assert!(
            next(&mut socket)
                .await
                .to_text()
                .unwrap()
                .contains("recovered")
        );
        assert!(
            next(&mut socket)
                .await
                .to_text()
                .unwrap()
                .contains("response.completed")
        );
        let logs_url = url
            .replace("ws://", "http://")
            .replace("/v1/responses", "/logs/api?local=true");
        let logs: Value = reqwest::get(logs_url).await.unwrap().json().await.unwrap();
        let entries: Vec<_> = logs["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["method"] == "SEND")
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["retries"], 3);
        assert_eq!(entries[0]["input_tokens"], 20);
        socket.close(None).await.unwrap();
        server.await.unwrap();
        task.abort();
    }
    #[tokio::test]
    async fn prelude_deadline_returns_retryable_error_and_cancel_stops_retries() {
        for cancel in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut config = Config {
                upstream_url: format!("http://{}", listener.local_addr().unwrap()),
                ..Config::test_fixture()
            };
            config.retry.recovery_timeout_ms = if cancel { 500 } else { 80 };
            config.retry.initial_delay_ms = 200;
            config.retry.max_delay_ms = 200;
            let (ready, wait) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                socket.next().await.unwrap().unwrap();
                if cancel {
                    socket
                        .send(Message::Text(
                            json!({"type":"error","status":503,"error":{"code":"server_is_overloaded"}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                } else {
                    socket
                        .send(Message::Text(
                            json!({"type":"response.created","response":{"id":"waiting"}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
                let _ = ready.send(());
                if let Ok(Some(Ok(message))) =
                    tokio::time::timeout(Duration::from_millis(350), socket.next()).await
                {
                    assert!(!message.is_text(), "cancelled work was replayed")
                }
            });
            let (url, task) = start(config).await;
            let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
            socket
                .send(Message::Text(
                    json!({"type":"response.create","model":"unknown"})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            wait.await.unwrap();
            if cancel {
                socket.close(None).await.unwrap();
            } else {
                let value: Value =
                    serde_json::from_str(next(&mut socket).await.to_text().unwrap()).unwrap();
                assert_eq!(value["error"]["code"], "server_error");
                assert_eq!(value["error"]["proxy_code"], "proxy_recovery_timeout");
                let _ = socket.close(None).await;
            }
            server.await.unwrap();
            task.abort();
        }
    }

    #[tokio::test]
    async fn deferred_handshake_and_prelude_share_one_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config {
            upstream_url: format!("http://{}", listener.local_addr().unwrap()),
            ..Config::test_fixture()
        };
        config.retry.recovery_timeout_ms = 500;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.next().await.unwrap().unwrap();
            socket
                .send(Message::Text(
                    json!({"type":"response.created","response":{"id":"waiting"}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            socket.next().await;
        });
        let (url, task) = start(config).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let started = tokio::time::Instant::now();
        socket
            .send(Message::Text(
                json!({"type":"response.create","model":"unknown"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let message = tokio::time::timeout(Duration::from_millis(650), next(&mut socket))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
        assert_eq!(value["error"]["proxy_code"], "proxy_recovery_timeout");
        assert!(started.elapsed() >= Duration::from_millis(450));
        let _ = socket.close(None).await;
        server.await.unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn exhausted_refusal_clears_usage_state_for_next_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config {
            upstream_url: format!("http://{}", listener.local_addr().unwrap()),
            ..Config::test_fixture()
        };
        config.retry.recovery_timeout_ms = 1000;
        config.retry.initial_delay_ms = 100;
        config.retry.max_delay_ms = 100;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut followups = 0;
            while let Some(Ok(message)) = socket.next().await {
                if !message.is_text() {
                    break;
                }
                let request: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                let followup = request["input"] == "followup";
                if followup {
                    followups += 1;
                }
                let id = if followup && followups > 1 {
                    "success"
                } else {
                    "refused"
                };
                socket
                    .send(Message::Text(
                        json!({"type":"response.created","response":{"id":id}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                if id == "success" {
                    socket.send(Message::Text(json!({"type":"response.completed","response":{"id":id,"usage":{"input_tokens":10,"output_tokens":2}}}).to_string().into())).await.unwrap();
                } else {
                    socket
                        .send(Message::Text(
                            json!({"type":"error","status":503,"error":{"code":"server_is_overloaded"}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
            }
            assert_eq!(followups, 2);
        });
        let (url, task) = start(config).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        for input in ["first", "followup"] {
            socket
                .send(Message::Text(
                    json!({"type":"response.create","model":"unknown","input":input})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let created: Value =
                serde_json::from_str(next(&mut socket).await.to_text().unwrap()).unwrap();
            let terminal: Value =
                serde_json::from_str(next(&mut socket).await.to_text().unwrap()).unwrap();
            if input == "first" {
                assert_eq!(created["response"]["id"], "refused");
                assert_eq!(terminal["error"]["code"], "server_error");
            } else {
                assert_eq!(created["response"]["id"], "success");
                assert_eq!(terminal["type"], "response.completed");
            }
        }
        socket.close(None).await.unwrap();
        server.await.unwrap();
        task.abort();
    }
}
