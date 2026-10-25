//! Zones: named stretches of code, timed per thread into a call tree.
//!
//! A zone is declared once, as a static, where the code it times lives:
//!
//! ```
//! use zonetrace::{Zone, enter};
//!
//! static LOAD: Zone = Zone::new(1, "chunk.load");
//!
//! fn load() {
//!     let _z = enter(&LOAD);
//!     // ... the work the zone times ...
//!     zonetrace::read(4096);
//! }
//! # load();
//! # let report = zonetrace::take();
//! # assert_eq!(report.rows()[0].calls, 1);
//! ```
//!
//! Each thread keeps a tree of nodes, one per path of zones (a zone entered
//! from two different zones is two nodes), and each node links to the node
//! it ran in. A node sums its calls, its time, and the bytes the code in it
//! said it read and copied. Nothing is recorded per call, and memory stays
//! the size of the tree.
//!
//! The hot path borrows nothing and allocates nothing. A zone entered again
//! from where it was last entered (in a loop, every call but the first)
//! finds its node in a small cache by zone number, without a look at the
//! tree; its counters are cells in a fixed array that a thread-local
//! reaches without a borrow. The time is two reads of the CPU's counter,
//! under half a nanosecond each, where the system clock behind `Instant`
//! costs about 18 on macOS. Ticks become nanoseconds only when the tree is
//! taken. An empty zone costs about 4 ns in all.
//!
//! A zone opened with [`enter_deep`] also sums what the kernel counted for
//! the thread over each call ([`Kernel`]); with the feature `timeline`,
//! [`timeline`] records a thread's zones in time, for [`join`] to hand
//! `perf` samples of the CPU's counters to the zones they fell in.
//!
//! The tracer's own cost lands in the zone that encloses each call.
//! [`overhead`] measures it on the running machine, and [`Report`] takes it
//! out of every node's time by the number of zones entered beneath it, so a
//! zone entered a million times does not inflate its parent.

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::marker::PhantomData;
use std::time::Instant;

mod clock;
#[cfg(feature = "timeline")]
pub mod join;
mod kernel;
#[cfg(feature = "timeline")]
pub mod timeline;

pub use kernel::Kernel;

/// A named stretch of code, declared once as a static.
#[derive(Debug)]
pub struct Zone {
    id: u16,
    name: &'static str,
}

impl Zone {
    /// A zone with a number unique among the zones one report shows, and a
    /// dotted name, e.g. `chunk.load`.
    #[must_use]
    pub const fn new(id: u16, name: &'static str) -> Self {
        Self { id, name }
    }

    /// Its number.
    #[must_use]
    pub const fn id(&self) -> u16 {
        self.id
    }

    /// Its name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }
}

/// No node: the parent of a top-level node, and where a thread is when no
/// zone is open.
const NONE: u32 = u32::MAX;

/// Nodes whose counters live in the fixed array [`Hot::counts`]; a tree
/// with more keeps the rest in [`Tree::spill`], behind a borrow.
const HOT_NODES: usize = 512;

/// Entries of the cache of where each zone was last entered.
const SLOTS: usize = 64;

/// What one node has summed. Cells, so the hot path updates them with
/// plain loads and stores and no borrow.
#[derive(Debug)]
struct Counts {
    calls: Cell<u64>,
    /// Counter ticks, not nanoseconds.
    ticks: Cell<u64>,
    read: Cell<u64>,
    copied: Cell<u64>,
}

impl Counts {
    const fn new() -> Self {
        Self {
            calls: Cell::new(0),
            ticks: Cell::new(0),
            read: Cell::new(0),
            copied: Cell::new(0),
        }
    }

    /// The sums, and zero left behind.
    fn take(&self) -> [u64; 4] {
        [
            self.calls.take(),
            self.ticks.take(),
            self.read.take(),
            self.copied.take(),
        ]
    }
}

/// Where a zone was last entered: from which node, as which node.
#[derive(Debug, Clone, Copy)]
struct Slot {
    zone: Option<&'static Zone>,
    parent: u32,
    node: u32,
}

const EMPTY: Slot = Slot {
    zone: None,
    parent: NONE,
    node: NONE,
};

