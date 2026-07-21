//! Allocation counter for the unchanged-fence path in the streaming markdown normalizer.
//!
//! The normalizer's output is consumed and checked against the source on each operation.  The
//! counter is enabled only around the normalizer, so fixture construction and validation do not
//! contribute to the reported allocation metrics.

use codex_tui::benchmark_unwrap_markdown_fences;
use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::alloc::System;
use std::cell::Cell;
use std::env;
use std::hint::black_box;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

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
    let mut total = AllocationCounts::default();

    for iteration in 0..ITERATIONS {
        let source = black_box(inputs[(iteration as usize) % inputs.len()].as_str());

        begin_measurement();
        let normalized = benchmark_unwrap_markdown_fences(source);
        let output = black_box(normalized.as_ref());
        total += end_measurement();

        assert_eq!(
            output, source,
            "an unchanged non-markdown fence must retain its source bytes"
        );
        black_box(output.len());
    }

    println!(
        r#"{{"metric":"allocated_bytes/op","value":{}}}"#,
        total.bytes / ITERATIONS
    );
    println!(
        r#"{{"metric":"allocations/op","value":{}}}"#,
        total.calls / ITERATIONS
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
