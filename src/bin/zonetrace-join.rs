//! Hands `perf` samples to the zones open when they were taken.
//!
//! ```text
//! perf record -k realtime -e cycles,instructions -- {program}
//! perf script -F tid,time,period,event --ns | zonetrace-join {timeline}...
//! ```
//!
//! Each timeline is one thread's, written by `zonetrace::timeline`. The
//! tree goes to stdout; lines that are not samples, and samples no
//! timeline covers, are counted on stderr.

use std::io::{BufRead, Write};
use std::process::ExitCode;

use zonetrace::join::{Sample, Thread, join};

fn main() -> ExitCode {
    let mut threads = Vec::new();
    for path in std::env::args().skip(1) {
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| Thread::parse(&text))
        {
            Ok(t) => threads.push(t),
            Err(e) => {
                eprintln!("zonetrace-join: {path}: {e}");
                return ExitCode::from(2);
            }
        }
    }
    let (mut samples, mut skipped) = (Vec::new(), 0u64);
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        match Sample::parse(&line) {
            Some(s) => samples.push(s),
            None => skipped += 1,
        }
    }
    let joined = join(&threads, &samples);
    let _ = std::io::stdout().write_all(joined.text().as_bytes());
    eprintln!(
        "zonetrace-join: {} samples, {} outside every timeline, {skipped} lines not samples",
        samples.len(),
        joined.unplaced
    );
    ExitCode::SUCCESS
}
