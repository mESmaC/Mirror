//! The post-seal kernel VA window (spec 0050-D3).
//!
//! 32 MiB at 0xffff_d400_0000_0000, PML4 slot 424. Its whole table hierarchy
//! (a PDPT, a PD and 16 leaf PTs, 18 frames) is pinned before
//! `share_with_secondaries` seals kernel mappings, so the window parcels out
//! guarded VA ranges at run time. A static page bitmap (8192 pages, 1 KiB of
//! .bss, never the heap) records reserved pages. Every reservation owns one
//! unmapped guard page on each side, 0016a's layout rule, so neighbours are
//! two pages apart.
//!
//! Thread kernel stacks are the first client: 64 KiB each, 72 KiB of VA with
//! their guards, so the window holds 455 of them. A freed stack stays mapped
//! in a reuse pool and allocation takes from the pool first, so a free never
//! needs a shootdown. Absent-to-present maps are local in every mode, since no
//! CPU caches a not-present translation. Only a trim unmaps, in batches of at
//! most 256 pages, and only while every CPU can acknowledge the shootdown
//! (`smp::prepare_kernel_update`); otherwise the pool simply stays mapped and
//! nothing is quarantined. After publication a failed shootdown halts.
//!
//! Every operation runs on the BSP with IF=0 under a reentry guard. IRQ, NMI
//! and AP paths never touch the window state.

#![cfg_attr(
    not(feature = "boot-smoke"),
    expect(
        dead_code,
        reason = "the per-thread stacks of spec 0050 part 2 are the first client outside the self-test"
    )
)]

use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    arch,
    memory::{FrameAllocator, PAGE_SIZE},
    paging::{PageTables, PagingError},
    serial, smp, stacks,
};

pub const BASE: u64 = 0xffff_d400_0000_0000;
pub const SIZE: usize = 32 * 1024 * 1024;
pub const PAGES: usize = SIZE / PAGE_SIZE as usize;
const WORDS: usize = PAGES / u64::BITS as usize;
/// A thread kernel stack: 64 KiB, the proven RSP0 depth (0050-D2).
pub const STACK_PAGES: usize = 16;
/// A stack's reservation: its guard page below, the stack and its guard above.
const STACK_SPAN_PAGES: usize = STACK_PAGES + 2;
/// The stacks the window holds when nothing else uses it.
pub const STACK_SPANS: usize = PAGES / STACK_SPAN_PAGES;
/// The pinned hierarchy: a PDPT, a PD and one leaf PT per 2 MiB.
const PINNED_TABLES: usize = crate::paging::KVA_TABLES + 2;
/// The pages one trim batch covers: one shootdown command.
const MAX_TRIM_PAGES: usize = smp::MAX_INVALIDATE_PAGES as usize;
/// The stacks one batch holds when they sit back to back.
const STACKS_PER_BATCH: usize = (MAX_TRIM_PAGES - STACK_PAGES) / STACK_SPAN_PAGES + 1;

const fn pml4_slot(address: u64) -> u64 {
    (address >> 39) & 0x1ff
}

const _: () = {
    assert!(PAGES == 8192 && WORDS * u64::BITS as usize == PAGES);
    assert!(STACK_SPANS == 455 && STACK_SPANS <= u16::MAX as usize);
    assert!(stacks::span(STACK_PAGES) == STACK_SPAN_PAGES as u64 * PAGE_SIZE);
    assert!(PINNED_TABLES == 18 && STACKS_PER_BATCH == 14);
    // One PD under one PDPT maps the whole window: the pinned hierarchy.
    assert!(BASE.is_multiple_of(1 << 30) && SIZE <= 1 << 30);
    let slot = pml4_slot(BASE);
    assert!(slot == 424 && pml4_slot(BASE + SIZE as u64 - 1) == slot);
    // The PML4 slots in use: 256 the HHDM (Limine's 4-level base; preparation
    // also checks at run time that every window page is absent), 416 the heap
    // with its upper guard, 418 the stack arena, 420 the shared window, 422 the
    // device and firmware windows and 511 the kernel image.
    let used = [
        256,
        pml4_slot(crate::heap::HEAP_START),
        pml4_slot(crate::heap::HEAP_START + crate::heap::HEAP_MAX_SIZE as u64),
        pml4_slot(stacks::ARENA),
        pml4_slot(crate::paging::SHARED_BASE),
        pml4_slot(crate::apic::DEVICE_WINDOW),
        511,
    ];
    let mut index = 0;
    while index < used.len() {
        assert!(used[index] != slot);
        index += 1;
    }
};
// Slot 384, the smoke test page.
#[cfg(feature = "boot-smoke")]
const _: () = assert!(pml4_slot(crate::paging::TEST_ADDRESS) != pml4_slot(BASE));