/// A thread's hot state: nothing in it needs dropping, so reaching it is a
/// thread-local address and no more, and nothing in it is borrowed.
#[derive(Debug)]
struct Hot {
    /// The node the thread is in.
    at: Cell<u32>,
    /// Bumped by [`take`]: a zone opened before it does not count in the
    /// tree after it.
    epoch: Cell<u32>,
    /// Whether the thread's zones are being recorded in time (feature
    /// `timeline`): here, so a zone learns it from the state it reads
    /// anyway.
    recording: Cell<bool>,
    /// By zone number, modulo its length: a zone entered again from the
    /// node it was last entered from is that node again, without a look at
    /// the tree. In a loop that is every call but the first.
    cache: [Cell<Slot>; SLOTS],
    counts: [Counts; HOT_NODES],
}

impl Hot {
    /// The counters of `node`, when they are in the fixed array.
    fn counts(&self, node: u32) -> Option<&Counts> {
        self.counts.get(node as usize)
    }
}

/// One path of zones on one thread: its place in the tree. What it has
/// summed is in [`Hot`], or in [`Tree::spill`] past [`HOT_NODES`].
#[derive(Debug)]
struct Node {
    zone: &'static Zone,
    parent: u32,
    /// Children in the order they were first entered: few, so a scan beats
    /// a map.
    children: Vec<u32>,
    /// What the kernel counted over its deep calls, when it had any.
    kernel: Option<Kernel>,
}

/// A thread's tree: looked at only when a zone is entered from a node it
/// has not been entered from lately, and when the tree is taken.
#[derive(Debug)]
struct Tree {
    nodes: Vec<Node>,
    roots: Vec<u32>,
    /// The counters of nodes past [`HOT_NODES`], in node order.
    spill: Vec<Counts>,
}

impl Tree {
    const fn new() -> Self {
        Self {
            nodes: Vec::new(),
            roots: Vec::new(),
            spill: Vec::new(),
        }
    }

    /// The node `zone` is under `at`, made on its first entry.
    fn find(&mut self, at: u32, zone: &'static Zone) -> u32 {
        let siblings = match self.nodes.get(at as usize) {
            Some(n) => &n.children,
            None => &self.roots,
        };
        let found = siblings
            .iter()
            .copied()
            .find(|&c| std::ptr::eq(self.nodes[c as usize].zone, zone));
        found.unwrap_or_else(|| {
            let id = u32::try_from(self.nodes.len()).unwrap_or(NONE - 1);
            self.nodes.push(Node {
                zone,
                parent: at,
                children: Vec::new(),
                kernel: None,
            });
            if self.nodes.len() > HOT_NODES {
                self.spill.push(Counts::new());
            }
            match self.nodes.get_mut(at as usize) {
                Some(p) => p.children.push(id),
                None => self.roots.push(id),
            }
            id
        })
    }

    /// Runs `f` on the counters of `node`, past the fixed array.
    fn spilled(&self, node: u32, f: impl FnOnce(&Counts)) {
        f(&self.spill[node as usize - HOT_NODES]);
    }
}

thread_local! {
    static HOT: Hot = const {
        Hot {
            at: Cell::new(NONE),
            epoch: Cell::new(0),
            recording: Cell::new(false),
            cache: [const { Cell::new(EMPTY) }; SLOTS],
            counts: [const { Counts::new() }; HOT_NODES],
        }
    };
    static TREE: RefCell<Tree> = const { RefCell::new(Tree::new()) };
}

/// Starts or stops recording this thread's zones in time.
#[cfg(feature = "timeline")]
fn set_recording(on: bool) -> bool {
    HOT.with(|h| h.recording.replace(on))
}

/// Runs `f` on the counters of `node`, wherever they are.
fn with_counts(hot: &Hot, node: u32, f: impl FnOnce(&Counts)) {
    match hot.counts(node) {
        Some(c) => f(c),
        None => TREE.with_borrow(|t| t.spilled(node, f)),
    }
}

/// An open zone; dropping it ends the call. It belongs to the thread that
/// opened it.
#[derive(Debug)]
#[must_use = "a zone ends when its guard drops"]
pub struct Guard {
    node: u32,
    parent: u32,
    epoch: u32,
    start: u64,
    _thread: PhantomData<*const ()>,
}

