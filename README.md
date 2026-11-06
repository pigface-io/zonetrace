# zonetrace

Zones: named stretches of code, timed per thread into a call tree.

A zone is a `static`, declared where the code it times lives. Entering it
returns a guard; dropping the guard ends the call:

```rust
use zonetrace::{Zone, enter};

static LOAD: Zone = Zone::new(1, "chunk.load");

fn load(buf: &mut Vec<u8>) {
    let _z = enter(&LOAD);
    // ... the work the zone times ...
    zonetrace::read(buf.len() as u64);
}
```

Each thread keeps one node per path of zones: a zone entered from two
different zones is two nodes. A node sums its calls, its time, and the
bytes the code in it said it read and copied. Nothing is recorded per
call, so memory stays the size of the tree however long the program runs.

`zonetrace::take()` returns the thread's tree as a `Report`. Reports of
several threads merge by path. In a thread pool, call `zonetrace::flush()`
at the end of each task; `zonetrace::take_all()` then returns every
thread's zones as one report. A report prints as an indented table or as
NDJSON, one row a line, for diffing runs:

```text
zone                        calls    total ms      %     self ms    read MB    copy MB copy/r
walk                            1   27335.812  100.0      46.338        0.0        0.0      -
  entry.read              4524201   19700.269   72.1   15754.213        0.0        0.0      -
    chunk.copy_out         190725      70.952    0.3      70.952        0.0     1562.4      -
```

## The tracer's own cost

An empty zone costs about 4 ns on Apple silicon: a lookup in a small cache, two reads of
the CPU's counter (one instruction each, `mrs cntvct_el0` on aarch64 and
`rdtsc` on x86_64), and two counter updates, with no borrow and no
allocation. The crate has no dependencies. Its one `unsafe` block is the
counter read, in `src/clock.rs`. The tracer's bookkeeping falls outside
its own time but inside its parent's.
`zonetrace::overhead()` measures one empty zone on the running machine, and
a report takes that cost out of each node's time once per zone entered
beneath it. A zone entered a million times does not inflate its parent.

`just bench` measures the cost: an empty zone at the top, one nested in
another, the same two inside a function of their own, and the two byte
counters. Measured on an Apple M-series laptop:

```text
zones/enter_exit_top              3.85 ns   (was 10.4 with quanta and RefCell)
zones/zone_in_fn                  3.93 ns   (was 5.3 before enter inlined)
zones/nested_in_fn                3.88 ns   (was 10.1)
zones/enter_exit_nested           3.86 ns   (was 11.3)
zones/count_read_and_copied       2.23 ns   (was 8.8)
```

The counter read sets the cost of a zone, and it depends on the machine.
From CI runners:

| Machine | enter + exit | byte counters |
|---|---|---|
| Apple silicon (macOS) | 4.1 ns | 2.7 ns |
| x86_64 Linux | 19.1 ns | 2.2 ns |
| x86_64 Windows | 21.5 ns | 3.9 ns |
| aarch64 Linux (Neoverse, cloud) | 26.7 ns | 2.2 ns |

Where a zone wraps a call of a few nanoseconds on such a machine, record a
timeline and sample with `perf` instead.

Rust 1.85 or later (edition 2024).

## Deep zones: what the kernel counted

`zonetrace::enter_deep(&ZONE)` opens a zone that also sums what the kernel
counted for the thread over each call: time on a CPU, time waiting for one,
page faults, context switches, and bytes the thread made storage read and
write. A report shows them beside the row, with the time off both, which
is the time the thread was blocked:

```text
zone     calls  total ms ...   cpu ms   wait ms   off ms  majflt switches   disk MB
run          1    68.539 ...   67.680     0.015    0.845       0        0       0.0
  sum       20    20.602 ...        -         -        -       -        -         -
  sort      20    47.934 ...        -         -        -       -        -         -
```

That is `examples/perf_join.rs` on Linux: `run` is deep, and all but under
a millisecond of it was on a CPU. A zone waiting on storage shows its time
under `off ms`, with the switches and bytes that go with it.

