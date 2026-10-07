//! Exact, local connectivity probe. Inspect original user content before routing/guidance.
use super::*;
use http_body::{Body as HttpBody, Frame, SizeHint};
use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

const PROMPT: &str = "hello-hey-proxy";
const REPLY: &str = "hello-dude";
const INSPECTION_LIMIT: usize = 64 * 1024 * 1024;

// Replay inspected frames (including trailers) when a request is not a probe.
// Large requests resume normal forwarding without buffering the entire body.
struct InspectedBody {
    frames: VecDeque<Frame<Bytes>>,
    inner: Body,
}

impl HttpBody for InspectedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        if let Some(frame) = self.frames.pop_front() {
            Poll::Ready(Some(Ok(frame)))
        } else {
            Pin::new(&mut self.inner).poll_frame(cx)
        }
    }

    fn size_hint(&self) -> SizeHint {
        let buffered = self
            .frames
            .iter()
            .filter_map(Frame::data_ref)
            .map(|v| v.len() as u64)
            .sum::<u64>();
        let mut hint = SizeHint::new();
        hint.set_lower(self.inner.size_hint().lower().saturating_add(buffered));
        if let Some(upper) = self
            .inner
            .size_hint()
            .upper()
            .and_then(|v| v.checked_add(buffered))
        {
            hint.set_upper(upper);
        }
        hint
    }
}

#[derive(Clone, Copy)]
enum Shape {
    Responses,
    Chat,
    Messages,
    Gemini,
}

fn shape(path: &str) -> Option<Shape> {
    match path.trim_end_matches('/') {
        "/v1/responses" => Some(Shape::Responses),
        "/v1/chat/completions" | "/v1/custom/chat/completions" => Some(Shape::Chat),
        "/v1/messages" | "/v1/custom/messages" | "/custom/v1/messages" => Some(Shape::Messages),
        path if gemini::native_path(path)
            && path.contains("/models/")
            && (path.ends_with(":generateContent") || path.ends_with(":streamGenerateContent")) =>
        {
            Some(Shape::Gemini)
        }
        _ => None,
    }
}

fn text_matches(content: &Value, kind: &str) -> bool {
    if let Some(text) = content.as_str() {
        return text == PROMPT;
    }
    content.as_array().is_some_and(|parts| {
        parts.len() == 1 && parts[0]["type"] == kind && parts[0]["text"] == PROMPT
    })
}

fn matches(shape: Shape, input: &Value) -> bool {
    let (field, kind) = match shape {
        Shape::Responses => {
            if input["input"].as_str() == Some(PROMPT) {
                return true;
            }
            ("input", "input_text")
        }
        Shape::Chat | Shape::Messages => ("messages", "text"),
        Shape::Gemini => {
            return input["contents"]
                .as_array()
                .and_then(|v| v.last())
                .is_some_and(|message| {
                    message["role"] == "user"
                        && message["parts"].as_array().is_some_and(|parts| {
                            parts.len() == 1
                                && parts[0].as_object().is_some_and(|part| part.len() == 1)
                                && parts[0]["text"] == PROMPT
                        })
                });
        }
    };
    input[field]
        .as_array()
        .and_then(|v| v.last())
        .is_some_and(|message| {
            message["role"] == "user"
                && message.get("type").is_none_or(|kind| kind == "message")
                && message.get("tool_calls").is_none()
                && message.get("function_call").is_none()
                && text_matches(&message["content"], kind)
        })
}

/// Authentication has already run. Non-probes retain their original bytes and headers.
#[allow(clippy::result_large_err)] // The alternate result is a complete local HTTP response.
pub(super) async fn intercept(request: Request) -> Result<Request, Response> {
    let Some(shape) = shape(request.uri().path()) else {
        return Ok(request);
    };
    let json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| {
            v.trim().eq_ignore_ascii_case("application/json") || v.trim().ends_with("+json")
        });
    if request.method() != axum::http::Method::POST
        || !json
        || request.headers().contains_key(header::UPGRADE)
        || request
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
    {
        return Ok(request);
    }
    let (parts, body) = request.into_parts();
    let mut body = InspectedBody {
        frames: VecDeque::new(),
        inner: body,
    };
    let mut bytes = Vec::new();
    while let Some(frame) =
        std::future::poll_fn(|cx| Pin::new(&mut body.inner).poll_frame(cx)).await
    {
        let frame = match frame {
            Ok(frame) => frame,
            Err(_) => {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "Could not read request body",
                ));
            }
        };
        if let Some(data) = frame.data_ref() {
            if bytes.len().saturating_add(data.len()) > INSPECTION_LIMIT {
                body.frames.push_back(frame);
                return Ok(Request::from_parts(parts, Body::new(body)));
            }
            bytes.extend_from_slice(data);
        }
        body.frames.push_back(frame);
    }
    if let Ok(input) = serde_json::from_slice::<Value>(&bytes)
        && matches(shape, &input)
    {
        let streaming =
            input["stream"] == true || parts.uri.path().ends_with(":streamGenerateContent");
        return Err(reply(shape, &input, streaming));
    }
    Ok(Request::from_parts(parts, Body::new(body)))
}

