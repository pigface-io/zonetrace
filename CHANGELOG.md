# Changelog

All notable changes to zonetrace. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/).

## [0.2.0] - 2026-11-19

### Added

- The feature `heap`: `heap::Counting`, a global allocator that books each
  allocator call to the zone open on its thread. A `Row` carries its own
  allocations, bytes allocated, reallocations and their growth, frees,
  bytes freed, and peak live bytes above where its calls began.
  - What the tracer allocates goes to the row `zones.tracer`, and never
    to a zone.
  - The rows `zones.outside` and `zones.unplaced` hold the rest, so every
    call lands in exactly one row; `heap::thread_totals()` gives the sums
    to check that against.
  - `heap::overhead()` measures what booking a call costs, and a report
    takes it out of each row's time.
  - `text` and `ndjson` show the counts when a row has any, and print as
    before when none does.
- `Report::net_times_with(zone, event)`: net times with both costs given.

### Changed

- `Row` has a new public field, `heap`, so code that builds a `Row` by
  hand must set it. This is the breaking change behind 0.2.
- `just bench` runs three builds: the feature off, on, and on with the
  allocator installed. It fails when the feature adds more than half a
  nanosecond a zone, or more than half the cost.

## [0.1.4] - 2026-11-06

### Added

- `add(path, calls, nanos)` records calls on a path of zones in the
  process's report from any thread, and `stretch(path)` returns a `Send`
  `Stretch` that records one call when it drops: for the stretches no
  guard can hold, one across an `.await` or one that ends on another
  thread. The hot path is untouched; a stretch reads the system clock
  twice and takes the report's lock once, so it is for stages, never rows.

## [0.1.3] - 2026-11-01

### Added

- A JavaScript port under `js/`, the same model in a browser or under
  Bun: `zone`, `enter`/`exit`, `read`/`copied`, `overhead`, `take`, and a
  `Report` whose `text` and `ndjson` match the crate's, a fixture test
  holding the two to one format. With `countHeap`, each call also sums
  what the heap grew by, for a check that a steady state allocates
  nothing. The npm package `zonetrace` from this repository is that port.

## [0.1.2] - 2026-10-28

### Fixed

- A zone in a crate that uses zonetrace cost about 6 ns on Apple silicon,
  and two nested zones 13, where the bench measured under 4: the closures
  of `enter` and a guard's `drop` did not inline across the crate. They
  inline now, the first visit of a zone and the spilled counters out of
  line, and a zone costs under 4 ns wherever it is.
- The bench times zones inside a function of their own, as code that uses
  the crate has them, and fails when they stop inlining.

## [0.1.1] - 2026-10-25

### Fixed

- The release workflow wrote its notes into the checkout, and `cargo
  publish` refused the untracked file: 0.1.0 was released on GitHub only.
  0.1.1 is the first version on crates.io, with the same code.

## [0.1.0] - 2026-10-25

### Added

- Zones declared as statics, timed per thread into a call tree: calls,
  time, and bytes read and copied per path of zones.
- The tracer's own cost, measured by `overhead()` and taken out of every
  row of a report.
- Reports that merge across threads, and print as a table or as NDJSON;
  `flush` and `take_all` gather the threads of a pool into one.
- Deep zones (`enter_deep`): what the kernel counted for the thread over
  each call, on Linux: time on a CPU and waiting for one, page faults,
  context switches, and bytes moved to and from storage.
- Timelines (feature `timeline`): a thread's zones in time, and
  `zonetrace-join` to hand `perf` samples of the CPU's counters to the
  zones they were taken in.

### Performance

- An empty zone costs about 4 ns on Apple silicon, and 19 to 27 ns where
  the CPU's counter is slower to read (x86_64, cloud aarch64), with no
  dependencies:
  the CPU's counter read in one instruction, a cache of where each zone
  was last entered, and counters in cells that need no borrow.

[0.2.0]: https://github.com/pigface-io/zonetrace/releases/tag/v0.2.0
[0.1.4]: https://github.com/pigface-io/zonetrace/releases/tag/v0.1.4
[0.1.3]: https://github.com/pigface-io/zonetrace/releases/tag/v0.1.3
[0.1.2]: https://github.com/pigface-io/zonetrace/releases/tag/v0.1.2
[0.1.1]: https://github.com/pigface-io/zonetrace/releases/tag/v0.1.1
[0.1.0]: https://github.com/pigface-io/zonetrace/releases/tag/v0.1.0
