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
several threads merge by path. A report prints as an indented table or as
NDJSON, one row a line, for diffing runs:

```text
zone                        calls    total ms      %     self ms    read MB    copy MB copy/r
walk                            1   27335.812  100.0      46.338        0.0        0.0      -
  entry.read              4524201   19700.269   72.1   15754.213        0.0        0.0      -
    chunk.copy_out         190725      70.952    0.3      70.952        0.0     1562.4      -
```

## The tracer's own cost

A zone reads the CPU's counter on entry and exit, through `quanta`. Its
bookkeeping falls outside its own time but inside its parent's.
`zonetrace::overhead()` measures one empty zone on the running machine, and
a report takes that cost out of each node's time once per zone entered
beneath it. A zone entered a million times does not inflate its parent.

`just bench` measures the cost: an empty zone at the top, one nested in
another, and the two byte counters.

## Development

```text
just hooks    install the git hooks (prek)
just check    every hook on every file
just cover    tests, line coverage held to 100%
just bench    what a zone costs
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
