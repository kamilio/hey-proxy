use crate::config::{ClientConnection, Config, Mode, SshHost};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::Stdio,
};
use tokio::process::Command;
use toml_edit::{DocumentMut, Item, Table, value};

fn table<'a>(doc: &'a mut Item, key: &str) -> Result<&'a mut Item> {
    if doc.get(key).is_none() {
        doc[key] = Item::Table(Table::new());
    }
    if !doc[key].is_table() {
        bail!("Codex {key} must be a TOML table");
    }
    Ok(&mut doc[key])
}
#[cfg(test)]
fn edit_codex(content: &str, base_url: &str, model: Option<&str>) -> Result<String> {
    edit_codex_token(content, base_url, model, None)
}
fn edit_codex_token(
    content: &str,
    base_url: &str,
    model: Option<&str>,
    token: Option<&str>,
) -> Result<String> {
    let mut doc = content
        .parse::<DocumentMut>()
        .context("Invalid Codex config TOML; left unchanged")?;
    doc["model_provider"] = value("hey-proxy");
    doc["approval_policy"] = value("on-request");
    doc["approvals_reviewer"] = value("auto_review");
    if let Some(model) = model {
        doc["model"] = value(model);
    }
    let providers = table(doc.as_item_mut(), "model_providers")?;
    // Recovery settings must survive rerunning configure-codex/rollout.
    let timeout = providers
        .get("hey-proxy")
        .and_then(|p| p.get("stream_idle_timeout_ms"))
        .cloned();
    // Replace only our own provider to remove stale env-key/auth overrides.
    providers["hey-proxy"] = Item::Table(Table::new());
    let provider = &mut providers["hey-proxy"];
    provider["name"] = value("hey-proxy");
    provider["base_url"] = value(base_url);
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(false);
    provider["stream_idle_timeout_ms"] = timeout.unwrap_or_else(|| value(300_000));
    if let Some(token) = token {
        provider["experimental_bearer_token"] = value(token);
    }
    // The proxy routes on uncompressed JSON model envelopes.
    table(doc.as_item_mut(), "features")?["enable_request_compression"] = value(false);
    if let Some(profile) = doc.get("profile").and_then(Item::as_str).map(str::to_owned)
        && let Some(profiles) = doc.get_mut("profiles")
        && let Some(active) = profiles.get_mut(&profile)
    {
        if !active.is_table() {
            bail!("Active Codex profile must be a table");
        }
        active["model_provider"] = value("hey-proxy");
        active["approval_policy"] = value("on-request");
        active["approvals_reviewer"] = value("auto_review");
        table(active, "features")?["enable_request_compression"] = value(false);
        if let Some(model) = model {
            active["model"] = value(model);
        }
    }
    Ok(doc.to_string())
}
pub(crate) fn write_private(path: &Path, content: &[u8], backup: bool) -> Result<()> {
    let parent = path.parent().context("File has no parent")?;
    fs::create_dir_all(parent)?;
    if path.exists() && fs::read(path)? == content {
        return Ok(());
    }
    if backup && path.exists() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let backup_path = path.with_file_name(format!(
            "{}.backup-{stamp}",
            path.file_name().unwrap().to_string_lossy()
        ));
        let mut copy = tempfile::NamedTempFile::new_in(parent)?;
        std::io::Write::write_all(&mut copy, &fs::read(path)?)?;
        copy.persist(&backup_path)?;
        println!("Backup: {}", backup_path.display());
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut file, content)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}
#[cfg(test)]
pub fn configure_gemini(base_url: &str, model: &str, home: Option<&Path>) -> Result<()> {
    configure_gemini_authenticated(base_url, model, home, None)
}
pub fn configure_gemini_authenticated(
    base_url: &str,
    model: &str,
    home: Option<&Path>,
    token: Option<&str>,
) -> Result<()> {
    let url = reqwest::Url::parse(base_url)?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"))
        || url.path() != "/v1"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("Gemini profile must use a loopback proxy URL ending in /v1");
    }
    let bare = model
        .strip_prefix("gemini/")
        .context("Gemini profile model must start with gemini/")?;
    if bare.is_empty()
        || !bare
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        bail!("Invalid Gemini profile model");
    }
    let home = home
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("CODEX_HOME").map(Into::into))
        .unwrap_or(
            std::path::PathBuf::from(std::env::var_os("HOME").context("HOME unset")?)
                .join(".codex"),
        );
    let path = home.join("gemini.config.toml");
    let original = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut doc = original
        .parse::<DocumentMut>()
        .context("Invalid Gemini profile; left unchanged")?;
    doc["model"] = value(model);
    doc["model_provider"] = value("hey_proxy_gemini");
    if doc.get("model_reasoning_effort").is_none() {
        doc["model_reasoning_effort"] = value("high");
    }
    doc["model_reasoning_summary"] = value("auto");
    doc["web_search"] = value("disabled");
    let features = table(doc.as_item_mut(), "features")?;
    features["enable_request_compression"] = value(false);
    features["multi_agent"] = value(false);
    let providers = table(doc.as_item_mut(), "model_providers")?;
    let provider = table(providers, "hey_proxy_gemini")?;
    for key in [
        "env_key",
        "experimental_bearer_token",
        "http_headers",
        "env_http_headers",
    ] {
        provider.as_table_mut().unwrap().remove(key);
    }
    provider["name"] = value("Gemini via hey-proxy");
    provider["base_url"] = value(base_url);
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(false);
    provider["supports_websockets"] = value(false);
    if let Some(token) = token {
        provider["experimental_bearer_token"] = value(token);
    }
    // Older generated profiles disabled Codex recovery entirely. Upgrade those
    // defaults, while keeping any positive user-selected retry counts.
    for (key, default) in [("request_max_retries", 4), ("stream_max_retries", 5)] {
        if provider.get(key).and_then(Item::as_integer).unwrap_or(0) == 0 {
            provider[key] = value(default);
        }
    }
    if provider.get("stream_idle_timeout_ms").is_none() {
        provider["stream_idle_timeout_ms"] = value(900_000);
    }
    write_private(&path, doc.to_string().as_bytes(), false)?;
    println!("Gemini profile ready: {}", path.display());
    Ok(())
}
/// The provider id Pi uses for this proxy. Only this entry is ever rewritten.
const PI_PROVIDER: &str = "hey-proxy";
use crate::model_registry::DEFAULT_BUDGET;

