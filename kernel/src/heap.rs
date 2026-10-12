//! Growable BSP heap over owned, supervisor-only, non-executable RAM pages.
//!
//! One MiB is initially backed inside a reserved 16 MiB arena. Explicit growth
//! adds at most one MiB per call and publishes free space only after every CPU
//! acknowledges its new mappings. The free-list allocator reuses and coalesces
//! released blocks; committed capacity remains mapped for the kernel's lifetime.
//! Allocation never implicitly grows the heap or borrows the frame allocator.
//! Interrupt/exception handlers must not allocate. The lock saves/restores IF so
//! later task-context allocation cannot deadlock a maskable interrupt on this CPU.

use core::{
    alloc::{GlobalAlloc, Layout},
    cell::UnsafeCell,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
};

use linked_list_allocator::Heap;

use crate::{
    arch,
    memory::{FrameAllocator, PAGE_SIZE},
    paging::{PageTables, PagingError},
};

pub const HEAP_START: u64 = 0xffff_d000_0000_0000;
pub const HEAP_SIZE: usize = 1024 * 1024;
pub const HEAP_MAX_SIZE: usize = 16 * 1024 * 1024;
pub const MAX_GROW_PAGES: usize = 256;
const HEAP_PAGES: usize = HEAP_SIZE / PAGE_SIZE as usize;

#[derive(Debug, PartialEq, Eq)]
pub enum HeapError {
    AlreadyInitialized,
    NotInitialized,
    InvalidGrowth,
    LimitExceeded,
    InvalidContext,
    Busy,
    OutOfFrames,
    Paging(PagingError),
    #[cfg(feature = "boot-smoke")]
    Cancelled,
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
    /// The largest failed allocation awaiting growth: (key, packed layout),
    /// guarded by this allocator's lock. The key is size + align - 1 (0059-D1).
    shortfall: UnsafeCell<(u64, u64)>,
    /// The slab layer's live pages (0059-D2), guarded by the same lock.
    slabs: UnsafeCell<SlabState>,
}

// SAFETY: Every access to the heap is protected by the acquire/release lock.
// Maskable interrupts are disabled before acquisition. Handler paths do not
// allocate, including non-maskable exceptions. Only the BSP currently uses the
// allocator; AP startup, idle and interrupt paths never allocate.
unsafe impl Sync for KernelAllocator {}

