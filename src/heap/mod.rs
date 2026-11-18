//! What the code in each zone did to the heap (feature `heap`).
//!
//! The binary installs [`Counting`] as its global allocator:
//!
//! ```ignore
//! #[global_allocator]
//! static HEAP: zonetrace::heap::Counting<std::alloc::System> =
//!     zonetrace::heap::Counting::new(std::alloc::System);
//! ```
//!
//! Every allocation, reallocation and free is then booked, as it happens,
//! to the zone open on the thread that made it: the node of that zone sums
//! allocations, bytes allocated (fresh, and what a reallocation grew by),
//! reallocations, frees, bytes freed (and what a reallocation shrank by),
//! and the most bytes live on the thread above where each call began. The
//! counts are a node's own, as `read` and `copied` are: a child's are in the
//! child's row. The peak is the call's whole stretch, children included.
//!
//! Booking never allocates, borrows or locks: a few loads and stores to
//! cells of the thread's fixed state. Three more rows keep the books whole:
//!
//! - `zones.tracer`: what the tracer itself allocated (a new path's node,
//!   taking and merging reports, formatting them); never charged to a zone.
//! - `zones.outside`: what the thread did with no zone open.
//! - `zones.unplaced`: what a node past the fixed counters did (a thread
//!   with over 512 paths of zones).
//!
//! A block freed on another thread than the one that allocated it is
//! booked where it is freed, so a node's bytes freed can exceed its bytes
//! allocated, and live bytes are signed. A [`Stretch`](crate::Stretch)
//! crosses threads and carries no heap counts.
//!
//! Booking costs time inside the zone that allocates. [`overhead`]
//! measures it per event on the running machine, and a report takes it out
//! of each row's time by the events beneath it, as it does the zones'.

#[cfg(feature = "heap")]
mod count;

#[cfg(feature = "heap")]
pub(crate) use count::{Books, Cells, Mark, as_tracer, cost};
#[cfg(feature = "heap")]
pub use count::{Counting, overhead, thread_totals};

/// What a node, or a whole thread, did to the heap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Heap {
    /// Allocations made (not reallocations).
    pub allocs: u64,
    /// Bytes allocated: fresh allocations, and what reallocations grew by.
    pub bytes: u64,
    /// Reallocations made.
    pub reallocs: u64,
    /// What reallocations grew by: the part of `bytes` a reserved capacity
    /// would have saved.
    pub realloc_bytes: u64,
    /// Frees made.
    pub frees: u64,
    /// Bytes freed: freed blocks, and what reallocations shrank by.
    pub freed: u64,
    /// The most bytes live on the thread above where a call began, over
    /// its calls, children included; 0 for the books' own rows.
    pub peak: u64,
}

impl Heap {
    /// Allocator calls booked: allocations, reallocations and frees.
    #[must_use]
    pub const fn events(&self) -> u64 {
        self.allocs + self.reallocs + self.frees
    }

    /// Sums `other` in; the peak is the greater.
    pub fn add(&mut self, other: &Self) {
        self.allocs += other.allocs;
        self.bytes += other.bytes;
        self.reallocs += other.reallocs;
        self.realloc_bytes += other.realloc_bytes;
        self.frees += other.frees;
        self.freed += other.freed;
        self.peak = self.peak.max(other.peak);
    }

    /// This less `earlier`, field by field (the peak is this one's).
    #[must_use]
    pub const fn since(&self, earlier: &Self) -> Self {
        Self {
            allocs: self.allocs - earlier.allocs,
            bytes: self.bytes - earlier.bytes,
            reallocs: self.reallocs - earlier.reallocs,
            realloc_bytes: self.realloc_bytes - earlier.realloc_bytes,
            frees: self.frees - earlier.frees,
            freed: self.freed - earlier.freed,
            peak: self.peak,
        }
    }
}
