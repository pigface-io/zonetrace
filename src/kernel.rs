//! What the kernel counted for a thread, read from the files Linux keeps
//! for it under `/proc/thread-self`: time on a CPU and waiting for one,
//! page faults, context switches, and bytes the thread made storage read
//! and write. Plain file reads, so no `unsafe` and no dependency; a
//! snapshot costs a few microseconds, so it is taken only for zones
//! entered with [`enter_deep`](crate::enter_deep). Elsewhere there is no
//! such file, and no snapshot.

/// What the kernel counted for a thread over a deep zone's calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Kernel {
    /// Nanoseconds on a CPU.
    pub cpu_ns: u64,
    /// Nanoseconds ready to run, waiting for a CPU.
    pub wait_ns: u64,
    /// Page faults served without storage.
    pub minor_faults: u64,
    /// Page faults that read storage.
    pub major_faults: u64,
    /// Times the thread gave up its CPU, to wait for something.
    pub voluntary_switches: u64,
    /// Times the thread was taken off its CPU.
    pub involuntary_switches: u64,
    /// Bytes the thread made storage read: page-cache hits are not counted.
    pub read_bytes: u64,
    /// Bytes the thread sent to be written to storage.
    pub write_bytes: u64,
}

impl Kernel {
    /// What was counted from `earlier` to `self`.
    #[must_use]
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            cpu_ns: self.cpu_ns.saturating_sub(earlier.cpu_ns),
            wait_ns: self.wait_ns.saturating_sub(earlier.wait_ns),
            minor_faults: self.minor_faults.saturating_sub(earlier.minor_faults),
            major_faults: self.major_faults.saturating_sub(earlier.major_faults),
            voluntary_switches: self
                .voluntary_switches
                .saturating_sub(earlier.voluntary_switches),
            involuntary_switches: self
                .involuntary_switches
                .saturating_sub(earlier.involuntary_switches),
            read_bytes: self.read_bytes.saturating_sub(earlier.read_bytes),
            write_bytes: self.write_bytes.saturating_sub(earlier.write_bytes),
        }
    }

    /// Adds `other` in.
    pub fn add(&mut self, other: &Self) {
        self.cpu_ns += other.cpu_ns;
        self.wait_ns += other.wait_ns;
        self.minor_faults += other.minor_faults;
        self.major_faults += other.major_faults;
        self.voluntary_switches += other.voluntary_switches;
        self.involuntary_switches += other.involuntary_switches;
        self.read_bytes += other.read_bytes;
        self.write_bytes += other.write_bytes;
    }

    /// The four files of a thread, as text: `schedstat`, `stat`, `status`
    /// and `io`. `None` when one does not read as the kernel writes it.
    #[must_use]
    pub fn parse(schedstat: &str, stat: &str, status: &str, io: &str) -> Option<Self> {
        let mut sched = schedstat.split_whitespace().map(str::parse::<u64>);
        let (cpu_ns, wait_ns) = (sched.next()?.ok()?, sched.next()?.ok()?);
        // The name in parentheses may hold spaces; the fields after it
        // start with the state, field 3, so field n is at n - 3.
        let after = &stat[stat.rfind(')')? + 1..];
        let fields: Vec<&str> = after.split_whitespace().collect();
        let field = |n: usize| fields.get(n - 3)?.parse::<u64>().ok();
        let keyed = |text: &str, key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key))?
                .trim()
                .parse::<u64>()
                .ok()
        };
        Some(Self {
            cpu_ns,
            wait_ns,
            minor_faults: field(10)?,
            major_faults: field(12)?,
            voluntary_switches: keyed(status, "voluntary_ctxt_switches:")?,
            involuntary_switches: keyed(status, "nonvoluntary_ctxt_switches:")?,
            read_bytes: keyed(io, "read_bytes:")?,
            write_bytes: keyed(io, "write_bytes:")?,
        })
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::Kernel;
    use std::cell::RefCell;
    use std::fs::File;
    use std::os::unix::fs::FileExt;

    /// The thread's four files, opened once and read again from their
    /// start for each snapshot: the kernel writes them afresh at each read.
    struct Files {
        files: [File; 4],
        buf: Vec<u8>,
    }

    thread_local! {
        static FILES: RefCell<Option<Files>> = const { RefCell::new(None) };
    }

    const NAMES: [&str; 4] = ["schedstat", "stat", "status", "io"];

    fn open() -> Option<Files> {
        let mut files = NAMES
            .iter()
            .map(|n| File::open(format!("/proc/thread-self/{n}")).ok());
        Some(Files {
            files: [
                files.next()??,
                files.next()??,
                files.next()??,
                files.next()??,
            ],
            buf: Vec::with_capacity(4096),
        })
    }

    /// One file read whole into `buf` from its start, as text.
    fn read<'b>(f: &File, buf: &'b mut Vec<u8>) -> Option<&'b str> {
        buf.resize(4096, 0);
        let n = f.read_at(buf, 0).ok()?;
        buf.truncate(n);
        std::str::from_utf8(buf).ok()
    }

    pub(super) fn snapshot() -> Option<Kernel> {
        FILES.with_borrow_mut(|slot| {
            if slot.is_none() {
                *slot = open();
            }
            let Files { files, buf } = slot.as_mut()?;
            let mut text: [String; 4] = Default::default();
            for (f, t) in files.iter().zip(&mut text) {
                read(f, buf)?.clone_into(t);
            }
            Kernel::parse(&text[0], &text[1], &text[2], &text[3])
        })
    }
}