impl KernelAllocator {
    const fn empty() -> Self {
        Self {
            locked: AtomicBool::new(false),
            heap: UnsafeCell::new(Heap::empty()),
            shortfall: UnsafeCell::new((0, 0)),
            slabs: UnsafeCell::new(SlabState {
                pages: [0; SLAB_PAGE_LIMIT],
            }),
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

    /// Record a missed layout for the next safe point (0059-D1), keeping the
    /// largest pending one by its size + align - 1 key. The caller holds the
    /// heap lock.
    fn note_shortfall_locked(&self, layout: Layout) {
        let key = layout_key(layout);
        let shortfall = self.shortfall.get();
        // SAFETY: the caller holds the heap lock, so the cell is owned.
        unsafe {
            if key > (*shortfall).0 {
                *shortfall = (key, pack_layout(layout));
            }
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
        // sizes/alignments; our bounded arena cannot service those requests.
        // The allocator deliberately supports alignments up to the arena's
        // capacity even when HEAP_START happens to satisfy a larger alignment.
        if layout.size() > HEAP_MAX_SIZE || layout.align() > HEAP_MAX_SIZE {
            return core::ptr::null_mut();
        }
        let mut heap = self.lock();
        // Slab classes serve from their pages first (0059-D2).
        if slab_class(layout).is_some() {
            let pointer = slab_alloc(&mut heap, &self.slabs, layout);
            if !pointer.is_null() {
                return pointer;
            }
            // A slab refill miss records a page-sized shortfall (0059-D1), so a
            // safe point grows for it even when first-fit serves this object.
            let page_layout = Layout::from_size_align(PAGE_SIZE as usize, PAGE_SIZE as usize)
                .expect("page layout");
            self.note_shortfall_locked(page_layout);
        }
        match heap.allocate_first_fit(layout) {
            Ok(pointer) => pointer.as_ptr(),
            Err(()) => {
                // 0059-D1: record the miss for the next safe point, keeping the
                // largest pending layout by its size + align - 1 key.
                self.note_shortfall_locked(layout);
                core::ptr::null_mut()
            }
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let mut heap = self.lock();
        // The page table decides: a slab-served pointer's page is tracked
        // (0059-D2); a first-fit-served one's is not, even with a slab layout.
        if slab_class(layout).is_some() && slab_tracked(&self.slabs, pointer) {
            slab_free(&mut heap, &self.slabs, pointer, layout);
            return;
        }
        // SAFETY: GlobalAlloc requires a non-null live allocation from this
        // allocator and exactly its original layout, relinquished by the caller.
        unsafe { heap.deallocate(NonNull::new_unchecked(pointer), layout) };
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator::empty();

// Mapping waits briefly enable IRQs. Serialize growth independently of the
// allocator lock, which must never be held across a shootdown barrier.
static GROWING: AtomicBool = AtomicBool::new(false);

struct GrowthGuard(PhantomData<*mut ()>);

impl GrowthGuard {
    fn acquire() -> Result<Self, HeapError> {
        if arch::interrupts_enabled() || crate::smp::is_secondary() {
            return Err(HeapError::InvalidContext);
        }
        GROWING
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| HeapError::Busy)?;
        Ok(Self(PhantomData))
    }
}

impl Drop for GrowthGuard {
    fn drop(&mut self) {
        GROWING.store(false, Ordering::Release);
    }
}

pub fn stats() -> HeapStats {
    let heap = ALLOCATOR.lock();
    HeapStats {
        size: heap.size(),
        used: heap.used(),
        free: heap.free(),
    }
}

/// Reserve the maximum arena and initialize its first MiB once.
///
/// Frame acquisition and mapping-preflight errors recycle every data frame
/// acquired here. Parent tables remain pinned. Once mapping publication starts,
/// a failed shootdown never returns; backing cannot escape before acknowledgement.
///
/// # Safety
/// Run on the sole boot CPU with interrupts disabled, the given page hierarchy
/// active, and its unique frame allocator configured for recycling. No other
/// subsystem may access the reserved maximum arena or its adjacent guard pages.
/// `offset` is the retained writable HHDM for every allocated frame. Prepare
/// before any AP or retained userspace root exists. Preserve committed mappings
/// for the remainder of the kernel's lifetime.
pub unsafe fn init(
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
    offset: u64,
) -> Result<(), HeapError> {
    let _growth = GrowthGuard::acquire()?;
    if stats().size != 0 {
        return Err(HeapError::AlreadyInitialized);
    }
    // SAFETY: Bootstrap-only exclusive hierarchy and untouched maximum arena.
    unsafe { tables.prepare_heap_window(frames) }.map_err(HeapError::Paging)?;
    let mut physical = [0; MAX_GROW_PAGES];
    // SAFETY: Unique allocator and retained HHDM; no pointer into these fresh
    // frames escapes. There is no heap allocation while acquiring backing.
    unsafe { acquire_frames(frames, offset, &mut physical, HEAP_PAGES, usize::MAX) }?;
    // SAFETY: Every frame is unique, zeroed, and owned. No consumer sees the
    // reserved alias before initialization; every parent was prepared above.
    if let Err(error) = unsafe { tables.map_heap_pages(HEAP_START, &physical[..HEAP_PAGES]) } {
        // SAFETY: Batch mapping errors precede every PTE publication.
        unsafe { recycle_frames(frames, &physical[..HEAP_PAGES]) };
        return Err(HeapError::Paging(error));
    }
    let mut heap = ALLOCATOR.lock();
    assert_eq!(
        heap.size(),
        0,
        "heap initialized during backing acquisition"
    );
    // SAFETY: Every contiguous page is zeroed, owned, and fully published. This
    // once-only initialization cannot race an allocator user. Backing is permanent.
    unsafe {
        heap.init(HEAP_START as *mut u8, HEAP_SIZE);
    }
    Ok(())
}

/// Append 1..=MAX_GROW_PAGES pages without moving any live allocation.
///
/// Errors before mapping publication preserve heap capacity and frame ownership.
/// Successful pages become permanent heap backing only after the shootdown.
/// This is an explicit task-context operation; GlobalAlloc never calls it.
///
/// # Safety
/// Run on the BSP with IF=0 and this kernel root active, exclusively borrowing
/// the original frame allocator. `offset` preserves its complete writable HHDM.
/// Other CPUs, DMA, and interrupt handlers must not access the uncommitted tail;
/// handlers never allocate or acquire the heap lock. Hold no IRQ-needed lock.
/// Existing heap allocations stay live; never unmap or recycle committed backing.
pub unsafe fn grow(
    tables: &PageTables,
    frames: &mut FrameAllocator<'_>,
    offset: u64,
    pages: usize,
) -> Result<HeapStats, HeapError> {
    // SAFETY: The caller establishes BSP ownership and retained mappings.
    unsafe { grow_inner(tables, frames, offset, pages, GrowthMode::Normal) }
}

enum GrowthMode {
    Normal,
    #[cfg(feature = "boot-smoke")]
    FailFramesAfter(usize),
    #[cfg(feature = "boot-smoke")]
    CancelBeforeCommit,
}

unsafe fn grow_inner(
    tables: &PageTables,
    frames: &mut FrameAllocator<'_>,
    offset: u64,
    pages: usize,
    mode: GrowthMode,
) -> Result<HeapStats, HeapError> {
    let _growth = GrowthGuard::acquire()?;
    let before = stats();
    let bytes = growth_bytes(before.size, pages)?;
    let address = HEAP_START + before.size as u64;
    #[cfg(feature = "boot-smoke")]
    let frame_limit = match mode {
        GrowthMode::FailFramesAfter(limit) => limit,
        _ => usize::MAX,
    };
    #[cfg(not(feature = "boot-smoke"))]
    let frame_limit = {
        let _ = mode;
        usize::MAX
    };
    let mut physical = [0; MAX_GROW_PAGES];
    // SAFETY: All fresh pages remain private and every failure recycles its
    // complete acquired prefix before any mapping becomes visible.
    unsafe { acquire_frames(frames, offset, &mut physical, pages, frame_limit) }?;
    // SAFETY: The new tail is absent and private, backed by unique zeroed frames.
    // No heap guard is held while this operation waits for remote invalidations.
    if let Err(error) = unsafe { tables.map_heap_pages(address, &physical[..pages]) } {
        // SAFETY: An error guarantees that no mapping was published.
        unsafe { recycle_frames(frames, &physical[..pages]) };
        return Err(HeapError::Paging(error));
    }
    #[cfg(feature = "boot-smoke")]
    {
        // SAFETY: The complete new tail is still private, zeroed and stable;
        // no free-list node or allocation has been published there yet. Probe
        // both ends with no allocator lock held to warm remote translations.
        let first_cpus = unsafe { crate::smp::probe_shared_page(address, 0) };
        let last = address + (pages as u64 - 1) * PAGE_SIZE;
        assert_eq!(
            // SAFETY: The final page has the same private, stable backing.
            unsafe { crate::smp::probe_shared_page(last, 0) },
            first_cpus
        );
        assert!(first_cpus > 0);
    }
    #[cfg(feature = "boot-smoke")]
    if matches!(mode, GrowthMode::CancelBeforeCommit) {
        // SAFETY: The new tail was never exposed to Heap or any consumer. Exact
        // shootdown completion precedes reclamation. A failed withdrawal panics
        // and cannot free a page whose translation may survive on another CPU.
        unsafe {
            tables
                .unmap_heap_pages(address, &physical[..pages])
                .expect("cancel unpublished heap growth");
            recycle_frames(frames, &physical[..pages]);
        }
        return Err(HeapError::Cancelled);
    }
    let mut heap = ALLOCATOR.lock();
    assert_eq!(heap.size(), before.size, "concurrent heap extension");
    assert_eq!(heap.top() as u64, address, "heap tail mismatch");
    // SAFETY: This page-aligned zeroed tail is contiguous with all prior heap
    // memory, acknowledged on every CPU, and retained permanently. Exclusive
    // heap access publishes free-list nodes only after mapping completion.
    unsafe { heap.extend(bytes) };
    Ok(HeapStats {
        size: heap.size(),
        used: heap.used(),
        free: heap.free(),
    })
}

fn growth_bytes(size: usize, pages: usize) -> Result<usize, HeapError> {
    if size == 0 {
        return Err(HeapError::NotInitialized);
    }
    if pages == 0 || pages > MAX_GROW_PAGES {
        return Err(HeapError::InvalidGrowth);
    }
    let bytes = pages * PAGE_SIZE as usize;
    if size > HEAP_MAX_SIZE || bytes > HEAP_MAX_SIZE - size {
        return Err(HeapError::LimitExceeded);
    }
    Ok(bytes)
}

/// Acquire and initialize a complete private batch, recycling its prefix on error.
unsafe fn acquire_frames(
    frames: &mut FrameAllocator<'_>,
    offset: u64,
    physical: &mut [u64; MAX_GROW_PAGES],
    pages: usize,
    limit: usize,
) -> Result<(), HeapError> {
    for index in 0..pages {
        let frame = if index < limit {
            frames.allocate()
        } else {
            None
        };
        let Some(frame) = frame else {
            // SAFETY: These pages have only dormant direct-map aliases.
            unsafe { recycle_frames(frames, &physical[..index]) };
            return Err(HeapError::OutOfFrames);
        };
        physical[index] = frame;
        let Some(address) = offset.checked_add(frame) else {
            // SAFETY: The entire prefix, including this frame, stays unmapped.
            unsafe { recycle_frames(frames, &physical[..=index]) };
            return Err(HeapError::Paging(PagingError::DirectMapOverflow));
        };
        // SAFETY: Fresh complete RAM frame, valid writable HHDM, no live alias
        // or reference. Clear before any page walker can reach the heap alias.
        unsafe { (address as *mut u8).write_bytes(0, PAGE_SIZE as usize) };
    }
    Ok(())
}

/// The caller proves every frame is private and every published alias withdrawn.
unsafe fn recycle_frames(frames: &mut FrameAllocator<'_>, physical: &[u64]) {
    for &frame in physical.iter().rev() {
        // SAFETY: This batch owns each frame once, with no surviving mappings
        // except its dormant HHDM alias, references, DMA, or table use.
        unsafe { frames.deallocate(frame) }.expect("recycle uncommitted heap frame");
    }
}

// ---- Demand-driven growth at safe points (0059-D1) ----
//
// `alloc` never grows: it cannot reach the frame allocator, the kernel tables
// or the HHDM offset under its lock, and `grow`'s contract (BSP, IF=0, kernel
// root, exclusive frames, no IRQ-needed lock) is met only at safe points.
// So a failed allocation records its layout here and the next safe point's
// `ensure` grows for the mark or the shortfall.

/// The low-water mark: the largest free block the heap guarantees (0059-D1).
const LOW_WATER: usize = 256 * 1024;

/// Pack a layout as (size << 8) | align_log2. Both are bounded by
/// HEAP_MAX_SIZE (16 MiB), so the pack fits a u64.
fn pack_layout(layout: Layout) -> u64 {
    ((layout.size() as u64) << 8) | layout.align().trailing_zeros() as u64
}

fn unpack_layout(packed: u64) -> Option<Layout> {
    let size = (packed >> 8) as usize;
    let align = 1usize << (packed & 0xff);
    if size == 0 || size > HEAP_MAX_SIZE || align > HEAP_MAX_SIZE {
        return None;
    }
    Layout::from_size_align(size, align).ok()
}

/// The feasibility key (0059-D1): size + align - 1.
fn layout_key(layout: Layout) -> u64 {
    layout.size() as u64 + layout.align() as u64 - 1
}

/// The largest shortfall key whose growth already failed; a repeated shortfall
/// no larger than it never re-arms (0059-D1). Cleared when the frame back-off
/// re-arms.
static FAILED_SHORTFALL: AtomicU64 = AtomicU64::new(0);

// Back-off modes (0059-D1's failure policy).
const BACKOFF_ARMED: u8 = 0;
/// LimitExceeded, the transport or a bug-class failure: never re-arm.
const BACKOFF_NEVER: u8 = 1;
/// OutOfFrames: re-arm only when allocated_frames drops below the failure's.
const BACKOFF_WAIT_FRAMES: u8 = 2;
static BACKOFF_MODE: AtomicU8 = AtomicU8::new(BACKOFF_ARMED);
/// The post-rollback allocated_frames at the last OutOfFrames failure.
static BACKOFF_FRAMES: AtomicUsize = AtomicUsize::new(0);

/// Test 5's injection (boot-smoke only), read and consumed by ensure: 0 off,
/// 1 = cancel before commit, N >= 2 = fail after N - 2 frames.
#[cfg(feature = "boot-smoke")]
static ENSURE_INJECT: AtomicU64 = AtomicU64::new(0);
/// Counts each growth attempted under the injection, so a skipped attempt
/// fails the test.
#[cfg(feature = "boot-smoke")]
static ENSURE_INJECTED: AtomicU64 = AtomicU64::new(0);

/// The result of one safe-point ensure call (0059-D1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ensure {
    /// Nothing needed growth.
    Unchanged,
    /// The heap grew at least one step.
    Grew,
    /// Growth is still needed but the heap is at HEAP_MAX_SIZE: the caller's
    /// boot-smoke test prints the one accepted SKIPPED record (0059-D4).
    AtCap,
    /// The back-off declined (a real failure earlier, or no transport).
    Deferred,
}

/// Whether a free block can serve a layout of `size`/`align`, probed under the
/// lock (the crate's hole list is private, so this probe is how the mark and a
/// shortfall's fit are checked, 0059-D1). It allocates and immediately
/// releases, changing nothing it does not restore.
fn block_fits(heap: &mut Heap, size: usize, align: usize) -> bool {
    let Ok(layout) = Layout::from_size_align(size, align) else {
        return false;
    };
    match heap.allocate_first_fit(layout) {
        Ok(pointer) => {
            // SAFETY: the probe's own fresh allocation, released at once with
            // its exact layout; the crate merges the padding holes back.
            unsafe { heap.deallocate(pointer, layout) };
            true
        }
        Err(()) => false,
    }
}

/// Read the pending shortfall under the lock: (key, packed layout).
fn pending_shortfall() -> (u64, u64) {
    let _heap = ALLOCATOR.lock();
    // SAFETY: the heap lock is held above, so the cell is exclusively owned.
    unsafe { *ALLOCATOR.shortfall.get() }
}

/// Clear the pending shortfall under the lock (satisfied, infeasible, or a
/// battery's deliberate failure, whose records the tests clear).
#[cfg(feature = "boot-smoke")]
fn clear_shortfall() {
    let _heap = ALLOCATOR.lock();
    // SAFETY: the heap lock is held above.
    unsafe { *ALLOCATOR.shortfall.get() = (0, 0) };
}

// ---- Boot-smoke test hooks (0059's tests 2 and 5) ----

/// Arms the held-memory step: the relaunch pair's first loop top holds the
/// heap near-full, so its next loop-top ensure grows mid-session (test 2).
#[cfg(feature = "boot-smoke")]
static HOLD_ARMED: AtomicBool = AtomicBool::new(false);

/// Arms the hold for the next pair session (called by run_init between the
/// pairs).
#[cfg(feature = "boot-smoke")]
pub(crate) fn arm_hold_across_session() {
    HOLD_ARMED.store(true, Ordering::Relaxed);
}

/// Test 5's injection arming: 1 = cancel before commit, N >= 2 = fail after
/// N - 2 frames (consumed once by the next ensure's growth).
#[cfg(feature = "boot-smoke")]
pub(crate) fn arm_ensure_inject(value: u64) {
    ENSURE_INJECT.store(value, Ordering::Relaxed);
}

/// How many growths ran under the injection (0059 test 5 asserts it).
#[cfg(feature = "boot-smoke")]
pub(crate) fn ensure_injected() -> u64 {
    ENSURE_INJECTED.load(Ordering::Relaxed)
}

/// Begin the hold, if armed: hold all but 128 KiB of the free heap, so the
/// largest free block falls below the 256 KiB mark and the next loop-top
/// ensure grows exactly one step (256 pages). Returns the block to hold across
/// the session, or None when not armed.
#[cfg(feature = "boot-smoke")]
pub(crate) fn test_hold_begin() -> Option<(*mut u8, Layout)> {
    if !HOLD_ARMED.swap(false, Ordering::Relaxed) {
        return None;
    }
    let free = stats().free;
    let size = free - 128 * 1024;
    let layout = Layout::from_size_align(size, PAGE_SIZE as usize).expect("hold layout");
    // SAFETY: a valid bounded layout on the live heap; null is checked.
    let pointer = unsafe { ALLOCATOR.alloc(layout) };
    assert!(!pointer.is_null(), "the hold block must fit the free heap");
    Some((pointer, layout))
}

/// Free the held block at the session's drain, before its frame check.
#[cfg(feature = "boot-smoke")]
pub(crate) fn test_hold_end(held: Option<(*mut u8, Layout)>) {
    let Some((pointer, layout)) = held else {
        return;
    };
    // SAFETY: the block is live, owned, and freed once with its exact layout.
    unsafe { ALLOCATOR.dealloc(pointer, layout) };
}

/// Grow the heap on demand at a safe point (0059-D1), while the largest free
/// block is below the mark or a feasible recorded shortfall still misses.
///
/// The caller runs on the BSP with IF=0, the kernel root active, the frame
/// allocator exclusively borrowed, no IRQ-needed lock held and no syscall
/// handler frame live — the contract `grow` already requires. Failures print
/// nothing; test 2's one accepted record is printed by `capacity_self_test`.
pub fn ensure(tables: &PageTables, frames: &mut FrameAllocator<'_>, offset: u64) -> Ensure {
    match BACKOFF_MODE.load(Ordering::Relaxed) {
        BACKOFF_NEVER => return Ensure::Deferred,
        BACKOFF_WAIT_FRAMES => {
            let failed = BACKOFF_FRAMES.load(Ordering::Relaxed);
            let now = frames
                .stats()
                .map(|stats| stats.allocated_frames)
                .unwrap_or(failed);
            if now >= failed {
                return Ensure::Deferred;
            }
            // Frames were freed below the failure's level: re-arm.
            BACKOFF_MODE.store(BACKOFF_ARMED, Ordering::Relaxed);
            FAILED_SHORTFALL.store(0, Ordering::Relaxed);
        }
        _ => {}
    }
    // The transport preflight runs before any frame is acquired (0059-D1): a
    // boot whose shootdown transport is unavailable never grows at runtime.
    // The returned guard is dropped at once; map_heap_pages re-checks it.
    if crate::smp::prepare_kernel_update().is_err() {
        BACKOFF_MODE.store(BACKOFF_NEVER, Ordering::Relaxed);
        return Ensure::Deferred;
    }

    let mut outcome = Ensure::Unchanged;
    loop {
        let before = stats();
        let mut needed = false;
        {
            let mut heap = ALLOCATOR.lock();
            if !block_fits(&mut heap, LOW_WATER, PAGE_SIZE as usize) {
                needed = true;
            }
            // SAFETY: the heap lock is held above, so the cell is owned.
            let (key, packed) = unsafe { *ALLOCATOR.shortfall.get() };
            if key != 0 {
                if key <= FAILED_SHORTFALL.load(Ordering::Relaxed) {
                    // A repeated shortfall no larger than the failed one does
                    // not re-arm; drop it.
                    // SAFETY: the heap lock is held above.
                    unsafe { *ALLOCATOR.shortfall.get() = (0, 0) };
                } else if key > (HEAP_MAX_SIZE - before.size) as u64 {
                    // Infeasible below the cap: dropped silently, stay armed.
                    // SAFETY: the heap lock is held above.
                    unsafe { *ALLOCATOR.shortfall.get() = (0, 0) };
                } else {
                    match unpack_layout(packed) {
                        Some(layout) if block_fits(&mut heap, layout.size(), layout.align()) => {
                            // Satisfied by the current free space: no growth.
                            // SAFETY: the heap lock is held above.
                            unsafe { *ALLOCATOR.shortfall.get() = (0, 0) };
                        }
                        _ => needed = true,
                    }
                }
            }
        }
        if !needed {
            return outcome;
        }
        if before.size == HEAP_MAX_SIZE {
            // LimitExceeded: at the cap with growth still needed. Never re-arm.
            BACKOFF_MODE.store(BACKOFF_NEVER, Ordering::Relaxed);
            return Ensure::AtCap;
        }
        let pages = MAX_GROW_PAGES.min((HEAP_MAX_SIZE - before.size) / PAGE_SIZE as usize);
        // The frame preflight runs before every growth, so a growth completes
        // or never starts (0059-D1).
        let free_frames = frames
            .stats()
            .map(|stats| stats.recycled_frames + stats.never_allocated_frames);
        if free_frames.is_some_and(|free| free < pages) {
            BACKOFF_FRAMES.store(
                frames
                    .stats()
                    .map(|stats| stats.allocated_frames)
                    .unwrap_or(0),
                Ordering::Relaxed,
            );
            BACKOFF_MODE.store(BACKOFF_WAIT_FRAMES, Ordering::Relaxed);
            return if outcome == Ensure::Grew {
                outcome
            } else {
                Ensure::Deferred
            };
        }

        #[cfg(feature = "boot-smoke")]
        let (mode, injected) = match ENSURE_INJECT.swap(0, Ordering::Relaxed) {
            0 => (GrowthMode::Normal, false),
            1 => {
                ENSURE_INJECTED.fetch_add(1, Ordering::Relaxed);
                (GrowthMode::CancelBeforeCommit, true)
            }
            count => {
                ENSURE_INJECTED.fetch_add(1, Ordering::Relaxed);
                (GrowthMode::FailFramesAfter((count - 2) as usize), true)
            }
        };
        #[cfg(not(feature = "boot-smoke"))]
        let (mode, injected) = (GrowthMode::Normal, false);

        // SAFETY: the caller established the safe-point contract (BSP, IF=0,
        // kernel root, exclusive frames, no IRQ-needed lock, no syscall frame).
        let result = unsafe { grow_inner(tables, frames, offset, pages, mode) };
        match result {
            Ok(grown) => {
                outcome = Ensure::Grew;
                #[cfg(feature = "test-trace")]
                // A phase=auto record, from its own format string (0059-D4);
                // default kernels grow silently.
                crate::serial::println!(
                    "MIRROR_HEAP_GROWN phase=auto size={} used={} free={} max={} added_pages={}",
                    grown.size,
                    grown.used,
                    grown.free,
                    HEAP_MAX_SIZE,
                    pages
                );
                #[cfg(not(feature = "test-trace"))]
                let _ = grown;
            }
            Err(error) => {
                if injected {
                    // Injected failures neither print nor arm the back-off;
                    // the attempt still stops this safe point (0059-D1).
                    return if outcome == Ensure::Grew {
                        outcome
                    } else {
                        Ensure::Deferred
                    };
                }
                let (key, _) = pending_shortfall();
                if key != 0 {
                    FAILED_SHORTFALL.fetch_max(key, Ordering::Relaxed);
                }
                match error {
                    HeapError::LimitExceeded => {
                        BACKOFF_MODE.store(BACKOFF_NEVER, Ordering::Relaxed);
                        return Ensure::AtCap;
                    }
                    HeapError::OutOfFrames => {
                        BACKOFF_FRAMES.store(
                            frames
                                .stats()
                                .map(|stats| stats.allocated_frames)
                                .unwrap_or(0),
                            Ordering::Relaxed,
                        );
                        BACKOFF_MODE.store(BACKOFF_WAIT_FRAMES, Ordering::Relaxed);
                        return if outcome == Ensure::Grew {
                            outcome
                        } else {
                            Ensure::Deferred
                        };
                    }
                    // The transport and the injection/bug classes never re-arm.
                    _ => {
                        BACKOFF_MODE.store(BACKOFF_NEVER, Ordering::Relaxed);
                        return if outcome == Ensure::Grew {
                            outcome
                        } else {
                            Ensure::Deferred
                        };
                    }
                }
            }
        }
    }
}

// ---- Slab caches (0059-D2) ----

/// Size classes 16..=2048 bytes by powers of two; one 4 KiB arena page per
/// slab page; a layout routes to a slab only when max(size, align) rounded up
/// to a power of two lands in that range. Everything else keeps first-fit.
const SLAB_CLASS_COUNT: usize = 8;
const SLAB_MIN_SHIFT: u32 = 4; // 16 bytes
const SLAB_PAGE_LIMIT: usize = 64; // at most 64 live slab pages (256 KiB)
/// Bits per slab page's object bitmap: enough for the densest class (class 16
/// keeps (4096 - 64) / 16 = 252 objects).
const SLAB_BITMAP_WORDS: usize = 4;

/// One slab page's header, at the page's base. Objects live after it, starting
/// at the next class-size boundary.
#[repr(C)]
struct SlabPage {
    class: u8,
    used: u16,
    /// Bit i is 1 while object i is free; bits at or past the page's capacity
    /// are always 0.
    bitmap: [u64; SLAB_BITMAP_WORDS],
}

/// The live slab pages, by arena virtual address (0 = an empty slot). Guarded
/// by the heap lock, like the free list.
struct SlabState {
    pages: [u64; SLAB_PAGE_LIMIT],
}

/// The size class serving a layout (0059-D2), or None to keep first-fit.
fn slab_class(layout: Layout) -> Option<usize> {
    let class = layout.size().max(layout.align()).next_power_of_two();
    if !(SLAB_MIN_SHIFT..SLAB_MIN_SHIFT + SLAB_CLASS_COUNT as u32).contains(&class.trailing_zeros())
    {
        return None;
    }
    Some((class.trailing_zeros() - SLAB_MIN_SHIFT) as usize)
}

/// The class's object size in bytes.
fn slab_class_size(class: usize) -> usize {
    1 << (SLAB_MIN_SHIFT + class as u32)
}

/// The class's object-area offset: the header rounded up to the class size, so
/// every object is class-size aligned (0059-D2).
fn slab_object_area(class: usize) -> usize {
    size_of::<SlabPage>().next_multiple_of(slab_class_size(class))
}

/// The class's objects per page.
fn slab_capacity(class: usize) -> usize {
    (PAGE_SIZE as usize - slab_object_area(class)) / slab_class_size(class)
}

/// Allocate one object of the class from a live page or a fresh page (0059-D2).
/// Returns null when no live page has a free object and no fresh page is
/// available (or the page table is full); the caller then falls back to
/// first-fit, which records the shortfall on its own miss.
///
/// The heap lock is held throughout, so the page table and the pages are
/// exclusively owned here.
fn slab_alloc(heap: &mut Heap, slabs: &UnsafeCell<SlabState>, layout: Layout) -> *mut u8 {
    let Some(class) = slab_class(layout) else {
        return core::ptr::null_mut();
    };
    // SAFETY: the heap lock is held by the caller, so the slab state is
    // exclusively owned here.
    let state = unsafe { &mut *slabs.get() };
    for &page in state.pages.iter().filter(|page| **page != 0) {
        // SAFETY: a live slab page is owned by the slab layer, mapped in the
        // arena for the kernel's lifetime, and exclusively owned under the lock.
        let header = unsafe { &mut *(page as *mut SlabPage) };
        if header.class as usize != class {
            continue;
        }
        let capacity = slab_capacity(class);
        for (word, bits) in header.bitmap.iter_mut().enumerate() {
            if *bits == 0 {
                continue;
            }
            let bit = bits.trailing_zeros() as usize;
            let index = word * 64 + bit;
            if index >= capacity {
                continue; // the bits past the page's capacity are never set
            }
            *bits &= !(1 << bit);
            header.used += 1;
            let offset = slab_object_area(class) + index * slab_class_size(class);
            return (page as usize + offset) as *mut u8;
        }
    }
    // No live page serves it: refill from the free list.
    let page_layout =
        Layout::from_size_align(PAGE_SIZE as usize, PAGE_SIZE as usize).expect("page layout");
    let Ok(page) = heap.allocate_first_fit(page_layout) else {
        return core::ptr::null_mut();
    };
    let Some(slot) = state.pages.iter_mut().find(|slot| **slot == 0) else {
        // The page table is full: release the page and decline to first-fit.
        // SAFETY: the refill's own fresh allocation, released with its layout.
        unsafe { heap.deallocate(page, page_layout) };
        return core::ptr::null_mut();
    };
    let page_va = page.as_ptr() as u64;
    *slot = page_va;
    let capacity = slab_capacity(class);
    assert!(
        capacity <= SLAB_BITMAP_WORDS * 64,
        "slab capacity over bitmap"
    );
    let mut bitmap = [0u64; SLAB_BITMAP_WORDS];
    for index in 0..capacity {
        bitmap[index / 64] |= 1 << (index % 64);
    }
    // SAFETY: the fresh page is exclusively owned by the slab layer here, and
    // the header write covers exactly the page's header.
    unsafe {
        (page_va as *mut SlabPage).write(SlabPage {
            class: class as u8,
            used: 1,
            bitmap,
        });
    }
    // The first object of the new page is object 0.
    // SAFETY: the header is initialized above and exclusively owned.
    unsafe { (*(page_va as *mut SlabPage)).bitmap[0] &= !1 };
    (page_va as usize + slab_object_area(class)) as *mut u8
}

/// Whether the pointer's page is a live slab page (0059-D2).
fn slab_tracked(slabs: &UnsafeCell<SlabState>, pointer: *mut u8) -> bool {
    let page = pointer as u64 & !(PAGE_SIZE - 1);
    // SAFETY: a read-only membership check under the heap lock.
    let state = unsafe { &*slabs.get() };
    state.pages.contains(&page)
}

/// Free one slab object (0059-D2); its empty page returns to the free list.
fn slab_free(heap: &mut Heap, slabs: &UnsafeCell<SlabState>, pointer: *mut u8, layout: Layout) {
    let class = slab_class(layout).expect("slab free of a non-slab layout");
    let page = pointer as u64 & !(PAGE_SIZE - 1);
    // SAFETY: the heap lock is held by the caller.
    let state = unsafe { &mut *slabs.get() };
    let Some(slot) = state.pages.iter_mut().find(|slot| **slot == page) else {
        panic!("slab free of an untracked page");
    };
    // SAFETY: the table tracks this page as a live slab page.
    let header = unsafe { &mut *(page as *mut SlabPage) };
    assert_eq!(
        header.class as usize, class,
        "slab free with a mismatched class"
    );
    let index =
        (pointer as usize - page as usize - slab_object_area(class)) / slab_class_size(class);
    let (word, bit) = (index / 64, index % 64);
    assert!(header.bitmap[word] & (1 << bit) == 0, "slab double free");
    header.bitmap[word] |= 1 << bit;
    header.used -= 1;
    if header.used == 0 {
        // The empty page returns to the free list, so coalescing is unchanged.
        *slot = 0;
        let page_layout =
            Layout::from_size_align(PAGE_SIZE as usize, PAGE_SIZE as usize).expect("page layout");
        // SAFETY: the page is no longer referenced by the slab layer, and it is
        // released with its exact refill layout.
        unsafe { heap.deallocate(NonNull::new_unchecked(page as *mut u8), page_layout) };
    }
}

/// Grow once by MAX_GROW_PAGES, retaining a live allocation and an older user root.
///
/// # Safety
/// Inherits grow's BSP/root/allocator/HHDM contract. Run after SMP startup and
/// before deliberate AP failures, with enough free heap for a >old-capacity
/// allocation after growth. No unrelated task may mutate heap state during tests.
#[cfg(feature = "boot-smoke")]
pub unsafe fn growth_self_test(
    tables: &PageTables,
    frames: &mut FrameAllocator<'_>,
    offset: u64,
    phase: &str,
) -> HeapStats {
    use crate::paging::UserAddressSpace;

    let before = stats();
    let frame_baseline = frames
        .stats()
        .expect("heap growth frame accounting")
        .allocated_frames;
    let old_end = HEAP_START + before.size as u64;
    assert!(before.size + MAX_GROW_PAGES * PAGE_SIZE as usize <= HEAP_MAX_SIZE);
    assert_heap_guards(tables, before.size);

    assert_eq!(growth_bytes(0, 1), Err(HeapError::NotInitialized));
    assert_eq!(
        growth_bytes(HEAP_MAX_SIZE, 1),
        Err(HeapError::LimitExceeded)
    );
    assert_eq!(
        growth_bytes(HEAP_MAX_SIZE - PAGE_SIZE as usize, 1),
        Ok(PAGE_SIZE as usize)
    );
    for pages in [0, MAX_GROW_PAGES + 1, usize::MAX] {
        assert_eq!(
            // SAFETY: Bad sizes reject before frame or mapping changes.
            unsafe { grow(tables, frames, offset, pages) },
            Err(HeapError::InvalidGrowth)
        );
    }
    let guard = GrowthGuard::acquire().expect("heap growth guard test");
    assert_eq!(
        // SAFETY: Reentry fails before touching the allocator or tables.
        unsafe { grow(tables, frames, offset, 1) },
        Err(HeapError::Busy)
    );
    drop(guard);
    assert_eq!(stats(), before);
    assert_eq!(frames.stats().unwrap().allocated_frames, frame_baseline);

    let live_layout = Layout::from_size_align(257, 64).unwrap();
    // SAFETY: Valid nonzero layout. Check ownership before initializing bytes.
    let live = unsafe { ALLOCATOR.alloc(live_layout) };
    assert!(!live.is_null(), "live growth canary allocation");
    // SAFETY: The successful allocation owns every byte being initialized.
    unsafe { live.write_bytes(0xa5, live_layout.size()) };
    let with_live = stats();
    // SAFETY: BSP/IF0 and active kernel root. Prepared heap parents precede this
    // borrowed root, which remains inactive during every mapping operation.
    let mut borrowed =
        unsafe { UserAddressSpace::new(tables, frames) }.expect("heap growth borrowed root");
    let frame_with_root = frames.stats().unwrap().allocated_frames;
    assert_eq!(frame_with_root, frame_baseline + 1);

    for limit in [0, 3] {
        assert_eq!(
            // SAFETY: Injection leaves backing unexposed and recycles every
            // acquired frame before returning OutOfFrames.
            unsafe {
                grow_inner(
                    tables,
                    frames,
                    offset,
                    8,
                    GrowthMode::FailFramesAfter(limit),
                )
            },
            Err(HeapError::OutOfFrames)
        );
        assert_eq!(stats(), with_live);
        assert_eq!(frames.stats().unwrap().allocated_frames, frame_with_root);
        for page in 0..8 {
            assert_eq!(tables.translate(old_end + page * PAGE_SIZE), Ok(None));
        }
    }
    assert_eq!(
        // SAFETY: The private tail is read by each CPU, then withdrawn with
        // exact acknowledgements before its frames are recycled.
        unsafe { grow_inner(tables, frames, offset, 8, GrowthMode::CancelBeforeCommit) },
        Err(HeapError::Cancelled)
    );
    assert_eq!(stats(), with_live);
    assert_eq!(frames.stats().unwrap().allocated_frames, frame_with_root);
    for page in 0..8 {
        assert_eq!(tables.translate(old_end + page * PAGE_SIZE), Ok(None));
    }

    // SAFETY: The permanent growth has the same exclusive tail/allocator
    // ownership; the live canary and borrowed upper hierarchy remain valid.
    let grown =
        unsafe { grow(tables, frames, offset, MAX_GROW_PAGES) }.expect("heap growth commit");
    let added = MAX_GROW_PAGES * PAGE_SIZE as usize;
    assert_eq!(grown.size, before.size + added);
    assert_eq!(grown.used, with_live.used);
    assert_eq!(grown.free, with_live.free + added);
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        frame_with_root + MAX_GROW_PAGES
    );
    assert_heap_guards(tables, grown.size);
    for index in 0..live_layout.size() {
        // SAFETY: This allocation remained live and unchanged through growth.
        assert_eq!(unsafe { live.add(index).read() }, 0xa5);
    }

    // Larger than the entire prior heap: success requires free space to
    // coalesce across the former boundary, and its last page is new backing.
    let large_layout =
        Layout::from_size_align(before.size + PAGE_SIZE as usize, PAGE_SIZE as usize).unwrap();
    // SAFETY: This bounded valid layout is within the newly committed capacity.
    let large = unsafe { ALLOCATOR.alloc(large_layout) };
    assert!(!large.is_null(), "grown heap large allocation");
    assert_eq!(large as usize % large_layout.align(), 0);
    // SAFETY: Unique allocation, no other CPU has access while initializing it.
    unsafe { large.write_bytes(0x6b, large_layout.size()) };
    let probe = large as u64 + (large_layout.size() - PAGE_SIZE as usize) as u64;
    assert!(probe >= old_end && probe + PAGE_SIZE <= HEAP_START + grown.size as u64);
    let expected = u64::from_ne_bytes([0x6b; 8]);
    // SAFETY: This owned allocation stays immutable until every read ACK; no
    // Rust reference or allocator lock spans the remote read operation.
    assert!(unsafe { crate::smp::probe_shared_page(probe, expected) } > 0);
    // SAFETY: The pre-existing root shares the pinned kernel hierarchy. There
    // is no ring3 entry; restore before assertions, allocation, or table changes.
    let observed = unsafe {
        borrowed.activate().expect("activate older heap borrower");
        let value = (probe as *const u64).read_volatile();
        tables.activate().expect("restore kernel after heap probe");
        value
    };
    assert_eq!(observed, expected, "new heap leaf absent in older root");
    for index in 0..live_layout.size() {
        // SAFETY: The disjoint canary remained allocated while the large block
        // was filled and read on every CPU and through the older user root.
        assert_eq!(unsafe { live.add(index).read() }, 0xa5);
    }
    // SAFETY: All remote reads completed. Each allocation is freed once with
    // its exact layout; reacquisition verifies the merged free region is reused.
    unsafe { ALLOCATOR.dealloc(large, large_layout) };
    // SAFETY: Reuse the same bounded valid layout after the prior release.
    let reused = unsafe { ALLOCATOR.alloc(large_layout) };
    assert_eq!(reused, large, "grown heap coalescing reuse");
    // SAFETY: Both allocations remain owned with the indicated layouts; all
    // reads ended and the borrowed root is inactive and exclusively owned.
    unsafe {
        ALLOCATOR.dealloc(reused, large_layout);
        ALLOCATOR.dealloc(live, live_layout);
        borrowed.destroy(frames).expect("release heap test root");
    }
    let after = stats();
    assert_eq!(after.size, before.size + added);
    assert_eq!(after.used, before.used);
    assert_eq!(after.free, before.free + added);
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        frame_baseline + MAX_GROW_PAGES
    );
    assert_heap_guards(tables, after.size);
    assert!(!arch::interrupts_enabled());
    crate::serial::println!(
        "MIRROR_HEAP_GROW_TESTS_OK phase={} acquisition_rollback=1 mapped_rollback=1 live=1 large=1 guards=1 borrowed_root=1",
        phase
    );
    after
}

#[cfg(feature = "boot-smoke")]
fn assert_heap_guards(tables: &PageTables, size: usize) {
    for address in [
        HEAP_START - PAGE_SIZE,
        HEAP_START + size as u64,
        HEAP_START + HEAP_MAX_SIZE as u64,
    ] {
        assert_eq!(tables.translate(address), Ok(None), "heap guard mapped");
    }
}

/// Exercise every reserved PT and the real maximum without releasing backing.
///
/// # Safety
/// Inherits grow's ownership and execution contract. Run after native clients
/// have released every heap allocation, before deliberately failing an AP.
#[cfg(feature = "boot-smoke")]
pub unsafe fn capacity_self_test(
    tables: &PageTables,
    frames: &mut FrameAllocator<'_>,
    offset: u64,
) {
    let start = stats();
    assert_eq!(start.used, 0, "heap limit test requires an empty heap");
    assert!(start.size > 0 && start.size <= HEAP_MAX_SIZE);
    let frame_start = frames
        .stats()
        .expect("heap limit frame accounting")
        .allocated_frames;

    // 0059 test 2, part 1 (the test-only safe point): an allocation one page
    // over the heap's size misses (alloc never grows), records the shortfall,
    // and this direct ensure grows exactly one step so the retry succeeds.
    // Frames rise by exactly the grown pages and nothing else (heap.rs:607-610's
    // pattern).
    let request =
        Layout::from_size_align(start.size + PAGE_SIZE as usize, PAGE_SIZE as usize).unwrap();
    // SAFETY: a valid bounded layout, one page over the heap's size.
    let block = unsafe { ALLOCATOR.alloc(request) };
    assert!(
        block.is_null(),
        "an allocation over the heap's size must miss"
    );
    assert_eq!(ensure(tables, frames, offset), Ensure::Grew);
    // SAFETY: the shortfall was feasible under the cap, and one step suffices.
    let block = unsafe { ALLOCATOR.alloc(request) };
    assert!(
        !block.is_null(),
        "the shortfall must be served after one step"
    );
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        frame_start + MAX_GROW_PAGES
    );
    assert_eq!(
        stats().size,
        start.size + MAX_GROW_PAGES * PAGE_SIZE as usize
    );
    // SAFETY: free the block once with its exact layout.
    unsafe { ALLOCATOR.dealloc(block, request) };

    // 0059 test 5: with the largest block held below the mark, the injected
    // FailFramesAfter and CancelBeforeCommit modes reach ensure through the
    // static; each stops the safe point with stats and frames unchanged and
    // the tail unmapped, neither prints nor arms the back-off, and the counter
    // asserts both attempts ran (a skipped attempt fails).
    let hold_size = stats().free - 128 * 1024;
    let hold_layout = Layout::from_size_align(hold_size, PAGE_SIZE as usize).unwrap();
    // SAFETY: a valid bounded layout sized to the free heap.
    let hold = unsafe { ALLOCATOR.alloc(hold_layout) };
    assert!(!hold.is_null(), "the injection workload must fit");
    let before_inject = stats();
    let frames_before_inject = frames.stats().unwrap().allocated_frames;
    arm_ensure_inject(2); // fail after 0 frames
    assert_eq!(ensure(tables, frames, offset), Ensure::Deferred);
    arm_ensure_inject(1); // cancel before commit
    assert_eq!(ensure(tables, frames, offset), Ensure::Deferred);
    assert_eq!(ensure_injected(), 2, "an injected attempt was skipped");
    assert_eq!(stats(), before_inject);
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        frames_before_inject
    );
    assert_heap_guards(tables, before_inject.size);
    // SAFETY: free the workload once with its exact layout.
    unsafe { ALLOCATOR.dealloc(hold, hold_layout) };

