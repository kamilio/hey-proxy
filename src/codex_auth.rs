//! Proxy-owned Codex (ChatGPT) OAuth credentials. Never mutates Codex CLI's own auth.json.
mod cache;
use aes_gcm_siv::{
    Aes256GcmSiv, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use anyhow::{Context, Result, bail, ensure};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use rand::TryRngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const DEFAULT_ISSUER: &str = "https://auth.openai.com";
const DEFAULT_PORT: u16 = 1455;
const FALLBACK_PORT: u16 = 1457;
const SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
const AAD: &[u8] = b"hey-proxy Codex OAuth v1";
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub access_token: String,
    pub account_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LoginOptions {
    pub no_browser: bool,
    pub device_code: bool,
    pub import_codex_home: bool,
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Cannot generate OAuth randomness"))?;
    Ok(bytes)
}

fn private_open(path: &Path, create: bool) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let file = options
            .open(path)
            .context("Cannot open private Codex credential file; run hey-proxy codex-login")?;
        ensure!(
            file.metadata()?.is_file() && file.metadata()?.permissions().mode() & 0o077 == 0,
            "Codex credential files must be regular files with private permissions (chmod 600)"
        );
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        Ok(options.open(path)?)
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("Codex credentials require a parent directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map_err(|_| anyhow::anyhow!("Cannot save Codex credentials"))?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn key(path: &Path, create: bool) -> Result<[u8; 32]> {
    let key_path = path.with_extension("key");
    if create && !key_path.exists() {
        let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        file.write_all(&random::<32>()?)?;
        file.as_file().sync_all()?;
        if let Err(error) = file.persist_noclobber(&key_path) {
            ensure!(
                error.error.kind() == std::io::ErrorKind::AlreadyExists,
                "Cannot save Codex credential key"
            );
        }
        #[cfg(unix)]
        fs::File::open(path.parent().unwrap())?.sync_all()?;
    }
    let mut file = private_open(&key_path, false)?;
    ensure!(
        file.metadata()?.len() == 32,
        "Invalid Codex credential encryption key"
    );
    let mut bytes = [0; 32];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn load(path: &Path) -> Result<Tokens> {
    let mut data = Vec::new();
    private_open(path, false)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= MAX_BYTES,
        "Codex credential file is too large"
    );
    let envelope: Value = serde_json::from_slice(&data)
        .map_err(|_| anyhow::anyhow!("Invalid Codex credential file"))?;
    let encoded = envelope["encrypted"]
        .as_str()
        .and_then(|s| s.strip_prefix("v1:"))
        .context("Invalid Codex credential envelope")?;
    let bytes = STANDARD
        .decode(encoded)
        .ok()
        .filter(|b| b.len() >= 28)
        .context("Invalid Codex credential encoding")?;
    let cipher = Aes256GcmSiv::new((&key(path, false)?).into());
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&bytes[..12]),
            Payload {
                msg: &bytes[12..],
                aad: AAD,
            },
        )
        .map_err(|_| {
            anyhow::anyhow!("Cannot decrypt Codex credentials; restore the matching key file")
        })?;
    let mut tokens: Tokens = serde_json::from_slice(&plaintext)
        .map_err(|_| anyhow::anyhow!("Invalid Codex credentials"))?;
    tokens.account_id = tokens.account_id.filter(|id| is_header_token(id));
    ensure!(
        is_oauth_token(&tokens.access_token) && !tokens.refresh_token.is_empty(),
        "Invalid Codex OAuth tokens; run hey-proxy codex-login"
    );
    Ok(tokens)
}

pub(crate) fn save(path: &Path, tokens: &Tokens) -> Result<()> {
    let cipher = Aes256GcmSiv::new((&key(path, true)?).into());
    let nonce = random::<12>()?;
    let encrypted = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &serde_json::to_vec(tokens)?,
                aad: AAD,
            },
        )
        .map_err(|_| anyhow::anyhow!("Cannot encrypt Codex credentials"))?;
    let mut bytes = nonce.to_vec();
    bytes.extend(encrypted);
    atomic_write(
        path,
        &serde_json::to_vec(&json!({"encrypted":format!("v1:{}", STANDARD.encode(bytes))}))?,
    )
}

