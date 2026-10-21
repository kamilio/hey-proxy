//! Native Claude Code transport; subscription authentication belongs to hey-proxy.
mod paths;
#[cfg(test)]
mod tests;
mod usage;
use super::*;
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;
pub(super) use usage::{reading, usage};

const OAUTH_BETA: &str = "oauth-2025-04-20";

fn default_routing() -> bool {
    true
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProviderConfig {
    pub upstream_url: String,
    /// Proxy-owned encrypted OAuth store. Relative paths resolve beside the config.
    pub credentials_file: Option<PathBuf>,
    pub usage_cache_seconds: u64,
    /// Whether /v1/messages routes through this subscription; false keeps it usage/recommender-only.
    #[serde(default = "default_routing")]
    pub routing: bool,
}
impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            upstream_url: "https://api.anthropic.com".into(),
            credentials_file: None,
            usage_cache_seconds: 60,
            routing: true,
        }
    }
}
impl ProviderConfig {
    pub(crate) fn validate(&self, _mode: Mode) -> Result<()> {
        let url = url::Url::parse(&self.upstream_url).context("Invalid Claude upstream URL")?;
        let loopback = url.host_str().is_some_and(|h| {
            h == "localhost" || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) || h == "[::1]"
        });
        ensure!(
            url.scheme() == "https" || url.scheme() == "http" && loopback,
            "Claude upstream requires HTTPS (HTTP is allowed only on loopback)"
        );
        ensure!(
            url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "Claude upstream must be a root URL without credentials, query or fragment"
        );
        ensure!(
            (30..=3600).contains(&self.usage_cache_seconds),
            "Claude usage_cache_seconds must be 30..3600"
        );
        if let Some(path) = &self.credentials_file {
            ensure!(
                path.extension().is_some_and(|e| e == "json"),
                "Claude credentials_file must end in .json"
            );
        }
        Ok(())
    }
    pub(crate) fn credentials_path(&self, config: Option<&Path>) -> Result<PathBuf> {
        let path = match (&self.credentials_file, config) {
            (Some(path), _) if path.is_absolute() => path.clone(),
            (Some(path), Some(config)) => config.parent().unwrap_or(Path::new(".")).join(path),
            (None, Some(config)) => config.with_extension("claude.json"),
            _ => anyhow::bail!(
                "Claude requires a file-backed config or an absolute credentials_file"
            ),
        };
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        if let Some(config) = config {
            let absolute = if config.is_absolute() {
                config.to_owned()
            } else {
                std::env::current_dir()?.join(config)
            };
            let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
            ensure!(
                canonical(&path) != canonical(&absolute),
                "Claude credentials_file must differ from the proxy config"
            );
        }
        Ok(path)
    }
}
#[derive(Default)]
pub(super) struct ClaudeState {
    tokens: crate::claude_auth::TokenManager,
    paths: paths::Cache,
    usage: tokio::sync::Mutex<usage::Cache>,
}

pub(super) fn is_enabled(config: &Config, source: Option<&Path>) -> bool {
    config.claude.is_some()
        || source
            .map(|p| p.with_extension("claude.json").exists())
            .unwrap_or(false)
}

pub(super) fn is_path(path: &str) -> bool {
    matches!(
        path.trim_end_matches('/'),
        "/v1/messages" | "/v1/messages/count_tokens"
    )
}
fn request_headers(mut headers: HeaderMap) -> HeaderMap {
    clean_headers(&mut headers);
    // Preserve Claude Code's feature headers and user agent, but never pass local credentials.
    for name in [
        "host",
        "authorization",
        "x-api-key",
        "api-key",
        "cookie",
        "openai-project",
        "openai-organization",
        "content-length",
        "accept-encoding",
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
    ] {
        headers.remove(name);
    }
    let mut betas: Vec<String> = headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if !betas.iter().any(|s| s == OAUTH_BETA) {
        betas.push(OAUTH_BETA.into());
    }
    headers.insert("anthropic-beta", betas.join(",").parse().unwrap());
    if !headers.contains_key("anthropic-version") {
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
    }
    headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    headers
}

