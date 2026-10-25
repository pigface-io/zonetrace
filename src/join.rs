//! Samples of the CPU's counters handed to the zones open when they were
//! taken (feature `timeline`): what `zonetrace-join` prints.
//!
//! The samples are `perf script` lines, `perf record -k realtime` so their
//! times are Unix nanoseconds, printed with `-F tid,time,period,event
//! --ns`. Each is handed to the innermost zone open on its thread at its
//! time, by one sweep of the thread's events and its samples in time
//! order. Paths of zones merge across threads by zone number, as reports
//! do.

/// One thread's timeline, read back from its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    /// The kernel's id of the thread.
    pub tid: u64,
    /// Every zone the events name: number and name.
    pub zones: Vec<(u16, String)>,
    /// Unix nanoseconds, and the zone entered or `None` for the innermost
    /// left.
    pub events: Vec<(u64, Option<u16>)>,
}

impl Thread {
    /// A timeline as [`Timeline::write`](crate::timeline::Timeline::write)
    /// writes it.
    ///
    /// # Errors
    ///
    /// The line that is not a timeline's, and why.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines().enumerate();
        if lines.next().map(|(_, l)| l) != Some(crate::timeline::MAGIC) {
            return Err(format!("line 1: not \"{}\"", crate::timeline::MAGIC));
        }
        let mut t = Self {
            tid: 0,
            zones: Vec::new(),
            events: Vec::new(),
        };
        for (i, line) in lines {
            let bad = || format!("line {}: {line:?}", i + 1);
            let mut f = line.splitn(3, ' ');
            let number = |s: Option<&str>| s.and_then(|s| s.parse::<u64>().ok());
            match f.next() {
                Some("thread") => t.tid = number(f.next()).ok_or_else(bad)?,
                Some("zone") => {
                    let id = f.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
                    t.zones.push((id, f.next().ok_or_else(bad)?.to_owned()));
                }
                Some("e") => {
                    let at = number(f.next()).ok_or_else(bad)?;
                    let zone = f.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
                    t.events.push((at, Some(zone)));
                }
                Some("x") => t.events.push((number(f.next()).ok_or_else(bad)?, None)),
                _ => return Err(bad()),
            }
        }
        Ok(t)
    }
}

/// One sample from `perf script`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The thread it was taken on.
    pub tid: u64,
    /// When, in Unix nanoseconds.
    pub at: u64,
    /// The counter, e.g. `cycles`.
    pub event: String,
    /// Counts the sample stands for.
    pub period: u64,
}

impl Sample {
    /// A line of `perf script -F tid,time,period,event --ns`, e.g.
    /// `987 1792923974.411030316:    1001001 cycles:`; `None` for any
    /// other line. Without a period the sample stands for 1.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let mut f = line.split_whitespace();
        let tid = f.next()?.parse().ok()?;
        let (secs, nanos) = f.next()?.strip_suffix(':')?.split_once('.')?;
        if nanos.len() != 9 {
            return None;
        }
        let at = secs.parse::<u64>().ok()? * 1_000_000_000 + nanos.parse::<u64>().ok()?;
        let (period, event) = match (f.next()?, f.next()) {
            (p, Some(e)) => (p.parse().ok()?, e),
            (e, None) => (1, e),
        };
        Some(Self {
            tid,
            at,
            event: event.strip_suffix(':')?.to_owned(),
            period,
        })
    }
}

/// A path of zones, and the counts of samples taken in it and in no zone
/// beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
    zone: u16,
    name: String,
    parent: Option<usize>,
    children: Vec<usize>,
    /// Period summed, by event.
    own: Vec<u64>,
}

/// Samples handed to zones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Joined {
    /// The counters, in the order first seen.
    events: Vec<String>,
    nodes: Vec<Node>,
    roots: Vec<usize>,
    /// Period of samples on a recorded thread, in its recording, with no
    /// zone open, by event.
    outside: Vec<u64>,
    /// Samples on no recorded thread, or outside its recording.
    pub unplaced: u64,
}

impl Joined {
    fn event(&mut self, name: &str) -> usize {
        if let Some(i) = self.events.iter().position(|e| e == name) {
            return i;
        }
        // Every counter is named before the first node is made, so no
        // node lacks its count.
        self.events.push(name.to_owned());
        self.outside.push(0);
        self.events.len() - 1
    }

