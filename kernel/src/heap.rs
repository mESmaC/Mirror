//! Fixed bootstrap heap over owned, supervisor-only, non-executable RAM pages.
//!
//! Adjacent guard pages stay absent. The free-list allocator reuses and coalesces
//! released blocks; the mapped capacity is retained for the kernel's lifetime.
//! This is a bootstrap heap, not a growable or latency-bounded kernel allocator.
//! Interrupt/exception handlers must not allocate. The lock saves/restores IF so
//! later task-context allocation cannot deadlock a maskable interrupt on this CPU.

use core::{
    alloc::{GlobalAlloc, Layout},
    cell::UnsafeCell,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use linked_list_allocator::Heap;

use crate::{
    arch,
    memory::{FrameAllocator, PAGE_SIZE},
    paging::{PageTables, PagingError},
};

pub const HEAP_START: u64 = 0xffff_d000_0000_0000;
pub const HEAP_SIZE: usize = 1024 * 1024;
const HEAP_PAGES: usize = HEAP_SIZE / PAGE_SIZE as usize;

#[derive(Debug, PartialEq, Eq)]
pub enum HeapError {
    AlreadyInitialized,
    RegionInUse,
    OutOfFrames,
    Paging(PagingError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapStats {
    pub size: usize,
    pub used: usize,
    pub free: usize,
}

struct KernelAllocator {
    locked: AtomicBool,
    heap: UnsafeCell<Heap>,
}

// SAFETY: Every access to the heap is protected by the acquire/release lock.
// Maskable interrupts are disabled before acquisition. Handler paths do not
// allocate, including non-maskable exceptions. Only one CPU currently runs.
unsafe impl Sync for KernelAllocator {}

impl KernelAllocator {
    const fn empty() -> Self {
        Self {
            locked: AtomicBool::new(false),
            heap: UnsafeCell::new(Heap::empty()),
        }
    }

    fn lock(&self) -> HeapGuard<'_> {
        let restore_interrupts = arch::save_and_disable_interrupts();
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        HeapGuard {
            allocator: self,
            restore_interrupts,
            _same_cpu: PhantomData,
        }
    }
}

struct HeapGuard<'a> {
    allocator: &'a KernelAllocator,
    restore_interrupts: bool,
    // The saved interrupt state belongs to this CPU and cannot travel with a task.
    _same_cpu: PhantomData<*mut ()>,
}

impl Deref for HeapGuard<'_> {
    type Target = Heap;

    fn deref(&self) -> &Heap {
        // SAFETY: This guard exclusively owns the lock until it is dropped.
        unsafe { &*self.allocator.heap.get() }
    }
}

impl DerefMut for HeapGuard<'_> {
    fn deref_mut(&mut self) -> &mut Heap {
        // SAFETY: The lock and exclusive guard borrow exclude every other access.
        unsafe { &mut *self.allocator.heap.get() }
    }
}

impl Drop for HeapGuard<'_> {
    fn drop(&mut self) {
        self.allocator.locked.store(false, Ordering::Release);
        // SAFETY: Restore the same CPU's saved state only after releasing the
        // lock. IRQ handlers are initialized before interrupts can be enabled.
        unsafe { arch::restore_interrupts(self.restore_interrupts) };
    }
}