/// Opens `zone` inside whatever zone is open on this thread.
#[inline]
pub fn enter(zone: &'static Zone) -> Guard {
    let (node, parent, epoch, recording) = HOT.with(|h| {
        let at = h.at.get();
        let slot = &h.cache[usize::from(zone.id) % SLOTS];
        let last = slot.get();
        let node = if last.parent == at && last.zone.is_some_and(|z| std::ptr::eq(z, zone)) {
            last.node
        } else {
            let node = TREE.with_borrow_mut(|t| t.find(at, zone));
            slot.set(Slot {
                zone: Some(zone),
                parent: at,
                node,
            });
            node
        };
        h.at.set(node);
        (node, at, h.epoch.get(), h.recording.get())
    });
    // The clock is read after the bookkeeping on entry and before it on
    // exit, so the zone's own time leaves the tracer out; its parent's
    // does not, and the report takes that out.
    #[cfg(feature = "timeline")]
    if recording {
        timeline::enter(zone, clock::ticks());
    }
    #[cfg(not(feature = "timeline"))]
    let _ = recording;
    Guard {
        node,
        parent,
        epoch,
        start: clock::ticks(),
        _thread: PhantomData,
    }
}

impl Drop for Guard {
    #[inline]
    fn drop(&mut self) {
        let end = clock::ticks();
        let ticks = end.wrapping_sub(self.start);
        HOT.with(|h| {
            #[cfg(feature = "timeline")]
            if h.recording.get() {
                timeline::exit(end);
            }
            if h.epoch.get() != self.epoch {
                return;
            }
            h.at.set(self.parent);
            with_counts(h, self.node, |c| {
                c.calls.set(c.calls.get() + 1);
                c.ticks.set(c.ticks.get() + ticks);
            });
        });
    }
}

/// An open deep zone: a zone whose calls also sum what the kernel counted
/// for the thread while they ran. Dropping it ends the call.
#[derive(Debug)]
#[must_use = "a zone ends when its guard drops"]
pub struct DeepGuard {
    guard: Option<Guard>,
    start: Option<Kernel>,
}

/// Opens `zone` as [`enter`] does, and also sums what the kernel counted
/// for this thread over the call (see [`Kernel`]): on Linux, from the
/// thread's files under `/proc`; elsewhere nothing is added. The two reads
/// of those files cost a few microseconds, outside the zone's own time and
/// inside its parent's, so a deep zone is for a stretch of a millisecond or
/// more, never an inner loop.
pub fn enter_deep(zone: &'static Zone) -> DeepGuard {
    let start = kernel::snapshot();
    DeepGuard {
        guard: Some(enter(zone)),
        start,
    }
}

impl Drop for DeepGuard {
    fn drop(&mut self) {
        // The call ends before the kernel is asked, so the second read
        // falls outside the zone's time, as the first does.
        let ended = self.guard.take().map(|g| {
            let at = (g.node, g.epoch);
            drop(g);
            at
        });
        if let (Some((node, epoch)), Some(start), Some(end)) =
            (ended, self.start, kernel::snapshot())
        {
            if HOT.with(|h| h.epoch.get()) == epoch {
                let spent = end.since(&start);
                TREE.with_borrow_mut(|t| {
                    let n = &mut t.nodes[node as usize];
                    n.kernel.get_or_insert_with(Kernel::default).add(&spent);
                });
            }
        }
    }
}

/// Adds `n` to the counter `pick` chooses, of the open zone.
#[inline(always)]
fn count(n: u64, pick: impl Fn(&Counts) -> &Cell<u64>) {
    HOT.with(|h| {
        let at = h.at.get();
        if let Some(c) = h.counts(at) {
            let cell = pick(c);
            cell.set(cell.get() + n);
        } else if at != NONE {
            TREE.with_borrow(|t| {
                t.spilled(at, |c| {
                    let cell = pick(c);
                    cell.set(cell.get() + n);
                });
            });
        }
    });
}

/// `n` bytes read from the source (a system call, a request) in the open
/// zone. Outside any zone it is not counted.
#[inline]
pub fn read(n: u64) {
    count(n, |c| &c.read);
}