pub(crate) fn is_oauth_token(token: &str) -> bool {
    !token.trim().is_empty()
        && token.len() >= 8
        && reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).is_ok()
}

fn is_header_token(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 256
        && reqwest::header::HeaderValue::from_str(value).is_ok()
}

pub(crate) fn jwt_metadata(token: &str) -> (Option<u64>, Option<String>) {
    let mut parts = token.split('.');
    let (Some(_), Some(payload), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
        return (None, None);
    };
    let decoded = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .or_else(|_| STANDARD.decode(payload));
    let Ok(bytes) = decoded else {
        return (None, None);
    };
    let Ok(claims) = serde_json::from_slice::<Value>(&bytes) else {
        return (None, None);
    };
    let exp = claims["exp"].as_u64();
    let account_id = claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .or_else(|| claims["chatgpt_account_id"].as_str())
        .or_else(|| claims["account_id"].as_str())
        .filter(|id| is_header_token(id))
        .map(str::to_owned);
    (exp, account_id)
}

async fn lock(path: &Path) -> Result<fs::File> {
    let path = path.with_extension("lock");
    let file = tokio::task::spawn_blocking(move || private_open(&path, true)).await??;
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(_) => bail!("Codex credential refresh is busy; retry shortly"),
        }
    }
}

fn parse_token_response(bytes: &[u8], previous: Option<&Tokens>) -> Result<Tokens> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("Invalid Codex OAuth response"))?;
    let access = value["access_token"]
        .as_str()
        .filter(|s| is_oauth_token(s))
        .context("Codex OAuth did not return an access token")?;
    let refresh = value["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| previous.map(|p| p.refresh_token.as_str()))
        .context("Codex OAuth did not return a refresh token")?;
    let (access_exp, access_account) = jwt_metadata(access);
    let (id_exp, id_account) = value["id_token"]
        .as_str()
        .map(jwt_metadata)
        .unwrap_or((None, None));
    let current = now();
    let expires_at = if let Some(expires_in) = value["expires_in"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= 366 * 86400)
    {
        current.saturating_add(expires_in)
    } else if let Some(exp) = access_exp
        .or(id_exp)
        .filter(|exp| *exp > current && *exp <= current.saturating_add(366 * 86400))
    {
        exp
    } else {
        current.saturating_add(3600)
    };
    let account_id = id_account
        .or(access_account)
        .or_else(|| {
            value["account_id"]
                .as_str()
                .filter(|id| is_header_token(id))
                .map(str::to_owned)
        })
        .or_else(|| previous.and_then(|p| p.account_id.clone()));
    Ok(Tokens {
        access_token: access.into(),
        refresh_token: refresh.into(),
        expires_at,
        account_id,
    })
}

