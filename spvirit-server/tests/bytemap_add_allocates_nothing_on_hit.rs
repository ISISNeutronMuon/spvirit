//! `ByteMap::add` is called on the byte-accounting path for every frame and
//! every monitor fan-out. `entry(key.to_string())` allocates a fresh String on
//! every call, including the overwhelmingly common case where the key is
//! already in the map.
//!
//! This binary installs a **global** allocator, so it must contain only
//! allocation tests. The counter and the arming flag are `thread_local!` (with
//! `const` initialisers, so touching them cannot itself allocate and they
//! register no destructor): cargo runs the tests in this binary on several
//! threads at once, and a process-wide `AtomicUsize` would count the other
//! test's — and the libtest harness's — allocations too, making the assertion
//! flaky.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use spvirit_server::diag::ByteMap;

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // `try_with` because a TLS access during thread teardown would
        // otherwise panic inside the allocator.
        let _ = COUNTING.try_with(|c| {
            if c.get() {
                let _ = ALLOCS.try_with(|a| a.set(a.get() + 1));
            }
        });
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn adding_to_an_existing_key_allocates_nothing() {
    let map = ByteMap::new();
    // Create the key, and let any lazy one-time machinery warm up.
    map.add("DEV:PV:LONG:NAME:THAT:WOULD:HEAP:ALLOCATE", 1);
    map.add("DEV:PV:LONG:NAME:THAT:WOULD:HEAP:ALLOCATE", 1);

    ALLOCS.with(|a| a.set(0));
    COUNTING.with(|c| c.set(true));
    for _ in 0..100 {
        map.add("DEV:PV:LONG:NAME:THAT:WOULD:HEAP:ALLOCATE", 2);
    }
    COUNTING.with(|c| c.set(false));

    assert_eq!(
        ALLOCS.with(|a| a.get()),
        0,
        "ByteMap::add must not allocate when the key already exists"
    );

    let snap = map.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].1, 202, "counts must still accumulate correctly");
}

#[test]
fn adding_a_new_key_still_creates_it() {
    let map = ByteMap::new();
    map.add("A", 3);
    map.add("B", 4);
    map.add("A", 5);
    let mut snap = map.snapshot();
    snap.sort();
    assert_eq!(snap, vec![("A".to_string(), 8), ("B".to_string(), 4)]);
}
