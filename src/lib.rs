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
//! said it read and copied. Nothing is recorded per call: once a path is
//! known, entering it costs a thread-local lookup among its parent's few
//! children and two reads of the CPU's counter (about a nanosecond each,
//! where the system clock behind `Instant` costs about 25 on macOS), and
//! memory stays the size of the tree. Ticks become nanoseconds only when
//! the tree is taken.
//!
//! The tracer's own cost lands in the zone that encloses each call.
//! [`overhead`] measures it on the running machine, and [`Report`] takes it
//! out of every node's time by the number of zones entered beneath it, so a
//! zone entered a million times does not inflate its parent.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::marker::PhantomData;
use std::sync::LazyLock;
use std::time::Instant;

use quanta::Clock;

/// The counter every zone reads, calibrated once per process.
static CLOCK: LazyLock<Clock> = LazyLock::new(Clock::new);

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

/// One path of zones on one thread.
#[derive(Debug)]
struct Node {
    zone: &'static Zone,
    parent: u32,
    calls: u64,
    /// Counter ticks, not nanoseconds.
    ticks: u64,
    read: u64,
    copied: u64,
    /// Children in the order they were first entered: few, so a scan beats
    /// a map.
    children: Vec<u32>,
}

/// A thread's tree, and the node it is in.
#[derive(Debug)]
struct Tree {
    nodes: Vec<Node>,
    roots: Vec<u32>,
    at: u32,
}

impl Tree {
    const fn new() -> Self {
        Self {
            nodes: Vec::new(),
            roots: Vec::new(),
            at: NONE,
        }
    }

    /// The node `zone` is under the current one, made on its first entry;
    /// it becomes the current node.
    fn enter(&mut self, zone: &'static Zone) -> u32 {
        let siblings = match self.nodes.get(self.at as usize) {
            Some(n) => &n.children,
            None => &self.roots,
        };
        let found = siblings
            .iter()
            .copied()
            .find(|&c| std::ptr::eq(self.nodes[c as usize].zone, zone));
        let node = found.unwrap_or_else(|| {
            let id = u32::try_from(self.nodes.len()).unwrap_or(NONE - 1);
            self.nodes.push(Node {
                zone,
                parent: self.at,
                calls: 0,
                ticks: 0,
                read: 0,
                copied: 0,
                children: Vec::new(),
            });
            match self.nodes.get_mut(self.at as usize) {
                Some(p) => p.children.push(id),
                None => self.roots.push(id),
            }
            id
        });
        self.at = node;
        node
    }

    /// One call of `node`, `ticks` long, ended; its parent is current again.
    fn exit(&mut self, node: u32, ticks: u64) {
        let n = &mut self.nodes[node as usize];
        n.calls += 1;
        n.ticks += ticks;
        self.at = n.parent;
    }

    /// The current node, if a zone is open.
    fn open(&mut self) -> Option<&mut Node> {
        self.nodes.get_mut(self.at as usize)
    }
}

thread_local! {
    static TREE: RefCell<Tree> = const { RefCell::new(Tree::new()) };
}

/// An open zone; dropping it ends the call. It belongs to the thread that
/// opened it.
#[derive(Debug)]
#[must_use = "a zone ends when its guard drops"]
pub struct Guard {
    node: u32,
    start: u64,
    _thread: PhantomData<*const ()>,
}

/// Opens `zone` inside whatever zone is open on this thread.
pub fn enter(zone: &'static Zone) -> Guard {
    let node = TREE.with_borrow_mut(|t| t.enter(zone));
    // The clock is read after the bookkeeping on entry and before it on
    // exit, so the zone's own time leaves the tracer out; its parent's
    // does not, and the report takes that out.
    Guard {
        node,
        start: CLOCK.raw(),
        _thread: PhantomData,
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let ticks = CLOCK.raw().wrapping_sub(self.start);
        TREE.with_borrow_mut(|t| t.exit(self.node, ticks));
    }
}

/// `n` bytes read from the source (a system call, a request) in the open
/// zone. Outside any zone it is not counted.
pub fn read(n: u64) {
    TREE.with_borrow_mut(|t| {
        if let Some(node) = t.open() {
            node.read += n;
        }
    });
}

/// `n` bytes copied from one buffer to another in the open zone. Outside
/// any zone it is not counted.
pub fn copied(n: u64) {
    TREE.with_borrow_mut(|t| {
        if let Some(node) = t.open() {
            node.copied += n;
        }
    });
}

/// Calls of the empty zone [`overhead`] times.
const CALIBRATION_CALLS: u32 = 100_000;