#[derive(Debug, PartialEq, Eq)]
pub enum KvaError {
    InvalidContext,
    Busy,
    NotReady,
    WindowFull,
    OutOfFrames,
    Paging(PagingError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KvaStats {
    /// Reservations, guards included, and the pages they hold, which the
    /// bitmap's own count confirms on every read.
    pub spans: usize,
    pub reserved_pages: usize,
    /// Allocated and pooled stacks; each stays mapped with its 16 frames.
    pub live: usize,
    pub pooled: usize,
}

impl KvaStats {
    /// The frames live and pooled stacks hold together.
    pub fn stack_frames(&self) -> usize {
        (self.live + self.pooled) * STACK_PAGES
    }
}

/// What one trim did: the stacks it unmapped and returned, the shootdown
/// batches it used, and whether it emptied the pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrimReport {
    pub stacks: usize,
    pub batches: usize,
    pub complete: bool,
}

/// A thread kernel stack: 16 mapped pages between two unmapped guard pages,
/// owned from `allocate_stack` until `free_stack`.
#[must_use = "a dropped stack leaks its span and frames"]
#[derive(Debug, PartialEq, Eq)]
pub struct KernelStack {
    first: u16,
}

impl KernelStack {
    pub fn bottom(&self) -> u64 {
        page_address(usize::from(self.first) + 1)
    }

    pub fn top(&self) -> u64 {
        self.bottom() + STACK_PAGES as u64 * PAGE_SIZE
    }

    pub fn guard_below(&self) -> u64 {
        page_address(usize::from(self.first))
    }

    pub fn guard_above(&self) -> u64 {
        self.top()
    }
}

const fn page_address(page: usize) -> u64 {
    BASE + page as u64 * PAGE_SIZE
}

struct Window {
    /// One bit per page, set while the page is reserved, guards included.
    used: [u64; WORDS],
    spans: usize,
    reserved_pages: usize,
    live: usize,
    /// The first pages of pooled stacks' reservations: still mapped, reused
    /// newest first.
    pool: [u16; STACK_SPANS],
    pooled: usize,
}

impl Window {
    const fn new() -> Self {
        Self {
            used: [0; WORDS],
            spans: 0,
            reserved_pages: 0,
            live: 0,
            pool: [0; STACK_SPANS],
            pooled: 0,
        }
    }

    fn is_used(&self, page: usize) -> bool {
        self.used[page / 64] & (1 << (page % 64)) != 0
    }

    fn mark(&mut self, first: usize, pages: usize, used: bool) {
        for page in first..first + pages {
            if used {
                self.used[page / 64] |= 1 << (page % 64);
            } else {
                self.used[page / 64] &= !(1 << (page % 64));
            }
        }
    }

    /// First fit: reserve `pages` with an unmapped guard page on each side and
    /// return the reservation's first page, its lower guard.
    fn reserve(&mut self, pages: usize) -> Option<usize> {
        let total = pages.checked_add(2)?;
        if pages == 0 || total > PAGES {
            return None;
        }
        let mut start = 0;
        while start + total <= PAGES {
            // Scanning down from the candidate's end skips past the highest
            // reserved page in it, so a full window costs one step per span.
            match (start..start + total)
                .rev()
                .find(|&page| self.is_used(page))
            {
                Some(page) => start = page + 1,
                None => {
                    self.mark(start, total, true);
                    self.spans += 1;
                    self.reserved_pages += total;
                    return Some(start);
                }
            }
        }
        None
    }

    fn release(&mut self, first: usize, pages: usize) {
        let total = pages + 2;
        assert!(
            first + total <= PAGES && (first..first + total).all(|page| self.is_used(page)),
            "kernel VA release of an unreserved range"
        );
        self.mark(first, total, false);
        self.spans -= 1;
        self.reserved_pages -= total;
    }

    fn stats(&self) -> KvaStats {
        let counted: usize = self
            .used
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum();
        assert_eq!(counted, self.reserved_pages, "kernel VA bitmap accounting");
        KvaStats {
            spans: self.spans,
            reserved_pages: self.reserved_pages,
            live: self.live,
            pooled: self.pooled,
        }
    }
}

struct WindowCell(UnsafeCell<Window>);

// SAFETY: WindowGuard is the only accessor. It requires the BSP with IF=0 and
// excludes reentry, and IRQ, NMI and AP paths never use the window.
unsafe impl Sync for WindowCell {}

static WINDOW: WindowCell = WindowCell(UnsafeCell::new(Window::new()));
static BUSY: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);
// Only the boot-smoke switch (0050 test 4) sets this.
static TRANSPORT_WITHHELD: AtomicBool = AtomicBool::new(false);

/// Shootdown waits briefly enable IRQs. Reject reentry instead of spinning;
/// handlers never take this guard.
struct WindowGuard(PhantomData<*mut ()>);

impl WindowGuard {
    fn acquire() -> Result<Self, KvaError> {
        if arch::interrupts_enabled() || smp::is_secondary() {
            return Err(KvaError::InvalidContext);
        }
        if !READY.load(Ordering::Acquire) {
            return Err(KvaError::NotReady);
        }
        BUSY.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| KvaError::Busy)?;
        Ok(Self(PhantomData))
    }

    fn window(&mut self) -> &mut Window {
        // SAFETY: This guard holds BUSY, so no other reference to the window
        // exists, and the returned borrow ends before the guard releases it.
        unsafe { &mut *WINDOW.0.get() }
    }
}