// SAFETY: The backing memory is exclusively owned, permanently mapped RAM after
// init. Heap provides aligned disjoint allocations, preserves live allocations,
// and merges freed space. Access is serialized without allocating recursively.
// Before init and on exhaustion alloc returns null; no allocation panics by design.
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Bound inputs before the free list rounds sizes and higher-half
        // pointers upward. A valid Layout can still request near-usize-limit
        // sizes/alignments; our fixed heap cannot service those requests.
        // The bootstrap allocator deliberately supports alignments up to its
        // capacity even when HEAP_START happens to satisfy a larger alignment.
        if layout.size() > HEAP_SIZE || layout.align() > HEAP_SIZE {
            return core::ptr::null_mut();
        }
        self.lock()
            .allocate_first_fit(layout)
            .map_or(core::ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: GlobalAlloc requires a non-null live allocation from this
        // allocator and exactly its original layout, relinquished by the caller.
        unsafe {
            self.lock()
                .deallocate(NonNull::new_unchecked(pointer), layout)
        };
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator::empty();

pub fn stats() -> HeapStats {
    let heap = ALLOCATOR.lock();
    HeapStats {
        size: heap.size(),
        used: heap.used(),
        free: heap.free(),
    }
}

/// Map and initialize the fixed bootstrap heap once.
///
/// Failures before initialization unmap and recycle every data frame acquired
/// here. Any empty parent tables created by the mapper remain owned by it.
///
/// # Safety
/// Run on the sole boot CPU with interrupts disabled, the given page hierarchy
/// active, and its unique frame allocator configured for recycling. No other
/// subsystem may access the reserved heap range or its adjacent guard pages.
/// Preserve these mappings for the remainder of the kernel's lifetime.
pub unsafe fn init(
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
) -> Result<(), HeapError> {
    let mut heap = ALLOCATOR.lock();
    if heap.size() != 0 {
        return Err(HeapError::AlreadyInitialized);
    }

    // Preflight every leaf plus both guard pages before acquiring data frames.
    for page in 0..HEAP_PAGES + 2 {
        let address = HEAP_START - PAGE_SIZE + page as u64 * PAGE_SIZE;
        if tables
            .translate(address)
            .map_err(HeapError::Paging)?
            .is_some()
        {
            return Err(HeapError::RegionInUse);
        }
    }

    for page in 0..HEAP_PAGES {
        let Some(physical) = frames.allocate() else {
            // SAFETY: Exactly the preceding pages were mapped here and have not
            // been exposed to the allocator or any other memory consumer.
            unsafe { rollback(tables, frames, page) };
            return Err(HeapError::OutOfFrames);
        };
        let address = HEAP_START + page as u64 * PAGE_SIZE;
        // SAFETY: This is fresh owned RAM, an absent reserved address, and the
        // unique active hierarchy. No references to the data frame exist.
        if let Err(error) = unsafe { tables.map_page(address, physical, frames) } {
            // SAFETY: map_page does not install a leaf on failure. This fresh
            // frame and the preceding heap mappings have no live users.
            unsafe {
                frames
                    .deallocate(physical)
                    .expect("recycle unmapped heap frame");
                rollback(tables, frames, page);
            }
            return Err(HeapError::Paging(error));
        }
    }

    // SAFETY: All contiguous heap pages are now owned and writable. Initialization
    // is exclusive, once-only, and no later code removes these mappings. Clearing
    // them avoids exposing stale physical-page contents to the first allocations.
    unsafe {
        (HEAP_START as *mut u8).write_bytes(0, HEAP_SIZE);
        heap.init(HEAP_START as *mut u8, HEAP_SIZE);
    }
    Ok(())
}

/// # Safety
/// The first `pages` heap pages must be exclusively owned mappings created by
/// this initialization attempt, with no surviving reference to any data frame.
unsafe fn rollback(tables: &mut PageTables, frames: &mut FrameAllocator<'_>, pages: usize) {
    for page in (0..pages).rev() {
        let address = HEAP_START + page as u64 * PAGE_SIZE;
        // SAFETY: These mappings are unpublished. Unmapping invalidates the TLB
        // before their physical memory is returned to the unique frame allocator.
        unsafe {
            let physical = tables.unmap_page(address).expect("rollback heap mapping");
            frames.deallocate(physical).expect("recycle heap frame");
        }
    }
}

/// Exercise the real global allocator while the freshly initialized heap is empty.
#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    use alloc::{boxed::Box, vec::Vec};

    let interrupts_before = arch::interrupts_enabled();
    let baseline = stats();
    assert_eq!(
        baseline,
        HeapStats {
            size: HEAP_SIZE,
            used: 0,
            free: HEAP_SIZE
        }
    );

    let layouts = [
        Layout::from_size_align(1, 1).unwrap(),
        Layout::from_size_align(17, 16).unwrap(),
        Layout::from_size_align(127, 64).unwrap(),
        Layout::from_size_align(4097, 4096).unwrap(),
        Layout::from_size_align(71, 32768).unwrap(),
    ];
    let mut allocations = [core::ptr::null_mut(); 5];
    for (index, layout) in layouts.iter().copied().enumerate() {
        // SAFETY: The layouts are nonzero and valid. Null is checked before use.
        let pointer = unsafe { ALLOCATOR.alloc(layout) };
        assert!(!pointer.is_null(), "mixed heap allocation failed");
        assert_eq!(pointer as usize % layout.align(), 0);
        assert!(
            (HEAP_START as usize..HEAP_START as usize + HEAP_SIZE).contains(&(pointer as usize))
        );
        assert!(pointer as usize + layout.size() <= HEAP_START as usize + HEAP_SIZE);
        for earlier in 0..index {
            let start = allocations[earlier] as usize;
            let end = start + layouts[earlier].size();
            assert!(pointer as usize >= end || pointer as usize + layout.size() <= start);
        }
        // SAFETY: This disjoint allocation owns layout.size() writable bytes.
        unsafe { pointer.write_bytes(0x40 + index as u8, layout.size()) };
        allocations[index] = pointer;
    }
    for index in [2, 0, 4, 1, 3] {
        let pointer = allocations[index];
        for offset in 0..layouts[index].size() {
            // SAFETY: This allocation remains live and these bytes were written.
            assert_eq!(unsafe { pointer.add(offset).read() }, 0x40 + index as u8);
        }
        // SAFETY: Free each allocation exactly once using its original layout.
        unsafe { ALLOCATOR.dealloc(pointer, layouts[index]) };
    }
    assert_eq!(stats(), baseline);

    for impossible in [
        Layout::from_size_align(isize::MAX as usize, 1).unwrap(),
        Layout::from_size_align(1, 1_usize << 62).unwrap(),
    ] {
        // SAFETY: Both nonzero layouts are valid but exceed heap capabilities.
        assert!(unsafe { ALLOCATOR.alloc(impossible) }.is_null());
    }

    // Fill the heap completely, make an interior hole, and prove it is reused.
    let quarter = Layout::from_size_align(HEAP_SIZE / 4, 16).unwrap();
    let mut quarters = [core::ptr::null_mut(); 4];
    for pointer in &mut quarters {
        // SAFETY: The valid nonzero layout fits exactly four times in this heap.
        *pointer = unsafe { ALLOCATOR.alloc(quarter) };
        assert!(!pointer.is_null());
    }
    assert_eq!(stats().free, 0);
    // SAFETY: Valid nonzero layout; exhaustion must return null without trapping.
    assert!(unsafe { ALLOCATOR.alloc(layouts[0]) }.is_null());
    let hole = quarters[1];
    // SAFETY: This is a live allocation with exactly its original layout.
    unsafe { ALLOCATOR.dealloc(hole, quarter) };
    // SAFETY: Valid layout, and the only free range has exactly this size.
    quarters[1] = unsafe { ALLOCATOR.alloc(quarter) };
    assert_eq!(quarters[1], hole);
    for index in [1, 3, 0, 2] {
        // SAFETY: Each quarter is live and freed once with its original layout.
        unsafe { ALLOCATOR.dealloc(quarters[index], quarter) };
    }
    let whole = Layout::from_size_align(HEAP_SIZE, 16).unwrap();
    // SAFETY: The layout is valid; success proves every free quarter coalesced.
    let pointer = unsafe { ALLOCATOR.alloc(whole) };
    assert_eq!(pointer as u64, HEAP_START);
    // SAFETY: The successful allocation above owns exactly this original layout.
    unsafe { ALLOCATOR.dealloc(pointer, whole) };
    assert_eq!(stats(), baseline);

    {
        let boxed = Box::new([0x4d49_5252_4f52_u64; 19]);
        let mut values = Vec::new();
        for index in 0..1025_u64 {
            values.push(index.wrapping_mul(31));
        }
        core::hint::black_box(&boxed);
        core::hint::black_box(&values);
        assert!(boxed.iter().all(|value| *value == 0x4d49_5252_4f52));
        assert!(
            values
                .iter()
                .enumerate()
                .all(|(index, value)| *value == index as u64 * 31)
        );
        let mut impossible = Vec::<u8>::new();
        assert!(impossible.try_reserve_exact(HEAP_SIZE + 1).is_err());
        assert_eq!(impossible.capacity(), 0);
    }
    assert_eq!(stats(), baseline, "heap smoke test leaked allocations");
    assert_eq!(arch::interrupts_enabled(), interrupts_before);
    crate::serial::println!(
        "MIRROR_HEAP_OK size={} free={}",
        baseline.size,
        baseline.free
    );
}