/// What the kernel has counted for this thread so far.
#[cfg(target_os = "linux")]
pub(crate) fn snapshot() -> Option<Kernel> {
    linux::snapshot()
}

/// No kernel files here; tests set what a snapshot returns.
#[cfg(not(target_os = "linux"))]
pub(crate) fn snapshot() -> Option<Kernel> {
    #[cfg(test)]
    return FAKE.get();
    #[cfg(not(test))]
    None
}

#[cfg(all(test, not(target_os = "linux")))]
thread_local! {
    static FAKE: std::cell::Cell<Option<Kernel>> = const { std::cell::Cell::new(None) };
}

/// What [`snapshot`] returns on this thread, in tests where there are no
/// kernel files.
#[cfg(test)]
pub(crate) fn fake(k: Option<Kernel>) {
    #[cfg(not(target_os = "linux"))]
    FAKE.set(k);
    #[cfg(target_os = "linux")]
    let _ = k;
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str =
        "1234 (a (b) c) S 1 1234 1234 0 -1 4194304 77 0 3 0 5 6 0 0 20 0 1 0 1000 1000 100\n";
    const STATUS: &str = "Name:\tt\nvoluntary_ctxt_switches:\t9\nnonvoluntary_ctxt_switches:\t2\n";
    const IO: &str = "rchar: 10\nwchar: 0\nread_bytes: 4096\nwrite_bytes: 8192\n";

    #[test]
    fn the_four_files_parse() {
        let k = Kernel::parse("1500 250 3\n", STAT, STATUS, IO).expect("parses");
        assert_eq!(
            k,
            Kernel {
                cpu_ns: 1500,
                wait_ns: 250,
                minor_faults: 77,
                major_faults: 3,
                voluntary_switches: 9,
                involuntary_switches: 2,
                read_bytes: 4096,
                write_bytes: 8192,
            }
        );
    }

    #[test]
    fn a_file_out_of_shape_is_none() {
        assert!(Kernel::parse("", STAT, STATUS, IO).is_none());
        assert!(Kernel::parse("1 x", STAT, STATUS, IO).is_none());
        assert!(Kernel::parse("1 2", "no parenthesis", STATUS, IO).is_none());
        assert!(Kernel::parse("1 2", "1 (t) S 1", STATUS, IO).is_none());
        assert!(Kernel::parse("1 2", STAT, "", IO).is_none());
        assert!(Kernel::parse("1 2", STAT, STATUS, "read_bytes: many").is_none());
    }

    #[test]
    fn counts_subtract_and_add() {
        let a = Kernel {
            cpu_ns: 10,
            read_bytes: 5,
            ..Kernel::default()
        };
        let b = Kernel {
            cpu_ns: 30,
            read_bytes: 4,
            major_faults: 1,
            ..Kernel::default()
        };
        let d = b.since(&a);
        assert_eq!((d.cpu_ns, d.read_bytes, d.major_faults), (20, 0, 1));
        let mut sum = d;
        sum.add(&d);
        assert_eq!((sum.cpu_ns, sum.major_faults), (40, 2));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn elsewhere_there_is_no_snapshot() {
        assert!(snapshot().is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_thread_snapshot_grows_with_work() {
        let a = snapshot().expect("a snapshot");
        let t = std::time::Instant::now();
        while t.elapsed() < std::time::Duration::from_millis(20) {
            std::hint::spin_loop();
        }
        let b = snapshot().expect("a snapshot");
        assert!(b.since(&a).cpu_ns >= 10_000_000, "{:?}", b.since(&a));
    }
}