pub(super) async fn forward(proxy: Arc<Proxy>, request: Request) -> Response {
    let Some(provider) = &proxy.config.claude else {
        return messages::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Configure providers.claude and run hey-proxy claude-login",
        );
    };
    if request.method() != axum::http::Method::POST {
        let mut response = messages::error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Claude Messages requires POST",
        );
        response
            .headers_mut()
            .insert(header::ALLOW, "POST".parse().unwrap());
        return response;
    }
    if request.headers().contains_key(header::UPGRADE)
        || request
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
    {
        return messages::error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Claude Messages requires uncompressed JSON over HTTP/SSE",
        );
    }
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 64 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return messages::error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Claude request exceeds 64 MiB or could not be read",
            );
        }
    };
    // This native route forwards the original bytes. Accounting needs only
    // model/options, so avoid materializing a second copy of conversation history.
    let input = match logs::metadata(&bytes) {
        Ok(value) => value,
        Err(_) => {
            return messages::error(StatusCode::BAD_REQUEST, "Claude body must be valid JSON");
        }
    };
    let Some(model) = input["model"].as_str().filter(|m| !m.is_empty()) else {
        return messages::error(StatusCode::BAD_REQUEST, "model must be a nonempty string");
    };
    proxy.service.logs.route(
        proxy.log_id,
        Some(model.into()),
        Some(model.into()),
        "claude",
    );
    proxy
        .service
        .logs
        .update(proxy.log_id, "pricing_options", Value::Null, |entry| {
            entry.speed = input
                .get("speed")
                .and_then(Value::as_str)
                .map(|s| s.chars().take(32).collect());
            entry.inference_geo = input
                .get("inference_geo")
                .and_then(Value::as_str)
                .map(|s| s.chars().take(32).collect());
            true
        });
    let path = match proxy
        .service
        .claude
        .paths
        .resolve(provider, proxy.service.source.as_deref())
        .await
    {
        Ok(path) => path,
        Err(_) => {
            return messages::error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Claude credential store is not configured",
            );
        }
    };
    let mut token = match proxy
        .service
        .claude
        .tokens
        .token(&path, &proxy.client)
        .await
    {
        Ok(token) => token,
        Err(error) => return messages::error(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    };
    let query = parts
        .uri
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let url = format!(
        "{}{}{query}",
        provider.upstream_url.trim_end_matches('/'),
        parts.uri.path().trim_end_matches('/')
    );
    let headers = request_headers(parts.headers);
    for index in 0..2 {
        let mut attempt = logs::Attempt::new(proxy.service.logs.clone(), proxy.log_id, "claude");
        let mut upstream = match proxy
            .client
            .post(&url)
            .headers(headers.clone())
            .bearer_auth(&token)
            .body(bytes.clone())
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => {
                attempt.finish("transport_error", None, Some("claude_unreachable"));
                return messages::error(StatusCode::BAD_GATEWAY, "Cannot reach Claude upstream");
            }
        };
        let status = upstream.status();
        attempt.finish(
            if status.is_success() {
                "accepted"
            } else {
                "http_error"
            },
            Some(status.as_u16()),
            None,
        );
        // Authentication was rejected before generation: rotate once, never replay a started stream.
        if status == StatusCode::UNAUTHORIZED && index == 0 {
            token = match proxy
                .service
                .claude
                .tokens
                .rejected(&path, &proxy.client, token)
                .await
            {
                Ok(token) => token,
                Err(error) => {
                    return messages::error(StatusCode::SERVICE_UNAVAILABLE, &error.to_string());
                }
            };
            proxy.service.logs.retries(proxy.log_id, 1);
            continue;
        }
        let mut headers = upstream.headers().clone();
        clean_headers(&mut headers);
        headers.remove(header::SET_COOKIE);
        let sse = headers
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h.contains("text/event-stream"));
        if sse {
            headers.remove(header::CONTENT_LENGTH);
        }
        proxy
            .service
            .logs
            .update(proxy.log_id, "response_type", json!({}), |entry| {
                entry.streaming = sse;
                true
            });
        let request_id = headers
            .get("request-id")
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        proxy
            .service
            .logs
            .update(proxy.log_id, "upstream_headers", json!({}), |entry| {
                entry.upstream_request_id = request_id;
                true
            });
        let mut reader = logs::UsageReader::new(sse);
        let body = Body::from_stream(async_stream::stream! {
            loop {
                match upstream.chunk().await {
                    Ok(Some(bytes)) => { reader.feed(&bytes, &proxy.service.logs, proxy.log_id); yield Ok::<_, reqwest::Error>(bytes); }
                    Ok(None) => { reader.finish(&proxy.service.logs, proxy.log_id); break; }
                    Err(error) => { yield Err(error); break; }
                }
            }
        });
        let mut response = Response::new(body);
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        return response;
    }
    unreachable!()
}
