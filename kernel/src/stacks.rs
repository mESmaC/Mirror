//! Guarded kernel stacks (spec 0016a).
//!
//! Layout rule: every stack owns one unmapped guard page directly below it and
//! one directly above it, so neighbouring stacks are separated by two unmapped
//! pages. A fault frame pushed just below a guard page therefore faults too.
//! One page per side is enough because the x86_64-unknown-none target emits
//! inline stack probes for frames larger than a page.
//!
//! Stacks are mapped only through the general kernel mapping path, so they must
//! be created before `share_with_secondaries` seals kernel mappings.
//!
//! The stack arena starts with the BSP region: the scheduler stack, which
//! replaces Limine's boot stack, the RSP0 trap stack and the IST1 (#DF), IST2
//! (NMI) and IST3 (#MC) stacks. The AP slots (smp.rs) follow it from `AP_BASE`.
//! The arena sits in the shared upper half, so every process root, which
//! copies the upper PML4 entries, maps the BSP stacks too.

use core::{
    arch::asm,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    memory::{FrameAllocator, PAGE_SIZE},
    paging::PageTables,
};

/// The largest stack this module maps: the BSP's 256 KiB scheduler stack.
pub const MAX_PAGES: usize = 64;
/// Guarded stacks live in the kernel half only.
const KERNEL_HALF: u64 = 0xffff_8000_0000_0000;

const ARENA: u64 = 0xffff_d100_0000_0000;
const SCHEDULER_PAGES: usize = 64;
const RSP0_PAGES: usize = 16;
const IST_COUNT: usize = 3;
const IST_PAGES: usize = 4;
const SCHEDULER_BOTTOM: u64 = ARENA + PAGE_SIZE;
const RSP0_BOTTOM: u64 = ARENA + span(SCHEDULER_PAGES) + PAGE_SIZE;
const IST_BASE: u64 = ARENA + span(SCHEDULER_PAGES) + span(RSP0_PAGES);
/// The first AP slot's guard below starts right after the BSP region.
pub const AP_BASE: u64 = IST_BASE + IST_COUNT as u64 * span(IST_PAGES);
/// The BSP region in map order: scheduler, RSP0, then IST1-3.
const BSP_STACKS: [(u64, usize); 2 + IST_COUNT] = [
    (SCHEDULER_BOTTOM, SCHEDULER_PAGES),
    (RSP0_BOTTOM, RSP0_PAGES),
    (ist_bottom(0), IST_PAGES),
    (ist_bottom(1), IST_PAGES),
    (ist_bottom(2), IST_PAGES),
];

const _: () = {
    assert!(SCHEDULER_PAGES <= MAX_PAGES && RSP0_PAGES <= MAX_PAGES && IST_PAGES <= MAX_PAGES);
    // Each BSP stack keeps its own guard on both sides: two unmapped pages
    // between neighbours, and between IST3 and the first AP slot's stack.
    let mut index = 1;
    while index < BSP_STACKS.len() {
        let (previous, pages) = BSP_STACKS[index - 1];
        assert!(BSP_STACKS[index].0 == top(previous, pages) + 2 * PAGE_SIZE);
        index += 1;
    }
    let (last, pages) = BSP_STACKS[BSP_STACKS.len() - 1];
    assert!(AP_BASE == top(last, pages) + PAGE_SIZE);
};

static BSP_MAPPED: AtomicBool = AtomicBool::new(false);

/// Address space one guarded stack of `pages` pages takes: its guard below,
/// the stack and its guard above. The stack's bottom is one page into it.
pub const fn span(pages: usize) -> u64 {
    (pages as u64 + 2) * PAGE_SIZE
}

const fn top(bottom: u64, pages: usize) -> u64 {
    bottom + pages as u64 * PAGE_SIZE
}

const fn ist_bottom(index: usize) -> u64 {
    IST_BASE + index as u64 * span(IST_PAGES) + PAGE_SIZE
}

