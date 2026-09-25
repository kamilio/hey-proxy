mod capacity;
mod chat;
mod fallback;
mod gemini;
mod guidance;
pub(crate) mod logs;
mod overview;
mod recovery;
mod sse;
mod websocket;
use crate::config::{self, Config, Fingerprint, IpVersion, Mode};
use anyhow::Result;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use recovery::Recovery;
use serde_json::{Value, json};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

/// Services that report the caller's public IP address as plain text, per address family.
/// Each host only resolves in its own family, so the answer matches how the upstream saw us.
const IP_LOOKUP_V4: [&str; 2] = ["https://api.ipify.org", "https://checkip.amazonaws.com"];
const IP_LOOKUP_V6: [&str; 2] = ["https://api6.ipify.org", "https://ipv6.icanhazip.com"];
const IP_CACHE_TTL: Duration = Duration::from_secs(60);
/// Sites unrelated to any upstream that answer plain HTTPS quickly. Any HTTP response from one
/// proves this machine is online, so a failed upstream connection is then the upstream's fault.
const CONNECTIVITY_PROBES: [&str; 2] = [
    "https://www.gstatic.com/generate_204",
    "https://cp.cloudflare.com/",
];
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Process-wide state shared by every request.
struct Service {
    credentials: hey_proxy::credentials::CredentialResolver,
    gemini: gemini::GeminiState,
    logs: Arc<logs::Store>,
    access_config: Option<PathBuf>,
    state: RwLock<Loaded>,
    /// Config file to watch; `None` disables hot reload.
    source: Option<PathBuf>,
    ip_lookup: IpLookup,
    /// Cached public addresses, indexed by family: `[IPv4, IPv6]`.
    public_ip: Mutex<[Option<(Instant, String)>; 2]>,
    connectivity_probes: Vec<String>,
}

struct Loaded {
    config: Arc<Config>,
    /// Rebuilt on reload because `ip_version` binds the client to an address family.
    client: reqwest::Client,
    fingerprint: Fingerprint,
}

/// A request-scoped view: the config and client in force when the request arrived.
struct Proxy {
    fallback_attempt: bool,
    config: Arc<Config>,
    client: reqwest::Client,
    service: Arc<Service>,
    log_id: u64,
}

pub struct IpLookup {
    pub v4: Vec<String>,
    pub v6: Vec<String>,
}

pub struct Options {
    pub logs: Option<Arc<logs::Store>>,
    pub access_config: Option<PathBuf>,
    /// Config file and its fingerprint at load time; later changes apply to new requests.
    pub source: Option<(PathBuf, Fingerprint)>,
    /// Public IP lookup URLs used when the upstream rejects this machine's IP.
    pub ip_lookup: IpLookup,
    /// Unrelated URLs probed when the upstream cannot be reached, to tell a dead internet
    /// connection from a dead upstream. Empty disables the check.
    pub connectivity_probes: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            logs: None,
            access_config: None,
            source: None,
            ip_lookup: IpLookup {
                v4: IP_LOOKUP_V4.iter().map(|url| (*url).to_owned()).collect(),
                v6: IP_LOOKUP_V6.iter().map(|url| (*url).to_owned()).collect(),
            },
            connectivity_probes: CONNECTIVITY_PROBES
                .iter()
                .map(|url| (*url).to_owned())
                .collect(),
        }
    }
}

fn unbounded_model_wait(config: &Config) -> bool {
    config.mode == Mode::Client || config.fallbacks.values().any(|targets| !targets.is_empty())
}

fn build_client(config: &Config) -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15));
    let builder = if unbounded_model_wait(config) {
        builder
    } else {
        builder.read_timeout(Duration::from_secs(300))
    };
    // Binding to the unspecified address of one family makes the resolver drop the other.
    let builder = match config.ip_version {
        IpVersion::Auto => builder,
        IpVersion::Ipv4 => builder.local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        IpVersion::Ipv6 => builder.local_address(IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
    };
    Ok(builder.build()?)
}

#[cfg(test)]
fn router(config: Config) -> Result<Router> {
    router_with(config, Options::default())
}

pub fn router_with(config: Config, options: Options) -> Result<Router> {
    config.validate()?;
    let client = build_client(&config)?;
    let (source, fingerprint) = match options.source {
        Some((path, fingerprint)) => (Some(path), fingerprint),
        None => (None, None),
    };
    if config.mode == Mode::Host && options.access_config.is_none() {
        anyhow::bail!("Host mode requires an access key file");
    }
    let service = Arc::new(Service {
        credentials: Default::default(),
        gemini: gemini::GeminiState::new(source.as_deref())?,
        logs: match options.logs {
            Some(logs) => logs,
            None => Arc::new(if let Some(path) = &source {
                logs::Store::open(&config, path)?
            } else {
                logs::Store::default()
            }),
        },
        access_config: options.access_config,
        state: RwLock::new(Loaded {
            config: Arc::new(config.effective()),
            client,
            fingerprint,
        }),
        source,
        ip_lookup: options.ip_lookup,
        public_ip: Mutex::new([None, None]),
        connectivity_probes: options.connectivity_probes,
    });
    Ok(Router::new()
        .route("/", axum::routing::get(overview::page))
        .route("/overview.js", axum::routing::get(overview::script))
        .route("/overview/api", axum::routing::get(overview::data))
        .route("/logs", axum::routing::get(logs::page))
        .route("/logs/dashboard.js", axum::routing::get(logs::script))
        .route("/logs/api", axum::routing::get(logs::entries))
        .route(
            "/logs/reporting.js",
            axum::routing::get(logs::report::script),
        )
        .route("/logs/prices.js", axum::routing::get(logs::report::prices))
        .route("/logs/api/export", axum::routing::get(logs::report::export))
        .route(
            "/logs/api/reports",
            axum::routing::get(logs::report::reports),
        )
        .route(
            "/logs/api/history",
            axum::routing::get(logs::report::history),
        )
        .route(
            "/logs/api/requests/{id}",
            axum::routing::get(logs::report::detail),
        )
        .route("/logs/api/health", axum::routing::get(logs::report::health))
        .route("/logs/login", axum::routing::post(login))
        .fallback(forward)
        .layer(axum::middleware::from_fn_with_state(
            service.clone(),
            authenticate,
        ))
        .with_state(service))
}

fn token(request: &Request) -> Option<&str> {
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| {
            request
                .headers()
                .get(header::COOKIE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| {
                    v.split(';')
                        .find_map(|cookie| cookie.trim().strip_prefix("hey_proxy_access="))
                })
        })
}
const LOGIN: &str = "<!doctype html><html lang=\"en\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>hey-proxy access</title><body style=\"background:#101318;color:#e7eaf0;font-family:system-ui;padding:40px\"><h1>Request logs</h1><form method=\"post\" action=\"/logs/login\"><label>Host access key <input name=\"api_key\" type=\"password\" required autocomplete=\"off\"></label><button>Open logs</button></form></body></html>";
async fn authenticate(
    State(service): State<Arc<Service>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let proxy = service.snapshot();
    if proxy.config.mode != Mode::Host || request.uri().path() == "/logs/login" {
        return next.run(request).await;
    }
    let accepted = service
        .access_config
        .as_ref()
        .and_then(|path| crate::access::read(path).ok())
        .is_some_and(|keys| token(&request).is_some_and(|token| keys.accepts(token)));
    if accepted {
        return next.run(request).await;
    }
    let path = request.uri().path();
    if path != "/" && !path.starts_with("/overview") && !path.starts_with("/logs") {
        let id = service
            .logs
            .begin(request.method().as_str(), request.uri().path(), "HTTP");
        service.logs.finish(id, 401);
        service
            .logs
            .complete(id, "failed", "authentication", Some("unauthorized"), 0);
    }
    if path == "/" {
        let page = LOGIN
            .replace("Request logs", "hey-proxy overview")
            .replace("Open logs", "Open overview")
            .replace(
                "<label>",
                "<input type=\"hidden\" name=\"next\" value=\"/\"><label>",
            );
        return (StatusCode::UNAUTHORIZED, axum::response::Html(page)).into_response();
    }
    if path == "/logs" {
        return (StatusCode::UNAUTHORIZED, axum::response::Html(LOGIN)).into_response();
    }
    error(
        StatusCode::UNAUTHORIZED,
        "A valid host access key is required",
    )
}
#[derive(serde::Deserialize)]
struct Login {
    api_key: String,
    next: Option<String>,
}
async fn login(
    State(service): State<Arc<Service>>,
    axum::extract::Form(form): axum::extract::Form<Login>,
) -> Response {
    let accepted = service
        .access_config
        .as_ref()
        .and_then(|path| crate::access::read(path).ok())
        .is_some_and(|keys| keys.accepts(&form.api_key));
    if !accepted {
        return error(StatusCode::UNAUTHORIZED, "Invalid host access key");
    }
    let next = if form.next.as_deref() == Some("/") {
        "/"
    } else {
        "/logs"
    };
    let mut response = axum::response::Redirect::to(next).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&format!(
        "hey_proxy_access={}; HttpOnly; SameSite=Strict; Path=/",
        form.api_key
    )) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

