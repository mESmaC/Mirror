#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod ahci;
mod apic;
mod arch;
mod block;
mod boot;
mod capability;
mod clock;
mod console;
mod context;
mod cpudata;
mod dma;
mod drivers;
mod elf;
mod fpu;
mod framebuffer;
mod hardening;
mod heap;
mod input;
mod interrupts;
mod ipc;
mod kernel_va;
mod kernel_vm;
mod keyboard;
mod memory;
mod mouse;
mod paging;
mod pci;
mod perf;
mod pointer;
mod ps2;
mod reset;
mod serial;
mod smp;
mod stacks;
mod surface;
mod timer;
mod usb;
mod userspace;
mod vm;
mod vmmouse;
mod windows;
mod xhci;

use core::{mem::ManuallyDrop, panic::PanicInfo};
use limine::memory_map::EntryType;

// Track linker-script edits as a compiler input without a host build script.
const _: &str = include_str!("../linker.ld");

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    arch::disable_interrupts();
    perf::init(boot::perf_command_line());
    serial::init();
    serial::println!("Mirror kernel 0.1.0 | x86_64");
    perf::announce();
    // Trace and smoke kernels announce their test serial output once, before
    // PCI discovery. A default kernel never prints it (spec 0010).
    #[cfg(feature = "test-trace")]
    serial::println!("MIRROR_TRACE_ON");
    interrupts::init();
    // SAFETY: CPL0 with IF clear on the BSP, after interrupts::init's
    // descriptor load zeroed both GS MSRs and before anything reads through
    // GS (spec 0050-D1 (e)). Slot 0 is the BSP's and is installed once.
    assert!(unsafe { cpudata::install(0) }, "BSP per-CPU data install");
    fpu::init();
    hardening::init();
    interrupts::tls_ready();
    #[cfg(feature = "boot-smoke")]
    {
        fpu::self_test();
        cpudata::self_test();
    }

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
    unsafe { heap::init(&mut tables, &mut frames, boot.direct_map_offset) }
        .expect("could not initialize kernel heap");

    // Leave Limine's stack for the guarded BSP stacks. They are mapped before
    // share_with_secondaries seals kernel mappings and before any process root
    // copies the upper half. Limine's stack stays reserved and idle.
    // SAFETY: The BSP runs alone with IF=0 on the active owned root, with the
    // sole allocator. Nothing has sealed kernel mappings or used the arena.
    let bsp_stacks =
        unsafe { stacks::map_bsp(&mut tables, &mut frames) }.expect("could not map the BSP stacks");
    // SAFETY: Once, on the BSP with IF=0 after interrupts::init. Each IST top
    // sits above its own guarded supervisor stack in the shared upper half.
    unsafe { interrupts::install_bsp_ists(bsp_stacks.ist_tops) };
    let early = ManuallyDrop::new(EarlyBoot {
        boot,
        usable_bytes,
        tables,
        frames,
    });
    // SAFETY: The scheduler stack is mapped, zeroed and guarded, and the
    // owned root is active. The noreturn call follows the SysV alignment rule
    // from the page-aligned top and never returns to Limine's stack, which
    // stays mapped, so the continuation can move `early` out of this frame.
    unsafe {
        core::arch::asm!("mov rsp, rsi", "xor ebp, ebp", "call {entry}", "ud2",
            entry = sym continue_boot, in("rdi") &raw const *early,
            in("rsi") bsp_stacks.scheduler_top, options(noreturn));
    }
}

/// Boot state that `_start` hands to `continue_boot` across the stack switch.
struct EarlyBoot {
    boot: boot::BootInfo,
    usable_bytes: u64,
    tables: paging::PageTables,
    frames: memory::FrameAllocator<'static>,
}