/// The tops of the BSP's guarded stacks that boot installs by hand. RSP0 is
/// read through `bsp_rsp0_top` when userspace starts.
pub struct BspStacks {
    pub scheduler_top: u64,
    pub ist_tops: [u64; IST_COUNT],
}

/// Map the BSP region: the 256 KiB scheduler stack, the 64 KiB RSP0 stack and
/// three 16 KiB IST stacks, each guarded. If one fails, the stacks mapped
/// before it are unmapped again and the error returns.
///
/// # Safety
/// The `map_guarded` contract holds, and this runs once. The BSP region of
/// the arena is unused: nothing else maps or accesses it.
pub unsafe fn map_bsp(
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
) -> Result<BspStacks, &'static str> {
    assert!(
        !BSP_MAPPED.load(Ordering::Acquire),
        "BSP stacks mapped twice"
    );
    for (index, &(bottom, pages)) in BSP_STACKS.iter().enumerate() {
        // SAFETY: Inherited from the caller; each stack has its own range.
        if let Err(reason) = unsafe { map_guarded(tables, frames, bottom, pages) } {
            for &(bottom, pages) in &BSP_STACKS[..index] {
                // SAFETY: Mapped by this loop; nothing has run on it yet.
                unsafe { unmap_guarded(tables, frames, bottom, pages) };
            }
            return Err(reason);
        }
    }
    BSP_MAPPED.store(true, Ordering::Release);
    Ok(BspStacks {
        scheduler_top: top(SCHEDULER_BOTTOM, SCHEDULER_PAGES),
        ist_tops: core::array::from_fn(|index| top(ist_bottom(index), IST_PAGES)),
    })
}

/// The top of the BSP's guarded RSP0 stack, for the TSS on user entry.
pub fn bsp_rsp0_top() -> u64 {
    assert!(BSP_MAPPED.load(Ordering::Acquire), "BSP stacks not mapped");
    top(RSP0_BOTTOM, RSP0_PAGES)
}

/// Whether the current RSP lies in the BSP's guarded scheduler stack.
pub fn on_bsp_scheduler_stack() -> bool {
    let rsp: u64;
    // SAFETY: Reads this CPU's stack pointer at CPL0 without side effects.
    unsafe { asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags)) };
    BSP_MAPPED.load(Ordering::Acquire)
        && (SCHEDULER_BOTTOM..top(SCHEDULER_BOTTOM, SCHEDULER_PAGES)).contains(&rsp)
}

/// Validate a guarded stack's placement and return its top. The guard below,
/// the stack itself and the guard above must all lie in the kernel half.
fn bounds(bottom: u64, pages: usize) -> Result<u64, &'static str> {
    if !(1..=MAX_PAGES).contains(&pages)
        || !bottom.is_multiple_of(PAGE_SIZE)
        || bottom < KERNEL_HALF + PAGE_SIZE
    {
        return Err("stack_layout_invalid");
    }
    let top = bottom
        .checked_add(pages as u64 * PAGE_SIZE)
        .ok_or("stack_layout_invalid")?;
    top.checked_add(PAGE_SIZE).ok_or("stack_layout_invalid")?;
    Ok(top)
}

/// Both guard pages of the stack spanning `bottom..top` must be unmapped.
fn check_guards(tables: &PageTables, bottom: u64, top: u64) -> Result<(), &'static str> {
    for guard in [bottom - PAGE_SIZE, top] {
        if tables
            .translate(guard)
            .map_err(|_| "stack_guard_invalid")?
            .is_some()
        {
            return Err("stack_guard_mapped");
        }
    }
    Ok(())
}

