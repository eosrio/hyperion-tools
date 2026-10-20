//! Optional profiling instrumentation, excluded from normal release builds.
//! Counts Rust allocator requests, not C allocations, usable bytes or physical RSS.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Default)]
struct Counters {
    allocations: AtomicU64,
    reallocations: AtomicU64,
    deallocations: AtomicU64,
    requested: AtomicU64,
    live: AtomicU64,
    peak: AtomicU64,
}

impl Counters {
    const fn new() -> Self {
        Self {
            allocations: AtomicU64::new(0),
            reallocations: AtomicU64::new(0),
            deallocations: AtomicU64::new(0),
            requested: AtomicU64::new(0),
            live: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        }
    }
    fn grow(&self, bytes: usize) {
        let live = self.live.fetch_add(bytes as u64, Relaxed) + bytes as u64;
        self.peak.fetch_max(live, Relaxed);
    }
    fn allocated(&self, bytes: usize) {
        self.allocations.fetch_add(1, Relaxed);
        self.requested.fetch_add(bytes as u64, Relaxed);
        self.grow(bytes);
    }
    fn resized(&self, old: usize, new: usize) {
        self.reallocations.fetch_add(1, Relaxed);
        self.requested.fetch_add(new as u64, Relaxed);
        if new >= old {
            self.grow(new - old);
        } else {
            self.live.fetch_sub((old - new) as u64, Relaxed);
        }
    }
    fn freed(&self, bytes: usize) {
        self.deallocations.fetch_add(1, Relaxed);
        self.live.fetch_sub(bytes as u64, Relaxed);
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            allocations: self.allocations.load(Relaxed),
            reallocations: self.reallocations.load(Relaxed),
            deallocations: self.deallocations.load(Relaxed),
            requested_bytes: self.requested.load(Relaxed),
            live_requested_bytes: self.live.load(Relaxed),
            peak_live_requested_bytes: self.peak.load(Relaxed),
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct Snapshot {
    allocations: u64,
    reallocations: u64,
    deallocations: u64,
    requested_bytes: u64,
    live_requested_bytes: u64,
    peak_live_requested_bytes: u64,
}

static COUNTERS: Counters = Counters::new();
struct MeasuredSystem;

// Every operation forwards the caller's unchanged pointer/layout to System. Bookkeeping
// uses only atomics, never allocates, and counts only successful allocation/reallocation.
unsafe impl GlobalAlloc for MeasuredSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            COUNTERS.allocated(layout.size());
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            COUNTERS.allocated(layout.size());
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        COUNTERS.freed(layout.size());
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let resized = unsafe { System.realloc(pointer, layout, new_size) };
        if !resized.is_null() {
            COUNTERS.resized(layout.size(), new_size);
        }
        resized
    }
}

#[global_allocator]
static ALLOCATOR: MeasuredSystem = MeasuredSystem;

pub fn report() {
    let snapshot = COUNTERS.snapshot();
    // Snapshot before serialization so diagnostics do not count their own formatting.
    eprintln!(
        "allocation-metrics: {}",
        serde_json::to_string(&snapshot).unwrap()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_layout_requests_growth_shrink_and_release() {
        let counters = Counters::new();
        counters.allocated(16);
        counters.resized(16, 64);
        counters.resized(64, 8);
        counters.freed(8);
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.allocations, 1);
        assert_eq!(snapshot.reallocations, 2);
        assert_eq!(snapshot.deallocations, 1);
        assert_eq!(snapshot.requested_bytes, 88);
        assert_eq!(snapshot.live_requested_bytes, 0);
        assert_eq!(snapshot.peak_live_requested_bytes, 64);
    }
}
