//! Boot validation and lifecycle checks for the shared kernel mapping window.
//!
//! Runtime APIs live in paging; this exercises real map/probe/unmap/reuse on
//! every CPU and through a pre-existing userspace root before enabling clients.

#[cfg(feature = "boot-smoke")]
use crate::paging::PagingError;
use crate::{
    arch,
    memory::{FrameAllocator, PAGE_SIZE},
    paging::{PageTables, SHARED_BASE, SHARED_PAGES, UserAddressSpace},
    serial, smp,
};

pub const PROBE_ADDRESS: u64 = SHARED_BASE;
const CANARY_ADDRESS: u64 = SHARED_BASE + PAGE_SIZE;
const CANARY: u64 = 0x4d49_5252_4f52_564d;

/// Exercise batch preflight and a run crossing a leaf-table boundary, well
/// beyond the committed heap. No byte in this temporary run reaches Heap.
#[cfg(feature = "boot-smoke")]
pub fn heap_window_self_test(tables: &PageTables, frames: &mut FrameAllocator<'_>, offset: u64) {
    use crate::heap::{HEAP_MAX_SIZE, HEAP_START, MAX_GROW_PAGES};

    let address = HEAP_START + 4 * 1024 * 1024 - PAGE_SIZE;
    assert!(HEAP_START + crate::heap::stats().size as u64 <= address);
    let baseline = frames.stats().unwrap().allocated_frames;
    let first = frames.allocate().expect("heap boundary first frame");
    let second = frames.allocate().expect("heap boundary second frame");
    // SAFETY: Distinct fresh owned RAM, dormant direct-map aliases, BSP/IF0.
    unsafe {
        clear_frame(offset, first);
        clear_frame(offset, second);
        write_word(offset, first, CANARY);
        write_word(offset, second, !CANARY);
    }
    for invalid in [
        HEAP_START - PAGE_SIZE,
        HEAP_START + HEAP_MAX_SIZE as u64,
        address + 1,
    ] {
        // SAFETY: Invalid ranges must reject without publishing any mapping.
        unsafe {
            assert!(tables.map_heap_pages(invalid, &[first]).is_err());
            assert!(tables.unmap_heap_pages(invalid, &[first]).is_err());
        }
    }
    // SAFETY: Boundary and conflicting-leaf probes remain unpublished to the
    // allocator. Every valid temporary mapping has complete owned WB backing.
    unsafe {
        assert_eq!(
            tables.map_heap_pages(address, &[]),
            Err(PagingError::InvalidHeapRange)
        );
        assert_eq!(
            tables.map_heap_pages(address, &[first; MAX_GROW_PAGES + 1]),
            Err(PagingError::InvalidHeapRange)
        );
        assert!(
            tables
                .map_heap_pages(address, &[first, second + 1])
                .is_err()
        );
        assert_eq!(tables.translate(address), Ok(None));
        tables
            .map_heap_pages(address + PAGE_SIZE, &[second])
            .unwrap();
        assert_eq!(
            tables.map_heap_pages(address, &[first, second]),
            Err(PagingError::AlreadyMapped)
        );
        assert_eq!(tables.translate(address), Ok(None));
        assert_eq!(tables.translate(address + PAGE_SIZE), Ok(Some(second)));
        tables
            .unmap_heap_pages(address + PAGE_SIZE, &[second])
            .unwrap();
    }
    let mut cpus = 0;
    for backing in [[first, second], [second, first]] {
        // SAFETY: The run is uncommitted, quiescent and absent. Both backing
        // frames are initialized and unchanged throughout each CPU read.
        unsafe {
            tables.map_heap_pages(address, &backing).unwrap();
            for (page, physical) in backing.iter().copied().enumerate() {
                let alias = address + page as u64 * PAGE_SIZE;
                assert_eq!(tables.translate(alias), Ok(Some(physical)));
                let observed =
                    smp::probe_shared_page(alias, if physical == first { CANARY } else { !CANARY });
                if page == 0 {
                    cpus = observed;
                } else {
                    assert_eq!(observed, cpus);
                }
            }
            // One mismatched identity must leave BOTH mappings intact.
            assert_eq!(
                tables.unmap_heap_pages(address, &[backing[0], backing[0]]),
                Err(PagingError::InvalidTable)
            );
            assert_eq!(tables.translate(address), Ok(Some(backing[0])));
            assert_eq!(tables.translate(address + PAGE_SIZE), Ok(Some(backing[1])));
            tables.unmap_heap_pages(address, &backing).unwrap();
        }
        assert_eq!(tables.translate(address), Ok(None));
        assert_eq!(tables.translate(address + PAGE_SIZE), Ok(None));
    }
    // SAFETY: All accesses ended and exact batch ACKs preceded both returns.
    unsafe {
        frames.deallocate(first).unwrap();
        frames.deallocate(second).unwrap();
    }
    assert_eq!(frames.stats().unwrap().allocated_frames, baseline);
    serial::println!(
        "MIRROR_HEAP_PAGING_OK cross_table=1 atomic=1 cpus={} reclaimed=2",
        cpus
    );
}