/// Map a stack of `pages` freshly allocated, zeroed frames at `bottom`, with an
/// unmapped guard page below and above, and return its top. Both guards are
/// checked before and after mapping. On failure every page this call mapped is
/// unmapped and its frame released before the error returns; page-table frames
/// created on the way stay owned by the hierarchy, as for any kernel mapping.
///
/// # Safety
/// The `PageTables::map_page` contract holds: the BSP with IF clear, this root
/// active, the unique allocator, and kernel mappings not yet sealed by
/// `share_with_secondaries`. The caller exclusively owns the virtual range from
/// the guard below to the guard above: no other code maps or accesses it.
pub unsafe fn map_guarded(
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
    bottom: u64,
    pages: usize,
) -> Result<u64, &'static str> {
    let top = bounds(bottom, pages)?;
    check_guards(tables, bottom, top)?;
    let mut mapped = 0;
    let result = (|| {
        for page in 0..pages {
            let physical = frames.allocate().ok_or("stack_out_of_frames")?;
            let address = bottom + page as u64 * PAGE_SIZE;
            // SAFETY: The caller supplies the map_page contract and exclusive
            // ownership of this range. The fresh frame belongs to no one else.
            if unsafe { tables.map_page(address, physical, frames) }.is_err() {
                // SAFETY: A failed leaf insertion never transfers ownership,
                // and the frame was never mapped or written.
                unsafe { frames.deallocate(physical) }.expect("unused stack frame");
                return Err("stack_mapping_failed");
            }
            mapped += 1;
            // SAFETY: This full page was just mapped writable and is owned
            // exclusively by this call; no reference to it exists yet.
            unsafe { (address as *mut u8).write_bytes(0, PAGE_SIZE as usize) };
        }
        check_guards(tables, bottom, top)
    })();
    match result {
        Ok(()) => Ok(top),
        Err(reason) => {
            // SAFETY: Exactly the first `mapped` pages were mapped above, and
            // nothing has used them: the stack top was never returned.
            unsafe { unmap_pages(tables, frames, bottom, mapped) };
            Err(reason)
        }
    }
}

/// Unmap a stack that `map_guarded` mapped and release its frames.
///
/// # Safety
/// The `PageTables::unmap_page` contract holds: the BSP with IF clear, this
/// root active and kernel mappings not yet sealed. `bottom` and `pages` are
/// exactly those of a successful `map_guarded` call. No CPU runs on the stack
/// or will use it again, and no reference into it is live.
pub unsafe fn unmap_guarded(
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
    bottom: u64,
    pages: usize,
) {
    bounds(bottom, pages).expect("guarded stack layout");
    // SAFETY: Inherited from the caller, for every page of the stack.
    unsafe { unmap_pages(tables, frames, bottom, pages) };
}

/// # Safety
/// As for `unmap_guarded`, for the first `pages` pages from `bottom`, each of
/// which maps a frame this module allocated.
unsafe fn unmap_pages(
    tables: &mut PageTables,
    frames: &mut FrameAllocator<'_>,
    bottom: u64,
    pages: usize,
) {
    for page in 0..pages {
        let address = bottom + page as u64 * PAGE_SIZE;
        // SAFETY: The caller guarantees this leaf maps a frame allocated for
        // the stack and that nothing uses it. Unmapping invalidates the local
        // translation before the frame returns to the allocator.
        unsafe {
            let physical = tables.unmap_page(address).expect("stack rollback mapping");
            frames.deallocate(physical).expect("stack rollback frame");
        }
    }
}

