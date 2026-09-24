use super::*;
use super::{
    query::Query,
    store::{Entry, now_ms},
};
use axum::body::to_bytes;
use rusqlite::Connection;
use std::collections::HashSet;

fn fixture() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(Store::open(&Config::test_fixture(), &dir.path().join("config.json")).unwrap());
    (dir, store)
}
fn all() -> Query {
    Query {
        start: Some(0),
        end: Some(now_ms() + 60000),
        ..Query::default()
    }
}
fn sample(store: &Store, state: &str) -> u64 {
    let id = store.begin("POST", "/v1/responses", "HTTP");
    store.route(
        id,
        Some("gpt-5.4".into()),
        Some("model-primary".into()),
        "test",
    );
    store.usage(id,&json!({"usage":{"input_tokens":100000,"output_tokens":10000,"input_tokens_details":{"cached_tokens":60000,"cache_write_tokens":20000}}}));
    store.finish(id, if state == "failed" { 503 } else { 200 });
    store.complete(id, state, "test", None, 42);
    id
}

#[tokio::test]
async fn dashboard_window_caps_are_explicit_and_keep_newest_records() {
    let (_dir, store) = fixture();
    let connection = Connection::open(&store.database.as_ref().unwrap().path).unwrap();
    connection.execute_batch("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<50001)
        INSERT INTO requests(request_id,session_id,timestamp_ms,updated_ms,path,method,transport,mode,state,retries,record)
        SELECT 'fixture-'||i,'fixture',i,i,'/v1/responses','POST','HTTP','standalone','succeeded',0,'{}' FROM n;").unwrap();
    let value = query::window(&connection, 0, 60000).unwrap();
    assert_eq!(value["coverage"]["truncated"], true);
    let entries = value["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 50000);
    assert_eq!(entries[0]["timestamp_ms"], 50001);
    assert_eq!(entries.last().unwrap()["timestamp_ms"], 2);
    assert_eq!(entries[0]["route_rule"], Value::Null);
    let narrow = query::window(&connection, 50000, 50002).unwrap();
    assert_eq!(narrow["entries"].as_array().unwrap().len(), 2);
    assert_eq!(narrow["coverage"]["truncated"], false);
}

#[tokio::test]
async fn dashboard_windows_read_archive_merge_live_and_change_ranges() {
    let (_dir, store) = fixture();
    let now = now_ms();
    for i in 0..650 {
        let id = sample(&store, "succeeded");
        store.update(id, "test_timestamp", json!({}), |e| {
            e.timestamp_ms = now - if i < 100 { 7_200_000 } else { 1_800_000 };
            true
        });
    }
    store.flush().await.unwrap();
    let connection = Connection::open(&store.database.as_ref().unwrap().path).unwrap();
    connection
        .execute(
            "UPDATE requests SET timestamp_ms=json_extract(record,'$.timestamp_ms')",
            [],
        )
        .unwrap();
    drop(connection);
    let app = router_with(
        Config::test_fixture(),
        Options {
            logs: Some(store.clone()),
            ..Options::default()
        },
    )
    .unwrap();
    let (url, task) = serve(app).await;
    let client = reqwest::Client::new();
    for (minutes, expected) in [(15, 0), (60, 550), (1440, 650), (15, 0)] {
        let value: Value = client
            .get(format!("{url}/logs/api?local=true&minutes={minutes}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(value["entries"].as_array().unwrap().len(), expected);
        assert_eq!(value["machines"][0]["coverage"]["source"], "sqlite");
        assert_eq!(value["machines"][0]["coverage"]["truncated"], false);
    }
    // A new request appears even while the archive snapshot is cached, only once.
    sample(&store, "succeeded");
    for _ in 0..2 {
        let value: Value = client
            .get(format!("{url}/logs/api?local=true&minutes=15"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(value["entries"].as_array().unwrap().len(), 1);
    }
    assert_eq!(
        client
            .get(format!("{url}/logs/api?minutes=999999"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    task.abort();
}

#[tokio::test]
async fn reasoning_evidence_survives_storage_and_effort_override() {
    let (_dir, store) = fixture();
    let config = Config {
        aliases: vec![serde_json::from_value(json!({"from":"gpt-5.4","to":"model-primary","reasoning":"high","reasoning_routes":{"low":{"to":"model-secondary"}}})).unwrap()],
        ..Config::test_fixture()
    };
    for path in ["/v1/responses", "/v1/chat/completions"] {
        for effort in [None, Some("low"), Some("medium"), Some("high")] {
            let mut input = json!({"model":"gpt-5.4","input":"private test content"});
            if let Some(effort) = effort {
                if path.ends_with("responses") {
                    input["reasoning"] = json!({"effort":effort});
                } else {
                    input["reasoning_effort"] = json!(effort);
                }
            }
            let id = store.begin("POST", path, "HTTP");
            store.routing_decision(id, &config, path, input.to_string().as_bytes());
            let (body, project) = rewrite(&config, path, Bytes::from(input.to_string())).unwrap();
            store.route(id, Some("gpt-5.4".into()), json_model(&body), project);
            store.complete(id, "succeeded", "test", None, 0);
            let entry = store.recent()[0].clone();
            assert_eq!(entry.requested_reasoning.as_deref(), effort);
            assert_eq!(entry.routed_reasoning.as_deref(), Some("high"));
            assert_eq!(
                entry.route_rule.as_deref(),
                Some(if effort == Some("low") {
                    "reasoning"
                } else {
                    "alias"
                })
            );
            assert_eq!(
                entry.routed_model.as_deref(),
                Some(if effort == Some("low") {
                    "model-secondary"
                } else {
                    "model-primary"
                })
            );
        }
    }
    store.flush().await.unwrap();
    let db = store.database.as_ref().unwrap();
    let window = db
        .read(|c| query::window(c, 0, now_ms() + 1))
        .await
        .unwrap();
    assert_eq!(window["entries"].as_array().unwrap().len(), 8);
    assert!(
        window["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["routed_reasoning"] == "high")
    );
    assert!(!window.to_string().contains("private test content"));
}

#[tokio::test]
async fn durable_history_exceeds_live_limit_and_late_active_request_completes() {
    let (_dir, store) = fixture();
    let old = store.begin("POST", "/v1/responses", "HTTP");
    let old_key = store.recent()[0].request_id.clone();
    for _ in 0..650 {
        sample(&store, "succeeded");
    }
    assert_eq!(store.recent().len(), 500);
    store.usage(old, &json!({"usage":{"input_tokens":5,"output_tokens":8}}));
    store.complete(old, "succeeded", "test", None, 99);
    store.flush().await.unwrap();
    let db = store.database.as_ref().unwrap();
    let report = db
        .read(|c| query::report(c, &all().filter()?))
        .await
        .unwrap();
    assert_eq!(report["summary"]["requests"], 651);
    assert_eq!(report["summary"]["succeeded"], 651);
    assert_eq!(report["summary"]["active"], 0);
    assert_eq!(report["summary"]["priced_requests"], 650);
    assert!((report["summary"]["estimated_cost_usd"].as_f64().unwrap() - 172.25).abs() < 1e-6);
    let detail = db
        .read(move |c| query::detail(c, &old_key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail["request"]["output_tokens"], 8);
    assert_eq!(detail["request"]["state"], "succeeded");
    assert_eq!(db.health()["dropped_events"], 0);
}

#[tokio::test]
async fn restart_preserves_completed_records_marks_unfinished_and_uses_new_ids() {
    let (dir, store) = fixture();
    sample(&store, "succeeded");
    let unfinished = store.begin("SEND", "/v1/responses", "WebSocket");
    let key = store.recent()[0].request_id.clone();
    assert_eq!(unfinished, 2);
    store.flush().await.unwrap();
    assert!(Store::open(&Config::test_fixture(), &dir.path().join("config.json")).is_err());
    drop(store);
    let mut reopened = None;
    for _ in 0..100 {
        match Store::open(&Config::test_fixture(), &dir.path().join("config.json")) {
            Ok(store) => {
                reopened = Some(store);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    let reopened = reopened.unwrap();
    let id = reopened.begin("GET", "/v1/models", "HTTP");
    assert_eq!(id, 1);
    assert_ne!(reopened.recent()[0].request_id, key);
    let record = reopened
        .database
        .as_ref()
        .unwrap()
        .read(move |c| query::detail(c, &key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record["request"]["state"], "interrupted");
    assert_eq!(record["request"]["error_code"], "proxy_process_ended");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.path().join("requests.sqlite3"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn writer_lock_and_queue_saturation_never_block_forwarding_and_gaps_are_visible() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::test_fixture();
    config.logging.queue_capacity = 128;
    config.logging.batch_size = 32;
    config.logging.flush_interval_ms = 10;
    let store = Store::open(&config, &dir.path().join("config.json")).unwrap();
    let connection = Connection::open(dir.path().join("requests.sqlite3")).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = Instant::now();
    for _ in 0..1000 {
        sample(&store, "succeeded");
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "Forwarding waited for a locked database"
    );
    let db = store.database.as_ref().unwrap();
    assert!(db.health()["dropped_events"].as_u64().unwrap() > 0);
    assert!(db.health()["pending_events"].as_u64().unwrap() <= 160);
    // Read-only snapshots still work while the writer is locked (WAL).
    assert_eq!(
        db.read(|c| query::history(c, &all().filter()?))
            .await
            .unwrap()["total"],
        0
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(db.health()["write_errors"].as_u64().unwrap() > 0);
    connection.execute_batch("ROLLBACK").unwrap();
    store.flush().await.unwrap();
    assert_eq!(db.health()["pending_events"], 0);
    assert_eq!(db.health()["status"], "gaps");
    let drops = db.health()["dropped_events"].as_u64().unwrap();
    assert_eq!(
        db.read(|c| Ok(c.query_row(
            "SELECT value FROM metadata WHERE key='dropped_events'",
            [],
            |r| r.get::<_, String>(0)
        )?))
        .await
        .unwrap(),
        drops.to_string()
    );
}

#[tokio::test]
async fn filters_keyset_pagination_percentiles_and_complete_exports() {
    let (_dir, store) = fixture();
    for i in 0..103 {
        let id = sample(&store, if i % 2 == 0 { "succeeded" } else { "failed" });
        store.update(id, "test_timing", json!({}), |e| {
            e.timestamp_ms = 1000;
            e.duration_ms = Some(i);
            e.total_duration_ms = Some(i * 10);
            true
        });
    }
    store.flush().await.unwrap();
    let db = store.database.as_ref().unwrap();
    let report = db
        .read(|c| query::report(c, &all().filter()?))
        .await
        .unwrap();
    assert_eq!(report["summary"]["total_duration_ms"]["p95"], 970);
    assert_eq!(report["summary"]["total_duration_ms"]["p50"], 510);
    let mut ids = HashSet::new();
    let mut q = all();
    q.limit = Some(17);
    loop {
        let f = q.filter().unwrap();
        let page = db.read(move |c| query::history(c, &f)).await.unwrap();
        for e in page["entries"].as_array().unwrap() {
            assert!(ids.insert(e["request_id"].as_str().unwrap().to_owned()));
        }
        q.cursor = page["next_cursor"].as_str().map(str::to_owned);
        if q.cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 103);
    let csv = db
        .read(|c| query::export(c, &all().filter()?, "csv"))
        .await
        .unwrap();
    assert_eq!(csv.lines().count(), 104);
    let jsonl = db
        .read(|c| query::export(c, &all().filter()?, "jsonl"))
        .await
        .unwrap();
    assert_eq!(jsonl.lines().count(), 103);
    let q = Query {
        model: Some("model-primary".into()),
        model_field: Some("routed_model".into()),
        state: Some("failed".into()),
        ..all()
    };
    let filtered = db
        .read(move |c| query::history(c, &q.filter()?))
        .await
        .unwrap();
    assert_eq!(filtered["total"], 51);
    let q = Query {
        q: Some("' OR 1=1 --".into()),
        ..all()
    };
    assert_eq!(
        db.read(move |c| query::history(c, &q.filter()?))
            .await
            .unwrap()["total"],
        0
    );
    assert!(
        Query {
            group: Some("state; DROP TABLE requests".into()),
            ..all()
        }
        .filter()
        .is_err()
    );
    assert!(
        Query {
            start: Some(10),
            end: Some(5),
            ..all()
        }
        .filter()
        .is_err()
    );
}

#[tokio::test]
async fn safe_metadata_and_csv_formula_escaping() {
    let (_dir, store) = fixture();
    let id = sample(&store, "failed");
    store.route(id, Some("=IMPORTXML(\"secret\")".into()), None, "@project");
    store.observe(id,&json!({"type":"error","error":{"code":"test_error","message":"sensitive upstream payload"},"input":"private prompt"}));
    store.flush().await.unwrap();
    let db = store.database.as_ref().unwrap();
    let csv = db
        .read(|c| query::export(c, &all().filter()?, "csv"))
        .await
        .unwrap();
    assert!(csv.contains("'@project"));
    assert!(csv.contains("'=IMPORTXML"));
    assert!(!csv.contains("private prompt"));
    let data = db
        .read(|c| query::export(c, &all().filter()?, "jsonl"))
        .await
        .unwrap();
    assert!(!data.contains("sensitive upstream payload"));
    assert!(data.contains("test_error"));
}

#[tokio::test]
async fn body_completion_cancellation_error_and_missing_sse_terminal_are_distinct() {
    let store = Arc::new(Store::default());
    let id = store.begin("POST", "/v1/responses", "HTTP");
    store.finish(id, 200);
    let body = RequestGuard::new(store.clone(), id).wrap(Body::from("body"), Some(4), false);
    assert_eq!(store.recent()[0].state, "streaming");
    assert_eq!(to_bytes(body, 1024).await.unwrap(), "body");
    assert_eq!(store.recent()[0].state, "succeeded");
    assert_eq!(store.recent()[0].response_bytes, 4);
    let id = store.begin("POST", "/v1/responses", "HTTP");
    drop(RequestGuard::new(store.clone(), id));
    assert_eq!(store.recent()[0].state, "cancelled");
    let id = store.begin("POST", "/v1/responses", "HTTP");
    let broken = futures_util::stream::iter([Err::<Bytes, _>(std::io::Error::other("broken"))]);
    assert!(
        to_bytes(
            RequestGuard::new(store.clone(), id).wrap(Body::from_stream(broken), None, false),
            1024
        )
        .await
        .is_err()
    );
    assert_eq!(store.recent()[0].state, "failed");
    assert_eq!(
        store.recent()[0].error_code.as_deref(),
        Some("upstream_stream_error")
    );
    let id = store.begin("POST", "/v1/responses", "HTTP");
    store.finish(id, 200);
    store.update(id, "sse", json!({}), |e| {
        e.streaming = true;
        true
    });
    to_bytes(
        RequestGuard::new(store.clone(), id).wrap(Body::from("data: {}\n\n"), None, true),
        1024,
    )
    .await
    .unwrap();
    assert_eq!(store.recent()[0].state, "failed");
    assert_eq!(
        store.recent()[0].error_code.as_deref(),
        Some("stream_missing_completed")
    );
}

#[test]
fn split_multiline_sse_and_oversize_events_do_not_poison_later_usage() {
    let store = Store::default();
    let id = store.begin("POST", "/v1/responses", "HTTP");
    let mut reader = UsageReader::new(true);
    for byte in b"data: {\"type\":\"response.completed\",\r\ndata: \"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":9}}}\r\n\r\n" {reader.feed(&[*byte],&store,id);}
    assert_eq!(store.recent()[0].output_tokens, Some(9));
    reader.feed(b"data: ", &store, id);
    reader.feed(&vec![b'x'; 8 * 1024 * 1024], &store, id);
    reader.feed(
        b"\ndata: {\"usage\":{\"input_tokens\":100,\"output_tokens\":100}}\n\n",
        &store,
        id,
    );
    assert_eq!(store.recent()[0].output_tokens, Some(9));
    reader.feed(
        b"data: {\"usage\":{\"input_tokens\":5,\"output_tokens\":12}}\n\n",
        &store,
        id,
    );
    assert_eq!(store.recent()[0].output_tokens, Some(12));
    reader.feed(
        b"data: {\"type\":\"error\",\"error\":{\"code\":\"test\"}}\n\ndata: [DONE]\n\n",
        &store,
        id,
    );
    store.complete(id, "succeeded", "stream_end", None, 0);
    assert_eq!(store.recent()[0].state, "failed");
}

#[test]
fn websocket_concurrent_responses_correlate_usage_and_disconnects() {
    let store = Arc::new(Store::default());
    let mut tracker = WebSocketTracker::new(store.clone());
    let a = store.begin("SEND", "/v1/responses", "WebSocket");
    tracker.begin(RequestGuard::new(store.clone(), a));
    let b = store.begin("SEND", "/v1/responses", "WebSocket");
    tracker.begin(RequestGuard::new(store.clone(), b));
    tracker.observe(
        &json!({"type":"response.created","response":{"id":"a"}}),
        50,
    );
    tracker.observe(
        &json!({"type":"response.created","response":{"id":"b"}}),
        50,
    );
    tracker.observe(&json!({"type":"response.completed","response":{"id":"b","usage":{"input_tokens":2,"output_tokens":3}}}),80);
    let recent = store.recent();
    assert_eq!(recent[0].state, "succeeded");
    assert_eq!(recent[0].output_tokens, Some(3));
    assert_eq!(recent[1].output_tokens, None);
    tracker.fail_all("upstream_disconnected");
    assert_eq!(store.recent()[1].state, "failed");
    assert!(tracker.is_empty());
}

#[test]
fn stored_prices_match_regular_price_book_cache_and_context_rules() {
    let mut e = Entry {
        method: "POST".into(),
        path: "/v1/responses".into(),
        requested_model: Some("gpt-5.4".into()),
        routed_model: Some("model-primary".into()),
        input_tokens: Some(100000),
        output_tokens: Some(10000),
        ..Entry::default()
    };
    assert_eq!(pricing::price(&e).cost_nano_usd, Some(400_000_000));
    e.cached_input_tokens = Some(60000);
    e.cache_write_tokens = Some(20000);
    assert_eq!(pricing::price(&e).cost_nano_usd, Some(265_000_000));
    e.input_tokens = Some(300000);
    e.cached_input_tokens = Some(100000);
    e.cache_write_tokens = Some(50000);
    assert_eq!(pricing::price(&e).cost_nano_usd, Some(1_275_000_000));
    e.output_tokens = None;
    assert_eq!(pricing::price(&e).cost_nano_usd, None);
}

#[test]
fn gemini_model_uses_pro_list_price_with_cache_and_long_context_boundary() {
    let mut e = Entry {
        method: "POST".into(),
        path: "/v1/responses".into(),
        requested_model: Some("gpt-5.4".into()),
        input_tokens: Some(200000),
        cached_input_tokens: Some(100000),
        output_tokens: Some(10000),
        reasoning_tokens: Some(6000),
        ..Entry::default()
    };
    for model in [
        "gemini/gemini-3.1-pro-preview",
        "gemini/models/gemini-3.1-pro-preview",
        "models/gemini-3.1-pro-preview",
    ] {
        e.routed_model = Some(model.into());
        let price = pricing::price(&e);
        assert_eq!(price.price_model.as_deref(), Some("gemini-3.1-pro-preview"));
        // Output already includes reasoning; never charge it twice.
        assert_eq!(price.cost_nano_usd, Some(340_000_000));
    }
    e.input_tokens = Some(200001);
    assert_eq!(pricing::price(&e).cost_nano_usd, Some(620_004_000));
    e.routed_model = Some("gemini/unknown".into());
    assert_eq!(pricing::price(&e).cost_nano_usd, None);
}

#[tokio::test]
async fn report_read_concurrency_is_bounded_without_blocking_writer() {
    let (_dir, store) = fixture();
    let db = store.database.as_ref().unwrap().clone();
    let ready = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let db = db.clone();
        let ready = ready.clone();
        let barrier = barrier.clone();
        jobs.push(tokio::spawn(async move {
            db.read(move |_| {
                ready.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                barrier.wait();
                Ok(())
            })
            .await
        }));
    }
    while ready.load(std::sync::atomic::Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    assert!(
        db.read(|_| Ok(()))
            .await
            .unwrap_err()
            .to_string()
            .contains("busy")
    );
    sample(&store, "succeeded");
    store.flush().await.unwrap();
    barrier.wait();
    for job in jobs {
        job.await.unwrap().unwrap();
    }
    assert_eq!(db.health()["pending_events"], 0);
}

async fn serve(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, task)
}

#[tokio::test]
async fn reports_authentication_validation_and_export_end_to_end() {
    let (dir, store) = fixture();
    sample(&store, "succeeded");
    store.flush().await.unwrap();
    let path = dir.path().join("host.json");
    let keys = crate::access::ensure(&path, &[]).unwrap();
    let config = Config {
        mode: Mode::Host,
        ..Config::test_fixture()
    };
    let app = router_with(
        config,
        Options {
            logs: Some(store.clone()),
            access_config: Some(path),
            ..Options::default()
        },
    )
    .unwrap();
    let (url, server) = serve(app).await;
    let client = reqwest::Client::new();
    for path in [
        "/logs/api?minutes=60",
        "/logs/api/reports",
        "/logs/api/history",
        "/logs/api/export",
        "/logs/api/health",
        "/logs/reporting.js",
        "/logs/prices.js",
    ] {
        assert_eq!(
            client
                .get(format!("{url}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let response = client
            .get(format!("{url}{path}"))
            .bearer_auth(&keys.local)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path}");
        assert!(response.headers().contains_key(header::CACHE_CONTROL));
    }
    assert_eq!(
        client
            .get(format!("{url}/logs/api/history?limit=99999"))
            .bearer_auth(&keys.local)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        client
            .get(format!("{url}/logs/api/requests/missing"))
            .bearer_auth(&keys.local)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let response = client
        .get(format!("{url}/logs/api/export?format=csv"))
        .bearer_auth(&keys.local)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/csv; charset=utf-8"
    );
    assert_eq!(response.text().await.unwrap().lines().count(), 2);
    client.get(format!("{url}/v1/models")).send().await.unwrap();
    store.flush().await.unwrap();
    assert_eq!(
        store.recent()[0].error_code.as_deref(),
        Some("unauthorized")
    );
    server.abort();
}

#[tokio::test]
async fn http_success_failure_and_sse_completion_are_durable_through_real_forwarding() {
    let mock=axum::Router::new().fallback(|request:axum::extract::Request|async move {
        let body=to_bytes(request.into_body(),1024*1024).await.unwrap();
        let value:Value=serde_json::from_slice(&body).unwrap();
        if value["stream"]==true { ([(header::CONTENT_TYPE,"text/event-stream")],"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r_test\",\"usage\":{\"input_tokens\":10,\"output_tokens\":20}}}\n\n").into_response() }
        else {axum::Json(json!({"id":"r_test","usage":{"input_tokens":10,"output_tokens":20}})).into_response()}
    });
    let (upstream, mock_task) = serve(mock).await;
    let (_dir, store) = fixture();
    let config = Config {
        upstream_url: upstream,
        ..Config::test_fixture()
    };
    let (url, proxy_task) = serve(
        router_with(
            config,
            Options {
                logs: Some(store.clone()),
                ..Options::default()
            },
        )
        .unwrap(),
    )
    .await;
    for stream in [false, true] {
        let response = reqwest::Client::new()
            .post(format!("{url}/v1/responses"))
            .json(&json!({"model":"gpt-5.4","input":"private input","stream":stream}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
    }
    store.flush().await.unwrap();
    let report = store
        .database
        .as_ref()
        .unwrap()
        .read(|c| query::report(c, &all().filter()?))
        .await
        .unwrap();
    assert_eq!(report["summary"]["succeeded"], 2);
    assert_eq!(report["summary"]["output_tokens"], 40);
    assert_eq!(report["summary"]["priced_requests"], 2);
    let id = store.recent()[0].request_id.clone();
    let d = store
        .database
        .as_ref()
        .unwrap()
        .read(move |c| query::detail(c, &id))
        .await
        .unwrap()
        .unwrap();
    let events = d["events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "attempt_started" && e["details"]["attempt"] == 1)
    );
    assert!(!d.to_string().contains("private input"));
    assert_eq!(d["request"]["response_id"], "r_test");
    proxy_task.abort();
    mock_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual loopback performance measurement; no external model calls"]
async fn logging_forwarding_benchmark() {
    let mock = axum::Router::new().fallback(|| async {
        axum::Json(json!({"usage":{"input_tokens":100,"output_tokens":20}}))
    });
    let (upstream, mock_task) = serve(mock).await;
    let mut results = Vec::new();
    for phase in ["memory_only", "sqlite_wal", "sqlite_writer_locked"] {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            upstream_url: upstream.clone(),
            ..Config::test_fixture()
        };
        config.logging.enabled = phase != "memory_only";
        config.logging.queue_capacity = 65536;
        let store = Arc::new(Store::open(&config, &dir.path().join("config.json")).unwrap());
        let lock = if phase == "sqlite_writer_locked" {
            let c = Connection::open(dir.path().join("requests.sqlite3")).unwrap();
            c.execute_batch("BEGIN IMMEDIATE").unwrap();
            Some(c)
        } else {
            None
        };
        let (url, proxy_task) = serve(
            router_with(
                config,
                Options {
                    logs: Some(store.clone()),
                    ..Options::default()
                },
            )
            .unwrap(),
        )
        .await;
        let client = reqwest::Client::new();
        let mut timings = Vec::new();
        for _ in 0..50 {
            client
                .post(format!("{url}/v1/responses"))
                .json(&json!({"model":"gpt-5.4","input":"benchmark"}))
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
        }
        let started = Instant::now();
        for _ in 0..1000 {
            let start = Instant::now();
            client
                .post(format!("{url}/v1/responses"))
                .json(&json!({"model":"gpt-5.4","input":"benchmark"}))
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            timings.push(start.elapsed().as_micros() as u64);
        }
        let elapsed = started.elapsed();
        timings.sort_unstable();
        let before = store.database.as_ref().map(|db| db.health());
        if let Some(lock) = lock {
            lock.execute_batch("ROLLBACK").unwrap();
        }
        store.flush().await.unwrap();
        results.push(json!({"phase":phase,"requests":1000,"requests_per_second":1000.0/elapsed.as_secs_f64(),"p50_us":timings[499],"p95_us":timings[949],"p99_us":timings[989],"logging_before_unlock":before}));
        proxy_task.abort();
    }
    println!(
        "LOGGING_BENCHMARK {}",
        serde_json::to_string_pretty(&results).unwrap()
    );
    mock_task.abort();
}

#[test]
fn normal_null_error_does_not_mark_success_as_failed_and_incomplete_is_recorded() {
    let store = Store::default();
    let id = store.begin("POST", "/v1/responses", "HTTP");
    store.observe(id,&json!({"id":"success","object":"response","status":"completed","error":null,"usage":{"input_tokens":10,"output_tokens":4}}));
    store.complete(id, "succeeded", "http", None, 0);
    assert_eq!(store.recent()[0].state, "succeeded");
    assert_eq!(store.recent()[0].response_id.as_deref(), Some("success"));
    let id = store.begin("POST", "/v1/responses", "HTTP");
    store.observe(id,&json!({"status":"incomplete","error":null,"incomplete_details":{"reason":"max_output_tokens"}}));
    store.complete(id, "succeeded", "http", None, 0);
    assert_eq!(store.recent()[0].state, "failed");
    assert_eq!(
        store.recent()[0].error_code.as_deref(),
        Some("max_output_tokens")
    );
}

#[test]
fn client_close_after_completed_event_is_success_but_blocked_or_partial_stream_is_not() {
    for (kind, expected, code) in [
        ("response.completed", "succeeded", None),
        ("response.failed", "failed", Some("cyber_policy")),
        ("response.created", "cancelled", None),
    ] {
        let store = Arc::new(Store::default());
        let id = store.begin("POST", "/v1/responses", "HTTP");
        store.finish(id, 200);
        store.update(id, "stream", json!({}), |e| {
            e.streaming = true;
            true
        });
        let guard = RequestGuard::new(store.clone(), id);
        store.observe(id,&json!({"type":kind,"response":{"id":"r","error":code.map(|code|json!({"code":code}))}}));
        drop(guard);
        let e = &store.recent()[0];
        assert_eq!(e.state, expected, "{kind}");
        if expected == "succeeded" {
            assert_eq!(e.error_code, None);
            assert_eq!(e.outcome_source.as_deref(), Some("responses_event"));
        }
        if let Some(code) = code {
            assert_eq!(e.error_code.as_deref(), Some(code));
        }
    }
}

#[tokio::test]
async fn startup_repairs_only_proven_old_completion_cancellations_and_keeps_audit() {
    let (dir, store) = fixture();
    let id = store.begin("POST", "/v1/responses", "HTTP");
    store.finish(id, 200);
    store.update(id, "stream", json!({}), |e| {
        e.streaming = true;
        true
    });
    store.observe(
        id,
        &json!({"type":"response.completed","response":{"id":"r"}}),
    );
    store.complete(id, "succeeded", "stream_end", None, 10);
    let key = store.recent()[0].request_id.clone();
    store.update(id, "simulate_old_bug", json!({}), |e| {
        e.state = "cancelled".into();
        e.error_code = Some("client_disconnected".into());
        e.outcome_source = Some("client_disconnect".into());
        true
    });
    let partial = store.begin("POST", "/v1/responses", "HTTP");
    store.complete(
        partial,
        "cancelled",
        "client_disconnect",
        Some("client_disconnected"),
        2,
    );
    store.flush().await.unwrap();
    // This fixture represents a database created by the pre-fix executable.
    Connection::open(dir.path().join("requests.sqlite3"))
        .unwrap()
        .execute(
            "DELETE FROM metadata WHERE key='completed_disconnect_fix_v1'",
            [],
        )
        .unwrap();
    drop(store);
    let reopened = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match Store::open(&Config::test_fixture(), &dir.path().join("config.json")) {
                Ok(store) => break store,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("database reopened after writer exit");
    let report = reopened
        .database
        .as_ref()
        .unwrap()
        .read(|c| query::report(c, &all().filter()?))
        .await
        .unwrap();
    assert_eq!(report["summary"]["succeeded"], 1);
    assert_eq!(report["summary"]["cancelled"], 1);
    let detail = reopened
        .database
        .as_ref()
        .unwrap()
        .read(move |c| query::detail(c, &key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail["request"]["error_code"], Value::Null);
    assert_eq!(
        detail["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "classification_corrected")
            .count(),
        1
    );
}
