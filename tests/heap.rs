//! The feature `heap` with its allocator installed: every allocator call
//! is booked to the zone that made it, the tracer's own to `zones.tracer`,
//! and nothing is lost or counted twice.

use std::cell::RefCell;
use std::hint::black_box;
use std::sync::Mutex;

use zonetrace::heap::{Counting, thread_totals};
use zonetrace::{Heap, Report, Row, Zone, add, enter, flush, take, take_all};

#[global_allocator]
static ALLOC: Counting<std::alloc::System> = Counting::new(std::alloc::System);

static OUTER: Zone = Zone::new(1, "outer");
static INNER: Zone = Zone::new(2, "inner");
static OTHER: Zone = Zone::new(3, "other");

/// The tests that use the process's report run one at a time.
static PROCESS_REPORT: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    PROCESS_REPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn row<'a>(r: &'a Report, name: &str) -> &'a Row {
    r.rows()
        .iter()
        .find(|x| x.name == name)
        .unwrap_or_else(|| panic!("no row {name}: {:?}", r.rows()))
}

fn heap_of(r: &Report, name: &str) -> Heap {
    row(r, name).heap.unwrap_or_default()
}

#[test]
fn an_allocation_is_booked_to_the_zone_that_made_it() {
    let _ = take();
    {
        let _o = enter(&OUTER);
        let v: Vec<u8> = Vec::with_capacity(1000);
        black_box(&v);
        {
            let _i = enter(&INNER);
            let w: Vec<u8> = Vec::with_capacity(500);
            drop(black_box(w));
        }
        drop(v);
    }
    let r = take();
    assert_eq!(
        heap_of(&r, "outer"),
        Heap {
            allocs: 1,
            bytes: 1000,
            frees: 1,
            freed: 1000,
            // The inner call's 500 on top of the 1000 still live.
            peak: 1500,
            ..Heap::default()
        }
    );
    assert_eq!(
        heap_of(&r, "inner"),
        Heap {
            allocs: 1,
            bytes: 500,
            frees: 1,
            freed: 500,
            peak: 500,
            ..Heap::default()
        }
    );
}

#[test]
fn a_buffer_that_grows_is_a_reallocation_and_its_growth() {
    let _ = take();
    {
        let _o = enter(&OUTER);
        let mut v: Vec<u8> = Vec::with_capacity(8);
        v.extend_from_slice(&[0; 8]);
        v.push(1);
        black_box(&v);
        v.truncate(1);
        v.shrink_to_fit();
        black_box(&v);
    }
    let h = heap_of(&take(), "outer");
    assert_eq!((h.allocs, h.reallocs), (1, 2), "{h:?}");
    // 8, grown to 16, shrunk to 1, freed.
    assert_eq!(h.realloc_bytes, 8, "{h:?}");
    assert_eq!(h.bytes, 16, "{h:?}");
    assert_eq!(h.freed, 16, "{h:?}");
    assert_eq!(h.frees, 1, "{h:?}");
    assert_eq!(h.peak, 16, "{h:?}");
}

#[test]
fn a_zeroed_allocation_is_an_allocation() {
    let _ = take();
    {
        let _o = enter(&OUTER);
        drop(black_box(vec![0u8; 300]));
    }
    let h = heap_of(&take(), "outer");
    assert_eq!((h.allocs, h.bytes, h.frees, h.freed), (1, 300, 1, 300));
}

#[test]
fn a_zone_that_allocates_nothing_has_no_heap_row() {
    let _ = take();
    drop(enter(&OUTER));
    let r = take();
    assert_eq!(row(&r, "outer").heap, None);
    // Its first entry grew the tree: the tracer's, shown on a row of its own.
    assert!(heap_of(&r, "zones.tracer").allocs > 0);
}

/// Enters OUTER and INNER in turn, `depth` deep, allocating nothing.
fn nest(depth: u32) {
    if depth == 0 {
        return;
    }
    let _z = enter(if depth % 2 == 0 { &OUTER } else { &INNER });
    nest(depth - 1);
}

