//! Subscription quota and provider recommendation SDK. Queries hey-proxy, never a provider credential store.
//!
//! Provider and account IDs are proxy-local aliases (`claude/default`, `codex/default`).
//! Readings are account-wide, may be cached or stale, and are not token budgets.
//!
//! ```no_run
//! use hey_proxy::usage::{Client, State};
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let token = std::env::var("HEY_PROXY_TOKEN").ok();
//! let client = Client::new("http://127.0.0.1:8080/v1", token.as_deref())?;
//! for account in client.accounts().await?.accounts {
//!     let usage = client.usage(&account.provider, &account.id).await?;
//!     if usage.reading.state == State::Ok {
//!         if let Some(data) = usage.reading.data {
//!             for window in data.windows {
//!                 println!("{}: {:?}% left", window.label, window.remaining_percent);
//!             }
//!             if let Some(spend) = data.extra_usage.and_then(|extra| extra.spend) {
//!                 println!("Extra spent: {:?} {}; left: {:?}", spend.used, spend.currency, spend.remaining);
//!             }
//!         }
//!     }
//! }
//! let recommendation = client.recommend().await?;
//! println!("Recommended provider: {:?}", recommendation.recommended_provider);
//! # Ok(()) }
//! ```
mod client;
mod recommend;
pub use client::{Client, Error};
pub use recommend::{format_duration_short, format_unix_iso8601, parse_timestamp, recommend};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub provider: String,
    pub id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Accounts {
    pub schema_version: u32,
    pub accounts: Vec<Account>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountUsage {
    pub schema_version: u32,
    pub account: Account,
    #[serde(flatten)]
    pub reading: Reading,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Ok,
    Stale,
    Error,
    Disabled,
    /// A newer server state this SDK does not yet understand; not a fresh reading.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reading {
    pub state: State,
    /// Unix seconds of the last successful provider fetch, never the CLI read time.
    pub updated_at: Option<u64>,
    pub data: Option<UsageData>,
    pub error: Option<String>,
    pub retry_after_seconds: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageData {
    pub windows: Vec<Window>,
    pub extra_usage: Option<ExtraUsage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Window {
    pub id: String,
    pub label: String,
    pub group: Option<String>,
    /// Can exceed 100 if the provider reports usage above a cap.
    pub used_percent: Option<f64>,
    /// max(0, 100 - used_percent). Missing utilization stays unknown.
    pub remaining_percent: Option<f64>,
    pub resets_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtraUsage {
    /// Disabled does not erase money already spent in this period.
    pub enabled: Option<bool>,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub spend: Option<SpendLimit>,
}

/// Provider-reported extra usage, separate from the dashboard's API-equivalent estimate.
/// Amounts use major currency units (e.g. USD dollars). A missing cap is unknown,
/// not unlimited. Remaining is cap headroom, not a prepaid balance or credit limit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpendLimit {
    pub currency: String,
    pub period: String,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
    pub over_limit: Option<f64>,
    /// Unknown unless explicitly supplied by the provider; never inferred from quota resets.
    pub resets_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExpiringWindow {
    pub id: String,
    pub label: String,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub resets_at: Option<String>,
    pub resets_in_seconds: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecommendationCandidate {
    pub account: Account,
    pub state: State,
    pub available: bool,
    pub exhausted: bool,
    pub using_extra_usage: bool,
    pub effective_remaining_percent: Option<f64>,
    pub earliest_expiring_window: Option<ExpiringWindow>,
    pub next_reset_at: Option<String>,
    pub next_reset_in_seconds: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    pub schema_version: u32,
    pub recommended_provider: Option<String>,
    pub recommended_account: Option<String>,
    pub reason: String,
    pub summary: String,
    pub candidates: Vec<RecommendationCandidate>,
}