    fn child(&mut self, parent: Option<usize>, zone: u16, name: &str) -> usize {
        let siblings = parent.map_or(&self.roots, |p| &self.nodes[p].children);
        if let Some(&c) = siblings.iter().find(|&&c| self.nodes[c].zone == zone) {
            return c;
        }
        let id = self.nodes.len();
        self.nodes.push(Node {
            zone,
            name: name.to_owned(),
            parent,
            children: Vec::new(),
            own: vec![0; self.events.len()],
        });
        match parent {
            Some(p) => self.nodes[p].children.push(id),
            None => self.roots.push(id),
        }
        id
    }

    /// Each node's period with every node beneath it, by event: a node's
    /// number is above its parent's, so one pass from the last up.
    fn totals(&self) -> Vec<Vec<u64>> {
        let mut total: Vec<Vec<u64>> = self.nodes.iter().map(|n| n.own.clone()).collect();
        for (i, n) in self.nodes.iter().enumerate().rev() {
            if let Some(p) = n.parent {
                let (head, tail) = total.split_at_mut(i);
                for (a, b) in head[p].iter_mut().zip(&tail[0]) {
                    *a += b;
                }
            }
        }
        total
    }

    /// The tree as text: per counter, the period taken in each zone and
    /// beneath it, its share of the counter's whole, and the part in no
    /// zone beneath.
    #[must_use]
    pub fn text(&self) -> String {
        use std::fmt::Write as _;
        let total = self.totals();
        let whole: Vec<u64> = (0..self.events.len())
            .map(|e| self.roots.iter().map(|&r| total[r][e]).sum::<u64>() + self.outside[e])
            .collect();
        let mut out = format!("{:<40}", "zone");
        for e in &self.events {
            let _ = write!(
                out,
                " {:>16} {:>6} {:>16}",
                format!("{e} total"),
                "%",
                format!("{e} self")
            );
        }
        out.push('\n');
        let share = |n: u64, w: u64| {
            #[allow(clippy::cast_precision_loss)]
            let s = if w == 0 {
                0.0
            } else {
                n as f64 / w as f64 * 100.0
            };
            s
        };
        let mut stack: Vec<(usize, usize)> = self.roots.iter().rev().map(|&r| (r, 0)).collect();
        while let Some((id, depth)) = stack.pop() {
            let n = &self.nodes[id];
            let _ = write!(
                out,
                "{:<40}",
                format!("{:indent$}{}", "", n.name, indent = depth * 2)
            );
            for (e, w) in whole.iter().enumerate() {
                let t = total[id][e];
                let _ = write!(out, " {t:>16} {:>6.1} {:>16}", share(t, *w), n.own[e]);
            }
            out.push('\n');
            stack.extend(n.children.iter().rev().map(|&c| (c, depth + 1)));
        }
        let _ = write!(out, "{:<40}", "(no zone open)");
        for (e, w) in whole.iter().enumerate() {
            let o = self.outside[e];
            let _ = write!(out, " {o:>16} {:>6.1} {:>16}", share(o, *w), o);
        }
        out.push('\n');
        out
    }
}