impl Service {
    /// Returns the current config, reloading the file first if it changed on disk.
    /// A single `stat` per request keeps this cheap; invalid edits are logged and ignored.
    fn snapshot(self: &Arc<Self>) -> Proxy {
        let (config, client) = match &self.source {
            Some(path) => {
                let fingerprint = config::fingerprint(path);
                let current = self.state.read().unwrap_or_else(|e| e.into_inner());
                if current.fingerprint == fingerprint {
                    (current.config.clone(), current.client.clone())
                } else {
                    drop(current);
                    let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
                    // Another request may have reloaded while we waited for the write lock.
                    if state.fingerprint != fingerprint {
                        match config::load(path).and_then(|mut config| {
                            if config.mode != state.config.mode {
                                anyhow::bail!("Mode changed; restart the proxy to apply it");
                            }
                            if config.logging != state.config.logging {
                                eprintln!(
                                    "Config `logging` changed; restart to apply storage settings"
                                );
                                config.logging = state.config.logging.clone();
                            }
                            let client = if config.ip_version == state.config.ip_version
                                && unbounded_model_wait(&config)
                                    == unbounded_model_wait(&state.config)
                            {
                                state.client.clone()
                            } else {
                                build_client(&config)?
                            };
                            Ok((config.effective(), client))
                        }) {
                            Ok((config, client)) => {
                                eprintln!("Config reloaded: {}", path.display());
                                if config.listen != state.config.listen {
                                    eprintln!(
                                        "Config `listen` changed to {}; restart to apply it",
                                        config.listen
                                    );
                                }
                                state.config = Arc::new(config);
                                state.client = client;
                            }
                            Err(e) => eprintln!(
                                "Config change ignored, keeping the previous config: {e:#}"
                            ),
                        }
                        state.fingerprint = fingerprint;
                    }
                    (state.config.clone(), state.client.clone())
                }
            }
            None => {
                let state = self.state.read().unwrap_or_else(|e| e.into_inner());
                (state.config.clone(), state.client.clone())
            }
        };
        Proxy {
            fallback_attempt: false,
            config,
            client,
            service: self.clone(),
            log_id: 0,
        }
    }

    /// Looks up this machine's public IP in one address family, cached briefly so repeated
    /// rejections stay cheap. Uses the request's client so any family binding applies.
    async fn public_ip(&self, client: &reqwest::Client, ipv6: bool) -> Option<String> {
        let slot = usize::from(ipv6);
        let cached = self.public_ip.lock().unwrap_or_else(|e| e.into_inner())[slot].clone();
        if let Some((when, ip)) = cached
            && when.elapsed() < IP_CACHE_TTL
        {
            return Some(ip);
        }
        let urls = if ipv6 {
            &self.ip_lookup.v6
        } else {
            &self.ip_lookup.v4
        };
        for url in urls {
            let Ok(response) = client.get(url).timeout(Duration::from_secs(5)).send().await else {
                continue;
            };
            if !response.status().is_success() {
                continue;
            }
            let Ok(text) = response.text().await else {
                continue;
            };
            let ip = text.trim();
            if ip.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv6() == ipv6) {
                self.public_ip.lock().unwrap_or_else(|e| e.into_inner())[slot] =
                    Some((Instant::now(), ip.to_owned()));
                return Some(ip.to_owned());
            }
        }
        None
    }

    /// Whether any unrelated probe site answers over HTTP. Any status counts: only a transport
    /// failure means the internet path itself is broken. `None` when no probes are configured.
    /// Uses the request's client so any address-family binding applies to the probe too.
    async fn internet_reachable(&self, client: &reqwest::Client) -> Option<bool> {
        if self.connectivity_probes.is_empty() {
            return None;
        }
        let probes = self
            .connectivity_probes
            .iter()
            .map(|url| Box::pin(client.get(url).timeout(PROBE_TIMEOUT).send()));
        Some(futures_util::future::select_ok(probes).await.is_ok())
    }
}

/// Explains a failed upstream connection: the transport-level cause plus whether this machine
/// is offline or only the upstream is unreachable. Logged and returned as the 502 message.
async fn diagnose_unreachable(proxy: &Proxy, url: &str, cause: &str) -> String {
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned());
    let verdict = match proxy.service.internet_reachable(&proxy.client).await {
        Some(true) => format!(
            "your internet connection works, so {host} appears to be down or unreachable"
        ),
        Some(false) => {
            "your internet connection appears to be down (no unrelated site could be reached either)"
                .to_owned()
        }
        None => "connectivity probes are disabled, so the cause could not be narrowed down"
            .to_owned(),
    };
    let message = format!("Could not reach upstream {host} ({cause}): {verdict}");
    eprintln!("{message}");
    message
}

/// A short, human-readable cause for a failed `reqwest` send, from the innermost error.
fn describe_send_error(err: &reqwest::Error) -> String {
    let mut cause: &dyn std::error::Error = err;
    while let Some(next) = cause.source() {
        cause = next;
    }
    let text = cause.to_string();
    let lower = text.to_ascii_lowercase();
    if err.is_timeout() {
        "connection timed out".to_owned()
    } else if lower.contains("dns") || lower.contains("lookup") || lower.contains("resolve") {
        format!("DNS lookup failed: {text}")
    } else if lower.contains("certificate") || lower.contains("tls") || lower.contains("ssl") {
        format!("TLS handshake failed: {text}")
    } else if err.is_connect() {
        format!("connection failed: {text}")
    } else {
        text
    }
}

/// True when the upstream rejected the request because this machine's IP is not allowlisted.
fn ip_not_authorized(status: StatusCode, prefix: &[u8]) -> bool {
    if !matches!(status.as_u16(), 401 | 403) {
        return false;
    }
    let code = serde_json::from_slice::<Value>(prefix).ok().and_then(|v| {
        v.pointer("/error/code")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    code.as_deref() == Some("ip_not_authorized")
        || String::from_utf8_lossy(prefix)
            .to_ascii_lowercase()
            .contains("ip is not authorized")
}

/// Logs which address family reached the upstream and our public address in that family.
/// When the whole error body is available, the same facts are appended to the upstream
/// error message so the client shows them too.
async fn annotate_ip_error(
    proxy: &Proxy,
    url: &str,
    upstream: Option<std::net::SocketAddr>,
    body: Vec<u8>,
    complete: bool,
) -> Vec<u8> {
    let ipv6 = upstream.is_some_and(|addr| addr.is_ipv6());
    let family = if ipv6 { "IPv6" } else { "IPv4" };
    let ip = proxy.service.public_ip(&proxy.client, ipv6).await;
    let shown = ip
        .as_deref()
        .unwrap_or("an unknown address (public IP lookup failed)");
    let peer = upstream.map_or_else(|| "unknown".to_owned(), |addr| addr.ip().to_string());
    eprintln!(
        "Upstream rejected {url}: IP not authorized. \
         Reached upstream {peer} over {family}; your public {family} address is {shown}"
    );
    let hint = match (ipv6, proxy.config.ip_version) {
        (true, IpVersion::Auto) => {
            " Allow that address, or set \"ip_version\": \"ipv4\" in the hey-proxy config to use your IPv4 address instead."
        }
        _ => " Allow that address on the upstream project.",
    };
    if !complete {
        return body;
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let Some(message) = value.pointer_mut("/error/message") else {
        return body;
    };
    let Some(text) = message.as_str() else {
        return body;
    };
    *message = Value::String(format!(
        "{} hey-proxy: this request reached the upstream over {family} from {shown}.{hint}",
        text.trim_end()
    ));
    serde_json::to_vec(&value).unwrap_or(body)
}

fn clean_headers(headers: &mut HeaderMap) {
    let nominated: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',').map(|s| s.trim().to_owned()))
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"error": {"message": message, "type": "proxy_error"}})),
    )
        .into_response()
}

