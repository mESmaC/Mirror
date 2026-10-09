#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod apic;
mod arch;
mod block;
mod boot;
mod capability;
mod console;
mod context;
mod dma;
mod elf;
mod fpu;
mod framebuffer;
mod heap;
mod input;
mod interrupts;
mod ipc;
mod keyboard;
mod memory;
mod mouse;
mod paging;
mod pci;
mod pointer;
mod ps2;
mod serial;
mod surface;
mod timer;
mod userspace;
mod vm;
mod windows;

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
    fpu::init();
    interrupts::tls_ready();
    #[cfg(feature = "boot-smoke")]
    fpu::self_test();

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
    let topology = acpi::discover(&boot, &mut tables, &mut frames);
    apic::init(topology.as_ref(), &boot, &mut tables, &mut frames);
    timer::init();

    let devices = pci::probe();
    if devices.functions == 0 {
        serial::println!("PCI: no functions discovered through configuration mechanism 1");
    }
    let mut display = boot.framebuffer.as_ref().and_then(|framebuffer| {
        // SAFETY: Limine provides a writable reserved framebuffer. The owned
        // hierarchy preserves its mapping and cache attributes; its memory is
        // never allocated as RAM. This local driver is the sole writer on the
        // BSP, and no interrupt handler or user mapping exposes its bytes.
        match unsafe { framebuffer::Driver::from_limine(framebuffer) } {
            Ok(mut driver) => {
                let info = driver.info();
                driver.render_boot(usable_bytes);
                driver.update_status("NATIVE USERSPACE STARTING", 70);
                serial::println!(
                    "MIRROR_FRAMEBUFFER_READY width={} height={} pitch={} bpp={} bytes={}",
                    info.width,
                    info.height,
                    info.pitch,
                    info.bpp,
                    info.byte_len
                );
                Some(driver)
            }
            Err(error) => {
                serial::println!("MIRROR_FRAMEBUFFER_SKIPPED reason={:?}", error);
                None
            }
        }
    });
    if boot.framebuffer.is_none() {
        serial::println!("MIRROR_FRAMEBUFFER_SKIPPED reason=missing");
    }

    #[cfg(feature = "boot-smoke")]
    {
        acpi::self_test();
        apic::self_test();
        pci::self_test();
        framebuffer::self_test();
        surface::self_test();
        // SAFETY: IF is clear; the sole allocator and preserved writable HHDM
        // exclusively own these test pages, which are never sent to a device.
        unsafe { dma::self_test(&mut frames, boot.direct_map_offset) };
        block::self_test();
        keyboard::self_test();
        mouse::self_test();
        ps2::self_test();
        input::self_test();
        pointer::self_test();
        console::self_test();
        assert!(devices.functions > 0, "no PCI devices discovered");
        assert!(
            devices.virtio_block_devices > 0,
            "runner's VirtIO boot disk was not discovered"
        );
        memory::self_test_recycling(&mut frames);
        interrupts::self_test();
        // SAFETY: This is the active, exclusively owned hierarchy and allocator.
        unsafe { paging::self_test(&mut tables, &mut frames) };
        heap::self_test();
        timer::self_test();
        test_heap_with_interrupts();
        elf::self_test();
    }

    let inject_block_error = {
        #[cfg(feature = "boot-smoke")]
        {
            matches!(boot::smoke_mode(), boot::SmokeMode::BlockIoError)
        }
        #[cfg(not(feature = "boot-smoke"))]
        {
            false
        }
    };
    probe_block_device(
        &devices,
        &mut frames,
        boot.direct_map_offset,
        inject_block_error,
    );

    let input_devices = ps2::init();
    let keyboard_ready = match input_devices.keyboard {
        Ok(()) => {
            serial::println!("MIRROR_KEYBOARD_READY transport=i8042 set=2 irq=1");
            true
        }
        Err(error) => {
            serial::println!("MIRROR_KEYBOARD_SKIPPED reason={:?}", error);
            false
        }
    };
    if keyboard_ready {
        timer::enable_keyboard_irq();
    }
    let mouse_ready = match input_devices.mouse {
        Ok(()) => {
            serial::println!("MIRROR_MOUSE_READY transport=i8042 packet=3 irq=12");
            true
        }
        Err(error) => {
            serial::println!("MIRROR_MOUSE_SKIPPED reason={:?}", error);
            false
        }
    };
    if mouse_ready {
        timer::enable_mouse_irq();
    }

    let scenario = {
        #[cfg(feature = "boot-smoke")]
        {
            if let boot::SmokeMode::User(value) = boot::smoke_mode() {
                value
            } else {
                0
            }
        }
        #[cfg(not(feature = "boot-smoke"))]
        {
            0
        }
    };
    userspace::run_init(
        &tables,
        &mut frames,
        boot.init,
        boot.direct_map_offset,
        scenario,
        display.as_mut(),
    );
    if let Some(display) = &mut display {
        display.update_status("KERNEL READY - NATIVE TASKS COMPLETE", 100);
        serial::println!("MIRROR_FRAMEBUFFER_PRESENTED");
    }

    #[cfg(not(feature = "boot-smoke"))]
    serial::println!("MIRROR_BOOT_OK");
    let mut input_consumer = ps2::Consumer::claim().expect("exclusive input consumer");
    if let Some(display) = display.as_mut().filter(|_| keyboard_ready || mouse_ready) {
        if cfg!(feature = "agent-demo") {
            userspace::run_desktop(
                &tables,
                &mut frames,
                [boot.desktop, boot.agent],
                boot.direct_map_offset,
                display,
                &mut input_consumer,
                cfg!(feature = "boot-smoke"),
            );
        } else {
            userspace::run_shell(
                &tables,
                &mut frames,
                [boot.desktop, boot.app],
                boot.direct_map_offset,
                display,
                &mut input_consumer,
                cfg!(feature = "boot-smoke"),
            );
        }
    } else {
        serial::println!("MIRROR_DESKTOP_SKIPPED reason=missing_display_or_input");
    }

    #[cfg(feature = "boot-smoke")]
    {
        match boot::smoke_mode() {
            boot::SmokeMode::Normal | boot::SmokeMode::User(_) | boot::SmokeMode::BlockIoError => {
                interrupts::arm_expected_page_fault(paging::TEST_ADDRESS)
            }
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
        serial::println!("Mirror idle: timer ticks={}", timer::ticks());
        if keyboard_ready || mouse_ready {
            run_console(&mut display, mouse_ready, &mut input_consumer);
        }
        // SAFETY: Only initialized IRQ sources are unmasked; the heap and
        // descriptors are ready. Keep allocator/table state alive here.
        unsafe { arch::idle() }
    }
}