/// Hands every sample to the innermost zone open on its thread at its
/// time.
#[must_use]
pub fn join(threads: &[Thread], samples: &[Sample]) -> Joined {
    let mut j = Joined::default();
    for s in samples {
        j.event(&s.event);
    }
    let mut placed = 0u64;
    for t in threads {
        let mut mine: Vec<&Sample> = samples.iter().filter(|s| s.tid == t.tid).collect();
        mine.sort_by_key(|s| s.at);
        let (Some(first), Some(last)) = (t.events.first(), t.events.last()) else {
            continue;
        };
        let name = |zone: u16| {
            t.zones
                .iter()
                .find(|(z, _)| *z == zone)
                .map_or("?", |(_, n)| n.as_str())
        };
        let mut stack: Vec<usize> = Vec::new();
        let mut next = 0;
        for s in mine {
            if s.at < first.0 || s.at > last.0 {
                continue;
            }
            while let Some(&(at, zone)) = t.events.get(next) {
                if at > s.at {
                    break;
                }
                match zone {
                    Some(z) => {
                        let node = j.child(stack.last().copied(), z, name(z));
                        stack.push(node);
                    }
                    None => {
                        stack.pop();
                    }
                }
                next += 1;
            }
            let e = j.event(&s.event);
            match stack.last() {
                Some(&n) => j.nodes[n].own[e] += s.period,
                None => j.outside[e] += s.period,
            }
            placed += 1;
        }
    }
    j.unplaced = samples.len() as u64 - placed;
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread() -> Thread {
        Thread::parse(
            "zonetrace timeline 1\nthread 7\nzone 1 walk\nzone 2 entry read\n\
             e 100 1\ne 110 2\nx 150\ne 160 2\nx 170\nx 200\n",
        )
        .expect("a timeline")
    }

    fn sample(tid: u64, at: u64, event: &str, period: u64) -> Sample {
        Sample {
            tid,
            at,
            event: event.to_owned(),
            period,
        }
    }

    #[test]
    fn a_timeline_reads_back() {
        let t = thread();
        assert_eq!(t.tid, 7);
        assert_eq!(
            t.zones,
            [(1, "walk".to_owned()), (2, "entry read".to_owned())]
        );
        assert_eq!(t.events[..2], [(100, Some(1)), (110, Some(2))]);
        assert_eq!(t.events[5], (200, None));
    }

    #[test]
    fn a_line_out_of_shape_is_named() {
        assert!(Thread::parse("").expect_err("empty").starts_with("line 1"));
        for bad in ["thread x", "zone x a", "zone 1", "e 1", "e x 1", "x", "y 1"] {
            let text = format!("zonetrace timeline 1\n{bad}\n");
            assert!(
                Thread::parse(&text).expect_err(bad).starts_with("line 2"),
                "{bad}"
            );
        }
    }

    #[test]
    fn perf_lines_parse() {
        assert_eq!(
            Sample::parse("    987 1792923974.411030316:    1001001 cycles: "),
            Some(sample(987, 1_792_923_974_411_030_316, "cycles", 1_001_001))
        );
        assert_eq!(
            Sample::parse("1 2.000000003: instructions:"),
            Some(sample(1, 2_000_000_003, "instructions", 1))
        );
        for bad in [
            "",
            "x 1.000000000: 1 cycles:",
            "1",
            "1 2.000000003 3 cycles:",
            "1 2000000003: 3 cycles:",
            "1 2.0003: 3 cycles:",
            "1 x.000000003: 3 cycles:",
            "1 2.00000000x: 3 cycles:",
            "1 2.000000003:",
            "1 2.000000003: x cycles:",
            "1 2.000000003: 3 cycles",
        ] {
            assert_eq!(Sample::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn samples_go_to_the_zone_open_on_their_thread() {
        let samples = [
            sample(7, 105, "cycles", 10),  // walk only
            sample(7, 120, "cycles", 100), // entry read
            sample(7, 165, "cycles", 50),  // entry read again
            sample(7, 180, "instructions", 3),
            sample(7, 50, "cycles", 1),  // before the recording
            sample(9, 120, "cycles", 1), // another thread
        ];
        let j = join(&[thread()], &samples);
        assert_eq!(j.unplaced, 2);
        assert_eq!(j.events, ["cycles", "instructions"]);
        assert_eq!(j.nodes.len(), 2);
        assert_eq!(j.nodes[0].own, [10, 3]);
        assert_eq!(j.nodes[1].own, [150, 0]);
        assert_eq!(j.totals()[0], [160, 3]);
        let text = j.text();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].contains("cycles total"));
        assert!(lines[1].starts_with("walk "));
        assert!(lines[1].contains(" 100.0 "));
        assert!(lines[2].starts_with("  entry read "));
        assert!(lines[3].starts_with("(no zone open)"));
    }

    #[test]
    fn a_sample_with_no_zone_open_is_outside() {
        let t = Thread::parse(
            "zonetrace timeline 1\nthread 7\nzone 1 a\ne 100 1\nx 110\ne 120 1\nx 130\n",
        )
        .expect("a timeline");
        let unnamed =
            Thread::parse("zonetrace timeline 1\nthread 8\ne 100 3\nx 130\n").expect("a timeline");
        let empty = Thread::parse("zonetrace timeline 1\nthread 9\n").expect("a timeline");
        let samples = [
            sample(7, 115, "cycles", 4),
            sample(7, 125, "cycles", 6),
            sample(8, 120, "cycles", 1),
            sample(9, 120, "cycles", 1),
        ];
        let j = join(&[t, unnamed, empty], &samples);
        assert_eq!(j.outside, [4]);
        assert_eq!(j.nodes[0].own, [6]);
        assert_eq!(j.nodes[1].name, "?");
        assert_eq!(j.unplaced, 1);
        assert!(join(&[], &[]).text().contains("(no zone open)"));
        let none = join(&[], &[sample(1, 1, "cycles", 1)]);
        assert!(none.text().lines().nth(1).expect("a line").contains("0.0"));
    }
}