static CALIBRATION: Zone = Zone::new(u16::MAX, "zones.calibration");

/// What one zone costs the zone around it, in nanoseconds: the wall time
/// of entering and leaving an empty zone, the median of five runs on this
/// thread. The thread's tree is left as it was.
#[must_use]
pub fn overhead() -> f64 {
    let saved = TREE.with_borrow_mut(|t| std::mem::replace(t, Tree::new()));
    let mut runs: Vec<f64> = (0..5)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..CALIBRATION_CALLS {
                drop(enter(&CALIBRATION));
            }
            start.elapsed().as_secs_f64() * 1e9 / f64::from(CALIBRATION_CALLS)
        })
        .collect();
    TREE.with_borrow_mut(|t| *t = saved);
    runs.sort_by(f64::total_cmp);
    runs[2]
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

/// This thread's tree, as a report; the thread starts a new one. Zones open
/// while it is taken are counted in the new tree when they end, under no
/// parent.
#[must_use]
pub fn take() -> Report {
    let tree = TREE.with_borrow_mut(|t| std::mem::replace(t, Tree::new()));
    let rows = tree
        .nodes
        .into_iter()
        .zip(0u32..)
        .map(|(n, id)| Row {
            id,
            parent: (n.parent != NONE).then_some(n.parent),
            zone: n.zone.id,
            name: n.zone.name,
            calls: n.calls,
            nanos: CLOCK.delta_as_nanos(0, n.ticks),
            read: n.read,
            copied: n.copied,
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

    /// Calls of every zone beneath `row`, at any depth.
    fn beneath(&self, row: u32) -> u64 {
        self.rows[row as usize]
            .children
            .iter()
            .map(|&c| self.rows[c as usize].calls + self.beneath(c))
            .sum()
    }

    /// `row`'s time with the tracer taken out: its measured time less
    /// `overhead` for every zone entered beneath it.
    fn net(&self, row: u32, overhead: f64) -> f64 {
        let r = &self.rows[row as usize];
        #[allow(clippy::cast_precision_loss)]
        let t = r.nanos as f64 - self.beneath(row) as f64 * overhead;
        t.max(0.0)
    }

    /// Each row's time with the tracer taken out, and the part of it not
    /// spent in its children, in nanoseconds, by row number.
    #[must_use]
    pub fn net_times(&self, overhead: f64) -> Vec<(f64, f64)> {
        (0u32..)
            .zip(&self.rows)
            .map(|(id, r)| {
                let total = self.net(id, overhead);
                let inner: f64 = r.children.iter().map(|&c| self.net(c, overhead)).sum();
                (total, (total - inner).max(0.0))
            })
            .collect()
    }

    /// The tree as text: one line per row, indented under its parent, with
    /// calls, net time and its share of the top rows, net self time, MB read
    /// and copied, and copies per byte read.
    #[must_use]
    pub fn text(&self, overhead: f64) -> String {
        let net = self.net_times(overhead);
        let whole: f64 = self.roots.iter().map(|&r| net[r as usize].0).sum();
        let mut out = format!(
            "{:<40} {:>10} {:>11} {:>6} {:>11} {:>10} {:>10} {:>6}\n",
            "zone", "calls", "total ms", "%", "self ms", "read MB", "copy MB", "copy/r"
        );
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
            let _ = writeln!(
                out,
                "{label:<40} {:>10} {:>11.3} {share:>6.1} {:>11.3} {:>10.1} {:>10.1} {ratio:>6}",
                r.calls,
                total / 1e6,
                own / 1e6,
                read / 1e6,
                copied / 1e6,
            );
            stack.extend(r.children.iter().rev().map(|&c| (c, depth + 1)));
        }
        out
    }

    /// The tree as NDJSON, one row a line, for diffing runs: `id`,
    /// `parent`, `zone`, `name`, `calls`, `nanos` as measured, `net` and
    /// `self` with the tracer taken out, `read`, `copied`.
    #[must_use]
    pub fn ndjson(&self, overhead: f64) -> String {
        let net = self.net_times(overhead);
        let mut out = String::new();
        for (r, (total, own)) in self.rows.iter().zip(net) {
            let parent = r
                .parent
                .map_or_else(|| "null".to_owned(), |p| p.to_string());
            let _ = writeln!(
                out,
                r#"{{"id":{},"parent":{parent},"zone":{},"name":"{}","calls":{},"nanos":{},"net":{total:.0},"self":{own:.0},"read":{},"copied":{}}}"#,
                r.id, r.zone, r.name, r.calls, r.nanos, r.read, r.copied,
            );
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
