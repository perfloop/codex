//! Allocation counter for the unchanged-fence path in the streaming markdown normalizer.
//!
//! The normalizer's output is consumed and checked against the source on each operation. A timed
//! pass runs with allocation counting disabled; a second pass enables the counter only around the
//! normalizer, so fixture construction and validation do not contribute to allocation metrics.

use codex_tui::benchmark_unwrap_markdown_fences;
use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::alloc::System;
use std::cell::Cell;
use std::env;
use std::hint::black_box;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

const ITERATIONS: u64 = 512;
const TARGET_SOURCE_BYTES: usize = 64 * 1024;

struct CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

static ALLOCATION_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOCATION_BYTES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static COUNTING_ENABLED: Cell<bool> = const { Cell::new(false) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            record_allocation(new_size);
        }
        new_pointer
    }
}

#[derive(Clone, Copy, Default)]
struct AllocationCounts {
    calls: u64,
    bytes: u64,
}

impl std::ops::AddAssign for AllocationCounts {
    fn add_assign(&mut self, other: Self) {
        self.calls += other.calls;
        self.bytes += other.bytes;
    }
}

fn record_allocation(bytes: usize) {
    COUNTING_ENABLED.with(|enabled| {
        if enabled.get() {
            ALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOCATION_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
        }
    });
}

fn begin_measurement() {
    ALLOCATION_CALLS.store(0, Ordering::Relaxed);
    ALLOCATION_BYTES.store(0, Ordering::Relaxed);
    COUNTING_ENABLED.with(|enabled| enabled.set(true));
}

fn end_measurement() -> AllocationCounts {
    COUNTING_ENABLED.with(|enabled| enabled.set(false));
    AllocationCounts {
        calls: ALLOCATION_CALLS.load(Ordering::Relaxed),
        bytes: ALLOCATION_BYTES.load(Ordering::Relaxed),
    }
}

fn unfinished_rust_fence(variant: u8) -> String {
    let mut source = String::with_capacity(TARGET_SOURCE_BYTES);
    source.push_str("```rust\n");
    while source.len() + "let value = 0;\n".len() + 5 <= TARGET_SOURCE_BYTES {
        source.push_str("let value = 0;\n");
    }
    source.push_str("// ");
    source.push(char::from(variant));
    source.push('\n');
    source
}

fn run_unfinished_rust_fence() {
    // Distinct runtime-owned inputs prevent compile-time folding while keeping every operation on
    // the same large, unchanged non-markdown fence path.
    let inputs = [unfinished_rust_fence(b'a'), unfinished_rust_fence(b'b')];
    let mut total_allocations = AllocationCounts::default();
    let mut total_nanoseconds = 0u128;

    for iteration in 0..ITERATIONS {
        let source = black_box(inputs[(iteration as usize) % inputs.len()].as_str());

        let started = Instant::now();
        let timed_normalized = benchmark_unwrap_markdown_fences(source);
        total_nanoseconds += started.elapsed().as_nanos();
        let timed_output = black_box(timed_normalized.as_ref());
        assert_eq!(
            timed_output, source,
            "an unchanged non-markdown fence must retain its source bytes"
        );
        black_box(timed_output.len());

        begin_measurement();
        let counted_normalized = benchmark_unwrap_markdown_fences(source);
        let counted_output = black_box(counted_normalized.as_ref());
        total_allocations += end_measurement();
        assert_eq!(
            counted_output, source,
            "an unchanged non-markdown fence must retain its source bytes"
        );
        black_box(counted_output.len());
    }

    println!(
        r#"{{"metric":"ns/op","value":{}}}"#,
        total_nanoseconds / u128::from(ITERATIONS)
    );
    println!(
        r#"{{"metric":"allocated_bytes/op","value":{}}}"#,
        total_allocations.bytes / ITERATIONS
    );
    println!(
        r#"{{"metric":"allocations/op","value":{}}}"#,
        total_allocations.calls / ITERATIONS
    );
}

fn main() {
    match env::args().nth(1).as_deref() {
        Some("unfinished-rust-64k") => run_unfinished_rust_fence(),
        _ => {
            eprintln!("usage: markdown_fence_unwrap unfinished-rust-64k");
            std::process::exit(2);
        }
    }
}