fn requested_effort<'a>(path: &str, value: &'a Value) -> Option<&'a str> {
    if path.trim_end_matches('/').ends_with("/chat/completions") {
        value
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .or_else(|| value.pointer("/reasoning/effort").and_then(Value::as_str))
            .or_else(
                || match value.pointer("/reasoning/enabled").and_then(Value::as_bool) {
                    Some(true) => Some("medium"),
                    Some(false) => Some("none"),
                    None => None,
                },
            )
    } else {
        value.pointer("/reasoning/effort").and_then(Value::as_str)
    }
}

fn rewrite<'a>(
    config: &'a Config,
    path: &str,
    body: Bytes,
) -> Result<(Bytes, &'a str), &'static str> {
    let mut project = config.default.api_key.as_str();
    if body.is_empty() {
        return Ok((body, project));
    }
    let mut value: Value =
        serde_json::from_slice(&body).map_err(|_| "Request body must be valid JSON")?;
    let guided = config.skip_blocked_security_work && guidance::apply(path, &mut value);
    let model_pointer = if path.contains("/realtime/") && value.pointer("/session/model").is_some()
    {
        "/session/model"
    } else {
        "/model"
    };
    let mut aliased = false;
    if let Some(alias) = value
        .pointer(model_pointer)
        .and_then(Value::as_str)
        .and_then(|model| config.alias_for(model, path))
    {
        aliased = true;
        let effort = requested_effort(path, &value);
        let route = effort.and_then(|effort| alias.reasoning_routes.get(effort));
        if let Some(key) = route
            .and_then(|route| route.api_key.as_ref())
            .or(alias.api_key.as_ref())
        {
            project = key;
        }
        if let Some(model) = route.map(|route| &route.to).or(alias.to.as_ref()) {
            *value.pointer_mut(model_pointer).expect("matched model") =
                Value::String(model.clone());
        }
        if let Some(effort) = &alias.reasoning {
            if path.trim_end_matches('/').ends_with("/responses") {
                if value.get("reasoning").is_none_or(Value::is_null) {
                    value["reasoning"] = json!({});
                }
                if !value["reasoning"].is_object() {
                    return Err("reasoning must be an object for Responses requests");
                }
                value["reasoning"]["effort"] = Value::String(effort.clone());
            } else if path.trim_end_matches('/').ends_with("/chat/completions") {
                value["reasoning_effort"] = Value::String(effort.clone());
            }
        }
    }
    let explicit_openai = value
        .pointer(model_pointer)
        .and_then(Value::as_str)
        .and_then(|m| m.strip_prefix("openai/"))
        .map(str::to_owned);
    if let Some(model) = &explicit_openai {
        *value.pointer_mut(model_pointer).unwrap() = json!(model);
    }
    if guided || explicit_openai.is_some() || aliased {
        return Ok((
            Bytes::from(serde_json::to_vec(&value).map_err(|_| "Cannot serialize request")?),
            project,
        ));
    }
    Ok((body, project))
}

fn capacity_error(status: StatusCode, prefix: &[u8]) -> bool {
    let error = serde_json::from_slice::<Value>(prefix).ok();
    let code = error
        .as_ref()
        .and_then(|v| v.pointer("/error/code"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let kind = error
        .as_ref()
        .and_then(|v| v.pointer("/error/type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if [code, kind].iter().any(|value| {
        guidance::is_security_block(value)
            || matches!(
                *value,
                "insufficient_quota"
                    | "invalid_prompt"
                    | "billing_hard_limit_reached"
                    | "billing_not_active"
                    | "invalid_api_key"
                    | "credit_balance_exhausted"
                    | "organization_spend_limit_exceeded"
                    | "project_spend_limit_exceeded"
            )
    }) {
        return false;
    }
    if matches!(status.as_u16(), 429 | 503 | 529) {
        return true;
    }
    if status == StatusCode::INTERNAL_SERVER_ERROR {
        let text = String::from_utf8_lossy(prefix).to_ascii_lowercase();
        return [
            "overloaded",
            "out of capacity",
            "at capacity",
            "capacity_exceeded",
        ]
        .iter()
        .any(|phrase| text.contains(phrase));
    }
    false
}

fn retry_delay(config: &Config, attempt: u32, headers: &HeaderMap) -> Duration {
    let cap = config.retry.max_delay_ms;
    let exponential = config
        .retry
        .initial_delay_ms
        .saturating_mul(1u64 << attempt.min(20))
        .min(cap);
    // Equal jitter keeps some backoff even if the random draw is zero.
    let jittered = exponential / 2 + rand::random_range(0..=exponential - exponential / 2);
    let retry_after = headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.parse::<u64>().ok().map(Duration::from_secs).or_else(|| {
                httpdate::parse_http_date(v)
                    .ok()
                    .map(|when| when.duration_since(SystemTime::now()).unwrap_or_default())
            })
        })
        .unwrap_or_default();
    Duration::from_millis(jittered)
        .max(retry_after)
        .min(Duration::from_millis(cap))
}

// Private temporary files are automatically removed when the request ends.
// Every retry replays a complete body without retaining binary uploads in RAM.
struct ReplayBody(tokio::fs::File, tempfile::NamedTempFile, Option<Value>);

impl ReplayBody {
    fn new() -> std::io::Result<Self> {
        let temp = tempfile::NamedTempFile::new()?;
        Ok(Self(tokio::fs::File::from_std(temp.reopen()?), temp, None))
    }

    async fn request_body(&mut self) -> std::io::Result<reqwest::Body> {
        Ok(reqwest::Body::wrap_stream(ReaderStream::new(
            tokio::fs::File::open(self.1.path()).await?,
        )))
    }
}

fn route_model<'a>(
    config: &'a Config,
    path: &str,
    model: &str,
    project: &mut String,
) -> Option<&'a str> {
    let alias = config.alias_for(model, path);
    if let Some(key) = alias.and_then(|a| a.api_key.as_ref()) {
        *project = key.clone();
    }
    alias.and_then(|a| a.to.as_deref())
}

fn rewrite_uri(config: &Config, uri: &axum::http::Uri, project: &mut String) -> String {
    let mut path = uri.path().to_owned();
    if let Some(model) = path
        .strip_prefix("/v1/models/")
        .filter(|m| !m.contains('/'))
    {
        let decoded = percent_encoding::percent_decode_str(model).decode_utf8_lossy();
        if let Some(target) = route_model(config, uri.path(), &decoded, project) {
            path = format!(
                "/v1/models/{}",
                percent_encoding::utf8_percent_encode(target, percent_encoding::NON_ALPHANUMERIC)
            );
        }
    }
    if let Some(query) = uri.query() {
        let query = query
            .split('&')
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                let decode = |s: &str| {
                    percent_encoding::percent_decode_str(&s.replace('+', " "))
                        .decode_utf8_lossy()
                        .into_owned()
                };
                if decode(key) == "model"
                    && let Some(target) = route_model(config, uri.path(), &decode(value), project)
                {
                    return format!(
                        "{key}={}",
                        percent_encoding::utf8_percent_encode(
                            target,
                            percent_encoding::NON_ALPHANUMERIC
                        )
                    );
                }
                pair.to_owned()
            })
            .collect::<Vec<_>>()
            .join("&");
        path.push('?');
        path.push_str(&query);
    }
    path
}

