#![no_std]
#![no_main]

extern crate alloc;

mod arch;
mod boot;
mod heap;
mod interrupts;
mod memory;
mod paging;
mod serial;
mod timer;

use core::panic::PanicInfo;
use limine::memory_map::EntryType;

// Track linker-script edits as a compiler input without a host build script.
const _: &str = include_str!("../linker.ld");

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    arch::disable_interrupts();
    serial::init();
    serial::println!("Mirror kernel 0.1.0 | x86_64");
    interrupts::init();

    let boot = boot::info();
    let entries = boot.memory_map.entries();
    let usable_bytes = entries
        .iter()
        .filter(|e| e.entry_type == EntryType::USABLE)
        .try_fold(0_u64, |sum, e| sum.checked_add(e.length))
        .expect("usable memory accounting overflow");
    serial::println!(
        "Memory map: {} regions, {} KiB usable",
        entries.len(),
        usable_bytes / 1024
    );
    serial::println!("Direct map offset: {:#018x}", boot.direct_map_offset);

    #[cfg(feature = "boot-smoke")]
    memory::self_test();

    let mut frames = memory::FrameAllocator::new(entries);
    let first = frames.allocate().expect("no usable physical frame");
    let second = frames.allocate().expect("need two usable physical frames");
    // SAFETY: This is the sole allocator. Limine marks these distinct full pages
    // USABLE and maps usable RAM through the requested higher-half direct map.
    // Kernel, stack, page-table, response, and firmware regions stay reserved.
    unsafe { memory::check_frames(first, second, boot.direct_map_offset) };
    serial::println!("MIRROR_FRAMES_OK {:#x} {:#x}", first, second);

    // SAFETY: Only this CPU and frame allocator exist; interrupts are disabled.
    // All bootloader mappings and reservations stay live during and after the
    // transition. The new hierarchy retains our code, stack and descriptor data.
    let mut tables = unsafe { paging::take_ownership(&mut frames, boot.direct_map_offset) }
        .expect("could not take ownership of page tables");
    serial::println!(
        "Page tables: root={:#x}, owned={}",
        tables.root_physical_address(),
        tables.owned_table_count()
    );

    // SAFETY: The sole allocator and the copied direct map still own all usable
    // RAM. Bitmap metadata excludes every already-allocated page-table frame.
    unsafe { frames.enable_recycling(boot.direct_map_offset) }
        .expect("could not initialize physical-frame recycling");
    // SAFETY: These two bootstrap test pages have no live references, DMA, or
    // mappings beyond the dormant direct map. Their content checks are finished.
    unsafe {
        frames.deallocate(first).expect("release first boot frame");
        frames
            .deallocate(second)
            .expect("release second boot frame");
    }
    assert_eq!(
        frames.stats().expect("frame accounting").allocated_frames,
        tables.owned_table_count(),
        "boot allocations must remain tracked during the allocator transition"
    );

    // SAFETY: IRQs are still disabled. This allocator and active hierarchy own
    // the heap range exclusively, and frame recycling is now available.
    unsafe { heap::init(&mut tables, &mut frames) }.expect("could not initialize kernel heap");
    let heap_stats = heap::stats();
    serial::println!(
        "MIRROR_HEAP_READY size={} free={}",
        heap_stats.size,
        heap_stats.free
    );
    let frame_stats = frames.stats().expect("frame accounting");
    serial::println!(
        "MIRROR_FRAME_POOL total={} allocated={} metadata={} free={}",
        frame_stats.total_frames,
        frame_stats.allocated_frames,
        frame_stats.metadata_frames,
        frame_stats.recycled_frames + frame_stats.never_allocated_frames
    );
    timer::init();

    #[cfg(feature = "boot-smoke")]
    {
        memory::self_test_recycling(&mut frames);
        interrupts::self_test();
        // SAFETY: This is the active, exclusively owned hierarchy and allocator.
        unsafe { paging::self_test(&mut tables, &mut frames) };
        heap::self_test();
        timer::self_test();
        test_heap_with_interrupts();
        match boot::smoke_mode() {
            boot::SmokeMode::Normal => interrupts::arm_expected_page_fault(paging::TEST_ADDRESS),
            boot::SmokeMode::PageFault => {
                serial::println!("MIRROR_TEST_EXPECT_FATAL vector=14");
            }
            boot::SmokeMode::InvalidOpcode => {
                serial::println!("MIRROR_TEST_EXPECT_FATAL vector=6");
                // SAFETY: Deliberate smoke-test #UD. The installed handler must
                // report it and terminate the guest; no Rust execution resumes.
                unsafe { core::arch::asm!("ud2", options(noreturn)) };
            }
        }
        // SAFETY: Deliberate #PF on an address just unmapped and invalidated by
        // the paging test. Assembly expresses a trapping access without claiming
        // Rust pointer validity. The handler must terminate this smoke guest.
        unsafe {
            core::arch::asm!("mov rax, [{address}]", address = in(reg) paging::TEST_ADDRESS,
                out("rax") _, options(nostack));
        }
        panic!("unmapped access unexpectedly returned");
    }

    #[cfg(not(feature = "boot-smoke"))]
    {
        serial::println!("MIRROR_BOOT_OK");
        serial::println!("Mirror idle: timer ticks={}", timer::ticks());
        // SAFETY: The PIC unmasks only the installed timer IRQ; the heap and
        // descriptors are initialized. Keep allocator/table state alive here.
        unsafe { arch::idle() }
    }
}

#[cfg(feature = "boot-smoke")]
fn test_heap_with_interrupts() {
    use alloc::vec::Vec;

    assert!(!arch::interrupts_enabled());
    let baseline = heap::stats();
    let before = timer::ticks();
    // SAFETY: Timer setup and its first hardware-delivery test have completed;
    // the only unmasked handler neither allocates nor takes the heap lock.
    unsafe { arch::restore_interrupts(true) };
    while timer::ticks().wrapping_sub(before) < 3 {
        let mut values = Vec::with_capacity(64);
        for index in 0..64_u64 {
            values.push(index ^ 0x4d49_5252);
        }
        core::hint::black_box(&values);
        assert_eq!(values[63], 63 ^ 0x4d49_5252);
        drop(values);
        assert!(
            arch::interrupts_enabled(),
            "allocator left interrupts disabled"
        );
    }
    arch::disable_interrupts();
    assert_eq!(heap::stats(), baseline, "IRQ-enabled allocations leaked");
    serial::println!(
        "MIRROR_HEAP_IRQ_OK ticks={}",
        timer::ticks().wrapping_sub(before)
    );
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    arch::disable_interrupts();
    serial::println!("MIRROR_PANIC: {}", info);

    #[cfg(feature = "boot-smoke")]
    arch::exit_qemu(false);

    #[cfg(not(feature = "boot-smoke"))]
    arch::halt()
}
