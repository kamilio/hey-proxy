mod error;
mod partial;
mod replay;
mod request;
mod response;
mod stream;
mod validate;

pub use error::ResponseError;
pub use replay::ReasoningCodec;
pub use request::{ConvertedRequest, Tool, convert_request};
pub use response::{convert_response, usage};
pub use stream::ResponseStream;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(default = "default_url")]
    pub upstream_url: String,
    #[serde(default)]
    pub auth: Auth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Unknown models default to budgets; select levels if the model requires them.
    #[serde(default)]
    pub thinking: Thinking,
    #[serde(default = "default_cache_seconds")]
    pub credential_cache_seconds: u64,
}
fn default_cache_seconds() -> u64 {
    2400
}
fn default_url() -> String {
    "https://generativelanguage.googleapis.com/v1beta".into()
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Auth {
    #[default]
    ApiKey,
    Bearer,
    Adc,
    GcloudAdc,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Thinking {
    #[default]
    Auto,
    Budget,
    Level,
}
impl ProviderConfig {
    pub fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.upstream_url)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!(
                "gemini.upstream_url must be an HTTP(S) API root without credentials, query or fragment"
            );
        }
        if !(1..=86400).contains(&self.credential_cache_seconds) {
            bail!("credential_cache_seconds must be 1..86400");
        }
        match self.auth {
            Auth::Adc | Auth::GcloudAdc if self.api_key.is_some() => {
                bail!("gcloud_adc obtains its own credentials; omit gemini.api_key")
            }
            Auth::Adc | Auth::GcloudAdc => {}
            _ => {
                let key = self.api_key.as_deref().unwrap_or("");
                if crate::credentials::validate_source(key).is_err() {
                    bail!("gemini.api_key must contain a valid API key or OAuth access token");
                }
            }
        }
        Ok(())
    }
    pub fn endpoint(&self, model: &str, stream: bool) -> Result<String> {
        let model = model.strip_prefix("models/").unwrap_or(model);
        if model.is_empty()
            || !model
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        {
            bail!("Invalid Gemini model name");
        }
        Ok(format!(
            "{}/models/{}:{}{}",
            self.upstream_url.trim_end_matches('/'),
            model,
            if stream {
                "streamGenerateContent"
            } else {
                "generateContent"
            },
            if stream { "?alt=sse" } else { "" }
        ))
    }
}
