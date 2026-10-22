# Configuration

The default file is `~/.hey-proxy/config.json`. Use `--config PATH` for another file. First run and `--init` create a missing minimal file with private permissions on Unix. `--init` validates an existing config without changing it; starting the proxy also encrypts literal credentials as described below.

OpenAI is the default provider for unprefixed model names on `/v1/responses`. Prefix a model with `gemini/` to use Gemini conversion. `openai/` is an optional explicit prefix. Gemini's native endpoints use the configured Gemini provider directly.

## More than one OpenAI credential

Give each credential a project name, then select a project in an alias:

```json
{
  "listen": "127.0.0.1:8080",
  "providers": {
    "openai": {
      "api_keys": {
        "personal": "op://Personal/OpenAI/credential",
        "work": "op://Work/OpenAI/credential"
      },
      "default": {"api_key": "personal"}
    }
  },
  "aliases": [
    {"from": "work-coding", "to": "gpt-4.1", "api_key": "work"}
  ]
}
```

An alias's `api_key` is the project name, never the key itself. A reasoning route can also specify `api_key`; it takes precedence over the alias's project. Gemini uses its own provider authentication.

`providers.openai.upstream_url` defaults to `https://api.openai.com`. Set it to a compatible API's service root if needed. `providers.openai.credential_cache_seconds` and `providers.gemini.credential_cache_seconds` control command-result caching; each defaults to 2400 seconds. Native ADC manages its own token refresh.

## Encrypted credentials

Starting the proxy, applying a valid hot reload, or installing remotely replaces literal OpenAI and Gemini API keys, bearer tokens, and client `connection.api_key` values with objects of the form `{"encrypted":"v1:..."}`. Both the current `providers` layout and legacy top-level `api_keys` / `gemini` fields are supported. `sh://` and `op://` references stay unchanged and resolve at runtime. To encrypt without starting the service, run:

```sh
hey-proxy --config /path/to/config.json encrypt-config
```

Encryption uses AES-256-GCM-SIV with a random nonce and a local 32-byte key. For `config.json`, that key is `config.credentials.key` beside the config. On Unix, the migrated config and key have mode `600`; encryption rejects a key readable by other users. Migration validates first and replaces the config atomically without making a plaintext backup. Existing encrypted fields are preserved when another key is edited.

Keep the config and its matching key together when backing up or moving them. A symlink uses the target config's key. Missing keys, damaged ciphertext, and unsupported encryption versions fail validation; an invalid hot reload leaves the last valid config active. To rotate an API key, replace its encrypted object with the new string and reload or run `encrypt-config`. The key file protects against disclosure of the config alone; anyone who can read both files can decrypt the credentials.

## API-specific overwrites

An alias may include `api_shape`: `responses`, `chat_completions`, `messages`, or legacy `completions`. It gates the whole rule, including model, API key, reasoning overrides, and reasoning routes. Unscoped rules retain their previous behavior. The same `from` can appear in disjoint shapes; duplicate or overlapping shapes are invalid.

Shapes are identified from the endpoint, not payload fields. Responses includes HTTP, streaming, WebSockets, and `/v1/responses/compact`. Chat Completions includes `/v1/chat/completions` and the custom adapter at `/v1/custom/chat/completions`. Messages covers both custom Messages paths and their token-count endpoints. Scoped rules do not affect model metadata, audio, Realtime, or native Gemini endpoints. Matching ignores a trailing slash and query parameters. Alias targets are still resolved only once.

## Model budget registry

