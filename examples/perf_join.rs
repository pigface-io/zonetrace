//! Two zones of different work, recorded for `zonetrace-join`:
//!
//! ```text
//! perf record -k realtime -e cycles -- \
//!     cargo run --release --features timeline --example perf_join -- {timeline}
//! perf script -F tid,time,period,event --ns | zonetrace-join {timeline}
//! ```

use std::hint::black_box;
use zonetrace::{Zone, enter};

static RUN: Zone = Zone::new(1, "run");
static SUM: Zone = Zone::new(2, "sum");
static SORT: Zone = Zone::new(3, "sort");

fn main() -> std::io::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "perf_join.timeline".into());
    zonetrace::timeline::start();
    {
        let _run = zonetrace::enter_deep(&RUN);
        for round in 0..20u64 {
            {
                let _z = enter(&SUM);
                let mut s = 0u64;
                for i in 0..2_000_000 {
                    s = s.wrapping_add(black_box(i ^ round));
                }
                black_box(s);
            }
            {
                let _z = enter(&SORT);
                let mut v: Vec<u64> = (0..200_000)
                    .map(|i| (i * 2_654_435_761) % 1_000_003)
                    .collect();
                v.sort_unstable();
                black_box(&v);
            }
        }
    }
    let t = zonetrace::timeline::stop().expect("recording");
    t.write(&mut std::fs::File::create(&path)?)?;
    println!("{} events to {path}", t.len());
    print!("{}", zonetrace::take().text(zonetrace::overhead()));
    Ok(())
}
