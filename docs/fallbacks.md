# Model fallbacks

Configure only the alternatives you need:

```json
"fallbacks": {
  "gpt-4.1": ["gpt-4.1-mini"]
}
```

The lookup key is the model after alias and reasoning-route rewrites. A request overwritten from `coding` to `gpt-4.1` uses this rule. A low-effort route directly to `gpt-4.1-mini` does not inherit it. Each fallback target also uses existing alias and credential rules; any subsequent fallback lookup uses its rewritten name.

Each candidate is attempted once. The proxy waits for an attempt to succeed or fail; there is no per-model response deadline or overall fallback timer. Credential acquisition and connection establishment retain their own limits. If all candidates fail, the last error and its `Retry-After` header are returned.

Transient HTTP failures, explicit transient stream errors before output, and pre-output transport failures can advance to another model. Authentication, billing, invalid requests, policy refusals, and local conversion errors remain terminal. Output, reasoning, tool calls, and unknown stream events commit the current model. The proxy does not splice two models' answers together.

Requests using server-side conversation state, background execution, or hosted tools bypass replay. Locally executed function and custom tools can be used, but their history and signatures must remain intact. Cross-provider replay can fail if the target cannot accept the original signed history.

Rules support ordered chains. For `a → [b, c]` and `b → [d]`, the order is `a, b, d, c`. Repeated model/project pairs are attempted once, cycles are rejected or bounded after alias resolution, and at most 16 candidates are attempted. New requests use edited rules; an in-flight chain keeps its original config.

The dashboard records the requested and served model. Responses with an eligible chain include `x-hey-proxy-fallback-count` and `x-hey-proxy-requested-model` headers.

The failure policy draws on the ordered routing and replay protections used by [LiteLLM](https://github.com/BerriAI/litellm), [Portkey](https://github.com/Portkey-AI/gateway), and [Bifrost](https://github.com/maximhq/bifrost). It runs within the Rust proxy without an additional gateway process.
