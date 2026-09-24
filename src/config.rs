use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::Path,
    time::SystemTime,
};

#[derive(Clone, Deserialize)]
#[serde(try_from = "ConfigFile")]
pub struct Config {
    pub fallbacks: hey_proxy::fallback::Fallbacks,
    #[serde(default)]
    pub ssh_hosts: Vec<SshHost>,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ClientConnection>,
    pub listen: SocketAddr,
    #[serde(default = "default_upstream")]
    pub upstream_url: String,
    #[serde(default)]
    pub api_keys: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini: Option<hey_proxy::gemini::ProviderConfig>,
    #[serde(default)]
    pub default: DefaultRoute,
    #[serde(default = "default_credential_cache_seconds")]
    pub credential_cache_seconds: u64,
    #[serde(default, alias = "alias")]
    pub aliases: Vec<Alias>,
    #[serde(default)]
    pub retry: Retry,
    #[serde(default)]
    pub logging: Logging,
    /// User-level direction to skip blocked security work and continue permitted work.
    #[serde(default)]
    pub skip_blocked_security_work: bool,
    /// Address family for upstream connections. `auto` lets the OS choose, which usually
    /// prefers IPv6 when the upstream has AAAA records.
    #[serde(default)]
    pub ip_version: IpVersion,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    fallbacks: hey_proxy::fallback::Fallbacks,
    #[serde(default)]
    providers: Providers,
    #[serde(default)]
    ssh_hosts: Vec<SshHost>,
    #[serde(default)]
    mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection: Option<ClientConnection>,
    listen: SocketAddr,
    #[serde(default = "default_upstream")]
    upstream_url: String,
    #[serde(default)]
    api_keys: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gemini: Option<hey_proxy::gemini::ProviderConfig>,
    #[serde(default)]
    default: DefaultRoute,
    #[serde(default = "default_credential_cache_seconds")]
    credential_cache_seconds: u64,
    #[serde(default, alias = "alias")]
    aliases: Vec<Alias>,
    #[serde(default)]
    retry: Retry,
    #[serde(default)]
    logging: Logging,
    /// User-level direction to skip blocked security work and continue permitted work.
    #[serde(default)]
    skip_blocked_security_work: bool,
    /// Address family for upstream connections. `auto` lets the OS choose, which usually
    /// prefers IPv6 when the upstream has AAAA records.
    #[serde(default)]
    ip_version: IpVersion,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Providers {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    openai: Option<OpenAiProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gemini: Option<hey_proxy::gemini::ProviderConfig>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OpenAiProvider {
    #[serde(default = "default_upstream")]
    upstream_url: String,
    #[serde(default)]
    api_keys: BTreeMap<String, String>,
    #[serde(default)]
    default: DefaultRoute,
    #[serde(default = "default_credential_cache_seconds")]
    credential_cache_seconds: u64,
}
impl TryFrom<ConfigFile> for Config {
    type Error = anyhow::Error;
    fn try_from(mut file: ConfigFile) -> Result<Self> {
        if let Some(openai) = file.providers.openai {
            if !file.api_keys.is_empty()
                || file.upstream_url != default_upstream()
                || file.default.api_key != "default"
            {
                bail!("Use providers.openai or legacy OpenAI fields, not both");
            }
            file.upstream_url = openai.upstream_url;
            file.api_keys = openai.api_keys;
            file.default = openai.default;
            file.credential_cache_seconds = openai.credential_cache_seconds;
        }
        if let Some(gemini) = file.providers.gemini {
            if file.gemini.is_some() {
                bail!("Use providers.gemini or legacy gemini, not both");
            }
            file.gemini = Some(gemini);
        }
        Ok(Self {
            fallbacks: file.fallbacks,
            ssh_hosts: file.ssh_hosts,
            mode: file.mode,
            connection: file.connection,
            listen: file.listen,
            upstream_url: file.upstream_url,
            api_keys: file.api_keys,
            gemini: file.gemini,
            default: file.default,
            credential_cache_seconds: file.credential_cache_seconds,
            aliases: file.aliases,
            retry: file.retry,
            logging: file.logging,
            skip_blocked_security_work: file.skip_blocked_security_work,
            ip_version: file.ip_version,
        })
    }
}
impl Serialize for Config {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let providers = if self.mode == Mode::Client {
            Providers::default()
        } else {
            Providers {
                openai: Some(OpenAiProvider {
                    upstream_url: self.upstream_url.clone(),
                    api_keys: self.api_keys.clone(),
                    default: self.default.clone(),
                    credential_cache_seconds: self.credential_cache_seconds,
                }),
                gemini: self.gemini.clone(),
            }
        };
        let mut value = serde_json::json!({"mode":self.mode,"listen":self.listen,"aliases":self.aliases,"retry":self.retry,"logging":self.logging,"ip_version":self.ip_version,"skip_blocked_security_work":self.skip_blocked_security_work});
        if self.mode != Mode::Client {
            if !self.fallbacks.is_empty() {
                value["fallbacks"] =
                    serde_json::to_value(&self.fallbacks).map_err(serde::ser::Error::custom)?;
            }
            value["providers"] =
                serde_json::to_value(providers).map_err(serde::ser::Error::custom)?;
        }
        if !self.ssh_hosts.is_empty() {
            value["ssh_hosts"] =
                serde_json::to_value(&self.ssh_hosts).map_err(serde::ser::Error::custom)?;
        }
        if let Some(connection) = &self.connection {
            value["connection"] =
                serde_json::to_value(connection).map_err(serde::ser::Error::custom)?;
        }
        value.serialize(serializer)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Standalone,
    Host,
    Client,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConnection {
    pub url: String,
    pub api_key: String,
}
fn default_credential_cache_seconds() -> u64 {
    2400
}
fn default_upstream() -> String {
    "https://api.openai.com".into()
}

/// SSH aliases and user@host destinations from ~/.ssh/config are supported.
#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SshHost {
    Name(String),
    Settings(RemoteHost),
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteHost {
    pub host: String,
    /// Use a supervised SSH reverse tunnel for controller-owned Gemini ADC.
    #[serde(default)]
    pub gemini_via_controller: bool,
    #[serde(default)]
    pub gemini_model: Option<String>,
    #[serde(default)]
    pub mode: Mode,
    /// SSH host name of the shared host this client uses.
    #[serde(default)]
    pub via: Option<String>,
    /// Service root URL reachable from clients.
    #[serde(default)]
    pub url: Option<String>,
    /// Local argv commands run before connecting (for VPN/SSH login).
    #[serde(default)]
    pub prepare: Vec<Vec<String>>,
    #[serde(default)]
    pub listen: Option<SocketAddr>,
    #[serde(default)]
    pub codex_home: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}
impl SshHost {
    pub fn host(&self) -> &str {
        match self {
            Self::Name(host) => host,
            Self::Settings(settings) => &settings.host,
        }
    }
    pub fn settings(&self) -> Option<&RemoteHost> {
        match self {
            Self::Name(_) => None,
            Self::Settings(settings) => Some(settings),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IpVersion {
    #[default]
    Auto,
    Ipv4,
    Ipv6,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultRoute {
    pub api_key: String,
}

impl Default for DefaultRoute {
    fn default() -> Self {
        Self {
            api_key: "default".into(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reasoning_routes: BTreeMap<String, ReasoningRoute>,
    pub from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningRoute {
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retry {
    pub max_retries: u32,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    /// When nonzero, recover by elapsed time instead of attempt count.
    /// max_retries=0 still disables retries. Leave margin below the client's idle timeout.
    pub recovery_timeout_ms: u64,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Logging {
    pub enabled: bool,
    /// Relative paths resolve beside the proxy config, never against the current directory.
    pub database: Option<String>,
    pub queue_capacity: usize,
    pub batch_size: usize,
    pub flush_interval_ms: u64,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            enabled: true,
            database: None,
            queue_capacity: 65_536,
            batch_size: 512,
            flush_interval_ms: 100,
        }
    }
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            max_retries: 5,
            initial_delay_ms: 500,
            max_delay_ms: 30_000,
            recovery_timeout_ms: 290_000,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            fallbacks: BTreeMap::new(),
            gemini: None,
            credential_cache_seconds: 2400,
            ssh_hosts: Vec::new(),
            mode: Mode::Standalone,
            connection: None,
            listen: "127.0.0.1:8080".parse().unwrap(),
            upstream_url: "https://api.openai.com".into(),
            api_keys: BTreeMap::new(),
            default: DefaultRoute {
                api_key: "default".into(),
            },
            aliases: Vec::new(),
            retry: Retry::default(),
            logging: Logging::default(),
            skip_blocked_security_work: false,
            ip_version: IpVersion::Auto,
        }
    }
}

#[cfg(test)]
impl Config {
    /// Synthetic routing fixture; production startup uses an empty configuration.
    pub fn test_fixture() -> Self {
        Self {
            api_keys: BTreeMap::from([
                ("default".into(), "something".into()),
                ("primary".into(), "replace-with-primary-api-key".into()),
            ]),
            default: DefaultRoute {
                api_key: "default".into(),
            },
            aliases: vec![
                Alias {
                    reasoning_routes: BTreeMap::new(),
                    from: "model-primary".into(),
                    to: None,
                    reasoning: None,
                    api_key: Some("primary".into()),
                },
                Alias {
                    reasoning_routes: BTreeMap::new(),
                    from: "gpt-4.1".into(),
                    to: Some("gpt-4.1-mini".into()),
                    reasoning: Some("low".into()),
                    api_key: None,
                },
            ],
            ..Self::default()
        }
    }
}

impl Config {
    pub fn local_address(&self) -> SocketAddr {
        if self.listen.ip().is_unspecified() {
            let ip = if self.listen.is_ipv6() {
                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
            } else {
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            };
            SocketAddr::new(ip, self.listen.port())
        } else {
            self.listen
        }
    }
    pub fn effective(&self) -> Self {
        let mut config = self.clone();
        if self.mode == Mode::Client
            && let Some(connection) = &self.connection
        {
            config.upstream_url = connection.url.trim_end_matches('/').into();
            config.api_keys = BTreeMap::from([("host".into(), connection.api_key.clone())]);
            config.default.api_key = "host".into();
            config.aliases.clear();
            config.fallbacks.clear();
            config.retry.max_retries = 0; // The host owns upstream retries.
        }
        config
    }
    pub fn validate(&self) -> Result<()> {
        if !(128..=1_048_576).contains(&self.logging.queue_capacity)
            || !(1..=4096).contains(&self.logging.batch_size)
            || self.logging.batch_size > self.logging.queue_capacity
            || !(10..=5000).contains(&self.logging.flush_interval_ms)
            || self
                .logging
                .database
                .as_ref()
                .is_some_and(|path| path.trim().is_empty())
        {
            bail!(
                "Logging limits: queue_capacity 128..1048576, batch_size 1..4096 and <= queue_capacity, flush_interval_ms 10..5000; database must be a nonempty path"
            );
        }
        if self.mode == Mode::Client {
            if !self.listen.ip().is_loopback() {
                bail!("Client mode requires a loopback listening address");
            }
            let connection = self
                .connection
                .as_ref()
                .context("Client mode requires connection.url and connection.api_key")?;
            validate_root_url(&connection.url)?;
            if connection.api_key.is_empty()
                || reqwest::header::HeaderValue::from_str(&format!("Bearer {}", connection.api_key))
                    .is_err()
            {
                bail!("Invalid host access key");
            }
            if !self.api_keys.is_empty()
                || !self.aliases.is_empty()
                || !self.fallbacks.is_empty()
                || self.gemini.is_some()
            {
                bail!(
                    "Client config must not contain upstream API keys, aliases or fallback rules"
                );
            }
        } else if self.connection.is_some() {
            bail!("Only client mode accepts connection");
        }
        let effective = self.effective();
        self.validate_inventory()?;
        effective.validate_routing()
    }
    fn validate_inventory(&self) -> Result<()> {
        let mut hosts = HashSet::new();
        for host in &self.ssh_hosts {
            let name = host.host();
            if name.is_empty()
                || name.starts_with('-')
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"@._:-[]".contains(&b))
                || !hosts.insert(name)
            {
                bail!("ssh_hosts must contain unique SSH aliases or user@host destinations");
            }
            if let Some(settings) = host.settings() {
                if settings.prepare.iter().any(|command| {
                    command.is_empty()
                        || command[0].is_empty()
                        || command.iter().any(|arg| arg.contains('\0'))
                }) {
                    bail!("Remote prepare commands must be nonempty argv arrays");
                }
                if settings.listen.is_some_and(|address| {
                    address.port() == 0
                        || (settings.mode != Mode::Host && !address.ip().is_loopback())
                }) {
                    bail!("Remote listen must be a loopback address with a nonzero port");
                }
                if settings
                    .codex_home
                    .as_ref()
                    .is_some_and(|p| !p.starts_with('/') || p.contains(['\n', '\r', '\0']))
                {
                    bail!("codex_home must be an absolute remote path without control characters");
                }
                if settings.model.as_ref().is_some_and(|m| m.trim().is_empty()) {
                    bail!("Remote model must be nonempty");
                }
            }
        }
        for host in &self.ssh_hosts {
            if let Some(settings) = host.settings() {
                match settings.mode {
                    Mode::Client => {
                        let via = settings
                            .via
                            .as_ref()
                            .context("Client SSH entry requires via")?;
                        let target = self
                            .ssh_hosts
                            .iter()
                            .find(|h| h.host() == via)
                            .context("Client via references unknown SSH host")?;
                        if target.settings().is_none_or(|s| s.mode != Mode::Host) {
                            bail!("Client via must reference a host-mode SSH entry");
                        }
                        if settings.url.is_some() {
                            bail!("Set url on the host, not the client");
                        }
                    }
                    Mode::Host => {
                        validate_root_url(
                            settings
                                .url
                                .as_ref()
                                .context("Host SSH entry requires its client-reachable url")?,
                        )?;
                        if settings.via.is_some() {
                            bail!("Host entries cannot have via");
                        }
                    }
                    Mode::Standalone => {
                        if settings.via.is_some() || settings.url.is_some() {
                            bail!("Standalone SSH entries do not use via or url");
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn validate_routing(&self) -> Result<()> {
        hey_proxy::fallback::validate(&self.fallbacks)?;
        if !(1..=86400).contains(&self.credential_cache_seconds) {
            bail!("OpenAI credential_cache_seconds must be 1..86400");
        }
        if let Some(gemini) = &self.gemini {
            gemini.validate()?;
        }
        let url = reqwest::Url::parse(&self.upstream_url).context("Invalid upstream_url")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("upstream_url must be an HTTP(S) URL without credentials, query, or fragment");
        }
        for key in self.api_keys.values() {
            if hey_proxy::credentials::validate_source(key).is_err() {
                bail!("api_keys contains an empty or invalid HTTP header value");
            }
        }
        if !self.api_keys.contains_key(&self.default.api_key)
            && !(self.api_keys.is_empty() && self.default.api_key == "default")
        {
            bail!("default.api_key references an unknown project");
        }
        let mut seen = HashSet::new();
        for alias in &self.aliases {
            for (effort, route) in &alias.reasoning_routes {
                if effort.trim().is_empty() || route.to.trim().is_empty() {
                    bail!("Reasoning route effort and model must be nonempty");
                }
                if route
                    .api_key
                    .as_ref()
                    .is_some_and(|key| !self.api_keys.contains_key(key))
                {
                    bail!("Reasoning route references an unknown API key project");
                }
            }
            if alias.from.is_empty() || !seen.insert(&alias.from) {
                bail!("Alias source names must be nonempty and unique");
            }
            if alias.to.as_ref().is_some_and(|s| s.trim().is_empty())
                || alias
                    .reasoning
                    .as_ref()
                    .is_some_and(|s| s.trim().is_empty())
            {
                bail!("Alias model and reasoning values must be nonempty");
            }
            if alias
                .api_key
                .as_ref()
                .is_some_and(|key| !self.api_keys.contains_key(key))
            {
                bail!("Alias references an unknown API key project");
            }
        }
        if self.retry.max_retries > 20
            || self.retry.max_delay_ms > 300_000
            || self.retry.initial_delay_ms > self.retry.max_delay_ms
            || self.retry.recovery_timeout_ms > 3_600_000
        {
            bail!(
                "Retry limits: max_retries <= 20, initial_delay_ms <= max_delay_ms <= 300000, recovery_timeout_ms <= 3600000"
            );
        }
        Ok(())
    }
}

pub fn validate_root_url(value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value).context("Invalid proxy URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!(
            "Proxy URL must be an HTTP(S) service root without credentials, path, query or fragment"
        );
    }
    Ok(())
}

pub fn load_or_create(path: &Path) -> Result<Config> {
    if !path.exists() {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(parent)
                .context("Cannot create config directory")?;
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(mut file) => {
                let mut content = serde_json::to_vec_pretty(&serde_json::json!({
                    "listen":"127.0.0.1:8080",
                    "providers":{"openai":{"api_keys":{}}},
                    "aliases":[], "fallbacks":{}
                }))?;
                content.push(b'\n');
                file.write_all(&content).context("Cannot write config")?;
                file.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).context("Cannot create config"),
        }
    }
    load(path)
}

/// Reads and validates an existing config without creating one.
pub fn load(path: &Path) -> Result<Config> {
    let content = fs::read(path).context("Cannot read config")?;
    let config: Config = serde_json::from_slice(&content).map_err(|e| {
        anyhow::anyhow!(
            "Invalid config JSON at line {}, column {}",
            e.line(),
            e.column()
        )
    })?;
    config.validate()?;
    Ok(config)
}

/// Cheap change detector for hot reload: modification time and size, or `None` if unreadable.
pub type Fingerprint = Option<(Option<SystemTime>, u64)>;

pub fn fingerprint(path: &Path) -> Fingerprint {
    fs::metadata(path)
        .ok()
        .map(|meta| (meta.modified().ok(), meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn creates_private_config_and_preserves_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.json");
        let mut config = load_or_create(&path).unwrap();
        config.api_keys.insert("default".into(), "edited".into());
        fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(load_or_create(&path).unwrap().api_keys["default"], "edited");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn provider_config_and_legacy_config_have_the_same_canonical_shape() {
        let config = Config::test_fixture();
        let canonical = serde_json::to_value(&config).unwrap();
        assert!(canonical.get("api_keys").is_none());
        assert!(canonical.get("upstream_url").is_none());
        assert_eq!(
            canonical["providers"]["openai"]["api_keys"]["default"],
            "something"
        );
        assert!(canonical["providers"].get("gemini").is_none());
        let roundtrip: Config = serde_json::from_value(canonical.clone()).unwrap();
        roundtrip.validate().unwrap();
        assert_eq!(roundtrip.api_keys, config.api_keys);
        let legacy = serde_json::json!({"listen":"127.0.0.1:8080","api_keys":{"default":"synthetic"},"aliases":[],"gemini":{"auth":"adc","upstream_url":"https://aiplatform.googleapis.com/v1/projects/example-project/locations/global/publishers/google"}});
        let converted: Config = serde_json::from_value(legacy).unwrap();
        converted.validate().unwrap();
        let converted = serde_json::to_value(converted).unwrap();
        assert_eq!(converted["providers"]["gemini"]["auth"], "adc");
        let mut mixed = canonical;
        mixed["api_keys"] = serde_json::json!({"default":"conflict"});
        assert!(serde_json::from_value::<Config>(mixed).is_err());
        let unsupported = serde_json::json!({"listen":"127.0.0.1:8080","providers":{"unknown":{}}});
        assert!(serde_json::from_value::<Config>(unsupported).is_err());
    }
    #[test]
    fn rejects_unknown_projects_duplicates_and_unbounded_retry() {
        let mut config = Config::test_fixture();
        config.default.api_key = "missing".into();
        assert!(config.validate().is_err());
        config = Config::test_fixture();
        config.aliases.push(config.aliases[0].clone());
        assert!(config.validate().is_err());
        config = Config::test_fixture();
        config.retry.max_retries = 100;
        assert!(config.validate().is_err());
        config = Config::test_fixture();
        assert_eq!(config.retry.recovery_timeout_ms, 290_000);
        config.retry.recovery_timeout_ms = 3_600_001;
        assert!(config.validate().is_err());
        config.retry.recovery_timeout_ms = 0;
        assert!(config.validate().is_ok());
    }
}
