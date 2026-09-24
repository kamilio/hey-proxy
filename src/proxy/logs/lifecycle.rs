use super::{Store, store::now_ms};
use axum::body::{Body, Bytes};
use http_body::{Body as HttpBody, Frame, SizeHint};
use serde_json::json;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

pub struct RequestGuard {
    store: Arc<Store>,
    id: u64,
    bytes: u64,
    done: bool,
}
impl RequestGuard {
    pub fn new(store: Arc<Store>, id: u64) -> Self {
        Self {
            store,
            id,
            bytes: 0,
            done: false,
        }
    }
    pub fn finish(&mut self, outcome: &str, source: &str, code: Option<&str>) {
        if !self.done {
            self.store
                .complete(self.id, outcome, source, code, self.bytes);
            self.done = true;
        }
    }
    pub fn add_bytes(&mut self, bytes: usize) {
        if self.bytes == 0 && bytes > 0 {
            self.store.first_byte(self.id);
        }
        self.bytes = self.bytes.saturating_add(bytes as u64);
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn wrap(mut self, inner: Body, expected_length: Option<u64>, is_sse: bool) -> Body {
        if inner.is_end_stream() {
            self.finish(
                "succeeded",
                if is_sse { "stream_end" } else { "http" },
                None,
            );
        }
        Body::new(TrackedBody {
            inner,
            guard: self,
            expected_length,
            is_sse,
        })
    }
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.finish(
            "cancelled",
            "client_disconnect",
            Some("client_disconnected"),
        );
    }
}

struct TrackedBody {
    inner: Body,
    guard: RequestGuard,
    expected_length: Option<u64>,
    is_sse: bool,
}
impl HttpBody for TrackedBody {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_frame(context);
        match &result {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    if this.guard.bytes == 0 && !data.is_empty() {
                        this.guard.store.first_byte(this.guard.id);
                    }
                    this.guard.bytes = this.guard.bytes.saturating_add(data.len() as u64);
                }
                if this.inner.is_end_stream()
                    || this.expected_length.is_some_and(|n| this.guard.bytes >= n)
                {
                    this.guard.finish(
                        "succeeded",
                        if this.is_sse { "stream_end" } else { "http" },
                        None,
                    );
                }
            }
            Poll::Ready(None) => this.guard.finish(
                "succeeded",
                if this.is_sse { "stream_end" } else { "http" },
                None,
            ),
            Poll::Ready(Some(Err(_))) => {
                this.guard
                    .finish("failed", "transport", Some("upstream_stream_error"))
            }
            Poll::Pending => {}
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

pub struct Attempt {
    store: Arc<Store>,
    id: u64,
    number: u32,
    started: Instant,
    finished: bool,
}
impl Attempt {
    pub fn new(store: Arc<Store>, id: u64, phase: &str) -> Self {
        let mut number = 0;
        store.update(
            id,
            "attempt_started",
            json!({"phase":phase,"started_ms":now_ms()}),
            |entry| {
                entry.attempts = entry.attempts.saturating_add(1);
                number = entry.attempts;
                true
            },
        );
        Self {
            store,
            id,
            number,
            started: Instant::now(),
            finished: false,
        }
    }
    pub fn finish(&mut self, outcome: &str, status: Option<u16>, code: Option<&str>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.store.update(
            self.id,
            "attempt_finished",
            json!({"attempt":self.number,"outcome":outcome,"status":status,
            "error_code":code,"duration_ms":self.started.elapsed().as_millis() as u64}),
            |_| true,
        );
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        self.finish("interrupted", None, Some("attempt_interrupted"));
    }
}