/// The rest of BSP boot, on the guarded scheduler stack.
///
/// # Safety
/// Only `_start` calls this, once, through its stack switch. `early` points to
/// a live value on Limine's stack that `_start` never uses or drops again.
unsafe extern "C" fn continue_boot(early: *const EarlyBoot) -> ! {
    // SAFETY: The caller's contract makes this read the value's only move.
    let EarlyBoot {
        boot,
        usable_bytes,
        mut tables,
        mut frames,
    } = unsafe { early.read() };
    assert!(
        stacks::on_bsp_scheduler_stack(),
        "BSP boot continued off its scheduler stack"
    );
    let heap_stats = heap::stats();
    serial::println!(
        "MIRROR_HEAP_READY size={} free={}",
        heap_stats.size,
        heap_stats.free
    );
    let frame_stats = frames.stats().expect("frame accounting");
    serial::println!(
        "MIRROR_FRAME_POOL total={} allocated={} metadata={} free={} refcount_meta={}",
        frame_stats.total_frames,
        frame_stats.allocated_frames,
        frame_stats.metadata_frames,
        frame_stats.recycled_frames + frame_stats.never_allocated_frames,
        // 0054-D3: the refcount array's exact bytes (two per frame, own span).
        frame_stats.total_frames * 2,
    );
    // 0054-D4: the reserve floor, fixed when recycling started and identical
    // in every flavor.
    serial::println!("MIRROR_MEM_RESERVE frames={}", frames.reserve_floor());
    let discovered = acpi::discover(&boot, &mut tables, &mut frames);
    reset::init(discovered.reset);
    let topology = discovered.topology;
    apic::init(topology.as_ref(), &boot, &mut tables, &mut frames);
    clock::init(
        discovered.hpet.as_ref(),
        topology.as_ref(),
        &boot,
        &mut tables,
        &mut frames,
    );
    perf::milestone(mirror_perf_protocol::Event::Clock);
    timer::init();

    let devices = pci::probe();
    drivers::report(&devices);
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
                perf::milestone(mirror_perf_protocol::Event::Display);
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
        ahci::self_test();
        xhci::self_test();
        usb::self_test();
        drivers::self_test();
        keyboard::self_test();
        mouse::self_test();
        vmmouse::self_test();
        ps2::self_test();
        input::self_test();
        pointer::self_test();
        console::self_test();
        reset::policy_self_test();
        assert!(devices.functions > 0, "no PCI devices discovered");
        assert!(
            devices.virtio_block_devices > 0,
            "runner's VirtIO boot disk was not discovered"
        );
        memory::self_test_recycling(&mut frames);
        interrupts::self_test();
        // SAFETY: This is the active, exclusively owned hierarchy and allocator.
        unsafe { paging::self_test(&mut tables, &mut frames) };
        // SAFETY: Same exclusive hierarchy and allocator, before sealing. The
        // paging test has run and nothing else uses the scratch pages.
        unsafe { stacks::self_test(&mut tables, &mut frames) };
        heap::self_test();
        timer::self_test();
        clock::self_test();
        test_heap_with_interrupts();
        elf::self_test();
    }

    probe_ahci_device(
        &devices,
        &boot,
        topology.as_ref(),
        discovered.hpet.as_ref(),
        &mut tables,
        &mut frames,
    );

    probe_xhci_controller(
        &devices,
        &boot,
        topology.as_ref(),
        discovered.hpet.as_ref(),
        &mut tables,
        &mut frames,
    );

    #[cfg(feature = "boot-smoke")]
    match boot::smoke_mode() {
        mode @ (boot::SmokeMode::BlockWrite
        | boot::SmokeMode::BlockWriteError
        | boot::SmokeMode::BlockFlushError
        | boot::SmokeMode::BlockNoFlush) => {
            probe_writable_block_device(&devices, &mut frames, boot.direct_map_offset, mode);
        }
        mode => probe_block_device(
            &devices,
            &mut frames,
            boot.direct_map_offset,
            matches!(mode, boot::SmokeMode::BlockIoError),
        ),
    }
    #[cfg(not(feature = "boot-smoke"))]
    probe_block_device(&devices, &mut frames, boot.direct_map_offset);

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
    // Absolute pointing (spec 0017c) after the mouse result prints and before
    // IRQ12 is unmasked; it reports reason=no_mouse when the mouse failed.
    vmmouse::init(mouse_ready);
    if mouse_ready {
        timer::enable_mouse_irq();
    }
    perf::milestone(mirror_perf_protocol::Event::Input);
    perf::select_session(display.is_some() && (keyboard_ready || mouse_ready));

    // SAFETY: BSP owns the hierarchy and allocator, IF=0, no AP or retained
    // user root has been released. Runtime leaf changes keep these tables pinned.
    unsafe { tables.prepare_shared_window(&mut frames) }.expect("shared kernel window");
    // The kernel VA window's tables are pinned before smp::init seals kernel
    // mappings, and every flavor reports the window once (spec 0050-D4).
    // SAFETY: As above: the BSP alone, IF=0, the active root and its sole
    // allocator, before AP release and before any retained user root.
    unsafe { kernel_va::init(&mut tables, &mut frames) };
    smp::init(&boot, topology.as_ref(), &mut tables, &mut frames);
    // Every flavor checks that each online CPU runs with its own CpuData in
    // GS and reports it once (spec 0050-D4).
    smp::report_cpu_data();
    // Every flavor reports each online CPU's stacks, then the BSP region,
    // then each online CPU's protections and the BSP's speculative-execution
    // exposure.
    smp::report_stacks();
    stacks::report(&tables);
    smp::report_hardening();
    hardening::report_vulnerabilities();
    kernel_vm::init(&tables, &mut frames, boot.direct_map_offset);
    #[cfg(feature = "boot-smoke")]
    kernel_vm::heap_window_self_test(&tables, &mut frames, boot.direct_map_offset);
    // SAFETY: The BSP with IF=0 on the active kernel root, after SMP startup,
    // with the sole allocator and its HHDM. No client uses the window yet and
    // no IRQ-needed lock is held.
    #[cfg(feature = "boot-smoke")]
    unsafe {
        kernel_va::self_test(&tables, &mut frames, boot.direct_map_offset)
    };
    grow_kernel_heap(&tables, &mut frames, boot.direct_map_offset, "boot");
    // The process and thread tables are .bss statics; every flavor reports
    // them once, before the first session or the skipped-desktop path.
    userspace::report_scheduler();
    // Only smoke kernels run init's self-test battery. Default and trace
    // kernels start the shell as the first userspace process (spec 0010-D3).
    // The rendezvous and heap-growth phase below run in every flavor.
    #[cfg(feature = "boot-smoke")]
    {
        // 0059-D1: a session's seeding is a heap safe point.
        heap::ensure(&tables, &mut frames, boot.direct_map_offset);
        let scenario = if let boot::SmokeMode::User(value) = boot::smoke_mode() {
            value
        } else {
            0
        };
        // 0059 acceptance test 4: the charge leak probe belongs to the boot's
        // last session drain — the relaunch pair's when no desktop session
        // follows.
        let desktop_follows = display.is_some() && (keyboard_ready || mouse_ready);
        userspace::run_init(
            &tables,
            &mut frames,
            boot.init,
            boot.direct_map_offset,
            scenario,
            display.as_mut(),
            desktop_follows,
        );
        // Init's register probe entered with AC set; while SMAP is active,
        // every entry must have run with AC clear (spec 0016b).
        hardening::report_entry_ac();
    }
    smp::rendezvous("after_userspace", 4);
    grow_kernel_heap(
        &tables,
        &mut frames,
        boot.direct_map_offset,
        "after_userspace",
    );
    if let Some(display) = &mut display {
        display.update_status("KERNEL READY - NATIVE TASKS COMPLETE", 100);
        serial::println!("MIRROR_FRAMEBUFFER_PRESENTED");
    }

    #[cfg(not(feature = "boot-smoke"))]
    serial::println!("MIRROR_BOOT_OK");
    let mut input_consumer = ps2::Consumer::claim().expect("exclusive input consumer");
    if let Some(display) = display.as_mut().filter(|_| keyboard_ready || mouse_ready) {
        // 0059-D1: a session's seeding is a heap safe point.
        heap::ensure(&tables, &mut frames, boot.direct_map_offset);
        userspace::run_shell(
            &tables,
            &mut frames,
            [boot.desktop, boot.app],
            boot.direct_map_offset,
            display,
            &mut input_consumer,
            cfg!(feature = "boot-smoke"),
        );
    } else {
        serial::println!("MIRROR_DESKTOP_SKIPPED reason=missing_display_or_input");
    }
    smp::rendezvous("after_desktop", 4);

    #[cfg(feature = "boot-smoke")]
    {
        // SAFETY: All CPU rendezvous completed, BSP/IF0 owns the active root
        // and frame allocator. The desktop has relinquished its allocations.
        unsafe { heap::capacity_self_test(&tables, &mut frames, boot.direct_map_offset) };
        match boot::smoke_mode() {
            boot::SmokeMode::SmpUnmap => {
                // SAFETY: The runtime probe removed this alias and completed
                // its shootdowns. This boot intentionally faults the last AP.
                unsafe { smp::unmapped_shared_fault_self_test(kernel_vm::PROBE_ADDRESS) };
                kernel_vm::offline_self_test(&tables, &mut frames, boot.direct_map_offset);
            }
            boot::SmokeMode::SmpTlbTimeout => {
                kernel_vm::timeout_self_test(&tables, &mut frames, boot.direct_map_offset);
            }
            _ => {
                smp::guard_fault_self_test();
                kernel_vm::offline_self_test(&tables, &mut frames, boot.direct_map_offset);
            }
        }
        match boot::smoke_mode() {
            boot::SmokeMode::Normal
            | boot::SmokeMode::User(_)
            | boot::SmokeMode::BlockIoError
            | boot::SmokeMode::BlockWrite
            | boot::SmokeMode::BlockWriteError
            | boot::SmokeMode::BlockFlushError
            | boot::SmokeMode::BlockNoFlush
            | boot::SmokeMode::AhciIoError
            | boot::SmokeMode::AhciInitFailure
            | boot::SmokeMode::XhciInitFailure
            | boot::SmokeMode::XhciDeviceFailure
            | boot::SmokeMode::SmpUnmap
            | boot::SmokeMode::TimerCalibration
            | boot::SmokeMode::ClockActivation
            | boot::SmokeMode::Ps2BatTimeout
            | boot::SmokeMode::Ps2StaleScancode
            | boot::SmokeMode::VmmouseError
            | boot::SmokeMode::OomOrder
            | boot::SmokeMode::ClassLimits
            | boot::SmokeMode::SmpTlbTimeout => {
                interrupts::arm_expected_page_fault(paging::TEST_ADDRESS)
            }
            // Every self-test has run; restart instead of the expected #PF
            // (spec 0058a, D11). QEMU's -no-reboot ends the boot.
            boot::SmokeMode::Reset(_) => reset::restart(reset::Cause::Test),
            boot::SmokeMode::PageFault => {
                serial::println!("MIRROR_TEST_EXPECT_FATAL vector=14");
            }
            boot::SmokeMode::InvalidOpcode => {
                serial::println!("MIRROR_TEST_EXPECT_FATAL vector=6");
                // SAFETY: Deliberate smoke-test #UD. The installed handler must
                // report it and terminate the guest; no Rust execution resumes.
                unsafe { core::arch::asm!("ud2", options(noreturn)) };
            }
            boot::SmokeMode::KernelStackOverflow => {
                serial::println!("MIRROR_TEST_EXPECT_FATAL vector=8");
                // SAFETY: Deliberate smoke-test #DF on the BSP. Userspace has
                // finished, so no user context is active and the RSP0 stack is
                // idle; the probe checks that IF is clear. The #DF handler on
                // IST1 must report the fault and terminate the guest.
                unsafe { stacks::overflow_rsp0() };
            }
            boot::SmokeMode::ThreadStackOverflow => {
                // Spec 0050, acceptance test 3: a thread kernel stack from
                // the kernel VA window, with its unmapped guard page below.
                // SAFETY: The BSP with IF=0 on the active kernel root, with
                // the sole allocator and its HHDM; no window guard is held.
                let stack = unsafe {
                    kernel_va::allocate_stack(&tables, &mut frames, boot.direct_map_offset)
                }
                .expect("thread stack for the overflow probe");
                serial::println!("MIRROR_KSTACK_GUARD guard={:#x}", stack.guard_below());
                serial::println!("MIRROR_TEST_EXPECT_FATAL vector=8");
                // SAFETY: Deliberate smoke-test #DF on the BSP. Userspace has
                // finished, so no user context is active, IF is clear, and
                // this freshly allocated stack is used by nothing else; its
                // guard page below is unmapped. The #DF handler on IST1 must
                // report the fault and terminate the guest.
                unsafe { stacks::overflow(stack.top()) };
            }
            mode @ (boot::SmokeMode::KernelSmep | boot::SmokeMode::KernelSmap) => {
                serial::println!("MIRROR_TEST_EXPECT_FATAL vector=14");
                let probe = if matches!(mode, boot::SmokeMode::KernelSmep) {
                    hardening::UserAccessProbe::Execute
                } else {
                    hardening::UserAccessProbe::Read
                };
                // SAFETY: Deliberate smoke-test #PF on the BSP. Userspace and
                // the desktop have finished, so no user context is active; the
                // kernel root is active, `frames` is its unique allocator, and
                // the probe checks that IF is clear. The #PF handler must
                // report the fault and terminate the guest.
                unsafe {
                    hardening::user_access_probe(
                        probe,
                        &tables,
                        &mut frames,
                        boot.direct_map_offset,
                    )
                };
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
        // After the shell or desktop exits, the tick count dates the user's
        // exit keystroke. Input-dependent timing stays out of default kernels
        // (spec 0010-D4).
        #[cfg(feature = "test-trace")]
        serial::println!("Mirror idle: timer ticks={}", timer::ticks());
        if keyboard_ready || mouse_ready {
            run_console(&mut display, mouse_ready, &mut input_consumer, &devices);
        }
        // SAFETY: Only initialized IRQ sources are unmasked; the heap and
        // descriptors are ready. Keep allocator/table state alive here.
        unsafe { arch::idle() }
    }
}

fn grow_kernel_heap(
    tables: &paging::PageTables,
    frames: &mut memory::FrameAllocator<'_>,
    offset: u64,
    phase: &str,
) {
    #[cfg(feature = "boot-smoke")]
    // SAFETY: BSP, IF=0, active kernel root and exclusive allocator; APs only
    // service bounded IPI commands and IRQ handlers never allocate.
    let result =
        Ok::<_, heap::HeapError>(unsafe { heap::growth_self_test(tables, frames, offset, phase) });
    #[cfg(not(feature = "boot-smoke"))]
    // SAFETY: Same BSP ownership and quiescent uncommitted tail as above. No
    // allocator or IRQ-needed lock is held while shootdowns enable interrupts.
    let result = unsafe { heap::grow(tables, frames, offset, heap::MAX_GROW_PAGES) };
    match result {
        Ok(stats) => serial::println!(
            "MIRROR_HEAP_GROWN phase={} size={} used={} free={} max={} added_pages={}",
            phase,
            stats.size,
            stats.used,
            stats.free,
            heap::HEAP_MAX_SIZE,
            heap::MAX_GROW_PAGES
        ),
        Err(error) => serial::println!(
            "MIRROR_HEAP_GROW_SKIPPED phase={} reason={:?}",
            phase,
            error
        ),
    }
}

#[cfg(not(feature = "boot-smoke"))]
fn run_console(
    display: &mut Option<framebuffer::Driver<'_>>,
    mouse_ready: bool,
    input_consumer: &mut ps2::Consumer,
    devices: &pci::Inventory,
) -> ! {
    assert!(!arch::interrupts_enabled());
    // Discard the bounded backlog left by boot: up to 64 events per device,
    // each behind one pending loss marker. Limit work even if new bytes keep
    // arriving through the controller's deferred-drain path.
    for _ in 0..ps2::MAX_BACKLOG {
        if input_consumer.next_event().is_none() {
            break;
        }
    }
    let mut console = console::Console::with_devices(devices);
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
    // Per-input markers and their bookkeeping exist only in trace kernels. A
    // default kernel prints nothing in response to typing or pointer motion
    // (spec 0010-D4).
    #[cfg(feature = "test-trace")]
    let mut revision = 0_u64;
    loop {
        let mut dirty = false;
        let mut redraw_content = false;
        let mut pointer_changed = false;
        #[cfg(feature = "test-trace")]
        let mut pointer_buttons = None;
        let mut full_batch = true;
        // Keep work per pass bounded. IRQs may append while an event is handled,
        // but only this loop owns console state and the framebuffer writer.
        for _ in 0..32 {
            // One controller consumer preserves keyboard/mouse event order and
            // places each device's loss marker before that device's later input.
            let Some(event) = input_consumer.next_event() else {
                full_batch = false;
                break;
            };
            // SAFETY: IRQ handlers only update independent timer/input state;
            // the console/display are never borrowed or accessed by an IRQ.
            unsafe { arch::restore_interrupts(true) };
            let pointer_update = match event {
                // Each loss resets only its own device's state (spec 0017a-D3):
                // the unsent line survives a pointer loss, and click arming
                // survives a keyboard loss.
                ps2::InputEvent::Lost {
                    keyboard,
                    pointer: pointer_lost,
                } => {
                    if keyboard {
                        console.input_lost();
                        dirty = true;
                        redraw_content = true;
                        serial::println!("MIRROR_CONSOLE_INPUT_LOST device=keyboard");
                    }
                    if pointer_lost {
                        pointer.input_lost();
                        serial::println!("MIRROR_CONSOLE_INPUT_LOST device=pointer");
                    }
                    None
                }
                ps2::InputEvent::Key(event) => {
                    if let Some(action) = console.process(event) {
                        redraw_content |= matches!(action, console::Action::Submitted { .. });
                        #[cfg(feature = "test-trace")]
                        action.log();
                        dirty = true;
                    }
                    None
                }
                ps2::InputEvent::Mouse(motion) => Some((pointer.apply(motion), motion.buttons)),
                // A vmmouse position places the pointer on the framebuffer
                // (spec 0017-D14) and shares click arming with motion.
                ps2::InputEvent::Position(position) => Some((
                    pointer.place(position.x, position.y, position.buttons),
                    position.buttons,
                )),
            };
            if let Some((update, buttons)) = pointer_update {
                pointer_changed |= update.moved;
                #[cfg(feature = "test-trace")]
                {
                    pointer_buttons = Some(buttons);
                }
                #[cfg(not(feature = "test-trace"))]
                let _ = buttons;
                let (x, y) = pointer.position();
                if update.left_pressed
                    && let Some(display) = display
                    && let Some(caret) = display.console_caret_at(&console, x, y)
                {
                    dirty |= console.place_cursor(caret);
                    #[cfg(feature = "test-trace")]
                    serial::println!("MIRROR_POINTER_CLICK x={} y={} caret={}", x, y, caret);
                }
            }
            arch::disable_interrupts();
            // Input after `restart` stays queued: redraw, then restart.
            if console.restart_requested() {
                full_batch = false;
                break;
            }
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
            #[cfg(feature = "test-trace")]
            {
                revision = revision.wrapping_add(1);
                serial::println!(
                    "MIRROR_CONSOLE_UPDATED revision={} display={} irq={}",
                    revision,
                    u8::from(display.is_some()),
                    keyboard::irq_count()
                );
            }
            arch::disable_interrupts();
        }
        #[cfg(feature = "test-trace")]
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
        // The redraw above showed "Restarting." (spec 0058a, D9). The shell
        // and its apps have exited and released their memory, the device
        // probes have quiesced their DMA, and the APs only idle (D10).
        if console.restart_requested() {
            reset::restart(reset::Cause::Console);
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

/// Probe USB host commands and bounded device descriptors before publishing
/// kernel page tables. Device slots and DMA end before native userspace starts.
fn probe_xhci_controller(
    devices: &pci::Inventory,
    boot: &boot::BootInfo,
    topology: Option<&mirror_acpi::Topology>,
    hpet: Option<&mirror_acpi::Hpet>,
    tables: &mut paging::PageTables,
    frames: &mut memory::FrameAllocator<'_>,
) {
    let baseline = frames.stats().expect("frame accounting").allocated_frames;
    let table_baseline = tables.owned_table_count();
    // SAFETY: BSP, IF=0, active private kernel root and exclusive allocator.
    // No AP, user root, IRQ handler, or other driver owns the xHCI controller.
    let opened = unsafe { xhci::Driver::open(devices, boot, topology, hpet, tables, frames) };
    #[cfg(feature = "boot-smoke")]
    let opened = if matches!(boot::smoke_mode(), boot::SmokeMode::XhciInitFailure) {
        assert!(
            matches!(opened, Err(xhci::XhciError::InjectedInitialization)),
            "xHCI initialization failure was not exercised"
        );
        assert_eq!(
            frames.stats().unwrap().allocated_frames,
            baseline + tables.owned_table_count() - table_baseline,
            "xHCI initialization failure leaked DMA"
        );
        serial::println!("MIRROR_XHCI_INIT_ERROR_OK dma_reclaimed=1");
        // SAFETY: Failed initialization halted/reset the controller, released
        // its claim, and reclaimed DMA before this fresh, exclusive open.
        unsafe { xhci::Driver::open(devices, boot, topology, hpet, tables, frames) }
    } else {
        opened
    };
    let mut driver = match opened {
        Ok(Some(driver)) => driver,
        Ok(None) => {
            assert_eq!(
                frames.stats().unwrap().allocated_frames,
                baseline + tables.owned_table_count() - table_baseline,
                "absent xHCI probe leaked DMA"
            );
            serial::println!("MIRROR_XHCI_SKIPPED reason=no_controller");
            return;
        }
        Err(xhci::XhciError::ConnectedPorts) => {
            assert_eq!(
                frames.stats().unwrap().allocated_frames,
                baseline + tables.owned_table_count() - table_baseline,
                "connected-port xHCI refusal leaked DMA"
            );
            serial::println!("MIRROR_XHCI_SKIPPED reason=connected_ports");
            return;
        }
        Err(xhci::XhciError::ClockRequired) => {
            assert_eq!(
                frames.stats().unwrap().allocated_frames,
                baseline + tables.owned_table_count() - table_baseline,
                "missing-clock xHCI refusal leaked DMA"
            );
            serial::println!("MIRROR_XHCI_SKIPPED reason=clock_required");
            return;
        }
        Err(error) => {
            serial::println!("MIRROR_XHCI_FAILED phase=open error={:?}", error);
            return;
        }
    };
    serial::println!(
        "MIRROR_XHCI_READY ports={} slots={}",
        driver.max_ports(),
        driver.max_slots()
    );
    // Each no-op must produce a matching successful hardware completion. This
    // count crosses both ring boundaries twice, exercising both cycle values.
    let mut completed = 0;
    for _ in 0..520 {
        match driver.noop() {
            Ok(()) => completed += 1,
            Err(error) => {
                serial::println!("MIRROR_XHCI_FAILED phase=command error={:?}", error);
                break;
            }
        }
    }
    if completed == 520 {
        serial::println!(
            "MIRROR_XHCI_COMMAND_OK completed=520 command_wraps={} event_wraps={}",
            driver.command_wraps(),
            driver.event_wraps()
        );
        let enumerated = driver.enumerate();
        #[cfg(feature = "boot-smoke")]
        let enumerated = if matches!(boot::smoke_mode(), boot::SmokeMode::XhciDeviceFailure) {
            assert!(
                matches!(enumerated, Err(xhci::XhciError::InjectedDeviceFailure)),
                "USB addressed-device failure was not exercised"
            );
            // SAFETY: This caller owns the failed slot and all its DMA. Full
            // controller reset must retire every device access before reuse.
            unsafe { driver.shutdown(tables, frames) }.expect("USB device failure cleanup");
            assert_eq!(
                frames.stats().unwrap().allocated_frames,
                baseline + tables.owned_table_count() - table_baseline,
                "USB device failure leaked DMA"
            );
            serial::println!("MIRROR_USB_DEVICE_ERROR_OK dma_reclaimed=1");
            // SAFETY: Shutdown revoked every old slot/ring and released the
            // controller claim; the same BSP/root/allocator remains exclusive.
            driver = unsafe { xhci::Driver::open(devices, boot, topology, hpet, tables, frames) }
                .expect("USB device failure reopen")
                .expect("USB controller disappeared after reset");
            driver.enumerate()
        } else {
            enumerated
        };
        match enumerated {
            Ok(Some(descriptors)) => {
                if let Err(error) = report_usb_device(&descriptors) {
                    serial::println!("MIRROR_USB_FAILED phase=descriptor error={:?}", error);
                }
            }
            Ok(None) => serial::println!("MIRROR_USB_SKIPPED reason=no_device"),
            Err(error) => serial::println!("MIRROR_USB_FAILED phase=enumerate error={:?}", error),
        }
    }
    // SAFETY: This caller owns every command and DMA alias. Shutdown must
    // quiesce the controller before releasing any device-visible allocation.
    if let Err(error) = unsafe { driver.shutdown(tables, frames) } {
        serial::println!("MIRROR_XHCI_FAILED phase=shutdown error={:?}", error);
        return;
    }
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        baseline + tables.owned_table_count() - table_baseline,
        "xHCI shutdown leaked DMA"
    );
    serial::println!("MIRROR_XHCI_CLEANUP_OK dma_reclaimed=1");
}

fn report_usb_device(descriptors: &xhci::UsbDescriptors) -> Result<(), usb::DescriptorError> {
    let device = usb::parse_device(&descriptors.device, descriptors.speed)?;
    if u16::from(device.max_packet0) != descriptors.max_packet0 {
        return Err(usb::DescriptorError::PacketSize);
    }
    let bytes = descriptors
        .configuration
        .get(..descriptors.configuration_len)
        .ok_or(usb::DescriptorError::Length)?;
    let header = usb::configuration_header(bytes.get(..9).ok_or(usb::DescriptorError::Length)?)?;
    let configuration = usb::parse_configuration(bytes, header.total_length, descriptors.speed)?;
    let speed = match descriptors.speed {
        usb::UsbSpeed::Low => "low",
        usb::UsbSpeed::Full => "full",
        usb::UsbSpeed::High => "high",
    };
    serial::println!(
        "MIRROR_USB_DEVICE port={} slot={} address={} speed={} vid={:04x} pid={:04x} ep0={} configs={}",
        descriptors.port,
        descriptors.slot,
        descriptors.address,
        speed,
        device.vendor,
        device.product,
        device.max_packet0,
        device.num_configurations
    );
    serial::println!(
        "MIRROR_USB_CONFIGURATION value={} interfaces={} bytes={}",
        configuration.value,
        configuration.interfaces,
        configuration.total_length
    );
    match device.boot_hid(&configuration) {
        Ok(interface) => serial::println!(
            "MIRROR_USB_HID_CANDIDATE protocol={} interface={} endpoint={:02x} packet={} interval={}",
            interface.protocol,
            interface.interface,
            interface.endpoint,
            interface.max_packet,
            interface.interval
        ),
        Err(reason) => serial::println!("MIRROR_USB_UNSUPPORTED reason={:?}", reason),
    }
    serial::println!(
        "MIRROR_USB_ENUMERATION_OK transfers={} short_packets={} control_wraps={} slot_released=1",
        descriptors.control_transfers,
        descriptors.short_packets,
        descriptors.control_wraps
    );
    Ok(())
}

/// Probe the standard SATA interface before sharing the kernel page tables.
/// Every disk receives one sector-0 read. Only smoke kernels recognize the
/// known disposable fixture and give it extended checks; default and trace
/// kernels compare nothing (spec 0010-D5). The driver has no disk-write command.
fn probe_ahci_device(
    devices: &pci::Inventory,
    boot: &boot::BootInfo,
    topology: Option<&mirror_acpi::Topology>,
    hpet: Option<&mirror_acpi::Hpet>,
    tables: &mut paging::PageTables,
    frames: &mut memory::FrameAllocator<'_>,
) {
    let baseline = frames.stats().expect("frame accounting").allocated_frames;
    let table_baseline = tables.owned_table_count();
    #[cfg(feature = "boot-smoke")]
    let inject_error = matches!(boot::smoke_mode(), boot::SmokeMode::AhciIoError);
    // SAFETY: BSP, IF=0, active private kernel root and exclusive allocator.
    // No AP, user root, IRQ handler, or other driver owns this SATA controller.
    let opened = unsafe { ahci::Driver::open(devices, boot, topology, hpet, tables, frames) };
    #[cfg(feature = "boot-smoke")]
    let opened = if matches!(boot::smoke_mode(), boot::SmokeMode::AhciInitFailure) {
        assert!(
            matches!(opened, Err(ahci::AhciError::InjectedInitialization)),
            "AHCI initialization failure was not exercised"
        );
        assert_eq!(
            frames.stats().unwrap().allocated_frames,
            baseline + tables.owned_table_count() - table_baseline,
            "AHCI initialization failure leaked DMA"
        );
        serial::println!("MIRROR_AHCI_INIT_ERROR_OK dma_reclaimed=1");
        // SAFETY: The injected failure stopped every engine, released the
        // exclusive claim, and reclaimed all DMA before this second open.
        unsafe { ahci::Driver::open(devices, boot, topology, hpet, tables, frames) }
    } else {
        opened
    };
    let mut driver = match opened {
        Ok(Some(driver)) => driver,
        Ok(None) => {
            #[cfg(feature = "boot-smoke")]
            assert!(!inject_error, "AHCI error test requires a disk");
            assert_eq!(
                frames.stats().unwrap().allocated_frames,
                baseline + tables.owned_table_count() - table_baseline,
                "empty AHCI probe leaked DMA"
            );
            serial::println!("MIRROR_AHCI_SKIPPED reason=no_disk");
            return;
        }
        Err(error) => {
            serial::println!("MIRROR_AHCI_FAILED phase=open error={:?}", error);
            return;
        }
    };
    let capacity = driver.capacity_sectors();
    serial::println!(
        "MIRROR_AHCI_READY sectors={} readonly=1 port={}",
        capacity,
        driver.port()
    );
    #[cfg(feature = "boot-smoke")]
    let result = check_ahci_data(&mut driver, inject_error);
    #[cfg(not(feature = "boot-smoke"))]
    let result = driver.read_sector(0, &mut [0_u8; ahci::SECTOR_SIZE]);
    match result {
        #[cfg(feature = "boot-smoke")]
        Ok(true) => serial::println!(
            "MIRROR_AHCI_READ_OK sectors={} reads=262 bounds_rejected=2",
            capacity
        ),
        // Smoke: any disk but the fixture. Default and trace: every disk.
        Ok(_) => serial::println!("MIRROR_AHCI_PROBE_OK fixture=0 reads=1"),
        Err(error) => serial::println!("MIRROR_AHCI_FAILED phase=read error={:?}", error),
    }
    // SAFETY: No other caller retains controller or DMA access. Shutdown must
    // stop every engine before releasing device-visible memory or MMIO aliases.
    let cleanup = unsafe { driver.shutdown(tables, frames) };
    if let Err(error) = cleanup {
        serial::println!("MIRROR_AHCI_FAILED phase=shutdown error={:?}", error);
        return;
    }
    assert_eq!(
        frames.stats().unwrap().allocated_frames,
        baseline + tables.owned_table_count() - table_baseline,
        "AHCI shutdown leaked DMA"
    );
    serial::println!("MIRROR_AHCI_CLEANUP_OK dma_reclaimed=1");
}

#[cfg(feature = "boot-smoke")]
fn check_ahci_data(driver: &mut ahci::Driver, inject_error: bool) -> Result<bool, ahci::AhciError> {
    let mut bytes = [0_u8; 512];
    driver.read_sector(0, &mut bytes)?;
    if driver.capacity_sectors() != 2048 || &bytes[8..16] != b"MIRROR\0\x01" {
        assert!(!inject_error, "unexpected AHCI error fixture");
        return Ok(false);
    }
    check_fixture_sector(0, &bytes);
    if inject_error {
        bytes.fill(0xcc);
        let result = driver.read_sector(1234, &mut bytes);
        assert!(
            matches!(result, Err(ahci::AhciError::IoError)),
            "AHCI backend error was not reported: {:?}",
            result
        );
        assert_eq!(bytes, [0xcc; 512], "failed AHCI read changed output");
        driver.read_sector(1234, &mut bytes)?;
        check_fixture_sector(1234, &bytes);
        serial::println!("MIRROR_AHCI_IO_ERROR_OK retry=1 output_preserved=1");
    }
    for sector in [driver.capacity_sectors(), u64::MAX] {
        bytes.fill(0x5a);
        assert!(matches!(
            driver.read_sector(sector, &mut bytes),
            Err(ahci::AhciError::SectorOutOfRange)
        ));
        assert_eq!(bytes, [0x5a; 512]);
    }
    for sector in [1, 127, 255, 256, 2047] {
        driver.read_sector(sector, &mut bytes)?;
        check_fixture_sector(sector, &bytes);
    }
    for index in 0..256 {
        let sector = (index * 37 + 11) % 2048;
        driver.read_sector(sector, &mut bytes)?;
        check_fixture_sector(sector, &bytes);
    }
    Ok(true)
}

/// Every VirtIO disk receives one sector-0 read. Only smoke kernels recognize
/// the fixture and check its data; default and trace kernels compare nothing
/// (spec 0010-D5).
fn probe_block_device(
    devices: &pci::Inventory,
    frames: &mut memory::FrameAllocator<'_>,
    direct_map: u64,
    #[cfg(feature = "boot-smoke")] inject_error: bool,
) {
    let baseline = frames.stats().expect("frame accounting").allocated_frames;
    // SAFETY: IF is clear; this boot path alone claims the device. Firmware has
    // exited; the sole allocator owns DMA pages and its HHDM remains mapped.
    // No other driver or interrupt handler accesses this transport or buffers.
    let mut driver = match unsafe {
        block::Driver::open(devices, frames, direct_map, block::Access::ReadOnly)
    } {
        Ok(Some(driver)) => driver,
        Ok(None) => {
            #[cfg(feature = "boot-smoke")]
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
        "MIRROR_BLOCK_READY sectors={} queue={} readonly={} flush={}",
        capacity,
        queue_size,
        u8::from(driver.is_read_only()),
        u8::from(driver.supports_flush())
    );
    #[cfg(feature = "boot-smoke")]
    let result = check_block_data(&mut driver, inject_error);
    #[cfg(not(feature = "boot-smoke"))]
    let result = driver.read_sector(0, &mut [0_u8; block::SECTOR_SIZE]);
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
        #[cfg(feature = "boot-smoke")]
        Ok(Some(reads)) => serial::println!(
            "MIRROR_BLOCK_READ_OK sectors={} reads={} queue={} bounds_rejected=2",
            capacity,
            reads,
            queue_size
        ),
        // Smoke: any disk but the fixture. Default and trace: every disk.
        Ok(_) => serial::println!("MIRROR_BLOCK_PROBE_OK fixture=0 reads=1"),
        Err(error) => serial::println!("MIRROR_BLOCK_FAILED phase=read error={}", error),
    }
}

/// Read arbitrary disks only for identification; extensive data checks run only
/// on the dedicated fixture produced by scripts/common.ps1, never on a filesystem.
#[cfg(feature = "boot-smoke")]
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
    assert!(matches!(
        driver.write_sector(0, &bytes),
        Err(block::BlockError::ReadOnly)
    ));
    assert!(matches!(driver.flush(), Err(block::BlockError::ReadOnly)));
    assert!(
        !driver.needs_flush(),
        "read-only rejection dirtied the device"
    );
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

#[cfg(feature = "boot-smoke")]
fn check_fixture_sector(sector: u64, bytes: &[u8; 512]) {
    assert_eq!(&bytes[..8], &sector.to_le_bytes(), "wrong sector returned");
    assert_eq!(&bytes[8..16], b"MIRROR\0\x01", "fixture signature mismatch");
    for (offset, &value) in bytes.iter().enumerate().skip(16) {
        let expected = ((sector * 73) ^ (sector >> 8) ^ (offset as u64 * 29) ^ 0xa5) as u8;
        assert_eq!(value, expected, "fixture data mismatch");
    }
}

/// Only the explicitly selected smoke modes may request write authority. The
/// runner supplies a fresh disposable fixture, separate from firmware storage.
#[cfg(feature = "boot-smoke")]
fn probe_writable_block_device(
    devices: &pci::Inventory,
    frames: &mut memory::FrameAllocator<'_>,
    direct_map: u64,
    mode: boot::SmokeMode,
) {
    assert!(!arch::interrupts_enabled());
    let baseline = frames.stats().expect("frame accounting").allocated_frames;
    let sectors = [0_u64, 1234, 2047];
    let neighbors = [1_u64, 1233, 1235, 2046];
    let mut bytes = [0_u8; 512];

    // SAFETY: IF is clear; this boot path alone claims the disposable device.
    // Firmware has exited, the allocator owns DMA pages, and HHDM is mapped.
    let mut driver =
        unsafe { block::Driver::open(devices, frames, direct_map, block::Access::ReadOnly) }
            .expect("open write fixture without write authority")
            .expect("missing writable fixture");
    assert_eq!(driver.capacity_sectors(), 2048);
    assert!(
        !driver.is_read_only(),
        "write fixture is hardware read-only"
    );
    driver
        .read_sector(0, &mut bytes)
        .expect("identify write fixture");
    check_fixture_sector(0, &bytes);
    let denied_payload = writable_fixture_sector(0, 1);
    assert!(matches!(
        driver.write_sector(0, &denied_payload),
        Err(block::BlockError::WriteDenied)
    ));
    assert_eq!(denied_payload, writable_fixture_sector(0, 1));
    assert!(matches!(
        driver.flush(),
        Err(block::BlockError::WriteDenied)
    ));
    assert!(
        !driver.needs_flush(),
        "denied operations dirtied the device"
    );
    driver
        .read_sector(0, &mut bytes)
        .expect("read fixture after denied write");
    check_fixture_sector(0, &bytes);
    // SAFETY: No request, reference, or handler retains these DMA buffers.
    unsafe { driver.shutdown(frames) }.expect("reset read-only policy probe");
    assert_eq!(frames.stats().unwrap().allocated_frames, baseline);
    serial::println!("MIRROR_BLOCK_POLICY_OK writes_denied=1 flush_denied=1");

    // SAFETY: The previous driver confirmed reset before releasing its DMA.
    // This sole owner now explicitly requests writes to the identified fixture.
    let mut driver =
        unsafe { block::Driver::open(devices, frames, direct_map, block::Access::ReadWrite) }
            .expect("open writable fixture")
            .expect("writable fixture disappeared");
    let queue = driver.queue_size();
    assert_eq!(driver.capacity_sectors(), 2048);
    assert!(!driver.is_read_only());
    assert!(!driver.needs_flush());
    driver
        .read_sector(0, &mut bytes)
        .expect("recheck write fixture");
    check_fixture_sector(0, &bytes);
    serial::println!(
        "MIRROR_BLOCK_WRITABLE_READY sectors={} queue={} readonly=0 flush={}",
        driver.capacity_sectors(),
        queue,
        u8::from(driver.supports_flush())
    );

    if matches!(mode, boot::SmokeMode::BlockNoFlush) {
        assert!(
            !driver.supports_flush(),
            "flush feature unexpectedly offered"
        );
        let payload = writable_fixture_sector(0, 1);
        assert!(matches!(
            driver.write_sector(0, &payload),
            Err(block::BlockError::FlushUnsupported)
        ));
        assert_eq!(payload, writable_fixture_sector(0, 1));
        assert!(matches!(
            driver.flush(),
            Err(block::BlockError::FlushUnsupported)
        ));
        assert!(!driver.needs_flush());
        for sector in sectors.into_iter().chain(neighbors) {
            driver
                .read_sector(sector, &mut bytes)
                .expect("read no-flush fixture");
            check_fixture_sector(sector, &bytes);
        }
        // SAFETY: All requests completed; rejected writes published no DMA.
        unsafe { driver.shutdown(frames) }.expect("reset no-flush fixture");
        assert_eq!(frames.stats().unwrap().allocated_frames, baseline);
        serial::println!("MIRROR_BLOCK_NO_FLUSH_OK writes_denied=1 dirty=0");
        serial::println!("MIRROR_BLOCK_CLEANUP_OK dma_reclaimed=1");
        return;
    }
    assert!(
        driver.supports_flush(),
        "write probe requires negotiated flush"
    );

    if matches!(mode, boot::SmokeMode::BlockWriteError) {
        let payload = writable_fixture_sector(1234, 0);
        assert!(matches!(
            driver.write_sector(1234, &payload),
            Err(block::BlockError::IoError)
        ));
        assert_eq!(payload, writable_fixture_sector(1234, 0));
        assert!(
            driver.needs_flush(),
            "submitted write error lost dirty state"
        );
        driver
            .write_sector(1234, &payload)
            .expect("retry failed write");
        driver
            .read_sector(1234, &mut bytes)
            .expect("read retried write");
        assert_eq!(bytes, payload);
        assert!(driver.needs_flush(), "read cleared pending writes");
        serial::println!("MIRROR_BLOCK_WRITE_ERROR_OK retry=1 input_preserved=1");
    }

    let writes = u64::from(queue) * 2 + 3;
    let mut generations = [0_u64; 3];
    for index in 0..writes {
        let selected = index as usize % sectors.len();
        let sector = sectors[selected];
        let generation = index + 1;
        let payload = writable_fixture_sector(sector, generation);
        driver
            .write_sector(sector, &payload)
            .expect("write fixture sector");
        assert_eq!(payload, writable_fixture_sector(sector, generation));
        assert!(
            driver.needs_flush(),
            "successful write was not marked dirty"
        );
        driver
            .read_sector(sector, &mut bytes)
            .expect("read written fixture sector");
        assert_eq!(bytes, payload, "written sector did not round-trip");
        assert!(driver.needs_flush(), "read incorrectly cleared dirty state");
        generations[selected] = generation;
    }
    let inject_flush_error = matches!(mode, boot::SmokeMode::BlockFlushError);
    if inject_flush_error {
        assert!(matches!(driver.flush(), Err(block::BlockError::IoError)));
        assert!(driver.needs_flush(), "failed flush cleared dirty state");
    }
    driver.flush().expect("flush written fixture sectors");
    assert!(
        !driver.needs_flush(),
        "successful flush did not clear dirty state"
    );
    if inject_flush_error {
        serial::println!("MIRROR_BLOCK_FLUSH_ERROR_OK retry=1 dirty_preserved=1");
    }
    serial::println!("MIRROR_BLOCK_FLUSH_OK dirty=0");

    // Rejections must not make a clean device dirty or touch caller memory.
    for sector in [driver.capacity_sectors(), u64::MAX] {
        let payload = [0x5a; 512];
        assert!(matches!(
            driver.write_sector(sector, &payload),
            Err(block::BlockError::SectorOutOfRange)
        ));
        assert_eq!(payload, [0x5a; 512]);
        assert!(!driver.needs_flush());
    }
    for sector in neighbors {
        driver
            .read_sector(sector, &mut bytes)
            .expect("read untouched neighbor");
        check_fixture_sector(sector, &bytes);
    }
    serial::println!(
        "MIRROR_BLOCK_WRITE_OK sectors=2048 writes={} queue={} bounds_rejected=2",
        writes,
        queue
    );

    // A reset must report pending writes, while still quiescing and reclaiming
    // DMA. Rewriting identical bytes preserves the deterministic final image.
    let payload = writable_fixture_sector(sectors[0], generations[0]);
    driver
        .write_sector(sectors[0], &payload)
        .expect("dirty shutdown probe");
    assert!(driver.needs_flush());
    // SAFETY: All requests completed, and this driver still exclusively owns
    // the device. Successful reset releases DMA even when durability is pending.
    let shutdown = unsafe { driver.shutdown(frames) };
    assert!(matches!(shutdown, Err(block::BlockError::UnflushedWrites)));
    assert_eq!(frames.stats().unwrap().allocated_frames, baseline);
    serial::println!("MIRROR_BLOCK_DIRTY_SHUTDOWN_OK dma_reclaimed=1");

    // SAFETY: The prior driver confirmed reset and released its entire DMA set.
    let mut driver =
        unsafe { block::Driver::open(devices, frames, direct_map, block::Access::ReadWrite) }
            .expect("reopen fixture for explicit post-reset flush")
            .expect("write fixture disappeared after reset");
    // Reset alone does not establish durability. Send a real flush even though
    // the freshly opened session has not itself submitted a write.
    driver.flush().expect("flush after dirty reset");
    assert!(!driver.needs_flush());
    // SAFETY: Completed flush, exclusive device ownership, no retained DMA user.
    unsafe { driver.shutdown(frames) }.expect("clean shutdown after explicit flush");
    assert_eq!(frames.stats().unwrap().allocated_frames, baseline);

    // SAFETY: The clean reset completed; this final owner requests reads only.
    let mut driver =
        unsafe { block::Driver::open(devices, frames, direct_map, block::Access::ReadOnly) }
            .expect("reopen fixture for persistence checks")
            .expect("write fixture disappeared during persistence check");
    for (index, sector) in sectors.into_iter().enumerate() {
        driver
            .read_sector(sector, &mut bytes)
            .expect("read persisted sector");
        assert_eq!(bytes, writable_fixture_sector(sector, generations[index]));
    }
    for sector in neighbors {
        driver
            .read_sector(sector, &mut bytes)
            .expect("read persisted neighbor");
        check_fixture_sector(sector, &bytes);
    }
    assert!(!driver.needs_flush());
    // SAFETY: Only completed read requests have used this final DMA allocation.
    unsafe { driver.shutdown(frames) }.expect("reset persistence-check driver");
    assert_eq!(frames.stats().unwrap().allocated_frames, baseline);
    serial::println!("MIRROR_BLOCK_PERSIST_OK sectors=3 neighbors=4");
    serial::println!("MIRROR_BLOCK_CLEANUP_OK dma_reclaimed=1");
}

#[cfg(feature = "boot-smoke")]
fn writable_fixture_sector(sector: u64, generation: u64) -> [u8; 512] {
    let mut bytes = [0_u8; 512];
    bytes[..8].copy_from_slice(&sector.to_le_bytes());
    bytes[8..16].copy_from_slice(b"MIRRORW1");
    bytes[16..24].copy_from_slice(&generation.to_le_bytes());
    for (offset, value) in bytes.iter_mut().enumerate().skip(24) {
        *value = ((sector * 73) ^ (generation * 131) ^ (offset as u64 * 29) ^ 0x5a) as u8;
    }
    bytes
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
    if smp::is_secondary() {
        smp::secondary_fault(u64::MAX, u64::MAX, 0, 0);
    }
    serial::println!("MIRROR_PANIC: {}", info);

    #[cfg(feature = "boot-smoke")]
    arch::exit_qemu(false);

    #[cfg(not(feature = "boot-smoke"))]
    arch::halt()
}