async fn read_oauth_response(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let status = response.status();
    if !status.is_success() {
        if matches!(status.as_u16(), 400 | 401 | 403) {
            bail!(
                "Codex OAuth authorization rejected (HTTP {}); run hey-proxy codex-login",
                status.as_u16()
            );
        }
        bail!(
            "Codex OAuth temporarily unavailable (HTTP {}); retry shortly",
            status.as_u16()
        );
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("Cannot read Codex OAuth response"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= MAX_BYTES as usize,
            "Codex OAuth response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn exchange_code(
    client: &reqwest::Client,
    endpoint: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<Tokens> {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    form.extend_pairs([
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ]);
    let response = client
        .post(endpoint)
        .timeout(Duration::from_secs(30))
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form.finish())
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Cannot reach Codex OAuth token endpoint"))?;
    let bytes = read_oauth_response(response).await?;
    parse_token_response(&bytes, None)
}

async fn exchange_refresh(
    client: &reqwest::Client,
    endpoint: &str,
    previous: &Tokens,
) -> Result<Tokens> {
    let body = json!({
        "client_id": CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": previous.refresh_token,
        "scope": "openid profile email",
    });
    let response = client
        .post(endpoint)
        .timeout(Duration::from_secs(30))
        .header("accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Cannot reach Codex OAuth token endpoint"))?;
    let bytes = read_oauth_response(response).await?;
    parse_token_response(&bytes, Some(previous))
}

#[derive(Default, Clone)]
pub(crate) struct TokenManager {
    backoff: Arc<Mutex<Option<(PathBuf, Instant)>>>,
    cache: Arc<cache::Cache>,
}

impl TokenManager {
    pub async fn token(
        &self,
        path: &Path,
        client: &reqwest::Client,
        endpoint: &str,
    ) -> Result<String> {
        Ok(self.credentials(path, client, endpoint).await?.access_token)
    }

    pub async fn credentials(
        &self,
        path: &Path,
        client: &reqwest::Client,
        endpoint: &str,
    ) -> Result<Credentials> {
        self.obtain(path, client, None, endpoint).await
    }

    pub async fn rejected(
        &self,
        path: &Path,
        client: &reqwest::Client,
        endpoint: &str,
        token: String,
    ) -> Result<Credentials> {
        self.obtain(path, client, Some(token), endpoint).await
    }

    async fn obtain(
        &self,
        path: &Path,
        client: &reqwest::Client,
        rejected: Option<String>,
        endpoint: &str,
    ) -> Result<Credentials> {
        if let Some(cached) = self.cache.get(path, rejected.as_deref()) {
            return cached;
        }
        let path = path.to_owned();
        let client = client.clone();
        let endpoint = endpoint.to_owned();
        let backoff = self.backoff.clone();
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let mut gate = backoff.lock().await;
            if let Some(cached) = cache.get(&path, rejected.as_deref()) {
                return cached;
            }
            let disk_path = path.clone();
            let tokens = match tokio::task::spawn_blocking(move || load(&disk_path)).await? {
                Ok(tokens) => tokens,
                Err(error) => {
                    cache.failure(path, &error);
                    return Err(error);
                }
            };
            let usable = |t: &Tokens| {
                t.expires_at > now().saturating_add(60)
                    && rejected.as_ref().is_none_or(|r| r != &t.access_token)
            };
            if usable(&tokens) {
                cache.store(path, &tokens);
                return Ok(Credentials {
                    access_token: tokens.access_token,
                    account_id: tokens.account_id,
                });
            }
            let _lock = lock(&path).await?;
            let disk_path = path.clone();
            let tokens = tokio::task::spawn_blocking(move || load(&disk_path)).await??;
            if usable(&tokens) {
                cache.store(path, &tokens);
                return Ok(Credentials {
                    access_token: tokens.access_token,
                    account_id: tokens.account_id,
                });
            }
            if gate
                .as_ref()
                .is_some_and(|(p, until)| p == &path && *until > Instant::now())
            {
                bail!("Codex OAuth refresh is cooling down; retry shortly or run hey-proxy codex-login");
            }
            let fresh = match exchange_refresh(&client, &endpoint, &tokens).await {
                Ok(fresh) => fresh,
                Err(error) => {
                    *gate = Some((path, Instant::now() + Duration::from_secs(30)));
                    return Err(error);
                }
            };
            let (save_path, save_tokens) = (path.clone(), fresh.clone());
            tokio::task::spawn_blocking(move || save(&save_path, &save_tokens)).await??;
            cache.store(path, &fresh);
            *gate = None;
            Ok(Credentials {
                access_token: fresh.access_token,
                account_id: fresh.account_id,
            })
        })
        .await
        .map_err(|_| anyhow::anyhow!("Codex credential operation failed"))?
    }
}

pub(crate) fn authorization_url(
    issuer: &str,
    redirect_uri: &str,
    state: &str,
    verifier: &str,
) -> Result<url::Url> {
    let mut url = url::Url::parse(&format!("{}/oauth/authorize", issuer.trim_end_matches('/')))?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPES),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "codex_cli_rs"),
    ]);
    Ok(url)
}