    let before = stats();
    let frame_baseline = frames.stats().unwrap().allocated_frames;
    let mut current = before;
    while current.size < HEAP_MAX_SIZE {
        let pages = ((HEAP_MAX_SIZE - current.size) / PAGE_SIZE as usize).min(MAX_GROW_PAGES);
        // SAFETY: The next uncommitted tail is owned and absent. Every batch
        // completes all shootdowns before permanent capacity is exposed.
        current = unsafe { grow(tables, frames, offset, pages) }.expect("grow heap to capacity");
        assert_eq!(current.used, 0);
        assert_eq!(current.free, current.size);
        assert_eq!(
            frames.stats().unwrap().allocated_frames,
            frame_baseline + (current.size - before.size) / PAGE_SIZE as usize
        );
        assert_heap_guards(tables, current.size);
    }
    let full = stats();
    assert_eq!(full.size, HEAP_MAX_SIZE);
    let full_frames = frames.stats().unwrap().allocated_frames;
    assert_eq!(
        // SAFETY: Capacity rejection precedes backing or mapping changes,
        // leaving both permanent guards absent.
        unsafe { grow(tables, frames, offset, 1) },
        Err(HeapError::LimitExceeded)
    );
    assert_eq!(stats(), full);
    assert_eq!(frames.stats().unwrap().allocated_frames, full_frames);
    assert_heap_guards(tables, HEAP_MAX_SIZE);