/// `n` bytes copied from one buffer to another in the open zone. Outside
/// any zone it is not counted.
#[inline]
pub fn copied(n: u64) {
    count(n, |c| &c.copied);
}

/// Calls of the empty zone [`overhead`] times.
const CALIBRATION_CALLS: u32 = 100_000;

static CALIBRATION: Zone = Zone::new(u16::MAX, "zones.calibration");

/// What one zone costs the zone around it, in nanoseconds: the wall time
/// of entering and leaving an empty zone, the median of five runs. It runs
/// on a thread of its own, so no thread's tree is touched.
#[must_use]
pub fn overhead() -> f64 {
    std::thread::spawn(|| {
        let mut runs: Vec<f64> = (0..5)
            .map(|_| {
                let start = Instant::now();
                for _ in 0..CALIBRATION_CALLS {
                    drop(enter(&CALIBRATION));
                }
                start.elapsed().as_secs_f64() * 1e9 / f64::from(CALIBRATION_CALLS)
            })
            .collect();
        runs.sort_by(f64::total_cmp);
        runs[2]
    })
    .join()
    .unwrap_or(0.0)
}

/// One node of a report: a path of zones, and what ran in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Its number in the report.
    pub id: u32,
    /// The row it ran in; `None` at the top.
    pub parent: Option<u32>,
    /// The zone's number.
    pub zone: u16,
    /// The zone's name.
    pub name: &'static str,
    /// Calls ended.
    pub calls: u64,
    /// Wall time of every call, as measured, tracer included.
    pub nanos: u64,
    /// Bytes read from the source.
    pub read: u64,
    /// Bytes copied between buffers.
    pub copied: u64,
    /// What the kernel counted over its deep calls (see [`enter_deep`]).
    pub kernel: Option<Kernel>,
    /// Rows that ran in it.
    pub children: Vec<u32>,
}

/// A thread's call tree, taken by [`take`]; reports of several threads
/// [`merge`](Report::merge) into one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    rows: Vec<Row>,
    roots: Vec<u32>,
}

/// This thread's tree, as a report; the thread starts a new one. Zones
/// open while it is taken are not counted in either.
#[must_use]
pub fn take() -> Report {
    let tree = TREE.with_borrow_mut(|t| std::mem::replace(t, Tree::new()));
    let hot: Vec<[u64; 4]> = HOT.with(|h| {
        h.at.set(NONE);
        h.epoch.set(h.epoch.get().wrapping_add(1));
        for slot in &h.cache {
            slot.set(EMPTY);
        }
        h.counts[..tree.nodes.len().min(HOT_NODES)]
            .iter()
            .map(Counts::take)
            .collect()
    });
    let counts = hot.into_iter().chain(tree.spill.iter().map(Counts::take));
    let rows = tree
        .nodes
        .into_iter()
        .zip(counts)
        .zip(0u32..)
        .map(|((n, [calls, ticks, read, copied]), id)| Row {
            id,
            parent: (n.parent != NONE).then_some(n.parent),
            zone: n.zone.id,
            name: n.zone.name,
            calls,
            nanos: clock::nanos(ticks),
            read,
            copied,
            kernel: n.kernel,
            children: n.children,
        })
        .collect();
    Report {
        rows,
        roots: tree.roots,
    }
}

impl Report {
    /// Every row, parents before their children.
    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Adds `other` in: rows on the same path of zone numbers are summed,
    /// the others are added.
    pub fn merge(&mut self, other: &Self) {
        for &r in &other.roots {
            self.merge_row(other, r, None);
        }
    }

    fn merge_row(&mut self, other: &Self, at: u32, parent: Option<u32>) {
        let o = &other.rows[at as usize];
        let siblings = match parent {
            Some(p) => &self.rows[p as usize].children,
            None => &self.roots,
        };
        let found = siblings
            .iter()
            .copied()
            .find(|&c| self.rows[c as usize].zone == o.zone);
        let id = if let Some(id) = found {
            let r = &mut self.rows[id as usize];
            r.calls += o.calls;
            r.nanos += o.nanos;
            r.read += o.read;
            r.copied += o.copied;
            if let Some(k) = &o.kernel {
                r.kernel.get_or_insert_with(Kernel::default).add(k);
            }
            id
        } else {
            let id = u32::try_from(self.rows.len()).unwrap_or(NONE - 1);
            self.rows.push(Row {
                id,
                parent,
                children: Vec::new(),
                ..o.clone()
            });
            match parent {
                Some(p) => self.rows[p as usize].children.push(id),
                None => self.roots.push(id),
            }
            id
        };
        for &c in &o.children {
            self.merge_row(other, c, Some(id));
        }
    }

