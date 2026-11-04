use super::*;
use serde_json::json;
use std::sync::Barrier;

struct Clock {
    wall: Mutex<f64>,
    perf: Mutex<f64>,
}
impl Clock {
    fn new() -> Self {
        Self {
            wall: Mutex::new(1000.0),
            perf: Mutex::new(0.0),
        }
    }
}
impl MemoryClock for Clock {
    fn wall(&self) -> f64 {
        *self.wall.lock().unwrap()
    }
    fn performance(&self) -> f64 {
        let mut value = self.perf.lock().unwrap();
        let before = *value;
        *value += 0.001;
        before
    }
}
fn fixture() -> (tempfile::TempDir, Arc<MemoryService>, Arc<Clock>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let db = crate::db::open_writer(&path).unwrap();
    ingest::apply_full_schema(&db).unwrap();
    drop(db);
    let clock = Arc::new(Clock::new());
    let service = Arc::new(
        MemoryService::with_clock(
            &path,
            4,
            Duration::from_millis(1),
            Duration::ZERO,
            clock.clone(),
        )
        .unwrap(),
    );
    let mut sink = WriterSink(service.writer.clone());
    sink.upsert_session(
        "s",
        "generic",
        Some("project"),
        None,
        None,
        Some(1.0),
        Some(1.0),
        None,
    )
    .unwrap();
    sink.insert_chunks(
        "s",
        &["alpha needle memory".into(), "alpha second memory".into()],
        Some("user"),
        None,
        Some(1.0),
        None,
    )
    .unwrap();
    (dir, service, clock)
}
fn input(query: &str) -> RecallInput {
    RecallInput::typed(query, 10, &Filters::default(), 1200)
}
fn source(dir: &tempfile::TempDir) -> String {
    let path = dir.path().join("sample.jsonl");
    std::fs::write(
        &path,
        "{\"role\":\"user\",\"text\":\"alpha fixture message\",\"session_id\":\"fixture\"}\n",
    )
    .unwrap();
    path.to_str().unwrap().into()
}
#[test]
fn recall_cache_updates_shared_query_stats_and_wall_jump() {
    let (_dir, service, clock) = fixture();
    let request = input("alpha");
    let first = service.recall(&request).unwrap();
    assert!(!first.is_empty());
    let mut copy = service.recall(&request).unwrap();
    copy[0].text.clear();
    assert_eq!(first, service.recall(&request).unwrap());
    *clock.wall.lock().unwrap() = 900.0;
    service.recall(&request).unwrap();
    *clock.wall.lock().unwrap() = 1015.0;
    service.recall(&request).unwrap(); // inclusive expiry
    *clock.wall.lock().unwrap() = 1016.0;
    service.recall(&request).unwrap();
    let stats = service.query_stats().percentiles();
    assert_eq!(stats["count"], 6);
    assert_eq!(stats["cache_hits"], 4);
    assert_eq!(stats["errors"], 0);
    assert_eq!(stats["p50"], 1.0);
}
#[test]
fn cache_lru_capacity_and_hits_move_recency() {
    let (_dir, service, _) = fixture();
    for n in 0..1024 {
        service.recall(&input(&format!("token{n}"))).unwrap();
    }
    service.recall(&input("token0")).unwrap();
    service.recall(&input("token1024")).unwrap();
    service.recall(&input("token0")).unwrap();
    service.recall(&input("token1")).unwrap();
    assert_eq!(service.cache.lock().unwrap().len(), 1024);
    assert_eq!(service.query_stats().percentiles()["cache_hits"], 2);
}
#[test]
fn raw_keys_preserve_types_and_python_numeric_equality() {
    let (_dir, service, _) = fixture();
    let mut request = input("alpha");
    for since in [json!("1"), json!(1), json!(true), json!(1.0)] {
        request.since = since.into();
        service.recall(&request).unwrap();
    }
    assert_eq!(service.query_stats().percentiles()["cache_hits"], 2);
    for since in [json!(false), json!(0), json!(-0.0), Value::Null, json!("")] {
        request.since = since.into();
        service.recall(&request).unwrap();
    }
    assert_eq!(service.query_stats().percentiles()["cache_hits"], 4);
}
#[test]
fn empty_input_precedes_limit_and_hash_errors_without_error_metrics() {
    let (_dir, service, _) = fixture();
    let mut request = input("");
    request.limit = json!({}).into();
    request.since = json!([]).into();
    service.recall(&request).unwrap();
    assert_eq!(service.query_stats().percentiles()["p50"], 0.0);
    request.query = json!("alpha").into();
    assert!(service.recall(&request).is_err());
    request.limit = json!(10).into();
    assert!(service.recall(&request).is_err());
    request.query = json!(true).into();
    assert!(service.recall(&request).is_err());
    let stats = service.query_stats().percentiles();
    assert_eq!(stats["count"], 1);
    assert_eq!(stats["errors"], 0);
}
#[test]
fn signed_zero_huge_limits_and_fetch_errors() {
    let (_dir, service, _) = fixture();
    for limit in [
        json!(0),
        json!(-7),
        json!("-99999999999999999999999999999"),
        json!(true),
        json!(1.9),
        json!("١_٠"),
    ] {
        let mut request = input("alpha");
        request.limit = limit.clone().into();
        let rows = service.recall(&request).unwrap();
        assert_eq!(rows.len(), if limit == json!("١_٠") { 2 } else { 1 });
    }
    let mut request = input("alpha");
    request.limit = json!("99999999999999999999999999999").into();
    assert!(service
        .recall(&request)
        .unwrap_err()
        .to_string()
        .contains("SQLite INTEGER"));
    request.limit = json!(10).into();
    request.since = json!("bad float").into();
    assert!(service.recall(&request).is_err());
    assert_eq!(service.query_stats().percentiles()["errors"], 2);
}
#[test]
fn typed_nonfinite_is_not_null_and_conversion_failure_precedes_metrics() {
    let (_dir, service, _) = fixture();
    let mut request = RecallInput::typed(
        "alpha",
        10,
        &Filters {
            since: Some(f64::NAN),
            ..Filters::default()
        },
        1200,
    );
    assert!(matches!(request.since,QueryInput::Float(v) if v.is_nan()));
    service.recall(&request).unwrap();
    request.since = QueryInput::Float(f64::INFINITY);
    assert!(service.recall(&request).unwrap().is_empty());
    request.limit = QueryInput::Float(f64::INFINITY);
    assert!(service.recall(&request).is_err());
    assert_eq!(service.query_stats().percentiles()["count"], 2);
}
#[test]
fn read_error_records_and_propagates() {
    let (_dir, service, _) = fixture();
    service
        .writer
        .call(|db| {
            db.execute_batch("DROP TABLE chunks_fts")
                .map_err(map_sqlite)
        })
        .unwrap();
    assert!(service.recall(&input("alpha")).is_err());
    assert_eq!(service.query_stats().percentiles()["errors"], 1);
}
#[test]
fn concurrent_queries_share_stats_without_writer_jobs() {
    let (_dir, service, _) = fixture();
    let before = service.writer.call(|db| Ok(db.total_changes())).unwrap();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let service = service.clone();
            scope.spawn(move || {
                for _ in 0..8 {
                    service.recall(&input("alpha")).unwrap();
                }
            });
        }
    });
    assert_eq!(service.query_stats().percentiles()["count"], 64);
    assert_eq!(
        before,
        service.writer.call(|db| Ok(db.total_changes())).unwrap()
    );
}
#[test]
fn latency_window_uses_ties_even_and_omits_empty_cache_hits() {
    let stats = QueryStats::default();
    assert!(stats.percentiles().get("cache_hits").is_none());
    for n in 0..4 {
        stats.record(n as f64, false, false);
    }
    assert_eq!(stats.percentiles()["p50"], 2.0);
    for _ in 0..4000 {
        stats.record(9.0, false, true);
    }
    let result = stats.percentiles();
    assert_eq!(result["count"], 4004);
    assert_eq!(result["p50"], 9.0);
    assert_eq!(result["cache_hits"], 4000);
}
#[test]
fn pending_session_commits_before_batch_rollback_and_checkpoint() {
    let (_dir, service, _) = fixture();
    let mut sink = WriterSink(service.writer.clone());
    sink.upsert_session("pending", "generic", None, None, None, None, None, None)
        .unwrap();
    let count = || {
        service
            .pool
            .with(|db| {
                db.query_row(
                    "SELECT COUNT(*) FROM sessions WHERE id='pending'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .map_err(map_sqlite)
            })
            .unwrap()
    };
    assert_eq!(count(), 0);
    assert!(sink
        .insert_chunks(
            "missing-session",
            &["foreign key failure".into()],
            None,
            None,
            None,
            None
        )
        .is_err());
    assert_eq!(count(), 1);
    assert!(service.writer.call(|db| Ok(db.is_autocommit())).unwrap());
    sink.upsert_session("checkpoint", "generic", None, None, None, None, None, None)
        .unwrap();
    let actual = service.checkpoint().unwrap();
    assert_eq!(actual.0, 0);
    assert!(actual.1 >= actual.2);
    assert!(service.writer.call(|db| Ok(db.is_autocommit())).unwrap());
}
#[test]
fn parser_block_allows_same_actor_maintenance_and_bounded_stop_retains_worker() {
    let (dir, service, _) = fixture();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let (a, b) = (entered.clone(), release.clone());
    service.ingestor.set_parse_hook(Arc::new(move || {
        a.wait();
        b.wait();
    }));
    service.enqueue(&source(&dir), None, None).unwrap();
    service.start().unwrap();
    entered.wait();
    let id = service.ingestor.worker_id();
    service.start().unwrap();
    assert_eq!(id, service.ingestor.worker_id());
    service.checkpoint().unwrap();
    service.recover_leases().unwrap();
    assert!(!service.ingestor.stop_with_timeout(Duration::ZERO));
    assert!(service.ingestor.status().running);
    release.wait();
    service.stop_and_join().unwrap();
    assert!(!service.ingestor.status().running);
    service.stop_and_join().unwrap();
    assert!(service.writer.call(|_| Ok(())).is_err());
}
#[test]
fn startup_recovery_precedes_claim_and_failure_is_nonfatal() {
    let (dir, service, _) = fixture();
    service.enqueue(&source(&dir), None, None).unwrap();
    service.writer.call(|db| {db.execute_batch("UPDATE ingest_queue SET state='leased',lease_until=0; UPDATE sources SET state='ingesting'").map_err(map_sqlite)}).unwrap();
    service.pause();
    service.start().unwrap();
    assert_eq!(service.stats().unwrap()["queue"]["queued"], 1);
    assert!(service.ingestor.status().paused);
    service.ingestor.stop();
    service
        .writer
        .call(|db| {
            db.execute_batch("DROP TABLE ingest_queue")
                .map_err(map_sqlite)
        })
        .unwrap();
    service.start().unwrap();
    assert!(service.ingestor.status().running);
    assert!(service.ingestor.status().last_error.is_some());
    service.stop_and_join().unwrap();
}
#[test]
fn missing_path_refuses_and_service_has_no_owner_cycle() {
    let dir = tempfile::tempdir().unwrap();
    assert!(MemoryService::open(
        dir.path().join("missing"),
        1,
        Duration::ZERO,
        Duration::ZERO
    )
    .is_err());
    let (_dir, service, _) = fixture();
    let weak = Arc::downgrade(&service);
    service.stop().unwrap();
    drop(service);
    assert!(weak.upgrade().is_none());
}
#[test]
fn context_and_status_use_shared_queries_and_actual_database_counts() {
    let (_dir, service, _) = fixture();
    let value = service.context("alpha", 2, 2).unwrap();
    assert_eq!(value["counts"]["sessions"], 1);
    let status = service.status().unwrap();
    assert_eq!(status["queries"]["count"], 1);
    assert_eq!(status["chunks"], 2);
    assert_eq!(status["ok"], true);
    assert_eq!(status["ingestor"]["running"], false);
}

#[test]
fn expired_refresh_preserves_lru_position() {
    let (_dir, service, clock) = fixture();
    service.recall(&input("oldest")).unwrap();
    service.recall(&input("newer")).unwrap();
    *clock.wall.lock().unwrap() = 1016.0;
    service.recall(&input("oldest")).unwrap();
    let cache = service.cache.lock().unwrap();
    assert_eq!(cache.front().unwrap().key.query, "oldest");
    assert_eq!(cache.back().unwrap().key.query, "newer");
}

#[test]
fn source_finalize_and_queue_mutations_use_same_connection_and_commit_pending() {
    let (dir, service, _) = fixture();
    service.writer.call(|db| {
        // A TEMP table and its trigger can only be seen on this exact connection.
        db.execute_batch("CREATE TEMP TABLE actor_trace(event TEXT); CREATE TEMP TRIGGER trace_queue AFTER INSERT ON main.ingest_queue BEGIN INSERT INTO actor_trace VALUES('enqueue'); END;").map_err(map_sqlite)
    }).unwrap();
    let mut sink = WriterSink(service.writer.clone());
    sink.upsert_session(
        "pending-source",
        "generic",
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();
    let path = source(&dir);
    sink.upsert_source_row(&path, "generic", None).unwrap();
    assert!(service.writer.call(|db| Ok(db.is_autocommit())).unwrap());
    service.enqueue(&path, None, None).unwrap();
    let lease = service
        .writer
        .call(|db| ingest::claim_next(db, 120.0))
        .unwrap()
        .unwrap();
    service
        .writer
        .call(move |db| ingest::finish(db, &lease, ingest::Outcome::Ok, &json!({})))
        .unwrap();
    service.requeue_partial().unwrap();
    service.recover_leases().unwrap();
    service.checkpoint().unwrap();
    assert_eq!(
        service
            .writer
            .call(|db| db
                .query_row("SELECT COUNT(*) FROM actor_trace", [], |row| row
                    .get::<_, i64>(0))
                .map_err(map_sqlite))
            .unwrap(),
        1
    );
    assert_eq!(service.stats().unwrap()["queue"]["done"], 1);
    service.stop_and_join().unwrap();
    assert!(service.checkpoint().is_err());
}

#[test]
fn paused_start_resume_restart_and_source_failure_keep_honest_status() {
    let (dir, service, _) = fixture();
    assert!(service.stop().unwrap());
    service.pause();
    let path = source(&dir);
    service.enqueue(&path, None, None).unwrap();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let (a, b) = (entered.clone(), release.clone());
    service.ingestor.set_parse_hook(Arc::new(move || {
        a.wait();
        b.wait();
    }));
    service.start().unwrap();
    assert_eq!(service.stats().unwrap()["queue"]["queued"], 1);
    service.resume();
    entered.wait();
    // Fail the source after claim, without touching any live source file.
    std::fs::remove_file(path).unwrap();
    assert!(!service.ingestor.stop_with_timeout(Duration::ZERO));
    release.wait();
    service.ingestor.stop();
    assert!(service.ingestor.status().last_error.is_some());
    assert_eq!(service.stats().unwrap()["queue"]["failed"], 1);
    service.pause();
    service.start().unwrap();
    assert!(service.ingestor.status().running);
    assert!(service.stop().unwrap());
    assert!(service.stop().unwrap());
    service.stop_and_join().unwrap();
}

#[test]
fn raw_since_uses_python_float_conversion_and_preserves_integer_overflow() {
    let (_dir, service, _) = fixture();
    let mut request = input("alpha");
    request.since = json!("٠_٠.٠").into();
    assert_eq!(service.recall(&request).unwrap().len(), 2);
    request.since = serde_json::from_str::<Value>(&"9".repeat(400))
        .unwrap()
        .into();
    assert!(service
        .recall(&request)
        .unwrap_err()
        .to_string()
        .contains("too large"));
    assert_eq!(service.query_stats().percentiles()["errors"], 1);
    request.since = json!("inf").into();
    assert!(service.recall(&request).unwrap().is_empty());
}

#[test]
fn oversized_event_keeps_post_append_batch_and_role_flush_boundaries() {
    let (dir, service, _) = fixture();
    let path = dir.path().join("oversized.jsonl");
    // More than 500 4000-character chunks even without counting overlap.
    // Distinct role changes before/after expose accidental cross-role grouping.
    let events = [
        json!({"role":"user", "text":"before oversized event"}),
        json!({"role":"assistant", "text":"x".repeat(2_200_000)}),
        json!({"role":"user", "text":"after oversized event"}),
    ];
    std::fs::write(
        &path,
        events
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let mut sink = WriterSink(service.writer.clone());
    super::super::jsonl::ingest_jsonl(&mut sink, &path, "generic", None, &|| false, Duration::ZERO)
        .unwrap();
    let (count, maximum, minimum) = service.pool.with(|db| {
        db.query_row("SELECT COUNT(*), MAX(ordinal), MIN(ordinal) FROM chunks WHERE session_id='oversized' AND role='assistant'", [], |row| Ok((row.get::<_,i64>(0)?, row.get::<_,i64>(1)?, row.get::<_,i64>(2)?))).map_err(map_sqlite)
    }).unwrap();
    assert!(
        count > 500,
        "whole event must cross the threshold before flushing"
    );
    assert_eq!(minimum, 0);
    assert_eq!(
        maximum,
        count - 1,
        "no artificial split/reset at ordinal 500"
    );
    let user_rows = service.pool.with(|db| {
        let mut statement = db.prepare("SELECT text, ordinal FROM chunks WHERE session_id='oversized' AND role='user' ORDER BY id").map_err(map_sqlite)?;
        let rows = statement.query_map([], |row| Ok((row.get::<_,String>(0)?, row.get::<_,i64>(1)?))).map_err(map_sqlite)?;
        rows.collect::<Result<Vec<_>,_>>().map_err(map_sqlite)
    }).unwrap();
    assert_eq!(
        user_rows,
        vec![
            ("before oversized event".into(), 0),
            ("after oversized event".into(), 0)
        ]
    );
}

#[test]
fn runtime_memory_reaper_uses_shared_writer_during_real_ingest_and_reports_closed_owner() {
    use crate::server_runtime::LoopRuntimeAdapter;
    use crate::terminal::fakes::FakeTerminal;
    let (dir, service, _) = fixture();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    service.set_parse_hook(Arc::new(move || {
        entered_tx.send(()).unwrap();
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(30))
            .unwrap();
    }));
    let source = source(&dir);
    service.enqueue(&source, Some("generic"), None).unwrap();
    service.start().unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // The actual parser is gated outside SQL while maintenance uses its same writer.
    let expired = dir.path().join("expired.jsonl");
    let partial = dir.path().join("partial.jsonl");
    std::fs::write(&expired, "{}\n").unwrap();
    std::fs::write(&partial, "{}\n").unwrap();
    service
        .writer
        .call(move |db| {
            ingest::enqueue(db, expired.to_str().unwrap(), Some("generic"), None)?;
            ingest::claim_next(db, -1.0)?.unwrap();
            ingest::enqueue(db, partial.to_str().unwrap(), Some("generic"), None)?;
            let lease = ingest::claim_next(db, 60.0)?.unwrap();
            ingest::finish(
                db,
                &lease,
                ingest::Outcome::Skipped("stopped by operator"),
                &json!({}),
            )?;
            Ok(())
        })
        .unwrap();
    let flock = Arc::new(crate::flock::Flock::new(
        dir.path().join("launches"),
        dir.path().join("lanes"),
        false,
    ));
    flock.attach_memory_service(service.clone()).unwrap();
    let adapter = LoopRuntimeAdapter::new(flock, Arc::new(|| Box::new(FakeTerminal::new())));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(adapter.helper_reaperenv_memory_maintenance());
    assert_eq!(service.stats().unwrap()["queue"]["queued"], 2);
    assert_eq!(service.stats().unwrap()["queue"]["leased"], 1);
    // Stop before releasing the gate so the recovered sources cannot enter it again.
    assert!(!service.ingestor.stop_with_timeout(Duration::ZERO));
    release_tx.send(()).unwrap();
    service.stop_and_join().unwrap();
    assert!(service.worker_id().is_none());
    assert!(service.recover_leases().is_err());
    assert!(service.requeue_partial().is_err());
    // A closed service is not replaced with a fresh path-based writer by the reaper.
    let before = service.stats().unwrap()["queue"].clone();
    runtime.block_on(adapter.helper_reaperenv_memory_maintenance());
    assert_eq!(before, service.stats().unwrap()["queue"]);
}

#[test]
fn memory_throttle_real_worker_accepts_raw_float_then_fails_after_committed_flush() {
    for (seconds, message) in [
        (-1.0, "sleep length must be non-negative"),
        (f64::NAN, "Invalid value NaN (not a number)"),
        (f64::INFINITY, "timestamp out of range for platform time_t"),
        (
            f64::NEG_INFINITY,
            "timestamp out of range for platform time_t",
        ),
        (1e300, "timestamp out of range for platform time_t"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let service = MemoryService::create_at_start(
            dir.path().join("memory.db"),
            2,
            Duration::from_millis(1),
            Throttle::Python(seconds),
        )
        .unwrap();
        let empty = dir.path().join("empty.jsonl");
        std::fs::write(&empty, "{}\n").unwrap();
        service
            .enqueue(empty.to_str().unwrap(), Some("generic"), None)
            .unwrap();
        service.start().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while service.stats().unwrap()["queue"]["done"] != 1 {
            assert!(
                Instant::now() < deadline,
                "empty input failed: {:?}",
                service.ingestor.status()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(service.ingestor.status().last_error.is_none());
        let raw = service.ingestor.status().throttle_s;
        assert!(raw.to_bits() == seconds.to_bits() || (raw.is_nan() && seconds.is_nan()));
        let text = dir.path().join("text.jsonl");
        std::fs::write(
            &text,
            "{\"role\":\"user\",\"text\":\"committed before native sleep failure\"}\n",
        )
        .unwrap();
        service
            .enqueue(text.to_str().unwrap(), Some("generic"), None)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if service.ingestor.status().last_error.as_deref() == Some(message) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "missing delayed error: {:?}",
                service.ingestor.status()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let (state, queue_state, source_error, queue_error, stored) = service.pool.with(|db| {
            db.query_row("SELECT s.state,q.state,s.error,q.error,(SELECT text FROM chunks WHERE session_id='text') FROM sources s JOIN ingest_queue q ON q.source_id=s.id WHERE s.path=?", [text.to_str().unwrap()], |row| {
                Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?, row.get::<_,String>(3)?, row.get::<_,String>(4)?))
            }).map_err(map_sqlite)
        }).unwrap();
        assert_eq!(state, "error");
        assert_eq!(queue_state, "failed");
        assert_eq!(source_error, message);
        assert_eq!(queue_error, message);
        assert_eq!(stored, "committed before native sleep failure");
        service.stop_and_join().unwrap();
        assert!(service.worker_id().is_none());
    }
}

#[test]
fn memory_throttle_status_preserves_numeric_and_wire_nonfinite_values() {
    for (seconds, token) in [
        (f64::NAN, "NaN"),
        (f64::INFINITY, "Infinity"),
        (f64::NEG_INFINITY, "-Infinity"),
        (-0.0, "-0.0"),
        (-1.0, "-1.0"),
        (1e300, "1e+300"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let service = MemoryService::create_at_start(
            dir.path().join("memory.db"),
            2,
            Duration::from_millis(1),
            Throttle::Python(seconds),
        )
        .unwrap();
        let status = service.status().unwrap();
        assert!(status["ingestor"]["throttle_s"].is_number());
        assert_eq!(
            crate::wire_json::to_string(&status["ingestor"]["throttle_s"]).unwrap(),
            token
        );
        // Same direct Value move and response encoder as the memory.status handler.
        let response = crate::protocol::Response::ok(json!(1), status);
        let wire = crate::protocol::encode_line(&response);
        assert!(wire.contains(&format!("\"throttle_s\":{token}")), "{wire}");
        let parsed = crate::wire_json::from_str(&wire).unwrap();
        assert!(parsed["result"]["ingestor"]["throttle_s"].is_number());
        let output = std::process::Command::new("python3")
            .args(["-c", "import json,math,sys; v=json.loads(sys.argv[1])['result']['ingestor']['throttle_s']; e=float(sys.argv[2]); assert isinstance(v,float); assert (math.isnan(v) and math.isnan(e)) or (v==e and math.copysign(1,v)==math.copysign(1,e))"])
            .arg(&wire).arg(token).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        service.stop_and_join().unwrap();
    }
}