#[derive(Deserialize)]
struct Callback {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}
struct CallbackState {
    state: String,
    sender: Mutex<Option<tokio::sync::oneshot::Sender<Result<String>>>>,
}
async fn callback(
    axum::extract::State(state): axum::extract::State<Arc<CallbackState>>,
    axum::extract::Query(query): axum::extract::Query<Callback>,
) -> (axum::http::StatusCode, &'static str) {
    use axum::http::StatusCode;
    let state_matches = query.state.as_deref().is_some_and(|received| {
        received == state.state
            || received
                .strip_suffix("__codex_ls")
                .is_some_and(|base| base == state.state)
    });
    if !state_matches {
        return (
            StatusCode::BAD_REQUEST,
            "OAuth state mismatch. Return to the authorization page.",
        );
    }
    let result = if query.error.is_some() {
        Err(anyhow::anyhow!("Codex sign-in was declined"))
    } else if let Some(code) = query.code.filter(|c| !c.is_empty()) {
        Ok(code)
    } else {
        return (StatusCode::BAD_REQUEST, "Missing authorization code.");
    };
    if let Some(sender) = state.sender.lock().await.take() {
        let _ = sender.send(result);
    }
    (
        StatusCode::OK,
        "Authorization received. Return to your terminal to check that sign-in completed. You can close this tab.",
    )
}

fn import_tokens_from_codex_home() -> Result<Tokens> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|s| !s.is_empty())
                .map(|home| PathBuf::from(home).join(".codex"))
        })
        .context("Cannot determine CODEX_HOME or HOME")?;
    let auth_path = codex_home.join("auth.json");
    let raw = fs::read(&auth_path).with_context(|| {
        format!(
            "Cannot read {}; sign in with browser OAuth or --device-code instead",
            auth_path.display()
        )
    })?;
    let value: Value = serde_json::from_slice(&raw).context("Invalid Codex auth.json format")?;
    let root = if value["tokens"].is_object() {
        &value["tokens"]
    } else {
        &value
    };
    let access_token = root["access_token"]
        .as_str()
        .filter(|s| is_oauth_token(s))
        .context("auth.json does not contain a valid OAuth access_token")?;
    let refresh_token = root["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("auth.json does not contain a valid OAuth refresh_token")?;
    let (access_exp, access_account) = jwt_metadata(access_token);
    let (id_exp, id_account) = root["id_token"]
        .as_str()
        .map(jwt_metadata)
        .unwrap_or((None, None));
    let account_id = root["account_id"]
        .as_str()
        .filter(|id| is_header_token(id))
        .map(str::to_owned)
        .or(id_account)
        .or(access_account);
    let expires_at = access_exp
        .or(id_exp)
        .unwrap_or_else(|| now().saturating_add(3600));
    Ok(Tokens {
        access_token: access_token.into(),
        refresh_token: refresh_token.into(),
        expires_at,
        account_id,
    })
}

