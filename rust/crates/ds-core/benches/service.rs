use std::hint::black_box;
use std::time::Instant;

use ds_core::terminal::control::PaneTable;
use ds_core::terminal::parse::PANE_SEP;

const PANE_COUNT: usize = 1_000;
const WARMUP_ITERATIONS: usize = 1_000;
const SAMPLE_COUNT: usize = 500;
const ITERATIONS_PER_SAMPLE: usize = 20;

fn synthetic_panes(count: usize) -> String {
    let mut output = String::with_capacity(count * 112);
    for index in 0..count {
        use std::fmt::Write;
        writeln!(
            output,
            "%{index}{sep}$1{sep}@{index}{sep}/workspace/project{index}{sep}bash{sep}synthetic-pane-{index}{sep}0{sep}session{sep}window{sep}{index}{sep}0{sep}{}{sep}120{sep}40",
            10_000 + index,
            sep = PANE_SEP,
        )
        .expect("writing to a String cannot fail");
    }
    output
}

fn percentile(sorted: &[u128], percent: usize) -> u128 {
    sorted[(sorted.len() - 1) * percent / 100]
}

fn bench_list_panes_1000() {
    let output = synthetic_panes(PANE_COUNT);
    let mut table = PaneTable::default();
    let loaded = table.load_panes(&output);
    assert_eq!(PANE_COUNT, loaded, "all synthetic rows must parse");
    assert_eq!(PANE_COUNT, table.list_panes().len());

    for _ in 0..WARMUP_ITERATIONS {
        black_box(table.list_panes());
    }

    let mut samples_ns_per_call = Vec::with_capacity(SAMPLE_COUNT);
    for _ in 0..SAMPLE_COUNT {
        let started = Instant::now();
        for _ in 0..ITERATIONS_PER_SAMPLE {
            black_box(table.list_panes());
        }
        samples_ns_per_call.push(started.elapsed().as_nanos() / ITERATIONS_PER_SAMPLE as u128);
    }
    samples_ns_per_call.sort_unstable();

    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string());
    println!("benchmark: bench_list_panes_1000");
    println!(
        "host: {host} ({} {})",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("panes: {PANE_COUNT}");
    println!("warmup iterations: {WARMUP_ITERATIONS}");
    println!(
        "samples: {SAMPLE_COUNT} x {ITERATIONS_PER_SAMPLE} calls = {} measured calls",
        SAMPLE_COUNT * ITERATIONS_PER_SAMPLE
    );
    let p50 = percentile(&samples_ns_per_call, 50);
    println!("p50: {p50} ns/call");
    println!("p95: {} ns/call", percentile(&samples_ns_per_call, 95));
    println!("p99: {} ns/call", percentile(&samples_ns_per_call, 99));
    println!(
        "max: {} ns/call",
        samples_ns_per_call[samples_ns_per_call.len() - 1]
    );
    println!("target p50 < 50 ms: {}", p50 < 50_000_000);
}

// RPC measurements deliberately use the production dispatcher. Only terminal discovery
// is a controlled fixture; authentication, handlers, stores and socket framing are real.
const RPC_WARMUP: usize = 20;
const RPC_SAMPLES: usize = 200;
const STATUS_CLIENTS: usize = 30;
const WORKER: &str = "w1:p1";
const MESSAGE: &str = "RPC benchmark seeded inbox message";

struct Client {
    stream: std::io::BufReader<std::os::unix::net::UnixStream>,
    sequence: u64,
}

impl Client {
    fn connect(path: &std::path::Path) -> Self {
        let stream = std::os::unix::net::UnixStream::connect(path).unwrap();
        let timeout = Some(std::time::Duration::from_secs(30));
        stream.set_read_timeout(timeout).unwrap();
        stream.set_write_timeout(timeout).unwrap();
        let mut client = Self {
            stream: std::io::BufReader::new(stream),
            sequence: 0,
        };
        let identity = client.call("whoami", serde_json::json!({}));
        assert_eq!(identity["pane_id"], WORKER);
        assert_eq!(identity["role"], "worker");
        assert_eq!(identity["bound"], true, "real peer must bind: {identity}");
        assert!(identity["caller"]["pid"].as_i64().unwrap() > 0);
        assert!(!identity["caller"]["start"].as_str().unwrap().is_empty());
        client
    }

    fn call(&mut self, command: &str, args: serde_json::Value) -> serde_json::Value {
        use std::io::{BufRead, Write};
        self.sequence += 1;
        let request = serde_json::json!({
            "id": self.sequence, "cmd": command, "args": args, "self_pane_id": WORKER
        });
        serde_json::to_writer(self.stream.get_mut(), &request).unwrap();
        self.stream.get_mut().write_all(b"\n").unwrap();
        let mut line = String::new();
        assert!(self.stream.read_line(&mut line).unwrap() > 0, "RPC EOF");
        let mut response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], self.sequence);
        assert_eq!(response["ok"], true, "{command}: {response}");
        response["result"].take()
    }
}

