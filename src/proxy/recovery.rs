use super::*;
use std::future::Future;
use tokio::time::{Instant as Deadline, timeout_at};

/// One budget spans attempts, backoff, and response-prefix inspection. Healthy streams
/// are not subject to it. A WebSocket gets a new budget for each recovery episode.
#[derive(Clone)]
pub(super) struct Recovery {
    deadline: Option<Deadline>,
    disabled: bool,
    pub retries: u32,
}

impl Recovery {
    pub fn new(config: &Config) -> Self {
        Self {
            disabled: false,
            deadline: (config.retry.max_retries > 0 && config.retry.recovery_timeout_ms > 0)
                .then(|| Deadline::now() + Duration::from_millis(config.retry.recovery_timeout_ms)),
            retries: 0,
        }
    }
    /// Fallback attempts wait for failure without nested retries; keep the original shared config snapshot.
    pub fn for_request(proxy: &Proxy) -> Self {
        if proxy.fallback_attempt {
            Self {
                deadline: None,
                disabled: true,
                retries: 0,
            }
        } else {
            Self::new(&proxy.config)
        }
    }
    pub fn timed(&self) -> bool {
        self.deadline.is_some()
    }
    pub async fn run<T>(&self, future: impl Future<Output = T>) -> Result<T, ()> {
        if let Some(deadline) = self.deadline {
            if Deadline::now() >= deadline {
                return Err(());
            }
            timeout_at(deadline, future).await.map_err(|_| ())
        } else {
            Ok(future.await)
        }
    }
    pub async fn retry(&mut self, config: &Config, headers: &HeaderMap) -> bool {
        if self.disabled
            || config.retry.max_retries == 0
            || (!self.timed() && self.retries >= config.retry.max_retries)
        {
            return false;
        }
        let mut delay = retry_delay(config, self.retries, headers);
        if let Some(deadline) = self.deadline {
            // A zero-delay config must not create a tight loop against an unavailable service.
            delay = delay.max(Duration::from_millis(10));
            if Deadline::now() + delay >= deadline {
                tokio::time::sleep_until(deadline).await;
                return false;
            }
        }
        tokio::time::sleep(delay).await;
        self.retries = self.retries.saturating_add(1);
        true
    }
}

#[derive(Clone)]
pub(super) struct Timeout;

pub(super) fn timeout_response() -> Response {
    let mut response = (StatusCode::GATEWAY_TIMEOUT, axum::Json(json!({"error":{
        "type":"server_error", "code":"server_error", "proxy_code":"proxy_recovery_timeout",
        "message":"hey-proxy internal recovery time budget exhausted before a response could be forwarded; retry the request."
    }}))).into_response();
    response.extensions_mut().insert(Timeout);
    response
}

