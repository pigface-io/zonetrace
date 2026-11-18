//! What a zone costs with the feature `heap` and its allocator installed,
//! and what booking one allocator call costs: an allocation and its free
//! through the counting allocator against the same through the system's,
//! inside a zone, as code that uses the crate has them. See
//! `shared/cases.rs`.

// The system allocator is called directly, to time it without the
// counting around it: `GlobalAlloc`'s methods are unsafe to call.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;

use zonetrace::heap::Counting;
use zonetrace::{Zone, enter, take};

#[path = "shared/cases.rs"]
mod cases;

#[global_allocator]
static ALLOC: Counting<System> = Counting::new(System);

static ALLOCS: Zone = Zone::new(3, "allocs");

/// One allocation of 64 bytes and its free, through `a`.
fn pair(a: &dyn GlobalAlloc) {
    let layout = Layout::new::<[u64; 8]>();
    // SAFETY: a non-zero layout, freed with the same layout.
    unsafe {
        let p = black_box(a.alloc(layout));
        if !p.is_null() {
            a.dealloc(p, layout);
        }
    }
}

fn main() {
    let mut results = cases::zones();
    let z = enter(&ALLOCS);
    let raw = cases::time(|| pair(&System));
    let counted = cases::time(|| pair(&ALLOC));
    drop(z);
    let _ = take();
    results.push(("alloc_pair_raw", raw));
    results.push(("alloc_pair_counted", counted));
    // The two pairs are timed for the record; only the zones meet the
    // ceiling, and the booking's cost is what `heap::overhead` subtracts.
    let zones: Vec<_> = results
        .iter()
        .copied()
        .filter(|(n, _)| !n.starts_with("alloc_pair"))
        .collect();
    cases::report(&zones, cases::CEILING);
    for (name, ns) in results.iter().filter(|(n, _)| n.starts_with("alloc_pair")) {
        println!("zones/{name:<24} {ns:>7.2} ns");
    }
    println!(
        "zones/{:<24} {:>7.2} ns",
        "booking_per_event",
        ((counted - raw) / 2.0).max(0.0)
    );
    println!(
        "zones/{:<24} {:>7.2} ns",
        "heap::overhead()",
        zonetrace::heap::overhead()
    );
    println!(
        "zones/{:<24} {:>7.2} ns",
        "overhead()",
        zonetrace::overhead()
    );
}
