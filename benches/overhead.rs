//! What a zone costs with no allocator of the crate's installed: with the
//! feature `heap` off, and on (`cargo bench --features heap --bench
//! overhead`), where a zone still keeps its heap cells and must cost about
//! the same. See `shared/cases.rs`.

#[path = "shared/cases.rs"]
mod cases;

fn main() {
    let results = cases::zones();
    cases::report(&results, cases::CEILING);
    println!(
        "zones/{:<24} {:>7.2} ns",
        "overhead()",
        zonetrace::overhead()
    );
}