pub fn init(tables: &PageTables, frames: &mut FrameAllocator<'_>, offset: u64) {
    assert!(!arch::interrupts_enabled());
    match smp::prepare_kernel_update() {
        Ok(ticket) => drop(ticket),
        Err(reason) => {
            // A late or failed AP cannot be omitted from a reclamation barrier.
            // Preserve the original fixed mappings and decline runtime updates.
            serial::println!("MIRROR_SHARED_VM_SKIPPED reason={}", reason);
            return;
        }
    }
    let baseline = frames.stats().expect("frame accounting").allocated_frames;
    let first = frames.allocate().expect("shared first frame");
    let second = frames.allocate().expect("shared second frame");
    let canary = frames.allocate().expect("shared canary frame");
    assert!(first != second && first != canary && second != canary);
    // SAFETY: Fresh owned RAM, no references, DMA or non-direct-map aliases.
    unsafe {
        clear_frame(offset, first);
        clear_frame(offset, second);
        clear_frame(offset, canary);
        write_word(offset, canary, CANARY);
    }
    // This root predates every leaf update, proving that permanently shared
    // upper branches expose later mappings without changing copied PML4 entries.
    // SAFETY: BSP/IF0, unique allocator; the runtime window's parents are pinned.
    let mut borrowed =
        unsafe { UserAddressSpace::new(tables, frames) }.expect("borrowed kernel mapping root");
    #[cfg(feature = "boot-smoke")]
    boundary_self_test(tables, first);
    // SAFETY: Owned initialized page and reserved empty alias, no active users.
    unsafe { tables.map_shared_page(CANARY_ADDRESS, canary) }.expect("map shared canary");
    let rounds = if cfg!(feature = "boot-smoke") { 4 } else { 1 };
    let mut cpus = 0;
    for round in 0..rounds {
        for (physical, salt) in [(first, 0xaaaa_1111), (second, 0xbbbb_2222)] {
            let expected = CANARY ^ salt ^ round;
            // SAFETY: Previous shootdown completed and no CPU accesses this
            // frame's dormant direct-map alias. Only this scalar is published.
            unsafe { write_word(offset, physical, expected) };
            // SAFETY: Full owned initialized frame, empty reserved alias, IF0.
            unsafe { tables.map_shared_page(PROBE_ADDRESS, physical) }.expect("map shared probe");
            #[cfg(feature = "boot-smoke")]
            // SAFETY: A duplicate insertion is a validation error before any
            // mutation; it must preserve the currently owned mapping.
            unsafe {
                assert_eq!(
                    tables.map_shared_page(PROBE_ADDRESS, physical),
                    Err(PagingError::AlreadyMapped)
                );
            }
            assert_eq!(tables.translate(PROBE_ADDRESS), Ok(Some(physical)));
            // SAFETY: All probe targets may read this stable initialized word;
            // their ACK ends the access before any mapping or content changes.
            cpus = unsafe { smp::probe_shared_page(PROBE_ADDRESS, expected) };
            assert!(cpus > 0);
            assert_eq!(
                // SAFETY: This separate initialized canary mapping stays stable.
                unsafe { smp::probe_shared_page(CANARY_ADDRESS, CANARY) },
                cpus
            );
            // SAFETY: Both roots map live kernel code/stacks. IF0; no ring3
            // entry. Switch back before assertions and any mapping operation.
            let observed = unsafe {
                borrowed.activate().expect("activate borrowed kernel root");
                let value = (PROBE_ADDRESS as *const u64).read_volatile();
                borrowed.deactivate(tables).expect("restore kernel root");
                value
            };
            assert_eq!(observed, expected, "shared leaf missing from borrowed root");
            // SAFETY: Every read command completed; no live reference or DMA.
            // The API only returns after every required CPU discarded the PTE.
            assert_eq!(
                // SAFETY: Completed probes relinquished this alias on every CPU.
                unsafe { tables.unmap_shared_page(PROBE_ADDRESS) },
                Ok(physical)
            );
            assert_eq!(tables.translate(PROBE_ADDRESS), Ok(None));
            // Poison the old backing so stale TLB reuse cannot accidentally
            // produce the expected value of the next, distinct physical frame.
            // SAFETY: All invalidation ACKs preceded unmap's return above.
            unsafe { write_word(offset, physical, !expected) };
            assert_eq!(
                // SAFETY: The neighboring canary is still mapped and unchanged.
                unsafe { smp::probe_shared_page(CANARY_ADDRESS, CANARY) },
                cpus
            );
        }
    }
    // SAFETY: No CPU retains a live canary access; exact shootdown ACKs precede
    // removal return and the subsequent frame reclamation.
    assert_eq!(
        // SAFETY: All canary probes completed before this removal.
        unsafe { tables.unmap_shared_page(CANARY_ADDRESS) },
        Ok(canary)
    );
    #[cfg(feature = "boot-smoke")]
    // SAFETY: Removing an absent owned-window alias is a pre-mutation error.
    unsafe {
        assert_eq!(
            tables.unmap_shared_page(PROBE_ADDRESS),
            Err(PagingError::NotMapped)
        );
    }
    // SAFETY: The borrowed root is inactive, never used by APs, and owns only
    // its empty lower half. Its destruction must retain shared upper tables.
    unsafe { borrowed.destroy(frames) }.expect("borrowed root reclamation");
    // SAFETY: No live mappings, references or DMA remain. The shared unmaps
    // completed all CPU invalidations before returning these physical frames.
    unsafe {
        for physical in [first, second, canary] {
            frames
                .deallocate(physical)
                .expect("shared probe reclamation");
        }
    }
    assert_eq!(frames.stats().unwrap().allocated_frames, baseline);
    assert_eq!(tables.translate(PROBE_ADDRESS), Ok(None));
    assert_eq!(tables.translate(CANARY_ADDRESS), Ok(None));
    serial::println!(
        "MIRROR_SHARED_VM_READY pages={} cpus={} rounds={} reclaimed=3 borrowed_root=1",
        SHARED_PAGES,
        cpus,
        rounds
    );
}

