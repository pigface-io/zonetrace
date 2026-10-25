# Changelog

All notable changes to zonetrace. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-10-25

### Added

- Zones declared as statics, timed per thread into a call tree: calls,
  time, and bytes read and copied per path of zones.
- The tracer's own cost, measured by `overhead()` and taken out of every
  row of a report.
- Reports that merge across threads, and print as a table or as NDJSON.
- Deep zones (`enter_deep`): what the kernel counted for the thread over
  each call, on Linux: time on a CPU and waiting for one, page faults,
  context switches, and bytes moved to and from storage.
- Timelines (feature `timeline`): a thread's zones in time, and
  `zonetrace-join` to hand `perf` samples of the CPU's counters to the
  zones they were taken in.

### Performance

- An empty zone costs about 4 ns on Apple silicon, with no dependencies:
  the CPU's counter read in one instruction, a cache of where each zone
  was last entered, and counters in cells that need no borrow.

[0.1.0]: https://github.com/pigface-io/zonetrace/releases/tag/v0.1.0
