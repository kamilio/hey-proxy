# hey-proxy

Use one local endpoint for OpenAI and Gemini. hey-proxy forwards OpenAI requests, translates Responses API requests for Gemini, and lets you change models through simple overwrite rules. It runs as a Rust binary and includes a local request dashboard.

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

Point your client's base URL at **`http://127.0.0.1:8080/v1`**. Open **`http://127.0.0.1:8080/logs`** for the dashboard.

**Installing or running hey-proxy never changes your Codex configuration.** Codex setup is a separate, optional command.

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

Model routing and credential changes apply to new requests without restarting. Invalid edits leave the last valid config active. See the complete [overwrite example](examples/overwrites.config.json).

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

This command updates Codex's user configuration to select hey-proxy. It backs up changed existing files and preserves unrelated settings. Use `--model coding` to select an overwrite, or `--codex-home /path/to/codex-home` to choose which installation to configure.

To create a separate Gemini profile while keeping Codex's default provider:

```sh
hey-proxy configure-gemini \
  --base-url http://127.0.0.1:8080/v1 \
  --model gemini/gemini-2.5-pro
codex --profile gemini
```

This writes `gemini.config.toml` in Codex's configuration directory. It uses Responses over HTTP, disables request compression, and enables client retries. Re-running it updates that profile. See the [Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).

Normal startup, `--init`, credential checks, and remote rollout do not create or modify Codex files.

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

The dashboard at `/logs` shows requested and served models, errors, token usage, timing, and estimated spend. Local history persists across restarts. Prompts, response bodies, and credentials are not stored in dashboard history. Prices are estimates based on recognized public model names; unknown models may remain unpriced.

For SSH deployment, add hosts to `ssh_hosts` and run `hey-proxy rollout`. This installs the proxy service and syncs its proxy configuration. **Codex setup remains a separate command on each machine.** See [remote setup](docs/configuration.md#remote-setup) for host/client modes and credential requirements.

## Troubleshooting

- **No OpenAI credential configured:** add a `default` entry under `providers.openai.api_keys`, or configure Gemini and request a `gemini/` model.
- **Credential command timed out:** check the `sh://` command or 1Password login. For Vertex AI, prefer native `auth: "adc"`.
- **A config edit is ignored:** check the terminal for a validation error. The previous valid config keeps serving requests.
- **Address already in use:** stop the other listener or run `hey-proxy --listen 127.0.0.1:9090`.
- **Gemini rejects a request:** check [API compatibility](docs/compatibility.md), your model's capabilities, and Google project access.

Run `hey-proxy --help` for commands. Use `--config /path/to/config.json` to keep multiple proxy configurations separate.
