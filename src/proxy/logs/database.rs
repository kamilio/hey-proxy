use super::{
    pricing,
    store::{Entry, now_ms},
};
use crate::config::Logging;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

pub struct Event {
    pub entry: Entry,
    pub kind: String,
    pub details: Value,
    pub timestamp_ms: u64,
}
enum Command {
    Event(Box<Event>),
    Flush(tokio::sync::oneshot::Sender<()>),
}
#[derive(Default)]
struct Health {
    enqueued: AtomicU64,
    committed: AtomicU64,
    pending: AtomicUsize,
    dropped: AtomicU64,
    write_errors: AtomicU64,
    last_commit_ms: AtomicU64,
    last_drop_ms: AtomicU64,
    commit_duration_us: AtomicU64,
    last_error: Mutex<Option<String>>,
}
pub struct Database {
    pub path: PathBuf,
    sender: SyncSender<Command>,
    health: Arc<Health>,
    capacity: usize,
    readers: Arc<tokio::sync::Semaphore>,
    closed: Arc<AtomicBool>,
    started_ms: u64,
    worker: std::thread::Thread,
    batch_size: usize,
}

impl Database {
    pub fn open(path: PathBuf, config: &Logging, session_id: &str) -> Result<Self> {
        let parent = path.parent().context("Database path must have a parent")?;
        if !parent.exists() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(parent)?;
        }
        let private_file = |path: &std::path::Path| -> Result<File> {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(file)
        };
        let lock = private_file(&path.with_extension("sqlite3.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock).context("Logging database is already owned by another proxy; use a separate database for previews")?;
        private_file(&path)?;
        let mut connection = Connection::open(&path).context("Open logging database")?;
        connection.busy_timeout(Duration::from_millis(250))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA wal_autocheckpoint=1000;")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > 1 {
            bail!("Logging database schema {version} is newer than this executable supports");
        }
        connection.execute_batch(SCHEMA)?;
        repair_completed_disconnects(&mut connection)?;
        let transaction = connection.transaction()?;
        let timestamp = integer(now_ms());
        transaction.execute("INSERT INTO request_events(request_id,timestamp_ms,kind,details)
            SELECT request_id,?1,'interrupted','{\"source\":\"process_restart\",\"error_code\":\"proxy_process_ended\"}' FROM requests WHERE ended_ms IS NULL",[timestamp])?;
        transaction.execute("UPDATE requests SET state='interrupted', ended_ms=?1, updated_ms=?1,
            total_duration_ms=NULL, error_code='proxy_process_ended',
            record=json_set(record,'$.state','interrupted','$.ended_ms',?1,'$.updated_ms',?1,
                '$.total_duration_ms',NULL,'$.error_code','proxy_process_ended','$.outcome_source','process_restart')
            WHERE ended_ms IS NULL", [timestamp])?;
        transaction.execute(
            "INSERT INTO sessions(session_id,started_ms) VALUES(?1,?2)",
            params![session_id, timestamp],
        )?;
        transaction.commit()?;
        let health = Arc::new(Health::default());
        let historical_drops = connection
            .query_row(
                "SELECT value FROM metadata WHERE key='dropped_events'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        health.dropped.store(historical_drops, Ordering::Relaxed);
        let (sender, receiver) = mpsc::sync_channel(config.queue_capacity);
        let worker_health = health.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let worker_closed = closed.clone();
        let config = config.clone();
        let capacity = config.queue_capacity;
        let batch_size = config.batch_size;
        let worker = std::thread::Builder::new()
            .name("proxy-log-writer".into())
            .spawn(move || {
                let _database_lock = lock;
                writer(connection, receiver, worker_health, config, worker_closed);
            })
            .context("Start logging writer")?
            .thread()
            .clone();
        Ok(Self {
            path,
            sender,
            health,
            capacity,
            readers: Arc::new(tokio::sync::Semaphore::new(2)),
            closed,
            started_ms: now_ms(),
            worker,
            batch_size,
        })
    }
    /// The forwarding path never blocks on a queue slot, SQLite, or disk.
    pub fn enqueue(&self, event: Event) {
        let pending = self.health.pending.fetch_add(1, Ordering::Relaxed) + 1;
        match self.sender.try_send(Command::Event(Box::new(event))) {
            Ok(()) => {
                self.health.enqueued.fetch_add(1, Ordering::Relaxed);
                // Wake for the first event or a full batch, not every lifecycle update.
                if pending == 1 || pending.is_multiple_of(self.batch_size) {
                    self.worker.unpark();
                }
            }
            Err(_) => {
                self.health.pending.fetch_sub(1, Ordering::Relaxed);
                self.health.dropped.fetch_add(1, Ordering::Relaxed);
                self.health.last_drop_ms.store(now_ms(), Ordering::Relaxed);
            }
        }
    }
    pub async fn flush(&self) -> Result<()> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut command = Command::Flush(sender);
            loop {
                match self.sender.try_send(command) {
                    Ok(()) => {
                        self.worker.unpark();
                        break;
                    }
                    Err(mpsc::TrySendError::Full(returned)) => {
                        command = returned;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => bail!("Logging writer stopped"),
                }
            }
            receiver.await.context("Logging writer stopped")
        })
        .await
        .context("Logging flush timed out")??;
        Ok(())
    }
    pub fn health(&self) -> Value {
        let h = &self.health;
        let last_commit = h.last_commit_ms.load(Ordering::Relaxed);
        let pending = h.pending.load(Ordering::Relaxed);
        let last_error = h
            .last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        json!({"enabled":true,"storage":"sqlite","journal_mode":"WAL","synchronous":"FULL",
            "queue_capacity":self.capacity,"pending_events":pending,"enqueued_events":h.enqueued.load(Ordering::Relaxed),
            "committed_events":h.committed.load(Ordering::Relaxed),"dropped_events":h.dropped.load(Ordering::Relaxed),
            "last_drop_ms":h.last_drop_ms.load(Ordering::Relaxed),"write_errors":h.write_errors.load(Ordering::Relaxed),
            "last_error":last_error,"last_commit_ms":last_commit,"commit_duration_us":h.commit_duration_us.load(Ordering::Relaxed),
            "lag_ms":if pending > 0 {now_ms().saturating_sub(last_commit.max(self.started_ms))} else {0},
            "retention":"all","status":if last_error.is_some(){"error"}else if h.dropped.load(Ordering::Relaxed)>0{"gaps"}else if pending>self.capacity/2{"lagging"}else{"healthy"}})
    }
    pub async fn read<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = self
            .readers
            .clone()
            .try_acquire_owned()
            .context("Historical reports are busy; retry shortly")?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let connection = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            connection.busy_timeout(Duration::from_millis(500))?;
            connection.execute_batch(
                "PRAGMA query_only=ON; PRAGMA temp_store=FILE; PRAGMA cache_size=-4096;",
            )?;
            let started = Instant::now();
            connection.progress_handler(
                10_000,
                Some(move || started.elapsed() > Duration::from_secs(10)),
            )?;
            connection.execute_batch("BEGIN")?;
            let result = operation(&connection);
            let _ = connection.execute_batch("ROLLBACK");
            result
        })
        .await
        .context("Historical query worker stopped")?
    }
}