async fn login_with_device_code(client: &reqwest::Client, issuer: &str) -> Result<Tokens> {
    let base = issuer.trim_end_matches('/');
    let api_base = format!("{base}/api/accounts");
    let response = client
        .post(format!("{api_base}/deviceauth/usercode"))
        .timeout(Duration::from_secs(30))
        .header("accept", "application/json")
        .json(&json!({"client_id": CLIENT_ID}))
        .send()
        .await
        .context("Cannot request Codex device code")?;
    ensure!(
        response.status().is_success(),
        "Codex device code request failed (HTTP {})",
        response.status().as_u16()
    );
    let payload: Value = response
        .json()
        .await
        .context("Invalid Codex device code response")?;
    let device_auth_id = payload["device_auth_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("Missing device_auth_id")?;
    let user_code = payload["user_code"]
        .as_str()
        .or_else(|| payload["usercode"].as_str())
        .filter(|s| !s.is_empty())
        .context("Missing user_code")?;
    let interval = payload["interval"]
        .as_str()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or_else(|| payload["interval"].as_u64())
        .unwrap_or(5)
        .clamp(2, 30);
    println!(
        "Sign in to Codex for hey-proxy using device code:\n1. Open: {base}/codex/device\n2. Enter code: {user_code}\nWaiting up to 15 minutes for authorization…"
    );
    let deadline = Instant::now() + Duration::from_secs(900);
    let poll_url = format!("{api_base}/deviceauth/token");
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let poll = client
            .post(&poll_url)
            .timeout(Duration::from_secs(30))
            .header("accept", "application/json")
            .json(&json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code,
            }))
            .send()
            .await
            .context("Cannot poll Codex device code status")?;
        if poll.status().is_success() {
            let data: Value = poll
                .json()
                .await
                .context("Invalid Codex device token response")?;
            let code = data["authorization_code"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("Missing authorization_code in device auth response")?;
            let verifier = data["code_verifier"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("Missing code_verifier in device auth response")?;
            let redirect_uri = format!("{base}/deviceauth/callback");
            return exchange_code(
                client,
                &format!("{base}/oauth/token"),
                code,
                &redirect_uri,
                verifier,
            )
            .await;
        }
        if matches!(poll.status().as_u16(), 403 | 404) {
            if Instant::now() >= deadline {
                bail!(
                    "Codex device code sign-in timed out; run hey-proxy codex-login --device-code again"
                );
            }
            continue;
        }
        bail!(
            "Codex device code sign-in failed (HTTP {})",
            poll.status().as_u16()
        );
    }
}

pub(crate) async fn login(
    config: &crate::config::Config,
    config_path: &Path,
    options: LoginOptions,
) -> Result<()> {
    let provider = config.codex.clone().unwrap_or_default();
    ensure!(
        config.mode != crate::config::Mode::Client,
        "Sign in on the proxy host, not a client relay"
    );
    let path = provider.credentials_path(Some(config_path))?;
    fs::create_dir_all(path.parent().context("Invalid Codex credential path")?)?;
    let client = crate::proxy::build_client(config)?;
    let tokens = if options.import_codex_home {
        import_tokens_from_codex_home()?
    } else if options.device_code {
        login_with_device_code(&client, &provider.issuer_url).await?
    } else {
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", DEFAULT_PORT)).await {
            Ok(listener) => listener,
            Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", FALLBACK_PORT))
                .await
                .context(
                    "Cannot bind Codex OAuth callback on port 1455 or 1457; try --device-code",
                )?,
        };
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://localhost:{port}/auth/callback");
        let state = URL_SAFE_NO_PAD.encode(random::<32>()?);
        let verifier = URL_SAFE_NO_PAD.encode(random::<32>()?);
        let url = authorization_url(&provider.issuer_url, &redirect_uri, &state, &verifier)?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let shared = Arc::new(CallbackState {
            state: state.clone(),
            sender: Mutex::new(Some(sender)),
        });
        let app = axum::Router::new()
            .route("/auth/callback", axum::routing::get(callback))
            .route("/callback", axum::routing::get(callback))
            .with_state(shared);
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        println!(
            "Sign in to Codex (ChatGPT) for hey-proxy:\n{url}\nWaiting up to five minutes for browser authorization…"
        );
        if !options.no_browser {
            #[cfg(target_os = "macos")]
            let command = "open";
            #[cfg(not(target_os = "macos"))]
            let command = "xdg-open";
            let _ = std::process::Command::new(command)
                .arg(url.as_str())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        let result = tokio::time::timeout(Duration::from_secs(300), receiver).await;
        server.abort();
        let code = result
            .context("Codex sign-in timed out; run hey-proxy codex-login again")?
            .context("Codex sign-in callback closed")??;
        exchange_code(
            &client,
            &provider.token_url(),
            &code,
            &redirect_uri,
            &verifier,
        )
        .await?
    };
    let _lock = lock(&path).await?;
    save(&path, &tokens)?;
    println!("Codex subscription connected. hey-proxy owns and refreshes these credentials.");
    if config.codex.is_none() {
        println!(
            "Subscription usage is ready (`hey-proxy usage --provider codex` and `hey-proxy recommend`)."
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