#[test]
fn the_tracer_s_own_allocations_are_never_a_zone_s() {
    // 10,000 new paths: every first entry grows the tree, past the fixed
    // counters too, and all of it is the tracer's.
    let worker = std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(|| {
            // Held to the end: a report dropped is freed where it drops.
            let first = take();
            let (before, tracer_before) = thread_totals();
            nest(10_000);
            let (after, tracer_after) = thread_totals();
            let r = take();
            drop(first);
            (r, after.since(&before), tracer_after.since(&tracer_before))
        })
        .expect("a thread");
    let (r, counted, tracer) = worker.join().expect("joined");
    assert_eq!(counted.events(), 0, "{counted:?}");
    assert!(tracer.allocs > 0, "{tracer:?}");
    let charged: Vec<&Row> = r
        .rows()
        .iter()
        .filter(|x| x.heap.is_some() && x.name != "zones.tracer")
        .collect();
    assert!(charged.is_empty(), "{charged:?}");
    assert!(heap_of(&r, "zones.tracer").allocs > 0);
}

#[test]
fn reading_merging_and_printing_a_report_charge_nothing_to_the_open_zone() {
    let _serial = serial();
    let _ = take();
    {
        let _o = enter(&OTHER);
        drop(Vec::<u8>::with_capacity(64));
    }
    let made = take();
    let (before, _) = thread_totals();
    {
        let _o = enter(&OUTER);
        let mut merged = Report::default();
        merged.merge(&made);
        let text = merged.text(1.0);
        let rows = merged.ndjson(1.0);
        let net = merged.net_times(1.0);
        black_box((&text, &rows, &net));
        static PATH: [&Zone; 1] = [&INNER];
        add(&PATH, 1, 10);
        drop(zonetrace::stretch(&PATH));
        // What the tracer made is dropped here, in the zone: a free is
        // booked where it happens, but nothing was allocated.
        drop((merged, text, rows, net));
    }
    let (after, _) = thread_totals();
    let spent = after.since(&before);
    assert_eq!(
        (spent.allocs, spent.bytes, spent.reallocs),
        (0, 0, 0),
        "{spent:?}"
    );
    let r = take();
    assert_eq!(row(&r, "outer").heap.map_or(0, |h| h.allocs), 0);
    // flush and take_all: the tracer's as well.
    let (before, _) = thread_totals();
    {
        let _o = enter(&OUTER);
        flush();
        let all = take_all();
        black_box(&all);
    }
    let (after, _) = thread_totals();
    assert_eq!(after.since(&before).allocs, 0);
}

/// A small generator, so the sequences repeat without a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Random zones entered and left, buffers allocated, grown, shrunk and
/// freed among them.
fn wander(seed: u64) {
    let zones = [&OUTER, &INNER, &OTHER];
    let mut rng = Rng(seed.max(1));
    let mut open = Vec::with_capacity(64);
    let mut held: Vec<Vec<u8>> = Vec::with_capacity(256);
    for _ in 0..2_000 {
        match rng.below(6) {
            0 if open.len() < 32 => {
                let z = zones[usize::try_from(rng.below(3)).unwrap_or(0)];
                open.push(enter(z));
            }
            1 => drop(open.pop()),
            2 if held.len() < 200 => {
                let n = usize::try_from(rng.below(4096)).unwrap_or(1) + 1;
                held.push(Vec::with_capacity(n));
            }
            3 if !held.is_empty() => {
                let i = usize::try_from(rng.below(held.len() as u64)).unwrap_or(0);
                drop(held.swap_remove(i));
            }
            4 if !held.is_empty() => {
                let i = usize::try_from(rng.below(held.len() as u64)).unwrap_or(0);
                let extra = usize::try_from(rng.below(8192)).unwrap_or(0) + 1;
                let cap = held[i].capacity();
                held[i].reserve_exact(cap + extra);
            }
            5 if !held.is_empty() => {
                let i = usize::try_from(rng.below(held.len() as u64)).unwrap_or(0);
                held[i].shrink_to(usize::try_from(rng.below(64)).unwrap_or(0));
            }
            _ => {}
        }
    }
    while let Some(z) = open.pop() {
        drop(z);
    }
    drop(held);
}

