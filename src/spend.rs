use crate::proxy::logs::{pricing, store::Entry};
use anyhow::{Context, Result};
use hey_proxy::usage::{AccountUsage, SCHEMA_VERSION, State, Window};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const SPEND_DB_FILENAME: &str = "spend.sqlite3";
const SPEND_PRICE_BOOK_VERSION: &str = "2026-10-02-astra-v3";
const FIVE_HOURS_MS: i64 = 5 * 3600 * 1000;
const SEVEN_DAYS_MS: i64 = 7 * 86_400 * 1000;
const DAY_MS: i64 = 86_400 * 1000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS spend_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS spend_ledger (
    record_key TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    timestamp_ms INTEGER NOT NULL,
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL DEFAULT 'default',
    model TEXT NOT NULL,
    requested_model TEXT,
    price_model TEXT,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    cached_input_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_1h_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
    cost_nano_usd INTEGER NOT NULL DEFAULT 0,
    uncached_equivalent_nano_usd INTEGER NOT NULL DEFAULT 0,
    is_subscription INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_spend_ledger_time ON spend_ledger(timestamp_ms);
CREATE INDEX IF NOT EXISTS idx_spend_ledger_provider_time ON spend_ledger(provider, timestamp_ms);
CREATE INDEX IF NOT EXISTS idx_spend_ledger_model_time ON spend_ledger(model, timestamp_ms);

CREATE TABLE IF NOT EXISTS subscription_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp_ms INTEGER NOT NULL,
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    window_id TEXT NOT NULL,
    window_label TEXT NOT NULL,
    used_percent REAL NOT NULL,
    remaining_percent REAL NOT NULL,
    resets_at TEXT,
    window_start_ms INTEGER,
    window_end_ms INTEGER
);
CREATE INDEX IF NOT EXISTS idx_sub_snapshots_lookup
    ON subscription_snapshots(provider, account_id, window_id, timestamp_ms DESC);

CREATE TABLE IF NOT EXISTS sync_cursors (
    file_path TEXT PRIMARY KEY,
    mtime_ms INTEGER NOT NULL,
    file_size INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL
);
";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpendRecord {
    pub record_key: String,
    pub source: String,
    pub timestamp_ms: i64,
    pub provider: String,
    pub account_id: String,
    pub model: String,
    pub requested_model: Option<String>,
    pub price_model: Option<String>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_nano_usd: i64,
    pub uncached_equivalent_nano_usd: i64,
    pub is_subscription: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelSpendSummary {
    pub provider: String,
    pub model: String,
    pub price_model: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub uncached_cost_usd: f64,
    pub cache_savings_usd: f64,
    pub share_percent: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderSpendSummary {
    pub provider: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub uncached_cost_usd: f64,
    pub cache_savings_usd: f64,
    pub share_percent: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowYieldEstimate {
    pub window_id: String,
    pub label: String,
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub resets_at: Option<String>,
    pub window_start_ms: Option<i64>,
    pub window_end_ms: Option<i64>,
    pub window_requests: u64,
    pub window_tokens: u64,
    pub window_spend_usd: f64,
    pub window_uncached_usd: f64,
    /// Estimated API-equivalent USD per 1% of subscription window consumed (when used_percent > 0)
    pub usd_per_one_percent: Option<f64>,
    /// Implied API-equivalent USD capacity of a full 100% subscription window (when used_percent > 0)
    pub implied_full_window_usd: Option<f64>,
    /// Estimated remaining API-equivalent USD left in this window before reset
    pub estimated_remaining_usd: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubscriptionYieldReport {
    pub provider: String,
    pub account_id: String,
    pub state: String,
    pub spend_24h_usd: f64,
    pub spend_7d_usd: f64,
    pub spend_30d_usd: f64,
    pub spend_all_time_usd: f64,
    pub uncached_7d_usd: f64,
    pub projected_monthly_spend_usd: f64,
    pub windows: Vec<WindowYieldEstimate>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpendReport {
    pub schema_version: u32,
    pub database_path: String,
    pub window_label: String,
    pub since_timestamp_ms: Option<i64>,
    pub generated_at_ms: i64,
    pub total_requests: u64,
    pub total_input_tokens: u64,
    pub total_cached_input_tokens: u64,
    pub total_cache_write_tokens: u64,
    pub total_output_tokens: u64,
    pub total_reasoning_tokens: u64,
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    pub total_uncached_cost_usd: f64,
    pub total_cache_savings_usd: f64,
    pub by_provider: Vec<ProviderSpendSummary>,
    pub by_model: Vec<ModelSpendSummary>,
    pub subscription_yield: Vec<SubscriptionYieldReport>,
}

enum Command {
    Record(SpendRecord),
    Snapshot(AccountUsage),
    Flush(tokio::sync::oneshot::Sender<()>),
}

pub struct SpendTracker {
    sender: SyncSender<Command>,
    enqueued: AtomicU64,
    committed: AtomicU64,
}

static GLOBAL_TRACKER: OnceLock<Arc<SpendTracker>> = OnceLock::new();

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub fn default_db_path(config_path: Option<&Path>) -> PathBuf {
    if let Ok(env_path) = std::env::var("HEY_PROXY_SPEND_DB")
        && !env_path.trim().is_empty()
    {
        return PathBuf::from(env_path);
    }
    if let Some(cfg) = config_path
        && let Some(parent) = cfg.parent()
    {
        return parent.join(SPEND_DB_FILENAME);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".hey-proxy")
            .join(SPEND_DB_FILENAME);
    }
    PathBuf::from(SPEND_DB_FILENAME)
}

pub fn global_tracker(config_path: Option<&Path>) -> Option<Arc<SpendTracker>> {
    if let Some(tracker) = GLOBAL_TRACKER.get() {
        return Some(tracker.clone());
    }
    let path = default_db_path(config_path);
    let tracker = SpendTracker::open(path).ok()?;
    let _ = GLOBAL_TRACKER.set(tracker.clone());
    Some(GLOBAL_TRACKER.get().cloned().unwrap_or(tracker))
}

impl SpendTracker {
    pub fn open(path: PathBuf) -> Result<Arc<Self>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let (sender, receiver) = mpsc::sync_channel(8192);
        let tracker = Arc::new(Self {
            sender,
            enqueued: AtomicU64::new(0),
            committed: AtomicU64::new(0),
        });
        let worker_tracker = tracker.clone();
        std::thread::Builder::new()
            .name("hey-proxy-spend-writer".into())
            .spawn(move || {
                if let Ok(mut conn) = open_connection(&path) {
                    writer_loop(&mut conn, receiver, &worker_tracker);
                } else {
                    while let Ok(cmd) = receiver.recv() {
                        if let Command::Flush(done) = cmd {
                            let _ = done.send(());
                        }
                    }
                }
            })
            .context("Failed to spawn spend writer thread")?;
        Ok(tracker)
    }

    pub fn record_entry(&self, entry: &Entry) {
        if let Some(record) = record_from_proxy_entry(entry) {
            self.enqueued.fetch_add(1, Ordering::Relaxed);
            let _ = self.sender.try_send(Command::Record(record));
        }
    }

    pub fn record_subscription(&self, usage: &AccountUsage) {
        if usage.reading.state == State::Ok
            && usage
                .reading
                .data
                .as_ref()
                .is_some_and(|d| !d.windows.is_empty())
        {
            self.enqueued.fetch_add(1, Ordering::Relaxed);
            let _ = self.sender.try_send(Command::Snapshot(usage.clone()));
        }
    }

    pub async fn flush(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.sender.try_send(Command::Flush(tx)).is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
        }
    }
}

fn open_connection(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    let conn = Connection::open(path).context("Failed to open spend SQLite database")?;
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY;",
    )?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

fn writer_loop(conn: &mut Connection, rx: Receiver<Command>, tracker: &SpendTracker) {
    let mut batch = Vec::with_capacity(128);
    while let Ok(first) = rx.recv() {
        batch.push(first);
        while batch.len() < 256 {
            match rx.try_recv() {
                Ok(cmd) => batch.push(cmd),
                Err(_) => break,
            }
        }
        let mut flush_waiters = Vec::new();
        if let Ok(tx) = conn.transaction() {
            for cmd in batch.drain(..) {
                match cmd {
                    Command::Record(rec) => {
                        let _ = upsert_spend_record(&tx, &rec);
                        tracker.committed.fetch_add(1, Ordering::Relaxed);
                    }
                    Command::Snapshot(usage) => {
                        let _ = insert_subscription_snapshot(&tx, &usage);
                        tracker.committed.fetch_add(1, Ordering::Relaxed);
                    }
                    Command::Flush(done) => flush_waiters.push(done),
                }
            }
            let _ = tx.commit();
        } else {
            for cmd in batch.drain(..) {
                if let Command::Flush(done) = cmd {
                    flush_waiters.push(done);
                }
            }
        }
        for done in flush_waiters {
            let _ = done.send(());
        }
    }
}

fn upsert_spend_record(conn: &Connection, rec: &SpendRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO spend_ledger (
            record_key, source, timestamp_ms, provider, account_id, model,
            requested_model, price_model, input_tokens, cached_input_tokens,
            cache_write_tokens, cache_write_1h_tokens, output_tokens, reasoning_tokens,
            cost_nano_usd, uncached_equivalent_nano_usd, is_subscription
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
        ON CONFLICT(record_key) DO UPDATE SET
            timestamp_ms = excluded.timestamp_ms,
            provider = excluded.provider,
            model = excluded.model,
            price_model = excluded.price_model,
            input_tokens = excluded.input_tokens,
            cached_input_tokens = excluded.cached_input_tokens,
            cache_write_tokens = excluded.cache_write_tokens,
            cache_write_1h_tokens = excluded.cache_write_1h_tokens,
            output_tokens = excluded.output_tokens,
            reasoning_tokens = excluded.reasoning_tokens,
            cost_nano_usd = excluded.cost_nano_usd,
            uncached_equivalent_nano_usd = excluded.uncached_equivalent_nano_usd",
        params![
            rec.record_key,
            rec.source,
            rec.timestamp_ms,
            rec.provider,
            rec.account_id,
            rec.model,
            rec.requested_model,
            rec.price_model,
            rec.input_tokens as i64,
            rec.cached_input_tokens as i64,
            rec.cache_write_tokens as i64,
            rec.cache_write_1h_tokens as i64,
            rec.output_tokens as i64,
            rec.reasoning_tokens as i64,
            rec.cost_nano_usd,
            rec.uncached_equivalent_nano_usd,
            i32::from(rec.is_subscription),
        ],
    )?;
    Ok(())
}

fn insert_subscription_snapshot(conn: &Connection, usage: &AccountUsage) -> Result<()> {
    let ts = now_ms();
    for w in usage
        .reading
        .data
        .as_ref()
        .map(|d| d.windows.as_slice())
        .unwrap_or(&[])
    {
        let end_ms = w.resets_at.as_deref().and_then(parse_rfc3339_ms);
        let start_ms = end_ms.map(|end| {
            if w.id == "five_hour" || w.id.contains("five_hour") || w.id.contains("session") {
                end - FIVE_HOURS_MS
            } else {
                end - SEVEN_DAYS_MS
            }
        });
        conn.execute(
            "INSERT INTO subscription_snapshots (
                timestamp_ms, provider, account_id, window_id, window_label,
                used_percent, remaining_percent, resets_at, window_start_ms, window_end_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                ts,
                usage.account.provider,
                usage.account.id,
                w.id,
                w.label,
                w.used_percent.unwrap_or(0.0),
                w.remaining_percent.unwrap_or(100.0),
                w.resets_at,
                start_ms,
                end_ms,
            ],
        )?;
    }
    Ok(())
}