/// The caller exclusively owns a complete usable RAM frame and dormant HHDM.
unsafe fn clear_frame(offset: u64, physical: u64) {
    let address = offset
        .checked_add(physical)
        .expect("owned frame direct map");
    // SAFETY: Inherits initialized HHDM and full frame ownership from caller.
    unsafe { (address as *mut u8).write_bytes(0, PAGE_SIZE as usize) };
}

/// The caller excludes every other access to this owned frame's first word.
unsafe fn write_word(offset: u64, physical: u64, value: u64) {
    let address = offset
        .checked_add(physical)
        .expect("owned frame direct map");
    // SAFETY: Owned aligned RAM, initialized HHDM, no live reference or access.
    unsafe { (address as *mut u64).write_volatile(value) };
}

#[cfg(feature = "boot-smoke")]
fn boundary_self_test(tables: &PageTables, physical: u64) {
    for address in [
        PROBE_ADDRESS + 1,
        SHARED_BASE - PAGE_SIZE,
        SHARED_BASE + SHARED_PAGES as u64 * PAGE_SIZE,
        0x8000_0000_0000,
    ] {
        // SAFETY: These validation probes must reject addresses before mutation.
        unsafe {
            assert!(tables.map_shared_page(address, physical).is_err());
            assert!(tables.unmap_shared_page(address).is_err());
        }
    }
    // SAFETY: Invalid physical alignment must leave the valid window untouched.
    assert!(unsafe { tables.map_shared_page(PROBE_ADDRESS, physical + 1) }.is_err());
    assert_eq!(tables.translate(PROBE_ADDRESS), Ok(None));
    serial::println!("MIRROR_SHARED_VM_TESTS_OK");
}

