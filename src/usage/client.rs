use super::{AccountUsage, Accounts, Recommendation, SCHEMA_VERSION};
use reqwest::{Url, header::HeaderValue};
use serde::de::DeserializeOwned;
use std::{fmt, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidBaseUrl,
    InvalidIdentifier,
    InvalidTimeout,
    InvalidToken,
    Transport,
    Http(u16),
    ResponseTooLarge,
    InvalidResponse,
    UnsupportedSchema(u32),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBaseUrl => write!(
                f,
                "Base URL must be an HTTP(S) proxy root or /v1 URL without credentials or query"
            ),
            Self::InvalidIdentifier => write!(
                f,
                "Provider and account IDs must be 1..128 ASCII letters, digits, '-' or '_'"
            ),
            Self::InvalidTimeout => write!(f, "Timeout must be greater than zero"),
            Self::InvalidToken => write!(f, "Access token cannot be used as an HTTP Bearer header"),
            Self::Transport => write!(f, "Cannot reach hey-proxy usage endpoint"),
            Self::Http(status) => write!(f, "Proxy usage endpoint returned HTTP {status}"),
            Self::ResponseTooLarge => write!(f, "Proxy usage response exceeded 1 MiB"),
            Self::InvalidResponse => write!(f, "Invalid proxy usage response"),
            Self::UnsupportedSchema(version) => {
                write!(f, "Unsupported usage schema version {version}")
            }
        }
    }
}
impl std::error::Error for Error {}

/// Reusable async HTTP client. Construction performs no network or file access.
/// The token is a hey-proxy host access key, never a Claude/Codex OAuth token.
/// Redirects are rejected. Responses and errors are bounded; HTTP error bodies
/// and credentials are never included in error messages. No automatic polling/retry.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
    token: Option<HeaderValue>,
    timeout: Duration,
}
impl Client {
    /// Accepts a proxy root or its client `/v1` base URL. Loopback skips environment proxies.
    pub fn new(base_url: &str, token: Option<&str>) -> Result<Self, Error> {
        let mut base = Url::parse(base_url).map_err(|_| Error::InvalidBaseUrl)?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !matches!(base.path(), "/" | "/v1" | "/v1/")
        {
            return Err(Error::InvalidBaseUrl);
        }
        base.set_path("/");
        let token = token
            .map(|token| {
                if token.is_empty() {
                    return Err(Error::InvalidToken);
                }
                let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| Error::InvalidToken)?;
                value.set_sensitive(true);
                Ok(value)
            })
            .transpose()?;
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5));
        if base.host_str().is_some_and(|host| {
            host == "localhost"
                || host == "[::1]"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }) {
            builder = builder.no_proxy();
        }
        Ok(Self {
            http: builder.build().map_err(|_| Error::Transport)?,
            base,
            token,
            timeout: Duration::from_secs(30),
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, Error> {
        if timeout.is_zero() {
            return Err(Error::InvalidTimeout);
        }
        self.timeout = timeout;
        Ok(self)
    }

    /// List configured account aliases without fetching provider usage or resolving OAuth.
    pub async fn accounts(&self) -> Result<Accounts, Error> {
        let result: Accounts = self.get("usage/v1/accounts").await?;
        check_version(result.schema_version)?;
        Ok(result)
    }

    /// Recommend the best subscription provider (`codex` or `claude`) based on earliest expiring usage and remaining quota.
    pub async fn recommend(&self) -> Result<Recommendation, Error> {
        let result: Recommendation = self.get("usage/v1/recommend").await?;
        check_version(result.schema_version)?;
        Ok(result)
    }

    /// Fetch remaining quota and extra spend for one provider/account. Inspect `state`
    /// before using the data: stale/disabled/provider errors are successful HTTP readings.
    pub async fn usage(&self, provider: &str, account: &str) -> Result<AccountUsage, Error> {
        for id in [provider, account] {
            if id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            {
                return Err(Error::InvalidIdentifier);
            }
        }
        let result: AccountUsage = self.get(&format!("usage/v1/{provider}/{account}")).await?;
        check_version(result.schema_version)?;
        if result.account.provider != provider || result.account.id != account {
            return Err(Error::InvalidResponse);
        }
        Ok(result)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let mut request = self
            .http
            .get(self.base.join(path).map_err(|_| Error::InvalidBaseUrl)?)
            .timeout(self.timeout)
            .header("accept", "application/json");
        if let Some(token) = &self.token {
            request = request.header("authorization", token.clone());
        }
        let mut response = request.send().await.map_err(|_| Error::Transport)?;
        if !response.status().is_success() {
            return Err(Error::Http(response.status().as_u16()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            if bytes.len() + chunk.len() > 1024 * 1024 {
                return Err(Error::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse)
    }
}
fn check_version(version: u32) -> Result<(), Error> {
    if version == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(Error::UnsupportedSchema(version))
    }
}