    /// Each row's time with the tracer taken out, and the part of it not
    /// spent in its children, in nanoseconds, by row number. One pass from
    /// the last row up: a row's number is always above its parent's, so the
    /// calls beneath every row are summed before its parent needs them.
    #[must_use]
    pub fn net_times(&self, overhead: f64) -> Vec<(f64, f64)> {
        let mut beneath = vec![0u64; self.rows.len()];
        for r in self.rows.iter().rev() {
            if let Some(p) = r.parent {
                beneath[p as usize] += r.calls + beneath[r.id as usize];
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let net: Vec<f64> = self
            .rows
            .iter()
            .zip(&beneath)
            .map(|(r, &b)| (r.nanos as f64 - b as f64 * overhead).max(0.0))
            .collect();
        self.rows
            .iter()
            .zip(&net)
            .map(|(r, &total)| {
                let inner: f64 = r.children.iter().map(|&c| net[c as usize]).sum();
                (total, (total - inner).max(0.0))
            })
            .collect()
    }

    /// The tree as text: one line per row, indented under its parent, with
    /// calls, net time and its share of the top rows, net self time, MB read
    /// and copied, and copies per byte read. When a row has kernel counts
    /// (see [`enter_deep`]), every line also shows time on a CPU, time
    /// waiting for one, time off both (blocked: the net time less the two),
    /// major faults, voluntary switches, and MB storage moved.
    #[must_use]
    pub fn text(&self, overhead: f64) -> String {
        let net = self.net_times(overhead);
        let whole: f64 = self.roots.iter().map(|&r| net[r as usize].0).sum();
        let deep = self.rows.iter().any(|r| r.kernel.is_some());
        let mut out = format!(
            "{:<40} {:>10} {:>11} {:>6} {:>11} {:>10} {:>10} {:>6}",
            "zone", "calls", "total ms", "%", "self ms", "read MB", "copy MB", "copy/r"
        );
        if deep {
            let _ = write!(
                out,
                " {:>10} {:>10} {:>10} {:>7} {:>8} {:>9}",
                "cpu ms", "wait ms", "off ms", "majflt", "switches", "disk MB"
            );
        }
        out.push('\n');
        let mut stack: Vec<(u32, usize)> = self.roots.iter().rev().map(|&r| (r, 0)).collect();
        while let Some((id, depth)) = stack.pop() {
            let r = &self.rows[id as usize];
            let (total, own) = net[id as usize];
            #[allow(clippy::cast_precision_loss)]
            let (read, copied) = (r.read as f64, r.copied as f64);
            let ratio = if r.read == 0 {
                "-".to_owned()
            } else {
                format!("{:.2}", copied / read)
            };
            let share = if whole > 0.0 {
                total / whole * 100.0
            } else {
                0.0
            };
            let label = format!("{:indent$}{}", "", r.name, indent = depth * 2);
            let _ = write!(
                out,
                "{label:<40} {:>10} {:>11.3} {share:>6.1} {:>11.3} {:>10.1} {:>10.1} {ratio:>6}",
                r.calls,
                total / 1e6,
                own / 1e6,
                read / 1e6,
                copied / 1e6,
            );
            match (deep, &r.kernel) {
                (true, Some(k)) => {
                    #[allow(clippy::cast_precision_loss)]
                    let (cpu, wait, disk) = (
                        k.cpu_ns as f64,
                        k.wait_ns as f64,
                        (k.read_bytes + k.write_bytes) as f64,
                    );
                    let off = (total - cpu - wait).max(0.0);
                    let _ = write!(
                        out,
                        " {:>10.3} {:>10.3} {:>10.3} {:>7} {:>8} {:>9.1}",
                        cpu / 1e6,
                        wait / 1e6,
                        off / 1e6,
                        k.major_faults,
                        k.voluntary_switches,
                        disk / 1e6,
                    );
                }
                (true, None) => {
                    let _ = write!(
                        out,
                        " {:>10} {:>10} {:>10} {:>7} {:>8} {:>9}",
                        "-", "-", "-", "-", "-", "-"
                    );
                }
                (false, _) => {}
            }
            out.push('\n');
            stack.extend(r.children.iter().rev().map(|&c| (c, depth + 1)));
        }
        out
    }

    /// The tree as NDJSON, one row a line, for diffing runs: `id`,
    /// `parent`, `zone`, `name`, `calls`, `nanos` as measured, `net` and
    /// `self` with the tracer taken out, `read`, `copied`, and `kernel`
    /// when the row has kernel counts.
    #[must_use]
    pub fn ndjson(&self, overhead: f64) -> String {
        let net = self.net_times(overhead);
        let mut out = String::new();
        for (r, (total, own)) in self.rows.iter().zip(net) {
            let parent = r
                .parent
                .map_or_else(|| "null".to_owned(), |p| p.to_string());
            let _ = write!(
                out,
                r#"{{"id":{},"parent":{parent},"zone":{},"name":"{}","calls":{},"nanos":{},"net":{total:.0},"self":{own:.0},"read":{},"copied":{}"#,
                r.id, r.zone, r.name, r.calls, r.nanos, r.read, r.copied,
            );
            if let Some(k) = &r.kernel {
                let _ = write!(
                    out,
                    r#","kernel":{{"cpu_ns":{},"wait_ns":{},"minor_faults":{},"major_faults":{},"voluntary_switches":{},"involuntary_switches":{},"read_bytes":{},"write_bytes":{}}}"#,
                    k.cpu_ns,
                    k.wait_ns,
                    k.minor_faults,
                    k.major_faults,
                    k.voluntary_switches,
                    k.involuntary_switches,
                    k.read_bytes,
                    k.write_bytes,
                );
            }
            out.push_str("}\n");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static OUTER: Zone = Zone::new(1, "outer");
    static INNER: Zone = Zone::new(2, "inner");
    static OTHER: Zone = Zone::new(3, "other");

    fn spin(micros: u64) {
        let start = Instant::now();
        while start.elapsed().as_micros() < u128::from(micros) {
            std::hint::spin_loop();
        }
    }

    #[test]
    fn a_zone_is_its_number_and_name() {
        assert_eq!(OUTER.id(), 1);
        assert_eq!(OUTER.name(), "outer");
        let made = Zone::new(9, "made");
        assert_eq!((made.id(), made.name()), (9, "made"));
    }

    #[test]
    fn zones_nest_by_path_and_sum_their_calls() {
        let _ = take();
        for _ in 0..3 {
            let _o = enter(&OUTER);
            {
                let _i = enter(&INNER);
                read(10);
                copied(20);
                spin(50);
            }
            let _i = enter(&INNER);
        }
        // The same zone from another parent is another node.
        drop(enter(&INNER));
        let r = take();
        let rows = r.rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            (rows[0].name, rows[0].calls, rows[0].parent),
            ("outer", 3, None)
        );
        assert_eq!(
            (rows[1].name, rows[1].calls, rows[1].parent),
            ("inner", 6, Some(0))
        );
        assert_eq!((rows[1].read, rows[1].copied), (30, 60));
        assert_eq!((rows[2].name, rows[2].parent), ("inner", None));
        assert!(rows[0].nanos >= rows[1].nanos);
        assert!(rows[1].nanos >= 150_000);
        assert_eq!(rows[0].children, vec![1]);
    }

    /// Enters OUTER and INNER in turn, `depth` deep, counting a byte read
    /// at each level: every level is a node of its own.
    fn nest(depth: u32) {
        if depth == 0 {
            return;
        }
        let _z = enter(if depth.is_multiple_of(2) {
            &OUTER
        } else {
            &INNER
        });
        read(1);
        copied(1);
        nest(depth - 1);
    }

    #[test]
    fn a_tree_past_the_fixed_counters_counts_the_same() {
        let deep = u32::try_from(HOT_NODES).expect("small") + 100;
        let _ = take();
        nest(deep);
        nest(deep);
        let r = take();
        assert_eq!(r.rows().len(), deep as usize);
        assert!(
            r.rows()
                .iter()
                .all(|row| row.calls == 2 && row.read == 2 && row.copied == 2)
        );
        assert_eq!(
            r.rows()[HOT_NODES + 50].parent,
            Some(u32::try_from(HOT_NODES).expect("small") + 49)
        );
    }

    #[test]
    fn a_zone_open_while_the_tree_is_taken_counts_in_neither() {
        let _ = take();
        let o = enter(&OUTER);
        // Taken while open: the zone is in the tree, with no call ended.
        let first = take();
        assert_eq!((first.rows().len(), first.rows()[0].calls), (1, 0));
        {
            let _i = enter(&INNER);
            read(3);
        }
        drop(o);
        let r = take();
        assert_eq!(r.rows().len(), 1);
        assert_eq!(
            (r.rows()[0].name, r.rows()[0].parent, r.rows()[0].read),
            ("inner", None, 3)
        );
    }

    #[test]
    fn a_deep_zone_is_a_zone_with_kernel_counts_where_there_are_some() {
        let _ = take();
        {
            let _d = enter_deep(&OUTER);
            drop(enter(&INNER));
        }
        let r = take();
        assert_eq!((r.rows()[0].name, r.rows()[0].calls), ("outer", 1));
        assert_eq!(r.rows()[0].kernel.is_some(), cfg!(target_os = "linux"));
        assert_eq!(r.rows()[1].kernel, None);
    }

    #[test]
    fn a_deep_zone_open_while_the_tree_is_taken_counts_in_neither() {
        let _ = take();
        kernel::fake(Some(Kernel::default()));
        let d = enter_deep(&OUTER);
        let _ = take();
        drop(d);
        assert!(take().rows().is_empty());
        kernel::fake(None);
    }

    #[test]
    fn a_deep_zone_sums_what_the_kernel_counted_over_each_call() {
        let _ = take();
        for _ in 0..2 {
            kernel::fake(Some(Kernel::default()));
            let _d = enter_deep(&OUTER);
            kernel::fake(Some(Kernel {
                cpu_ns: 5,
                ..Kernel::default()
            }));
        }
        kernel::fake(None);
        let r = take();
        // On Linux the kernel's own counts stand in for the fake ones.
        let cpu = r.rows()[0].kernel.map(|k| k.cpu_ns);
        #[cfg(target_os = "linux")]
        assert!(cpu.is_some());
        #[cfg(not(target_os = "linux"))]
        assert_eq!(cpu, Some(10));
    }

    #[test]
    fn kernel_counts_merge_print_and_serialise() {
        let k = Kernel {
            cpu_ns: 2_000_000,
            wait_ns: 1_000_000,
            major_faults: 4,
            voluntary_switches: 7,
            read_bytes: 3_000_000,
            ..Kernel::default()
        };
        let row = |id, parent, kernel| Row {
            id,
            parent,
            zone: u16::try_from(id + 1).expect("small"),
            name: if id == 0 { "outer" } else { "inner" },
            calls: 1,
            nanos: 10_000_000,
            read: 0,
            copied: 0,
            kernel,
            children: if id == 0 { vec![1] } else { Vec::new() },
        };
        let mut a = Report {
            rows: vec![row(0, None, Some(k)), row(1, Some(0), None)],
            roots: vec![0],
        };
        let b = a.clone();
        a.merge(&b);
        let got = a.rows()[0].kernel.expect("summed");
        assert_eq!((got.cpu_ns, got.major_faults), (4_000_000, 8));
        let text = a.text(0.0);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].ends_with("disk MB"));
        // 20 ms net, 4 on a CPU, 2 waiting: 14 off both.
        assert!(lines[1].contains("14.000"), "{}", lines[1]);
        assert!(lines[2].ends_with('-'));
        let json = a.ndjson(0.0);
        assert!(json.lines().next().expect("a line").ends_with(
            r#""kernel":{"cpu_ns":4000000,"wait_ns":2000000,"minor_faults":0,"major_faults":8,"voluntary_switches":14,"involuntary_switches":0,"read_bytes":6000000,"write_bytes":0}}"#
        ));
        assert!(
            json.lines()
                .nth(1)
                .expect("a line")
                .ends_with(r#""copied":0}"#)
        );
    }

    #[test]
    fn counts_outside_any_zone_are_dropped() {
        let _ = take();
        read(5);
        copied(5);
        assert!(take().rows().is_empty());
    }

    #[test]
    fn the_tracer_is_taken_out_of_the_parent() {
        let _ = take();
        {
            let _o = enter(&OUTER);
            for _ in 0..1000 {
                drop(enter(&INNER));
            }
        }
        let r = take();
        let raw = r.net_times(0.0);
        let net = r.net_times(1.0);
        // 1000 inner calls at 1 ns each come off the outer total.
        assert!((raw[0].0 - net[0].0 - 1000.0).abs() < 1e-6);
        // A huge overhead never makes a time negative; a leaf keeps its own.
        let huge = r.net_times(1e12);
        assert_eq!(huge[0], (0.0, 0.0));
        assert_eq!(huge[1], raw[1]);
    }

    #[test]
    fn the_tracer_is_taken_out_at_every_depth() {
        let _ = take();
        {
            let _o = enter(&OUTER);
            for _ in 0..10 {
                let _i = enter(&INNER);
                for _ in 0..10 {
                    drop(enter(&OTHER));
                }
            }
        }
        let r = take();
        let raw = r.net_times(0.0);
        let net = r.net_times(1.0);
        // Outer has 10 inner and 100 other beneath it; inner has the 100.
        assert!((raw[0].0 - net[0].0 - 110.0).abs() < 1e-6);
        assert!((raw[1].0 - net[1].0 - 100.0).abs() < 1e-6);
        assert_eq!(raw[2], net[2]);
    }

    #[test]
    fn overhead_is_small_and_leaves_the_tree_alone() {
        let _ = take();
        let _o = enter(&OUTER);
        let cost = overhead();
        assert!(cost > 0.0 && cost < 10_000.0, "{cost}");
        drop(_o);
        let r = take();
        assert_eq!(r.rows().len(), 1);
        assert_eq!(r.rows()[0].name, "outer");
    }

    #[test]
    fn reports_merge_by_path() {
        let _ = take();
        {
            let _o = enter(&OUTER);
            let _i = enter(&INNER);
            read(1);
        }
        let mut a = take();
        let b = std::thread::spawn(|| {
            {
                let _o = enter(&OUTER);
                let _i = enter(&INNER);
                read(2);
                let _x = enter(&OTHER);
            }
            drop(enter(&OTHER));
            take()
        })
        .join()
        .expect("thread");
        a.merge(&b);
        let rows = a.rows();
        assert_eq!(rows.len(), 4);
        assert_eq!((rows[0].calls, rows[1].calls, rows[1].read), (2, 2, 3));
        assert_eq!((rows[2].name, rows[2].parent), ("other", Some(1)));
        assert_eq!((rows[3].name, rows[3].parent), ("other", None));
    }

    #[test]
    fn text_and_ndjson_show_every_row() {
        let _ = take();
        {
            let _o = enter(&OUTER);
            let _i = enter(&INNER);
            read(1_000_000);
            copied(2_000_000);
        }
        drop(enter(&OTHER));
        let r = take();
        let text = r.text(0.0);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("zone"));
        assert!(lines[1].starts_with("outer "));
        assert!(lines[2].starts_with("  inner "));
        assert!(lines[2].ends_with("2.00"));
        assert!(lines[1].ends_with('-'));
        assert!(lines[3].starts_with("other "));
        let json = r.ndjson(0.0);
        let lines: Vec<&str> = json.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with(r#"{"id":0,"parent":null,"zone":1,"name":"outer","calls":1,"#)
        );
        assert!(lines[1].contains(r#""parent":0"#));
        assert!(lines[1].ends_with(r#""read":1000000,"copied":2000000}"#));
    }

    #[test]
    fn an_empty_report_has_no_share() {
        let r = Report::default();
        assert_eq!(r.text(0.0).lines().count(), 1);
        let _ = take();
        {
            let _o = enter(&OUTER);
            drop(enter(&INNER));
        }
        let r = take();
        // A top row whose time is all the tracer's still prints, with no
        // share to divide by.
        assert!(r.text(1e12).contains("outer"));
    }
}