/// Every model this proxy serves by name: each overwrite plus the models it
/// routes to. A reasoning overwrite is marked so Pi sends an effort level, which
/// is what `reasoning_routes` selects on.
fn pi_models(config: &Config) -> BTreeMap<String, bool> {
    let mut models = BTreeMap::new();
    for alias in &config.aliases {
        let reasoning = alias.reasoning.is_some() || !alias.reasoning_routes.is_empty();
        let entry = models.entry(alias.from.clone()).or_insert(false);
        *entry = *entry || reasoning;
        for destination in alias
            .to
            .iter()
            .chain(alias.reasoning_routes.values().map(|route| &route.to))
        {
            models.entry(destination.clone()).or_insert(false);
        }
    }
    models
}
/// Names usable by Pi's Responses transport, including direct destinations.
fn pi_response_models(config: &Config) -> BTreeSet<String> {
    let mut models = BTreeSet::new();
    for alias in &config.aliases {
        if !alias.matches_shape("/v1/responses") {
            continue;
        }
        models.insert(alias.from.clone());
        for destination in alias
            .to
            .iter()
            .chain(alias.reasoning_routes.values().map(|r| &r.to))
        {
            models.insert(destination.clone());
        }
    }
    models
}

fn pi_object<'a>(
    parent: &'a mut Value,
    key: &str,
    location: &str,
) -> Result<&'a mut serde_json::Map<String, Value>> {
    let parent = parent
        .as_object_mut()
        .with_context(|| format!("Pi {location} parent must be an object; left unchanged"))?;
    parent
        .entry(key)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .with_context(|| format!("Pi {location} must be an object; left unchanged"))
}

fn pi_tokens(value: Option<&Value>, default: u64, location: &str) -> Result<u64> {
    match value {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|n| *n <= 9_007_199_254_740_991)
            .with_context(|| {
                format!("Pi {location} must be a non-negative safe integer; left unchanged")
            }),
    }
}

fn pi_registry(
    settings: &mut Value,
    catalog: &mut Value,
    config: &Config,
    registry: &crate::model_registry::ModelRegistry,
) -> Result<()> {
    let provider = &mut catalog["providers"][PI_PROVIDER];
    if let Some(overrides) = provider.get("modelOverrides")
        && !overrides.is_object()
    {
        bail!("Pi models.json hey-proxy modelOverrides must be an object; left unchanged");
    }
    pi_object(settings, "compaction", "settings.json compaction")?;
    let compaction = &mut settings["compaction"];
    pi_object(
        compaction,
        "modelOverrides",
        "settings.json compaction.modelOverrides",
    )?;
    for id in pi_response_models(config) {
        let budget = registry.resolve(config, &id);
        // Model definitions are the generated copy. Remove competing limit
        // overrides, but keep costs, compatibility and other user metadata.
        if let Some(overrides) = provider["modelOverrides"].as_object_mut()
            && let Some(entry) = overrides.get_mut(&id)
        {
            let entry = entry
                .as_object_mut()
                .context("Pi model override must be an object; left unchanged")?;
            entry.remove("contextWindow");
            entry.remove("maxTokens");
            if entry.is_empty() {
                overrides.remove(&id);
            }
        }
        let model = provider["models"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|model| model["id"] == id)
            .unwrap();
        model["contextWindow"] = json!(budget.context_window);
        model["maxTokens"] = json!(budget.max_tokens);
        let key = format!("{PI_PROVIDER}/{id}");
        let entry = pi_object(
            &mut compaction["modelOverrides"],
            &key,
            "Pi compaction model override",
        )?;
        entry.insert("keepRecentTokens".into(), json!(budget.keep_recent_tokens));
        entry.insert("reserveTokens".into(), json!(budget.reserve_tokens));
    }
    Ok(())
}