#[cfg(not(feature = "boot-smoke"))]
fn run_console(
    display: &mut Option<framebuffer::Driver<'_>>,
    mouse_ready: bool,
    input_consumer: &mut ps2::Consumer,
) -> ! {
    assert!(!arch::interrupts_enabled());
    // Discard the bounded queue left by boot. Limit work even if new bytes keep
    // arriving through the controller's deferred-drain path.
    for _ in 0..128 {
        if input_consumer.next_event().is_none() {
            break;
        }
    }
    let mut console = console::Console::new();
    let (width, height) = display.as_ref().map_or((1, 1), |display| {
        let info = display.info();
        (info.width, info.height)
    });
    let mut pointer = pointer::Pointer::new(width, height);
    let mut drawn_cursor = None;
    if let Some(display) = display {
        display.render_console(&console);
        if mouse_ready {
            let (x, y) = pointer.position();
            display.draw_cursor(x, y);
            drawn_cursor = Some((x, y));
        }
    }
    if mouse_ready {
        let (x, y) = pointer.position();
        serial::println!(
            "MIRROR_POINTER_READY x={} y={} width={} height={}",
            x,
            y,
            width,
            height
        );
    }
    serial::println!(
        "MIRROR_CONSOLE_READY input=ps2 display={}",
        u8::from(display.is_some())
    );
    let mut revision = 0_u64;
    loop {
        let mut dirty = false;
        let mut redraw_content = false;
        let mut pointer_changed = false;
        let mut pointer_buttons = None;
        let mut full_batch = true;
        // Keep work per pass bounded. IRQs may append while an event is handled,
        // but only this loop owns console state and the framebuffer writer.
        for _ in 0..32 {
            // One controller consumer preserves keyboard/mouse event order and
            // places a loss barrier before any input after a discarded queue.
            let Some(event) = input_consumer.next_event() else {
                full_batch = false;
                break;
            };
            // SAFETY: IRQ handlers only update independent timer/input state;
            // the console/display are never borrowed or accessed by an IRQ.
            unsafe { arch::restore_interrupts(true) };
            match event {
                ps2::InputEvent::Lost => {
                    console.input_lost();
                    pointer.input_lost();
                    dirty = true;
                    redraw_content = true;
                    serial::println!("MIRROR_CONSOLE_INPUT_LOST");
                }
                ps2::InputEvent::Key(event) => {
                    if let Some(action) = console.process(event) {
                        redraw_content |= matches!(action, console::Action::Submitted { .. });
                        action.log();
                        dirty = true;
                    }
                }
                ps2::InputEvent::Mouse(event) => {
                    let update = pointer.apply(event);
                    pointer_changed |= update.moved;
                    pointer_buttons = Some(event.buttons);
                    let (x, y) = pointer.position();
                    if update.left_pressed
                        && let Some(display) = display
                        && let Some(caret) = display.console_caret_at(&console, x, y)
                    {
                        dirty |= console.place_cursor(caret);
                        serial::println!("MIRROR_POINTER_CLICK x={} y={} caret={}", x, y, caret);
                    }
                }
            }
            arch::disable_interrupts();
        }
        if dirty || pointer_changed {
            // Rendering must not hold off IRQ delivery for a whole frame.
            // SAFETY: Same initialized sources and exclusive display ownership.
            unsafe { arch::restore_interrupts(true) };
            if let Some(display) = display {
                if let Some((x, y)) = drawn_cursor {
                    // Restore from current scene state rather than reading VRAM.
                    display.restore_cursor(&console, x, y);
                }
                if dirty {
                    if redraw_content {
                        display.update_console_content(&console);
                    } else {
                        display.update_console_input(&console);
                    }
                }
                if mouse_ready {
                    let (x, y) = pointer.position();
                    display.draw_cursor(x, y);
                    drawn_cursor = Some((x, y));
                }
            }
            revision = revision.wrapping_add(1);
            serial::println!(
                "MIRROR_CONSOLE_UPDATED revision={} display={} irq={}",
                revision,
                u8::from(display.is_some()),
                keyboard::irq_count()
            );
            arch::disable_interrupts();
        }
        if let Some(buttons) = pointer_buttons {
            let (x, y) = pointer.position();
            serial::println!(
                "MIRROR_POINTER_UPDATED x={} y={} buttons={} irq={}",
                x,
                y,
                buttons,
                mouse::irq_count()
            );
        }
        if dirty || pointer_changed || full_batch {
            continue;
        }
        // SAFETY: Installed IRQ handlers are ready; IF stayed clear since the
        // empty-queue check. STI's interrupt shadow
        // makes the following HLT atomic with enabling delivery; a pending key
        // or timer wakes us instead of being lost before sleep.
        unsafe { core::arch::asm!("sti", "hlt", "cli", options(nostack)) };
    }
}