On Linux the counts come from the thread's files under `/proc/thread-self`,
kept open and read again: plain file reads, no `unsafe`. Two snapshots cost
a few microseconds, outside the zone's time and inside its parent's, so a
deep zone is for a stretch of a millisecond or more. Elsewhere a deep zone
is an ordinary zone.

## Stretches: time no guard can hold

A guard belongs to the thread that opened it, so it cannot live across an
`.await` (the future would not be `Send`) or end on another thread, and a
thread that never exits never flushes its tree. For such a stretch,
`zonetrace::stretch(&PATH)` returns a `Send` `Stretch` that records one
call of the last zone of `PATH`, under the ones before it, straight into
the process's report when it drops; `zonetrace::add(&PATH, calls, nanos)`
records a count by hand:

```rust
use zonetrace::Zone;

static RUN: Zone = Zone::new(30, "run");
static FETCH: Zone = Zone::new(31, "run.fetch");
static FETCH_PATH: [&Zone; 2] = [&RUN, &FETCH];

async fn fetch_all() {
    let _s = zonetrace::stretch(&FETCH_PATH);
    // ... awaits, on whichever threads the runtime chooses ...
}
```

The ancestors on the path only place the row; their own time comes from
their own guards or stretches. A stretch reads the system clock twice and
takes the report's lock once, so it is for stages, never for rows; the hot
path of `enter` is untouched.

## Timelines: hardware counters per zone, from `perf`

With the feature `timeline`, `zonetrace::timeline::start()` records each
zone entered and left on the thread with its time, and `stop()` returns
the recording to write out. `perf` samples the CPU's counters from outside
the process, stamped on the same clock, and `zonetrace-join` hands each
sample to the zone open on its thread when it was taken:

```text
perf record -k realtime -e cycles,instructions -- ./my-program   # writes run.timeline
perf script -F tid,time,period,event --ns | zonetrace-join run.timeline
```

```text
zone                                      cycles total      %      cycles self
run                                          68413680  100.0                0
  sum                                        19803960   28.9         19803960
  sort                                       48609720   71.1         48609720
(no zone open)                                      0    0.0                0
```

No counter is read in the process and nothing is `unsafe`; recording costs
a push per enter and per exit. Compiled in but not recording, the feature
adds about 1 ns to a zone. `examples/perf_join.rs` is a run to try.

## From JavaScript

The same model is under `js/`, for a browser or Bun, and installs as the
npm package `zonetrace` from this repository (`bun add
github:pigface-io/zonetrace#v0.1.3`). A zone is declared where its code
lives; `enter` returns its node and `exit` of that node ends the call:

```js
import { zone, enter, exit, read, take, overhead } from "zonetrace";

const LOAD = zone(1, "chunk.load");

function load(buf) {
  const z = enter(LOAD);
  // ... the work the zone times ...
  read(buf.byteLength);
  exit(z);
}

zones.on = true;
load(buf);
console.log(take().text(overhead()));
```

With `zones.on` false, `enter` returns null and a zone costs one branch,
so a page can ship its zones. `take()` gives a `Report` whose `text()` and
`ndjson()` are the crate's formats, held to them by `tests/format.rs` and
`js/zonetrace.test.ts` through one fixture. `countHeap(f)`, with `f` a
function returning the heap's size in bytes (`performance.memory
.usedJSHeapSize` in Chrome, `heapStats().heapCapacity` from `bun:jsc`),
also sums what the heap grew by in each call, and a call the collector
ran through counts under `gc` instead: a steady state that is preallocated
shows zero. The clock is `performance.now()` in a browser, where it is
coarse, so read a zone there as a mean over many calls.

## Development

```text
just hooks    install the git hooks (prek)
just check    every hook on every file
just cover    tests, line coverage held to 100%
just bench    what a zone costs
just js-test  the JavaScript port's tests
just test-linux   the tests on Linux, in a container (podman)
```

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