struct RpcFixture {
    services: std::sync::Arc<ds_core::server_runtime::RuntimeServices>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
    socket: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

impl RpcFixture {
    fn new() -> Self {
        use ds_core::server_runtime::{
            RuntimeObservability, RuntimeServiceInputs, RuntimeServices,
        };
        use std::sync::Arc;
        let root = std::env::var_os("DS_BENCH_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        let directory = tempfile::Builder::new()
            .prefix(".rpc-")
            .tempdir_in(root)
            .unwrap();
        let dir = directory.path();
        let socket = dir.join("rpc.sock");
        assert!(
            socket.as_os_str().len() < 104,
            "set DS_BENCH_DIR to a shorter workspace path"
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(ds_core::tasks::Tasks::open(dir.join("tasks.db")))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let inbox = Arc::new(
            ds_core::inbox_store::Store::open(dir.join("inbox.db"), dir.join("blobs")).unwrap(),
        );
        let env = ds_core::config::Env::default();
        let mut roles = ds_core::roles::Roles::new(
            &env,
            Some(dir.join("roles.json")),
            Some(dir.join("tokens.json")),
            None,
        )
        .unwrap();
        // Use the SAME process resolver as the server, with the actual socket peer PID.
        // The pane root is the resolved live caller, never a nonexistent canned PID.
        let caller = ds_core::proc::caller_from_peer(
            Some(i64::from(std::process::id())),
            Some(WORKER.into()),
        );
        let root_pid = caller.pid.expect("live caller PID");
        assert!(ds_core::proc::caller_alive(Some(&caller)));
        roles
            .register(
                WORKER,
                "worker",
                Some(root_pid),
                caller.start.as_deref(),
                Some("codex"),
                None,
                None,
                None,
            )
            .unwrap();
        assert_ne!(
            std::env::var("DUNKINGSHEEP_BIND_IDENTITY").as_deref(),
            Ok("0")
        );
        let services = RuntimeServices::new(RuntimeServiceInputs {
            launches_file: dir.join("launches.json"),
            lane_registry_file: dir.join("lanes.json"),
            persist_enabled: false,
            stores: ds_core::flock::panes::Stores {
                tasks: Some(tasks),
                inbox: Some(inbox),
                ..Default::default()
            },
            roles,
            legacy_superusers: None,
            release_version: "rpc-benchmark".into(),
            state_file: dir.join("state.json"),
            executable: std::env::current_exe().unwrap(),
            env,
            observability: Some(RuntimeObservability {
                state_dir: dir.join("state"),
                memory_db: None,
            }),
            runtime,
            terminal_factory: Arc::new(move || {
                let mut terminal = ds_core::terminal::fakes::FakeTerminal::new();
                terminal.panes.truncate(1);
                terminal.panes[0].pane_pid = Some(root_pid);
                Box::new(terminal)
            }),
        })
        .unwrap();
        let mut server =
            ds_core::server::FlockServer::new(services.production_dispatcher(), &socket);
        server.bind().unwrap();
        let stop = server.stop_handle();
        let server = std::thread::spawn(move || server.serve_forever().unwrap());
        Self {
            services,
            stop,
            server: Some(server),
            socket,
            _directory: directory,
        }
    }

    fn seed_message(&self) -> String {
        self.services
            .flock()
            .stores()
            .inbox
            .unwrap()
            .send(&ds_core::inbox_store::Outgoing {
                mailbox: WORKER,
                body: Some(MESSAGE),
                ..Default::default()
            })
            .unwrap()
    }
}

impl Drop for RpcFixture {
    fn drop(&mut self) {
        // Callers have received every response and dropped their sockets before this.
        // FlockServer does not expose its internal connection JoinHandles: this is
        // NOT evidence for the separate production request-thread drain gate.
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
        self.services.shutdown();
    }
}

fn report_rpc(name: &str, mut samples: Vec<u128>, clients: usize, ceiling_ns: u128) {
    assert_eq!(samples.len(), RPC_SAMPLES * clients);
    assert!(!samples.is_empty());
    samples.sort_unstable();
    println!("benchmark: {name}");
    println!("persistent clients: {clients}; warmup calls/client: {RPC_WARMUP}");
    println!(
        "samples/client: {RPC_SAMPLES}; measured successful RPCs: {}",
        samples.len()
    );
    println!("p50: {} ns/RPC", percentile(&samples, 50));
    println!("p95: {} ns/RPC", percentile(&samples, 95));
    println!("p99: {} ns/RPC", percentile(&samples, 99));
    println!("max: {} ns/RPC", samples.last().unwrap());
    println!(
        "CEILING p50 < {ceiling_ns} ns: {}",
        if percentile(&samples, 50) < ceiling_ns {
            "PASS"
        } else {
            "FAIL"
        }
    );
}

fn bench_status_30_clients() {
    let fixture = RpcFixture::new();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(STATUS_CLIENTS));
    let mut clients = Vec::new();
    let connected = (0..STATUS_CLIENTS)
        .map(|_| Client::connect(&fixture.socket))
        .collect::<Vec<_>>();
    for mut client in connected {
        let barrier = std::sync::Arc::clone(&barrier);
        clients.push(std::thread::spawn(move || {
            let mut samples = Vec::with_capacity(RPC_SAMPLES);
            // All 30 established clients start together; waiting is outside timing.
            barrier.wait();
            for iteration in 0..RPC_WARMUP + RPC_SAMPLES {
                let started = Instant::now();
                let result = client.call("status", serde_json::json!({}));
                let elapsed = started.elapsed().as_nanos();
                assert_eq!(result["version"], "rpc-benchmark");
                assert_eq!(result["pid"], std::process::id());
                assert!(result["tasks"].is_object(), "real task totals: {result}");
                if iteration >= RPC_WARMUP {
                    samples.push(elapsed);
                }
            }
            samples
        }));
    }
    let joined = clients
        .into_iter()
        .map(|thread| thread.join())
        .collect::<Vec<_>>();
    let samples = joined.into_iter().flat_map(Result::unwrap).collect();
    report_rpc(
        "bench_status_30_clients",
        samples,
        STATUS_CLIENTS,
        2_000_000,
    );
}

fn bench_inbox_read() {
    let fixture = RpcFixture::new();
    let mut client = Client::connect(&fixture.socket);
    let mut samples = Vec::with_capacity(RPC_SAMPLES);
    for iteration in 0..RPC_WARMUP + RPC_SAMPLES {
        // A fresh unread message per call, inserted outside the timed section.
        let id = fixture.seed_message();
        let started = Instant::now();
        let result = client.call("inbox_read", serde_json::json!({}));
        let elapsed = started.elapsed().as_nanos();
        assert_eq!(result["mailbox"], WORKER);
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            1,
            "each read must consume its seeded message"
        );
        assert_eq!(messages[0]["message_id"], id);
        assert_eq!(messages[0]["body"], MESSAGE);
        assert_eq!(result["marked_read"], 1);
        assert_eq!(
            fixture
                .services
                .flock()
                .stores()
                .inbox
                .unwrap()
                .unread_for_pane(WORKER)
                .unwrap(),
            0
        );
        if iteration >= RPC_WARMUP {
            samples.push(elapsed);
        }
    }
    drop(client);
    report_rpc("bench_inbox_read", samples, 1, 5_000_000);
}

fn bench_task_next() {
    let fixture = RpcFixture::new();
    let mut client = Client::connect(&fixture.socket);
    let task = client.call("task_add", serde_json::json!({
        "title": "RPC benchmark claim", "body": "Isolated fixture, repeatedly claimed then released",
        "scope": "global", "assignees": [WORKER]
    }));
    let id = task["id"].as_str().unwrap();
    let mut samples = Vec::with_capacity(RPC_SAMPLES);
    for iteration in 0..RPC_WARMUP + RPC_SAMPLES {
        let started = Instant::now();
        let result = client.call("task_next", serde_json::json!({"scope":"global"}));
        let elapsed = started.elapsed().as_nanos();
        assert_eq!(
            result["task"]["id"], id,
            "must successfully claim a real task: {result}"
        );
        assert_eq!(result["task"]["status"], "in_progress");
        let released = client.call("task_release", serde_json::json!({"id":id}));
        assert_eq!(released["status"], "open");
        if iteration >= RPC_WARMUP {
            samples.push(elapsed);
        }
    }
    drop(client);
    report_rpc("bench_task_next", samples, 1, 10_000_000);
}

fn main() {
    let requested = std::env::args()
        .skip(1)
        .filter(|arg| arg != "--bench")
        .collect::<Vec<_>>();
    let benchmarks: [(&str, fn()); 4] = [
        ("bench_list_panes_1000", bench_list_panes_1000),
        ("bench_status_30_clients", bench_status_30_clients),
        ("bench_inbox_read", bench_inbox_read),
        ("bench_task_next", bench_task_next),
    ];
    for name in &requested {
        assert!(
            benchmarks.iter().any(|(known, _)| known == name),
            "unknown benchmark filter: {name}"
        );
    }
    let revision = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(revision.status.success());
    println!(
        "revision: {}",
        String::from_utf8_lossy(&revision.stdout).trim()
    );
    println!(
        "host: {} ({} {})",
        std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into()),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "CPU: {}",
        std::fs::read_to_string("/proc/cpuinfo")
            .unwrap_or_default()
            .lines()
            .find(|line| line.starts_with("model name"))
            .unwrap_or("unknown")
    );
    println!(
        "load before: {}",
        std::fs::read_to_string("/proc/loadavg")
            .unwrap_or_else(|_| "unknown".into())
            .trim()
    );
    for (name, benchmark) in benchmarks {
        if requested.is_empty() || requested.iter().any(|arg| arg == name) {
            benchmark();
        }
    }
    println!(
        "load after: {}",
        std::fs::read_to_string("/proc/loadavg")
            .unwrap_or_else(|_| "unknown".into())
            .trim()
    );
}