    // 0059 test 2's record (0059-D4): at the cap with the largest free block
    // held below the mark, this test-only safe point reports AtCap, and this
    // test prints the one accepted SKIPPED record per boot-smoke boot.
    let hold_size = HEAP_MAX_SIZE - 128 * 1024;
    let hold_layout = Layout::from_size_align(hold_size, PAGE_SIZE as usize).unwrap();
    // SAFETY: a valid bounded layout sized to the full heap.
    let hold = unsafe { ALLOCATOR.alloc(hold_layout) };
    assert!(!hold.is_null(), "the near-full hold must fit at the cap");
    assert_eq!(ensure(tables, frames, offset), Ensure::AtCap);
    crate::serial::println!("MIRROR_HEAP_GROW_SKIPPED phase=auto reason=LimitExceeded");
    assert_eq!(stats().size, HEAP_MAX_SIZE);
    assert_eq!(frames.stats().unwrap().allocated_frames, full_frames);
    // SAFETY: free the hold once with its exact layout.
    unsafe { ALLOCATOR.dealloc(hold, hold_layout) };

    let layout = Layout::from_size_align(HEAP_MAX_SIZE, PAGE_SIZE as usize).unwrap();
    // SAFETY: Valid bounded allocation of the complete empty heap. This can
    // succeed only if every committed tail coalesced with previous free space.
    let allocation = unsafe { ALLOCATOR.alloc(layout) };
    assert_eq!(allocation as u64, HEAP_START, "maximum heap coalescing");
    assert_eq!(stats().free, 0);
    let pages = HEAP_MAX_SIZE / PAGE_SIZE as usize;
    for page in 0..pages {
        let pattern = 0x4d49_5252_4845_4150_u64 ^ page as u64;
        let byte = page * PAGE_SIZE as usize;
        // SAFETY: The complete arena is one exclusively owned allocation.
        // Distinct first/last words prove every mapped page is writable and
        // detect accidental physical aliases when read after the full pass.
        unsafe {
            allocation.add(byte).cast::<u64>().write_volatile(pattern);
            allocation
                .add(byte + PAGE_SIZE as usize - core::mem::size_of::<u64>())
                .cast::<u64>()
                .write_volatile(!pattern);
        }
    }
    for page in (0..pages).rev() {
        let pattern = 0x4d49_5252_4845_4150_u64 ^ page as u64;
        let byte = page * PAGE_SIZE as usize;
        // SAFETY: The initialized allocation remains owned and unmodified.
        unsafe {
            assert_eq!(allocation.add(byte).cast::<u64>().read_volatile(), pattern);
            assert_eq!(
                allocation
                    .add(byte + PAGE_SIZE as usize - core::mem::size_of::<u64>())
                    .cast::<u64>()
                    .read_volatile(),
                !pattern
            );
        }
    }
    // SAFETY: Free the sole allocation once with its exact original layout.
    // The backing frames remain permanent heap capacity.
    unsafe { ALLOCATOR.dealloc(allocation, layout) };
    assert_eq!(stats(), full);
    assert_eq!(frames.stats().unwrap().allocated_frames, full_frames);
    assert_heap_guards(tables, HEAP_MAX_SIZE);
    crate::serial::println!(
        "MIRROR_HEAP_LIMIT_OK size={} limit_rejected=1 guards=1 reclaimed_allocations=1",
        HEAP_MAX_SIZE
    );
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

