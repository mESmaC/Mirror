#![no_std]
#![no_main]

mod arch;
mod boot;
mod memory;
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
    serial::println!("MIRROR_BOOT_OK");

    #[cfg(feature = "boot-smoke")]
    arch::exit_qemu(true);

    #[cfg(not(feature = "boot-smoke"))]
    arch::halt()
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