pub fn parse_rfc3339_ms(value: &str) -> Option<i64> {
    // Parse ISO-8601 / RFC3339 timestamps such as 2026-10-09T14:52:05Z or with fractional seconds
    let trimmed = value.trim();
    let z = trimmed
        .strip_suffix('Z')
        .or_else(|| trimmed.strip_suffix("+00:00"))?;
    let (date_part, time_part) = z.split_once('T')?;
    let mut date_iter = date_part.split('-');
    let year: i64 = date_iter.next()?.parse().ok()?;
    let month: i64 = date_iter.next()?.parse().ok()?;
    let day: i64 = date_iter.next()?.parse().ok()?;
    let mut time_iter = time_part.split(':');
    let hour: i64 = time_iter.next()?.parse().ok()?;
    let minute: i64 = time_iter.next()?.parse().ok()?;
    let sec_str = time_iter.next()?;
    let (sec_whole, millis) = if let Some((s, frac)) = sec_str.split_once('.') {
        let mut frac_digits = frac.chars().take(3).collect::<String>();
        while frac_digits.len() < 3 {
            frac_digits.push('0');
        }
        (
            s.parse::<i64>().ok()?,
            frac_digits.parse::<i64>().unwrap_or(0),
        )
    } else {
        (sec_str.parse::<i64>().ok()?, 0)
    };
    let days = days_from_civil(year, month, day)?;
    Some((((days * 24 + hour) * 60 + minute) * 60 + sec_whole) * 1000 + millis)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// Canonicalize model aliases (e.g. `ultima` / `ultima-alpha` -> `gpt-6-astra`).
pub fn canonical_model_name(model: &str) -> String {
    let trimmed = model.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower == "ultima" || lower == "ultima-alpha" || lower.starts_with("ultima-") {
        return "gpt-6-astra".to_string();
    }
    trimmed.to_string()
}

/// Fallback pricing rates `[input_per_1m, cached_per_1m, output_per_1m, cache_write_5m_per_1m, long_threshold, cache_write_1h_per_1m]`
fn extended_model_rates(model: &str) -> (String, [f64; 6]) {
    let m = model.trim();
    let lower = m.to_ascii_lowercase();
    let stripped = lower
        .strip_prefix("gemini/models/")
        .or_else(|| lower.strip_prefix("models/"))
        .or_else(|| lower.strip_prefix("gemini/"))
        .unwrap_or(&lower);

    match stripped {
        "gpt-6-astra" | "ultima-alpha" | "ultima" => (
            "gpt-6-astra".into(),
            [10.0, 1.0, 50.0, 12.5, 272_000.0, 12.5],
        ),
        "gpt-6.1-sol" => ("gpt-6.1-sol".into(), [2.0, 0.1, 10.0, 2.5, 272_000.0, 2.5]),
        "gpt-6-sol" => ("gpt-6-sol".into(), [2.0, 0.2, 10.0, 2.5, 272_000.0, 2.5]),
        "gpt-6-luna" => (
            "gpt-6-luna".into(),
            [0.1, 0.01, 0.5, 0.125, 272_000.0, 0.125],
        ),
        "gpt-5.6-sol" => ("gpt-5.6-sol".into(), [4.0, 0.4, 20.0, 5.0, 272_000.0, 5.0]),
        "gpt-5.6-terra" => (
            "gpt-5.6-terra".into(),
            [2.0, 0.2, 12.0, 2.5, 272_000.0, 2.5],
        ),
        "gpt-5.6-luna" => (
            "gpt-5.6-luna".into(),
            [0.2, 0.02, 1.2, 0.25, 272_000.0, 0.25],
        ),
        "vega-alpha" | "gpt-daybreak-red-latest" | "gpt-daybreak-blue-latest" | "gpt-5.4" => {
            ("gpt-5.4".into(), [2.5, 0.25, 15.0, 2.5, 272_000.0, 2.5])
        }
        "gpt-5.5" => ("gpt-5.5".into(), [5.0, 0.5, 30.0, 5.0, 272_000.0, 5.0]),
        "gpt-5.4-mini" => ("gpt-5.4-mini".into(), [0.75, 0.075, 4.5, 0.75, 0.0, 0.75]),
        "gpt-5.4-nano" => ("gpt-5.4-nano".into(), [0.2, 0.02, 1.25, 0.2, 0.0, 0.2]),
        "gpt-5.3-codex"
        | "gpt-5.3-codex-spark"
        | "codex-auto-review"
        | "gpt-5.2"
        | "gpt-5.2-codex" => ("gpt-5.3-codex".into(), [1.75, 0.175, 14.0, 1.75, 0.0, 1.75]),
        "gpt-5.1" | "gpt-5.1-codex" | "gpt-5" | "gpt-5-codex" => {
            ("gpt-5.1".into(), [1.25, 0.125, 10.0, 1.25, 0.0, 1.25])
        }
        "gemini-early-exp"
        | "gemini-3.1-pro-preview"
        | "gemini-2.5-pro"
        | "gemini-test" => (
            "gemini-3.1-pro-preview".into(),
            [2.0, 0.2, 12.0, 2.0, 200_000.0, 2.0],
        ),
        "claude-fable-5-1" | "claude-mythos-5-1" => (
            "claude-fable-5-1".into(),
            [10.0, 0.25, 50.0, 12.5, 0.0, 20.0],
        ),
        "claude-fable-5" | "claude-mythos-5" => {
            ("claude-fable-5".into(), [10.0, 1.0, 50.0, 12.5, 0.0, 20.0])
        }
        "claude-opus-5-5" => ("claude-opus-5-5".into(), [4.0, 0.2, 20.0, 5.0, 0.0, 8.0]),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6"
        | "claude-opus-4-5" => ("claude-opus-4-7".into(), [5.0, 0.5, 25.0, 6.25, 0.0, 10.0]),
        "claude-opus-4-1" | "claude-opus-4" => {
            ("claude-opus-4".into(), [15.0, 1.5, 75.0, 18.75, 0.0, 30.0])
        }
        "claude-sonnet-5-5" | "claude-sonnet-5" => {
            ("claude-sonnet-5-5".into(), [2.0, 0.2, 10.0, 2.5, 0.0, 4.0])
        }
        "claude-sonnet-4-6" | "claude-sonnet-4-5" | "claude-sonnet-4" => {
            ("claude-sonnet-4-6".into(), [3.0, 0.3, 15.0, 3.75, 0.0, 6.0])
        }
        "claude-haiku-4-5" | "claude-3-5-haiku" => {
            ("claude-haiku-4-5".into(), [1.0, 0.1, 5.0, 1.25, 0.0, 2.0])
        }
        _ if stripped.starts_with("claude-opus") => {
            ("claude-opus-4-7".into(), [5.0, 0.5, 25.0, 6.25, 0.0, 10.0])
        }
        _ if stripped.starts_with("claude-sonnet") => {
            ("claude-sonnet-4-6".into(), [3.0, 0.3, 15.0, 3.75, 0.0, 6.0])
        }
        _ if stripped.starts_with("claude-haiku") => {
            ("claude-haiku-4-5".into(), [1.0, 0.1, 5.0, 1.25, 0.0, 2.0])
        }
        _ if stripped.starts_with("claude-") => {
            ("claude-sonnet-4-6".into(), [3.0, 0.3, 15.0, 3.75, 0.0, 6.0])
        }
        _ if stripped.starts_with("gemini") => (
            "gemini-3.1-pro-preview".into(),
            [2.0, 0.2, 12.0, 2.0, 200_000.0, 2.0],
        ),
        _ if stripped.contains("codex") => {
            ("gpt-5.3-codex".into(), [1.75, 0.175, 14.0, 1.75, 0.0, 1.75])
        }
        _ => ("gpt-5.4".into(), [2.5, 0.25, 15.0, 2.5, 272_000.0, 2.5]),
    }
}

pub fn compute_token_costs(
    model: &str,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_tokens: u64,
    cache_write_1h_tokens: u64,
    output_tokens: u64,
) -> (String, i64, i64) {
    let canonical = canonical_model_name(model);
    let mut dummy = Entry {
        method: "POST".into(),
        path: if canonical.starts_with("claude-") {
            "/v1/messages".into()
        } else {
            "/v1/responses".into()
        },
        routed_model: Some(canonical.clone()),
        response_model: Some(canonical.clone()),
        input_tokens: Some(input_tokens),
        cached_input_tokens: Some(cached_input_tokens),
        cache_write_tokens: Some(cache_write_tokens),
        cache_write_1h_tokens: Some(cache_write_1h_tokens),
        output_tokens: Some(output_tokens),
        ..Entry::default()
    };
    let book_price = pricing::price(&dummy);
    let (price_model, rates) =
        extended_model_rates(book_price.price_model.as_deref().unwrap_or(&canonical));
    let cost_nano = if let Some(nano) = book_price.cost_nano_usd {
        nano
    } else {
        let cached = cached_input_tokens.min(input_tokens);
        let writes = cache_write_tokens.min(input_tokens.saturating_sub(cached));
        let writes_1h = cache_write_1h_tokens.min(writes);
        let uncached = input_tokens.saturating_sub(cached).saturating_sub(writes);
        let long = rates[4] > 0.0 && (input_tokens as f64) > rates[4];
        let mult_in = if long { 2.0 } else { 1.0 };
        let mult_out = if long { 1.5 } else { 1.0 };
        let cost_per_m = (uncached as f64 * rates[0]
            + cached as f64 * rates[1]
            + (writes - writes_1h) as f64 * rates[3]
            + writes_1h as f64 * rates[5])
            * mult_in
            + (output_tokens as f64) * rates[2] * mult_out;
        (cost_per_m * 1000.0).round() as i64
    };
    // Uncached list-price equivalent (what it would cost with 0% prompt caching)
    dummy.cached_input_tokens = Some(0);
    dummy.cache_write_tokens = Some(0);
    dummy.cache_write_1h_tokens = Some(0);
    let uncached_nano = pricing::price(&dummy).cost_nano_usd.unwrap_or_else(|| {
        let long = rates[4] > 0.0 && (input_tokens as f64) > rates[4];
        let mult_in = if long { 2.0 } else { 1.0 };
        let mult_out = if long { 1.5 } else { 1.0 };
        (((input_tokens as f64) * rates[0] * mult_in
            + (output_tokens as f64) * rates[2] * mult_out)
            * 1000.0)
            .round() as i64
    });
    (price_model, cost_nano, uncached_nano.max(cost_nano))
}

pub fn classify_provider_for_entry(entry: &Entry) -> (String, bool) {
    let model = entry
        .response_model
        .as_deref()
        .or(entry.routed_model.as_deref())
        .or(entry.requested_model.as_deref())
        .unwrap_or("unknown");
    if entry.project.as_deref() == Some("claude")
        || entry.path.contains("/messages")
        || model.starts_with("claude-")
    {
        return ("claude".into(), true);
    }
    if entry.project.as_deref() == Some("codex")
        || entry.path.contains("/codex")
        || model.contains("codex")
        || matches!(
            model,
            "gpt-6-astra"
                | "ultima-alpha"
                | "ultima"
                | "gpt-6-luna"
                | "gpt-5.6-luna"
                | "gpt-daybreak-red-latest"
                | "gpt-daybreak-blue-latest"
        )
    {
        return ("codex".into(), true);
    }
    if entry.project.as_deref() == Some("gemini")
        || model.starts_with("gemini")
        || model.starts_with("models/gemini")
    {
        return ("gemini".into(), false);
    }
    ("openai".into(), false)
}

pub fn record_from_proxy_entry(entry: &Entry) -> Option<SpendRecord> {
    let (Some(input), Some(output)) = (entry.input_tokens, entry.output_tokens) else {
        return None;
    };
    if input == 0 && output == 0 {
        return None;
    }
    let raw_model = entry
        .response_model
        .as_deref()
        .or(entry.routed_model.as_deref())
        .or(entry.requested_model.as_deref())
        .unwrap_or("unknown")
        .to_string();
    let model = canonical_model_name(&raw_model);
    let (provider, is_subscription) = classify_provider_for_entry(entry);
    let cached = entry.cached_input_tokens.unwrap_or(0);
    let writes = entry.cache_write_tokens.unwrap_or(0);
    let writes_1h = entry.cache_write_1h_tokens.unwrap_or(0);
    let reasoning = entry.reasoning_tokens.unwrap_or(0);
    let (price_model, cost_nano_usd, uncached_equivalent_nano_usd) =
        compute_token_costs(&model, input, cached, writes, writes_1h, output);
    Some(SpendRecord {
        record_key: format!("proxy:{}", entry.request_id),
        source: "proxy".into(),
        timestamp_ms: entry.timestamp_ms as i64,
        provider,
        account_id: "default".into(),
        model,
        requested_model: entry.requested_model.clone().or(Some(raw_model)),
        price_model: Some(price_model),
        input_tokens: input,
        cached_input_tokens: cached,
        cache_write_tokens: writes,
        cache_write_1h_tokens: writes_1h,
        output_tokens: output,
        reasoning_tokens: reasoning,
        cost_nano_usd,
        uncached_equivalent_nano_usd,
        is_subscription,
    })
}

pub fn sync_local_sources(db_path: &Path, cutoff_ms: Option<i64>) -> Result<usize> {
    let mut conn = open_connection(db_path)?;
    let stored_version: Option<String> = conn
        .query_row(
            "SELECT value FROM spend_meta WHERE key = 'price_version'",
            [],
            |r| r.get(0),
        )
        .ok();
    if stored_version.as_deref() != Some(SPEND_PRICE_BOOK_VERSION) {
        conn.execute_batch(
            "DELETE FROM sync_cursors;
             DELETE FROM spend_ledger;",
        )?;
        conn.execute(
            "INSERT INTO spend_meta(key, value) VALUES ('price_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SPEND_PRICE_BOOK_VERSION],
        )?;
    }
    let cursors = load_sync_cursors(&conn);
    let now = now_ms();
    let min_mtime_ms = cutoff_ms.unwrap_or(0);
    let mut inserted = 0usize;

    // 1. Sync any existing proxy request SQLite files in the same directory
    if let Some(parent) = db_path.parent()
        && let Ok(entries) = fs::read_dir(parent)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.ends_with(".sqlite3") && name != SPEND_DB_FILENAME {
                inserted += sync_proxy_sqlite_file(&mut conn, &cursors, &path, now).unwrap_or(0);
            }
        }
    }

    // 2. Sync Codex CLI session JSONL files (~/.codex/sessions/YYYY/MM/DD/*.jsonl)
    // Note: Never open Codex's state_*.sqlite; only read session JSONL files.
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let primary_codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        let codex_dirs = [
            primary_codex_home.join("sessions"),
            primary_codex_home.join("archived_sessions"),
            home.join(".poe-code").join("codex").join("sessions"),
            home.join(".poe-code").join("codex").join("archived_sessions"),
        ];
        let mut codex_jsonl_files = Vec::new();
        for dir in &codex_dirs {
            if dir.is_dir() {
                collect_recent_jsonl(dir, min_mtime_ms, &mut codex_jsonl_files);
            }
        }
        for chunk in codex_jsonl_files.chunks(500) {
            if let Ok(tx) = conn.transaction() {
                for file in chunk {
                    inserted += sync_codex_jsonl_file(&tx, &cursors, file, now).unwrap_or(0);
                }
                let _ = tx.commit();
            }
        }

        // 3. Sync Claude Code session JSONL files (~/.claude/projects/**/*.jsonl)
        let claude_projects = home.join(".claude").join("projects");
        if claude_projects.is_dir() {
            let mut jsonl_files = Vec::new();
            collect_recent_jsonl(&claude_projects, min_mtime_ms, &mut jsonl_files);
            for chunk in jsonl_files.chunks(500) {
                if let Ok(tx) = conn.transaction() {
                    for file in chunk {
                        inserted += sync_claude_jsonl_file(&tx, &cursors, file, now).unwrap_or(0);
                    }
                    let _ = tx.commit();
                }
            }
        }
    }

    Ok(inserted)
}