    // 0059-D2: slabs. Cycle every size class: allocate one page's worth of
    // objects plus one (forcing a refill), pattern-check, and free. Every
    // emptied page returns to the free list, proven by the stats restoration
    // here and the whole-heap reuse below. The large-alignment case bypasses
    // the slabs entirely.
    assert!(slab_class(Layout::from_size_align(71, 32768).unwrap()).is_none());
    let slab_baseline = stats();
    let mut pointers = [core::ptr::null_mut(); 257];
    for class in 0..SLAB_CLASS_COUNT {
        let size = slab_class_size(class);
        let layout = Layout::from_size_align(size, size).unwrap();
        let count = slab_capacity(class) + 1;
        assert!(count <= 257, "slab test count over the pointer array");
        for slot in pointers.iter_mut().take(count) {
            // SAFETY: a valid nonzero layout; null is checked before use.
            *slot = unsafe { ALLOCATOR.alloc(layout) };
            assert!(!slot.is_null(), "slab allocation failed");
            // SAFETY: the fresh object owns `size` writable bytes.
            unsafe { slot.write_bytes(0x60 + class as u8, size) };
        }
        for slot in pointers.iter().take(count) {
            // SAFETY: each object remains live; its first and last bytes were
            // written and still hold the class's pattern.
            unsafe {
                assert_eq!(slot.read(), 0x60 + class as u8);
                assert_eq!(slot.add(size - 1).read(), 0x60 + class as u8);
            }
        }
        for slot in pointers.iter_mut().take(count) {
            // SAFETY: each object is freed exactly once with its original
            // layout.
            unsafe { ALLOCATOR.dealloc(*slot, layout) };
            *slot = core::ptr::null_mut();
        }
        assert_eq!(stats(), slab_baseline, "slab cycle leaked a page");
    }

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
    // The deliberate failures above recorded shortfalls (0059-D1); assert the
    // recording worked, then clear it so no safe point grows for a battery
    // artifact.
    assert_ne!(
        pending_shortfall().0,
        0,
        "the battery recorded no shortfall"
    );
    clear_shortfall();
    crate::serial::println!(
        "MIRROR_HEAP_OK size={} free={}",
        baseline.size,
        baseline.free
    );
}
