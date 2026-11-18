//! The counting itself: the allocator, the books each call goes to, and
//! the cost of booking. The allocator is the one thing here safe Rust
//! cannot say: `GlobalAlloc` is an unsafe trait.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::OnceLock;
use std::time::Instant;

use super::Heap;
use crate::{HOT, Hot, NONE, Zone, enter};

/// One allocator call, as booked.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Event {
    Alloc(u64),
    Realloc { old: u64, new: u64 },
    Free(u64),
}

impl Event {
    /// What it did to the bytes live on the thread.
    #[allow(clippy::cast_possible_wrap)]
    const fn live(self) -> i64 {
        match self {
            Self::Alloc(n) => n as i64,
            Self::Realloc { old, new } => new as i64 - old as i64,
            Self::Free(n) => -(n as i64),
        }
    }
}

/// A node's or a book's heap counts: cells, as [`crate::Counts`] are.
#[derive(Debug)]
pub(crate) struct Cells {
    allocs: Cell<u64>,
    bytes: Cell<u64>,
    reallocs: Cell<u64>,
    realloc_bytes: Cell<u64>,
    frees: Cell<u64>,
    freed: Cell<u64>,
    /// The most bytes live above where a call began, over every call ended.
    peak: Cell<i64>,
}

impl Cells {
    pub(crate) const fn new() -> Self {
        Self {
            allocs: Cell::new(0),
            bytes: Cell::new(0),
            reallocs: Cell::new(0),
            realloc_bytes: Cell::new(0),
            frees: Cell::new(0),
            freed: Cell::new(0),
            peak: Cell::new(0),
        }
    }

    #[inline(always)]
    fn book(&self, e: Event) {
        let bump = |c: &Cell<u64>, n: u64| c.set(c.get() + n);
        match e {
            Event::Alloc(n) => {
                bump(&self.allocs, 1);
                bump(&self.bytes, n);
            }
            Event::Realloc { old, new } => {
                bump(&self.reallocs, 1);
                if new > old {
                    bump(&self.bytes, new - old);
                    bump(&self.realloc_bytes, new - old);
                } else {
                    bump(&self.freed, old - new);
                }
            }
            Event::Free(n) => {
                bump(&self.frees, 1);
                bump(&self.freed, n);
            }
        }
    }

    /// The counts, without taking them.
    pub(crate) fn get(&self) -> Heap {
        Heap {
            allocs: self.allocs.get(),
            bytes: self.bytes.get(),
            reallocs: self.reallocs.get(),
            realloc_bytes: self.realloc_bytes.get(),
            frees: self.frees.get(),
            freed: self.freed.get(),
            peak: u64::try_from(self.peak.get()).unwrap_or(0),
        }
    }

    /// The counts, or `None` when nothing was booked; zero left behind.
    pub(crate) fn take(&self) -> Option<Heap> {
        let h = self.get();
        for c in [
            &self.allocs,
            &self.bytes,
            &self.reallocs,
            &self.realloc_bytes,
            &self.frees,
            &self.freed,
        ] {
            c.set(0);
        }
        self.peak.set(0);
        (h.events() > 0).then_some(h)
    }

    /// A call's peak, kept when it is the most so far.
    #[inline(always)]
    pub(crate) fn keep_peak(&self, over: i64) {
        if over > self.peak.get() {
            self.peak.set(over);
        }
    }
}

/// Where the heap stood when a call began, held by its guard on the stack:
/// so a call's peak costs a zone two loads and three stores of the
/// thread's own cells, and no look at any node's.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mark {
    /// Live bytes when the call began.
    base: i64,
    /// The thread's high-water mark then, given back when the call ends.
    saved: i64,
}

impl Mark {
    /// A call begins: the high-water mark starts again from live.
    #[inline(always)]
    pub(crate) fn begin(b: &Books) -> Self {
        let live = b.live.get();
        Self {
            base: live,
            saved: b.high.replace(live),
        }
    }

    /// The call ends: its peak above its base, for `node` to keep; the
    /// mark it replaced is given back, raised to the call's, so the call's
    /// peak is its parent's too.
    #[inline(always)]
    pub(crate) fn end(self, b: &Books) -> i64 {
        let high = b.high.get();
        b.high.set(high.max(self.saved));
        high - self.base
    }
}

/// The thread's books beside its nodes': what no node takes.
#[derive(Debug)]
pub(crate) struct Books {
    /// Live bytes on the thread, by the zones' bookings (not the tracer's).
    pub(crate) live: Cell<i64>,
    /// The most `live` has been since the innermost open call began.
    high: Cell<i64>,
    /// Above zero while the tracer runs: its allocations are its own.
    pub(crate) tracer_depth: Cell<u32>,
    pub(crate) tracer: Cells,
    pub(crate) outside: Cells,
    pub(crate) unplaced: Cells,
    /// Since the thread began, never taken: what [`thread_totals`] gives.
    counted: Cells,
    tracer_total: Cells,
}

