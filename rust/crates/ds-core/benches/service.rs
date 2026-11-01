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
    println!("p50: {} ns/call", percentile(&samples_ns_per_call, 50));
    println!("p95: {} ns/call", percentile(&samples_ns_per_call, 95));
    println!("p99: {} ns/call", percentile(&samples_ns_per_call, 99));
    println!(
        "max: {} ns/call",
        samples_ns_per_call[samples_ns_per_call.len() - 1]
    );
}

fn main() {
    let requested = std::env::args().skip(1).collect::<Vec<_>>();
    if !requested.is_empty() && !requested.iter().any(|arg| arg == "bench_list_panes_1000") {
        eprintln!("unknown benchmark filter: {}", requested.join(" "));
        std::process::exit(2);
    }
    bench_list_panes_1000();
}