impl Drop for WindowGuard {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

/// Pin the window's tables before SMP startup seals kernel mappings, and
/// report the window once in every flavor (0050-D4).
///
/// # Safety
/// Run once on the BSP with IF=0 and this root active, with its unique frame
/// allocator, before AP release and before any retained userspace root
/// exists. Nothing else maps or uses the window.
pub unsafe fn init(tables: &mut PageTables, frames: &mut FrameAllocator<'_>) {
    assert!(!arch::interrupts_enabled());
    assert!(
        !READY.load(Ordering::Acquire),
        "kernel VA window prepared twice"
    );
    #[cfg(feature = "boot-smoke")]
    let before = allocated(frames);
    // SAFETY: Inherited from the caller; no AP or retained user root exists.
    unsafe { tables.prepare_kva_window(frames) }.expect("kernel VA window");
    #[cfg(feature = "boot-smoke")]
    {
        assert_eq!(allocated(frames), before + PINNED_TABLES);
        // The general mapping paths refuse the window before any table write
        // or frame allocation, so the unowned frame number is never mapped.
        // SAFETY: Each call is rejected before it changes anything.
        unsafe {
            for address in [BASE, BASE + SIZE as u64 - PAGE_SIZE] {
                assert_eq!(
                    tables.map_page(address, PAGE_SIZE, frames),
                    Err(PagingError::KvaWindowReserved)
                );
                assert_eq!(
                    tables.unmap_page(address),
                    Err(PagingError::KvaWindowReserved)
                );
            }
        }
        assert_eq!(allocated(frames), before + PINNED_TABLES);
    }
    READY.store(true, Ordering::Release);
    serial::println!("MIRROR_KVA_READY base={:#x} size={}", BASE, SIZE);
}

pub fn stats() -> Result<KvaStats, KvaError> {
    let mut guard = WindowGuard::acquire()?;
    Ok(guard.window().stats())
}

/// Take a stack from the reuse pool, newest first, or build one: reserve its
/// guarded span, acquire and zero all 16 frames, then map them locally. Every
/// failure rolls back before it returns: the frames go back to the allocator
/// and the span to the bitmap, and no PTE was written (0050-D2).
///
/// # Safety
/// Run on the BSP with IF=0 and the kernel root active. `frames` is the
/// unique allocator and `offset` its writable HHDM. Hold no allocator guard.
pub unsafe fn allocate_stack(
    tables: &PageTables,
    frames: &mut FrameAllocator<'_>,
    offset: u64,
) -> Result<KernelStack, KvaError> {
    // SAFETY: Inherited from the caller.
    unsafe { allocate(tables, frames, offset, Source::Allocator) }
}

/// Return a stack to the reuse pool. It keeps its mapping and frames, so a
/// free never needs a shootdown and never changes a translation (0050-D2).
///
/// # Safety
/// Run on the BSP with IF=0. Nothing uses the stack on any CPU any more: no
/// live frame, reference, TSS RSP0 or IST points into it until a later
/// allocation hands it out again.
pub unsafe fn free_stack(stack: KernelStack) {
    let mut guard = WindowGuard::acquire().expect("kernel VA window for a stack free");
    let window = guard.window();
    let first = usize::from(stack.first);
    assert!(
        window.live > 0
            && window.pooled < STACK_SPANS
            && first + STACK_SPAN_PAGES <= PAGES
            && (first..first + STACK_SPAN_PAGES).all(|page| window.is_used(page)),
        "free of an unknown kernel stack"
    );
    window.pool[window.pooled] = stack.first;
    window.pooled += 1;
    window.live -= 1;
}

/// Unmap every pooled stack and return its frames, in batches of at most 256
/// pages, each with one shootdown. A batch runs only while every CPU can
/// acknowledge it (`smp::prepare_kernel_update`). Otherwise the rest of the
/// pool stays mapped for reuse and the report says the trim is incomplete.
/// After publication a failed shootdown halts before any frame returns.
///
/// # Safety
/// Run on the BSP with IF=0 and the kernel root active, with the unique frame
/// allocator. No pooled stack is in use (free_stack's contract). Hold no lock
/// an IRQ handler needs: each shootdown waits with IF briefly enabled.
pub unsafe fn trim(
    tables: &PageTables,
    frames: &mut FrameAllocator<'_>,
) -> Result<TrimReport, KvaError> {
    let mut guard = WindowGuard::acquire()?;
    let window = guard.window();
    let mut report = TrimReport {
        stacks: 0,
        batches: 0,
        complete: true,
    };
    window.pool[..window.pooled].sort_unstable();
    while window.pooled > 0 {
        if TRANSPORT_WITHHELD.load(Ordering::Acquire) {
            report.complete = false;
            break;
        }
        // Batch from the top of the sorted pool, so trimmed stacks come off
        // its end. The cover runs from the lowest stack's bottom page to the
        // highest stack's top page.
        let end = window.pooled;
        let cover_end = usize::from(window.pool[end - 1]) + 1 + STACK_PAGES;
        let mut count = 1;
        while count < end
            && cover_end - (usize::from(window.pool[end - 1 - count]) + 1) <= MAX_TRIM_PAGES
        {
            count += 1;
        }
        let cover_start = usize::from(window.pool[end - count]) + 1;
        let mut leaves = [(0, 0); MAX_TRIM_PAGES];
        let mut used = 0;
        for &first in &window.pool[end - count..end] {
            let bottom = page_address(usize::from(first) + 1);
            for page in 0..STACK_PAGES {
                let address = bottom + page as u64 * PAGE_SIZE;
                let physical = tables
                    .translate(address)
                    .map_err(KvaError::Paging)?
                    .ok_or(KvaError::Paging(PagingError::NotMapped))?;
                leaves[used] = (address, physical);
                used += 1;
            }
        }
        // SAFETY: The window guard keeps these stacks pooled, and pooled
        // stacks are unused on every CPU. The caller holds no IRQ-needed lock.
        match unsafe {
            tables.unmap_kva_pages(
                page_address(cover_start),
                cover_end - cover_start,
                &leaves[..used],
            )
        } {
            Ok(()) => {}
            Err(PagingError::SharedUpdateUnavailable) => {
                report.complete = false;
                break;
            }
            Err(error) => return Err(KvaError::Paging(error)),
        }
        for &(_, physical) in &leaves[..used] {
            // SAFETY: Every CPU acknowledged the cover's invalidation, so only
            // the frame's dormant HHDM alias remains.
            unsafe { frames.deallocate(physical) }.expect("recycle a trimmed stack frame");
        }
        for index in end - count..end {
            let first = usize::from(window.pool[index]);
            window.release(first, STACK_PAGES);
        }
        window.pooled = end - count;
        report.stacks += count;
        report.batches += 1;
    }
    Ok(report)
}

/// Where a fresh stack's frames come from.
enum Source {
    Allocator,
    /// The allocator, failing at the Nth frame (0050 tests 4 and 5).
    #[cfg(feature = "boot-smoke")]
    FailAt(usize),
    /// Frames the caller owns, which stay the caller's if the build fails.
    #[cfg(feature = "boot-smoke")]
    Given([u64; STACK_PAGES]),
}

impl Source {
    fn frame_limit(&self) -> usize {
        #[cfg(feature = "boot-smoke")]
        if let Self::FailAt(limit) = self {
            return *limit;
        }
        usize::MAX
    }

