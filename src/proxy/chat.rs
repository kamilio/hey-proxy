//! Opt-in Chat Completions facade over the existing Responses/provider pipeline.
mod request;
mod response;
mod stream;
#[cfg(test)]
mod tests;
use super::*;

const LIMIT: usize = 64 * 1024 * 1024;
pub(super) fn is_path(path: &str) -> bool {
    path.trim_end_matches('/') == "/v1/custom/chat/completions"
}

pub(super) async fn forward(proxy: Arc<Proxy>, request: Request) -> Response {
    if request.method() != axum::http::Method::POST {
        return (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response();
    }
    if request.uri().query().is_some() {
        return error(
            StatusCode::BAD_REQUEST,
            "Custom Chat Completions does not accept query parameters",
        );
    }
    if request
        .headers()
        .get(header::CONTENT_ENCODING)
        .is_some_and(|v| v != "identity")
    {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Custom Chat Completions requires uncompressed JSON",
        );
    }
    let (mut parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Chat request exceeds 64 MiB or could not be read",
            );
        }
    };
    let input: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Request body must be valid JSON"),
    };
    // Select by the caller's API shape before translating. The resulting per-request
    // config keeps fallback candidates on that same shape and never double-applies aliases.
    let mut config = (*proxy.config).clone();
    config.aliases.retain(|a| a.matches_shape(parts.uri.path()));
    for alias in &mut config.aliases {
        alias.api_shape = None;
    }
    let routed = match rewrite(&config, parts.uri.path(), bytes) {
        Ok((bytes, _)) => serde_json::from_slice::<Value>(&bytes).expect("rewritten JSON"),
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let converted = match request::convert(
        &input,
        routed["model"].as_str().unwrap_or(""),
        &proxy.service.gemini.codec,
    ) {
        Ok(converted) => converted,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let requested_model = input["model"].as_str().unwrap_or("").to_owned();
    let inner = Arc::new(Proxy {
        config: Arc::new(config),
        client: proxy.client.clone(),
        service: proxy.service.clone(),
        log_id: proxy.log_id,
        fallback_attempt: false,
    });
    parts.uri = "/v1/responses".parse().unwrap();
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    parts.headers.insert(
        header::ACCEPT,
        header::HeaderValue::from_static(if converted.stream {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    let upstream = fallback::forward(
        inner,
        Request::from_parts(parts, Body::from(converted.body.to_string())),
    )
    .await;
    if !upstream.status().is_success() {
        return upstream;
    }
    if converted.stream {
        return stream::adapt_with_logs(
            upstream,
            requested_model,
            converted.include_usage,
            converted.exclude_reasoning,
            Some((proxy.service.logs.clone(), proxy.log_id)),
        );
    }
    let (mut parts, body) = upstream.into_parts();
    let value = match axum::body::to_bytes(body, LIMIT)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    {
        Some(value) => value,
        None => {
            return error(
                StatusCode::BAD_GATEWAY,
                "Could not read a valid Responses reply within 64 MiB",
            );
        }
    };
    if value["status"] == "failed" || value.get("error").is_some_and(|e| !e.is_null()) {
        parts.status = StatusCode::BAD_GATEWAY;
        let body = json!({"error":value.get("error").filter(|e| !e.is_null()).cloned().unwrap_or_else(|| json!({"message":"Responses request failed","type":"server_error"}))});
        response::clean_headers(&mut parts.headers);
        return Response::from_parts(parts, Body::from(body.to_string()));
    }
    match response::completion(&value, &requested_model, converted.exclude_reasoning) {
        Ok(value) => {
            response::clean_headers(&mut parts.headers);
            Response::from_parts(parts, Body::from(value.to_string()))
        }
        Err(e) => error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}