fn collect_recent_jsonl(dir: &Path, min_mtime_ms: i64, out: &mut Vec<PathBuf>) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            collect_recent_jsonl(&path, min_mtime_ms, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if mtime >= min_mtime_ms {
                out.push(path);
            }
        }
    }
}

fn load_sync_cursors(conn: &Connection) -> HashMap<String, (i64, i64)> {
    let mut map = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT file_path, mtime_ms, file_size FROM sync_cursors")
        && let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
            ))
        })
    {
        for (k, v) in rows.flatten() {
            map.insert(k, v);
        }
    }
    map
}

fn should_sync_file(
    cursors: &HashMap<String, (i64, i64)>,
    path: &Path,
) -> Option<(String, i64, i64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    let size = meta.len() as i64;
    let key = path.to_string_lossy().into_owned();
    if let Some(&(prev_mtime, prev_size)) = cursors.get(&key)
        && prev_mtime == mtime_ms
        && prev_size == size
    {
        return None;
    }
    Some((key, mtime_ms, size))
}

fn mark_file_synced(
    conn: &Connection,
    key: &str,
    mtime_ms: i64,
    size: i64,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_cursors(file_path, mtime_ms, file_size, updated_ms)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(file_path) DO UPDATE SET
            mtime_ms = excluded.mtime_ms,
            file_size = excluded.file_size,
            updated_ms = excluded.updated_ms",
        params![key, mtime_ms, size, now],
    )?;
    Ok(())
}