    fn given(&self) -> Option<&[u64; STACK_PAGES]> {
        #[cfg(feature = "boot-smoke")]
        if let Self::Given(frames) = self {
            return Some(frames);
        }
        None
    }
}

/// allocate_stack with a choice of frame source. Only the allocator source
/// takes from the pool; the test sources always build a fresh stack.
///
/// # Safety
/// As for allocate_stack. Given frames are distinct, initialized, owned WB
/// RAM with no other mapping; a successful build takes them over.
unsafe fn allocate(
    tables: &PageTables,
    frames: &mut FrameAllocator<'_>,
    offset: u64,
    source: Source,
) -> Result<KernelStack, KvaError> {
    let mut guard = WindowGuard::acquire()?;
    let window = guard.window();
    if matches!(source, Source::Allocator) && window.pooled > 0 {
        window.pooled -= 1;
        window.live += 1;
        return Ok(KernelStack {
            first: window.pool[window.pooled],
        });
    }
    let first = window.reserve(STACK_PAGES).ok_or(KvaError::WindowFull)?;
    let stack = KernelStack {
        first: u16::try_from(first).expect("kernel VA page index"),
    };
    let mut physical = [0; STACK_PAGES];
    // SAFETY: Inherited from the caller; every frame stays private until
    // the map below, and a failure recycles the acquired prefix.
    if let Err(error) = unsafe { acquire_frames(frames, offset, &mut physical, &source) } {
        window.release(first, STACK_PAGES);
        return Err(error);
    }
    // SAFETY: The reserved span is absent and private, and the frames are
    // distinct, zeroed or given, and owned. Nothing uses the span yet.
    if let Err(error) = unsafe { tables.map_kva_pages(stack.bottom(), &physical) } {
        if source.given().is_none() {
            // SAFETY: A map error precedes every PTE write.
            unsafe { recycle_frames(frames, &physical) };
        }
        window.release(first, STACK_PAGES);
        return Err(KvaError::Paging(error));
    }
    window.live += 1;
    Ok(stack)
}

/// Acquire and zero a stack's frames before any PTE write, recycling the
/// acquired prefix on failure. Given frames are used as they are.
///
/// # Safety
/// `frames` is the unique allocator and `offset` its writable HHDM.
unsafe fn acquire_frames(
    frames: &mut FrameAllocator<'_>,
    offset: u64,
    physical: &mut [u64; STACK_PAGES],
    source: &Source,
) -> Result<(), KvaError> {
    if let Some(given) = source.given() {
        *physical = *given;
        return Ok(());
    }
    let limit = source.frame_limit();
    for index in 0..STACK_PAGES {
        let frame = if index < limit {
            frames.allocate()
        } else {
            None
        };
        let Some(frame) = frame else {
            // SAFETY: These frames have only dormant direct-map aliases.
            unsafe { recycle_frames(frames, &physical[..index]) };
            return Err(KvaError::OutOfFrames);
        };
        physical[index] = frame;
        let Some(address) = offset.checked_add(frame) else {
            // SAFETY: The whole prefix, this frame included, stays unmapped.
            unsafe { recycle_frames(frames, &physical[..=index]) };
            return Err(KvaError::Paging(PagingError::DirectMapOverflow));
        };
        // SAFETY: A fresh complete RAM frame with a writable HHDM alias and no
        // other alias or reference. Clear it before any page walk can reach it.
        unsafe { (address as *mut u8).write_bytes(0, PAGE_SIZE as usize) };
    }
    Ok(())
}

/// # Safety
/// The caller owns every frame once, and none is mapped except through its
/// dormant HHDM alias.
unsafe fn recycle_frames(frames: &mut FrameAllocator<'_>, physical: &[u64]) {
    for &frame in physical.iter().rev() {
        // SAFETY: Inherited from the caller.
        unsafe { frames.deallocate(frame) }.expect("recycle a kernel stack frame");
    }
}

fn allocated(frames: &FrameAllocator<'_>) -> usize {
    frames.stats().expect("frame accounting").allocated_frames
}

/// The boot-smoke switch (0050 test 4): while set, the window sees an
/// unavailable shootdown transport, as on a real degraded boot.
#[cfg(feature = "boot-smoke")]
pub fn withhold_transport_for_test(withheld: bool) {
    TRANSPORT_WITHHELD.store(withheld, Ordering::Release);
}

#[cfg(feature = "boot-smoke")]
const CANARY: u64 = 0x4d49_5252_4f52_4b56;

#[cfg(feature = "boot-smoke")]
fn canary(address: u64, salt: u64) -> u64 {
    CANARY ^ address ^ salt.rotate_left(32)
}

/// Write each page's first word.
///
/// # Safety
/// The stack is live, owned by the self-test and in use on no CPU.
#[cfg(feature = "boot-smoke")]
unsafe fn write_canaries(stack: &KernelStack, salt: u64) {
    for page in 0..STACK_PAGES {
        let address = stack.bottom() + page as u64 * PAGE_SIZE;
        // SAFETY: A mapped, writable page of the owned stack; no reference
        // to it exists.
        unsafe { (address as *mut u64).write_volatile(canary(address, salt)) };
    }
}

/// # Safety
/// As for write_canaries.
#[cfg(feature = "boot-smoke")]
unsafe fn check_canaries(stack: &KernelStack, salt: u64) {
    for page in 0..STACK_PAGES {
        let address = stack.bottom() + page as u64 * PAGE_SIZE;
        // SAFETY: A mapped, initialized page of the owned stack.
        let value = unsafe { (address as *const u64).read_volatile() };
        assert_eq!(value, canary(address, salt), "kernel stack backing aliased");
    }
}

/// Read a stack's lowest and highest pages on every CPU and return how many
/// CPUs read them.
///
/// # Safety
/// As for write_canaries, after write_canaries with this salt. Run on the BSP
/// with IF=0, holding no IRQ-needed lock or allocator guard.
#[cfg(feature = "boot-smoke")]
unsafe fn probe_stack(stack: &KernelStack, salt: u64) -> usize {
    let low = stack.bottom();
    let high = stack.top() - PAGE_SIZE;
    // SAFETY: Both pages stay mapped and unchanged until every read ends.
    let cpus = unsafe { smp::probe_shared_page(low, canary(low, salt)) };
    // SAFETY: As above.
    let high_cpus = unsafe { smp::probe_shared_page(high, canary(high, salt)) };
    assert_eq!(high_cpus, cpus);
    assert!(cpus > 0);
    cpus
}

#[cfg(feature = "boot-smoke")]
fn check_guards(tables: &PageTables, stack: &KernelStack) {
    assert_eq!(tables.translate(stack.guard_below()), Ok(None));
    assert_eq!(tables.translate(stack.guard_above()), Ok(None));
    for address in [stack.bottom(), stack.top() - PAGE_SIZE] {
        assert!(matches!(tables.translate(address), Ok(Some(_))));
    }
}

/// The window check of 0050 test 4 and D2's frame rule, from an empty
/// window: the reservations equal the live plus pooled stacks, every
/// reservation holds 18 pages, and the allocated frames are the baseline plus
/// 16 per stack plus the frames the test holds itself.
#[cfg(feature = "boot-smoke")]
fn check_window(frames: &FrameAllocator<'_>, baseline: usize, held: usize) -> KvaStats {
    let now = stats().expect("kernel VA stats");
    assert_eq!(
        now.spans,
        now.live + now.pooled,
        "kernel VA span accounting"
    );
    assert_eq!(now.reserved_pages, now.spans * STACK_SPAN_PAGES);
    assert_eq!(allocated(frames), baseline + now.stack_frames() + held);
    now
}

/// Spec 0050, acceptance test 4: allocate, map, probe on every CPU, free into
/// the pool, reuse and trim, with guard gaps intact and exact frame baselines;
/// fill all 455 spans and refuse the next stack; roll back injected frame
/// failures; and keep working with the transport withheld. Leaves the window
/// empty.
///
/// # Safety
/// Run on the BSP with IF=0 and the kernel root active, after SMP startup and
/// before any client uses the window, with the unique allocator and its
/// writable HHDM. Hold no IRQ-needed lock or allocator guard.
#[cfg(feature = "boot-smoke")]
pub unsafe fn self_test(tables: &PageTables, frames: &mut FrameAllocator<'_>, offset: u64) {
    let empty = KvaStats {
        spans: 0,
        reserved_pages: 0,
        live: 0,
        pooled: 0,
    };
    assert_eq!(stats(), Ok(empty));
    let baseline = allocated(frames);
    // SAFETY: Every call below honours the caller's BSP, IF=0, root, allocator
    // and HHDM contract. Each stack the test allocates is used only by the
    // test, through raw volatile accesses and completed probes, until freed.
    unsafe {
        let probe_frame = frames.allocate().expect("kernel VA probe frame");
        boundary_checks(tables, probe_frame);
        let cpus = pool_checks(tables, frames, offset, baseline, probe_frame);
        frames
            .deallocate(probe_frame)
            .expect("kernel VA probe frame reclamation");
        assert_eq!(check_window(frames, baseline, 0), empty);
        assert_eq!(
            stale_translation_check(tables, frames, offset, baseline),
            cpus
        );
        rollback_checks(tables, frames, offset, baseline);
        assert_eq!(degraded_checks(tables, frames, offset, baseline), cpus);
        let (filled, batches) = exhaustion_checks(tables, frames, offset, baseline, cpus);
        assert_eq!(stats(), Ok(empty));
        assert_eq!(allocated(frames), baseline);
        serial::println!(
            "MIRROR_KVA_TESTS_OK cpus={} spans={} trimmed={} batches={} reuse=1 rollback=1 degraded=1",
            cpus,
            filled,
            filled,
            batches
        );
    }
}

/// Paging-layer validation: every malformed request fails before a PTE write.
///
/// # Safety
/// As for self_test, with an empty window; `probe` is an owned frame.
#[cfg(feature = "boot-smoke")]
unsafe fn boundary_checks(tables: &PageTables, probe: u64) {
    let last = BASE + SIZE as u64 - PAGE_SIZE;
    // SAFETY: Each request is rejected before it changes a leaf.
    unsafe {
        for (address, run, error) in [
            (BASE - PAGE_SIZE, &[probe][..], PagingError::InvalidKvaRange),
            (last, &[probe, probe][..], PagingError::InvalidKvaRange),
            (BASE + 1, &[probe][..], PagingError::UnalignedAddress),
            (BASE, &[][..], PagingError::InvalidKvaRange),
            (BASE, &[probe + 1][..], PagingError::UnalignedAddress),
        ] {
            assert_eq!(tables.map_kva_pages(address, run), Err(error));
        }
        for (cover, pages, leaves, error) in [
            (BASE, 0, &[(BASE, probe)][..], PagingError::InvalidKvaRange),
            (
                BASE,
                MAX_TRIM_PAGES + 1,
                &[(BASE, probe)][..],
                PagingError::InvalidKvaRange,
            ),
            (BASE, 1, &[][..], PagingError::InvalidKvaRange),
            (
                BASE,
                1,
                &[(BASE + PAGE_SIZE, probe)][..],
                PagingError::InvalidKvaRange,
            ),
            (last, 2, &[(last, probe)][..], PagingError::InvalidKvaRange),
            (BASE, 1, &[(BASE, probe)][..], PagingError::NotMapped),
        ] {
            assert_eq!(tables.unmap_kva_pages(cover, pages, leaves), Err(error));
        }
    }
    for address in [BASE, last] {
        assert_eq!(tables.translate(address), Ok(None));
    }
}

/// Two fresh stacks, their layout and rejected corrupting requests, probes on
/// every CPU, then free into the pool and reuse, newest first.
///
/// # Safety
/// As for self_test, with an empty window; `probe` is an owned frame.
#[cfg(feature = "boot-smoke")]
unsafe fn pool_checks(
    tables: &PageTables,
    frames: &mut FrameAllocator<'_>,
    offset: u64,
    baseline: usize,
    probe: u64,
) -> usize {
    // SAFETY: Inherited from the caller; both stacks belong to this test.
    unsafe {
        let first = allocate_stack(tables, frames, offset).expect("first kernel stack");
        let second = allocate_stack(tables, frames, offset).expect("second kernel stack");
        assert_eq!(first.guard_below(), BASE);
        assert_eq!(second.bottom(), first.top() + 2 * PAGE_SIZE);
        check_guards(tables, &first);
        check_guards(tables, &second);
        let stats = check_window(frames, baseline, 1);
        assert_eq!((stats.live, stats.pooled), (2, 0));
        // Corrupting requests fail before any change and keep the mapping.
        let low = first.bottom();
        let frame = tables.translate(low).unwrap().unwrap();
        let next = tables.translate(low + PAGE_SIZE).unwrap().unwrap();
        assert_eq!(
            tables.map_kva_pages(low, &[probe]),
            Err(PagingError::AlreadyMapped)
        );
        assert_eq!(
            tables.unmap_kva_pages(low, 1, &[(low, probe)]),
            Err(PagingError::InvalidTable)
        );
        assert_eq!(
            tables.unmap_kva_pages(low, 2, &[(low + PAGE_SIZE, next), (low, frame)]),
            Err(PagingError::InvalidKvaRange)
        );
        assert_eq!(
            tables.unmap_kva_pages(first.guard_below(), 2, &[(first.guard_below(), probe)]),
            Err(PagingError::NotMapped)
        );
        assert_eq!(tables.translate(low), Ok(Some(frame)));
        assert_eq!(tables.translate(low + PAGE_SIZE), Ok(Some(next)));
        // Fresh frames are zeroed, and each CPU sees the local-only maps.
        assert_eq!((low as *const u64).read_volatile(), 0);
        write_canaries(&first, 1);
        write_canaries(&second, 1);
        check_canaries(&first, 1);
        let cpus = probe_stack(&first, 1);
        assert_eq!(probe_stack(&second, 1), cpus);

        let (first_bottom, second_bottom) = (first.bottom(), second.bottom());
        free_stack(first);
        free_stack(second);
        let stats = check_window(frames, baseline, 1);
        assert_eq!((stats.live, stats.pooled), (0, 2));
        assert_eq!(tables.translate(low), Ok(Some(frame)));
        // Reuse takes the newest pooled stack first and allocates no frame.
        let reused = allocate_stack(tables, frames, offset).expect("pooled kernel stack");
        let older = allocate_stack(tables, frames, offset).expect("older pooled kernel stack");
        assert_eq!(
            (reused.bottom(), older.bottom()),
            (second_bottom, first_bottom)
        );
        let stats = check_window(frames, baseline, 1);
        assert_eq!((stats.live, stats.pooled), (2, 0));
        check_canaries(&reused, 1);
        write_canaries(&reused, 2);
        write_canaries(&older, 2);
        assert_eq!(probe_stack(&reused, 2), cpus);
        assert_eq!(probe_stack(&older, 2), cpus);
        free_stack(reused);
        free_stack(older);
        let report = trim(tables, frames).expect("trim two pooled stacks");
        assert_eq!(
            report,
            TrimReport {
                stacks: 2,
                batches: 1,
                complete: true
            }
        );
        for address in [first_bottom, second_bottom] {
            assert_eq!(tables.translate(address), Ok(None));
        }
        cpus
    }
}

/// Every CPU read a stack, so its translation is warm everywhere; trim it,
/// rebuild the same span on frames that differ, and probe again. A CPU that
/// kept the old translation would read the old frame's canary, which the trim
/// left in place.
///
/// # Safety
/// As for self_test, with an empty window.
#[cfg(feature = "boot-smoke")]
unsafe fn stale_translation_check(
    tables: &PageTables,
    frames: &mut FrameAllocator<'_>,
    offset: u64,
    baseline: usize,
) -> usize {
    // SAFETY: Inherited from the caller; the stacks and frames are the test's.
    unsafe {
        let old = allocate_stack(tables, frames, offset).expect("kernel stack to trim");
        write_canaries(&old, 3);
        let cpus = probe_stack(&old, 3);
        // Taken while the old stack holds its frames, so none is reused.
        let mut given = [0; STACK_PAGES];
        for (page, frame) in given.iter_mut().enumerate() {
            *frame = frames.allocate().expect("replacement stack frame");
            let address = old.bottom() + page as u64 * PAGE_SIZE;
            ((offset + *frame) as *mut u8).write_bytes(0, PAGE_SIZE as usize);
            ((offset + *frame) as *mut u64).write_volatile(canary(address, 4));
        }
        let old_bottom = old.bottom();
        free_stack(old);
        assert_eq!(
            trim(tables, frames),
            Ok(TrimReport {
                stacks: 1,
                batches: 1,
                complete: true
            })
        );
        check_window(frames, baseline, STACK_PAGES);
        let rebuilt = allocate(tables, frames, offset, Source::Given(given))
            .expect("rebuild the trimmed span");
        assert_eq!(rebuilt.bottom(), old_bottom);
        for (page, &frame) in given.iter().enumerate() {
            let address = old_bottom + page as u64 * PAGE_SIZE;
            assert_eq!(tables.translate(address), Ok(Some(frame)));
        }
        check_window(frames, baseline, 0);
        assert_eq!(probe_stack(&rebuilt, 4), cpus);
        free_stack(rebuilt);
        // The trim returns the given frames, which the stack took over.
        assert_eq!(
            trim(tables, frames),
            Ok(TrimReport {
                stacks: 1,
                batches: 1,
                complete: true
            })
        );
        check_window(frames, baseline, 0);
        cpus
    }
}

/// A fresh build that fails at its Nth frame, or finds the window guard held,
/// restores the frame and window baselines exactly.
///
/// # Safety
/// As for self_test, with an empty window.
#[cfg(feature = "boot-smoke")]
unsafe fn rollback_checks(
    tables: &PageTables,
    frames: &mut FrameAllocator<'_>,
    offset: u64,
    baseline: usize,
) {
    let empty = stats().expect("kernel VA stats");
    for limit in [0, 1, STACK_PAGES / 2, STACK_PAGES - 1] {
        assert_eq!(
            // SAFETY: Inherited from the caller.
            unsafe { allocate(tables, frames, offset, Source::FailAt(limit)) },
            Err(KvaError::OutOfFrames)
        );
        assert_eq!(stats(), Ok(empty));
        assert_eq!(allocated(frames), baseline);
        assert_eq!(tables.translate(BASE + PAGE_SIZE), Ok(None));
    }
    let guard = WindowGuard::acquire().expect("kernel VA guard test");
    assert_eq!(
        // SAFETY: Reentry fails before touching frames or tables.
        unsafe { allocate_stack(tables, frames, offset) },
        Err(KvaError::Busy)
    );
    assert_eq!(
        // SAFETY: As above.
        unsafe { trim(tables, frames) },
        Err(KvaError::Busy)
    );
    assert_eq!(stats(), Err(KvaError::Busy));
    drop(guard);
    assert_eq!(stats(), Ok(empty));
    assert_eq!(allocated(frames), baseline);
}

/// With the transport withheld a trim is skipped and the pool stays mapped,
/// while reuse and fresh local maps keep working; restored, the trim empties
/// the pool.
///
/// # Safety
/// As for self_test, with an empty window.
#[cfg(feature = "boot-smoke")]
unsafe fn degraded_checks(
    tables: &PageTables,
    frames: &mut FrameAllocator<'_>,
    offset: u64,
    baseline: usize,
) -> usize {
    // SAFETY: Inherited from the caller; every stack belongs to this test.
    unsafe {
        let first = allocate_stack(tables, frames, offset).expect("degraded first stack");
        let second = allocate_stack(tables, frames, offset).expect("degraded second stack");
        let (first_bottom, second_bottom) = (first.bottom(), second.bottom());
        free_stack(first);
        free_stack(second);
        withhold_transport_for_test(true);
        let skipped = TrimReport {
            stacks: 0,
            batches: 0,
            complete: false,
        };
        assert_eq!(trim(tables, frames), Ok(skipped));
        let stats = check_window(frames, baseline, 0);
        assert_eq!((stats.live, stats.pooled), (0, 2));
        assert!(matches!(tables.translate(first_bottom), Ok(Some(_))));
        let reused = allocate_stack(tables, frames, offset).expect("degraded reuse");
        let older = allocate_stack(tables, frames, offset).expect("degraded older reuse");
        assert_eq!(
            (reused.bottom(), older.bottom()),
            (second_bottom, first_bottom)
        );
        let fresh = allocate_stack(tables, frames, offset).expect("degraded fresh stack");
        assert_eq!(
            fresh.bottom(),
            second_bottom + STACK_SPAN_PAGES as u64 * PAGE_SIZE
        );
        check_guards(tables, &fresh);
        write_canaries(&fresh, 5);
        let cpus = probe_stack(&fresh, 5);
        for stack in [reused, older, fresh] {
            free_stack(stack);
        }
        assert_eq!(trim(tables, frames), Ok(skipped));
        let stats = check_window(frames, baseline, 0);
        assert_eq!((stats.live, stats.pooled), (0, 3));
        withhold_transport_for_test(false);
        assert_eq!(
            trim(tables, frames),
            Ok(TrimReport {
                stacks: 3,
                batches: 1,
                complete: true
            })
        );
        check_window(frames, baseline, 0);
        cpus
    }
}

/// Fill all 455 spans with live stacks, check their guards and distinct
/// backing, refuse the next stack cleanly, then pool them all and trim in
/// full batches. Returns the stacks filled and the trim's batch count.
///
/// # Safety
/// As for self_test, with an empty window.
#[cfg(feature = "boot-smoke")]
unsafe fn exhaustion_checks(
    tables: &PageTables,
    frames: &mut FrameAllocator<'_>,
    offset: u64,
    baseline: usize,
    cpus: usize,
) -> (usize, usize) {
    let mut filled: [Option<KernelStack>; STACK_SPANS] = [const { None }; STACK_SPANS];
    // SAFETY: Inherited from the caller; every stack belongs to this test.
    unsafe {
        for (index, slot) in filled.iter_mut().enumerate() {
            let stack = allocate_stack(tables, frames, offset).expect("fill the kernel VA window");
            assert_eq!(stack.guard_below(), page_address(index * STACK_SPAN_PAGES));
            check_guards(tables, &stack);
            write_canaries(&stack, 6);
            *slot = Some(stack);
        }
        let stats = check_window(frames, baseline, 0);
        assert_eq!((stats.spans, stats.live), (STACK_SPANS, STACK_SPANS));
        assert_eq!(
            allocate_stack(tables, frames, offset),
            Err(KvaError::WindowFull)
        );
        assert_eq!(check_window(frames, baseline, 0), stats);
        for stack in filled.iter().flatten() {
            check_canaries(stack, 6);
        }
        let last = filled[STACK_SPANS - 1].as_ref().expect("last kernel stack");
        assert_eq!(probe_stack(last, 6), cpus);
        for slot in &mut filled {
            free_stack(slot.take().expect("filled kernel stack"));
        }
        let stats = check_window(frames, baseline, 0);
        assert_eq!((stats.live, stats.pooled), (0, STACK_SPANS));
        let report = trim(tables, frames).expect("trim the full pool");
        assert_eq!(
            report,
            TrimReport {
                stacks: STACK_SPANS,
                batches: STACK_SPANS.div_ceil(STACKS_PER_BATCH),
                complete: true
            }
        );
        (report.stacks, report.batches)
    }
}