/// Exercise placement checks, zeroing, guard rejection and rollback on scratch
/// pages beside `paging::TEST_ADDRESS`, whose page table the paging self-test
/// already created, so frame counts compare exactly. Every page is unmapped
/// again on return.
///
/// # Safety
/// The `map_guarded` contract holds, the paging self-test has run, and no
/// other code uses the 16 pages from `paging::TEST_ADDRESS + 8 * PAGE_SIZE`.
#[cfg(feature = "boot-smoke")]
pub unsafe fn self_test(tables: &mut PageTables, frames: &mut FrameAllocator<'_>) {
    const BOTTOM: u64 = crate::paging::TEST_ADDRESS + 16 * PAGE_SIZE;
    const PAGES: usize = 4;
    const TOP: u64 = BOTTOM + PAGES as u64 * PAGE_SIZE;
    let allocated = |frames: &FrameAllocator<'_>| {
        frames
            .stats()
            .expect("stack test frame accounting")
            .allocated_frames
    };
    let before = (allocated(frames), tables.owned_table_count());
    for (bottom, pages) in [
        (BOTTOM + 8, PAGES),
        (BOTTOM, 0),
        (BOTTOM, MAX_PAGES + 1),
        (0x10_0000, PAGES),
        (KERNEL_HALF, PAGES),
        (u64::MAX - (PAGE_SIZE - 1), 1),
    ] {
        // SAFETY: Each placement is rejected before anything is mapped.
        let result = unsafe { map_guarded(tables, frames, bottom, pages) };
        assert_eq!(result, Err("stack_layout_invalid"));
    }
    let check = |tables: &PageTables, fill: Option<u8>| {
        assert_eq!(tables.translate(BOTTOM - PAGE_SIZE), Ok(None));
        assert_eq!(tables.translate(TOP), Ok(None));
        for word in 0..PAGES * PAGE_SIZE as usize / 8 {
            let pointer = (BOTTOM as *mut u64).wrapping_add(word);
            // SAFETY: The stack is mapped writable and exclusively owned by
            // this test. Raw volatile accesses create no Rust references.
            unsafe {
                assert_eq!(pointer.read_volatile(), 0, "stack not zeroed");
                if let Some(fill) = fill {
                    pointer.write_volatile(u64::from_ne_bytes([fill; 8]));
                }
            }
        }
    };
    // Dirty the frames, release them and map again: recycled frames must come
    // back zeroed.
    for fill in [Some(0xa5), None] {
        // SAFETY: The scratch range and its guards are unmapped and unused.
        let top = unsafe { map_guarded(tables, frames, BOTTOM, PAGES) };
        assert_eq!(top, Ok(TOP));
        check(tables, fill);
        if fill.is_some() {
            // SAFETY: Mapped by the call above; no reference into it remains.
            unsafe { unmap_guarded(tables, frames, BOTTOM, PAGES) };
            assert_eq!(allocated(frames), before.0);
        }
    }
    let mapped = (allocated(frames), tables.owned_table_count());
    assert_eq!(mapped, (before.0 + PAGES, before.1));
    // A neighbour whose own guard would be a page of this stack is rejected
    // before it maps anything, from below and from above.
    for bottom in [BOTTOM - 2 * PAGE_SIZE, TOP] {
        // SAFETY: The placement fails its guard check before mapping.
        let result = unsafe { map_guarded(tables, frames, bottom, 2) };
        assert_eq!(result, Err("stack_guard_mapped"));
        assert_eq!((allocated(frames), tables.owned_table_count()), mapped);
    }
    // This range's guards are free, but its fourth page is the stack's bottom:
    // three pages, including the stack's lower guard, are mapped and then
    // rolled back.
    // SAFETY: The first three pages are unmapped scratch pages; the fourth is
    // already mapped, so map_page refuses it without replacing it.
    let result = unsafe { map_guarded(tables, frames, BOTTOM - 3 * PAGE_SIZE, 8) };
    assert_eq!(result, Err("stack_mapping_failed"));
    assert_eq!((allocated(frames), tables.owned_table_count()), mapped);
    for page in 1..=3 {
        assert_eq!(tables.translate(BOTTOM - page * PAGE_SIZE), Ok(None));
    }
    check(tables, None);
    // SAFETY: Mapped above; the checks have ended and no reference remains.
    unsafe { unmap_guarded(tables, frames, BOTTOM, PAGES) };
    assert_eq!((allocated(frames), tables.owned_table_count()), before);
    for page in 0..PAGES as u64 {
        assert_eq!(tables.translate(BOTTOM + page * PAGE_SIZE), Ok(None));
    }
}