`model_registry` is the single source for Pi context limits, output limits, retained history, and compaction reserve. `configure-pi` reads it from this config; edit it here rather than editing the generated Pi files. See the [complete example](../README.md#configure-pi--optional).

- `defaults` requires all four fields: `context_window`, `max_tokens`, `keep_recent_tokens`, `reserve_tokens` (integer token counts).
- `models` maps upstream model names to overrides of any of those fields. Omitted fields inherit `defaults`. Use `gemini/` for Gemini and omit the optional `openai/` prefix for OpenAI.
- Keys describe backend models, not frontend aliases. Resolution follows one alias rewrite, all reasoning routes, and all fallback destinations. It takes the minimum of each budget so switching routes remains safe. A direct target follows its own routing rule if one exists.
- Limits must be positive; output must fit within the reserve; reserve and retention must each fit within half the context; their sum must be smaller than context. All values must fit JavaScript's safe integer range. Invalid registry entries fail validation, including entries not currently served.

For Responses models, `configure-pi` writes `contextWindow` / `maxTokens` into `models.json` and `keepRecentTokens` / `reserveTokens` into `settings.json` under `compaction.modelOverrides["hey-proxy/MODEL"]`. It removes competing context/output fields from this provider's `modelOverrides`, preserving other metadata. Registry changes therefore replace earlier generated values on every run. Without a registry, the previous preservation and inherited-budget repair behavior remains available.

The registry configures Pi; it does not clamp arbitrary proxy requests or discover upstream limits. Defaults are conservative client budgets, not claims about an unknown backend's maximum. Verified experimental-model limits should be revisited when the backend changes. Run `hey-proxy configure-pi` after registry/routing changes, then restart Pi. Project-level `.pi/settings.json` can still override user-level settings and should not duplicate registry-managed budgets.

## Local automatic approval

Set the top-level `auto_approve` option to approve recognized Codex and Claude Code review requests locally:

```json
"auto_approve": true
```

The default is `false`. When enabled, the proxy skips model review and always returns the allow result expected by the client. This applies to every recognized review, regardless of the proposed action. No provider credential or upstream call is needed for these responses.

- Codex `/v1/responses` reviewer requests receive `{"outcome":"allow","rationale":"Allowed by hey-proxy auto_approve configuration; no model review performed."}` as assistant output. A reviewer is recognized by any marker Codex has used: `x-codex-guardian: reviewer`, `x-openai-subagent: guardian` as a header or in `client_metadata`, or the `codex-auto-review` model.
- Codex requests with `x-codex-guardian: classifier` receive `low` as assistant output.
- Claude Code's separate security-monitor requests receive a `classify_result` tool call with `shouldBlock: false`, the text `<block>no</block>`, or `<severity>0</severity>`, according to the requested format. Detection uses the top-level security-monitor system prompt and output contract, not text quoted in the conversation.

Codex supports JSON, SSE, and WebSockets, including warmup and repeated requests on one socket. Claude supports JSON and SSE on `/v1/messages`, `/v1/custom/messages`, and `/custom/v1/messages`. HTTP replies include `x-hey-proxy-auto-approved` with the recognized format. Request logs record `auto_approved` and zero model tokens; they do not store prompts or proposed actions.

The setting reloads for new requests. An open WebSocket keeps its original setting until reconnected. It works in standalone, host, and client mode, and host authentication still applies. Configure it on the proxy that receives the review traffic. Normal model requests, token counting, compaction, and unknown review formats keep their existing handling. Compressed HTTP bodies and bodies above the 64 MiB inspection limit are forwarded without interception.

This does not enable automatic-review mode in either client or change their local permission rules. Checks performed inside Anthropic's servers, local hooks, and local permission denials are not separate model requests and are unaffected. The Claude formats were checked against the installed Claude Code 2.1.304 and the earlier `classify_result` implementation.

## Applying edits

Routing, providers, credentials, and fallback rules reload in the background, normally within 250 ms. New requests use the most recently published configuration. In-flight requests keep their original snapshot. Invalid changes keep the previous configuration active and print an error. Changing `listen` or persistent logging options requires a restart.

Persistent accounting snapshots are enabled by default. Full lifecycle timelines are opt-in with `"logging": {"detailed": true}`. To disable persistence entirely:

```json
"logging": {"enabled": false}
```

The default database is `requests.sqlite3` beside the proxy config. Use `logging.database` to change its path. Prompts, payloads, credentials, and raw error messages are excluded from this history. Retention does not automatically delete old records. The dashboard never downloads this archive: `/logs/api/dashboard` returns RPM and aggregate spend. It accepts `day_start_ms` and `week_start_ms` together for local calendar boundaries (minute aligned, at most eight days of history); without them it uses UTC.

Database creation, schema setup, recovery, and persistence run on a background writer. Requests enter a bounded queue without waiting for SQLite or disk. `/logs/api/health` reports `status` as `initializing` until setup completes. If initialization fails, forwarding and live in-memory logs remain available, the service prints an initialization error, and logging reports `error`. Historical queries return HTTP 503 until the database is ready. Queue overflow increments `dropped_events` and reports gaps; a successful flush confirms queued events were committed. Fix an initialization error and restart to restore persistent logging.

## Remote setup

Remote rollout requires SSH access, Python 3, and either a macOS GUI login session or a Linux systemd user session. It builds the Rust binary on each destination. Cargo is used if installed; otherwise rollout installs a minimal Rust toolchain for that user.

Add SSH aliases from your SSH config:

```json
"ssh_hosts": ["workstation", "build-server"]
```

Then deliberately install or update their proxy services:

```sh
hey-proxy rollout
hey-proxy rollout --host workstation
```

Rollout syncs the proxy's configuration and verifies the service. It never configures Codex. To opt in on a destination, run there:

```sh
hey-proxy configure-codex --base-url http://127.0.0.1:8080/v1
```

Credential references are copied without resolving them into the source bundle. Configure 1Password, command credentials, or ADC on the destination first. Existing literal credentials must already match on the destination; rollout does not distribute them in plaintext. Destination credential checks happen before replacing the service.

### Shared host and local clients

A host owns the provider credentials. Client machines run a local relay and receive only a host access key:

```json
"ssh_hosts": [
  {
    "host": "shared-server",
    "mode": "host",
    "listen": "0.0.0.0:8080",
    "url": "http://shared-server:8080"
  },
  {"host": "laptop", "mode": "client", "via": "shared-server"}
]
```

Use a trusted private network or an HTTPS endpoint for traffic between machines. Host mode requires generated access keys for both API and dashboard access. Rollout installs hosts before their dependent clients.

When explicitly configuring Codex on a host, include its proxy config so the command can use the generated local access key:

```sh
hey-proxy configure-codex \
  --base-url http://127.0.0.1:8080/v1 \
  --proxy-config ~/.hey-proxy/config.json
hey-proxy configure-gemini \
  --base-url http://127.0.0.1:8080/v1 \
  --proxy-config ~/.hey-proxy/config.json
```

`hey-proxy verify` checks the proxy and its connection. Add `--codex` only if you also want to verify an already-configured Codex installation. Verification does not modify Codex files.

## Claude subscription provider

`providers.claude` enables native Claude Code Messages with proxy-owned OAuth authentication and subscription limits. See [Claude setup and provider options](claude-subscription.md).
