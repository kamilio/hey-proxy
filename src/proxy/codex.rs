//! Codex subscription usage adapter; never alters OpenAI/Chat/Responses proxy routing.
mod paths;
#[cfg(test)]
mod tests;
mod usage;
use super::*;
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;
pub(super) use usage::{reading, usage};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProviderConfig {
    pub upstream_url: String,
    pub issuer_url: String,
    /// Proxy-owned encrypted OAuth store. Relative paths resolve beside the config.
    pub credentials_file: Option<PathBuf>,
    pub usage_cache_seconds: u64,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            upstream_url: "https://chatgpt.com".into(),
            issuer_url: crate::codex_auth::DEFAULT_ISSUER.into(),
            credentials_file: None,
            usage_cache_seconds: 60,
        }
    }
}

impl ProviderConfig {
    pub(crate) fn validate(&self, _mode: Mode) -> Result<()> {
        for (label, raw, allow_backend_api) in [
            ("Codex upstream", &self.upstream_url, true),
            ("Codex issuer", &self.issuer_url, false),
        ] {
            let url = url::Url::parse(raw).with_context(|| format!("Invalid {label} URL"))?;
            let loopback = url.host_str().is_some_and(|h| {
                h == "localhost"
                    || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
                    || h == "[::1]"
            });
            ensure!(
                url.scheme() == "https" || url.scheme() == "http" && loopback,
                "{label} requires HTTPS (HTTP is allowed only on loopback)"
            );
            let valid_path = url.path() == "/"
                || (allow_backend_api && matches!(url.path(), "/backend-api" | "/backend-api/"));
            ensure!(
                url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && valid_path,
                "{label} must be a root URL without credentials, query or fragment"
            );
        }
        ensure!(
            (30..=3600).contains(&self.usage_cache_seconds),
            "Codex usage_cache_seconds must be 30..3600"
        );
        if let Some(path) = &self.credentials_file {
            ensure!(
                path.extension().is_some_and(|e| e == "json"),
                "Codex credentials_file must end in .json"
            );
        }
        Ok(())
    }

    pub(crate) fn token_url(&self) -> String {
        format!("{}/oauth/token", self.issuer_url.trim_end_matches('/'))
    }

    pub(crate) fn usage_url(&self) -> String {
        let base = self.upstream_url.trim_end_matches('/');
        if base.ends_with("/backend-api") {
            format!("{base}/wham/usage")
        } else {
            format!("{base}/backend-api/wham/usage")
        }
    }

    pub(crate) fn credentials_path(&self, config: Option<&Path>) -> Result<PathBuf> {
        let path = match (&self.credentials_file, config) {
            (Some(path), _) if path.is_absolute() => path.clone(),
            (Some(path), Some(config)) => config.parent().unwrap_or(Path::new(".")).join(path),
            (None, Some(config)) => config.with_extension("codex.json"),
            _ => {
                anyhow::bail!("Codex requires a file-backed config or an absolute credentials_file")
            }
        };
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        if let Some(config) = config {
            let absolute = if config.is_absolute() {
                config.to_owned()
            } else {
                std::env::current_dir()?.join(config)
            };
            let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
            ensure!(
                canonical(&path) != canonical(&absolute),
                "Codex credentials_file must differ from the proxy config"
            );
        }
        Ok(path)
    }
}

#[derive(Default)]
pub(super) struct CodexState {
    tokens: crate::codex_auth::TokenManager,
    paths: paths::Cache,
    usage: tokio::sync::Mutex<usage::Cache>,
}

pub(super) fn is_enabled(config: &Config, source: Option<&Path>) -> bool {
    config.codex.is_some()
        || source
            .map(|p| p.with_extension("codex.json").exists())
            .unwrap_or(false)
}
