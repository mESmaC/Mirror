# Mirror

Mirror is an independent operating system being built around a new Rust kernel and native userspace, designed for people and AI agents to use the same computer efficiently. Mirror owns the system interfaces for processes, memory, scheduling, devices, and native services. Windows and Linux are application compatibility targets: separate compatibility implementations will translate their application interfaces onto Mirror. Other goals are fast and controllable automation and coordination of local inference with the rest of the system.

**Status:** the freestanding Rust kernel boots through UEFI/Limine in QEMU with its own exception descriptors and page tables, reusable physical pages, a 1 MiB kernel heap, and timer interrupts. It preemptively schedules two isolated native init processes, then runs a separate persistent ring-3 desktop prototype with keyboard editing and pointer interaction. Sixteen native syscalls provide process-local capability handles, bounded message channels, sleeping, yielding, checked pixel uploads, and an exclusive input stream. Init probes have a finite user-tick budget; the desktop receives a recurring allowance of 32 delivered user-execution ticks per 100 delivered PIT ticks. Termination releases handles and private memory and returns display/input ownership to the diagnostic console. The kernel also inventories PCI functions and reads sectors through a selected legacy VirtIO block driver with owned DMA buffers. A complete desktop environment, filesystems, broader driver ports, application compatibility, unified resource management, and inference services remain future work. The broader designs below are proposals; performance improvements must be demonstrated with measurements.

**Validation:** formatting, Clippy, all 32 debug/release smoke boots, and normal desktop-then-console captures in both profiles pass. Coverage includes process isolation, IPC and scheduling, recurring allowances, checked input copies, pixel uploads, storage error recovery and DMA cleanup, device-absence fallbacks, ordered input and loss recovery, bounded editing, and cursor restoration. QMP captures exercise real keyboard and mouse delivery through the guest devices. An additional debug capture passes at 640 by 480 pixels. Validation runs in QEMU; physical hardware remains untested.

## Build and boot the kernel

The kernel, native init, and desktop target `x86_64-unknown-none` using stable Rust (minimum 1.88; developed with 1.96.0). Rustup installs the target and components declared in `rust-toolchain.toml`. Cargo dependencies are locked, and the boot scripts download Limine v12.9.3 from its official release and verify the archive's pinned SHA-256. The Rust protocol bindings are pinned to 0.5.0, which supports stable Rust and Limine base revision 3. `scripts/build.ps1` builds each package with its own linker settings using Rust's bundled linker; no host C linker is required. The workspace defaults to the kernel, so use the script to build and stage all three executables.

Both native programs use the small code model for their low virtual addresses. Their optimized builds disable LTO to avoid merging incompatible code-model metadata with Rust's prebuilt `core`; kernel release LTO remains enabled. A dedicated userspace target with a matching rebuilt `core` remains future build-system work.

From the repository root in PowerShell:

```powershell
# Compile the kernel, native init, and desktop, then stage their UEFI boot files.
.\scripts\build.ps1

# Run formatting, Clippy, 32 smoke cases, and debug/release desktop/console captures.
.\scripts\test.ps1

# Build the smoke-test kernel, boot it, and verify its serial results and exit code.
.\scripts\run.ps1 -Smoke

# Verify the optimized kernel, including LTO.
.\scripts\run.ps1 -Smoke -Release

# Verify fatal exception diagnostics (intentional failures inside the guest).
.\scripts\run.ps1 -Smoke -Fault PageFault
.\scripts\run.ps1 -Smoke -Fault InvalidOpcode

# Verify containment of userspace protection faults.
.\scripts\run.ps1 -Smoke -Fault UserKernelRead
.\scripts\run.ps1 -Smoke -Fault UserCodeWrite
.\scripts\run.ps1 -Smoke -Fault UserNx
.\scripts\run.ps1 -Smoke -Fault UserGuard
.\scripts\run.ps1 -Smoke -Fault UserPrivileged
.\scripts\run.ps1 -Smoke -Fault UserBadStack

# Verify that timer preemption lets a peer finish beside a CPU-bound process.
.\scripts\run.ps1 -Smoke -Fault UserBudget

# Verify that process exit closes its last sender and wakes a blocked receiver.
.\scripts\run.ps1 -Smoke -Fault IpcPeerExit

# Verify receive timeout while the peer keeps the endpoint open, then recovery.
.\scripts\run.ps1 -Smoke -Fault IpcTimeout

# Verify successful serial-only boot with no display device attached.
.\scripts\run.ps1 -Smoke -NoDisplayDevice

# Verify normal boot when no supported legacy test disk is attached.
.\scripts\run.ps1 -Smoke -NoTestDisk

# Verify boot with the PS/2 controller removed and its input IRQs left masked.
.\scripts\run.ps1 -Smoke -NoKeyboard

# Inject one backend read error; require preserved output and a successful retry.
.\scripts\run.ps1 -Smoke -Fault BlockIoError

# Boot normally into the native desktop. Escape returns to the diagnostic console.
.\scripts\run.ps1

# Open Mirror's native desktop in a QEMU window, with serial diagnostics.
.\scripts\run.ps1 -Graphical

# Boot, type/click through PS/2, check cursor restoration, capture PNGs, and close QEMU.
.\scripts\capture.ps1
.\scripts\capture.ps1 -Release
```

Running requires `qemu-system-x86_64.exe` and a matching pair of EDK II/OVMF firmware files. The runner checks `.tools/qemu/`, then PATH; alternatively pass `-QemuPath`, `-FirmwareCode`, and `-FirmwareVars`, or set `QEMU_PATH`, `OVMF_CODE`, and `OVMF_VARS`. QEMU's bundled `edk2-x86_64-code.fd` pairs with `edk2-i386-vars.fd`. Obtain Windows builds through the [official QEMU download page](https://www.qemu.org/download/#windows); QEMU is a local development prerequisite, not a checked-in dependency.

The PNG capture test requires QEMU 7.1 or newer with PNG screendump support; development uses QEMU 11.1. After checking init completion, block reads, and DMA cleanup, the script waits for the native desktop's first completed frame and input-wait timeout. It uses [QMP input-send-event](https://www.qemu.org/docs/master/interop/qemu-qmp-ref.html#command-input-send-event) to send key presses/releases, relative mouse motion, and separate button edges through the emulated devices. Desktop checks clamp the pointer at both corners, verify exact cursor restoration, type `abcd`, click the caret between `b` and `c`, insert `x`, and activate Apply. The script requires the exact `abxcd` submission and a completed frame after nonzero keyboard and mouse IRQ counts. Escape then closes the desktop with status 0, and handle cleanup must finish before the fallback console becomes ready.

