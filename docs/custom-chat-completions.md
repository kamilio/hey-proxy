# Custom Chat Completions

Set an OpenAI-compatible chat client's base URL to **`http://127.0.0.1:8080/v1/custom`**. Its normal `/chat/completions` suffix then reaches:

```
POST /v1/custom/chat/completions
```

This opt-in endpoint translates Chat Completions requests to Responses, then translates the reply back to Chat Completions. It works with the configured OpenAI upstream and with `gemini/MODEL` through the existing Responses-to-Gemini converter. The ordinary `/v1/chat/completions` endpoint continues to forward requests unchanged apart from configured overwrites.

```sh
curl http://127.0.0.1:8080/v1/custom/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "gemini/gemini-2.5-flash",
    "messages": [{"role": "user", "content": "Hello!"}],
    "reasoning": {"effort": "low"},
    "stream": true,
    "stream_options": {"include_usage": true}
  }'
```

Configure Gemini and its credentials before using a Gemini model. In standalone mode the proxy supplies provider credentials; host mode still requires its host access key. Client relays forward this custom path to the host, where conversion runs.

## Overwrites and fallbacks

Rules match the **incoming** `chat_completions` API shape, including on this custom endpoint. For example:

```json
"aliases": [
  {"from": "coding", "to": "responses-model", "api_shape": "responses"},
  {"from": "coding", "to": "gemini/gemini-2.5-flash", "api_shape": "chat_completions"}
]
```

Model, project, reasoning settings, and fallback candidates use the same snapshot of those rules. The adapter does not recursively apply the Responses-specific rules after conversion. The returned Chat Completions `model` stays the name requested by the client; the dashboard records the actual routed model.

## Reasoning and tool continuity

The interface follows [OpenRouter's reasoning fields](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens):

- `reasoning_effort` or `reasoning: {"effort":"low"}` selects effort. `reasoning.enabled` and `include_reasoning` are also accepted. Requesting reasoning enables a summary unless an explicit summary setting is supplied. The provider decides which effort levels and summaries it supports.
- `message.reasoning` contains the available reasoning summary. It is not a promise of access to private model reasoning.
- `message.reasoning_details` carries `reasoning.summary`, provider-supplied `reasoning.text`, and `reasoning.encrypted` entries with `id`, `format`, and `index`. Streaming uses the same fields on `choices[0].delta`.
- Preserve the complete assistant message, including `reasoning_details` and `tool_calls`, in subsequent `messages`. For a stream, concatenate text and tool arguments, merge reasoning details by `index`, and concatenate their `summary`, `text`, or `data` fields in order. The encrypted entry arrives when its reasoning item completes; wait for the terminal chunk before starting the next turn.

OpenAI's opaque reasoning bytes are passed back through the Responses API. Gemini's `google-gemini-v1` encrypted entry is a **hey-proxy carrier**, not a raw OpenRouter or Google signature. It preserves the signed native turn and restores its original message/tool boundaries after checking the flattened assistant message. It requires the same Gemini model and proxy reasoning key. Altered content, a changed model, or an invalid carrier produces a clear error instead of dropping the signature.

Clients that discard unknown message fields still work for ordinary text, but can lose reasoning and Gemini tool continuity. `reasoning.exclude: true` or `include_reasoning: false` omits both reasoning fields, including replay data; use it only when you do not need that continuity. Plaintext `reasoning`/`reasoning_content` in history is not injected into user or system instructions.

## Supported conversions and limits

The adapter handles system/developer/user/assistant messages, text, user images and files, function tools and tool results, parallel tool calls, structured output, token limits, sampling settings, streaming, refusals, and usage. `max_completion_tokens` takes precedence over `max_tokens`. Both become `max_output_tokens`, which includes reasoning tokens. Chat tool schemas remain non-strict unless explicitly marked strict. Storage defaults to `false`; encrypted reasoning is requested for stateless replay.

Streaming emits `chat.completion.chunk` events, tool-call indices, finish reasons, optional final usage, and `[DONE]`. Upstream HTTP errors keep their status and body. Mid-stream errors or a missing terminal Responses event produce an error event followed by `[DONE]`, without a successful finish reason. Heartbeats keep quiet streams active. Requests and buffered responses are limited to 64 MiB; individual SSE frames to 16 MiB.

This is a lossy adapter, not full Chat Completions emulation. It flattens assistant text and tools into one choice and does not preserve message `name` labels. It rejects multiple choices, nonempty stop sequences, nonzero presence/frequency penalties, logprobs, audio, legacy `functions`/`function_call`, unsupported content, and options without a usable mapping. Provider restrictions still apply, including Gemini's stateless storage and media limitations. It does not synthesize reasoning that the provider did not return.
