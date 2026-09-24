use super::*;
use hey_proxy::fallback::{self as policy, Prelude, SsePrelude};

/// Only real upstream responses may trigger HTTP status based fallback. A local
/// credential or conversion failure must never be mistaken for an upstream 502.
#[derive(Clone)]
pub(super) struct Upstream {
    pub gemini: bool,
}
#[derive(Clone)]
pub(super) struct EligibleFailure;

struct Candidate {
    requested: String,
    actual: String,
}

/// Resolve each candidate through the existing overwrite rules, and only then
/// look up fallback rules. Identity deduplication also bounds alias-induced cycles.
fn plan(
    config: &Config,
    path: &str,
    original: &Value,
    rewritten: &Value,
) -> Result<Vec<Candidate>, &'static str> {
    let Some(model) = original["model"].as_str() else {
        return Ok(Vec::new());
    };
    let mut pending = vec![model.to_owned()];
    let mut seen = std::collections::HashSet::new();
    let mut candidates = Vec::new();
    while let Some(requested) = pending.pop() {
        if candidates.len() == policy::MAX_ATTEMPTS {
            break;
        }
        let mut payload = if candidates.is_empty() {
            original.clone()
        } else {
            rewritten.clone()
        };
        payload["model"] = json!(requested);
        let (bytes, project) = rewrite(config, path, Bytes::from(payload.to_string()))?;
        let value: Value = serde_json::from_slice(&bytes).expect("rewritten JSON");
        let Some(actual) = value["model"].as_str() else {
            break;
        };
        if !seen.insert((actual.to_owned(), project.to_owned())) {
            continue;
        }
        pending.extend(
            policy::targets(&config.fallbacks, actual)
                .iter()
                .rev()
                .cloned(),
        );
        candidates.push(Candidate {
            requested,
            actual: actual.to_owned(),
        });
    }
    Ok(candidates)
}