fn sync_proxy_sqlite_file(
    conn: &mut Connection,
    cursors: &HashMap<String, (i64, i64)>,
    path: &Path,
    now: i64,
) -> Result<usize> {
    let Some((key, mtime_ms, size)) = should_sync_file(cursors, path) else {
        return Ok(0);
    };
    let src = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    src.busy_timeout(Duration::from_millis(250))?;
    let mut stmt = src.prepare(
        "SELECT record FROM requests WHERE input_tokens IS NOT NULL AND output_tokens IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .flatten()
        .collect::<Vec<_>>();
    let tx = conn.transaction()?;
    let mut count = 0usize;
    for record_json in rows {
        if let Ok(entry) = serde_json::from_str::<Entry>(&record_json)
            && let Some(rec) = record_from_proxy_entry(&entry)
        {
            upsert_spend_record(&tx, &rec)?;
            count += 1;
        }
    }
    mark_file_synced(&tx, &key, mtime_ms, size, now)?;
    tx.commit()?;
    Ok(count)
}

fn sync_codex_jsonl_file(
    tx: &Connection,
    cursors: &HashMap<String, (i64, i64)>,
    path: &Path,
    now: i64,
) -> Result<usize> {
    let Some((key, mtime_ms, size)) = should_sync_file(cursors, path) else {
        return Ok(0);
    };
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("codex_session")
        .to_string();
    let file_mtime = mtime_ms;
    let mut current_model = "gpt-5.4".to_string();
    let mut prev_total_tokens = 0u64;
    let mut event_idx = 0usize;
    let mut records = Vec::new();
    let mut snapshots = Vec::new();

    for line in reader.lines().map_while(Result::ok) {
        if !line.contains("\"turn_context\"")
            && !line.contains("\"token_count\"")
            && !line.contains("\"session_meta\"")
        {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let ts = val["timestamp"]
            .as_str()
            .and_then(parse_rfc3339_ms)
            .unwrap_or(file_mtime);
        let payload = &val["payload"];
        match val["type"].as_str() {
            Some("session_meta") | Some("turn_context") => {
                if let Some(m) = payload["model"].as_str().filter(|s| !s.is_empty()) {
                    current_model = m.to_string();
                }
            }
            Some("event_msg") if payload["type"].as_str() == Some("token_count") => {
                let info = &payload["info"];
                let total = info["total_token_usage"]["total_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                let last = &info["last_token_usage"];
                let input = last["input_tokens"].as_u64().unwrap_or(0);
                let output = last["output_tokens"].as_u64().unwrap_or(0);
                if (input > 0 || output > 0) && (total == 0 || total != prev_total_tokens) {
                    prev_total_tokens = total;
                    event_idx += 1;
                    let cached = last["cached_input_tokens"].as_u64().unwrap_or(0);
                    let writes = last["cache_write_input_tokens"].as_u64().unwrap_or(0);
                    let reasoning = last["reasoning_output_tokens"].as_u64().unwrap_or(0);
                    let canonical_model = canonical_model_name(&current_model);
                    let (provider, is_sub) = if canonical_model.starts_with("gemini") {
                        ("gemini".to_string(), false)
                    } else if canonical_model.starts_with("claude-") {
                        ("claude".to_string(), true)
                    } else {
                        ("codex".to_string(), true)
                    };
                    let billable_writes = writes.min(input.saturating_sub(cached));
                    let (price_model, cost_nano_usd, uncached_equivalent_nano_usd) =
                        compute_token_costs(
                            &canonical_model,
                            input,
                            cached,
                            billable_writes,
                            0,
                            output,
                        );
                    records.push(SpendRecord {
                        record_key: format!("codex_cli:{stem}:{event_idx}"),
                        source: "codex_cli".into(),
                        timestamp_ms: ts,
                        provider,
                        account_id: "default".into(),
                        model: canonical_model,
                        requested_model: Some(current_model.clone()),
                        price_model: Some(price_model),
                        input_tokens: input,
                        cached_input_tokens: cached,
                        cache_write_tokens: writes,
                        cache_write_1h_tokens: 0,
                        output_tokens: output,
                        reasoning_tokens: reasoning,
                        cost_nano_usd,
                        uncached_equivalent_nano_usd,
                        is_subscription: is_sub,
                    });
                }
                let rl = &payload["rate_limits"];
                for (win_key, win_id, win_label, dur_ms) in [
                    ("primary", "five_hour", "Session (5h)", FIVE_HOURS_MS),
                    (
                        "secondary",
                        "seven_day",
                        "Weekly · all models",
                        SEVEN_DAYS_MS,
                    ),
                ] {
                    if let Some(used) = rl[win_key]["used_percent"].as_f64() {
                        let resets_epoch = rl[win_key]["resets_at"].as_i64();
                        let end_ms = resets_epoch.map(|s| s * 1000);
                        let start_ms = end_ms.map(|e| e - dur_ms);
                        snapshots.push((
                            ts,
                            win_id.to_string(),
                            win_label.to_string(),
                            used.clamp(0.0, 100.0),
                            (100.0 - used).clamp(0.0, 100.0),
                            end_ms,
                            start_ms,
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    let count = records.len();
    for rec in &records {
        upsert_spend_record(tx, rec)?;
    }
    if let Some((ts, wid, wlabel, used, rem, end_ms, start_ms)) = snapshots.last() {
        let _ = tx.execute(
            "INSERT INTO subscription_snapshots (
                timestamp_ms, provider, account_id, window_id, window_label,
                used_percent, remaining_percent, resets_at, window_start_ms, window_end_ms
            ) VALUES (?1, 'codex', 'default', ?2, ?3, ?4, ?5, NULL, ?6, ?7)",
            params![ts, wid, wlabel, used, rem, start_ms, end_ms],
        );
    }
    mark_file_synced(tx, &key, mtime_ms, size, now)?;
    Ok(count)
}

fn sync_claude_jsonl_file(
    tx: &Connection,
    cursors: &HashMap<String, (i64, i64)>,
    path: &Path,
    now: i64,
) -> Result<usize> {
    let Some((key, mtime_ms, size)) = should_sync_file(cursors, path) else {
        return Ok(0);
    };
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("claude_session")
        .to_string();
    let mut by_msg_id: HashMap<String, SpendRecord> = HashMap::new();
    let mut fallback_idx = 0usize;

    for line in reader.lines().map_while(Result::ok) {
        if !line.contains("\"usage\"") {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let msg = &val["message"];
        let usage = &msg["usage"];
        if !usage.is_object() {
            continue;
        }
        let model = msg["model"].as_str().unwrap_or("claude-sonnet-4-6");
        if model == "<synthetic>" {
            continue;
        }
        let uncached_in = usage["input_tokens"].as_u64().unwrap_or(0);
        let cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
        let cache_write = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        let cache_write_1h = usage["cache_creation"]["ephemeral_1h_input_tokens"]
            .as_u64()
            .unwrap_or(0);
        let total_in = uncached_in + cache_read + cache_write;
        let output = usage["output_tokens"].as_u64().unwrap_or(0);
        let reasoning = usage["output_tokens_details"]["thinking_tokens"]
            .as_u64()
            .unwrap_or(0);
        if total_in == 0 && output == 0 {
            continue;
        }
        let ts = val["timestamp"]
            .as_str()
            .and_then(parse_rfc3339_ms)
            .unwrap_or(mtime_ms);
        let msg_id = msg["id"].as_str().map(String::from).unwrap_or_else(|| {
            fallback_idx += 1;
            format!("{stem}:{fallback_idx}")
        });
        let (price_model, cost_nano_usd, uncached_equivalent_nano_usd) = compute_token_costs(
            model,
            total_in,
            cache_read,
            cache_write,
            cache_write_1h,
            output,
        );
        by_msg_id.insert(
            msg_id.clone(),
            SpendRecord {
                record_key: format!("claude_cli:{msg_id}"),
                source: "claude_cli".into(),
                timestamp_ms: ts,
                provider: "claude".into(),
                account_id: "default".into(),
                model: model.to_string(),
                requested_model: Some(model.to_string()),
                price_model: Some(price_model),
                input_tokens: total_in,
                cached_input_tokens: cache_read,
                cache_write_tokens: cache_write,
                cache_write_1h_tokens: cache_write_1h,
                output_tokens: output,
                reasoning_tokens: reasoning,
                cost_nano_usd,
                uncached_equivalent_nano_usd,
                is_subscription: true,
            },
        );
    }

    let count = by_msg_id.len();
    for rec in by_msg_id.values() {
        upsert_spend_record(tx, rec)?;
    }
    mark_file_synced(tx, &key, mtime_ms, size, now)?;
    Ok(count)
}

pub fn generate_spend_report(
    db_path: &Path,
    since_ms: Option<i64>,
    window_label: &str,
    live_usages: &[AccountUsage],
) -> Result<SpendReport> {
    let conn = open_connection(db_path)?;
    for usage in live_usages {
        if usage.reading.state == State::Ok
            && usage
                .reading
                .data
                .as_ref()
                .is_some_and(|d| !d.windows.is_empty())
        {
            let _ = insert_subscription_snapshot(&conn, usage);
        }
    }

    let cutoff = since_ms.unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT provider, model, COALESCE(price_model, model),
                COUNT(*),
                SUM(input_tokens),
                SUM(cached_input_tokens),
                SUM(cache_write_tokens),
                SUM(output_tokens),
                SUM(reasoning_tokens),
                SUM(cost_nano_usd),
                SUM(uncached_equivalent_nano_usd)
         FROM spend_ledger
         WHERE timestamp_ms >= ?1
         GROUP BY provider, model, COALESCE(price_model, model)
         ORDER BY SUM(cost_nano_usd) DESC",
    )?;

    let mut by_model = Vec::new();
    let mut provider_map: HashMap<String, ProviderSpendSummary> = HashMap::new();
    let mut total_requests = 0u64;
    let mut total_input_tokens = 0u64;
    let mut total_cached_input_tokens = 0u64;
    let mut total_cache_write_tokens = 0u64;
    let mut total_output_tokens = 0u64;
    let mut total_reasoning_tokens = 0u64;
    let mut total_cost_nano = 0i128;
    let mut total_uncached_nano = 0i128;

    let rows = stmt.query_map([cutoff], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)? as u64,
            r.get::<_, i64>(4)? as u64,
            r.get::<_, i64>(5)? as u64,
            r.get::<_, i64>(6)? as u64,
            r.get::<_, i64>(7)? as u64,
            r.get::<_, i64>(8)? as u64,
            r.get::<_, i64>(9)?,
            r.get::<_, i64>(10)?,
        ))
    })?;

    for row in rows {
        let (
            provider,
            model,
            price_model,
            reqs,
            inp,
            cached,
            writes,
            out,
            reas,
            cost_nano,
            uncached_nano,
        ) = row?;
        let cost_usd = cost_nano as f64 / 1e9;
        let uncached_usd = uncached_nano as f64 / 1e9;
        let savings_usd = (uncached_usd - cost_usd).max(0.0);
        let tok_sum = inp + out;

        total_requests += reqs;
        total_input_tokens += inp;
        total_cached_input_tokens += cached;
        total_cache_write_tokens += writes;
        total_output_tokens += out;
        total_reasoning_tokens += reas;
        total_cost_nano += i128::from(cost_nano);
        total_uncached_nano += i128::from(uncached_nano);

        by_model.push(ModelSpendSummary {
            provider: provider.clone(),
            model,
            price_model,
            requests: reqs,
            input_tokens: inp,
            cached_input_tokens: cached,
            cache_write_tokens: writes,
            output_tokens: out,
            reasoning_tokens: reas,
            total_tokens: tok_sum,
            cost_usd,
            uncached_cost_usd: uncached_usd,
            cache_savings_usd: savings_usd,
            share_percent: 0.0,
        });

        let prov = provider_map
            .entry(provider.clone())
            .or_insert(ProviderSpendSummary {
                provider,
                requests: 0,
                input_tokens: 0,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                reasoning_tokens: 0,
                total_tokens: 0,
                cost_usd: 0.0,
                uncached_cost_usd: 0.0,
                cache_savings_usd: 0.0,
                share_percent: 0.0,
            });
        prov.requests += reqs;
        prov.input_tokens += inp;
        prov.cached_input_tokens += cached;
        prov.cache_write_tokens += writes;
        prov.output_tokens += out;
        prov.reasoning_tokens += reas;
        prov.total_tokens += tok_sum;
        prov.cost_usd += cost_usd;
        prov.uncached_cost_usd += uncached_usd;
        prov.cache_savings_usd += savings_usd;
    }

    let total_cost_usd = total_cost_nano as f64 / 1e9;
    let total_uncached_cost_usd = total_uncached_nano as f64 / 1e9;
    let total_cache_savings_usd = (total_uncached_cost_usd - total_cost_usd).max(0.0);

    for m in &mut by_model {
        if total_cost_usd > 0.0 {
            m.share_percent = (m.cost_usd / total_cost_usd) * 100.0;
        }
    }
    let mut by_provider: Vec<ProviderSpendSummary> = provider_map.into_values().collect();
    for p in &mut by_provider {
        if total_cost_usd > 0.0 {
            p.share_percent = (p.cost_usd / total_cost_usd) * 100.0;
        }
    }
    by_provider.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let now = now_ms();
    let mut subscription_yield = Vec::new();
    for provider in ["codex", "claude"] {
        let live = live_usages.iter().find(|u| u.account.provider == provider);
        let spend_24h = query_provider_spend_between(&conn, provider, now - DAY_MS, now)?.2;
        let (_, _, spend_7d, uncached_7d) =
            query_provider_spend_between(&conn, provider, now - SEVEN_DAYS_MS, now)?;
        let spend_30d = query_provider_spend_between(&conn, provider, now - 30 * DAY_MS, now)?.2;
        let spend_all = query_provider_spend_between(&conn, provider, 0, now)?.2;

        if live.is_none() && spend_all <= 0.0 {
            continue;
        }
        let state = live
            .map(|u| format!("{:?}", u.reading.state).to_ascii_lowercase())
            .unwrap_or_else(|| "historical".into());
        let windows_src: Vec<Window> = live
            .and_then(|u| u.reading.data.as_ref().map(|d| d.windows.clone()))
            .filter(|w| !w.is_empty())
            .unwrap_or_else(|| latest_snapshot_windows(&conn, provider));

        let mut yield_windows = Vec::new();
        for w in windows_src {
            let end_ms = w.resets_at.as_deref().and_then(parse_rfc3339_ms);
            let dur_ms =
                if w.id == "five_hour" || w.id.contains("five_hour") || w.id.contains("session") {
                    FIVE_HOURS_MS
                } else {
                    SEVEN_DAYS_MS
                };
            let start_ms = end_ms
                .map(|e| e - dur_ms)
                .filter(|&s| s < now - 60_000)
                .unwrap_or(now - dur_ms);
            let (w_reqs, w_tokens, w_spend, w_uncached) =
                query_provider_spend_between(&conn, provider, start_ms, now)?;
            let used = w.used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
            let rem = w
                .remaining_percent
                .unwrap_or(100.0 - used)
                .clamp(0.0, 100.0);
            let (usd_per_pct, implied_full, est_remaining) = if used > 0.05 && w_spend > 0.0 {
                let per_pct = w_spend / used;
                (Some(per_pct), Some(per_pct * 100.0), Some(per_pct * rem))
            } else {
                (None, None, None)
            };
            yield_windows.push(WindowYieldEstimate {
                window_id: w.id,
                label: w.label,
                used_percent: used,
                remaining_percent: rem,
                resets_at: w.resets_at,
                window_start_ms: Some(start_ms),
                window_end_ms: end_ms,
                window_requests: w_reqs,
                window_tokens: w_tokens,
                window_spend_usd: w_spend,
                window_uncached_usd: w_uncached,
                usd_per_one_percent: usd_per_pct,
                implied_full_window_usd: implied_full,
                estimated_remaining_usd: est_remaining,
            });
        }

        let projected_monthly = if spend_7d > 0.0 {
            spend_7d * (30.0 / 7.0)
        } else {
            spend_30d
        };

        subscription_yield.push(SubscriptionYieldReport {
            provider: provider.to_string(),
            account_id: "default".into(),
            state,
            spend_24h_usd: spend_24h,
            spend_7d_usd: spend_7d,
            spend_30d_usd: spend_30d,
            spend_all_time_usd: spend_all,
            uncached_7d_usd: uncached_7d,
            projected_monthly_spend_usd: projected_monthly,
            windows: yield_windows,
        });
    }

    Ok(SpendReport {
        schema_version: SCHEMA_VERSION,
        database_path: db_path.display().to_string(),
        window_label: window_label.to_string(),
        since_timestamp_ms: since_ms,
        generated_at_ms: now,
        total_requests,
        total_input_tokens,
        total_cached_input_tokens,
        total_cache_write_tokens,
        total_output_tokens,
        total_reasoning_tokens,
        total_tokens: total_input_tokens + total_output_tokens,
        total_cost_usd,
        total_uncached_cost_usd,
        total_cache_savings_usd,
        by_provider,
        by_model,
        subscription_yield,
    })
}

fn query_provider_spend_between(
    conn: &Connection,
    provider: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<(u64, u64, f64, f64)> {
    let row = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(input_tokens + output_tokens), 0),
                COALESCE(SUM(cost_nano_usd), 0),
                COALESCE(SUM(uncached_equivalent_nano_usd), 0)
         FROM spend_ledger
         WHERE provider = ?1 AND timestamp_ms >= ?2 AND timestamp_ms <= ?3",
        params![provider, start_ms, end_ms],
        |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as f64 / 1e9,
                r.get::<_, i64>(3)? as f64 / 1e9,
            ))
        },
    )?;
    Ok(row)
}

fn latest_snapshot_windows(conn: &Connection, provider: &str) -> Vec<Window> {
    let mut stmt = match conn.prepare(
        "SELECT window_id, window_label, used_percent, remaining_percent, resets_at
         FROM subscription_snapshots
         WHERE provider = ?1
         GROUP BY window_id
         HAVING id = MAX(id)
         ORDER BY window_id",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map([provider], |r| {
        Ok(Window {
            id: r.get(0)?,
            label: r.get(1)?,
            group: None,
            used_percent: Some(r.get(2)?),
            remaining_percent: Some(r.get(3)?),
            resets_at: r.get(4)?,
        })
    })
    .ok()
    .into_iter()
    .flatten()
    .flatten()
    .collect()
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.2}B", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