/// After the deliberately faulted AP, shared updates must reject BEFORE the
/// PTE changes. Ordinary proof rendezvous may tolerate survivors; reclaim may not.
#[cfg(feature = "boot-smoke")]
pub fn offline_self_test(tables: &PageTables, frames: &mut FrameAllocator<'_>, offset: u64) {
    if smp::prepare_kernel_update().is_ok() {
        return; // UP/PIC has no intentionally faulted secondary.
    }
    let physical = frames.allocate().expect("offline rejection frame");
    // SAFETY: Fresh owned RAM with no live aliases or users on any CPU.
    unsafe { clear_frame(offset, physical) };
    // SAFETY: The dead-target preflight must reject without publishing a PTE.
    assert_eq!(
        // SAFETY: Valid owned frame; the offline preflight must reject it.
        unsafe { tables.map_shared_page(PROBE_ADDRESS, physical) },
        Err(PagingError::SharedUpdateUnavailable)
    );
    assert_eq!(tables.translate(PROBE_ADDRESS), Ok(None));
    // SAFETY: Failed insertion retained ownership and published no mapping.
    unsafe { frames.deallocate(physical) }.expect("offline rejection reclamation");
    serial::println!("MIRROR_SHARED_OFFLINE_OK unchanged=1");
}

/// This dedicated smoke boot must terminate inside unmap, before the removed
/// physical frame can be returned to its caller or handed back to the allocator.
#[cfg(feature = "boot-smoke")]
pub fn timeout_self_test(tables: &PageTables, frames: &mut FrameAllocator<'_>, offset: u64) -> ! {
    let physical = frames.allocate().expect("timeout probe frame");
    // SAFETY: Fresh owned RAM and no outstanding alias probes.
    unsafe {
        clear_frame(offset, physical);
        write_word(offset, physical, CANARY);
        tables
            .map_shared_page(PROBE_ADDRESS, physical)
            .expect("timeout probe mapping");
        assert!(smp::probe_shared_page(PROBE_ADDRESS, CANARY) >= 2);
    }
    smp::arm_shootdown_timeout_for_test();
    serial::println!(
        "MIRROR_SHARED_TIMEOUT_EXPECT address={:#x} frame={:#x}",
        PROBE_ADDRESS,
        physical
    );
    // SAFETY: Every probe finished. Deliberately suppress one remote ACK;
    // no physical frame may escape this function through a successful return.
    let _ = unsafe { tables.unmap_shared_page(PROBE_ADDRESS) };
    serial::println!("MIRROR_SHARED_TIMEOUT_UNEXPECTED_RETURN");
    panic!("shared unmap returned without every CPU acknowledgement");
}