impl Books {
    pub(crate) const fn new() -> Self {
        Self {
            live: Cell::new(0),
            high: Cell::new(0),
            tracer_depth: Cell::new(0),
            tracer: Cells::new(),
            outside: Cells::new(),
            unplaced: Cells::new(),
            counted: Cells::new(),
            tracer_total: Cells::new(),
        }
    }
}

/// Books `e` to wherever this thread is. Called by the allocator, so it
/// never allocates, borrows or locks; on a thread whose state is gone (a
/// thread's last frees, as it exits) the event goes unbooked.
#[inline(always)]
fn book(e: Event) {
    let _ = HOT.try_with(|h| book_on(h, e));
}

#[inline(always)]
fn book_on(h: &Hot, e: Event) {
    let b = &h.books;
    if b.tracer_depth.get() > 0 {
        b.tracer.book(e);
        b.tracer_total.book(e);
        return;
    }
    b.counted.book(e);
    let live = b.live.get() + e.live();
    b.live.set(live);
    if live > b.high.get() {
        b.high.set(live);
    }
    let at = h.at.get();
    if at == NONE {
        b.outside.book(e);
        return;
    }
    match h.counts(at) {
        Some(c) => c.heap.book(e),
        None => b.unplaced.book(e),
    }
}

/// Runs `f` as the tracer: what it allocates is booked to `zones.tracer`.
pub(crate) fn as_tracer<R>(f: impl FnOnce() -> R) -> R {
    struct Leave;
    impl Drop for Leave {
        fn drop(&mut self) {
            HOT.with(|h| {
                let d = &h.books.tracer_depth;
                d.set(d.get() - 1);
            });
        }
    }
    HOT.with(|h| {
        let d = &h.books.tracer_depth;
        d.set(d.get() + 1);
    });
    let _leave = Leave;
    f()
}

/// This thread's counts since it began, never reset by a take: what the
/// zones, the time outside them and nodes past the fixed counters did, and
/// apart, what the tracer did. Their sum is every event the allocator
/// booked on the thread.
#[must_use]
pub fn thread_totals() -> (Heap, Heap) {
    HOT.with(|h| (h.books.counted.get(), h.books.tracer_total.get()))
}

/// The global allocator that books each call to the zone open on its
/// thread, around the allocator `A` that does the work.
#[derive(Debug, Default)]
pub struct Counting<A>(A);

impl<A> Counting<A> {
    /// `inner`, counted.
    pub const fn new(inner: A) -> Self {
        Self(inner)
    }
}

/// Bytes, from a layout's size.
#[inline(always)]
const fn size(n: usize) -> u64 {
    n as u64
}

// SAFETY: every method passes its arguments to `A` unchanged and returns
// what `A` returned, so `Counting<A>` keeps every promise `A` makes. The
// booking around each call touches only this thread's cells: it does not
// allocate (which would recurse), unwind, or take a lock.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Counting<A> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's promises about `layout` are passed on.
        let p = unsafe { self.0.alloc(layout) };
        if !p.is_null() {
            book(Event::Alloc(size(layout.size())));
        }
        p
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as `alloc`.
        let p = unsafe { self.0.alloc_zeroed(layout) };
        if !p.is_null() {
            book(Event::Alloc(size(layout.size())));
        }
        p
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's promises about `ptr` and `layout` are passed on.
        unsafe { self.0.dealloc(ptr, layout) };
        book(Event::Free(size(layout.size())));
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's promises are passed on.
        let p = unsafe { self.0.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            book(Event::Realloc {
                old: size(layout.size()),
                new: size(new_size),
            });
        }
        p
    }
}

/// Pairs of an allocation and its free [`overhead`] times.
const CALIBRATION_PAIRS: u32 = 200_000;

static CALIBRATION: Zone = Zone::new(u16::MAX - 4, "zones.heap_calibration");

/// What booking one allocator call costs the zone it falls in, in
/// nanoseconds: an allocation and its free through [`Counting`] less the
/// same through the system allocator, halved; the median of five runs, on
/// a thread of its own inside a zone, so no thread's tree is touched.
#[must_use]
pub fn overhead() -> f64 {
    std::thread::spawn(|| {
        let layout = Layout::from_size_align(64, 8).unwrap_or(Layout::new::<u64>());
        let counted = Counting::new(System);
        let _z = enter(&CALIBRATION);
        let pairs = |a: &dyn GlobalAlloc| {
            let start = Instant::now();
            for _ in 0..CALIBRATION_PAIRS {
                // SAFETY: a non-zero layout, freed with the same layout.
                unsafe {
                    let p = black_box(a.alloc(layout));
                    if !p.is_null() {
                        a.dealloc(p, layout);
                    }
                }
            }
            start.elapsed().as_secs_f64() * 1e9 / f64::from(CALIBRATION_PAIRS)
        };
        let mut runs: Vec<f64> = (0..5)
            .map(|_| ((pairs(&counted) - pairs(&System)) / 2.0).max(0.0))
            .collect();
        runs.sort_by(f64::total_cmp);
        runs[2]
    })
    .join()
    .unwrap_or(0.0)
}

