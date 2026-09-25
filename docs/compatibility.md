# API compatibility

## OpenAI

OpenAI requests use the configured upstream and credential. JSON model names and selected reasoning settings can be overwritten. HTTP bodies, streaming Responses, Chat Completions, and supported WebSocket routes are forwarded through the existing endpoint paths.

Keep request compression disabled when using model overwrites or Responses-to-Gemini conversion. The optional Codex setup commands configure this setting for you.

## Chat clients using Responses or Gemini

The optional [custom Chat Completions adapter](custom-chat-completions.md) exposes `/v1/custom/chat/completions`, with streaming, function calls, structured output, and OpenRouter-style reasoning details. Use `/v1/custom` as the client base URL.

## Gemini through the Responses API

Use `gemini/MODEL_NAME` on `/v1/responses`. The converter supports text, images, function calls and results, structured output, streaming, usage, and signed reasoning replay. Gemini's capabilities still depend on the selected model and endpoint.

The converter preserves provider signatures in opaque reasoning items so they can be sent back on subsequent turns. Return reasoning and tool-call items in order; do not strip or edit their encrypted content. The proxy stores a private reasoning-encryption key beside its config. Preserve this key across restarts if you need to continue existing conversations.

Providers have different schemas and capabilities. Unsupported options or incompatible signed history produce explicit errors; the proxy does not silently discard them. Stateful OpenAI features such as `previous_response_id` are not a portable replacement for sending full history to Gemini. Hosted tools also depend on provider support.

`providers.gemini.thinking` accepts `auto`, `budget`, or `level`. Auto selects levels for the Gemini 3 model family and budgets otherwise. Use an explicit setting when another model requires a particular thinking format. Reasoning summaries reflect the provider's available thought summaries, not a guarantee of access to hidden reasoning.

For provider-specific options, the request's `gemini.native_request` extension accepts supported native Gemini fields. Native request validation still applies. Use the native Gemini endpoint if your client already speaks Gemini's API.

## Streams and retries

Once output has been forwarded, hey-proxy does not switch models mid-stream. A disconnected stream may need client recovery. Preserve the conversation history when retrying.

With fallback rules configured, Responses WebSocket upgrades return HTTP 426 so clients supporting HTTP fallback can use SSE. Native Gemini endpoints do not use cross-model fallback rules.

Without an eligible fallback chain, the proxy's `retry` settings control same-model recovery. To disable the elapsed recovery budget, set `retry.recovery_timeout_ms` to `0`; retries then use the configured attempt count. [Fallback chains](fallbacks.md) make one attempt per candidate and do not use that timed retry loop.
