# hey-proxy

Use one local endpoint for OpenAI, Gemini, and your Claude subscription. hey-proxy forwards OpenAI requests, translates Responses API requests for Gemini, and lets you change models through simple overwrite rules. It runs as a Rust binary and includes a traffic and spend dashboard.

## Install and start

Install a current stable [Rust toolchain](https://rustup.rs/), then:

```sh
git clone https://github.com/kamilio/hey-proxy.git
cd hey-proxy
cargo install --path . --locked
hey-proxy --init
```

This creates `~/.hey-proxy/config.json` if it is missing:

```json
{
  "listen": "127.0.0.1:8080",
  "providers": {
    "openai": {
      "api_keys": {}
    }
  },
  "aliases": [],
  "fallbacks": {}
}
```

Normal startup also creates this file if needed. Existing proxy configs are preserved. The initial config has no credentials, model overwrites, or remote hosts. Configure a provider below, then start:

```sh
hey-proxy
```

Point your client's base URL at **`http://127.0.0.1:8080/v1`**. Open **`http://127.0.0.1:8080/`** (or **`/logs`**) for RPM and estimated spend. The API overview and client setup are at **`http://127.0.0.1:8080/apis`**.

The overview lists API routes, copyable client base URLs, and model names from your configured aliases, reasoning routes, and fallbacks. It respects API-specific overwrite rules and separates native Gemini model names from Responses model names. It never fetches an upstream model catalog. Click **Refresh config** after editing your config. Host mode uses the same access-key login as the dashboard; client relays show that model configuration belongs to their host.

**Installing or running hey-proxy never changes your Codex configuration.** Codex setup is a separate, optional command.

## Verify a request goes through hey-proxy

Send exactly `hello-hey-proxy` as your entire user message. hey-proxy replies locally with `hello-dude`, without calling a model or using tokens. Earlier conversation history and system instructions are allowed; only the final message is checked. Extra whitespace, longer text, multiple content blocks, attachments, and tool results do not trigger the probe.

The probe supports HTTP JSON and SSE on Responses, Chat Completions, Messages (including the custom adapters), and native Gemini generation endpoints. Host access-key authentication still applies. Responses include `x-hey-proxy-probe: true`. WebSocket requests and bodies over 64 MiB are forwarded normally. The synthetic Responses ID is not stored upstream; send conversation history rather than using it as `previous_response_id` in a later request.

```sh
curl http://127.0.0.1:8080/v1/responses \
  -H 'Content-Type: application/json' \
  -d '{"model":"probe","input":"hello-hey-proxy"}'
```

## Connect your Claude subscription

Add `"claude": {}` under `providers` in your proxy config, then run `hey-proxy claude-login` and start the proxy. hey-proxy owns the OAuth tokens and refreshes them automatically; Claude Code needs only these overrides:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:8080 \
ANTHROPIC_AUTH_TOKEN=hey-proxy \
claude
```

Open **http://127.0.0.1:8080/apis** for session/weekly subscription limits, reset times, usage freshness, and provider-reported extra spend. The native `/v1/messages` route preserves Claude requests and streaming responses. [Setup, credential ownership, and research sources](docs/claude-subscription.md).

## Check remaining subscription usage

```sh
hey-proxy usage                         # Claude/default: quota left and extra spend
hey-proxy usage --json                  # Versioned, machine-readable reading
hey-proxy usage --accounts              # Configured provider/account aliases
hey-proxy usage --provider claude --account default
```

The CLI queries the running proxy at the address in `--config`. Host mode reads its local proxy access key; client mode queries its connected host through the local relay. To query another proxy, use `--base-url https://your-proxy` and set `HEY_PROXY_TOKEN` to that proxy's access key. The `/v1` client base URL is also accepted. Subscription OAuth tokens stay on the proxy host.

Remaining percentages and reset times are account-wide, including usage outside this proxy. Extra spend shows the provider-reported monthly amount, cap, remaining budget, and amount above that cap. It is separate from the dashboard's estimated API-equivalent spend. Unknown values remain unknown; cached and stale readings retain their timestamps. CLI exit status is nonzero when the reading is stale, disabled, or unavailable; `--json` still prints that reading when the server returns one.

The Rust SDK is available as `hey_proxy::usage::Client`, with typed provider/account readings and `accounts()` / `usage(provider, account)` methods. [CLI, SDK, and API details](docs/claude-subscription.md#cli-and-rust-sdk). Only `claude/default` is currently supported; the versioned contract is ready for future Codex accounts.

## Connect OpenAI

Set your API key in the shell where you will run the proxy:

```sh
export OPENAI_API_KEY="your-api-key"
```

Add this provider to your proxy config:

```json
"providers": {
  "openai": {
    "api_keys": {
      "default": "sh://printf '%s' \"$OPENAI_API_KEY\""
    }
  }
}
```

The key is read from the process environment and cached in memory. If you run the proxy as a service, provide the environment variable to that service. You can also use a literal API key or a [1Password reference](#credentials).

Unprefixed model names go to OpenAI by default. Try a request:

```sh
curl http://127.0.0.1:8080/v1/responses \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4.1","input":"Say hello in one sentence."}'
```

The proxy supplies the upstream credential. A client that requires a local API key can use a placeholder in the default loopback-only mode.

## Set up model overwrites

Overwrites live in the `aliases` list. Each rule matches an incoming model name exactly:

```json
"aliases": [
  {"from": "coding", "to": "gpt-4.1"},
  {"from": "fast", "to": "gpt-4.1-mini"},
  {"from": "google", "to": "gemini/gemini-2.5-pro"}
]
```

Your client can now request `coding`, `fast`, or `google`. Configure Gemini before using the last rule. Other model names keep their normal routing. Alias rules apply once; a destination is not recursively resolved through another alias.

To select a destination based on the incoming reasoning effort:

```json
"aliases": [
  {
    "from": "reasoning",
    "to": "gpt-5",
    "reasoning_routes": {
      "low": {"to": "gpt-5-mini"}
    }
  },
  {"from": "careful", "to": "gpt-5", "reasoning": "high"}
]
```

`reasoning` uses `gpt-5-mini` for low-effort requests and `gpt-5` otherwise. `careful` always sends high reasoning effort. Use models that support the reasoning options you select.

Restrict a rule to an API shape with `"api_shape": "responses"`, `"chat_completions"`, or `"completions"`. Omit the field to keep applying it everywhere. Separate rules can share a `from` name when their shapes differ; overlapping rules are rejected.

```json
{"from": "coding", "to": "responses-model", "api_shape": "responses"}
```

For a chat-only client using a Responses model or Gemini, set its base URL to **`http://127.0.0.1:8080/v1/custom`**. The [custom Chat Completions adapter](docs/custom-chat-completions.md) supports streaming, tools, and OpenRouter-style reasoning replay.

Model routing and credential changes apply to new requests without restarting. Invalid edits leave the last valid config active. See the complete [overwrite example](examples/overwrites.config.json).

## Custom Messages / Claude Code

Use `http://127.0.0.1:8080/custom` as the Anthropic SDK or Claude Code base URL. Its `/v1/messages` call reaches the custom Messages shim. Gemini requests and replies translate directly to/from native Gemini, including SSE, tools, signed history, and token counting for compaction. [Setup and compatibility details](docs/custom-messages.md).

## Set up Gemini

Choose either a Gemini API key or Google Cloud Application Default Credentials (ADC). After setup, send Responses requests with a `gemini/` model prefix:

```sh
curl http://127.0.0.1:8080/v1/responses \
  -H 'Content-Type: application/json' \
  -d '{"model":"gemini/gemini-2.5-pro","input":"Write a Python function that sorts a list."}'
```

### Gemini API key

Create a key in [Google AI Studio](https://aistudio.google.com/apikey) and make it available to the proxy:

```sh
export GEMINI_API_KEY="your-api-key"
```

Add `gemini` beside `openai` in `providers`:

```json
"gemini": {
  "auth": "api_key",
  "api_key": "sh://printf '%s' \"$GEMINI_API_KEY\""
}
```

The default Gemini endpoint is Google's Generative Language API. See the complete [API-key example](examples/gemini-api-key.config.json).

### Google Cloud / Vertex AI with ADC

Use ADC to connect with your Google Cloud identity instead of managing an API key.

1. Install the [Google Cloud CLI](https://cloud.google.com/sdk/docs/install).
2. Choose a Google Cloud project with billing and Vertex AI access. Your identity needs permission to use Vertex AI, such as the Vertex AI User role.
3. Enable the API and create local ADC credentials:

   ```sh
   gcloud services enable aiplatform.googleapis.com --project YOUR_PROJECT_ID
   gcloud auth application-default login
   gcloud auth application-default set-quota-project YOUR_PROJECT_ID
   ```

4. Add this provider, replacing `YOUR_PROJECT_ID`:

   ```json
   "gemini": {
     "auth": "adc",
     "upstream_url": "https://aiplatform.googleapis.com/v1/projects/YOUR_PROJECT_ID/locations/global/publishers/google"
   }
   ```

5. Check that the proxy can obtain credentials:

   ```sh
   hey-proxy check-credentials
   ```

ADC uses the Google Rust SDK to discover, cache, and refresh credentials. No `api_key` field or per-request `gcloud` command is needed. In a cloud environment, ADC can use the attached service identity. See [Google's ADC setup guide](https://cloud.google.com/docs/authentication/provide-credentials-adc) and the complete [Vertex AI example](examples/gemini-adc.config.json).

### Use Gemini's native API

You can also send Gemini requests directly through the proxy:

```sh
curl http://127.0.0.1:8080/v1beta/models/gemini-2.5-pro:generateContent \
  -H 'Content-Type: application/json' \
  -d '{"contents":[{"role":"user","parts":[{"text":"Say hello."}]}]}'
```

The proxy uses your configured Gemini endpoint and credentials. Native requests retain the Gemini wire format; `/v1/responses` requests use the Responses conversion.

## Configure Codex — optional

After the proxy is running, explicitly connect Codex:

```sh
hey-proxy configure-codex --base-url http://127.0.0.1:8080/v1
codex
```

This command updates Codex's user configuration to select hey-proxy and use automatic approval review (`approval_policy = "on-request"`, `approvals_reviewer = "auto_review"`). It applies these settings to the selected profile too, so new sessions use automatic review. It backs up changed existing files and preserves sandbox permissions and unrelated settings. Use `--model coding` to select an overwrite, or `--codex-home /path/to/codex-home` to choose which installation to configure.

To create a separate Gemini profile while keeping Codex's default provider:

```sh
hey-proxy configure-gemini \
  --base-url http://127.0.0.1:8080/v1 \
  --model gemini/gemini-2.5-pro
codex --profile gemini
```

This writes `gemini.config.toml` in Codex's configuration directory. It uses Responses over HTTP, disables request compression, and enables client retries. Re-running it updates that profile. See the [Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).

Normal startup, `--init`, credential checks, and remote rollout do not create or modify Codex files.

## Configure Gemini CLI — optional

Install [Gemini CLI](https://github.com/google-gemini/gemini-cli), then connect it to your configured Gemini provider:

```sh
npm install -g @google/gemini-cli
hey-proxy configure-gemini-cli
gemini
```

Like `configure-pi`, setup reads the proxy's listening address and routing. It selects the first Gemini destination from your aliases (including reasoning routes), then checks `model_registry` and fallbacks if needed. It uses the native model name and API, with credentials supplied by the proxy. To choose a specific model, or when your provider has no model routes:

```sh
hey-proxy configure-gemini-cli --model gemini-2.5-pro
```

The command updates `~/.gemini/settings.json` and `.env`, selects API-key authentication, and sets the proxy URL without a `/v1` suffix. Host mode uses the local proxy access key with bearer authentication; standalone and client modes use a placeholder. A client relay requires `--model`, since model routing belongs to its host. Use `--config` for another proxy config and `--gemini-home` for another Gemini configuration directory. `GEMINI_CLI_HOME` is also respected as Gemini CLI's user-home override (setup writes its `.gemini` subdirectory).

Existing hooks, MCP servers, trust settings, and unrelated environment values are preserved. Stale model/auth environment overrides are removed. Changed files receive private backups, and repeating setup with identical settings does not rewrite them. Restart Gemini after setup. Gemini CLI loads `.gemini/.env` in trusted workspaces; for headless use in a workspace you trust, run `gemini --skip-trust -p "Your prompt"`. Existing shell variables and project `.env` files may override the generated configuration.

This command configures Gemini CLI only. The existing `configure-gemini` command above continues to configure the Gemini **Codex profile**. Normal proxy startup and rollout do not modify Gemini CLI files.

## Configure Pi — optional

After the proxy is running, connect [Pi](https://pi.dev):

```sh
hey-proxy configure-pi
pi
```

Everything comes from your proxy config, so there is nothing to type. The command writes a `hey-proxy` provider to `~/.pi/agent/models.json` using the address the proxy listens on and the Responses API, and generates its model list from your overwrites: every overwrite name plus the model each one routes to. With the [overwrite example](examples/overwrites.config.json) above, Pi offers `coding`, `fast` and `reasoning` alongside `gpt-4.1`, `gpt-4.1-mini`, `gpt-5` and `gpt-5-mini`. An overwrite with `reasoning` or `reasoning_routes` is marked as a thinking model, so Pi sends the effort level your routes select on. It then points `defaultProvider` and `defaultModel` in `~/.pi/agent/settings.json` at the proxy and your first overwrite. Switch models inside Pi with Ctrl+P, and save a different startup model with `/model` and Ctrl+S.

Put model limits and compaction budgets in the proxy config's [`model_registry`](docs/configuration.md#model-budget-registry). This is the authoritative source for `configure-pi`: define each backend once and every alias inherits it. For example:

```json
"model_registry": {
  "defaults": {
    "context_window": 128000,
    "max_tokens": 16384,
    "keep_recent_tokens": 20000,
    "reserve_tokens": 16384
  },
  "models": {
    "gemini/gemini-early-exp": {
      "context_window": 1048576,
      "max_tokens": 65536,
      "keep_recent_tokens": 100000,
      "reserve_tokens": 200000
    }
  }
}
```

After editing the registry or routes, run `hey-proxy configure-pi` again. It regenerates context/output limits in Pi's model entries and per-model compaction budgets in its settings, replacing stale values and removing competing limit overrides. Other providers, global settings, and unrelated metadata (reasoning, costs, compatibility, theme, packages) are preserved. Changed files are backed up privately; unchanged files are not rewritten.

An alias that can reach several models through reasoning routes or fallbacks uses the smallest of each budget across those destinations. Unknown backends inherit `defaults`; those conservative defaults are compatibility settings, not verified upstream maxima. The Gemini entry above uses limits validated directly against `gemini-early-exp` on 2026-09-28; they are not applied to unrelated models.

Without a registry, setup preserves existing model limits and safe compaction settings, repairing only unsafe inherited budgets against the configured context. Unsafe explicit budgets fail before either Pi file is written. This compatibility behavior supports older configurations; use the registry to maintain values in one place.

These settings require Pi's per-model compaction support (verified with Pi 0.87.1). Pi also merges project settings from `.pi/settings.json`, which can override generated user settings. Restart Pi after setup to reload model limits and compaction; `/model` alone reloads only the model catalog.

The offline regression check uses an installed Pi without making model requests: after `cargo build`, run `node tests/pi_compaction.mjs /path/to/pi-coding-agent`. An optional second argument replays a saved session JSONL read-only; summaries are simulated to verify the restored output budget.

Normal startup, `--init`, credential checks, and remote rollout do not create or modify Pi files.

## Fall back when a model fails

Add alternatives in the order you want them tried:

```json
"fallbacks": {
  "gpt-4.1": ["gpt-4.1-mini"]
}
```

Fallback lookup uses the model **after overwrites**, including reasoning routes. A `coding → gpt-4.1` overwrite uses this rule. A route directly to `gpt-4.1-mini` does not inherit it.

The proxy waits for the current model to succeed or fail. It does not switch because a response is slow. Transient failures before output can advance to the next model; once text, reasoning, or a tool call starts, the current stream remains intact. Authentication, invalid-request, billing, and policy errors are returned directly. See [fallback behavior](docs/fallbacks.md) and a complete [config example](examples/fallback.config.json).

## Credentials

Both providers accept these credential sources:

| Value | Behavior |
| --- | --- |
| A literal key | Used as supplied |
| `sh://command` | Runs a trusted shell command and uses its output |
| `op://vault/item/field` | Reads the field with the 1Password CLI |

For example:

```json
"api_keys": {
  "default": "op://MyVault/OpenAI/credential"
}
```

Install and authenticate the [1Password CLI](https://developer.1password.com/docs/cli/). For unattended use, configure a service account with access only to the needed items. Successful command results are cached in memory; concurrent requests share the same refresh. Commands have a 30-second execution limit. This credential limit is separate from model response waiting.

`hey-proxy check-credentials` checks access without printing secret values. Multiple OpenAI projects and credential selection per overwrite are covered in [configuration](docs/configuration.md).

## Dashboard and remote use

The default page at `/` (also `/logs`) shows RPM and estimated USD spend for today, this week, and all time. Days use your browser's local midnight; weeks start Monday. Client relays show their host's totals without double counting. API setup and subscription limits are at `/apis`.

The dashboard reads small, cached aggregates rather than request histories. A background writer maintains totals, and forwarding never waits for dashboard queries or disk writes. Accounting snapshots are the default; set `logging.detailed: true` to retain full diagnostic timelines. Historical query and export APIs remain available.

Spend uses reported tokens and published model rates, including Claude cache writes and reads. **Claude subscription usage is shown as its API-equivalent value, not extra subscription charges.** Unrecognized models and missing usage stay unpriced, and logging gaps are visible. Totals survive restarts; disabling persistence makes spend unavailable. Estimates exclude subscription fees, discounts, server-side tool charges and unreported retry usage. No prompts, response bodies or credentials are retained in request history.

For SSH deployment, add hosts to `ssh_hosts` and run `hey-proxy rollout`. This installs the proxy service and syncs its proxy configuration. **Codex setup remains a separate command on each machine.** See [remote setup](docs/configuration.md#remote-setup) for host/client modes and credential requirements.

## Troubleshooting

- **No OpenAI credential configured:** add a `default` entry under `providers.openai.api_keys`, or configure Gemini and request a `gemini/` model.
- **Credential command timed out:** check the `sh://` command or 1Password login. For Vertex AI, prefer native `auth: "adc"`.
- **A config edit is ignored:** check the terminal for a validation error. The previous valid config keeps serving requests.
- **Address already in use:** stop the other listener or run `hey-proxy --listen 127.0.0.1:9090`.
- **Gemini rejects a request:** check [API compatibility](docs/compatibility.md), your model's capabilities, and Google project access.

Run `hey-proxy --help` for commands. Use `--config /path/to/config.json` to keep multiple proxy configurations separate.
