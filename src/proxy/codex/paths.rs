//! Resolve the configured Codex credential path once without filesystem work on hot requests.
use super::*;

struct Entry {
    source: Option<PathBuf>,
    configured: Option<PathBuf>,
    resolved: PathBuf,
}

#[derive(Default)]
pub(super) struct Cache(tokio::sync::Mutex<Option<Entry>>);
impl Cache {
    pub async fn resolve(
        &self,
        provider: &ProviderConfig,
        source: Option<&Path>,
    ) -> Result<PathBuf> {
        let mut cache = self.0.lock().await;
        if let Some(entry) = cache.as_ref()
            && entry.source.as_deref() == source
            && entry.configured == provider.credentials_file
        {
            return Ok(entry.resolved.clone());
        }
        let source = source.map(Path::to_owned);
        let (owned_provider, owned_source) = (provider.clone(), source.clone());
        let resolved = tokio::task::spawn_blocking(move || {
            owned_provider.credentials_path(owned_source.as_deref())
        })
        .await
        .context("Codex credential path resolution failed")??;
        *cache = Some(Entry {
            source,
            configured: provider.credentials_file.clone(),
            resolved: resolved.clone(),
        });
        Ok(resolved)
    }
}