fn probe_block_device(
    devices: &pci::Inventory,
    frames: &mut memory::FrameAllocator<'_>,
    direct_map: u64,
    inject_error: bool,
) {
    let baseline = frames.stats().expect("frame accounting").allocated_frames;
    // SAFETY: IF is clear; this boot path alone claims the device. Firmware has
    // exited; the sole allocator owns DMA pages and its HHDM remains mapped.
    // No other driver or interrupt handler accesses this transport or buffers.
    let mut driver = match unsafe { block::Driver::open(devices, frames, direct_map) } {
        Ok(Some(driver)) => driver,
        Ok(None) => {
            assert!(!inject_error, "block error probe requires its test disk");
            assert_eq!(frames.stats().unwrap().allocated_frames, baseline);
            serial::println!("MIRROR_BLOCK_SKIPPED reason=no_supported_device");
            return;
        }
        Err(error) => {
            serial::println!("MIRROR_BLOCK_FAILED phase=open error={}", error);
            return;
        }
    };
    let capacity = driver.capacity_sectors();
    let queue_size = driver.queue_size();
    serial::println!(
        "MIRROR_BLOCK_READY sectors={} queue={} readonly={}",
        capacity,
        queue_size,
        u8::from(driver.is_read_only())
    );
    let result = check_block_data(&mut driver, inject_error);
    // SAFETY: IF is clear; no users or handlers retain this device/buffers.
    // Shutdown must observe reset completion before releasing its DMA pages;
    // an unconfirmed reset deliberately quarantines the allocations instead.
    let cleanup = unsafe { driver.shutdown(frames) };
    if let Err(error) = cleanup {
        serial::println!("MIRROR_BLOCK_FAILED phase=shutdown error={}", error);
        return;
    }
    assert_eq!(
        frames.stats().expect("frame accounting").allocated_frames,
        baseline,
        "quiesced block driver leaked DMA pages"
    );
    serial::println!("MIRROR_BLOCK_CLEANUP_OK dma_reclaimed=1");
    match result {
        Ok(Some(reads)) => serial::println!(
            "MIRROR_BLOCK_READ_OK sectors={} reads={} queue={} bounds_rejected=2",
            capacity,
            reads,
            queue_size
        ),
        Ok(None) => serial::println!("MIRROR_BLOCK_PROBE_OK fixture=0 reads=1"),
        Err(error) => serial::println!("MIRROR_BLOCK_FAILED phase=read error={}", error),
    }
}

