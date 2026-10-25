//! What a zone costs: an empty zone at the top, one nested in another, and
//! the two byte counters. The budget is what `overhead` subtracts; these
//! keep it honest. No harness: each case is the median of seven rounds of
//! ten million calls, timed by `Instant` around the round, and a case over
//! its ceiling fails the run.

use std::hint::black_box;
use std::time::Instant;
use zonetrace::{Zone, copied, enter, read, take};

static OUTER: Zone = Zone::new(1, "outer");
static INNER: Zone = Zone::new(2, "inner");

const CALLS: u32 = 10_000_000;
const ROUNDS: usize = 7;

/// Nanoseconds a call of `f`, the median round.
fn time(mut f: impl FnMut()) -> f64 {
    for _ in 0..CALLS / 10 {
        f();
    }
    let mut rounds: Vec<f64> = (0..ROUNDS)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..CALLS {
                f();
            }
            start.elapsed().as_secs_f64() * 1e9 / f64::from(CALLS)
        })
        .collect();
    rounds.sort_by(f64::total_cmp);
    rounds[ROUNDS / 2]
}

fn main() {
    // Twice the cost: it catches a tracer gone wrong, not a noisy machine.
    // A zone is two reads of the CPU's counter and a few loads and stores;
    // the reads cost under half a nanosecond each on Apple silicon, where
    // the hook runs, about 8 ns as `rdtsc` on x86_64, and about 12 ns on
    // the Neoverse machines of a cloud's aarch64 Linux.
    const CEILING: f64 = if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        8.0
    } else {
        50.0
    };
    let mut results = vec![("enter_exit_top", time(|| drop(black_box(enter(&OUTER)))))];
    let outer = enter(&OUTER);
    results.push(("enter_exit_nested", time(|| drop(black_box(enter(&INNER))))));
    results.push((
        "count_read_and_copied",
        time(|| {
            read(black_box(8192));
            copied(black_box(8192));
        }),
    ));
    drop(outer);
    let _ = take();
    for (name, ns) in &results {
        println!("zones/{name:<24} {ns:>7.2} ns");
    }
    println!(
        "zones/{:<24} {:>7.2} ns",
        "overhead()",
        zonetrace::overhead()
    );
    let over: Vec<_> = results.iter().filter(|(_, ns)| *ns > CEILING).collect();
    assert!(over.is_empty(), "over {CEILING} ns: {over:?}");
}
