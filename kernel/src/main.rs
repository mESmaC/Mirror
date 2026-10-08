#![no_std]
#![no_main]

mod arch;
mod boot;
mod interrupts;
mod memory;
mod paging;
mod serial;

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

    #[cfg(feature = "boot-smoke")]
    {
        interrupts::self_test();
        // SAFETY: This is the active, exclusively owned hierarchy and allocator.
        unsafe { paging::self_test(&mut tables, &mut frames) };
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
        // Keep the ownership object in this boot scope until the scheduler can
        // take over. No page-table frames or bootloader memory are reclaimed.
        let _ = &mut tables;
        serial::println!("MIRROR_BOOT_OK");
        arch::halt()
    }
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
