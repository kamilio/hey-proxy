# API compatibility

## OpenAI

OpenAI requests use the configured upstream and credential. JSON model names and selected reasoning settings can be overwritten. HTTP bodies, streaming Responses, Chat Completions, and supported WebSocket routes are forwarded through the existing endpoint paths.

For HTTP Responses requests routed to OpenAI, the proxy removes Gemini reasoning capsules marked `hey_gemini_v1.` from the outgoing attempt. OpenAI cannot decrypt these capsules when a conversation moves from Gemini to OpenAI or supplies Gemini history to a separate reviewer. Visible messages, tool calls/results, review instructions, and the proposed action are preserved. Native OpenAI reasoning, compaction items, and unknown encrypted formats remain unchanged; upstream validation failures are returned without stripping more context or retrying them. Client relays leave this decision to their host. WebSocket replay does not perform this conversion.

To check a configured route using fresh synthetic history, run `python3 tests/review_context_live.py --parent-model YOUR_PARENT_MODEL --reviewer-model YOUR_REVIEWER_MODEL`. Add `--require-encrypted` to require an encrypted parent reply, or `--context compaction` to test a fresh checkpoint. These manual checks make billable model requests, print only result metadata, and test both an authorized read and an unauthorized destructive action description. They execute neither action and do not verify a live Codex owner's approval flow.

Keep request compression disabled when using model overwrites or Responses-to-Gemini conversion. The optional Codex setup commands configure this setting for you.

## Chat clients using Responses or Gemini

The optional [custom Chat Completions adapter](custom-chat-completions.md) exposes `/v1/custom/chat/completions`, with streaming, function calls, structured output, and OpenRouter-style reasoning details. Use `/v1/custom` as the client base URL.

## Gemini through the Responses API

Use `gemini/MODEL_NAME` on `/v1/responses`. The converter supports text, images, function calls and results, structured output, streaming, usage, and signed reasoning replay. Gemini's capabilities still depend on the selected model and endpoint.

The converter preserves provider signatures in opaque reasoning items so they can be sent back on subsequent turns. Return reasoning and tool-call items in order; do not strip or edit their encrypted content. The proxy stores a private reasoning-encryption key beside its config. Preserve this key across restarts if you need to continue existing conversations.

Providers have different schemas and capabilities. When routing to Gemini, foreign reasoning items are omitted; Gemini's own capsules replay their native signatures only when the original model and key authenticate them and their visible output matches. After a model or key change, unreadable capsules are omitted and visible history is imported without the old signatures. Malformed capsules and altered visible output attached to an authenticated capsule still produce errors. Unsupported options or incompatible native signed history produce explicit errors. Stateful OpenAI features such as `previous_response_id` are not a portable replacement for sending full history to Gemini. Hosted tools also depend on provider support.

Responses `tool_search` is shimmed through Gemini function calling. With `execution: "client"`, the proxy returns a `tool_search_call` for the caller to execute; replay its `tool_search_output` to load the returned functions or namespaces. With hosted search (the default), Gemini selects catalog paths, the proxy loads their original definitions, and generation continues within the same response. Deferred schemas stay unavailable until loaded. Search calls, results, names, namespace metadata, schemas, and signed native turns are retained for full-history replay. Strict function schemas are validated locally before calls are released.

Responses `web_search` (including `web_search_preview` and dated versions) is mapped to Gemini's Google Search grounding, alongside any function tools. Gemini decides when to search and answers with grounded text in the same turn; there is no `web_search_call` item or URL citation annotation. Search queries and sources remain in the native `gemini` response metadata. Domain filters, user location, context size, and a forced `web_search` tool choice are hints Gemini cannot enforce. Declare `web_search` in the request `tools`, not in `additional_tools` or a namespace.

Hosted discovery is limited to 16 native turns and shares the request's output-token budget. Its SSE output is emitted after each native turn completes, with keepalives while waiting; client search keeps incremental streaming. Gemini's declaration placement and caching differ from OpenAI's, so identical prompt-cache behavior is not guaranteed. Other OpenAI-hosted services, including MCP server execution, still require provider support. The wire format follows [OpenAI tool search](https://developers.openai.com/api/docs/guides/tools-tool-search).

`providers.gemini.thinking` accepts `auto`, `budget`, or `level`. Auto selects levels for the Gemini 3 model family and budgets otherwise. Use an explicit setting when another model requires a particular thinking format. Reasoning summaries reflect the provider's available thought summaries, not a guarantee of access to hidden reasoning.

For provider-specific options, the request's `gemini.native_request` extension accepts supported native Gemini fields. Native request validation still applies. Use the native Gemini endpoint if your client already speaks Gemini's API.

## Streams and retries

Once output has been forwarded, hey-proxy does not switch models mid-stream. A disconnected stream may need client recovery. Preserve the conversation history when retrying.

With fallback rules configured, Responses WebSocket upgrades return HTTP 426 so clients supporting HTTP fallback can use SSE. Native Gemini endpoints do not use cross-model fallback rules.

Without an eligible fallback chain, the proxy's `retry` settings control same-model recovery. To disable the elapsed recovery budget, set `retry.recovery_timeout_ms` to `0`; retries then use the configured attempt count. [Fallback chains](fallbacks.md) make one attempt per candidate and do not use that timed retry loop.