fn reply(shape: Shape, input: &Value, streaming: bool) -> Response {
    let suffix = format!("{:016x}", rand::random::<u64>());
    let id = format!("resp_hey_proxy_{suffix}");
    let message_id = format!("msg_hey_proxy_{suffix}");
    let model = input["model"].as_str().unwrap_or("hey-proxy");
    let created = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let usage = json!({"input_tokens":0,"output_tokens":0,"total_tokens":0});
    let (value, events, named, done) = match shape {
        Shape::Responses => {
            let part = json!({"type":"output_text","text":REPLY,"annotations":[],"logprobs":[]});
            let item = json!({"id":message_id,"type":"message","role":"assistant","status":"completed","content":[part]});
            let value = json!({"id":id,"object":"response","created_at":created,"status":"completed","model":model,"output":[item],"error":null,"incomplete_details":null,"usage":usage,"store":false});
            let mut start = value.clone();
            start["status"] = json!("in_progress");
            start["output"] = json!([]);
            start["usage"] = Value::Null;
            let mut empty_item = item.clone();
            empty_item["status"] = json!("in_progress");
            empty_item["content"] = json!([]);
            let mut empty_part = part.clone();
            empty_part["text"] = json!("");
            let mut events = vec![
                json!({"type":"response.created","response":start}),
                json!({"type":"response.in_progress","response":start}),
                json!({"type":"response.output_item.added","output_index":0,"item":empty_item}),
                json!({"type":"response.content_part.added","output_index":0,"item_id":message_id,"content_index":0,"part":empty_part}),
                json!({"type":"response.output_text.delta","output_index":0,"item_id":message_id,"content_index":0,"delta":REPLY,"logprobs":[]}),
                json!({"type":"response.output_text.done","output_index":0,"item_id":message_id,"content_index":0,"text":REPLY,"logprobs":[]}),
                json!({"type":"response.content_part.done","output_index":0,"item_id":message_id,"content_index":0,"part":part}),
                json!({"type":"response.output_item.done","output_index":0,"item":item}),
                json!({"type":"response.completed","response":value}),
            ];
            for (index, event) in events.iter_mut().enumerate() {
                event["sequence_number"] = json!(index);
            }
            (value, events, true, false)
        }
        Shape::Chat => {
            let id = format!("chatcmpl_hey_proxy_{suffix}");
            let usage = json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":0});
            let value = json!({"id":id,"object":"chat.completion","created":created,"model":model,"choices":[{"index":0,"message":{"role":"assistant","content":REPLY},"finish_reason":"stop"}],"usage":usage});
            let mut chunk = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[{"index":0,"delta":{"role":"assistant","content":REPLY},"finish_reason":null}]});
            let first = chunk.clone();
            chunk["choices"][0]["delta"] = json!({});
            chunk["choices"][0]["finish_reason"] = json!("stop");
            let mut events = vec![first, chunk.clone()];
            if input.pointer("/stream_options/include_usage") == Some(&json!(true)) {
                chunk["choices"] = json!([]);
                chunk["usage"] = usage;
                events.push(chunk);
            }
            (value, events, false, true)
        }
        Shape::Messages => {
            let value = json!({"id":message_id,"type":"message","role":"assistant","model":model,"content":[{"type":"text","text":REPLY}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}});
            let mut start = value.clone();
            start["content"] = json!([]);
            start["stop_reason"] = Value::Null;
            let events = vec![
                json!({"type":"message_start","message":start}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":REPLY}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":0}}),
                json!({"type":"message_stop"}),
            ];
            (value, events, true, false)
        }
        Shape::Gemini => {
            let value = json!({"candidates":[{"content":{"role":"model","parts":[{"text":REPLY}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":0,"candidatesTokenCount":0,"totalTokenCount":0}});
            (value.clone(), vec![value], false, false)
        }
    };
    let mut response = if streaming {
        let mut body = String::new();
        for event in events {
            if named {
                body.push_str(&format!("event: {}\n", event["type"].as_str().unwrap()));
            }
            body.push_str(&format!("data: {event}\n\n"));
        }
        if done {
            body.push_str("data: [DONE]\n\n");
        }
        (
            [
                (header::CONTENT_TYPE, "text/event-stream"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body,
        )
            .into_response()
    } else {
        axum::Json(value).into_response()
    };
    response.headers_mut().insert(
        "x-hey-proxy-probe",
        header::HeaderValue::from_static("true"),
    );
    response
}

#[cfg(test)]
mod tests;
