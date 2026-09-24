//! Credential sources are evaluated only from trusted configuration, never from
//! request bodies. Shell and 1Password sources share a bounded asynchronous single-flight cache.
use anyhow::{Result, anyhow, bail};
use reqwest::header::{HeaderMap, HeaderValue};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex, OnceCell},
    time::Instant,
};

// Each trusted shell command owns a process group. Cancellation/timeout must
// also terminate helpers that inherited its stdout, not only /bin/sh itself.
#[cfg(unix)]
struct CommandGroup(u32);
#[cfg(unix)]
impl Drop for CommandGroup {
    fn drop(&mut self) {
        // SAFETY: this is the group created for our child, never our own group.
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}

#[derive(Default)]
struct ShellState {
    value: Option<(Instant, HeaderValue)>,
    failed: Option<(Instant, String)>,
}
type ShellCell = Arc<Mutex<ShellState>>;
#[derive(Default)]
pub struct CredentialResolver {
    shell: Mutex<HashMap<String, ShellCell>>,
    adc: OnceCell<google_cloud_auth::credentials::Credentials>,
    op_cli: Option<std::path::PathBuf>,
}
pub fn validate_source(source: &str) -> Result<()> {
    if let Some(command) = source.strip_prefix("sh://") {
        if command.trim().is_empty() || command.contains('\0') {
            bail!("sh:// requires a nonempty shell command without NUL");
        }
    } else if let Some(reference) = source.strip_prefix("op://") {
        let parts: Vec<_> = reference.split('/').collect();
        if !(3..=4).contains(&parts.len())
            || parts
                .iter()
                .any(|p| p.trim().is_empty() || matches!(*p, "." | ".."))
            || reference.chars().any(char::is_control)
            || reference.contains(['?', '#'])
            || reference.len() > 4096
        {
            bail!(
                "op:// requires vault/item/field or vault/item/section/field without query parameters"
            );
        }
    } else if source.contains("://") {
        bail!(
            "Unsupported credential source scheme; supported sources are literal keys, sh:// and op://"
        );
    } else if source.trim().is_empty() || HeaderValue::from_str(source).is_err() {
        bail!("Invalid literal credential");
    }
    Ok(())
}
impl CredentialResolver {
    /// Select a trusted CLI executable when it is not installed on the service PATH.
    /// Arguments are always passed directly; an op:// reference is never shell code.
    pub fn with_op_cli(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            op_cli: Some(path.into()),
            ..Self::default()
        }
    }
    fn op_command(&self) -> tokio::process::Command {
        let program = self
            .op_cli
            .clone()
            .or_else(|| std::env::var_os("HEY_PROXY_OP_CLI").map(Into::into))
            .unwrap_or_else(|| {
                // launchd/systemd often have a smaller PATH than interactive shells.
                let mut paths = vec![
                    std::path::PathBuf::from("/opt/homebrew/bin/op"),
                    "/usr/local/bin/op".into(),
                ];
                if let Some(home) = std::env::var_os("HOME") {
                    let home = std::path::PathBuf::from(home);
                    // Optional service-account adapter obtains its bootstrap token
                    // from the OS credential store, including under supervisors.
                    paths.insert(0, home.join(".hey-proxy/op-agent"));
                    paths.push(home.join(".local/bin/op"));
                }
                paths
                    .into_iter()
                    .find(|p| p.is_file())
                    .unwrap_or_else(|| "op".into())
            });
        tokio::process::Command::new(program)
    }
    /// Literal keys never allocate cache entries. Commands are executed once per
    /// source/TTL, even with concurrent callers. Requests already waiting for a
    /// failed execution share its error; later requests can immediately retry.
    pub async fn resolve(&self, source: &str, ttl: Duration) -> Result<HeaderValue> {
        let started = Instant::now();
        validate_source(source)?;
        let shell = source.strip_prefix("sh://");
        let onepassword = source.starts_with("op://");
        if shell.is_none() && !onepassword {
            let mut value = HeaderValue::from_str(source)?;
            value.set_sensitive(true);
            return Ok(value);
        }
        let cell = {
            let mut entries = self.shell.lock().await;
            // Never evict active cells: doing so breaks same-source single-flight.
            if !entries.contains_key(source) && entries.len() >= 128 {
                let idle = entries
                    .iter()
                    .find(|(_, cell)| Arc::strong_count(cell) == 1)
                    .map(|(key, _)| key.clone());
                if let Some(key) = idle {
                    entries.remove(&key);
                } else {
                    bail!(
                        "Credential command cache is busy; retry after an active source completes"
                    );
                }
            }
            entries.entry(source.to_owned()).or_default().clone()
        };
        let mut cached = cell.lock().await;
        if let Some((when, value)) = &cached.value
            && when.elapsed() < ttl
        {
            return Ok(value.clone());
        }
        if let Some((finished, message)) = &cached.failed
            && *finished > started
        {
            return Err(anyhow!(message.clone()));
        }
        let execute = async {
            let mut process = if let Some(command) = shell {
                let mut process = tokio::process::Command::new("/bin/sh");
                process.args(["-c", command]);
                process
            } else {
                let mut process = self.op_command();
                process.args(["read", "--no-newline", "--", source]);
                process
            };
            #[cfg(unix)]
            process.process_group(0);
            let mut child = process
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(|_| anyhow!("Could not start credential command"))?;
            #[cfg(unix)]
            let _group = CommandGroup(
                child
                    .id()
                    .ok_or_else(|| anyhow!("Credential command unavailable"))?,
            );
            let mut bytes = Vec::new();
            child
                .stdout
                .take()
                .ok_or_else(|| anyhow!("Credential command stdout unavailable"))?
                .take(16 * 1024 + 1)
                .read_to_end(&mut bytes)
                .await?;
            if bytes.len() > 16 * 1024 {
                bail!("Credential command output exceeds 16 KiB");
            }
            if !child.wait().await?.success() {
                bail!("Credential command failed");
            }
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| anyhow!("Credential command output must be UTF-8"))?
                .trim();
            if text.is_empty() {
                bail!("Credential command returned an empty credential");
            }
            let mut value = HeaderValue::from_str(text)
                .map_err(|_| anyhow!("Credential command output is not a valid HTTP header"))?;
            value.set_sensitive(true);
            Ok(value)
        };
        let value = tokio::time::timeout(Duration::from_secs(30), execute)
            .await
            .map_err(|_| anyhow!("Credential command timed out"))
            .and_then(|result| result);
        let value = match value {
            Ok(value) => value,
            Err(error) => {
                cached.failed = Some((Instant::now(), error.to_string()));
                return Err(error);
            }
        };
        cached.value = Some((Instant::now(), value.clone()));
        cached.failed = None;
        Ok(value)
    }
    /// The official Google Rust SDK handles ADC discovery, federation,
    /// impersonation, refresh and token caching. No gcloud subprocess per request.
    pub async fn adc_headers(&self) -> Result<HeaderMap> {
        use google_cloud_auth::credentials::{Builder, CacheableResource};
        let credentials = self
            .adc
            .get_or_try_init(|| async {
                // The Google SDK uses reqwest's rustls-no-provider transport.
                // Keep an existing process provider, or install our ring provider
                // before the SDK constructs its HTTPS client.
                let _ = rustls::crypto::ring::default_provider().install_default();
                tokio::task::spawn_blocking(|| {
                    Builder::default()
                        .with_scopes(["https://www.googleapis.com/auth/cloud-platform"])
                        .build()
                })
                .await
                .map_err(|_| anyhow!("ADC initialization failed"))?
                .map_err(|_| {
                    anyhow!(
                        "ADC credentials unavailable; configure Application Default Credentials"
                    )
                })
            })
            .await?;
        match tokio::time::timeout(
            Duration::from_secs(30),
            credentials.headers(Default::default()),
        )
        .await
        .map_err(|_| anyhow!("ADC token acquisition timed out"))?
        .map_err(|_| anyhow!("ADC token acquisition failed"))?
        {
            CacheableResource::New { mut data, .. } => {
                for (_, value) in data.iter_mut() {
                    value.set_sensitive(true);
                }
                Ok(data)
            }
            CacheableResource::NotModified => bail!("ADC returned no credential headers"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn saturated_cache_preserves_active_cells_and_rejects_new_sources() {
        let resolver = CredentialResolver::default();
        let mut active = Vec::new();
        {
            let mut entries = resolver.shell.lock().await;
            for i in 0..128 {
                let cell = Arc::new(Mutex::new(ShellState {
                    value: Some((Instant::now(), HeaderValue::from_static("cached"))),
                    failed: None,
                }));
                entries.insert(format!("sh://printf {i}"), cell.clone());
                active.push(cell);
            }
        }
        assert!(
            resolver
                .resolve("sh://printf new", Duration::from_secs(60))
                .await
                .unwrap_err()
                .to_string()
                .contains("busy")
        );
        assert_eq!(
            resolver
                .resolve("sh://printf 0", Duration::from_secs(60))
                .await
                .unwrap(),
            "cached"
        );
        assert!(Arc::ptr_eq(
            &active[0],
            resolver.shell.lock().await.get("sh://printf 0").unwrap()
        ));
        active.pop(); // Exactly one idle entry is now evictable.
        assert_eq!(
            resolver
                .resolve("sh://printf new", Duration::from_secs(60))
                .await
                .unwrap(),
            "new"
        );
        let entries = resolver.shell.lock().await;
        assert_eq!(entries.len(), 128);
        assert!(!entries.contains_key("sh://printf 127"));
        assert!(Arc::ptr_eq(
            &active[0],
            entries.get("sh://printf 0").unwrap()
        ));
    }
}