/// Read arbitrary disks only for identification; extensive data checks run only
/// on the dedicated fixture produced by scripts/common.ps1, never on a filesystem.
fn check_block_data(
    driver: &mut block::Driver,
    inject_error: bool,
) -> Result<Option<usize>, block::BlockError> {
    let mut bytes = [0_u8; 512];
    driver.read_sector(0, &mut bytes)?;
    if driver.capacity_sectors() != 2048 || &bytes[8..16] != b"MIRROR\0\x01" {
        assert!(!inject_error, "unexpected block error fixture");
        return Ok(None);
    }
    assert!(driver.is_read_only(), "fixture must be read-only");
    check_fixture_sector(0, &bytes);
    if inject_error {
        bytes.fill(0xcc);
        let result = driver.read_sector(1234, &mut bytes);
        assert!(
            matches!(&result, Err(block::BlockError::IoError)),
            "backend error injection was not observed: {:?}",
            result
        );
        assert_eq!(bytes, [0xcc; 512], "failed read modified caller buffer");
        driver.read_sector(1234, &mut bytes)?;
        check_fixture_sector(1234, &bytes);
        serial::println!("MIRROR_BLOCK_IO_ERROR_OK retry=1 output_preserved=1");
    }
    for sector in [driver.capacity_sectors(), u64::MAX] {
        bytes.fill(0x5a);
        assert!(driver.read_sector(sector, &mut bytes).is_err());
        assert_eq!(bytes, [0x5a; 512], "invalid sector modified caller buffer");
    }
    let mut reads = 1 + usize::from(inject_error);
    for sector in [1, 127, 255, 256, 1023, 2047] {
        driver.read_sector(sector, &mut bytes)?;
        check_fixture_sector(sector, &bytes);
        reads += 1;
    }
    // Cross the ring's slot boundary twice while reusing the same descriptor
    // chain and bounce page. Distinct LBAs reveal stale or misordered data.
    for index in 0..u64::from(driver.queue_size()) * 2 + 3 {
        let sector = (index * 37 + 11) % 2048;
        driver.read_sector(sector, &mut bytes)?;
        check_fixture_sector(sector, &bytes);
        reads += 1;
    }
    Ok(Some(reads))
}

fn check_fixture_sector(sector: u64, bytes: &[u8; 512]) {
    assert_eq!(&bytes[..8], &sector.to_le_bytes(), "wrong sector returned");
    assert_eq!(&bytes[8..16], b"MIRROR\0\x01", "fixture signature mismatch");
    for (offset, &value) in bytes.iter().enumerate().skip(16) {
        let expected = ((sector * 73) ^ (sector >> 8) ^ (offset as u64 * 29) ^ 0xa5) as u8;
        assert_eq!(value, expected, "fixture data mismatch");
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