/// [`overhead`], measured once per process: what a report takes out per
/// event when its rows counted the heap.
pub(crate) fn cost() -> f64 {
    static COST: OnceLock<f64> = OnceLock::new();
    *COST.get_or_init(overhead)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The books are made in the thread-local's constant; made at run
    /// time, they start empty too.
    #[test]
    fn new_books_are_empty() {
        let b = Books::new();
        assert_eq!(
            (b.live.get(), b.high.get(), b.tracer_depth.get()),
            (0, 0, 0)
        );
        assert_eq!(b.counted.get(), Heap::default());
        assert_eq!(b.tracer.take(), None);
    }

    static OUTER: Zone = Zone::new(1, "outer");
    static INNER: Zone = Zone::new(2, "inner");

    fn heap_of(r: &crate::Report, name: &str) -> Heap {
        r.rows()
            .iter()
            .find(|x| x.name == name)
            .and_then(|x| x.heap)
            .unwrap_or_default()
    }

    /// The unit tests' binary installs no counting allocator, so events
    /// are booked here by hand, as the allocator would book them.
    #[test]
    fn events_booked_by_hand_land_where_the_thread_is() {
        let _ = crate::take();
        book(Event::Alloc(10));
        {
            let _o = enter(&OUTER);
            book(Event::Alloc(100));
            {
                let _i = enter(&INNER);
                book(Event::Alloc(50));
                book(Event::Free(50));
            }
            book(Event::Realloc { old: 100, new: 160 });
            book(Event::Realloc { old: 160, new: 40 });
            book(Event::Free(40));
        }
        as_tracer(|| book(Event::Alloc(7)));
        // A zone that booked nothing: a row with no heap among those with.
        drop(enter(&INNER));
        let (counted, tracer) = thread_totals();
        let r = crate::take();
        assert_eq!(
            heap_of(&r, "outer"),
            Heap {
                allocs: 1,
                bytes: 160,
                reallocs: 2,
                realloc_bytes: 60,
                frees: 1,
                freed: 160,
                // 100 live, then the inner call's 50 on top; then 160.
                peak: 160,
            }
        );
        assert_eq!(heap_of(&r, "inner").peak, 50);
        assert_eq!(heap_of(&r, "zones.outside").bytes, 10);
        assert_eq!(heap_of(&r, "zones.tracer").bytes, 7);
        assert!(counted.events() >= 7 && tracer.allocs >= 1);
        // The formats and a merge carry them.
        let text = r.text(0.0);
        assert!(text.contains("alloc MB"));
        let bare = text.lines().find(|l| l.starts_with("inner ")).unwrap_or("");
        // copy/r, then the five heap columns.
        let dashes = bare.split_whitespace().filter(|w| *w == "-").count();
        assert_eq!(dashes, 6, "{bare}");
        assert!(r.ndjson(0.0).contains(r#""heap":{"bytes":160,"allocs":1"#));
        let mut m = r.clone();
        m.merge(&r);
        assert_eq!(heap_of(&m, "outer").bytes, 320);
        assert!(r.net_times(0.0).iter().all(|&(t, s)| t >= 0.0 && s >= 0.0));
    }

    #[test]
    fn a_node_past_the_fixed_counters_is_unplaced() {
        fn deep(depth: u32) {
            if depth == 0 {
                book(Event::Alloc(3));
                return;
            }
            let _z = enter(if depth % 2 == 0 { &OUTER } else { &INNER });
            deep(depth - 1);
        }
        let _ = crate::take();
        deep(600);
        assert_eq!(heap_of(&crate::take(), "zones.unplaced").bytes, 3);
    }

    #[test]
    fn the_allocator_passes_every_call_through() {
        let a = Counting::new(System);
        let layout = Layout::from_size_align(32, 8).unwrap_or(Layout::new::<u64>());
        let _ = crate::take();
        {
            let _o = enter(&OUTER);
            // SAFETY: each block is used within its layout and freed with it.
            unsafe {
                let p = a.alloc_zeroed(layout);
                assert!(!p.is_null() && *p == 0);
                let q = a.realloc(p, layout, 64);
                assert!(!q.is_null());
                a.dealloc(q, Layout::from_size_align(64, 8).unwrap_or(layout));
                let r = a.alloc(layout);
                a.dealloc(r, layout);
            }
        }
        let h = heap_of(&crate::take(), "outer");
        assert_eq!((h.allocs, h.reallocs, h.frees), (2, 1, 2));
        assert_eq!((h.bytes, h.realloc_bytes, h.freed), (96, 32, 96));
        assert!(format!("{:?}", Counting::<System>::default()).contains("Counting"));
    }

    #[test]
    fn booking_costs_little_and_is_measured_once() {
        let once = cost();
        assert!((0.0..1_000.0).contains(&once), "{once}");
        assert!((cost() - once).abs() < f64::EPSILON);
    }
}
