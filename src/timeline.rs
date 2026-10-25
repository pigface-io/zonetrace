//! A thread's zones in time, for joining with samples of the CPU's
//! counters taken outside the process (feature `timeline`).
//!
//! The tree sums; it cannot say which zone was open at a given moment. A
//! timeline can: between [`start`] and [`stop`] each zone entered and
//! left on the thread is kept with its time, as Unix nanoseconds when
//! written out. `perf record -k realtime` stamps its samples on the same
//! clock, so `zonetrace-join` hands each sample to the zone open on its
//! thread when it was taken: cycles, instructions or cache misses per
//! zone, with no counter read in the process and no `unsafe`. Recording
//! costs a push per enter and per exit, and the memory of every call, so
//! it is for a run, not for always.
//!
//! The text written, one record a line:
//!
//! ```text
//! zonetrace timeline 1
//! thread {tid}                 the kernel's thread id, 0 where unknown
//! zone {id} {name}             every zone the events name
//! e {unix_ns} {zone id}        a zone entered
//! x {unix_ns}                  the innermost open zone left
//! ```

use std::cell::{Cell, RefCell};
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Zone, clock};

/// The first line of a timeline.
pub const MAGIC: &str = "zonetrace timeline 1";

/// One zone entered, with its zone, or the innermost left, without.
#[derive(Debug, Clone, Copy)]
struct Event {
    ticks: u64,
    zone: Option<&'static Zone>,
}

/// The zones of one thread between [`start`] and [`stop`].
#[derive(Debug)]
pub struct Timeline {
    tid: u64,
    /// The counter, and Unix nanoseconds, at [`start`].
    origin: (u64, u64),
    events: Vec<Event>,
}

thread_local! {
    static EVENTS: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) };
    static ORIGIN: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
}

/// Starts recording this thread's zones, dropping any recording not yet
/// stopped.
pub fn start() {
    EVENTS.with_borrow_mut(Vec::clear);
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    ORIGIN.set((clock::ticks(), unix));
    crate::set_recording(true);
}

/// Stops recording, and returns what was recorded since [`start`];
/// `None` when this thread was not recording.
#[must_use]
pub fn stop() -> Option<Timeline> {
    if !crate::set_recording(false) {
        return None;
    }
    Some(Timeline {
        tid: thread_id(),
        origin: ORIGIN.get(),
        events: EVENTS.take(),
    })
}

#[inline]
pub(crate) fn enter(zone: &'static Zone, ticks: u64) {
    EVENTS.with_borrow_mut(|e| {
        e.push(Event {
            ticks,
            zone: Some(zone),
        });
    });
}

#[inline]
pub(crate) fn exit(ticks: u64) {
    EVENTS.with_borrow_mut(|e| e.push(Event { ticks, zone: None }));
}

/// The kernel's id of this thread, as `perf` reports it: on Linux the
/// last part of where `/proc/thread-self` points (`{pid}/task/{tid}`);
/// 0 elsewhere.
fn thread_id() -> u64 {
    tid_of(std::fs::read_link("/proc/thread-self").ok().as_deref())
}

/// The thread id at the end of `link`, `{pid}/task/{tid}`; 0 without one.
fn tid_of(link: Option<&std::path::Path>) -> u64 {
    link.and_then(|p| p.file_name()?.to_str()?.parse().ok())
        .unwrap_or(0)
}

impl Timeline {
    /// Events recorded: an enter and an exit per call.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Unix nanoseconds at `ticks` of the counter.
    fn unix(&self, ticks: u64) -> u64 {
        self.origin.1 + clock::nanos(ticks.wrapping_sub(self.origin.0))
    }

    /// The timeline as text (see the module).
    ///
    /// # Errors
    ///
    /// When `out` cannot be written.
    pub fn write(&self, out: &mut impl Write) -> io::Result<()> {
        let mut out = io::BufWriter::new(out);
        writeln!(out, "{MAGIC}")?;
        writeln!(out, "thread {}", self.tid)?;
        let mut seen: Vec<&'static Zone> = Vec::new();
        for z in self.events.iter().filter_map(|e| e.zone) {
            if !seen.iter().any(|s| std::ptr::eq(*s, z)) {
                seen.push(z);
                writeln!(out, "zone {} {}", z.id(), z.name())?;
            }
        }
        for e in &self.events {
            match e.zone {
                Some(z) => writeln!(out, "e {} {}", self.unix(e.ticks), z.id())?,
                None => writeln!(out, "x {}", self.unix(e.ticks))?,
            }
        }
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static A: Zone = Zone::new(1, "a");
    static B: Zone = Zone::new(2, "b");

    #[test]
    fn a_recording_keeps_each_enter_and_exit_in_order() {
        assert!(stop().is_none());
        start();
        {
            let _a = crate::enter(&A);
            drop(crate::enter(&B));
            drop(crate::enter(&B));
        }
        let t = stop().expect("recording");
        assert!(stop().is_none());
        assert_eq!(t.len(), 6);
        assert!(!t.is_empty());
        let mut out = Vec::new();
        t.write(&mut out).expect("written");
        let text = String::from_utf8(out).expect("text");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], MAGIC);
        assert_eq!(lines[1], format!("thread {}", thread_id()));
        assert_eq!(&lines[2..4], ["zone 1 a", "zone 2 b"]);
        let kinds: Vec<&str> = lines[4..].iter().map(|l| &l[..1]).collect();
        assert_eq!(kinds, ["e", "e", "x", "e", "x", "x"]);
        let times: Vec<u64> = lines[4..]
            .iter()
            .map(|l| {
                l.split(' ')
                    .nth(1)
                    .expect("a time")
                    .parse()
                    .expect("a number")
            })
            .collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_nanos();
        assert!(u128::from(times[0]).abs_diff(now) < 60_000_000_000);
    }

    #[test]
    fn nothing_is_kept_unless_recording() {
        drop(crate::enter(&A));
        start();
        let t = stop().expect("recording");
        assert!(t.is_empty());
    }

    #[test]
    fn a_failed_write_is_an_error() {
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("full"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        start();
        drop(crate::enter(&A));
        let t = stop().expect("recording");
        assert!(t.write(&mut Full).is_err());
        assert!(Full.flush().is_ok());
    }

    #[test]
    fn a_thread_id_is_the_end_of_its_link() {
        use std::path::Path;
        assert_eq!(tid_of(Some(Path::new("12/task/34"))), 34);
        assert_eq!(tid_of(Some(Path::new("12/task/x"))), 0);
        assert_eq!(tid_of(None), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_thread_is_named_as_perf_names_it() {
        assert_ne!(thread_id(), 0);
    }
}
