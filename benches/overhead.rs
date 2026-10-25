//! What a zone costs: an empty zone at the top, one nested in another, and
//! the two byte counters. The budget is what `overhead` subtracts; these
//! keep it honest.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use zonetrace::{Zone, copied, enter, read, take};

static OUTER: Zone = Zone::new(1, "outer");
static INNER: Zone = Zone::new(2, "inner");

fn zones(c: &mut Criterion) {
    c.bench_function("zones/enter_exit_top", |b| {
        b.iter(|| drop(black_box(enter(&OUTER))));
    });
    let outer = enter(&OUTER);
    c.bench_function("zones/enter_exit_nested", |b| {
        b.iter(|| drop(black_box(enter(&INNER))));
    });
    c.bench_function("zones/count_read_and_copied", |b| {
        b.iter(|| {
            read(black_box(8192));
            copied(black_box(8192));
        });
    });
    drop(outer);
    let _ = take();
}

criterion_group!(benches, zones);
criterion_main!(benches);
