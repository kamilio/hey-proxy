//! A bounded token cache with background validation of the proxy-owned files.
use super::*;
use std::sync::Mutex as SyncMutex;

const REFRESH_AFTER: Duration = Duration::from_millis(250);
const MAX_AGE: Duration = Duration::from_secs(2);

struct Entry {
    path: PathBuf,
    checked: Instant,
    tokens: std::result::Result<Arc<Tokens>, String>,
    checking: bool,
}
#[derive(Default)]
struct State {
    version: u64,
    entry: Option<Entry>,
}
#[derive(Default)]
pub(super) struct Cache(SyncMutex<State>);
impl Cache {
    pub fn store(&self, path: PathBuf, tokens: &Tokens) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.version = state.version.wrapping_add(1);
        state.entry = Some(Entry {
            path,
            checked: Instant::now(),
            tokens: Ok(Arc::new(tokens.clone())),
            checking: false,
        });
    }
    pub fn failure(&self, path: PathBuf, error: &anyhow::Error) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.version = state.version.wrapping_add(1);
        state.entry = Some(Entry {
            path,
            checked: Instant::now(),
            tokens: Err(error.to_string()),
            checking: false,
        });
    }
    pub fn get(
        self: &Arc<Self>,
        path: &Path,
        rejected: Option<&str>,
    ) -> Option<Result<Credentials>> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let version = state.version;
        let entry = state.entry.as_mut().filter(|entry| entry.path == path)?;
        if rejected.is_some_and(|old| {
            entry
                .tokens
                .as_ref()
                .is_ok_and(|tokens| tokens.access_token == old)
        }) {
            state.version = state.version.wrapping_add(1);
            state.entry = None;
            return None;
        }
        if entry.checked.elapsed() >= MAX_AGE {
            return None;
        }
        if entry.checked.elapsed() >= REFRESH_AFTER && !entry.checking {
            entry.checking = true;
            let cache = Arc::downgrade(self);
            let path = path.to_owned();
            tokio::task::spawn_blocking(move || {
                let tokens = load(&path).map(Arc::new).map_err(|error| error.to_string());
                if let Some(cache) = cache.upgrade() {
                    cache.checked(version, path, tokens);
                }
            });
        }
        match &entry.tokens {
            Ok(tokens) if tokens.expires_at > now().saturating_add(60) => Some(Ok(Credentials {
                access_token: tokens.access_token.clone(),
                account_id: tokens.account_id.clone(),
            })),
            Ok(_) => None,
            Err(error) => Some(Err(anyhow::anyhow!(error.clone()))),
        }
    }
    fn checked(
        &self,
        version: u64,
        path: PathBuf,
        tokens: std::result::Result<Arc<Tokens>, String>,
    ) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.version != version || state.entry.as_ref().is_none_or(|entry| entry.path != path) {
            return;
        }
        state.version = state.version.wrapping_add(1);
        state.entry = Some(Entry {
            path,
            checked: Instant::now(),
            tokens,
            checking: false,
        });
    }
}