async fn prepare_body(
    config: &Config,
    path: &str,
    content_type: &str,
    body: Body,
    project: &mut String,
    log: (&logs::Store, u64),
) -> Result<ReplayBody> {
    let mut original = ReplayBody::new()?;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        original.0.write_all(&chunk?).await?;
    }
    original.0.flush().await?;
    original.0.rewind().await?;
    let kind = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if kind == "application/json" || kind.ends_with("+json") {
        let mut bytes = Vec::new();
        original.0.read_to_end(&mut bytes).await?;
        let incoming = logs::json_model(&bytes);
        log.0.routing_decision(log.1, config, path, &bytes);
        let (rewritten, key) =
            rewrite(config, path, Bytes::from(bytes)).map_err(anyhow::Error::msg)?;
        // A body model, when supplied, takes precedence over URL routing.
        let value: Value = serde_json::from_slice(&rewritten).unwrap_or(Value::Null);
        if guidance::present(&value) {
            log.0.update(
                log.1,
                "guidance_applied",
                json!({"policy":"skip_blocked_security_work"}),
                |entry| {
                    entry.security_guidance = true;
                    true
                },
            );
        }
        if value.get("model").is_some() || value.pointer("/session/model").is_some() {
            *project = key.to_owned();
            log.0
                .route(log.1, incoming, logs::json_model(&rewritten), project);
        }
        original.2 = Some(value);
        original.0.rewind().await?;
        original.0.set_len(0).await?;
        original.0.write_all(&rewritten).await?;
    } else if kind == "multipart/form-data" {
        let boundary = multer::parse_boundary(content_type)?;
        let mut fields =
            multer::Multipart::new(ReaderStream::new(original.0.try_clone().await?), &boundary);
        let mut output = ReplayBody::new()?;
        let mut changed = false;
        while let Some(mut field) = fields.next_field().await? {
            output
                .0
                .write_all(format!("--{boundary}\r\n").as_bytes())
                .await?;
            for (name, value) in field.headers() {
                output.0.write_all(name.as_str().as_bytes()).await?;
                output.0.write_all(b": ").await?;
                output.0.write_all(value.as_bytes()).await?;
                output.0.write_all(b"\r\n").await?;
            }
            output.0.write_all(b"\r\n").await?;
            if field.file_name().is_none() && field.name() == Some("model") {
                let bytes = field.bytes().await?;
                let model = std::str::from_utf8(&bytes)?;
                *project = config.default.api_key.clone();
                let target = route_model(config, path, model, project);
                log.0.route(
                    log.1,
                    Some(model.into()),
                    Some(target.unwrap_or(model).into()),
                    project,
                );
                changed |= target.is_some();
                output
                    .0
                    .write_all(target.map_or(bytes.as_ref(), str::as_bytes))
                    .await?;
            } else if field.name() == Some("session") && path.contains("/realtime/") {
                let bytes = field.bytes().await?;
                log.0.routing_decision(log.1, config, path, &bytes);
                let (rewritten, key) =
                    rewrite(config, path, bytes.clone()).map_err(anyhow::Error::msg)?;
                let session: Value = serde_json::from_slice(&rewritten)?;
                if session.get("model").is_some() || session.pointer("/session/model").is_some() {
                    *project = key.to_owned();
                    log.0.route(
                        log.1,
                        logs::json_model(&bytes),
                        logs::json_model(&rewritten),
                        project,
                    );
                }
                changed |= rewritten != bytes;
                output.0.write_all(&rewritten).await?;
            } else {
                while let Some(chunk) = field.chunk().await? {
                    output.0.write_all(&chunk).await?;
                }
            }
            output.0.write_all(b"\r\n").await?;
        }
        output
            .0
            .write_all(format!("--{boundary}--\r\n").as_bytes())
            .await?;
        if changed {
            output.0.flush().await?;
            return Ok(output);
        }
    }
    original.0.flush().await?;
    Ok(original)
}

async fn forward(State(service): State<Arc<Service>>, request: Request) -> Response {
    let mut snapshot = service.snapshot();
    snapshot.log_id = service
        .logs
        .begin(request.method().as_str(), request.uri().path(), "HTTP");
    let id = snapshot.log_id;
    if request
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        service
            .logs
            .update(id, "websocket_handshake", json!({}), |entry| {
                entry.transport = "WebSocket handshake".into();
                true
            });
    }
    let guard = logs::RequestGuard::new(service.logs.clone(), id);
    let proxy = Arc::new(snapshot);
    let response = if chat::is_path(request.uri().path()) && proxy.config.mode != Mode::Client {
        chat::forward(proxy, request).await
    } else {
        fallback::forward(proxy, request).await
    };
    service.logs.finish(id, response.status().as_u16());
    if response.extensions().get::<recovery::Timeout>().is_some() {
        service
            .logs
            .update(id, "recovery_timeout", json!({}), |entry| {
                entry.error_code = Some("proxy_recovery_timeout".into());
                true
            });
    }
    let is_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    let length = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, guard.wrap(body, length, is_sse))
}