pub fn render_spend_report(report: &SpendReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Spend Monitor ({}) · DB: {}",
        report.window_label, report.database_path
    );
    let _ = writeln!(
        out,
        "Total Spend: ${:.2}",
        report.total_cost_usd
    );
    let _ = writeln!(
        out,
        "Volume: {} turns/requests · {} tokens ({} in, {} cache read, {} cache write, {} out)",
        report.total_requests,
        fmt_tokens(report.total_tokens),
        fmt_tokens(report.total_input_tokens),
        fmt_tokens(report.total_cached_input_tokens),
        fmt_tokens(report.total_cache_write_tokens),
        fmt_tokens(report.total_output_tokens)
    );

    if !report.by_provider.is_empty() {
        let _ = writeln!(out, "\nBy Provider:");
        for p in &report.by_provider {
            let _ = writeln!(
                out,
                "- {:<8} ${:>10.2} ({:>5.1}%) · {:>7} reqs · {:>8} tok (in {}, cached {}, write {}, out {})",
                p.provider,
                p.cost_usd,
                p.share_percent,
                p.requests,
                fmt_tokens(p.total_tokens),
                fmt_tokens(p.input_tokens),
                fmt_tokens(p.cached_input_tokens),
                fmt_tokens(p.cache_write_tokens),
                fmt_tokens(p.output_tokens),
            );
        }
    }

    if !report.by_model.is_empty() {
        let _ = writeln!(out, "\nBy Model:");
        for m in &report.by_model {
            let price_tag = if m.price_model != m.model {
                format!(" [priced as {}]", m.price_model)
            } else {
                String::new()
            };
            let _ = writeln!(
                out,
                "- {:<8} {:<26}{} ${:>10.2} ({:>5.1}%) · {:>7} reqs · {:>8} tok (in {}, cached {}, write {}, out {})",
                m.provider,
                m.model,
                price_tag,
                m.cost_usd,
                m.share_percent,
                m.requests,
                fmt_tokens(m.total_tokens),
                fmt_tokens(m.input_tokens),
                fmt_tokens(m.cached_input_tokens),
                fmt_tokens(m.cache_write_tokens),
                fmt_tokens(m.output_tokens)
            );
        }
    }

    if !report.subscription_yield.is_empty() {
        let _ = writeln!(out, "\nSubscription Value & Quota Yield:");
        for sub in &report.subscription_yield {
            let _ = writeln!(
                out,
                "- {}/{} ({}) · Spend: 24h ${:.2} · 7d ${:.2} · 30d ${:.2} · All-time ${:.2} · Pace ~${:.2}/mo",
                sub.provider,
                sub.account_id,
                sub.state,
                sub.spend_24h_usd,
                sub.spend_7d_usd,
                sub.spend_30d_usd,
                sub.spend_all_time_usd,
                sub.projected_monthly_spend_usd
            );
            for w in &sub.windows {
                let yield_detail = match (
                    w.usd_per_one_percent,
                    w.implied_full_window_usd,
                    w.estimated_remaining_usd,
                ) {
                    (Some(per_pct), Some(full), Some(rem_usd)) => format!(
                        " · Yield: ${:.2}/1% quota (~${:.2} per 100% window, ~${:.2} remaining)",
                        per_pct, full, rem_usd
                    ),
                    _ => format!(
                        " · Active window spend: ${:.2} at {:.1}% used",
                        w.window_spend_usd, w.used_percent
                    ),
                };
                let _ = writeln!(
                    out,
                    "    Window {}: {:.1}% left ({:.1}% used) · {} reqs ({} tok) · ${:.2} in window{}",
                    w.label,
                    w.remaining_percent,
                    w.used_percent,
                    w.window_requests,
                    fmt_tokens(w.window_tokens),
                    w.window_spend_usd,
                    yield_detail
                );
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_token_costs_and_uncached_savings_accurately() {
        let (price_model, cost_nano, uncached_nano) =
            compute_token_costs("gpt-5.4", 100_000, 80_000, 0, 0, 10_000);
        assert_eq!(price_model, "gpt-5.4");
        // Standard context (<=272k): 20k * $2.5/M ($0.05) + 80k * $0.25/M ($0.02) + 10k * $15/M ($0.15) = $0.22
        assert_eq!(cost_nano, 220_000_000);
        // Uncached equivalent: 100k * $2.5/M ($0.25) + 10k * $15/M ($0.15) = $0.40
        assert_eq!(uncached_nano, 400_000_000);

        assert_eq!(canonical_model_name("ultima-alpha"), "gpt-6-astra");
        assert_eq!(canonical_model_name("ultima"), "gpt-6-astra");
        // 100k input (80k cached @ $1/M = $0.08, 10k write @ $12.50/M = $0.125, 10k uncached @ $10/M = $0.10)
        // + 10k output @ $50/M = $0.50 -> Total $0.805 = 805_000_000 nano USD
        let (astra_model, astra_cost, _) =
            compute_token_costs("ultima-alpha", 100_000, 80_000, 10_000, 0, 10_000);
        assert_eq!(astra_model, "gpt-6-astra");
        assert_eq!(astra_cost, 805_000_000);
    }

    #[tokio::test]
    async fn async_spend_tracker_persists_records_and_calculates_subscription_yield() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("spend.sqlite3");
        let tracker = SpendTracker::open(db_path.clone()).unwrap();

        let now = now_ms();
        let entry = Entry {
            id: 1,
            request_id: "req-codex-1".into(),
            timestamp_ms: now as u64,
            method: "POST".into(),
            path: "/v1/responses".into(),
            project: Some("codex".into()),
            routed_model: Some("gpt-5.4".into()),
            input_tokens: Some(100_000),
            cached_input_tokens: Some(50_000),
            output_tokens: Some(20_000),
            ..Entry::default()
        };
        tracker.record_entry(&entry);
        tracker.flush().await;

        let live = AccountUsage {
            schema_version: 1,
            account: hey_proxy::usage::Account {
                provider: "codex".into(),
                id: "default".into(),
            },
            reading: hey_proxy::usage::Reading {
                state: State::Ok,
                updated_at: Some(1_790_950_000),
                data: Some(hey_proxy::usage::UsageData {
                    windows: vec![Window {
                        id: "seven_day".into(),
                        label: "Weekly · all models".into(),
                        group: None,
                        used_percent: Some(10.0),
                        remaining_percent: Some(90.0),
                        resets_at: None,
                    }],
                    extra_usage: None,
                }),
                error: None,
                retry_after_seconds: Some(60),
            },
        };

        let report = generate_spend_report(&db_path, None, "All time", &[live]).unwrap();
        assert_eq!(report.total_requests, 1);
        assert!((report.total_cost_usd - 0.4375).abs() < 1e-6);
        assert_eq!(report.by_provider[0].provider, "codex");
        let sub = &report.subscription_yield[0];
        let win = &sub.windows[0];
        assert!((win.window_spend_usd - 0.4375).abs() < 1e-6);
        assert!((win.implied_full_window_usd.unwrap() - 4.375).abs() < 1e-4);
    }
}