fn sum_of(r: &Report, keep: impl Fn(&Row) -> bool) -> Heap {
    let mut s = Heap::default();
    for x in r.rows().iter().filter(|x| keep(x)) {
        if let Some(h) = &x.heap {
            s.add(h);
        }
    }
    s.peak = 0;
    s
}

#[test]
fn every_event_is_booked_once_somewhere() {
    for seed in 1..=200u64 {
        // Held to the end: a report dropped is freed where it drops.
        let first = take();
        let (before, tracer_before) = thread_totals();
        wander(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let r = take();
        // After the take: what it allocated is the tracer's, never counted.
        let (after, tracer_after) = thread_totals();
        drop(first);
        let mut counted = after.since(&before);
        counted.peak = 0;
        let in_rows = sum_of(&r, |x| x.name != "zones.tracer");
        assert_eq!(in_rows, counted, "seed {seed}");
        // The tracer's row is everything the tracer did, the take included.
        let mut tracer = tracer_after.since(&tracer_before);
        tracer.peak = 0;
        assert_eq!(heap_of(&r, "zones.tracer"), tracer, "seed {seed}");
    }
}

#[test]
fn a_node_past_the_fixed_counters_books_to_unplaced() {
    let worker = std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(|| {
            fn deep(depth: u32) {
                if depth == 0 {
                    drop(black_box(Vec::<u8>::with_capacity(77)));
                    return;
                }
                let _z = enter(if depth % 2 == 0 { &OUTER } else { &INNER });
                deep(depth - 1);
            }
            let _ = take();
            deep(600);
            take()
        })
        .expect("a thread");
    let r = worker.join().expect("joined");
    let h = heap_of(&r, "zones.unplaced");
    assert_eq!((h.allocs, h.bytes, h.frees), (1, 77, 1));
}

#[test]
fn threads_flush_their_heap_into_one_report() {
    let _serial = serial();
    let _ = take_all();
    let workers: Vec<_> = (0..8u64)
        .map(|i| {
            std::thread::spawn(move || {
                {
                    let _o = enter(&OUTER);
                    let n = usize::try_from(1000 + i).unwrap_or(1000);
                    drop(black_box(Vec::<u8>::with_capacity(n)));
                }
                flush();
            })
        })
        .collect();
    for w in workers {
        w.join().expect("a worker");
    }
    let all = take_all();
    let h = heap_of(&all, "outer");
    assert_eq!((h.allocs, h.frees), (8, 8), "{h:?}");
    assert_eq!(h.bytes, (0..8).map(|i| 1000 + i).sum::<u64>());
    assert_eq!(h.peak, 1007);
}

#[test]
fn a_block_freed_on_another_thread_is_booked_where_it_is_freed() {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let maker = std::thread::spawn(move || {
        let _ = take();
        let v = {
            let _o = enter(&OUTER);
            Vec::with_capacity(4096)
        };
        // Sent outside the zone: a channel allocates a block of its own.
        tx.send(v).expect("sent");
        take()
    });
    let made = maker.join().expect("maker");
    let freer = std::thread::spawn(move || {
        let v = rx.recv().expect("received");
        let _ = take();
        {
            let _i = enter(&INNER);
            drop(v);
        }
        take()
    });
    let freed = freer.join().expect("freer");
    let a = heap_of(&made, "outer");
    assert_eq!((a.allocs, a.bytes, a.frees), (1, 4096, 0));
    let b = heap_of(&freed, "inner");
    assert_eq!((b.allocs, b.frees, b.freed), (0, 1, 4096));
}

thread_local! {
    /// Allocates and frees as the thread's locals are torn down.
    static LATE: RefCell<Option<Late>> = const { RefCell::new(None) };
}

struct Late(Vec<u8>);

impl Drop for Late {
    fn drop(&mut self) {
        let v = vec![1u8; 64];
        black_box((&v, &self.0));
    }
}

#[test]
fn threads_that_end_while_allocating_do_not_trip_the_books() {
    for _ in 0..1_000 {
        std::thread::spawn(|| {
            LATE.with_borrow_mut(|l| *l = Some(Late(vec![0; 32])));
            let _o = enter(&OUTER);
            drop(black_box(Vec::<u8>::with_capacity(16)));
        })
        .join()
        .expect("a thread");
    }
}

#[test]
fn the_cost_of_booking_comes_off_every_row_above_it() {
    let _ = take();
    {
        let _o = enter(&OUTER);
        for _ in 0..1000 {
            let _i = enter(&INNER);
            drop(black_box(Vec::<u8>::with_capacity(64)));
        }
    }
    let r = take();
    let raw = r.net_times_with(0.0, 0.0);
    let net = r.net_times_with(1.0, 2.0);
    // Outer: 1000 zones beneath at 1 ns, 2000 events beneath at 2 ns.
    assert!((raw[0].0 - net[0].0 - 1000.0 - 4000.0).abs() < 1e-6);
    // Inner: no zone beneath, its own 2000 events.
    assert!((raw[1].0 - net[1].0 - 4000.0).abs() < 1e-6);
    // The rows that count no time stay at zero.
    let books = r
        .rows()
        .iter()
        .position(|x| x.name == "zones.tracer")
        .expect("the tracer's row");
    assert_eq!(net[books], (0.0, 0.0));
}

#[test]
fn measured_costs_leave_a_parent_of_allocating_zones_its_own_time_only() {
    // The tracer's time measured, then taken out: what is left in the
    // outer zone's self time is the loop's, a few nanoseconds a turn.
    let zone = zonetrace::overhead();
    let event = zonetrace::heap::overhead();
    assert!((0.0..1_000.0).contains(&event), "{event}");
    let turns = 100_000u32;
    let mut best = f64::MAX;
    for _ in 0..5 {
        let _ = take();
        {
            let _o = enter(&OUTER);
            for _ in 0..turns {
                let _i = enter(&INNER);
                drop(black_box(Vec::<u8>::with_capacity(64)));
            }
        }
        let r = take();
        let net = r.net_times_with(zone, event);
        best = best.min(net[0].1 / f64::from(turns));
    }
    // Less than one zone's cost left, plus the loop's few nanoseconds: on
    // a machine where a zone costs 20 ns, an error in either subtraction
    // would leave that much or more.
    assert!(
        best < zone + 3.0,
        "outer self {best:.2} ns a turn; a zone costs {zone:.2}"
    );
    // Not taking the costs out leaves both in, a turn's worth at least.
    let _ = take();
    {
        let _o = enter(&OUTER);
        for _ in 0..turns {
            let _i = enter(&INNER);
            drop(black_box(Vec::<u8>::with_capacity(64)));
        }
    }
    let raw = take().net_times_with(0.0, 0.0)[0].1 / f64::from(turns);
    assert!(raw > best, "raw {raw:.2} against net {best:.2}");
}

#[test]
fn the_formats_carry_the_heap() {
    let _ = take();
    {
        let _o = enter(&OUTER);
        drop(black_box(Vec::<u8>::with_capacity(2_000_000)));
        drop(enter(&INNER));
    }
    let r = take();
    let text = r.text(0.0);
    let head = text.lines().next().unwrap_or("");
    assert!(
        head.contains("alloc MB") && head.contains("peak MB"),
        "{head}"
    );
    let outer = text.lines().nth(1).unwrap_or("");
    assert!(outer.contains("2.0"), "{outer}");
    let inner = text.lines().nth(2).unwrap_or("");
    assert!(
        inner.split_whitespace().filter(|w| *w == "-").count() >= 5,
        "{inner}"
    );
    let rows = r.ndjson(0.0);
    let first = rows.lines().next().unwrap_or("");
    assert!(
        first.ends_with(
            r#","heap":{"bytes":2000000,"allocs":1,"reallocs":0,"realloc_bytes":0,"frees":1,"freed":2000000,"peak":2000000}}"#
        ),
        "{first}"
    );
    // Reports merge their heap: counts sum, peaks are the greater.
    let mut a = r.clone();
    a.merge(&r);
    let h = heap_of(&a, "outer");
    assert_eq!((h.allocs, h.bytes, h.peak), (2, 4_000_000, 2_000_000));
}
