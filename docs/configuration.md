# Configuration

The default file is `~/.hey-proxy/config.json`. Use `--config PATH` for another file. First run and `--init` create a missing minimal file with private permissions on Unix; neither overwrites an existing config.

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

## Applying edits

Routing, providers, credentials, and fallback rules reload for new requests. In-flight requests keep their original snapshot. Invalid changes keep the previous configuration active and print an error. Changing `listen` or persistent logging options requires a restart.

Persistent request metadata is enabled by default. To disable it:

```json
"logging": {"enabled": false}
```

The default database is `requests.sqlite3` beside the proxy config. Use `logging.database` to change its path. Prompts, payloads, credentials, and raw error messages are excluded from this history. Retention does not automatically delete old records.

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
```

`hey-proxy verify` checks the proxy and its connection. Add `--codex` only if you also want to verify an already-configured Codex installation. Verification does not modify Codex files.