async fn forward_request(proxy: Arc<Proxy>, request: Request) -> Response {
    if request
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        return websocket::forward(proxy, request).await;
    }
    let (parts, body) = request.into_parts();
    if gemini::native_path(parts.uri.path()) && proxy.config.mode != Mode::Client {
        return gemini::forward_native(proxy, parts, body).await;
    }
    let mut project = proxy.config.default.api_key.clone();
    let path_and_query = rewrite_uri(&proxy.config, &parts.uri, &mut project);
    proxy.service.logs.route(
        proxy.log_id,
        logs::uri_model(&parts.uri),
        logs::uri_model(&path_and_query.parse().unwrap()),
        &project,
    );
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let encoded = parts
        .headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|v| v != "identity");
    let mut body = match prepare_body(
        &proxy.config,
        parts.uri.path(),
        if encoded { "" } else { content_type },
        body,
        &mut project,
        (&proxy.service.logs, proxy.log_id),
    )
    .await
    {
        Ok(body) => body,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "Could not read or prepare request body",
            );
        }
    };
    if body
        .2
        .as_ref()
        .and_then(|v| v.get("model"))
        .and_then(Value::as_str)
        .is_some_and(|m| m.starts_with("gemini/"))
        && proxy.config.mode != Mode::Client
    {
        return gemini::forward(proxy, parts, body.2.take().expect("parsed Gemini request")).await;
    }
    let url = format!(
        "{}{}",
        proxy.config.upstream_url.trim_end_matches('/'),
        path_and_query
    );
    let mut headers = parts.headers;
    clean_headers(&mut headers);
    let protocols = headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(", ");
    if !protocols.is_empty() {
        let filtered = protocols
            .split(',')
            .map(str::trim)
            .filter(|p| !p.starts_with("openai-insecure-api-key."))
            .collect::<Vec<_>>()
            .join(", ");
        headers.remove("sec-websocket-protocol");
        if !filtered.is_empty() {
            headers.insert(
                "sec-websocket-protocol",
                filtered.parse().expect("valid protocols"),
            );
        }
    }
    for name in [
        "host",
        "content-length",
        "authorization",
        "x-api-key",
        "api-key",
        "cookie",
        "openai-organization",
        "openai-project",
    ] {
        headers.remove(name);
    }
    let body_length = match body.0.metadata().await {
        Ok(metadata) => metadata.len(),
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not read request size",
            );
        }
    };
    headers.insert(header::CONTENT_LENGTH, body_length.into());
    proxy
        .service
        .logs
        .update(proxy.log_id, "request_body", json!({}), |entry| {
            entry.request_bytes = body_length;
            true
        });
    let Some(source) = proxy.config.api_keys.get(&project) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "No OpenAI credential configured; add providers.openai.api_keys.default to your proxy config",
        );
    };
    let resolved = match proxy
        .service
        .credentials
        .resolve(
            source,
            Duration::from_secs(proxy.config.credential_cache_seconds),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_GATEWAY, &e.to_string()),
    };
    let mut authorization = header::HeaderValue::from_str(&format!(
        "Bearer {}",
        resolved.to_str().expect("validated credential")
    ))
    .expect("validated credential");
    authorization.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, authorization);
    let mut recovery = Recovery::for_request(&proxy);
    loop {
        proxy.service.logs.retries(proxy.log_id, recovery.retries);
        let mut attempt = logs::Attempt::new(proxy.service.logs.clone(), proxy.log_id, "http");
        let request_body = match body.request_body().await {
            Ok(body) => body,
            Err(_) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not replay request body",
                );
            }
        };
        let mut upstream = match recovery
            .run(
                proxy
                    .client
                    .request(parts.method.clone(), &url)
                    .headers(headers.clone())
                    .body(request_body)
                    .send(),
            )
            .await
        {
            Ok(Ok(response)) => response,
            Err(()) => {
                attempt.finish("timeout", None, Some("proxy_recovery_timeout"));
                return recovery::timeout_response();
            }
            Ok(Err(err)) => {
                attempt.finish(
                    "connection_error",
                    None,
                    Some(if recovery::connection_failure(&err) {
                        "connect_failed"
                    } else {
                        "transport_error"
                    }),
                );
                if recovery.timed()
                    && recovery::connection_failure(&err)
                    && recovery.retry(&proxy.config, &HeaderMap::new()).await
                {
                    continue;
                }
                if proxy.fallback_attempt {
                    let mut response = error(
                        StatusCode::BAD_GATEWAY,
                        "Upstream transport failed before a response",
                    );
                    if recovery::connection_failure(&err) || err.is_timeout() {
                        response.extensions_mut().insert(fallback::EligibleFailure);
                    }
                    return response;
                }
                let cause = describe_send_error(&err);
                let message = match recovery
                    .run(diagnose_unreachable(&proxy, &url, &cause))
                    .await
                {
                    Ok(message) => message,
                    Err(()) => {
                        attempt.finish("timeout", None, Some("proxy_recovery_timeout"));
                        return recovery::timeout_response();
                    }
                };
                return error(StatusCode::BAD_GATEWAY, &message);
            }
        };
        let mut status = upstream.status();
        let peer = upstream.remote_addr();
        let mut response_headers = upstream.headers().clone();
        let upstream_id = response_headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.chars().take(128).collect::<String>());
        proxy.service.logs.update(
            proxy.log_id,
            "upstream_headers",
            json!({"status":status.as_u16()}),
            |entry| {
                entry.upstream_request_id = upstream_id;
                true
            },
        );
        let mut prefix = Vec::new();
        let mut complete = false;
        if !proxy.fallback_attempt
            && (matches!(status.as_u16(), 401 | 403)
                || matches!(status.as_u16(), 429 | 500 | 503 | 529))
        {
            // Inspect a bounded error prefix, including the final attempt for capacity translation.
            while prefix.len() < 64 * 1024 {
                match recovery.run(upstream.chunk()).await {
                    Ok(Ok(Some(chunk))) => prefix.extend_from_slice(&chunk),
                    Ok(Ok(None)) => {
                        complete = true;
                        break;
                    }
                    Err(()) => {
                        attempt.finish("timeout", None, Some("proxy_recovery_timeout"));
                        return recovery::timeout_response();
                    }
                    Ok(Err(_)) => {
                        return error(StatusCode::BAD_GATEWAY, "Could not read upstream error");
                    }
                }
            }
        }
        let refused = capacity_error(status, &prefix);
        if refused {
            attempt.finish(
                "refused",
                Some(status.as_u16()),
                Some("capacity_or_rate_limit"),
            );
        }
        if refused && recovery.retry(&proxy.config, &response_headers).await {
            eprintln!(
                "Upstream returned {}; internal retry {}",
                status.as_u16(),
                recovery.retries
            );
            continue;
        }
        let is_sse = response_headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"));
        if status.is_success() && is_sse && recovery.timed() {
            let refusal = loop {
                match capacity::prelude_sse(&prefix, complete) {
                    capacity::Prelude::Refused => break true,
                    capacity::Prelude::Started => break false,
                    capacity::Prelude::Waiting if complete => break false,
                    capacity::Prelude::Waiting => {}
                }
                match recovery.run(upstream.chunk()).await {
                    Ok(Ok(Some(chunk))) => prefix.extend_from_slice(&chunk),
                    Ok(Ok(None)) => complete = true,
                    // A disconnect without an explicit rejection is ambiguous; never replay it.
                    Ok(Err(_)) => {
                        return error(
                            StatusCode::BAD_GATEWAY,
                            "Upstream stream interrupted before output",
                        );
                    }
                    Err(()) => {
                        attempt.finish("timeout", None, Some("proxy_recovery_timeout"));
                        return recovery::timeout_response();
                    }
                }
            };
            if refusal {
                attempt.finish(
                    "refused",
                    Some(status.as_u16()),
                    Some("capacity_or_rate_limit"),
                );
            }
            if refusal && recovery.retry(&proxy.config, &response_headers).await {
                continue;
            }
        }
        if complete
            && !status.is_success()
            && let Some(rewritten) = capacity::http(&prefix)
        {
            prefix = rewritten;
            status = StatusCode::INTERNAL_SERVER_ERROR;
            response_headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            response_headers.insert(header::CONTENT_LENGTH, prefix.len().into());
            eprintln!(
                "Upstream model is at capacity; translated HTTP failure to retryable server error (500)"
            );
        }
        clean_headers(&mut response_headers);
        if proxy.config.mode != Mode::Client && ip_not_authorized(status, &prefix) {
            prefix = annotate_ip_error(&proxy, &url, peer, prefix, complete).await;
            if complete {
                response_headers.insert(header::CONTENT_LENGTH, prefix.len().into());
            }
        }
        let content_type = response_headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let inspect_usage = !proxy.fallback_attempt
            && (content_type.contains("json") || content_type.contains("text/event-stream"));
        let is_sse = content_type.contains("text/event-stream");
        if is_sse {
            response_headers.remove(header::CONTENT_LENGTH);
        }
        let mut usage = logs::UsageReader::new(is_sse);
        proxy
            .service
            .logs
            .update(proxy.log_id, "response_type", json!({}), |entry| {
                entry.streaming = is_sse;
                true
            });
        attempt.finish(
            if status.is_success() {
                "accepted"
            } else {
                "http_error"
            },
            Some(status.as_u16()),
            None,
        );
        let mut sse = capacity::Sse::default();
        let response_body = Body::from_stream(async_stream::stream! {
            if !prefix.is_empty() {
                if inspect_usage { usage.feed(&prefix, &proxy.service.logs, proxy.log_id); }
                let bytes = if is_sse { sse.feed(&prefix) } else { prefix };
                if !bytes.is_empty() { yield Ok::<_, reqwest::Error>(Bytes::from(bytes)); }
            }
            loop {
                match upstream.chunk().await {
                    Ok(Some(chunk)) => {
                        if inspect_usage { usage.feed(&chunk, &proxy.service.logs, proxy.log_id); }
                        if is_sse {
                            let bytes = sse.feed(&chunk);
                            if !bytes.is_empty() { yield Ok(Bytes::from(bytes)); }
                        } else { yield Ok(chunk); }
                    }
                    Ok(None) => {
                        if is_sse {
                            let bytes = sse.finish();
                            if !bytes.is_empty() { yield Ok(Bytes::from(bytes)); }
                        }
                        if inspect_usage { usage.finish(&proxy.service.logs, proxy.log_id); }
                        break;
                    }
                    Err(error) => {
                        if is_sse {
                            let bytes = sse.finish();
                            if !bytes.is_empty() { yield Ok(Bytes::from(bytes)); }
                        }
                        yield Err(error); break;
                    }
                }
            }
        });
        let mut response = Response::new(response_body);
        *response.status_mut() = status;
        *response.headers_mut() = response_headers;
        response
            .extensions_mut()
            .insert(fallback::Upstream { gemini: false });
        return response;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_by_incoming_effort_and_preserves_it() {
        let config = Config {
            aliases: vec![
                serde_json::from_value(json!({
                    "from": "gpt-5.4", "to": "model-primary", "api_key": "primary",
                    "reasoning_routes": {
                        "low": {"to": "model-secondary", "api_key": "default"},
                        "medium": {"to": "model-secondary", "api_key": "default"},
                        "mid": {"to": "model-secondary", "api_key": "default"}
                    }
                }))
                .unwrap(),
            ],
            ..Config::test_fixture()
        };
        config.validate().unwrap();
        for path in ["/v1/responses", "/v1/chat/completions"] {
            for effort in [
                None,
                Some("low"),
                Some("medium"),
                Some("mid"),
                Some("high"),
                Some("xhigh"),
                Some("unknown"),
            ] {
                let mut input = json!({"model": "gpt-5.4", "input": "hello"});
                if let Some(effort) = effort {
                    if path.ends_with("responses") {
                        input["reasoning"] = json!({"effort": effort});
                    } else {
                        input["reasoning_effort"] = json!(effort);
                    }
                }
                let (body, key) = rewrite(&config, path, Bytes::from(input.to_string())).unwrap();
                let mut output: Value = serde_json::from_slice(&body).unwrap();
                let lower = matches!(effort, Some("low" | "medium" | "mid"));
                assert_eq!(
                    output["model"],
                    if lower {
                        "model-secondary"
                    } else {
                        "model-primary"
                    }
                );
                assert_eq!(key, if lower { "default" } else { "primary" });
                output["model"] = input["model"].clone();
                assert_eq!(output, input);
            }
        }
    }

    use axum::{body::to_bytes, extract::State, routing::any};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::{net::TcpListener, task::JoinHandle};

    async fn serve(app: Router) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (address, task)
    }

    #[tokio::test]
    async fn routes_keys_models_reasoning_and_query_and_strips_credentials() {
        let upstream = Router::new().fallback(any(|request: Request| async move {
            let (parts, body) = request.into_parts();
            axum::Json(json!({"uri": parts.uri.to_string(), "authorization": parts.headers["authorization"].to_str().unwrap(), "leaked": (["cookie", "x-api-key", "api-key", "openai-project", "x-hop"].iter().any(|key| parts.headers.contains_key(*key))), "body": serde_json::from_slice::<Value>(&to_bytes(body, 10000).await.unwrap()).unwrap()}))
        }));
        let (url, upstream_task) = serve(upstream).await;
        let mut config = Config {
            upstream_url: url,
            ..Config::test_fixture()
        };
        config
            .api_keys
            .insert("primary".into(), "primary-secret".into());
        let (url, proxy_task) = serve(router(config).unwrap()).await;
        let client = reqwest::Client::new();
        for (model, expected_model, expected_key) in [
            ("gpt-4.1", "gpt-4.1-mini", "something"),
            ("model-primary", "model-primary", "primary-secret"),
            ("unknown", "unknown", "something"),
        ] {
            let result: Value = client
                .post(format!("{url}/v1/responses?test=one%20two"))
                .header("authorization", "Bearer caller-secret")
                .header("cookie", "secret")
                .header("x-api-key", "secret")
                .header("api-key", "secret")
                .header("openai-project", "wrong")
                .header("connection", "x-hop")
                .header("x-hop", "secret")
                .json(&json!({"model": model, "reasoning": {"summary": "auto"}, "input": "hi"}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(result["uri"], "/v1/responses?test=one%20two");
            assert_eq!(result["authorization"], format!("Bearer {expected_key}"));
            assert_eq!(result["body"]["model"], expected_model);
            assert_eq!(result["leaked"], false);
            if model == "gpt-4.1" {
                assert_eq!(
                    result["body"]["reasoning"],
                    json!({"effort": "low", "summary": "auto"})
                );
            }
        }
        let result: Value = client
            .post(format!("{url}/v1/chat/completions"))
            .json(&json!({"model": "gpt-4.1"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result["body"]["reasoning_effort"], "low");
        assert!(result["body"].get("reasoning").is_none());
        proxy_task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn retries_capacity_then_returns_success_and_preserves_final_errors() {
        for (status, failures, max_retries, expected_calls) in [
            (503, 2, 3, 3),
            (429, 9, 2, 3),
            (529, 1, 2, 2),
            (401, 9, 3, 1),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let state = calls.clone();
            let upstream = Router::new()
                .fallback(any(
                    move |State(calls): State<Arc<AtomicUsize>>| async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        if n < failures {
                            (
                                StatusCode::from_u16(status).unwrap(),
                                [("retry-after", "0")],
                                "capacity/error",
                            )
                                .into_response()
                        } else {
                            "ok".into_response()
                        }
                    },
                ))
                .with_state(state);
            let (upstream_url, task) = serve(upstream).await;
            let mut config = Config {
                upstream_url,
                ..Config::test_fixture()
            };
            config.retry = crate::config::Retry {
                max_retries,
                initial_delay_ms: 1,
                max_delay_ms: 2,
                recovery_timeout_ms: 0,
            };
            let (url, proxy_task) = serve(router(config).unwrap()).await;
            let response = reqwest::get(format!("{url}/v1/models")).await.unwrap();
            assert_eq!(
                response.status().as_u16(),
                if failures < expected_calls {
                    200
                } else {
                    status
                }
            );
            assert_eq!(
                response.text().await.unwrap(),
                if failures < expected_calls {
                    "ok"
                } else {
                    "capacity/error"
                }
            );
            assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
            proxy_task.abort();
            task.abort();
        }
    }

    #[test]
    fn retry_delay_is_bounded_exponential_and_honors_retry_after() {
        let config = Config::test_fixture();
        for attempt in 0..8 {
            let delay = retry_delay(&config, attempt, &HeaderMap::new());
            let bound = (500 * (1u64 << attempt)).min(30_000);
            assert!(delay >= Duration::from_millis(bound / 2));
            assert!(delay <= Duration::from_millis(bound));
        }
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "12".parse().unwrap());
        assert_eq!(retry_delay(&config, 0, &headers), Duration::from_secs(12));
        headers.insert("retry-after", "999999".parse().unwrap());
        assert_eq!(retry_delay(&config, 0, &headers), Duration::from_secs(30));
        headers.insert(
            "retry-after",
            httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(15))
                .parse()
                .unwrap(),
        );
        let delay = retry_delay(&config, 0, &headers);
        assert!(delay >= Duration::from_secs(14) && delay <= Duration::from_secs(15));
    }

    #[tokio::test]
    async fn translates_capacity_http_and_successful_sse_responses() {
        for sse in [false, true] {
            let payload = if sse {
                "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_is_overloaded\",\"message\":\"busy\"}}}\n\n"
            } else {
                r#"{"error":{"code":"server_is_overloaded","message":"busy"}}"#
            };
            let upstream = Router::new().fallback(any(move || async move {
                (
                    if sse {
                        StatusCode::OK
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    },
                    [(
                        "content-type",
                        if sse {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                    )],
                    payload,
                )
            }));
            let (upstream_url, upstream_task) = serve(upstream).await;
            let mut config = Config {
                upstream_url,
                ..Config::test_fixture()
            };
            config.retry.max_retries = 0;
            let (url, proxy_task) = serve(router(config).unwrap()).await;
            let response = reqwest::get(format!("{url}/v1/test")).await.unwrap();
            assert_eq!(response.status().as_u16(), if sse { 200 } else { 500 });
            let returned = response.text().await.unwrap();
            assert!(returned.contains("\"code\":\"server_error\""));
            assert!(returned.contains("Original upstream message: busy"));
            assert!(returned.contains("\"upstream_code\":\"server_is_overloaded\""));
            proxy_task.abort();
            upstream_task.abort();
        }
    }

    #[tokio::test]
    async fn permanent_quota_errors_do_not_retry_and_500_capacity_does() {
        for (status, body, expected_calls) in [
            (
                429,
                r#"{"error":{"code":"insufficient_quota","message":"quota exhausted"}}"#,
                1,
            ),
            (429, r#"{"error":{"type":"billing_hard_limit_reached"}}"#, 1),
            (
                500,
                r#"{"error":{"message":"model is out of capacity"}}"#,
                3,
            ),
            (500, r#"{"error":{"message":"internal server error"}}"#, 1),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let upstream = Router::new()
                .fallback(any(
                    move |State(calls): State<Arc<AtomicUsize>>| async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::from_u16(status).unwrap(),
                            [
                                ("content-type", "application/json"),
                                ("x-upstream", "retained"),
                            ],
                            body,
                        )
                    },
                ))
                .with_state(calls.clone());
            let (upstream_url, task) = serve(upstream).await;
            let mut config = Config {
                upstream_url,
                ..Config::test_fixture()
            };
            config.retry = crate::config::Retry {
                max_retries: 2,
                initial_delay_ms: 1,
                max_delay_ms: 2,
                recovery_timeout_ms: 0,
            };
            let (url, proxy_task) = serve(router(config).unwrap()).await;
            let response = reqwest::get(format!("{url}/v1/test")).await.unwrap();
            assert_eq!(response.status().as_u16(), status);
            assert_eq!(response.headers()["x-upstream"], "retained");
            let returned = response.text().await.unwrap();
            if expected_calls == 3 {
                let value: Value = serde_json::from_str(&returned).unwrap();
                assert_eq!(value["error"]["code"], "server_error");
                assert!(
                    value["error"]["message"]
                        .as_str()
                        .unwrap()
                        .ends_with("model is out of capacity")
                );
            } else {
                assert_eq!(returned, body);
            }
            assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
            proxy_task.abort();
            task.abort();
        }
    }

    #[tokio::test]
    async fn validates_json_and_preserves_large_binary_uploads() {
        let calls = Arc::new(AtomicUsize::new(0));
        let upstream = Router::new()
            .fallback(any(
                |State(calls): State<Arc<AtomicUsize>>, request: Request| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(request.headers()["authorization"], "Bearer something");
                    to_bytes(request.into_body(), 32 * 1024 * 1024)
                        .await
                        .unwrap()
                },
            ))
            .with_state(calls.clone());
        let (upstream_url, task) = serve(upstream).await;
        let (url, proxy_task) = serve(
            router(Config {
                upstream_url,
                ..Config::test_fixture()
            })
            .unwrap(),
        )
        .await;
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{url}/v1/test"))
            .header("content-type", "application/json")
            .body("{")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = client
            .post(format!("{url}/v1/test"))
            .body(vec![0u8; 16 * 1024 * 1024 + 1])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap().len(), 16 * 1024 * 1024 + 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let payload = b"binary\x00upload";
        let response = client
            .post(format!("{url}/v1/test"))
            .header("content-type", "application/octet-stream")
            .body(payload.as_slice())
            .send()
            .await
            .unwrap();
        assert_eq!(response.bytes().await.unwrap(), payload.as_slice());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        proxy_task.abort();
        task.abort();
    }

    #[tokio::test]
    async fn replays_uploads_and_preserves_failed_websocket_handshakes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let upstream = Router::new()
            .fallback(any(
                |State(calls): State<Arc<AtomicUsize>>, request: Request| async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    let upgrade = request.headers().contains_key("upgrade");
                    let bytes = to_bytes(request.into_body(), usize::MAX).await.unwrap();
                    if upgrade {
                        return (StatusCode::UNAUTHORIZED, [("x-error", "kept")], "no access")
                            .into_response();
                    }
                    assert_eq!(bytes, vec![42u8; 128 * 1024]);
                    if n == 0 {
                        (StatusCode::SERVICE_UNAVAILABLE, "capacity").into_response()
                    } else {
                        bytes.into_response()
                    }
                },
            ))
            .with_state(calls.clone());
        let (upstream_url, task) = serve(upstream).await;
        let mut config = Config {
            upstream_url,
            ..Config::test_fixture()
        };
        config.retry.initial_delay_ms = 1;
        let (url, proxy_task) = serve(router(config).unwrap()).await;
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{url}/v1/test"))
            .body(vec![42u8; 128 * 1024])
            .send()
            .await
            .unwrap();
        assert_eq!(response.bytes().await.unwrap(), vec![42u8; 128 * 1024]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let response = client
            .get(format!("{url}/v1/test"))
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()["x-error"], "kept");
        assert_eq!(response.text().await.unwrap(), "no access");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        proxy_task.abort();
        task.abort();
    }

    #[tokio::test]
    async fn forwards_all_methods_paths_and_model_locations() {
        let upstream = Router::new().fallback(any(|request: Request| async move {
            let (parts, body) = request.into_parts();
            axum::Json(json!({"method": parts.method.as_str(), "uri": parts.uri.to_string(), "key": parts.headers["authorization"].to_str().unwrap(), "body": String::from_utf8_lossy(&to_bytes(body, usize::MAX).await.unwrap())}))
        }));
        let (upstream_url, task) = serve(upstream).await;
        let (url, proxy_task) = serve(
            router(Config {
                upstream_url,
                ..Config::test_fixture()
            })
            .unwrap(),
        )
        .await;
        let client = reqwest::Client::new();
        for method in ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
            let result: Value = client
                .request(
                    method.parse().unwrap(),
                    format!("{url}/v1/future/resource?x=a%20b&x=c"),
                )
                .body("opaque")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(result["method"], method);
            assert_eq!(result["uri"], "/v1/future/resource?x=a%20b&x=c");
            assert_eq!(result["body"], "opaque");
        }
        for (path, expected_path, key) in [
            (
                "/v1/models/gpt-4.1?x=1",
                "/v1/models/gpt%2D4%2E1%2Dmini?x=1",
                "something",
            ),
            (
                "/v1/realtime?model=model-primary&x=a%20b",
                "/v1/realtime?model=model-primary&x=a%20b",
                "replace-with-primary-api-key",
            ),
            ("/v1/files/gpt-4.1", "/v1/files/gpt-4.1", "something"),
        ] {
            let result: Value = client
                .get(format!("{url}{path}"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(result["uri"], expected_path);
            assert_eq!(result["key"], format!("Bearer {key}"));
        }
        let result: Value = client
            .post(format!("{url}/v1/realtime/client_secrets"))
            .json(&json!({"session":{"model":"model-primary"}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result["key"], "Bearer replace-with-primary-api-key");
        proxy_task.abort();
        task.abort();
    }

    #[tokio::test]
    async fn multipart_preserves_binary_and_routes_models_even_after_files() {
        let payload = b"\x00\xffbinary\r\n--almost-boundary\x00";
        let upstream = Router::new().fallback(any(|request: Request| async move {
            let key = request.headers()["authorization"].clone();
            let boundary =
                multer::parse_boundary(request.headers()["content-type"].to_str().unwrap())
                    .unwrap();
            let mut form = multer::Multipart::new(request.into_body().into_data_stream(), boundary);
            let mut values = serde_json::Map::new();
            while let Some(field) = form.next_field().await.unwrap() {
                let name = field.name().unwrap().to_owned();
                values.insert(name, json!(field.bytes().await.unwrap().to_vec()));
            }
            axum::Json(json!({"key":key.to_str().unwrap(), "fields":values}))
        }));
        let (upstream_url, task) = serve(upstream).await;
        let mut config = Config {
            upstream_url,
            ..Config::test_fixture()
        };
        config.aliases[1].api_key = Some("primary".into());
        let (url, proxy_task) = serve(router(config).unwrap()).await;
        for (path, name, input, expected) in [
            (
                "/v1/audio/transcriptions",
                "model",
                "gpt-4.1",
                "gpt-4.1-mini",
            ),
            (
                "/v1/images/edits",
                "model",
                "model-primary",
                "model-primary",
            ),
            (
                "/v1/realtime/calls",
                "session",
                r#"{"model":"gpt-4.1"}"#,
                r#"{"model":"gpt-4.1-mini"}"#,
            ),
            (
                "/v1/realtime/calls?model=model-primary",
                "session",
                "{}",
                "{}",
            ),
        ] {
            let mut body = b"--testboundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n".to_vec();
            body.extend_from_slice(payload);
            body.extend_from_slice(format!("\r\n--testboundary\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{input}\r\n--testboundary--\r\n").as_bytes());
            let result: Value = reqwest::Client::new()
                .post(format!("{url}{path}"))
                .header(
                    "content-type",
                    "multipart/form-data; boundary=\"testboundary\"",
                )
                .body(body)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(result["key"], "Bearer replace-with-primary-api-key");
            assert_eq!(result["fields"]["file"], json!(payload.to_vec()));
            assert_eq!(result["fields"][name], json!(expected.as_bytes().to_vec()));
        }
        proxy_task.abort();
        task.abort();
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)] // Tungstenite callback requires an HTTP response error.
    async fn websocket_tunnels_frames_and_routes_handshake() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    assert_eq!(request.uri(), "/v1/realtime?model=gpt%2D4%2E1%2Dmini");
                    assert_eq!(request.headers()["authorization"], "Bearer something");
                    assert_eq!(request.headers()["sec-websocket-protocol"], "realtime");
                    response
                        .headers_mut()
                        .insert("sec-websocket-protocol", "realtime".parse().unwrap());
                    Ok(response)
                },
            )
            .await
            .unwrap();
            while let Some(Ok(message)) = socket.next().await {
                if message.is_close() {
                    break;
                }
                if let Message::Ping(bytes) = message {
                    socket.send(Message::Pong(bytes)).await.unwrap();
                } else {
                    socket.send(message).await.unwrap();
                }
            }
        });
        let (url, proxy_task) = serve(
            router(Config {
                upstream_url,
                ..Config::test_fixture()
            })
            .unwrap(),
        )
        .await;
        let mut request = format!("{}/v1/realtime?model=gpt-4.1", url.replace("http:", "ws:"))
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "sec-websocket-protocol",
            "realtime, openai-insecure-api-key.caller-secret"
                .parse()
                .unwrap(),
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        for message in [
            Message::Text("hello".into()),
            Message::Binary(vec![0, 255, 1].into()),
            Message::Ping(vec![4].into()),
        ] {
            socket.send(message.clone()).await.unwrap();
            let response = tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if message.is_ping() {
                assert!(response.is_pong());
            } else {
                assert_eq!(response, message);
            }
        }
        socket.close(None).await.unwrap();
        proxy_task.abort();
        task.abort();
    }

    #[tokio::test]
    async fn reports_public_ip_when_upstream_rejects_ip() {
        let upstream = Router::new().fallback(any(|| async {
            (
                StatusCode::UNAUTHORIZED,
                [("content-type", "application/json")],
                r#"{"error":{"message":"Your IP is not authorized to make this request.","type":"invalid_request_error","code":"ip_not_authorized"}}"#,
            )
        }));
        let lookup = Router::new().fallback(any(|| async { "203.0.113.7\n" }));
        let (upstream_url, upstream_task) = serve(upstream).await;
        let (lookup_url, lookup_task) = serve(lookup).await;
        let config = Config {
            upstream_url,
            ..Config::test_fixture()
        };
        let options = Options {
            logs: None,
            access_config: None,
            source: None,
            ip_lookup: IpLookup {
                v4: vec!["http://127.0.0.1:9/".into(), lookup_url],
                v6: vec![],
            },
            connectivity_probes: vec![],
        };
        let (url, proxy_task) = serve(router_with(config, options).unwrap()).await;
        let client = reqwest::Client::new();
        for _ in 0..2 {
            let response = client
                .post(format!("{url}/v1/responses"))
                .json(&json!({"model": "gpt-5", "input": "hi"}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let length: usize = response.headers()["content-length"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            let body = response.bytes().await.unwrap();
            assert_eq!(body.len(), length);
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], "ip_not_authorized");
            let message = body["error"]["message"].as_str().unwrap();
            assert!(message.starts_with("Your IP is not authorized to make this request."));
            assert!(message.contains("over IPv4 from 203.0.113.7"), "{message}");
        }
        proxy_task.abort();
        upstream_task.abort();
        lookup_task.abort();
    }

    #[tokio::test]
    async fn explains_whether_internet_or_upstream_is_down() {
        // Nothing listens on the discard port, so every upstream connection is refused.
        let mut config = Config {
            upstream_url: "http://127.0.0.1:9".into(),
            ..Config::test_fixture()
        };
        config.retry.recovery_timeout_ms = 0;
        let (probe_url, probe_task) = serve(Router::new().fallback(any(|| async { "" }))).await;
        for (probes, expected) in [
            (
                vec![probe_url],
                "your internet connection works, so 127.0.0.1 appears to be down",
            ),
            (
                vec!["http://127.0.0.1:9/".into()],
                "your internet connection appears to be down",
            ),
            (vec![], "connectivity probes are disabled"),
        ] {
            let options = Options {
                connectivity_probes: probes,
                ..Options::default()
            };
            let (url, proxy_task) = serve(router_with(config.clone(), options).unwrap()).await;
            let response = reqwest::Client::new()
                .post(format!("{url}/v1/responses"))
                .json(&json!({"model": "gpt-5", "input": "hi"}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            let body: Value = response.json().await.unwrap();
            let message = body["error"]["message"].as_str().unwrap();
            assert!(
                message.starts_with("Could not reach upstream 127.0.0.1 ("),
                "{message}"
            );
            assert!(message.contains("connection failed"), "{message}");
            assert!(message.contains(expected), "{message}");
            proxy_task.abort();
        }
        probe_task.abort();
    }

    #[tokio::test]
    async fn ip_version_binds_upstream_connections_to_one_family() {
        let upstream = Router::new().fallback(any(|| async { "ok" }));
        let (upstream_url, upstream_task) = serve(upstream).await;
        // The test upstream listens on 127.0.0.1, so an IPv6-only client cannot reach it.
        for (version, expected) in [
            (IpVersion::Auto, StatusCode::OK),
            (IpVersion::Ipv4, StatusCode::OK),
            (IpVersion::Ipv6, StatusCode::BAD_GATEWAY),
        ] {
            let mut config = Config {
                upstream_url: upstream_url.clone(),
                ip_version: version,
                ..Config::test_fixture()
            };
            config.retry.recovery_timeout_ms = 0;
            let (url, proxy_task) = serve(router(config).unwrap()).await;
            let response = reqwest::get(format!("{url}/v1/test")).await.unwrap();
            assert_eq!(response.status(), expected, "{version:?}");
            proxy_task.abort();
        }
        upstream_task.abort();
    }

    #[tokio::test]
    async fn reloads_changed_config_without_restart() {
        let upstream = Router::new().fallback(any(|headers: HeaderMap| async move {
            headers["authorization"].to_str().unwrap().to_owned()
        }));
        let (upstream_url, upstream_task) = serve(upstream).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut config = Config {
            upstream_url,
            ..Config::test_fixture()
        };
        config.api_keys.insert("default".into(), "first".into());
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let options = Options {
            source: Some((path.clone(), crate::config::fingerprint(&path))),
            ..Options::default()
        };
        let (url, proxy_task) = serve(router_with(config.clone(), options).unwrap()).await;
        let client = reqwest::Client::new();
        let get = || client.get(format!("{url}/v1/test")).send();
        assert_eq!(get().await.unwrap().text().await.unwrap(), "Bearer first");

        // A valid edit applies to the very next request.
        tokio::time::sleep(Duration::from_millis(20)).await;
        config.api_keys.insert("default".into(), "second".into());
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(get().await.unwrap().text().await.unwrap(), "Bearer second");

        // A broken edit is ignored and the last good config stays in force.
        tokio::time::sleep(Duration::from_millis(20)).await;
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(get().await.unwrap().text().await.unwrap(), "Bearer second");

        // Deleting the file keeps the last good config and does not recreate it.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(get().await.unwrap().text().await.unwrap(), "Bearer second");
        assert!(!path.exists());

        // Fixing the file picks it up again.
        config.api_keys.insert("default".into(), "third".into());
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(get().await.unwrap().text().await.unwrap(), "Bearer third");
        proxy_task.abort();
        upstream_task.abort();
    }

    #[test]
    fn detects_ip_not_authorized_by_code_or_message() {
        let by_code = br#"{"error":{"code":"ip_not_authorized","message":"x"}}"#;
        let by_text = b"Your IP is not authorized to make this request.";
        assert!(ip_not_authorized(StatusCode::UNAUTHORIZED, by_code));
        assert!(ip_not_authorized(StatusCode::FORBIDDEN, by_text));
        assert!(!ip_not_authorized(StatusCode::OK, by_code));
        assert!(!ip_not_authorized(
            StatusCode::UNAUTHORIZED,
            br#"{"error":{"code":"invalid_api_key"}}"#
        ));
    }

    #[tokio::test]
    async fn streams_first_chunk_before_upstream_finishes() {
        let upstream = Router::new().fallback(any(|| async {
            let stream = async_stream::stream! {
                yield Ok::<_, std::io::Error>(Bytes::from_static(b"data: first\n\n"));
                tokio::time::sleep(Duration::from_secs(2)).await;
                yield Ok::<_, std::io::Error>(Bytes::from_static(b"data: [DONE]\n\n"));
            };
            (
                [("content-type", "text/event-stream")],
                Body::from_stream(stream),
            )
        }));
        let (upstream_url, task) = serve(upstream).await;
        let (url, proxy_task) = serve(
            router(Config {
                upstream_url,
                ..Config::test_fixture()
            })
            .unwrap(),
        )
        .await;
        let mut response = tokio::time::timeout(
            Duration::from_secs(1),
            reqwest::get(format!("{url}/v1/test")),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let chunk = tokio::time::timeout(Duration::from_secs(1), response.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(chunk, "data: first\n\n");
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: [DONE]\n\n");
        proxy_task.abort();
        task.abort();
    }
}