fn pi_compaction(settings: &mut Value, config: &Config, catalog: &Value) -> Result<()> {
    for id in pi_response_models(config) {
        pi_object(settings, "compaction", "settings.json compaction")?;
        let compaction = &mut settings["compaction"];
        let keep = pi_tokens(
            compaction.get("keepRecentTokens"),
            DEFAULT_BUDGET.keep_recent_tokens,
            "keepRecentTokens",
        )?;
        let reserve = pi_tokens(
            compaction.get("reserveTokens"),
            DEFAULT_BUDGET.reserve_tokens,
            "reserveTokens",
        )?;
        let provider = &catalog["providers"][PI_PROVIDER];
        let model = provider["models"]
            .as_array()
            .and_then(|models| models.iter().find(|m| m["id"] == id));
        let context = pi_tokens(
            provider["modelOverrides"][&id]
                .get("contextWindow")
                .or_else(|| model.and_then(|m| m.get("contextWindow"))),
            DEFAULT_BUDGET.context_window,
            "contextWindow",
        )?;
        if context < 4 {
            bail!("Pi model {id} contextWindow is too small for compaction; left unchanged");
        }
        pi_object(
            compaction,
            "modelOverrides",
            "settings.json compaction.modelOverrides",
        )?;
        let key = format!("{PI_PROVIDER}/{id}");
        // Pi uses reported tokens to trigger compaction, but a separate text
        // estimate to choose its cut point. Keep headroom for that discrepancy.
        let unsafe_budget = |keep: u64, reserve: u64| {
            keep > context / 2 || reserve > context / 2 || keep.saturating_add(reserve) >= context
        };
        let existing = compaction["modelOverrides"].get(&key);
        if existing.is_some_and(|entry| !entry.is_object()) {
            bail!("Pi compaction.modelOverrides[{key}] must be an object; left unchanged");
        }
        let explicit_keep = existing.and_then(|entry| entry.get("keepRecentTokens"));
        let explicit_reserve = existing.and_then(|entry| entry.get("reserveTokens"));
        let effective_keep = pi_tokens(explicit_keep, keep, "keepRecentTokens")?;
        let effective_reserve = pi_tokens(explicit_reserve, reserve, "reserveTokens")?;
        if !unsafe_budget(effective_keep, effective_reserve) {
            continue;
        }
        let repaired_keep = if explicit_keep.is_some() {
            effective_keep
        } else {
            keep.min(DEFAULT_BUDGET.keep_recent_tokens).min(context / 4)
        };
        let repaired_reserve = if explicit_reserve.is_some() {
            effective_reserve
        } else {
            reserve.min(DEFAULT_BUDGET.reserve_tokens).min(context / 4)
        };
        if unsafe_budget(repaired_keep, repaired_reserve) {
            bail!(
                "Pi model {id} has explicit compaction budgets too large for its {context}-token context; adjust compaction.modelOverrides[{key}] or verified model limits; left unchanged"
            );
        }
        let entry = pi_object(
            &mut compaction["modelOverrides"],
            &key,
            &format!("settings.json compaction.modelOverrides[{key}]"),
        )?;
        entry
            .entry("keepRecentTokens")
            .or_insert(json!(repaired_keep));
        entry
            .entry("reserveTokens")
            .or_insert(json!(repaired_reserve));
    }
    Ok(())
}
/// Read one of Pi's JSON configs; a missing or empty file is an empty object.
fn pi_document(path: &Path, name: &str) -> Result<Value> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    if content.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("Invalid Pi {name}; left unchanged"))?;
    if !value.is_object() {
        bail!("Pi {name} must be a JSON object; left unchanged");
    }
    Ok(value)
}
fn write_json(path: &Path, document: &Value) -> Result<()> {
    write_private(
        path,
        (serde_json::to_string_pretty(document)? + "\n").as_bytes(),
        true,
    )
}
/// Point Pi at this proxy: one provider entry holding the models this proxy
/// serves, plus the startup defaults. Other providers and settings are kept.
pub fn configure_pi(config: &Config, api_key: &str, home: Option<&Path>) -> Result<()> {
    // The proxy already knows where it listens and what it serves; nothing here
    // is worth retyping on the command line.
    let base_url = format!("http://{}/v1", config.local_address());
    let default = config
        .aliases
        .first()
        .map(|alias| alias.from.clone())
        .context("Proxy config has no model overwrites; add one to `aliases` first")?;
    let models = pi_models(config);
    let home = match home {
        Some(path) => path.to_path_buf(),
        // Pi's own directory, and its own override for it.
        None => std::env::var_os("PI_CODING_AGENT_DIR")
            .map(Into::into)
            .unwrap_or(
                std::path::PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                    .join(".pi/agent"),
            ),
    };
    let models_path = home.join("models.json");
    let mut catalog = pi_document(&models_path, "models.json")?;
    if !catalog["providers"].is_null() && !catalog["providers"].is_object() {
        bail!("Pi models.json providers must be an object; left unchanged");
    }
    // Keep full model entries by ID, including context/output limits, costs,
    // reasoning and compatibility fields. Retired IDs are removed below.
    let mut existing = BTreeMap::new();
    if let Some(provider) = catalog["providers"].get(PI_PROVIDER) {
        if !provider.is_object() {
            bail!("Pi models.json hey-proxy provider must be an object; left unchanged");
        }
        if let Some(entries) = provider.get("models") {
            let entries = entries
                .as_array()
                .context("Pi models.json hey-proxy models must be an array; left unchanged")?;
            for model in entries {
                let id = model
                    .get("id")
                    .and_then(Value::as_str)
                    .context("Pi models.json model must have a string id; left unchanged")?;
                if existing.insert(id.to_owned(), model.clone()).is_some() {
                    bail!("Pi models.json has duplicate model id {id}; left unchanged");
                }
            }
        }
    }
    let overrides = catalog["providers"][PI_PROVIDER]["modelOverrides"].clone();
    // Replace only our own provider, so a stale endpoint or key cannot survive.
    catalog["providers"][PI_PROVIDER] = json!({
        "name": PI_PROVIDER,
        "baseUrl": base_url,
        "api": "openai-responses",
        "apiKey": api_key,
        "models": models
            .iter()
            .map(|(id, reasoning)| {
                let mut model = existing.get(id).cloned().unwrap_or_else(|| json!({"id": id}));
                if *reasoning && model.get("reasoning").is_none() {
                    model["reasoning"] = json!(true);
                }
                model
            })
            .collect::<Vec<_>>(),
    });
    if !overrides.is_null() {
        catalog["providers"][PI_PROVIDER]["modelOverrides"] = overrides;
    }
    let settings_path = home.join("settings.json");
    let mut settings = pi_document(&settings_path, "settings.json")?;
    settings["defaultProvider"] = json!(PI_PROVIDER);
    settings["defaultModel"] = json!(default);
    if let Some(registry) = &config.model_registry {
        registry.validate()?;
        pi_registry(&mut settings, &mut catalog, config, registry)?;
    } else {
        pi_compaction(&mut settings, config, &catalog)?;
    }
    // Both documents parse and validate before either file is changed.
    write_json(&models_path, &catalog)?;
    write_json(&settings_path, &settings)?;
    if config.model_registry.is_some() {
        println!("Model limits and compaction generated from proxy config model_registry.");
    }
    println!(
        "Pi configured: {} → {base_url} ({} models, default {default})",
        models_path.display(),
        models.len()
    );
    println!("Restart running Pi sessions to reload model limits and compaction settings.");
    Ok(())
}
#[cfg(test)]
pub fn configure_codex(base_url: &str, home: Option<&Path>, model: Option<&str>) -> Result<()> {
    configure_codex_authenticated(base_url, home, model, None)
}
pub fn configure_codex_authenticated(
    base_url: &str,
    home: Option<&Path>,
    model: Option<&str>,
    token: Option<&str>,
) -> Result<()> {
    let url = reqwest::Url::parse(base_url).context("Invalid Codex base URL")?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"))
        || url.path() != "/v1"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("Codex base URL must be a local HTTP proxy URL ending in /v1");
    }
    if model.is_some_and(|m| m.trim().is_empty()) {
        bail!("Model must be nonempty");
    }
    let home = match home {
        Some(path) => path.to_path_buf(),
        None => std::env::var_os("CODEX_HOME").map(Into::into).unwrap_or(
            std::path::PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join(".codex"),
        ),
    };
    let path = home.join("config.toml");
    let original = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let changed = edit_codex_token(&original, base_url, model, token)?;
    // Modern Codex stores selected profiles as separate config files.
    let doc = original.parse::<DocumentMut>()?;
    let profile = doc.get("profile").and_then(Item::as_str);
    let mut profile_change = None;
    if let Some(profile) = profile {
        if profile.contains(['/', '\\']) || profile == ".." {
            bail!("Invalid Codex profile name");
        }
        let profile_path = home.join(format!("{profile}.config.toml"));
        if profile_path.exists() {
            let content = fs::read_to_string(&profile_path)?;
            let edited = edit_codex_token(&content, base_url, model, token)?;
            profile_change = Some((profile_path, edited));
        }
    }
    if let Some((profile_path, edited)) = profile_change {
        write_private(&profile_path, edited.as_bytes(), true)?;
    }
    write_private(&path, changed.as_bytes(), true)?;
    println!("Codex configured: {} → {base_url}", path.display());
    Ok(())
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
#[cfg(test)]
fn remote_config(config: &Config, host: &SshHost) -> Result<Config> {
    remote_config_with_key(config, host, None)
}
fn remote_config_with_key(config: &Config, host: &SshHost, key: Option<&str>) -> Result<Config> {
    let settings = host.settings();
    let mode = settings.map(|s| s.mode).unwrap_or_default();
    let mut remote = config.clone();
    remote.ssh_hosts.clear();
    remote.mode = mode;
    remote.connection = None;
    // Filesystem paths belong to their machine, not the controller.
    remote.logging.database = None;
    if settings.is_some_and(|s| s.gemini_via_controller) {
        if mode != Mode::Standalone || config.mode != Mode::Standalone {
            bail!("gemini_via_controller requires standalone controller and remote modes");
        }
        let provider = remote
            .gemini
            .as_mut()
            .context("Gemini provider required for controller tunnel")?;
        provider.upstream_url = "http://127.0.0.1:18082/v1beta".into();
        provider.auth = hey_proxy::gemini::Auth::ApiKey;
        // The SSH tunnel supplies transport authentication; the controller
        // replaces this marker with its own Google credential.
        provider.api_key = Some("controller-tunnel".into());
    }
    remote.listen = settings.and_then(|s| s.listen).unwrap_or_else(|| {
        if mode == Mode::Host {
            std::net::SocketAddr::from(([0, 0, 0, 0], config.listen.port()))
        } else {
            config.listen
        }
    });
    if mode != Mode::Host && !remote.listen.ip().is_loopback() || remote.listen.port() == 0 {
        bail!("Standalone/client rollout requires a loopback listen address with a nonzero port");
    }
    if mode == Mode::Client {
        let via = settings
            .and_then(|s| s.via.as_deref())
            .context("Client requires via")?;
        let target = config
            .ssh_hosts
            .iter()
            .find(|h| h.host() == via)
            .context("Unknown client host")?;
        let url = target
            .settings()
            .and_then(|s| s.url.clone())
            .context("Host requires url")?;
        remote.connection = Some(ClientConnection {
            url,
            api_key: key.context("Host key not provisioned")?.into(),
        });
        remote.api_keys.clear();
        remote.gemini = None;
        remote.claude = None;
        remote.codex = None;
        remote.aliases.clear();
        remote.fallbacks.clear();
        remote.upstream_url = "https://api.openai.com".into();
        remote.default = Default::default();
    }
    remote.validate()?;
    Ok(remote)
}
/// Resolve credentials before installation; never serialize resolved values.
/// A missing/unavailable 1Password source must fail before replacing a service.
pub(crate) async fn check_credentials(config: &Config) -> Result<()> {
    use hey_proxy::{credentials::CredentialResolver, gemini::Auth};
    if config.api_keys.is_empty()
        && config.gemini.is_none()
        && config.claude.is_none()
        && config.codex.is_none()
        && config.mode != Mode::Client
    {
        bail!("No provider credentials configured; edit the proxy config first");
    }
    let resolver = CredentialResolver::default();
    let ttl = std::time::Duration::from_secs(config.credential_cache_seconds);
    for source in config.api_keys.values() {
        resolver
            .resolve(source, ttl)
            .await
            .context("OpenAI credential source unavailable")?;
    }
    if let Some(provider) = &config.gemini {
        match provider.auth {
            Auth::Adc => {
                resolver
                    .adc_headers()
                    .await
                    .context("Gemini ADC unavailable")?;
            }
            Auth::GcloudAdc => {
                resolver
                    .resolve(
                        "sh://gcloud auth application-default print-access-token",
                        ttl,
                    )
                    .await
                    .context("Gemini gcloud ADC unavailable")?;
            }
            Auth::ApiKey | Auth::Bearer => {
                resolver
                    .resolve(provider.api_key.as_deref().unwrap_or(""), ttl)
                    .await
                    .context("Gemini credential source unavailable")?;
            }
        }
    }
    Ok(())
}
fn source_bundle(dir: &Path) -> Result<()> {
    // Embedded sources make the installed CLI independent of the checkout and remote architecture.
    // build.rs lists every file under src/ and tests/, so new modules ship automatically.
    const FILES: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/bundle_files.rs"));
    for (name, content) in FILES {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, content)?;
    }
    Ok(())
}
fn ssh(host: &str, script: &str) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "--",
        host,
        script,
    ]);
    cmd
}
async fn checked(cmd: &mut Command, label: &str) -> Result<()> {
    if !cmd
        .status()
        .await
        .with_context(|| format!("Could not run {label}"))?
        .success()
    {
        bail!("{label} failed");
    }
    Ok(())
}
async fn prepare_host(host: &SshHost) -> Result<()> {
    let Some(settings) = host.settings() else {
        return Ok(());
    };
    for argv in &settings.prepare {
        println!(
            "{}: running connection preparation ({})",
            host.host(),
            argv[0]
        );
        checked(
            Command::new(&argv[0]).args(&argv[1..]),
            "host connection preparation",
        )
        .await?;
    }
    Ok(())
}
async fn deploy(config: &Config, host: &SshHost, access_key: Option<&str>) -> Result<()> {
    prepare_host(host).await?;
    let mut remote = remote_config_with_key(config, host, access_key)?;
    let temp = tempfile::tempdir()?;
    source_bundle(temp.path())?;
    let tunneled = host.settings().is_some_and(|s| s.gemini_via_controller);
    if tunneled {
        checked(
            Command::new("python3")
                .arg(temp.path().join("src/controller_tunnel.py"))
                .arg(host.host())
                .arg(config.local_address().to_string()),
            "Gemini controller tunnel",
        )
        .await?;
    }
    // Never put static API keys into the source archive/staging directories.
    // Existing remote keys must match; new keys require a protected source.
    use sha2::{Digest, Sha256};
    let mut retained = std::collections::BTreeMap::new();
    for (project, source) in &mut remote.api_keys {
        if !source.starts_with("sh://") && !source.starts_with("op://") {
            retained.insert(
                project.clone(),
                format!("{:x}", Sha256::digest(source.as_bytes())),
            );
            *source = "retained-on-destination".into();
        }
    }
    let mut retained_gemini = None;
    if !tunneled
        && let Some(source) = remote.gemini.as_mut().and_then(|g| g.api_key.as_mut())
        && !source.starts_with("sh://")
        && !source.starts_with("op://")
    {
        retained_gemini = Some(format!("{:x}", Sha256::digest(source.as_bytes())));
        *source = "retained-on-destination".into();
    }
    let gemini_profile = config.gemini.as_ref().map(|_| serde_json::json!({
        "base_url": if tunneled {"http://127.0.0.1:18082/v1".to_owned()} else {format!("http://{}/v1", remote.local_address())},
        "model": host.settings().and_then(|s| s.gemini_model.as_deref()).unwrap_or("gemini/gemini-3.1-pro-preview")
    }));
    write_private(
        &temp.path().join("deployment.json"),
        &serde_json::to_vec_pretty(&serde_json::json!({
            "retained_openai_keys":retained,"retained_gemini_key":retained_gemini,"gemini_profile":gemini_profile
        }))?,
        false,
    )?;
    write_private(
        &temp.path().join("remote-config.json"),
        &serde_json::to_vec_pretty(&remote)?,
        false,
    )?;
    let archive = temp.path().join("bundle.tar");
    checked(
        Command::new("tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("--no-xattrs")
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(temp.path())
            .args([
                "Cargo.toml",
                "Cargo.lock",
                "README.md",
                "LICENSE",
                "build.rs",
                "src",
                "tests",
                "remote-config.json",
                "deployment.json",
            ]),
        "source archive",
    )
    .await?;
    let output = ssh(
        host.host(),
        "umask 077; mkdir -p \"$HOME/.hey-proxy/staging\" && mktemp -d \"$HOME/.hey-proxy/staging/hey-proxy-rollout.XXXXXXXX\"",
    )
    .output()
    .await?;
    if !output.status.success() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        bail!("SSH connection or remote staging failed");
    }
    let stage = String::from_utf8(output.stdout)?.trim().to_owned();
    if !stage.starts_with('/')
        || !stage.contains("/.hey-proxy/staging/hey-proxy-rollout.")
        || !stage
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-._".contains(&b))
    {
        bail!("Unexpected remote staging path");
    }
    let result = async {
        let file = fs::File::open(&archive)?;
        checked(
            ssh(host.host(), &format!("tar -xf - -C {}", quote(&stage))).stdin(Stdio::from(file)),
            "SSH upload",
        )
        .await?;
        let local_address = remote.local_address();
        let base_url = format!("http://{local_address}/v1");
        let settings = host.settings();
        let home = settings.and_then(|s| s.codex_home.as_deref()).unwrap_or("");
        let model = settings.and_then(|s| s.model.as_deref()).unwrap_or("");
        let script = format!(
            "sh {}/src/rollout.sh {} {} {} {}",
            quote(&stage),
            quote(&stage),
            quote(&base_url),
            quote(home),
            quote(model)
        );
        let script = format!("exec \"${{SHELL:-/bin/sh}}\" -lic {}", quote(&script));
        checked(
            &mut ssh(host.host(), &script),
            "remote install, config sync and service verification",
        )
        .await
    }
    .await;
    let _ = ssh(host.host(), &format!("rm -rf -- {}", quote(&stage)))
        .status()
        .await;
    result
}
async fn provision_keys(config: &Config, host: &SshHost) -> Result<crate::access::Keys> {
    let mut script = String::from("\"$HOME/.cargo/bin/hey-proxy\" host-keys");
    for client in &config.ssh_hosts {
        if client
            .settings()
            .is_some_and(|s| s.mode == Mode::Client && s.via.as_deref() == Some(host.host()))
        {
            script.push_str(&format!(" --client {}", quote(client.host())));
        }
    }
    let output = ssh(host.host(), &script).output().await?;
    if !output.status.success() {
        bail!("Host access-key provisioning failed");
    }
    serde_json::from_slice(&output.stdout).context("Invalid host access-key response")
}
pub async fn run(config: &Config, selected: &[String]) -> Result<()> {
    config.validate()?;
    if config.mode == Mode::Client {
        bail!("Run rollout from a standalone or host controller with upstream configuration");
    }
    for name in selected {
        if !config.ssh_hosts.iter().any(|h| h.host() == name) {
            bail!("Host {name} is not in ssh_hosts");
        }
    }
    let mut wanted: Vec<String> = config
        .ssh_hosts
        .iter()
        .filter(|h| selected.is_empty() || selected.iter().any(|s| s == h.host()))
        .map(|h| h.host().into())
        .collect();
    if wanted.is_empty() {
        bail!("No SSH hosts configured; add ssh_hosts to the config");
    }
    // Selecting a client includes its host dependency.
    for host in &config.ssh_hosts {
        if wanted.iter().any(|n| n == host.host())
            && let Some(via) = host.settings().and_then(|s| s.via.as_ref())
            && !wanted.contains(via)
        {
            wanted.push(via.clone());
        }
    }
    let mut hosts: Vec<_> = config
        .ssh_hosts
        .iter()
        .filter(|h| wanted.iter().any(|n| n == h.host()))
        .collect();
    hosts.sort_by_key(|h| match h.settings().map(|s| s.mode).unwrap_or_default() {
        Mode::Host => 0,
        Mode::Standalone => 1,
        Mode::Client => 2,
    });
    for host in &hosts {
        remote_config_with_key(config, host, Some("validation-only"))?;
    }
    let mut keys = std::collections::BTreeMap::new();
    let mut failed = Vec::new();
    for host in hosts {
        let mode = host.settings().map(|s| s.mode).unwrap_or_default();
        let client_key = if mode == Mode::Client {
            let via = host.settings().and_then(|s| s.via.as_ref()).unwrap();
            match keys
                .get(via)
                .and_then(|keys: &crate::access::Keys| keys.clients.get(host.host()))
                .cloned()
            {
                Some(key) => Some(key),
                None => {
                    eprintln!("{}: skipped because its host failed", host.host());
                    failed.push(host.host().to_owned());
                    continue;
                }
            }
        } else {
            None
        };
        println!(
            "{}: deploying {:?} mode, syncing config and verifying connections",
            host.host(),
            mode
        );
        let result = async {
            deploy(config, host, client_key.as_deref()).await?;
            if mode == Mode::Host {
                keys.insert(host.host().to_owned(), provision_keys(config, host).await?);
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => println!("{}: rollout verified", host.host()),
            Err(error) => {
                eprintln!("{}: {error:#}", host.host());
                failed.push(host.host().to_owned());
            }
        }
    }
    if !failed.is_empty() {
        bail!("Rollout failed on: {}", failed.join(", "));
    }
    Ok(())
}

pub async fn verify(
    config: &Config,
    config_path: &Path,
    codex_home: Option<&Path>,
    check_codex: bool,
) -> Result<()> {
    let token = if config.mode == Mode::Host {
        Some(crate::access::read(config_path)?.local)
    } else {
        None
    };
    let address = config.local_address();
    let base = format!("http://{address}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let mut request = client.get(format!("{base}/logs/api"));
    if let Some(token) = &token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .context("Cannot reach local logs API")?;
    if !response.status().is_success() {
        bail!("Local logs API returned {}", response.status());
    }
    let data: serde_json::Value = response.json().await?;
    if !data["entries"].is_array() {
        bail!("Unexpected logs API response");
    }
    if config.mode == Mode::Host
        && client
            .get(format!("{base}/logs/api"))
            .send()
            .await?
            .status()
            != reqwest::StatusCode::UNAUTHORIZED
    {
        bail!("Host logs API must reject unauthenticated requests");
    }
    if !config.api_keys.is_empty() || config.mode == Mode::Client {
        // No model generation: authenticated model listing verifies the entire forwarding connection.
        let mut request = client.get(format!("{base}/v1/models"));
        if let Some(token) = &token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .context("Cannot reach upstream through this proxy")?;
        if !response.status().is_success() {
            bail!("Upstream connection check returned {}", response.status());
        }
        let models: serde_json::Value = response
            .json()
            .await
            .context("Upstream model listing was not JSON")?;
        if !models["data"].is_array() {
            bail!("Upstream returned an invalid model listing");
        }
    } else {
        check_credentials(config).await?;
    }
    if let Some(connection) = &config.connection {
        let response = client
            .get(format!("{}/logs/api", connection.url.trim_end_matches('/')))
            .bearer_auth(&connection.api_key)
            .send()
            .await
            .context("Client cannot reach its assigned host")?;
        if !response.status().is_success()
            || !response.json::<serde_json::Value>().await?["entries"].is_array()
        {
            bail!("Assigned host logs API failed");
        }
    }
    if !check_codex {
        println!("Verified proxy service and upstream/host connection");
        return Ok(());
    }
    let home = codex_home
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("CODEX_HOME").map(Into::into))
        .unwrap_or(
            std::path::PathBuf::from(std::env::var_os("HOME").context("HOME unset")?)
                .join(".codex"),
        );
    let doc = fs::read_to_string(home.join("config.toml"))?.parse::<DocumentMut>()?;
    let expected = format!("{base}/v1");
    fn check_provider(
        doc: &DocumentMut,
        provider_id: &str,
        expected: &str,
        token: Option<&str>,
    ) -> Result<()> {
        if doc.get("model_provider").and_then(Item::as_str) != Some(provider_id) {
            bail!("Codex does not select {provider_id}");
        }
        let provider = doc
            .get("model_providers")
            .and_then(|p| p.get(provider_id))
            .with_context(|| format!("Codex provider {provider_id} missing"))?;
        if provider.get("base_url").and_then(Item::as_str) != Some(expected)
            || provider.get("wire_api").and_then(Item::as_str) != Some("responses")
            || provider.get("requires_openai_auth").and_then(Item::as_bool) != Some(false)
        {
            bail!("Codex provider configuration is incorrect");
        }
        if provider
            .get("experimental_bearer_token")
            .and_then(Item::as_str)
            != token
        {
            bail!("Codex host credential is incorrect");
        }
        if doc
            .get("features")
            .and_then(|f| f.get("enable_request_compression"))
            .and_then(Item::as_bool)
            != Some(false)
        {
            bail!("Codex request compression must be disabled for routing");
        }
        Ok(())
    }
    check_provider(&doc, "hey-proxy", &expected, token.as_deref())?;
    if let Some(profile) = doc.get("profile").and_then(Item::as_str) {
        if let Some(active) = doc.get("profiles").and_then(|p| p.get(profile))
            && (active.get("model_provider").and_then(Item::as_str) != Some("hey-proxy")
                || active
                    .get("features")
                    .and_then(|f| f.get("enable_request_compression"))
                    .and_then(Item::as_bool)
                    != Some(false))
        {
            bail!("Active inline Codex profile overrides proxy settings");
        }
        let path = home.join(format!("{profile}.config.toml"));
        if path.exists() {
            check_provider(
                &fs::read_to_string(path)?.parse::<DocumentMut>()?,
                "hey-proxy",
                &expected,
                token.as_deref(),
            )?;
        }
    }
    let gemini_path = home.join("gemini.config.toml");
    if gemini_path.exists() {
        check_provider(
            &fs::read_to_string(gemini_path)?.parse::<DocumentMut>()?,
            "hey_proxy_gemini",
            &expected,
            token.as_deref(),
        )?;
    }
    println!(
        "Verified {:?}: logs API, upstream/host connection and Codex configuration",
        config.mode
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn pi_catalog_lists_every_overwrite_and_the_models_it_routes_to() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "listen": "127.0.0.1:8080",
            "aliases": [
                {"from": "coding", "to": "gpt-4.1"},
                {"from": "careful", "to": "gpt-5", "reasoning": "high"},
                {"from": "reasoning", "to": "gpt-5", "reasoning_routes": {"low": {"to": "gpt-5-mini"}}}
            ]
        }))
        .unwrap();
        let models = pi_models(&config);
        assert_eq!(
            models.keys().cloned().collect::<Vec<_>>(),
            [
                "careful",
                "coding",
                "gpt-4.1",
                "gpt-5",
                "gpt-5-mini",
                "reasoning"
            ]
        );
        // Only the overwrites that route on effort ask Pi to send one.
        assert_eq!(
            models
                .iter()
                .filter(|(_, reasoning)| **reasoning)
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["careful", "reasoning"]
        );
    }

    #[test]
    fn pi_setup_replaces_only_our_provider_and_keeps_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let config: Config = serde_json::from_value(serde_json::json!({
            // A wildcard bind still points Pi at the loopback address.
            "listen": "0.0.0.0:18080",
            "aliases": [{"from": "coding", "to": "gpt-4.1"}, {"from": "fast", "to": "gpt-4.1-mini"}]
        }))
        .unwrap();
        fs::write(
            home.join("models.json"),
            r#"{"providers":{"mine":{"baseUrl":"http://localhost:1/v1","models":[{"id":"local"}]},"hey-proxy":{"apiKey":"stale","models":[{"id":"gone"}],"modelOverrides":{"coding":{"contextWindow":400000}}}}}"#,
        )
        .unwrap();
        fs::write(home.join("settings.json"), r#"{"theme":"light"}"#).unwrap();
        configure_pi(&config, "local-token", Some(home)).unwrap();

        let catalog: Value =
            serde_json::from_slice(&fs::read(home.join("models.json")).unwrap()).unwrap();
        assert_eq!(catalog["providers"]["mine"]["models"][0]["id"], "local");
        let provider = &catalog["providers"]["hey-proxy"];
        assert_eq!(provider["baseUrl"], "http://127.0.0.1:18080/v1");
        assert_eq!(provider["api"], "openai-responses");
        assert_eq!(provider["apiKey"], "local-token");
        assert_eq!(
            provider["models"]
                .as_array()
                .unwrap()
                .iter()
                .map(|model| model["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["coding", "fast", "gpt-4.1", "gpt-4.1-mini"]
        );
        // A stale model is gone, but the user's own per-model tuning survives.
        assert_eq!(
            provider["modelOverrides"]["coding"]["contextWindow"],
            400000
        );
        let settings: Value =
            serde_json::from_slice(&fs::read(home.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["theme"], "light");
        assert_eq!(settings["defaultProvider"], "hey-proxy");
        // The first overwrite in the config is where Pi starts.
        assert_eq!(settings["defaultModel"], "coding");

        let written = fs::read(home.join("models.json")).unwrap();
        configure_pi(&config, "local-token", Some(home)).unwrap();
        assert_eq!(fs::read(home.join("models.json")).unwrap(), written);
    }

    #[test]
    fn pi_setup_needs_an_overwrite_and_leaves_a_broken_catalog_alone() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let config: Config = serde_json::from_value(serde_json::json!({
            "listen": "127.0.0.1:8080",
            "aliases": [{"from": "coding", "to": "gpt-4.1"}]
        }))
        .unwrap();
        // Nothing to offer Pi until the proxy has an overwrite to serve.
        let empty: Config =
            serde_json::from_value(serde_json::json!({"listen": "127.0.0.1:8080"})).unwrap();
        assert!(configure_pi(&empty, "k", Some(home)).is_err());
        assert!(!home.join("models.json").exists());
        assert!(!home.join("settings.json").exists());

        let broken = b"{\"providers\":";
        fs::write(home.join("models.json"), broken).unwrap();
        assert!(configure_pi(&config, "k", Some(home)).is_err());
        assert_eq!(fs::read(home.join("models.json")).unwrap(), broken);
        assert!(!home.join("settings.json").exists());

        fs::write(home.join("models.json"), b"[]").unwrap();
        assert!(configure_pi(&config, "k", Some(home)).is_err());
        assert_eq!(fs::read(home.join("models.json")).unwrap(), b"[]");
    }

    #[test]
    fn gemini_profile_upgrades_disabled_retries_and_preserves_positive_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("gemini.config.toml");
        for (request, stream, expected_request, expected_stream) in [(0, 0, 4, 5), (7, 8, 7, 8)] {
            fs::write(&profile,format!("[model_providers.hey_proxy_gemini]\nrequest_max_retries={request}\nstream_max_retries={stream}\n")).unwrap();
            configure_gemini(
                "http://127.0.0.1:8080/v1",
                "gemini/gemini-3.1-pro-preview",
                Some(dir.path()),
            )
            .unwrap();
            let doc = fs::read_to_string(&profile)
                .unwrap()
                .parse::<DocumentMut>()
                .unwrap();
            let provider = &doc["model_providers"]["hey_proxy_gemini"];
            assert_eq!(
                provider["request_max_retries"].as_integer(),
                Some(expected_request)
            );
            assert_eq!(
                provider["stream_max_retries"].as_integer(),
                Some(expected_stream)
            );
        }
    }

    #[test]
    fn gemini_profile_updates_preserve_preferences_and_default_config() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.toml"), "model='openai-default'\n").unwrap();
        let profile = dir.path().join("gemini.config.toml");
        fs::write(&profile, "# user preferences\nmodel_reasoning_effort='low'\napproval_policy='on-request'\n[model_providers.hey_proxy_gemini]\nstream_idle_timeout_ms=1800000\nenv_key='STALE_KEY'\n").unwrap();
        configure_gemini(
            "http://127.0.0.1:18082/v1",
            "gemini/gemini-3.1-pro-preview",
            Some(dir.path()),
        )
        .unwrap();
        let first = fs::read_to_string(&profile).unwrap();
        let parsed = first.parse::<DocumentMut>().unwrap();
        assert_eq!(parsed["model_reasoning_effort"].as_str(), Some("low"));
        assert_eq!(parsed["approval_policy"].as_str(), Some("on-request"));
        assert_eq!(
            parsed["model_providers"]["hey_proxy_gemini"]["stream_idle_timeout_ms"].as_integer(),
            Some(1800000)
        );
        assert!(
            parsed["model_providers"]["hey_proxy_gemini"]
                .get("env_key")
                .is_none()
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            "model='openai-default'\n"
        );
        configure_gemini(
            "http://127.0.0.1:18082/v1",
            "gemini/gemini-3.1-pro-preview",
            Some(dir.path()),
        )
        .unwrap();
        assert_eq!(first, fs::read_to_string(&profile).unwrap());
        assert!(
            configure_gemini(
                "https://outside.example/v1",
                "gemini/gemini-3.1-pro-preview",
                Some(dir.path())
            )
            .is_err()
        );
        fs::write(&profile, "broken=[").unwrap();
        assert!(
            configure_gemini(
                "http://127.0.0.1:8080/v1",
                "gemini/gemini-3.1-pro-preview",
                Some(dir.path())
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(profile).unwrap(), "broken=[");
    }

    #[test]
    fn controller_tunnel_does_not_copy_adc_command_or_logging_path() {
        let mut config = Config {
            gemini: Some(serde_json::from_value(serde_json::json!({"auth":"bearer","api_key":"sh:///controller/gcloud auth application-default print-access-token"})).unwrap()),
            ..Config::test_fixture()
        };
        config.logging.database = Some("/controller/logs.sqlite".into());
        let host: SshHost = serde_json::from_value(
            serde_json::json!({"host":"remote","gemini_via_controller":true}),
        )
        .unwrap();
        let remote = remote_config(&config, &host).unwrap();
        let gemini = remote.gemini.unwrap();
        assert_eq!(gemini.upstream_url, "http://127.0.0.1:18082/v1beta");
        assert_eq!(gemini.api_key.as_deref(), Some("controller-tunnel"));
        assert!(remote.logging.database.is_none());
        assert_eq!(remote.api_keys, config.api_keys);
    }

    #[test]
    fn onepassword_references_survive_remote_serialization_without_resolution() {
        let mut config = Config::test_fixture();
        config
            .api_keys
            .insert("default".into(), "op://Agents/hey-proxy/codex".into());
        config.gemini = Some(
            serde_json::from_value(serde_json::json!({
                "auth":"api_key","api_key":"op://Agents/hey-proxy/gemini"
            }))
            .unwrap(),
        );
        let host: SshHost = serde_json::from_value(
            serde_json::json!({"host":"fixture-op","listen":"127.0.0.1:8080"}),
        )
        .unwrap();
        let remote = remote_config(&config, &host).unwrap();
        let value = serde_json::to_value(remote).unwrap();
        assert_eq!(
            value["providers"]["openai"]["api_keys"]["default"],
            "op://Agents/hey-proxy/codex"
        );
        assert_eq!(
            value["providers"]["gemini"]["api_key"],
            "op://Agents/hey-proxy/gemini"
        );
        assert!(value.get("api_keys").is_none());
    }

    #[tokio::test]
    async fn credential_preflight_fails_without_echoing_secret_command_output() {
        let mut config = Config::test_fixture();
        config.api_keys.clear();
        config.api_keys.insert(
            "default".into(),
            "sh://printf PRIVATE_SECRET; exit 1".into(),
        );
        let error = format!("{:#}", check_credentials(&config).await.unwrap_err());
        assert!(!error.contains("PRIVATE_SECRET"));
        assert!(error.contains("OpenAI credential source unavailable"));
    }

    use super::*;
    #[test]
    fn codex_changes_are_idempotent_and_preserve_unrelated_settings() {
        let content = "# my configuration\nmodel = 'existing-model'\napproval_policy = 'on-request'\nprofile = 'work'\n[projects.'/project']\ntrust_level = 'trusted'\n[model_providers.other]\nname = 'Other'\n[profiles.work]\nmodel_provider = 'other'\n";
        let edited = edit_codex(content, "http://127.0.0.1:8080/v1", None).unwrap();
        let doc = edited.parse::<DocumentMut>().unwrap();
        assert_eq!(doc["model"].as_str(), Some("existing-model"));
        assert_eq!(doc["approval_policy"].as_str(), Some("on-request"));
        assert_eq!(
            doc["projects"]["/project"]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(
            doc["model_providers"]["other"]["name"].as_str(),
            Some("Other")
        );
        assert_eq!(
            doc["profiles"]["work"]["model_provider"].as_str(),
            Some("hey-proxy")
        );
        assert_eq!(
            doc["features"]["enable_request_compression"].as_bool(),
            Some(false)
        );
        assert!(edited.contains("# my configuration"));
        assert_eq!(
            doc["model_providers"]["hey-proxy"]["stream_idle_timeout_ms"].as_integer(),
            Some(300_000)
        );
        assert_eq!(
            edited,
            edit_codex(&edited, "http://127.0.0.1:8080/v1", None).unwrap()
        );
    }
    #[test]
    fn configuring_codex_preserves_an_extended_idle_timeout() {
        let content = "[model_providers.hey-proxy]\nstream_idle_timeout_ms = 900000\n";
        let edited = edit_codex(content, "http://127.0.0.1:8080/v1", None).unwrap();
        let doc = edited.parse::<DocumentMut>().unwrap();
        assert_eq!(
            doc["model_providers"]["hey-proxy"]["stream_idle_timeout_ms"].as_integer(),
            Some(900_000)
        );
    }
    #[test]
    fn malformed_codex_is_not_overwritten_and_backups_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "invalid=[").unwrap();
        assert!(configure_codex("http://127.0.0.1:8080/v1", Some(dir.path()), None).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "invalid=[");
        fs::write(&path, "model='old'\n").unwrap();
        configure_codex("http://127.0.0.1:8080/v1", Some(dir.path()), None).unwrap();
        let files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 2);
        configure_codex("http://127.0.0.1:8080/v1", Some(dir.path()), None).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for file in files {
                assert_eq!(
                    fs::metadata(file).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }
    #[test]
    fn remote_sync_omits_host_inventory_and_preserves_routes() {
        let mut config = Config {
            ssh_hosts: vec![SshHost::Name("builder".into())],
            ..Config::test_fixture()
        };
        config.fallbacks =
            serde_json::from_value(serde_json::json!({"model-primary":["model-secondary"]}))
                .unwrap();
        let remote = remote_config(&config, &config.ssh_hosts[0]).unwrap();
        assert!(remote.ssh_hosts.is_empty());
        assert_eq!(remote.aliases[1].to, config.aliases[1].to);
        assert_eq!(remote.api_keys, config.api_keys);
        assert_eq!(
            serde_json::to_value(&remote.fallbacks).unwrap(),
            serde_json::to_value(&config.fallbacks).unwrap()
        );
        let dir = tempfile::tempdir().unwrap();
        source_bundle(dir.path()).unwrap();
        assert!(dir.path().join("src/remote_service.py").exists());
        assert!(dir.path().join("src/rollout.rs").exists());
        assert!(dir.path().join("src/proxy/dashboard.js").exists());
        assert!(dir.path().join("src/proxy/recovery.rs").exists());
        for file in [
            "src/fallback.rs",
            "src/proxy/fallback.rs",
            "src/proxy/fallback/tests.rs",
            "tests/fallback_policy.rs",
            "src/proxy/overview.rs",
            "src/proxy/overview.html",
            "src/proxy/overview.js",
            "src/proxy/overview/tests.rs",
            "src/proxy/chat.rs",
            "src/proxy/chat/request.rs",
            "src/proxy/chat/response.rs",
            "src/proxy/chat/stream.rs",
            "src/proxy/chat/tests.rs",
            "src/proxy/sse.rs",
            "src/model_registry.rs",
            "src/proxy/messages.rs",
            "src/proxy/messages/gemini.rs",
            "build.rs",
            "Cargo.lock",
        ] {
            assert!(
                dir.path().join(file).exists(),
                "Missing rollout source: {file}"
            );
        }
    }
    #[tokio::test]
    async fn invalid_selection_does_not_require_ssh() {
        let config = Config {
            ssh_hosts: vec![SshHost::Name("builder".into())],
            ..Config::test_fixture()
        };
        assert!(run(&config, &["missing".into()]).await.is_err());
        let mut invalid = config.clone();
        invalid.ssh_hosts = vec![SshHost::Name("-oProxyCommand=malicious".into())];
        assert!(invalid.validate().is_err());
        invalid.ssh_hosts = vec![
            SshHost::Name("builder".into()),
            SshHost::Name("builder".into()),
        ];
        assert!(invalid.validate().is_err());
        invalid = config;
        invalid.listen = "0.0.0.0:8080".parse().unwrap();
        assert!(run(&invalid, &[]).await.is_err());
    }
    #[test]
    fn selected_profile_file_is_updated_with_backup() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.toml"), "profile='work'\n").unwrap();
        fs::write(
            dir.path().join("work.config.toml"),
            "model_provider='other'\nmodel='existing'\n",
        )
        .unwrap();
        configure_codex("http://127.0.0.1:9090/v1", Some(dir.path()), None).unwrap();
        let profile = fs::read_to_string(dir.path().join("work.config.toml"))
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(profile["model_provider"].as_str(), Some("hey-proxy"));
        assert_eq!(profile["model"].as_str(), Some("existing"));
    }
}