/// Repeating a request after a known connection failure is safe: no HTTP request
/// reached the upstream. A read/write timeout is ambiguous and must not be replayed.
pub(super) fn connection_failure(error: &reqwest::Error) -> bool {
    if !error.is_connect() {
        return false;
    }
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    !text.to_ascii_lowercase().contains("certificate")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn time_budget_extends_attempt_count_and_caps_pending_work() {
        let mut config = Config::test_fixture();
        config.retry.max_retries = 1;
        config.retry.initial_delay_ms = 1;
        config.retry.max_delay_ms = 1;
        config.retry.recovery_timeout_ms = 100;
        let mut budget = Recovery::new(&config);
        assert!(budget.retry(&config, &HeaderMap::new()).await);
        assert!(budget.retry(&config, &HeaderMap::new()).await);
        assert_eq!(budget.retries, 2);
        assert!(budget.run(std::future::pending::<()>()).await.is_err());
        assert!(!budget.retry(&config, &HeaderMap::new()).await);
    }
    #[tokio::test]
    async fn count_mode_and_disabled_retries_still_work() {
        let mut config = Config::test_fixture();
        config.retry.recovery_timeout_ms = 0;
        config.retry.max_retries = 1;
        config.retry.initial_delay_ms = 0;
        let mut budget = Recovery::new(&config);
        assert!(budget.retry(&config, &HeaderMap::new()).await);
        assert!(!budget.retry(&config, &HeaderMap::new()).await);
        config.retry.max_retries = 0;
        config.retry.recovery_timeout_ms = 100;
        let mut budget = Recovery::new(&config);
        assert!(!budget.timed());
        assert!(!budget.retry(&config, &HeaderMap::new()).await);
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, task)
    }
    fn short_config(upstream_url: String) -> Config {
        let mut config = Config {
            upstream_url,
            ..Config::test_fixture()
        };
        config.retry.max_retries = 1;
        config.retry.initial_delay_ms = 1;
        config.retry.max_delay_ms = 1;
        config.retry.recovery_timeout_ms = 300;
        config
    }
    #[tokio::test]
    async fn http_and_sse_refusals_recover_beyond_attempt_limit_without_leaking_failed_ids() {
        for sse in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let count = calls.clone();
            let app = Router::new().fallback(move || {
                let n = count.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < 3 {
                        if sse {
                            (StatusCode::OK, [("content-type", "text/event-stream")],
                                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"failed-id\"}}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"failed-id\",\"error\":{\"code\":\"server_is_overloaded\",\"message\":\"busy\"}}}\n\n").into_response()
                        } else {
                            (StatusCode::SERVICE_UNAVAILABLE, "capacity").into_response()
                        }
                    } else if sse {
                        ([("content-type", "text/event-stream")], "data: {\"type\":\"response.output_text.delta\",\"delta\":\"recovered\"}\n\n").into_response()
                    } else { "recovered".into_response() }
                }
            });
            let (upstream_url, upstream) = serve(app).await;
            let (url, proxy) = serve(router(short_config(upstream_url)).unwrap()).await;
            let client = reqwest::Client::new();
            let response = client
                .post(format!("{url}/v1/responses"))
                .json(&json!({"model":"gpt-4.1"}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let body = response.text().await.unwrap();
            assert!(body.contains("recovered"));
            assert!(!body.contains("failed-id"));
            assert_eq!(calls.load(Ordering::SeqCst), 4);
            let logs: Value = client
                .get(format!("{url}/logs/api?local=true"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(logs["entries"][0]["retries"], 3);
            proxy.abort();
            upstream.abort();
        }
    }
    #[tokio::test]
    async fn deadline_bounds_headers_error_bodies_and_sse_preludes() {
        for phase in 0..3 {
            let app = Router::new().fallback(move || async move {
                if phase == 0 { tokio::time::sleep(Duration::from_secs(2)).await; }
                let body = Body::from_stream(async_stream::stream! {
                    if phase == 2 {
                        yield Ok::<_, std::io::Error>(Bytes::from_static(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"waiting\"}}\n\n"));
                    }
                    std::future::pending::<()>().await;
                });
                let mut response = Response::new(body);
                *response.status_mut() = if phase == 1 {StatusCode::SERVICE_UNAVAILABLE} else {StatusCode::OK};
                response.headers_mut().insert(header::CONTENT_TYPE, if phase == 2 {"text/event-stream"} else {"application/json"}.parse().unwrap());
                response
            });
            let (upstream_url, upstream) = serve(app).await;
            let mut config = short_config(upstream_url);
            config.retry.recovery_timeout_ms = 70;
            let (url, proxy) = serve(router(config).unwrap()).await;
            let response = tokio::time::timeout(
                Duration::from_secs(1),
                reqwest::get(format!("{url}/v1/test")),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(response.status(), 504, "phase {phase}");
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["error"]["proxy_code"], "proxy_recovery_timeout");
            proxy.abort();
            upstream.abort();
        }
    }
    #[tokio::test]
    async fn output_and_permanent_failures_are_never_replayed() {
        for body in [
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"already sent\"}\n\ndata: {\"type\":\"error\",\"error\":{\"code\":\"server_is_overloaded\"}}\n\n",
            "data: {\"type\":\"error\",\"error\":{\"code\":\"insufficient_quota\",\"message\":\"at capacity\"}}\n\n",
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let count = calls.clone();
            let (upstream_url, upstream) = serve(Router::new().fallback(move || {
                count.fetch_add(1, Ordering::SeqCst);
                async move { ([("content-type", "text/event-stream")], body) }
            }))
            .await;
            let (url, proxy) = serve(router(short_config(upstream_url)).unwrap()).await;
            let result = reqwest::get(format!("{url}/v1/test"))
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(!result.is_empty());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            proxy.abort();
            upstream.abort();
        }
    }
    #[tokio::test]
    async fn connection_failure_recovers_when_upstream_returns() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let upstream = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let listener = tokio::net::TcpListener::bind(address).await.unwrap();
            axum::serve(listener, Router::new().fallback(|| async { "online" }))
                .await
                .unwrap();
        });
        let config = short_config(format!("http://{address}"));
        let (url, proxy) = serve(router(config).unwrap()).await;
        assert_eq!(
            reqwest::get(format!("{url}/v1/test"))
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "online"
        );
        proxy.abort();
        upstream.abort();
    }
}