// Version-one logging mistook Codex's close after response.completed for a
// cancelled generation. Repair only records whose terminal event proves success,
// leaving an audit event and preserving all original events, usage and costs.
fn repair_completed_disconnects(connection: &mut Connection) -> Result<()> {
    let done: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='completed_disconnect_fix_v1')",
        [],
        |r| r.get(0),
    )?;
    if done {
        return Ok(());
    }
    let transaction = connection.transaction()?;
    let condition="r.state='cancelled' AND r.error_code='client_disconnected' AND r.status<400
        AND json_extract(r.record,'$.streaming')=1
        AND (SELECT json_extract(e.details,'$.type') FROM request_events e
            WHERE e.request_id=r.request_id AND e.kind='response_event'
            AND json_extract(e.details,'$.type') IN ('response.completed','proxy.stream.done','response.failed','response.incomplete','error')
            ORDER BY e.seq DESC LIMIT 1) IN ('response.completed','proxy.stream.done')";
    transaction.execute(&format!("INSERT INTO request_events(request_id,timestamp_ms,kind,details)
        SELECT r.request_id,?1,'classification_corrected','{{\"previous_state\":\"cancelled\",\"state\":\"succeeded\",\"reason\":\"client_closed_after_completed_event\"}}' FROM requests r WHERE {condition}"),[integer(now_ms())])?;
    let corrected=transaction.execute(&format!("UPDATE requests AS r SET state='succeeded',error_code=NULL,
        record=json_set(record,'$.state','succeeded','$.error_code',NULL,'$.outcome_source','responses_event') WHERE {condition}"),[])?;
    transaction.execute(
        "INSERT INTO metadata(key,value) VALUES('completed_disconnect_fix_v1',?1)",
        [corrected.to_string()],
    )?;
    transaction.commit()?;
    if corrected > 0 {
        eprintln!(
            "Corrected {corrected} completed responses previously classified as client cancellations"
        );
    }
    Ok(())
}

impl Drop for Database {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        self.worker.unpark();
    }
}

fn writer(
    mut connection: Connection,
    receiver: Receiver<Command>,
    health: Arc<Health>,
    config: Logging,
    closed: Arc<AtomicBool>,
) {
    let mut batch = Vec::with_capacity(config.batch_size);
    let mut flushes = Vec::new();
    let interval = Duration::from_millis(config.flush_interval_ms);
    let mut disconnected = false;
    let mut failure_since = None;
    let mut recorded_drops = health.dropped.load(Ordering::Relaxed);
    let mut deadline = Instant::now() + interval;
    loop {
        while batch.len() < config.batch_size && !disconnected && flushes.is_empty() {
            match receiver.try_recv() {
                Ok(Command::Event(event)) => batch.push(event),
                Ok(Command::Flush(sender)) => flushes.push(sender),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => disconnected = true,
            }
        }
        let drops = health.dropped.load(Ordering::Relaxed);
        if !disconnected
            && flushes.is_empty()
            && batch.len() < config.batch_size
            && recorded_drops == drops
            && (batch.is_empty() || Instant::now() < deadline)
        {
            std::thread::park_timeout(if batch.is_empty() {
                interval
            } else {
                deadline.saturating_duration_since(Instant::now())
            });
            if batch.is_empty() {
                deadline = Instant::now() + interval;
            }
            continue;
        }
        if !batch.is_empty() || recorded_drops != drops {
            let started = Instant::now();
            match commit(&mut connection, &batch, drops) {
                Ok(()) => {
                    health.pending.fetch_sub(batch.len(), Ordering::Relaxed);
                    health
                        .committed
                        .fetch_add(batch.len() as u64, Ordering::Relaxed);
                    health.last_commit_ms.store(now_ms(), Ordering::Relaxed);
                    health
                        .commit_duration_us
                        .store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                    *health.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    failure_since = None;
                    recorded_drops = drops;
                    batch.clear();
                }
                Err(error) => {
                    health.write_errors.fetch_add(1, Ordering::Relaxed);
                    *health.last_error.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(error.to_string());
                    let since = failure_since.get_or_insert_with(Instant::now);
                    if (disconnected || closed.load(Ordering::Relaxed))
                        && since.elapsed() > Duration::from_secs(5)
                    {
                        eprintln!(
                            "Logging writer could not flush {} events: {error}",
                            batch.len()
                        );
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
            }
        }
        for sender in flushes.drain(..) {
            let _ = sender.send(());
        }
        if disconnected {
            break;
        }
        deadline = Instant::now() + interval;
    }
    let _ = connection.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
}

fn commit(connection: &mut Connection, batch: &[Box<Event>], drops: u64) -> Result<()> {
    let transaction = connection.transaction()?;
    {
        let mut request = transaction.prepare_cached(UPSERT)?;
        let mut event = transaction.prepare_cached(
            "INSERT INTO request_events(request_id,timestamp_ms,kind,details) VALUES(?1,?2,?3,?4)",
        )?;
        // Only the last snapshot of each request is needed, but keep every timeline event.
        let mut latest = std::collections::HashMap::new();
        for item in batch {
            latest.insert(&item.entry.request_id, item);
        }
        for item in latest.values() {
            let entry = &item.entry;
            let price = pricing::price(entry);
            let mut record = serde_json::to_value(entry)?;
            record["price_model"] = json!(price.price_model);
            record["price_version"] = json!(price.price_version);
            record["cost_nano_usd"] = json!(price.cost_nano_usd);
            record["estimated_cost_usd"] = json!(price.cost_nano_usd.map(|n| n as f64 / 1e9));
            request.execute(params![
                entry.request_id,
                entry.session_id,
                integer(entry.timestamp_ms),
                integer(entry.updated_ms),
                entry.ended_ms.map(integer),
                entry.requested_model,
                entry.routed_model,
                entry.project,
                entry.path,
                entry.method,
                entry.transport,
                entry.mode,
                entry.state,
                entry.status,
                entry.retries,
                entry.input_tokens.map(integer),
                entry.output_tokens.map(integer),
                entry.cached_input_tokens.map(integer),
                entry.cache_write_tokens.map(integer),
                entry.reasoning_tokens.map(integer),
                entry.duration_ms.map(integer),
                entry.total_duration_ms.map(integer),
                entry.first_byte_ms.map(integer),
                entry.first_output_ms.map(integer),
                integer(entry.request_bytes),
                integer(entry.response_bytes),
                entry.error_code,
                price.cost_nano_usd,
                price.price_version,
                price.price_model,
                serde_json::to_string(&record)?
            ])?;
        }
        for item in batch {
            event.execute(params![
                item.entry.request_id,
                integer(item.timestamp_ms),
                item.kind,
                serde_json::to_string(&item.details)?
            ])?;
        }
    }
    transaction.execute("INSERT INTO metadata(key,value) VALUES('dropped_events',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[drops.to_string()])?;
    transaction.commit()?;
    Ok(())
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS requests (
    seq INTEGER PRIMARY KEY, request_id TEXT NOT NULL UNIQUE, session_id TEXT NOT NULL,
    timestamp_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL, ended_ms INTEGER,
    requested_model TEXT, routed_model TEXT, project TEXT, path TEXT NOT NULL, method TEXT NOT NULL,
    transport TEXT NOT NULL, mode TEXT NOT NULL, state TEXT NOT NULL, status INTEGER, retries INTEGER NOT NULL,
    input_tokens INTEGER, output_tokens INTEGER, cached_input_tokens INTEGER, cache_write_tokens INTEGER,
    reasoning_tokens INTEGER, duration_ms INTEGER, total_duration_ms INTEGER, first_byte_ms INTEGER,
    first_output_ms INTEGER, request_bytes INTEGER, response_bytes INTEGER, error_code TEXT,
    cost_nano_usd INTEGER, price_version TEXT, price_model TEXT, record TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS requests_time ON requests(timestamp_ms DESC,request_id DESC);
CREATE INDEX IF NOT EXISTS requests_requested_time ON requests(requested_model,timestamp_ms);
CREATE INDEX IF NOT EXISTS requests_routed_time ON requests(routed_model,timestamp_ms);
CREATE INDEX IF NOT EXISTS requests_project_time ON requests(project,timestamp_ms);
CREATE INDEX IF NOT EXISTS requests_state_time ON requests(state,timestamp_ms);
CREATE TABLE IF NOT EXISTS request_events(seq INTEGER PRIMARY KEY, request_id TEXT NOT NULL REFERENCES requests(request_id), timestamp_ms INTEGER NOT NULL, kind TEXT NOT NULL, details TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS events_request ON request_events(request_id,seq);
CREATE TABLE IF NOT EXISTS sessions(session_id TEXT PRIMARY KEY,started_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
PRAGMA user_version=1;";

const UPSERT: &str = "INSERT INTO requests(request_id,session_id,timestamp_ms,updated_ms,ended_ms,
requested_model,routed_model,project,path,method,transport,mode,state,status,retries,input_tokens,output_tokens,
cached_input_tokens,cache_write_tokens,reasoning_tokens,duration_ms,total_duration_ms,first_byte_ms,first_output_ms,
request_bytes,response_bytes,error_code,cost_nano_usd,price_version,price_model,record)
VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31)
ON CONFLICT(request_id) DO UPDATE SET updated_ms=excluded.updated_ms,ended_ms=excluded.ended_ms,
requested_model=excluded.requested_model,routed_model=excluded.routed_model,project=excluded.project,path=excluded.path,
transport=excluded.transport,state=excluded.state,status=excluded.status,retries=excluded.retries,input_tokens=excluded.input_tokens,
output_tokens=excluded.output_tokens,cached_input_tokens=excluded.cached_input_tokens,cache_write_tokens=excluded.cache_write_tokens,
reasoning_tokens=excluded.reasoning_tokens,duration_ms=excluded.duration_ms,total_duration_ms=excluded.total_duration_ms,
first_byte_ms=excluded.first_byte_ms,first_output_ms=excluded.first_output_ms,request_bytes=excluded.request_bytes,
response_bytes=excluded.response_bytes,error_code=excluded.error_code,cost_nano_usd=excluded.cost_nano_usd,
price_version=excluded.price_version,price_model=excluded.price_model,record=excluded.record";

fn integer(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}