pub(super) async fn forward(proxy: Arc<Proxy>, request: Request) -> Response {
    let config = &proxy.config;
    let has_fallbacks =
        config.mode != Mode::Client && config.fallbacks.values().any(|targets| !targets.is_empty());
    let path = request.uri().path().trim_end_matches('/');
    if has_fallbacks
        && path == "/v1/responses"
        && request
            .headers()
            .get(header::UPGRADE)
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        // Codex treats 426 as an immediate, session-wide HTTP/SSE downgrade.
        // The model may arrive only after upgrade, so this applies to the endpoint.
        return (
            StatusCode::UPGRADE_REQUIRED,
            axum::Json(json!({"error":{
                "type":"invalid_request_error", "code":"websocket_not_supported",
                "message":"Configured model fallbacks require HTTP/SSE Responses requests"
            }})),
        )
            .into_response();
    }
    if !has_fallbacks
        || request.method() != axum::http::Method::POST
        || !matches!(path, "/v1/responses" | "/v1/chat/completions")
        || request
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
        || !request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                let kind = v.split(';').next().unwrap_or("").trim();
                kind.eq_ignore_ascii_case("application/json")
                    || kind.to_ascii_lowercase().ends_with("+json")
            })
    {
        return forward_request(proxy, request).await;
    }
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 64 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Could not read fallback request within 64 MiB limit",
            );
        }
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Request body must be valid JSON"),
    };
    let fallback_payload: Value = match rewrite(config, parts.uri.path(), bytes.clone()) {
        Ok((bytes, _)) => serde_json::from_slice(&bytes).expect("rewritten JSON"),
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let candidates = match plan(config, parts.uri.path(), &value, &fallback_payload) {
        Ok(candidates) => candidates,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    if candidates.len() < 2 {
        return forward_request(proxy, Request::from_parts(parts, Body::from(bytes))).await;
    }
    if let Some(reason) = policy::replay_blocker(&value) {
        proxy.service.logs.update(
            proxy.log_id,
            "fallback_skipped",
            json!({"reason":reason}),
            |_| true,
        );
        return forward_request(proxy, Request::from_parts(parts, Body::from(bytes))).await;
    }

    // Each model is attempted once. No deadline or nested timed retry loop.
    let attempt_proxy = Arc::new(Proxy {
        config: config.clone(),
        client: proxy.client.clone(),
        service: proxy.service.clone(),
        log_id: proxy.log_id,
        fallback_attempt: true,
    });
    let requested = value["model"].as_str().unwrap_or("");
    // Preserve all history, reasoning, tool schemas, signatures, and request options.
    // Only the model changes between candidates; route-specific rewrites still apply.
    for (index, candidate) in candidates.iter().enumerate() {
        let mut payload = if index == 0 {
            value.clone()
        } else {
            fallback_payload.clone()
        };
        if index > 0 {
            payload["model"] = json!(candidate.requested);
        }
        let mut attempt_parts = parts.clone();
        // Idempotency keys are scoped to a deployment. A different body under the
        // same key can otherwise return the failed primary's cached response.
        if index > 0
            && let Some(key) = attempt_parts.headers.get("idempotency-key")
        {
            use sha2::{Digest, Sha256};
            let mut digest = Sha256::new();
            digest.update(key.as_bytes());
            digest.update(b"\0");
            digest.update(candidate.actual.as_bytes());
            let key = format!("hey-proxy-fallback-{:x}", digest.finalize());
            attempt_parts
                .headers
                .insert("idempotency-key", key.parse().unwrap());
        }
        proxy.service.logs.update(
            proxy.log_id,
            "fallback_attempt",
            json!({"model":candidate.actual,"attempt":index+1}),
            |_| true,
        );
        let (response, reason) = {
            let response = forward_request(
                attempt_proxy.clone(),
                Request::from_parts(
                    attempt_parts,
                    Body::from(serde_json::to_vec(&payload).expect("JSON value")),
                ),
            )
            .await;
            inspect(response).await
        };
        proxy.service.logs.retries(proxy.log_id, index as u32);
        proxy.service.logs.update(proxy.log_id, "fallback_result", json!({"model":candidate.actual,"attempt":index+1,"reason":reason,"status":response.status().as_u16()}), |entry| {
            entry.requested_model = Some(requested.into());
            entry.requested_reasoning = requested_effort(parts.uri.path(), &value).map(str::to_owned);
            true
        });
        if reason.is_none() || index + 1 == candidates.len() {
            return finish(proxy, response, index, requested);
        }
        // Dropping the failed response cancels its upstream body before advancing.
        drop(response);
    }
    unreachable!("nonempty validated chain")
}

async fn inspect(response: Response) -> (Response, Option<&'static str>) {
    // This client deliberately does not transparently decompress upstream bodies.
    // Never classify unreadable compressed error bytes as a transient failure.
    if response
        .headers()
        .get(header::CONTENT_ENCODING)
        .is_some_and(|v| v != "identity")
    {
        return (response, None);
    }
    if response.extensions().get::<EligibleFailure>().is_some() {
        return (response, Some("connection_error"));
    }
    let Some(upstream) = response.extensions().get::<Upstream>().cloned() else {
        return (response, None);
    };
    let sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    let status = response.status();
    if !status.is_success() && !matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 529) {
        return (response, None);
    }
    let (mut parts, body) = response.into_parts();
    let mut stream = body.into_data_stream();
    let mut chunks = Vec::new();
    let mut size = 0usize;
    let mut prelude = SsePrelude::new(upstream.gemini);
    let mut reason = None;
    let mut read_error = None;
    loop {
        match stream.next().await {
            Some(Ok(chunk)) => {
                size = size.saturating_add(chunk.len());
                let decision = if sse && status.is_success() {
                    prelude.feed(&chunk)
                } else {
                    Prelude::Waiting
                };
                chunks.push(chunk);
                if decision == Prelude::Committed {
                    break;
                }
                if decision == Prelude::Refused {
                    reason = Some("stream_refusal");
                    break;
                }
                if size >= policy::MAX_PRELUDE {
                    break;
                }
            }
            Some(Err(e)) => {
                // No output has been committed, no server-side state/hosted tools.
                // The original failure remains intact if this is the last candidate.
                read_error = Some(e);
                reason = Some("pre_output_disconnect");
                break;
            }
            None => {
                if !status.is_success() {
                    let bytes: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
                    if policy::transient_http(status.as_u16(), &bytes) {
                        reason = Some("http_transient");
                    }
                } else {
                    if sse {
                        reason = Some("pre_output_eof");
                    } else {
                        let bytes: Vec<u8> =
                            chunks.iter().flat_map(|c| c.iter().copied()).collect();
                        if policy::prelude_event(&bytes, upstream.gemini) == Prelude::Refused {
                            reason = Some("json_refusal");
                        }
                    }
                }
                break;
            }
        }
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    let body = Body::from_stream(async_stream::stream! {
        for chunk in chunks { yield Ok::<_, axum::Error>(chunk); }
        if let Some(e) = read_error { yield Err(e); }
        while let Some(chunk) = stream.next().await { yield chunk; }
    });
    (Response::from_parts(parts, body), reason)
}

fn finish(proxy: Arc<Proxy>, mut response: Response, index: usize, requested: &str) -> Response {
    response
        .headers_mut()
        .insert("x-hey-proxy-fallback-count", index.into());
    if let Ok(name) = header::HeaderValue::from_str(requested) {
        response
            .headers_mut()
            .insert("x-hey-proxy-requested-model", name);
    }
    let sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    let (parts, body) = response.into_parts();
    let mut stream = body.into_data_stream();
    let body = Body::from_stream(async_stream::stream! {
        let mut usage = logs::UsageReader::new(sse);
        while let Some(chunk) = stream.next().await {
            if let Ok(bytes) = &chunk { usage.feed(bytes, &proxy.service.logs, proxy.log_id); }
            yield chunk;
        }
        usage.finish(&proxy.service.logs, proxy.log_id);
    });
    Response::from_parts(parts, body)
}

#[cfg(test)]
mod tests;