Console checks then exercise overlapping Shift keys, Caps Lock, ignored Ctrl/Alt text, Escape, cursor/backspace/delete editing, and a click that changes `echo abcd` into `echo abxcd`. Exact submitted commands, redraw markers, fresh pointer/button markers, and hardware IRQ counts are required. [QMP screendump](https://www.qemu.org/docs/master/interop/qemu-qmp-ref.html#command-screendump) saves both applications' initial and edited frames; PNG signatures must be valid and edited results must differ, with the desktop cursor returned to its initial position for that comparison. In each application, moving the pointer away must change the image and returning it must reproduce the exact original PNG hash without trails. The script then requests a clean QEMU exit.

Desktop artifacts are `build/desktop.png`, `build/desktop-input.png`, `build/desktop-cursor-away.png`, and `build/desktop-cursor-restored.png`. Console artifacts remain `build/normal-boot.png`, `build/normal-console.png`, `build/normal-pointer.png`, `build/cursor-away.png`, and `build/cursor-restored.png`; `build/normal-boot.{serial,stderr}.log` covers the whole session. Capture runs without a visible window. `-Graphical` opens a window for a normal run; it cannot be combined with `-Smoke` or `-NoDisplayDevice`.

`build/esp/` contains `EFI/BOOT/BOOTX64.EFI`, `limine.conf`, `kernel.elf`, `init.elf`, and `desktop.elf`. Limine supplies both named native executables as boot modules for Mirror's ELF loader. QEMU exposes this directory as a read-only virtual FAT boot disk through a modern-only VirtIO device, which Mirror's initial legacy driver does not claim. A separate 1 MiB legacy VirtIO test disk is attached by default; `-NoTestDisk` omits it and cannot be combined with `-Fault BlockIoError`. The scripts do not format physical disks. Firmware variables are copied into `build/` before use. `.tools/`, `build/`, and `target/` are ignored by Git. This produces an EFI boot tree, not an installer or standalone disk image.

The smoke test has a 30-second boot timeout. It checks frame ownership, alignment, reuse, rejected invalid frees, physical-page contents, returning breakpoints, and mapping/unmapping. Heap checks cover mixed layouts, data preservation, exhaustion, hole reuse/coalescing, rejected oversized layouts, and actual `Box`/`Vec` allocation without leaks. Timer checks require repeated hardware IRQ delivery and acknowledgement. A separate allocation loop requires timer progress while verifying that the heap lock restores enabled interrupts.

Userspace checks cover private page-table permissions and reclamation, bounded ELF validation, rollback after a partially completed load fails, ring-3 execution, initialized data and zeroed BSS, stack writes, rejected syscall pointers and unknown calls, preserved registers, and timer progress while user code runs. Copy tests cross noncontiguous physical pages and require unchanged destination bytes when a later page is absent or read-only. Two instances of init are loaded simultaneously into distinct address spaces. Scheduler checks require repeated dispatch of both processes, yielding, sleeping, resumption, preservation of private data identified by each PID, and complete frame reclamation. PID-specific register canaries and the direction flag are checked across yield; finite arithmetic loops check registers and carry-dependent results while actual timer preemption occurs. Sleep and receive-wait tests check that blocked or idle time does not increase user tick charges; deadline checks cover counter wraparound.

Capability checks cover missing, stale, and foreign handles; denied operations; restricted duplicates; zero-rights handles; table exhaustion; and token exhaustion without reuse. Queue checks cover capacity, FIFO wraparound, message preservation, endpoint reference counts, and scrubbing after the last receiver closes. The native processes exchange three messages, verify that learning a peer's token grants no authority, and recover from short or read-only receive buffers without losing the queued message. Each smoke boot tears down its first pair and launches a fresh normal pair, checking fresh BSS, frame reuse, and complete handle/message cleanup after exit or a contained failure.

Device checks cover PCI address bounds, multifunction enumeration with absent functions, the final bus/slot/function, bridge decoding, missing devices, and bounded inventory overflow. Framebuffer checks use guarded RAM surfaces to exercise the same pixel writer: RGB24/BGR32 encoding, padded rows, clipped and extreme rectangles, text and progress rendering, tiny displays, invalid dimensions/masks/pitch, null and noncanonical addresses, and arithmetic overflow. Guard bytes and row padding must stay unchanged.

Contiguous-allocation checks cover fragmented regions, mixed recycled/never-used pages, the 4 GiB boundary, exhaustion without partial reservation, rejected partial releases, and ordinary allocations skipping live DMA reservations. DMA tests exercise real zeroed RAM, distinct three-page and one-page buffers, checked direct-map rollback, bounded access, and complete frame reclamation. Block checks cover queue layout, descriptor bounds, sector/capacity validation, and completion validation. Fixture reads verify selected sectors and enough requests to cross the queue's ring-slot boundary twice; invalid LBAs must leave the caller's buffer unchanged.

The runner requires `MIRROR_USER_PAGING_OK`, `MIRROR_ELF_TESTS_OK`, `MIRROR_USER_LOAD_ROLLBACK_OK`, `MIRROR_USERSPACE_OK`, `MIRROR_USER_CLEANUP_OK`, `MIRROR_SCHEDULER_OK`, `MIRROR_CAPABILITY_TESTS_OK`, `MIRROR_IPC_TESTS_OK`, and `MIRROR_IPC_OK` along with the earlier subsystem markers. The scheduler marker reports process, switch, yield, sleep, preemption, and idle-tick counts; the IPC marker reports messages, waits, timeouts, and handle reclamation. Nonfatal cases finish with the expected kernel page-fault probe, `MIRROR_PAGE_FAULT_OK`, `MIRROR_BOOT_OK`, and QEMU exit status 33.

Every smoke case also requires `MIRROR_PCI_TESTS_OK`, `MIRROR_PCI_READY`, and `MIRROR_FRAMEBUFFER_TESTS_OK`, including detection of the runner's VirtIO boot disk. Display cases require a PCI display function plus `MIRROR_FRAMEBUFFER_READY` and `MIRROR_FRAMEBUFFER_PRESENTED`. `-NoDisplayDevice` removes the emulated display and instead requires zero PCI display functions and `MIRROR_FRAMEBUFFER_SKIPPED reason=missing`, while completing the ordinary userspace and cleanup checks.

Storage checks require `MIRROR_CONTIGUOUS_TESTS_OK`, `MIRROR_DMA_TESTS_OK`, and `MIRROR_BLOCK_TESTS_OK`. With the fixture attached, `MIRROR_BLOCK_READY`, `MIRROR_BLOCK_READ_OK`, and `MIRROR_BLOCK_CLEANUP_OK` must confirm initialization, verified reads, and frame reclamation after reset. `-NoTestDisk` instead requires `MIRROR_BLOCK_SKIPPED` and rejects unexpected device binding. A `MIRROR_BLOCK_FAILED` diagnostic fails the smoke run.

`-Fault PageFault` and `-Fault InvalidOpcode` select intentional unexpected exceptions through the smoke kernel's boot command line. These cases require the matching exception vector, error code, page-fault address where applicable, and QEMU exit status 35. A panic, timeout, or ordinary successful boot cannot satisfy them. The normal kernel excludes these fault triggers and QEMU exit-port writes.

The five user-fault modes attempt to read supervisor kernel memory, write an RX code page, execute an NX stack page, read a stack guard page, and execute privileged `CLI`. The kernel checks the expected exception vector, error code, and fault address where applicable before reporting `MIRROR_USER_FAULT_CONTAINED`. Each case must recover to the scheduler, allow the peer process to finish, reclaim both processes, and launch a normal pair successfully; a fatal guest exception cannot satisfy these tests.

`-Fault UserBadStack` supplies a noncanonical user stack pointer. The return validator must reject it before IRET, report `MIRROR_USER_RETURN_REJECTED`, and complete the same cleanup and relaunch path without a hardware-fault marker. It also requires QEMU exit status 33.

`-Fault UserBudget` finishes the ordinary user checks, then enters an infinite CPU loop. Timer interrupts must preempt it, its peer must complete before its budget expires, and the kernel must terminate the loop after 32 timer interrupts charged to that process. This requires `MIRROR_USER_BUDGET_CONTAINED`, full cleanup and relaunch, and QEMU exit status 33. A hardware-fault marker cannot satisfy the budget test; an unexpected budget marker fails every other scenario.

`-Fault IpcPeerExit` leaves a duplicate sender open until its owning process exits. Teardown must release that final endpoint reference and wake the peer's blocked receive with the closed-channel result, reported by `MIRROR_IPC_PEER_CLOSED_OK`. `-Fault IpcTimeout` keeps the sender open while its owner sleeps for 110 ticks; the peer must observe exactly one 100-tick receive timeout and then recover when the sender closes, reported by `MIRROR_IPC_TIMEOUT_OK`. Both cases require ordinary process completion, cleanup and relaunch, and QEMU exit status 33. Hardware faults or a scenario marker in the wrong test fail validation.

`-Fault BlockIoError` uses [QEMU's blkdebug backend](https://www.qemu.org/docs/master/devel/testing/blkdebug.html) to inject one EIO on a read overlapping sector 1234 of the fixture. The driver must report the I/O error without changing the caller's buffer, then successfully retry and verify that sector. This requires `MIRROR_BLOCK_IO_ERROR_OK retry=1 output_preserved=1`, ordinary process completion and storage cleanup, and QEMU exit status 33. Other cases reject that marker. Smoke and capture tests also check that the fixture's SHA-256 is unchanged after QEMU exits.

Serial and QEMU diagnostic logs are saved under `build/`. `scripts/test.ps1` runs the thirteen fault/normal cases plus no-display, no-test-disk, and no-controller (`-NoKeyboard`) cases in debug with 128 MiB and release with 256 MiB, for 32 smoke boots. Each profile also builds normal executables and runs the desktop and fallback-console input/capture checks. Logs and screenshots are retained under `build/test-logs/`; the suite ends by staging the normal debug kernel, init, and desktop executables. Build, run, test, and capture commands should run one at a time because they share the staged files.

The current kernel runs one CPU. Its GDT, TSS, 32 exception gates, 16 legacy IRQ gates, a user-callable `INT 0x80` gate, and dedicated double-fault stack are installed before interrupts are enabled. Ring-3 entries use a dedicated kernel stack through TSS.RSP0. The QEMU q35 timer uses the legacy PIC/PIT at approximately 100 Hz. IRQ0 is unmasked first; its handler increments a counter and acknowledges the interrupt without allocation, logging, or locking. Successful keyboard initialization unmasks IRQ1; successful mouse initialization unmasks IRQ12 and the IRQ2 cascade. Both input handlers drain the shared controller before PIC acknowledgement. Timer IRQs delivered during user execution also charge that process's budget and can save its context for another process to run; input IRQs do not charge or schedule processes. The scheduler idles with interrupts enabled when tasks are sleeping, waiting to receive, or throttled. After both init processes exit, normal boots launch the native desktop when display and input are available; closing it returns to the kernel console. These successive consumers use ordered keyboard/mouse events and wake on IRQs while idle. Boots without either input device retain the ordinary idle loop. Ticks measure delivered interrupts, not calibrated elapsed time; long critical sections can lose ticks. APIC/IOAPIC support and broader hardware discovery remain future work.

Paging ownership is established by deep-copying Limine's four-level hierarchy, retaining existing mappings and huge-page leaves, and switching CR3. New heap and scratch mappings are supervisor-only, writable, and non-executable; unmapping invalidates the local TLB. NX and supervisor write protection are enabled. Each user address space owns its lower-half mappings and page tables while sharing supervisor-only upper-half kernel mappings. User text is RX, constants are read-only/NX, and data and stack pages are writable/NX; writable/executable user mappings are rejected. The user-fault suite exercises these restrictions. Existing kernel aliases remain, so user W^X does not establish complete kernel memory protection. Double-fault stack installation is still untested with an induced double fault. Five-level paging is explicitly rejected for now.

Physical allocation begins with a bump allocator, then tracks usable pages with a dynamically sized two-bit bitmap. Previously allocated page tables remain tracked, bitmap pages stay reserved, and released pages can be reused. Metadata currently needs one contiguous usable span. Freeing a page requires ending its uses and withdrawing non-direct-map aliases first; checks reject reserved, misaligned, never-allocated, metadata, and already-free pages. They cannot detect a stale ownership claim after a page has been reallocated. The bootstrap stack and boot responses still belong to Limine, so bootloader memory remains reserved.

The fixed 1 MiB heap has unmapped guard pages and uses `linked_list_allocator` 0.10.6 with an interrupt-state-preserving lock. Blocks are reused and coalesced; heap backing pages stay mapped. Unsupported sizes/alignments return allocation failure before entering the dependency. Interrupt, exception, and syscall handlers must not allocate. The scheduler restores the kernel root before closing a finished process's remaining handles and checking peer wakeups. After both processes finish, it releases their private user trees, including mapped frames and page-table frames, and checks that channel references and messages are gone. Kernel page-table reclamation, heap growth, bounded allocation latency, and a general device framework remain future work.

### PCI inventory and boot framebuffer

The PCI inventory in [kernel/src/pci.rs](kernel/src/pci.rs) reads firmware-configured segment 0 through x86 configuration mechanism 1. Each CF8/CFC address/read transaction preserves the caller's interrupt state on the single boot CPU. Scanning is bounded by 65,536 function addresses and 262,144 configuration reads; the first 32 functions are retained and logged, with full counts and an omitted count. The inventory records identifiers, class, revision, header type, and type-1 bridge bus numbers. It recognizes VirtIO block and GPU identifiers defined by the [Virtio 1.2 specification](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html).

Discovery writes only the host's configuration-address selector; it does not write device configuration, size or assign BARs, enable bus mastering, or start DMA or device IRQs. A separate exclusive legacy-block claim validates the selected function, its assigned I/O BAR, and its conventional capability list before permitting driver-controlled command-register changes. ECAM, extended configuration, additional PCI segments, hotplug synchronization, and general driver binding remain future work. Firmware and Limine still load the kernel, init, and desktop from the modern-only boot disk; Mirror's initial storage driver reads the separate legacy device described below.

The boot display in [kernel/src/framebuffer.rs](kernel/src/framebuffer.rs) accepts the first Limine framebuffer when it supplies RGB24 or RGB32 pixels with three disjoint eight-bit color masks. It validates dimensions, pitch, masks, address arithmetic, and canonical addresses before drawing; dimensions are capped at 8192 each and the surface at 128 MiB. Missing or unsupported framebuffers produce a serial diagnostic and boot continues. The renderer respects row padding, clips drawing to the surface, and uses aligned volatile 32-bit stores for RGB32 when possible and volatile byte stores for other layouts, without allocating a backbuffer or reading uninitialized video memory.

Limine supplies the virtual address with the direct-map offset already applied. Mirror preserves its mapping and cache attributes when taking ownership of paging, reserves the framebuffer and boot-response memory, and keeps the mapping supervisor-only in user address spaces. One local driver exclusively owns device writes; user processes submit checked pixel copies through surface capabilities, and interrupt handlers never draw. These assumptions follow the [Limine protocol's handoff and framebuffer contracts](https://github.com/limine-bootloader/limine-protocol/blob/trunk/PROTOCOL.md#framebuffer-feature).

The boot panel shows Mirror's identity, usable memory reported at boot, and status updated after the native tasks complete. Normal boots then launch the native desktop prototype when display and input are available; its exit or contained fault returns to the diagnostic console. Rendering preserves the boot-selected display mode; GPU acceleration, mode switching, general surface creation, a compositor, and window management remain future work. The full native desktop acceptance criteria appear below.

![Mirror's native boot panel captured from QEMU](docs/images/boot-framebuffer.png)

QEMU capture of the boot panel. Normal boots now continue into the native desktop prototype; the complete desktop environment remains in the roadmap.

### Keyboard, mouse, and diagnostic console

[kernel/src/ps2.rs](kernel/src/ps2.rs) exclusively owns the selected q35 platform's i8042 ports and routes every byte by its AUX source bit, independently of which IRQ arrived. Initialization disables translation, selects and verifies keyboard scan-code set 2, resets the mouse to standard three-byte ID 0 packets, and consumes command replies before activating each decoder. Controller configuration is verified before either device starts reporting. Polling, retries, and draining are bounded; each input IRQ is unmasked only after that device succeeds. Mouse failure can leave a working keyboard available. An absent controller leaves both input IRQs masked and the rest of boot available. This is a fixed platform driver; ACPI discovery, USB input, wheel extensions, hotplug, and physical-PC validation remain future work. The protocol is checked against [QEMU's controller and device implementations](https://github.com/qemu/qemu/tree/v10.1.0/hw/input).

Each input IRQ drains at most 32 bytes without allocation, printing, or rendering. Main context continues deferred draining, with the PIT providing wakeups when another input edge does not arrive. [keyboard.rs](kernel/src/keyboard.rs) decodes make/break and extended sequences into a 64-event queue. Events retain press/release, repeat, and modifier state. Left and right Shift/Control/Alt are tracked independently; Caps Lock changes only on a fresh press. Pause and PrintScreen sequences cannot introduce fake modifiers. Unknown keys are ignored. [mouse.rs](kernel/src/mouse.rs) decodes full nine-bit signed motion and three button levels into its own 64-event queue, converting positive Y to downward screen motion.

Completed events receive shared sequence numbers and are merged in order, so typing received after a caret click uses the selected insertion position. Corruption, motion overflow, or queue overflow reports a shared loss barrier: both queues are discarded and the active desktop or console clears its unfinished text before recovered input. Keyboard recovery suppresses makes until a complete known-key release; release and press again to resume. Mouse recovery preserves a known partial packet's remaining byte count, and the pointer waits for all buttons to be released before accepting another click. These rules avoid treating stale packet payload as a new press.

After the desktop exits, normal boots run the allocation-free [kernel console](kernel/src/console.rs). `help` lists commands, `status` reports live PIT ticks and keyboard IRQ deliveries, `echo TEXT` prints literal text, and `clear` clears console history. A 96-character ASCII editor supports Left/Right, Home/End, Backspace/Delete, Escape, and left-click caret placement; 12 bounded history rows retain results. Text entry preserves case and punctuation. [pointer.rs](kernel/src/pointer.rs) clamps motion to the framebuffer and recognizes fresh button edges. The framebuffer draws an outlined software cursor and restores only its previous rectangle from console state without reading VRAM. Hit testing shares the editor's rendered geometry and scrolling. Editing redraws the prompt and notice; submitted commands refresh the history area. Human commands parse into the same typed `Operation` dispatcher that changes console state; this is an internal interface, not an exposed agent API. The event loop renders with interrupts enabled and checks for work with interrupts disabled before `sti; hlt; cli`, keeping IRQ work separate from drawing and avoiding a lost wakeup.

This diagnostic console runs in kernel context. It does not launch applications, interpret shell syntax, or access files. The separate userspace pixel-surface path below establishes a display boundary; input and a persistent desktop process are now present, while application focus routing and the full native desktop remain planned. No model or cloud account is needed to use the console.

Smoke tests require `MIRROR_PS2_TESTS_OK`, `MIRROR_KEYBOARD_TESTS_OK`, `MIRROR_MOUSE_TESTS_OK`, `MIRROR_POINTER_TESTS_OK`, and `MIRROR_CONSOLE_TESTS_OK`, covering routing, order, decoding, modifiers, repeats, queue wraparound/overflow, corruption recovery, button edges, pointer bounds, editing, and typed dispatch. Guarded RGB24/RGB32 surfaces check exact cursor restoration, nested clipping, offscreen positions, caret geometry, and unchanged padding. Normal device cases require both `MIRROR_KEYBOARD_READY` and `MIRROR_MOUSE_READY`; `-NoKeyboard` removes the entire controller with `q35,i8042=off,vmport=off` and requires both devices to be skipped. Debug and release normal captures then verify the actual QMP-to-PS/2-to-IRQ-to-console-to-framebuffer path.

![Mirror's console after keyboard and mouse editing](docs/images/pointer-console.png)

QEMU framebuffer captured after moving the software cursor, clicking within a command, and inserting text with the keyboard. This is the running kernel console; multi-application windows and broader desktop services remain future work.

### Read-only block driver and DMA

The initial storage path in [kernel/src/block.rs](kernel/src/block.rs) targets a selected legacy VirtIO block device through its PIO transport registers and a DMA-backed split virtqueue. It issues synchronous, single-sector 512-byte reads through Mirror's own driver. The legacy PCI claim selects the first matching `1af4:1001` type-0 function among the 32 retained inventory entries, checks a firmware-assigned I/O BAR, and rejects enabled MSI/MSI-X or malformed capability lists. The driver owns reset, feature negotiation, queue setup, bus-master enablement, polling, and shutdown; it suppresses device interrupt delivery. See the [Virtio 1.2 legacy PCI and block-device contracts](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html).

Only queue 0 is used, with one request in flight and an offered power-of-two queue size from 4 through 256. The driver accepts the read-only feature when offered and emits only read requests regardless of device write capability. Request polling is capped at 5,000,000 iterations, reset polling at 100,000, and capacity sampling at eight repeated comparisons; these are work bounds, not calibrated time limits. Completion must advance the used index once, return the submitted descriptor head, and supply a valid status before data reaches the caller. I/O-error and unsupported-request completions preserve output and permit another request. Protocol errors or timeouts stop the queue and require reset.

DMA ownership is implemented in [kernel/src/dma.rs](kernel/src/dma.rs) over the real [frame allocator](kernel/src/memory.rs). Each buffer holds 1–4 consecutive pages from one usable region, wholly below 4 GiB. Allocation reserves a complete free run atomically, validates the direct-map range, and zeroes the pages before the driver publishes them. The block path uses two or three queue pages, depending on queue size, plus one request/data/status page. Raw device-shared fields require volatile access and driver-managed ordering. Buffers have no automatic freeing: the device must be reset and quiescent before the allocator releases them. If reset cannot be confirmed, the claim and DMA pages remain quarantined; disabling bus mastering alone is not treated as proof that outstanding DMA ended.

The disposable fixture at `build/block-test.img` is generated and hash-checked by [scripts/common.ps1](scripts/common.ps1). It contains 2048 sectors, totaling 1 MiB. Each sector begins with its little-endian 64-bit LBA, then the eight bytes `MIRROR\0\x01`; bytes at offsets `i = 16..511` contain `((lba * 73) ^ (lba >> 8) ^ (i * 29) ^ 0xa5) & 0xff`. The host backend is read-only. Boot checks in [kernel/src/main.rs](kernel/src/main.rs) identify this exact fixture before running extended data checks; other supported disks receive only a sector-zero identification read. The kernel checks frame counts after successful driver shutdown.

This milestone provides a kernel sector-read path. It does not implement writes, a filesystem, partition mounting, storage pooling, native storage syscalls, or loading applications from a Mirror filesystem. Modern VirtIO transport, general device selection, concurrent requests, interrupt-driven completion, hotplug, and IOMMU isolation remain future work. Missing supported devices leave the serial diagnostics and native userspace boot path available.

### Persistent native desktop prototype

Normal boots now start a separate [native desktop executable](userspace/desktop/src/main.rs) in ring 3 after the init checks finish. It paints a desktop background and one Scratchpad panel, accepts keyboard editing and mouse caret placement, and applies the entered text through a button or Enter. Escape closes the process and returns to the diagnostic console. This is the first persistent GUI application; a multi-application compositor, window management, launcher, files, settings, and the rest of the complete desktop remain in scope below.

The application owns its drawing and interaction state. A one-byte-per-pixel palette buffer fits its scene inside the loader's existing 2 MiB image limit. It uploads only dirty tiles of at most 16 by 16 pixels through the surface API, composing its software cursor during upload rather than storing it in the scene. The granted surface is centered and capped at 1280 by 800 pixels; displays smaller than 480 by 360 return to the diagnostic console. The current UI contains one fixed panel and a bounded 96-character editor; its layout, button behavior, and text handling live in userspace. No cloud account or model is involved.

Desktop execution retains the two-tick scheduling quantum. Its recurring allowance is 32 delivered timer interrupts during user execution per 100 delivered PIT ticks. Exhausting the allowance suspends it until the next period; yielding, sleeping, and input waits do not mint more allowance. Init probes retain their original lifetime budget. These counters remain coarse: time in kernel calls is not charged as user execution, so this is not a hard total-CPU guarantee.

The desktop receives an input capability with `RECEIVE` authority in bootstrap slot 4 and can wait for an event or poll without blocking. [kernel/src/input.rs](kernel/src/input.rs) borrows the single PS/2 consumer lease, preventing the desktop and fallback console from consuming concurrently. Each input record stays in kernel storage until a complete validated copy succeeds; retries with bad buffers cannot discard it. A new session drains at most 128 old events and stages an initial loss/reset record, without forcing the keyboard into release recovery. Subsequent device loss clears the unfinished edit and disarms pointer clicks until all buttons are released. Closing or faulting the desktop revokes its handles, releases its address space, ends both device-session borrows, and returns input/display ownership to the console.

Smoke boots run the separate desktop executable in a finite verification mode: check input permissions and copies, block through a timeout without modifying the output buffer, exceed the recurring CPU allowance, then deliberately fault to verify cleanup. Invalid handles, rights, lengths, and destinations must leave the initial loss record available to the first valid input call. Pure tests cover record encoding, staged retries, ordered delivery, loss barriers, anchored allowance periods, and counter wraparound. Every smoke case requires `MIRROR_INPUT_TESTS_OK` and `MIRROR_SERVICE_POLICY_TESTS_OK`. Display/input cases also require `MIRROR_DESKTOP_TESTS_OK`, `MIRROR_DESKTOP_INPUT_TIMEOUT`, and `MIRROR_DESKTOP_CLEANUP_OK reason=fault handles_reclaimed=1`. Missing display or input requires `MIRROR_DESKTOP_SKIPPED reason=missing_display_or_input` while retaining the other boot checks. Normal debug/release captures exercise real keyboard and mouse events in the GUI, verify cursor restoration and button activation, close it, and repeat the existing console checks.

![Mirror's native userspace desktop after input](docs/images/native-desktop.png)

The Scratchpad scene, text, button, caret, and pointer are rendered by the native userspace process through checked pixel uploads.

### Native userspace pixel surfaces

[kernel/src/surface.rs](kernel/src/surface.rs) provides an allocation-free display session over the existing framebuffer driver. The primary init process receives a surface capability in bootstrap slot 3 when a supported display exists; its peer receives no surface authority. Its test session owns a centered rectangle of up to 256 by 128 pixels. After init completes, the desktop receives its own session of up to 1280 by 800 pixels. Both extents shrink to fit smaller displays, and coordinates are local to the granted rectangle. Metadata exposes dimensions, pixel format, and transfer limits, without exposing a device address or hardware pitch.

The userspace program supplies every pixel. `SURFACE_PRESENT` copies one complete packet of at most 1040 bytes into kernel storage before validating its geometry and painting anything. A tile must fit entirely inside the granted rectangle; invalid tiles are rejected rather than clipped. RGBX bytes are converted to the boot framebuffer's RGB24/RGB32 masks, and the unused X byte is ignored. Each call writes at most 256 pixels with interrupts excluded. This is a bounded copy path, with no framebuffer mapping, full-screen backbuffer, or graphics allocation in a syscall.

The driver borrow belongs to the native process run. A temporary session pointer is available only during userspace execution and is cleared before scheduler access or teardown. Surface duplicates share that session and can retain or reduce authority; closing one handle does not revoke another. All handles are closed before the session ends and the kernel console regains exclusive drawing access. The bootstrap policy creates only one surface; creating additional surfaces, transferring authority, compositing windows, and display services remain later work.

Validation exercises actual ring-3 pixel uploads and attempts to use a live foreign surface token learned through IPC. It also checks inert, stale, and wrong-object handles, complete packet copies, read-only source data, rejected read-only destinations, malformed lengths and geometry, and headless operation. Guarded RAM framebuffers verify colors, nonzero surface origins, edge tiles, unchanged surrounding pixels and row padding, rejection before any pixel changes, and the separate init/desktop extent limits. Smoke boots require `MIRROR_SURFACE_TESTS_OK`, `MIRROR_USER_SURFACE_OK`, and cleanup after both init process runs. No-display boots require `MIRROR_USER_SURFACE_SKIPPED reason=no_display` instead of granting a dead handle. With supported input available, the diagnostic console runs after the desktop exits or when the display cannot support it.

### Native userspace and syscall ABI

The loader accepts a bounded subset of static x86-64 ELF `ET_EXEC` images: at most 16 program headers and 2 MiB of mapped image pages, with aligned, readable, non-overlapping segments. It validates the image before mapping segments, copies file data into fresh zeroed pages, and leaves the remainder zeroed for BSS. Dynamic linking and a general Linux or Windows ABI are not implemented. The first application is [userspace/init/src/main.rs](userspace/init/src/main.rs), built with its [own linker script](userspace/init/linker.ld). It runs on a 64 KiB user stack with an unmapped guard page at each end.

The initial native ABI uses `INT 0x80`: RAX holds the call number, RDI/RSI/RDX hold arguments, and RAX returns the result. Returning calls preserve the other general-purpose registers. Negative results are signed error codes. This is Mirror's native interface; matching an interrupt number does not imply Linux ABI compatibility.

| Call | Arguments | Result |
| --- | --- | --- |
| `WRITE` (1) | RDI = user buffer address; RSI = byte length; RDX = console handle. | Requires console `WRITE` authority. Writes UTF-8 diagnostics with a `user:` prefix and escaped controls; returns bytes accepted. Maximum 256 bytes. After checking the handle, a zero length returns 0 without accessing the pointer; invalid user memory returns -14; oversized input or invalid UTF-8 returns -22. |
| `TICKS` (2) | None. | Current delivered timer-interrupt count. |
| `EXIT` (3) | RDI = exit status. | Returns control to the kernel and ends this process; does not resume user code. |
| `YIELD` (4) | None. | Makes the process runnable and returns 0 when the scheduler resumes it. |
| `SLEEP` (5) | RDI = number of timer ticks. | Sleeps for the requested ticks and returns 0 on resumption. Zero acts as a yield; more than 1000 returns -22. |
| `PID` (6) | None. | This process's positive identifier. |
| `BOOT_HANDLE` (7) | RDI = bootstrap slot: 0 console, 1 outbound sender, 2 inbound receiver, 3 optional surface, 4 optional input. | Returns the existing handle if still open. Init grants the primary a surface when displayed; the desktop receives console, surface, and exclusive input handles. An absent slot or a closed original handle returns -9; this call does not mint replacement authority. |
| `SEND` (8) | RDI = channel handle; RSI = readable user buffer; RDX = byte length. | Requires `SEND`. Enqueues one complete message of 1–64 bytes and returns its length. Nonblocking: a full queue returns -11, and no remaining receiver returns -32. |
| `RECV` (9) | RDI = channel handle; RSI = writable user buffer; RDX = capacity. | Requires `RECEIVE`. Copies and removes one complete message, returning its length. Capacity must be 1–64; an empty open queue returns -11 and an empty queue with no senders returns -32. |
| `RECV_WAIT` (10) | Same as `RECV`. | Receives immediately if possible, otherwise suspends the process for up to a fixed 100 delivered timer ticks. Returns -110 if the queue remains empty and open at the deadline check. |
| `CLOSE` (11) | RDI = handle. | Removes this handle and releases its endpoint authority; returns 0. Invalid, stale, or foreign handles return -9. Other duplicates remain valid. |
| `DUP` (12) | RDI = existing handle; RSI = requested rights mask. | Creates a new handle with the same or fewer rights. Returns the new positive token; zero rights creates an inert handle. Adding rights is denied. |
| `SURFACE_INFO` (13) | RDI = surface handle; RSI = writable user buffer; RDX = exact length 24. | Requires surface `WRITE`. Copies the versioned surface metadata below; returns 24. Wrong length returns -22; invalid destination returns -14. |
| `SURFACE_PRESENT` (14) | RDI = surface handle; RSI = readable packet; RDX = exact packet length. | Requires surface `WRITE`. Validates and paints a tile of at most 16 by 16 RGBX pixels; returns packet length. Invalid size, geometry, or bounds returns -22; invalid source returns -14. Rejected calls paint nothing. |
| `INPUT_WAIT` (15) | RDI = input handle; RSI = writable record buffer; RDX = exact length 32. | Requires input `RECEIVE`. Returns one complete record (32 bytes), or blocks for up to 100 delivered timer ticks. Timeout returns -110 without modifying the buffer. |
| `INPUT_POLL` (16) | Same as `INPUT_WAIT`. | Returns one complete record immediately, or -11 if no input is queued. Both calls check authority, exact length, and the whole writable range before polling; failed copies preserve the pending event. |
| Unknown call | Any. | -38. |

Surface metadata contains six little-endian `u32` fields: ABI version `1`, width, height, pixel format `1` (R, G, B, ignored X bytes), maximum tile side `16`, and reserved `0`. A presentation packet starts with four little-endian `u32` fields: local x, local y, width, height. These are followed by exactly `4 * width * height` bytes of tightly packed row-major pixels. Width and height must each be 1–16; packet length is exactly `16 + 4 * width * height`. Unknown tails, wrapped coordinates, and tiles crossing the granted boundary are rejected.

Input records are eight little-endian `u32` words: version `1`, event kind, flags, key kind, unshifted character, printable ASCII or zero, signed X delta, signed Y delta. Event kinds are key `1`, mouse `2`, and loss `3`. Key flags are pressed `1`, repeat `2`, Shift `4`, Control `8`, Alt `16`, Caps Lock `32`. Key kinds are Character `1`, Enter `2`, Backspace `3`, Tab `4`, Escape `5`, Left `6`, Right `7`, Home `8`, End `9`, Delete `10`, Shift `11`, Control `12`, Alt `13`, Caps Lock `14`. Mouse flags hold left/right/middle levels (`1/2/4`), and its two deltas are sign-extended `i32` values with positive Y downward. Unused fields are zero; loss records clear every payload field. No Rust enum layout crosses this ABI.

Capability operations resolve the caller's handle and required rights before accessing the resource. Error results are:

| Result | Meaning |
| --- | --- |
| -9 | Missing, closed, stale, or foreign handle; invalid bootstrap slot. |
| -11 | Queue full on send, empty with live senders on nonblocking channel receive, or no pending input on poll. |
| -13 | Insufficient rights or the wrong object type. |
| -14 | Invalid user range or missing effective read/write permission. |
| -22 | Invalid ordinary argument, unsupported rights bits, oversized console write, invalid UTF-8, invalid surface packet/geometry, or wrong input-record length. |
| -24 | Eight-slot handle table full or the monotonic token allocator exhausted. |
| -32 | No receivers remain on send, or no senders remain and the receive queue is empty. |
| -38 | Unknown syscall. |
| -90 | Message length or receive capacity outside 1–64, or the head message exceeds the supplied capacity. |
| -110 | A blocking channel receive remains empty and open, or an input wait has no event, when its deadline is checked. |

The kernel validates an entire user buffer before copying it. It never formats a raw user pointer directly. Receive checks the whole advertised capacity, including effective user and write permissions at every page-table level, even when the queue is empty. It rechecks the handle and destination on wake. A short buffer or failed copy leaves the message queued, and rejected copies leave the destination unchanged. Copies follow each physical page through the direct map, including ranges that cross noncontiguous frames.

Every return to user mode, including an IRQ return, requires canonical lower-half instruction and stack pointers above the null page and the native CPL3 code/data selectors. An invalid context ends the process before IRET. Yield, sleep, blocking receive, timer preemption, exit, budget exhaustion, and ordinary user exceptions return through a trusted kernel continuation. Resumable cases first save the user's interrupt frame, and Rust handlers return normally before IRET resumes the scheduler. It restores the kernel page table before choosing the next process, then activates that process's private root and resumes its saved registers. NMI, double fault, and machine check remain fatal platform/kernel paths.

The scheduler runs two init processes on the boot CPU, then one desktop process, with one user thread per process. A quantum ends after two delivered timer interrupts during that process's user execution, or sooner on yield, sleep, blocking receive, exit, or a fault. Sleeping processes become runnable at their wake tick. A waiting receiver becomes runnable when a message, endpoint closure, input event, or timeout is observed by the scheduler. Readiness or channel end-of-stream wins over timeout when observed in the same scheduler check. Receive deadlines count delivered timer ticks and do not imply a precise wall-clock bound. A ready desktop still waits if its current recurring allowance is exhausted.

Each init probe has a 32-interrupt lifetime execution budget, charged only when IRQ0 interrupts CPL3. The desktop instead receives the recurring allowance described above. This is a coarse responsiveness guard rather than precise CPU-time accounting: ticks during kernel execution, sleep, or waits do not count, and long critical sections or frequent syscalls can undercount execution. No wall-clock runtime limit is implied.

### Capability and channel ownership

Each process has an eight-slot handle table. Handle tokens are positive, globally monotonic for the boot, and never reused; the allocator stops at the signed-result limit. Authority comes from finding the token in the caller's own table. Tokens are not secrets, and learning another process's token through a message does not grant access to its object.

Rights are a bitmask: `SEND = 1` for channels, `RECEIVE = 2` for channels and input, and `WRITE = 4` for console and surface operations. Init bootstrap grants one console writer, one sender to the peer, and one receiver from the peer; only the primary process also receives a surface writer when a display exists. The later desktop instead receives console `WRITE`, surface `WRITE`, and input `RECEIVE` grants, with no channel endpoints. Object type is checked independently of the rights mask, so a console writer cannot paint pixels and an input receiver cannot receive channel messages. `DUP` operates within the same process and can preserve or reduce these rights. A zero-rights duplicate takes a table slot but cannot perform resource operations or regain rights. It holds no channel sender/receiver reference. Closing a handle invalidates that token immediately; closing a bootstrap token also makes that `BOOT_HANDLE` slot fail, even if a duplicate remains open.

The pair has two directional channels with four queued messages each. Messages contain 1–64 copied bytes; a handle number sent in those bytes remains ordinary data and transfers no authority. Queues retain kernel-owned copies rather than pointers into user buffers. A queued message survives the last sender closing and remains receivable; once drained, receive reports -32. Closing the last receiver discards and scrubs any queued messages, and subsequent sends report -32. Endpoint references track every duplicate with the relevant right.

On exit, fault, rejected return context, or budget exhaustion, the scheduler closes all remaining process handles before checking waiting peers. This lets a blocked peer observe closure without waiting for the finished process's memory to be reclaimed. Pair teardown requires both empty handle tables and fully drained channel storage. There is no channel-creation syscall, handle transfer between processes, blocking send, caller-selected receive timeout, or general revocation mechanism yet.

There is also no process-creation syscall, additional thread support, SMP scheduler, or TLS yet. Initial general-purpose registers are cleared except RDI: init receives the test scenario in its low byte and the process role in its next byte; desktop receives 0 for interactive mode or 1 for smoke verification. FS/GS bases are cleared. The integer-only ABI sets CR0.TS during user execution so x87/SIMD use faults until extended-state management is implemented. These limits apply to the current executable milestone, while the architecture below describes the intended broader system.

Kernel entry is in [kernel/src/main.rs](kernel/src/main.rs), boot requests in [kernel/src/boot.rs](kernel/src/boot.rs), physical allocation in [kernel/src/memory.rs](kernel/src/memory.rs), paging in [kernel/src/paging.rs](kernel/src/paging.rs), and exception handling in [kernel/src/interrupts.rs](kernel/src/interrupts.rs). User transitions are in [kernel/src/userspace_entry.S](kernel/src/userspace_entry.S), ELF validation in [kernel/src/elf.rs](kernel/src/elf.rs), and process loading, scheduling, syscalls, and cleanup in [kernel/src/userspace.rs](kernel/src/userspace.rs). Handle tables are in [kernel/src/capability.rs](kernel/src/capability.rs), channel queues in [kernel/src/ipc.rs](kernel/src/ipc.rs), the heap in [kernel/src/heap.rs](kernel/src/heap.rs), and the first timer driver in [kernel/src/timer.rs](kernel/src/timer.rs). The native desktop source is in [userspace/desktop/src/main.rs](userspace/desktop/src/main.rs), with its renderer beside it. See Rust's [bare-metal target documentation](https://doc.rust-lang.org/rustc/platform-support/x86_64-unknown-none.html) and the [Limine bootloader](https://github.com/Limine-Bootloader/Limine) for the underlying build and handoff contracts.

## Goals

- A complete native desktop environment with responsive manual controls, typed agent interfaces, a small baseline resource footprint, and optional background services.
- Independent Windows and Linux application compatibility implementations over Mirror's native kernel and services, expanded through a published, tested application matrix.
- Structured application operations that agents can discover and invoke without repeatedly interpreting screenshots.
- OS-enforced limits on agent access to files, processes, networks, devices, credentials, and compute.
- A common resource policy for RAM, VRAM, storage, and cache locality, aware of the differences between them.
- Local inference that cooperates with interactive applications over memory, compute, bandwidth, power, and scheduling.
- Storage pooling that uses available devices and transfer paths while maintaining explicit durability guarantees.

Rust is the primary implementation language. Existing compatibility and inference components may use other languages. Language choice alone does not establish speed, isolation, or compatibility.

## Proposed architecture

Mirror's foundation is its own Rust kernel and native userspace. That work is already executable: the kernel boots on virtual hardware in QEMU and schedules native Rust programs through Mirror's syscall interface. Native init and the desktop prototype use Mirror interfaces; broader services and the device framework will extend them. Resource policies and inference services will build on those same memory, scheduling, and device mechanisms.

Two separate application compatibility personalities are planned. A personality implements the interfaces expected by applications from another operating system and translates their behavior onto Mirror's native primitives. The Windows personality targets Windows executables and APIs; the Linux personality targets Linux executables and its syscall ABI. Neither personality defines Mirror's kernel or native system services, and implementing the Windows personality does not depend on adopting Linux as a system base.

| Component | Responsibility |
| --- | --- |
| Mirror kernel | New Rust implementations of address spaces, threads, scheduling mechanisms, interrupts, memory mappings, IPC, capability enforcement, and device isolation where hardware supports it. |
| Native system services | Mirror filesystems, networking, device management, application lifecycle, and desktop services using native Mirror interfaces. Place services outside the kernel where practical. |
| Native desktop | Mirror's compositor, window management, input routing, application launcher, taskbar, settings, file manager, and notifications, with complete human controls and capability-checked agent operations. |
| Agent broker | Discover typed operations, validate arguments and authority, apply quotas, dispatch actions, and record results. |
| Capacity manager | Account for resources, choose object placement, reserve transfers, and coordinate reclamation and prefetching. |
| Inference services | Execute models through replaceable backends and supply workload hints to the capacity manager and scheduler. |
| Storage service | Manage a logical pool, extent placement, checksums, redundancy, recovery, and background rebalancing. |
| Windows application personality | Load Windows PE executables and supply the Windows APIs and NT behavior required by selected applications, using Mirror's native kernel and services. |
| Linux application personality | Load Linux ELF applications and implement their required syscall and userspace ABI behavior over Mirror's native kernel and services. |

The kernel supplies bounded mechanisms and enforces authority. Higher-level resource policy and model execution belong in restartable services. A model crash or unavailable cloud provider must not stop the machine from booting or being used manually.

Initial development target: **x86-64, UEFI, and QEMU**, using Limine for the boot handoff, followed by a deliberately small hardware support list. QEMU provides a repeatable machine to boot the actual Mirror kernel. GPU and target-application requirements will guide the first physical hardware targets.

## Native desktop environment

Mirror will have a complete native desktop environment built on its own kernel, drivers, and userspace services. The desktop must be usable manually with a keyboard and pointer, including when no model is loaded or cloud account is available. Every desktop feature must also be designed for typed, capability-checked agent access through the same underlying operations and state changes used by the human interface.

The planned scope and completion criteria are:

| Area | Native implementation and acceptance evidence |
| --- | --- |
| Compositor and windows | Compose surfaces from real native applications; open multiple independent windows, move and resize them, change focus, minimize, maximize, and close them. Rendering and repainting must reflect live application state. |
| Input | Route keyboard and pointer events through Mirror's device and input services to the focused surface; verify text entry, selection, pointer actions, and desktop shortcuts across application switches. |
| Application launcher and taskbar | Launch installed native applications, display running applications and their windows, switch or restore them, and reflect their actual exit state. |
| Settings | Provide working display, input, appearance, resource, and permission controls backed by the corresponding services; persist supported settings and verify them after reboot. |
| Files | Browse the real filesystem, open files in applications, and create, rename, move, copy, and delete permitted objects; verify durable results and reject operations outside the caller's authority. |
| Notifications | Deliver notifications from applications and services, show their source and actions, and support dismissal and notification preferences through both interfaces. |
| Human and agent operation | Complete the same representative desktop workflows manually and through typed operations; verify consistent results, change events, permission denials, and continued manual use when inference is unavailable. |

Desktop acceptance requires these features to work together on a documented supported machine with Mirror's actual rendering, input, application, and storage paths. Record responsiveness under application and inference load once those backends exist. The boot panel establishes early display access, and the current userspace Scratchpad adds text editing, a button, and pointer interaction. Multi-application composition, window management, and the other complete-desktop workflows above remain planned.

## AI as a first-class system interface

Mirror applications should expose versioned, typed operations alongside their human interface. An operation describes its inputs, outputs, required capabilities, side effects, cancellation behavior, and retry semantics. For example, an editor could expose `document.open`, `document.replace_range`, and `document.save`, returning structured results and change events.

Agents can inspect state, subscribe to changes, and batch supported operations instead of repeatedly capturing a screen, guessing a coordinate, and waiting. This should reduce interaction overhead; it does not eliminate model latency or make legacy applications automatically expose useful APIs.

Use native application APIs first, compatibility adapters and accessibility interfaces second, and visual interaction where necessary. Human and agent interfaces should invoke the same underlying application operations so behavior stays consistent.

### Authority and isolation

- Give each agent a separate principal and scoped, revocable capabilities. Scope can include an object or directory, an allowed operation, a network destination, a lifetime, and resource limits.
- Enforce grants at the underlying resource boundary. Broker checks alone are insufficient if an agent can bypass them through a shell, direct filesystem access, IPC, or an unrestricted application.
- Use kernel-managed handles and constrained delegation. Child processes and invoked applications must not gain broader authority merely by acting for an agent.
- Bind authorization to the actual object and operation, including when names or paths change between inspection and execution.
- Keep credentials in a dedicated broker. Ordinary application automation and model context should not receive raw provider tokens.
- Support previews and explicit grants for consequential operations, cancellation, and compensating actions where possible. Do not claim every action is reversible.
- Record the requesting principal, granted authority, affected resource, and outcome. Redact secrets and restrict access to audit records.
- Apply the same resource limits to local and remote agents. Treat application content, tool results, and retrieved text as data that cannot mint capabilities.

These controls limit what a compromised or misdirected agent can do. They do not establish that a model is always correct or immune to prompt injection. Compatibility applications and drivers remain part of the isolation design.

### Providers and sign-in

Support local models and interchangeable remote provider adapters. Provider authentication, permission to use inference, and permission to operate Mirror are separate decisions. Local OS login and offline use must remain available without a cloud account.

As checked on October 8, 2026, OpenAI documents Sign in with ChatGPT for open-source/local applications, including optional authorization for eligible Responses API requests using a ChatGPT plan. Identity scopes do not themselves grant inference access or access to conversations. Mirror's adapter must follow the applicable registration, consent, scope, and token-lifecycle requirements; eligibility depends on the integration and account. See the [official overview](https://developers.openai.com/siwc/token-sharing-open-source) and [integration availability](https://developers.openai.com/siwc/quickstart).

Mirror's distribution model and license are still undecided. Select the appropriate provider integration once those are established; do not assume an open-source integration applies to every distribution.

## Windows and Linux applications

Windows `.exe` files and Linux applications are independent execution targets for Mirror. Both will run against compatibility personalities built on Mirror's own kernel and native userspace. Linux application support means implementing the required application ABI; it does not mean using the Linux kernel or a Linux distribution as Mirror's foundation.

Loading an executable is only one part of compatibility. Applications also depend on system calls, libraries, threading, graphics, filesystem behavior, services, and sometimes kernel drivers. Each personality must supply the observable behavior required by its selected applications while preserving Mirror's process isolation and resource controls.

| Application class | Proposed route | Initial boundary |
| --- | --- | --- |
| Native Mirror applications | Rust services and applications using Mirror's own APIs. | Native init and the separate desktop prototype use these interfaces; broader services build on this environment. |
| Windows user applications | Mirror's Windows personality: PE loading plus the Windows API and NT behavior required by selected applications, implemented over Mirror primitives. | Start with named applications and bounded workflows; loading PE files alone is insufficient. |
| Linux command-line programs | Mirror's Linux personality: ELF loading and a bounded Linux syscall ABI mapped to Mirror primitives. | Start with small static binaries and the calls they actually require. |
| Larger Linux applications | Expand the Linux personality's dynamic loading, libraries, graphics, and service support. | Compatibility is verified per application and version. |
| Applications needing a full guest OS | Investigate optional integrated Linux or Windows VMs later. | Separate from either application personality; requires a virtualization backend, guest integration, suitable hardware, and appropriate guest software licensing. |

For Windows applications, the implementation scope includes executable and DLL loading, imported APIs, and required NT-style object, handle, thread, synchronization, exception, and filesystem behavior. Graphics and platform services follow the needs of the selected workflows. This is incremental application compatibility, with explicit supported behaviors and gaps; it is not a commitment to recreate every Windows subsystem before the first application runs.

For Linux applications, the ELF format and syscall ABI are application contracts to implement on Mirror. Native Mirror programs also use ELF today, but they already have a distinct native syscall interface. [FreeBSD's Linux compatibility documentation](https://docs.freebsd.org/en/books/handbook/linuxemu/) is a precedent for implementing a foreign application ABI on another kernel. POSIX source compatibility alone does not establish Linux binary compatibility.

Existing projects such as [Wine](https://www.winehq.org/about) may offer code worth reusing for particular Windows behaviors. Adopting any of that code requires an explicit later decision about licensing, dependencies, adaptation to Mirror interfaces, and maintainability. A Wine port is not a predetermined architecture or a roadmap acceptance gate. Kernel drivers, anti-cheat, DRM, and applications with deep platform dependencies require separate assessment.

GPU support is an early feasibility gate. For example, Mesa's RADV Vulkan driver depends on the Linux `amdgpu` kernel interface; a Vulkan API alone does not supply the required driver stack. CUDA likewise has supported OS and driver requirements. Mirror will pursue native driver ports for a selected hardware family and adapt the corresponding userspace graphics and compute stack. Accelerated inference is a milestone that depends on that working port. See [Mesa RADV](https://docs.mesa3d.org/drivers/radv.html) and [CUDA system requirements](https://docs.nvidia.com/cuda/cuda-installation-guide-linux/).

Booting Mirror inside QEMU does not give Mirror the ability to host accelerated guest operating systems. That is a separate project; [QEMU's accelerators](https://www.qemu.org/docs/master/system/introduction.html) rely on host facilities.

The first Windows and Linux application targets are still to be selected. Record application versions, dependencies, required workflows, graphics needs, and known failures in the compatibility matrix. Advance each personality against those concrete tests without making universal compatibility claims.

### Native driver ports

Port drivers into Mirror through a documented platform adaptation layer. Reuse device-specific source where licensing permits, and replace its dependencies on the original operating system with Mirror implementations for MMIO, DMA mappings and buffer lifetimes, interrupts, allocation, synchronization, timers, and work queues. Device discovery, power management, firmware loading, and recovery also need explicit integration.

Begin with serial diagnostics and a small set of virtual devices, then establish the device framework and port drivers for named physical hardware. Keep architecture-specific operations and device-independent services separate from device logic so each port has a bounded interface to implement and test. Userspace libraries that expect a particular kernel driver ABI need a matching compatibility interface or their own port.

Porting source does not mean Mirror can directly load Linux kernel modules or closed-source Windows/Linux binary drivers. Existing modules depend on their original kernel's ABI and internal behavior. Each candidate driver requires an assessment of available source, firmware, licensing, hardware documentation, and dependencies before it becomes part of the support plan.

## Unified capacity management

Treat resources as a shared management problem while preserving their individual properties.

| Resource | What Mirror can manage | Constraint |
| --- | --- | --- |
| CPU RAM | Allocations, resident pages, NUMA placement, pinned buffers, and reclaimable data. | Locality, bandwidth, and page ownership matter. |
| GPU VRAM | Runtime-managed buffers, residency, transfers, and supported mappings. | Access, coherence, and migration depend on the GPU and driver. |
| CPU/GPU caches | Locality, working-set behavior, and hardware-supported controls. | Ordinary hardware cache is not an additional independent RAM allocation pool. |
| NVMe, SSD, HDD, UFS, and other storage | Durable objects, staged model data, and spill space. | Persistence, latency, endurance, transfer paths, and failure domains differ. |

Do not report RAM plus VRAM plus disk capacity as a single quantity of equally usable memory. Report capacity and pressure by tier, then explain which workloads can use each tier. [Linux HMM](https://kernel.org/doc/html/latest/mm/hmm.html), [hardware cache resource control](https://docs.kernel.org/filesystems/resctrl.html), and [CUDA Unified Memory](https://docs.nvidia.com/cuda/cuda-programming-guide/04-special-topics/unified-memory.html) are useful precedents with hardware-specific limits.

The capacity manager should understand managed objects such as immutable model weights, active KV-cache blocks, temporary activations, files, and ordinary application pages. Each object needs a size, owner, permitted locations, mutability/coherence rules, current valid copies, durability requirement, eviction/recomputation cost, and optional next-use or deadline hints.

Native inference runtimes can provide these hints explicitly. Arbitrary legacy application memory cannot be assumed to identify tensors or safely move between CPU and GPU execution contexts. Begin with registered buffers and runtime cooperation, then expand general page-management policies.

### Transfer graph and Dijkstra's algorithm

Represent memory and storage locations as graph nodes. Edges represent supported transfers, including staging through RAM when direct transfer is unavailable. Record shared controllers, buses, and links as constraints so multiple transfers cannot each assume the full bandwidth of the same path.

For an object of a fixed size and a snapshot of current load, estimate an edge's nonnegative cost in units of time:

```text
transfer_cost = setup_latency + bytes / effective_bandwidth + queue_delay
```

Filter out paths that violate accessibility, capacity, ownership, or durability requirements. Dijkstra can then choose a minimum estimated cost route for a serialized transfer under that snapshot.

Routing is one part of the policy. Separate decisions choose the destination, decide what to evict or recompute, admit concurrent transfers, and reserve shared resources. Pipelined or parallel transfers require a richer model than summing edge costs. Re-evaluate plans when load changes, with a minimum expected benefit and cooldown to avoid repeatedly moving the same object.

A migration must reserve destination and staging space, synchronize writers, copy and verify data as appropriate, publish the new valid location, and only then release the old allocation. Failed or canceled transfers must release reservations while preserving a valid copy. Durable objects require storage commit semantics in addition to memory-copy correctness.

## Storage pooling

Every enrolled permanent storage device should participate in a logical pool. Device discovery proposes enrollment; enrollment must preserve existing data or explicitly initialize a device selected for that purpose. Merely attaching a drive must not silently overwrite it.

Use checksummed extents with separate policies for placement and durability. Candidate policies include weighted striping for parallel I/O, replicas for read concurrency and resilience, and tiering for active versus cold data. Schedule chunks according to measured device and path behavior so a slow drive does not automatically become part of every latency-sensitive read.

Multiple drives can increase throughput when there is enough parallel work and independent bandwidth. They can also share a controller or upstream link, and adding a slow drive can hurt a particular workload. The topology-aware storage discussion in [NVIDIA's GPUDirect Storage design guide](https://docs.nvidia.com/gpudirect-storage/design-guide/) is a useful reference; direct storage-to-GPU paths require specific platform support.

Proposed initial durability policy:

- Keep irreplaceable data and pool metadata redundant across at least two independent devices when available. Report when that requirement cannot be met.
- Allow explicitly disposable model caches and temporary data to use less redundancy, provided they can be reconstructed.
- Make persistent writes and metadata updates crash-consistent; verify recovery before treating the pool as usable for real data.
- Bound background rebalancing and rebuild traffic, and define degraded operation, device removal, and interruption recovery.

Striping alone provides no redundancy, and a redundant pool does not replace backups. Initial native storage work should use disposable QEMU disk images through Mirror's device drivers. A production filesystem and recovery implementation are later milestones.

## Inference and scheduling

Integrate inference with OS policy through dedicated services. Keep model weights, tokenizers, inference backends, and provider adapters outside the kernel.

The runtime should expose memory estimates and phase information: model loading, prefill, decoding, and background work. The scheduler and capacity manager can use these hints to reserve interactive headroom, cap queue depth and context growth, prefetch likely-needed weights, reclaim inactive objects, and account for shared memory bandwidth and power limits.

Batching can improve throughput while increasing wait time. Moving data out of VRAM can make a model fit while reducing its speed. Both decisions need workload-specific measurements. Inference-aware KV-cache management has useful precedent in the [PagedAttention paper](https://arxiv.org/abs/2309.06180).

CPU scheduling cannot guarantee immediate preemption of an arbitrary GPU workload. Backend support and bounded submissions determine how quickly inference can yield the device. Define admission and responsiveness rules around the capabilities of the actual driver and runtime.

Keep per-user model state isolated. Any sharing of cached prompts or KV data must respect ownership and information-flow policy. The machine must remain responsive when inference runs out of memory, stalls, or crashes.

## Development roadmap

Milestones are acceptance gates, not delivery-date promises. Work proceeds from the kernel to native userspace, then device support and higher-level services. Target applications, hardware, licensing, and reference workloads should be narrowed alongside implementation so they inform interfaces without blocking the first boot.

| Stage | Deliverable | Evidence required to advance |
| --- | --- | --- |
| 1. Kernel boot | Freestanding Rust x86-64 kernel, Limine boot handoff, serial diagnostics, and memory-map inspection. | Buildable EFI boot tree and repeatable QEMU boots; expected serial output; validated handoff data; diagnostic panic and halt paths. |
| 2. Memory and exceptions | Physical frame allocation, virtual memory ownership, kernel allocation, exception handling, interrupts, and timers. | Reserved memory is never allocated; mapping operations preserve ownership; faults produce useful diagnostics; timer interrupts work. |
| 3. Native userspace and syscalls | User address spaces, executable loading, syscall entry, threads, scheduling, IPC, capability handles, and a minimal native init process. | A native user program runs; process faults remain contained; invalid pointers and unauthorized handles are rejected; tasks yield and resume. |
| 4. Device framework and driver ports | Platform adaptation layer, device discovery, display and input foundations, DMA and interrupt support, VirtIO storage/network drivers, and selected physical-device ports. | Named devices work through Mirror's interfaces; display output and input delivery use real device paths; DMA ownership and interrupt handling are checked; failure and device recovery paths are exercised. |
| 5. Native services, application personalities, and AI | Mirror filesystem, shell, and desktop service foundations; initial, separately scoped Windows PE/API/NT and Linux ELF/syscall personalities over Mirror interfaces; typed-operation broker, CPU inference, and capacity-manager integration. | Native services run on Mirror; selected Windows and Linux workflows pass their personality-specific tests as support arrives; agent permissions are enforced; inference preserves interactive headroom. |
| 6. GPU ports and accelerated inference | Selected GPU driver port, userspace graphics/compute integration, real RAM/VRAM placement, and responsiveness controls. | Correct rendering and model outputs on named hardware; reproducible mixed-load measurements; migration and resource-pressure recovery. Assess the port's dependencies during earlier stages. |
| 7. Complete native desktop and durable pool | Native compositor and windows, keyboard/pointer input, application launcher and taskbar, settings, files, and notifications with full human controls and typed agent interfaces; installer/update recovery, storage pooling, expanded compatibility, and broader device testing. | All native desktop acceptance workflows above pass on supported hardware, manually and through authorized agent operations; settings and files survive reboot; recovery after interrupted writes/updates and device failures; application-specific workflows and repeatable permission tests. |

Full guest-OS integration remains a separate later compatibility option. Universal Windows/Linux compatibility, arbitrary hardware support, and a fully optimized inference stack are outside the first milestone.

### First implementation slice

Build the freestanding Rust kernel and its EFI boot tree first. Limine supplies the initial boot environment and memory map; Mirror owns the kernel entry point, serial diagnostics, handoff validation, and panic behavior. This establishes the executable base for physical memory management and exception handling before entering native userspace.

The first boot slice's acceptance criteria are:

1. The kernel builds for a bare-metal x86-64 target without a host operating-system runtime.
2. The EFI boot tree reaches Mirror's entry point under QEMU and emits an identifiable serial boot message.
3. The kernel validates the required boot-protocol revision and memory-map response before inspecting entries; missing required data produces a diagnostic halt.
4. Memory-map accounting and frame allocation distinguish usable memory from reserved regions. Frame recycling, page-table ownership, and the initial heap are implemented; private user trees are reclaimed on process teardown. Bootloader and kernel page-table reclamation remain later work.
5. Panic and normal idle paths remain within Mirror. Fatal paths disable maskable interrupts and halt; normal idle enables them and wakes for timer IRQs.

This original slice establishes boot and handoff behavior. Stage 3 now has isolated native user processes, syscalls, teardown, preemptive scheduling, console/channel/surface/input capabilities, and bounded IPC. The persistent desktop prototype adds a recurring execution allowance and exclusive input delivery to a separate native application. Broader thread and process lifecycle support, capability delegation, and additional IPC mechanisms remain work for that stage. Early stage-4 work adds PCI inventory, optional boot framebuffer output, bounded userspace pixel uploads, contiguous DMA ownership, a selected read-only legacy VirtIO block path, and PS/2 keyboard/mouse input with a software cursor and diagnostic console. A general device framework, filesystems, multi-application input and display services, broader drivers, and the full native desktop remain ahead. General application compatibility and inference capability remain later milestones.

### Performance evidence

Once real backends exist, record idle RAM and CPU use, boot and application launch time, interactive p95 latency, model load time, time to first token, inter-token latency, tokens per second, peak RAM/VRAM, transferred bytes, storage throughput, and recovery behavior.

Compare the same hardware, application workload, model, quantization, context length, concurrency, and warm/cold-cache conditions. Include both models that fit in VRAM and oversubscribed workloads. Evaluate a static placement baseline as well as existing operating-system/runtime behavior. Report throughput and responsiveness together, including regressions, before claiming Mirror is faster.
