# Roadmap re-plan: a modern OS first

Status: proposed 2026-10-10. Adopted 2026-10-11 in part (Cameron): RS1 (phases 0-11 replace M0-M9), RS2 (storage and network before Kimi's shell), RS3 (0029-D9's geometry withdrawn; 0029 part 1 to be merged as committed), RS4 (HD Audio in v1, ledger S22) and the first part of RD1 (0015b moves to Phase 11, so 0013 follows 0015a; 0015b is re-drafted and re-confirmed before Phase 11). The ledger (decisions.md, Phases) and README.md's spec map now carry the order. Confirmed 2026-10-12 (Cameron): RS6 (the agent-demo build retires at 0030a), the rest of RD1 (the vmrun lane, the G11 docs commit and 0058a join Phase 0), RD2 (the harness's kernel primitives land early; its product layer goes to Phase 10), RD6 (0053 in Phase 1, before 0028; 0054a carries the S18 reserve and OOM order; 0054b keeps only pressure notifications), RD7 (0058a, reboot without AML, in Phase 0; each other clock, power and crash piece lands where its parts exist), RD8 (0019a opens Phase 1 and 0019b follows 0062, with no daily resolution pin), RD16 (the G11 VM enters `docs/vm-settings.md` in Phase 0), RD21 (0021 is written per-CPU from the start, after 0050, and 0050 drops its rework clause) and RD24 for the vmrun lane and three Phase 1 items (the vmrun lane becomes 0015c, the host lane, and 0015d, the VMware cases; 0019's Phase 1 part becomes 0019a, 0019c and 0019d, and 0019b stays C3 telemetry; 0030a keeps its name; kernel heap v2 is 0059). Every other RS and RD decision here stays proposed and is decided when its Due column says. The other NEW specs and splits are confirmed and numbered when their phase comes up (RD24).

## 1. Summary

- Cameron's concern is right. The kernel and platform block Kimi's design, not the shell. User code runs only on the boot CPU, and each process gets at most 32% of it. Images stop at 2 MiB. Pixels move one 16x16 tile per syscall into a 1280x800 box. Text is a 5x7 font, and no disk works after boot. 0029 restyles this (tokens, focus rings, a written error toast, a Relaunch button, a new pointer) without lifting any limit.
- The previous order (M0-M9, replaced by Phases 0-11 on 2026-10-11) built the compositor (M1) and the harness (M3) before SMP (M4), storage (M5) and text (M7). It records only two reworks (0021's entry in 0050, and 0084's envelope payload) and requires SMP-safe harness code (decisions.md:165). We expect, but have not shown, that their UI will be redone once 0084 and 0086 land. No spec's scope line covers a renderer, frame clock, window manager or icons, and a toolkit and desktop chrome have no owner at all.
- The new order:
  - Phase 0: finish VMware boot.
  - Phases 1-3: rebuild the kernel core once.
  - Phase 4: a desktop at the VM window's size.
  - Phases 5-7: power, then files, then network.
  - Phase 8: Kimi's shell. Phase 9: apps.
  - Phase 10: the harness product layer. Phase 11: AI and acceptance.
  - v1's contents do not change, except as RS4 and RD18 record. Their outcome updates 0000-north-star's "modern desktop basics" or "Not in v1" list.
- Of about 126 specs, Kimi's visual language becomes buildable at about spec 45, desktop-shell.html (without volume and glass) at about 80, and apps.html at about 90.
- Next:
  - Merge 0029 part 1 as committed (it already carries 0029-D9's frame colors).
  - Start 0015a, then 0013, both built as confirmed.
  - Draft NEW 0030a as a docs-only task.
  - RS1-RS4 were confirmed on 2026-10-11, and RS6 on 2026-10-12. RS5 is decided when due, as its row in 6.1 says. It does not block 0015a or 0013.

## 2. Where Mirror stands

Mirror is a careful, well-tested kernel demo that runs on one CPU. It is not yet a general-purpose OS. It has never booted on its daily target, VMware.

| Area | Today | Evidence |
| --- | --- | --- |
| What works | Private address spaces with W^X loading, demand-zero memory with quotas, threads with futex waits and TLS, an HPET nanosecond clock, secondary CPUs up for IPIs and TLB shootdowns, device probes that reset before freeing DMA, and a large QEMU smoke and fault suite | readme.md status; init's battery |
| Execution model | No process table: processes are stack locals of three session loops. All user threads share one static kernel stack. User code runs only on the boot CPU. Each process gets 32 of every 100 ticks at 100 Hz | kernel/src/userspace.rs:614, :1182, :1958; :57-59; smp.rs:1-7; userspace.rs:44-45; timer.rs:38 |
| Capacity | Images up to 2 MiB, 32 MiB committed per process, 4 threads. The kernel heap grows only at two boot points. Userspace is soft-float, and the kernel reports `avx=0` | elf.rs:15; vm-protocol lib.rs:18; thread-protocol lib.rs:23; main.rs:292, :314; fpu.rs:78 |
| Display | A centered desktop of at most 1280x800, at most 2 apps of 320x192, one 16x16 tile per `int 0x80` written as volatile byte stores, and a 5x7 ASCII font in three copies | surface.rs:38; window-protocol lib.rs:12-20; framebuffer.rs:463-483 |
| Storage | No disk is usable after boot, because every driver shuts down before `smp::init`. AHCI accepts only IDENTIFY and READ DMA EXT. VirtIO writes exist only in boot-smoke builds. There is no filesystem, and nothing persists | main.rs:221-255 vs :288; ahci.rs:705-707; block.rs:445-481 |
| Power and devices | No power-off or reboot (the only exit is the smoke-only `exit_qemu`). No NIC driver, SVGA, audio or guest RPC. No Up/Down, F-keys, Super or wheel | arch.rs:140-148; keyboard.rs:25-40; ps2.rs:337-361 |
| Userspace | Four `no_std` programs, each with its own `int 0x80` wrapper. No allocator, runtime or `std` | userspace/*/src/main.rs |
| VMware | Never booted, because no disk image exists until 0015a. Static analysis predicts that a VMware boot today falls back to one CPU on the legacy PIC. With more than 64 CPU slots the MADT is discarded (0013-D4 fixes this); otherwise the IOAPIC ID check rejects the IOAPIC (0013-D9). This is a prediction, not an observation | apic.rs:242-243; crates/acpi/src/lib.rs:21, 24; 0013:13 |
| 0029 | Part 1 is committed (2654a75, 7c32fdb), and both suites passed. It restyles the demo. Only `crates/ui-tokens`, its contrast tests and the cursor bitmap carry forward. 0029-D9's geometry is not applied; its frame colors are | userspace/desktop/src/shell.rs:13 is still `HEIGHT + 30` |

The previous plan (M0-M9, replaced by Phases 0-11 on 2026-10-11) had four problems:
1. The kernel foundations (0050-0054) sit in M4, after about 20 specs that assume them.
2. Text (0086) and input (0084) sit in M7, after the M1-M3 UIs that need them: the compositor (0027a), owner login (0040) and the trusted approval surface (0049). Only 0082, 0083 and 0085 are ordered after them.
3. Storage (M5) comes after the harness (M3). This inverts the readme's own priority (readme.md:5).
4. No spec's scope line covers a renderer, frame clock, window manager or icons. 0029's non-goals and docs/design/README.md:22-23 defer them to 0027a and 0086, but README.md:38 and :104 do not include them. A toolkit, desktop chrome, notifications, scaling and an app model have no owner at all. Unless those gaps are filled, the design is not buildable.

## 3. Can today's OS host Kimi's design?

No. Each row is either a kernel or platform limit or a spec nobody owns, so shell work alone cannot reach the design.

| Missing foundation | Today | Supplied by |
| --- | --- | --- |
| Pixel transport and surfaces | One tile per syscall; app pixels in kernel pages; 2 apps of 320x192 | 0025, 0027a (scanout object, shared surfaces), 0027b |
| Any display size, mode setting, live resize | Centered 1280x800 cap; firmware GOP mode | 0027a lifts the cap (as S1c says); 0060a, 0087a and 0069 set modes and follow the window |
| HiDPI scaling (100-200%) | None | Output and buffer scale in 0027a from day one; NEW scaling (Phase 8) |
| Alpha, antialiasing, rounded corners, shadows, gradients | 8-bit palette scene; rectangle occlusion | NEW 0027c renderer |
| Motion and frame pacing | 100 Hz tick; the shell polls once per tick (shell.rs:1048) | NEW high-resolution timers with tickless idle; NEW 0027d |
| CPU for software composition | Boot CPU only, 32% per process, soft-float, no AVX | 0051, 0052, 0028, 0053 |
| Memory for full-screen buffers | 2 MiB images, 32 MiB commit, about 3 MiB of kernel heap | 0054a, NEW ELF loader v2, NEW 0059 kernel heap v2 |
| Kernel substrate | Stack-local processes, one kernel stack, polled wakeups | NEW 0030a, 0050 |
| Text and icons | 5x7 ASCII bitmap; no icons | 0086a (fonts and icons on 0027c's shared path rasterizer), 0086b |
| Input the design assumes | No Up/Down, PgUp/PgDn, F-keys or Super; no wheel; ASCII-only events | 0084a, 0084b |
| Widgets, focus model, element trees | Hand-coded rectangle tests in each app | 0039, 0044a, NEW toolkit core (Phase 4), NEW toolkit widgets (Phase 8) |
| Window management | Two fixed windows: drag, minimize, close | NEW 0027e |
| Dock, start menu, palette, quick settings, switcher, workspaces | Two launcher buttons and one toast | NEW chrome |
| Notifications | One refused-launch toast | NEW notification service |
| Drag and drop, cursor shapes, screenshots | None | 0027a (data offer, cursor-shape requests, human-seat capture API), 0069 (hardware cursor), NEW toolkit widgets (drag sources and drop targets), NEW screenshots |
| Files' Home layout and opening files | No filesystem | 0066b (layout, change notification), NEW app registry and file associations, NEW open/save chooser |
| Clock, power menu | None | 0056a/0056b, 0058a, 0057, 0058 |
| Persisted theme, scale, layout | Compile-time dark theme | 0068, NEW first-run setup |
| Volume in the top bar and quick settings | No audio | NEW HD Audio (recommended), or a G12 exception |
| Brightness slider | Meaningless in a VM | Dropped |
| Trusted approval layer, agent cursor | None | Layer reserved in 0027a and 0027c; built by 0049 and 0047 |
| Glass (backdrop blur) | Not representable | After the Phase 4 frame budget, and then only the dock and panels |

## 4. Proposed phases 0-11

Conventions:
- Counts are approximate and cumulative from today, with 0015a as spec 1. The cumulative counts after Phase 0 predate the 2026-10-12 numbering, which made the vmrun lane two specs (0015c, 0015d): add 1 to each, and 2 more from Phase 1 on if 0019-D15 divides 0019's Phase 1 part in three.
- "(proposed)" marks a starting threshold. Cameron sets the final number in the owning spec.
- VMware checks run by hand on the G11 VM unless noted (S1a). The vmrun lane (0015c, 0015d) scripts its own checks on a separate VM with the 0015a reference profile, not G11's (VMR-D2, confirmed 2026-10-12).
- RS and RD numbers are this proposal's decisions (section 6). Spec decisions are always written in full, for example 0029-D9.

### Phase 0: Mirror boots well in VMware (10 specs, about 10)

Goal: the VMDK boots in VMware on every vCPU, and the pointer is not grabbed. The guest can restart itself, and a script can boot, restart and suspend the VM headless.

Specs, in order:
1. Close 0029: Cameron merges part 1 as committed (it already carries 0029-D9's frame colors).
2. 0015a, as confirmed.
3. NEW vmrun lane, as two specs (RD24): 0015c, the host lane, and 0015d, the VMware cases.
   - It boots `build/dist/mirror.vmdk` headless (`vmrun -T ws start <vmx> nogui`) and reads the serial file.
   - It checks the boot markers, power-off and `vmrun suspend`/`start`.
   - Its typed checks use 0018's perf-probe over VMware's VNC server, so the lane depends on perf-probe, and its VM carries 0018's RemoteDisplay.vnc keys. A `restart` check joins when 0058a lands.
   - It joined Phase 0 on 2026-10-12 (RD1), after 0013 was done, so it lands at the end of Phase 0 with the G11 docs commit and 0058a.
4. 0013, as confirmed. If its tests 14 and 16 show VMware handing off in x2APIC mode (unsettled, 0013:134), 0055a with x2APIC handoff follows here.
5. A docs-only commit adds the G11 daily VM (12 vCPUs, 20 GB, NAT) to `docs/vm-settings.md`, with every reference-VM key and 0018's RemoteDisplay.vnc keys, and rewords the lines that give it to 0067 (RD16).
6. 0016a, 0016b, 0017a, 0017b, 0017c. If 0017c check 7 fails without VMware Tools, 0087a moves here from Phase 4.
7. NEW 0058a: reboot without AML (FADT reset register, then 8042, then 0xCF9, then triple fault), plus a console `restart` command.
8. In parallel: 0018's Linux and Windows baseline track.

Exits:
- On the 0015a reference VM and on the G11 VM:
  - `MIRROR_APIC_READY mode=xapic` (or x2apic after 0055a) prints.
  - `MIRROR_SMP_READY online=` equals `numvcpus`.
  - No `MIRROR_APIC_SKIPPED` or `MIRROR_ACPI_SKIPPED` appears.
- Serial has one `driver=ahci-sata` record and the e1000 record `id=8086:100f`.
- `MIRROR_VMMOUSE_READY mode=absolute` prints, and the host pointer enters and leaves the window without click-to-grab (0017c check 7).
- The vmrun lane passes three checks:
  - boot;
  - console `restart`;
  - `vmrun suspend` then `start`, after which `status`, typed with 0018's perf-probe, changes the console output region in the next VNC frame, and no `MIRROR_PANIC` appears.

Honest limits: the screen is VMware's GOP default mode with a centered 1280x800 desktop. Nothing persists. Power Off still ends every session.

### Phase 1: Kernel core, built once (9 specs, about 18)

Goal: replace the boot-CPU-only, run-to-trap kernel before anything else is built on it.

Specs, in order:
1. 0019a, 0019c and 0019d, split from 0019 (RD24 numbers them as the perf configuration, input-capture and burn slices; their own decisions are still proposed): boot milestones, idle memory and the input-to-present trace. They also add a `burn` perf item: one spinning thread per online CPU inside an existing module, content-free and opt-in through 0019-D12's perf image. They go first, so this phase is measured before and after. RD8 was confirmed on 2026-10-12 without the daily resolution pin, so daily images keep pinning no mode, as 0015a confirms. The agent-demo build retires with the next spec (RS6).
2. NEW 0030a: a global process and thread table, run queues, wait queues instead of `wake_threads` polling, wakeup preemption, and MAX_THREADS above 4.
3. 0050, plus a kernel VA allocator for mappings made after the paging seal.
4. NEW 0059 kernel heap v2: grows on demand, adds a slab allocator, and charges kernel memory to the owning process.
5. 0054a, split from 0054: frame allocator v2 with per-frame refcounts, dynamic per-process limits, and S18's fixed reserve and OOM victim order (an OOM class set at spawn).
6. 0051: user processes on all CPUs, locking, user TLB shootdowns, and an NMI stop on panic. It also brings S17's crash screen: the reason and a backtrace; development builds halt, and release builds reboot through 0058a.
7. 0052, then 0053.
8. 0055a, split from 0055: a TSC time source calibrated against the HPET, catch-all vectors, and x2APIC handoff unless it already moved to Phase 0.

Exits:
- On the G11 VM, `MIRROR_SMP_READY` reports `userspace_cpus=` equal to `online=`. `status` shows user time on every CPU and each process's CPU time and committed memory.
- 200 keys are pressed while 0019d's `burn` item spins on every CPU. 0019c's trace shows p99 key-to-present under 30 ms (proposed) on the G11 VM; in QEMU `-smp 4` the p99 is reported, not gated. Today's allowance permits stalls of up to 680 ms. That figure is derived from userspace.rs:44-45 and has never been measured.
- `MIRROR_FPU_READY ... avx=1 ... osxsave=1` prints on VMware (expected, since the host has AVX2) and under QEMU `-ExtendedCpu`. The default QEMU CPU (qemu64) keeps `avx=0`, and that path stays tested.
- QEMU smoke cases pass, each with frame counts restored afterwards:
  - the thread battery under `-smp 4` with 16 threads in one process;
  - a process committing 256 MiB, in a smoke case booted with `-MemoryMiB 1024` that test.ps1 adds;
  - a heap-growth stress case.
- A panic fault case draws the crash screen and prints `MIRROR_CRASH_SCREEN_OK` after a boot-smoke framebuffer readback of the reason line. It stops every CPU and exits with code 35 and the expected diagnostic.
- An OOM fault case kills the background-class test process first and leaves the reserve intact.

### Phase 2: ABI, kernel objects and timers (8 specs, about 26)

Specs, in order:
1. 0020.
2. 0056a: the RTC read as UTC, plus the realtime call 0020 reserves.
3. 0021, written with per-CPU entry from the start.
4. 0022, 0023, 0024, 0025.
5. NEW high-resolution timers: one-shot or TSC-deadline LAPIC, tickless idle, sub-millisecond deadlines, and timer objects on 0024. A `timer` perf item runs the 1,000-wakeup probe.

Exits:
- On VMware with `rtc.diffFromUTC = "0"`, `status` shows UTC within 2 s of the host's.
- Native test programs enter through SYSCALL with 6 arguments. The demo desktop still runs through the INT 0x80 shim (S6a).
- Smoke cases pass:
  - handle transfer over a created channel;
  - wait-many with timeouts;
  - shared memory mapped in two processes, then revoked through its grant (S6e), with frame counts restored.
- Timers: the `timer` perf item's 1,000 one-shot 1 ms wakeups, stamped against the HPET. p99 lateness is at most 0.5 ms on the G11 VM (proposed). QEMU TCG reports the figure without gating on it.
- Tickless idle: an idle AP takes fewer than 5 timer interrupts per second, per `status`.

### Phase 3: Userspace platform (8 specs, about 34)

Specs, in order:
1. 0026, then 0028.
2. NEW ELF loader v2: images over 2 MiB, PT_TLS, a configurable main stack, shared read-only text, and room for PIE.
3. 0030: spawn, wait, kill, exit status, suspend and resume, and a principal field.
4. 0031.
5. 0032, including the session model: the desktop restarts, and Esc stops being one-way.
6. 0033.
7. 0034, with the log in memory.

If Cameron picks the RS5 alternative, 0089's core stage and user exception delivery join here, after the loader and 0030.

Exits:
- init spawns the shell and apps from the boot bundle, more than four programs, as ordinary processes. A smoke image over 2 MiB with a PT_TLS segment loads.
- After the shell is killed from the console, the service manager restarts it within 2 s (proposed).
- Esc to the console and back to the desktop works 10 times without a reboot.
- The system log can be read from the kernel console in a default build, and the hygiene capture still finds no user content.
- Userspace binaries are hard-float (SSE2). 0028's AVX2 dispatch takes the AVX2 path under `-ExtendedCpu`.

### Phase 4: Modern desktop at the VM window's size (16 specs, about 50)

Specs, in order:
1. NEW design note on driver placement and the display object (RD12).
2. 0027a, the protocol only.
   - It carries from day one:
     - shared-memory surfaces and N outputs;
     - output scale and per-surface buffer scale;
     - a clipboard object with a drag-and-drop data offer;
     - cursor-shape requests;
     - a screen-capture API limited to the human seat;
     - the input envelope with seat and provenance;
     - the reserved trusted top layer.
   - It lands with two kernel objects: a scanout object (Limine's GOP framebuffer mapped write-combined into the compositor) and an input object.
   - Syscalls 13-16 (`SURFACE_INFO`, `SURFACE_PRESENT`, `INPUT_WAIT`, `INPUT_POLL`) and the 1280x800 cap retire here.
   - It keeps 0019-D6's `frame` and `shell_ready` hooks.
3. 0084a:
   - keycodes and the full PS/2 key set (Up/Down, PgUp/PgDn, Insert, F1-F12, Super, numpad, locks);
   - the IntelliMouse and vmmouse wheel;
   - LEDs and shortcut routing;
   - typematic delay and rate and relative-pointer speed, persisted by 0068 and shown on 0088's Input page;
   - a manual VMware check that Kimi's Ctrl+Alt shortcuts reach the guest (RD20).
4. 0027b: app launch moves to userspace, and syscalls 17-22 retire.
5. NEW 0027c renderer:
   - premultiplied RGBA, antialiased rounded rectangles, cached soft shadows, dithered gradients and clipping;
   - filtered scaled blits and damage-limited composition;
   - the shared path rasterizer (fills, strokes, round caps) that 0086a also uses;
   - compositor frame-time hooks.
6. NEW 0027d: a frame clock and easing, with a reduced-motion flag. The compositor and apps block on events and timers, and nothing polls per tick.
7. 0086a, split from 0086: OpenType outlines on the shared rasterizer, one Latin UI font and one mono font, the nine type roles, and the icon set. 0086a owns icons.
8. 0039 (design only, A1a), then 0044a, split from 0044 (the A19 element-tree contract).
9. NEW toolkit core: layout, button, text field, scroll view, a focus model with focus-visible rings, theming from `mirror-ui-tokens`, and element trees per 0044a. Notes and Counter are ported onto it.
10. NEW 0027e window manager:
    - any number of windows, with resize, maximize, snap and z-layers;
    - cursor shapes and 0029-D9's 38 px titlebar geometry;
    - a minimal taskbar (Restart and Shut Down arrive together in Phase 5, when 0058 adds a power object with reboot and power-off rights that 0033 hands only to the shell);
    - an app-crash notice with a log entry.

    **From here, Kimi's visual language is buildable (at the GOP mode).**
11. 0060a, split from 0060: device objects, BAR sizing, runtime MMIO with write-combining, and polled lifetime.
12. 0087a, split from 0087: the TCLO core with reset and ping handling, `tools.set.version`, and capability registration (`resolution_set`, `display_topology_set`).
13. 0069:
    - mode setting, FIFO UPDATE rectangles and a hardware alpha cursor;
    - a FIFO-aware kernel console and crash screen (expected to be needed under SVGA II);
    - a QEMU `-vga vmware` profile;
    - topology requests with more than one display clamp to display 0.
14. 0087b, split from 0087: the host text clipboard.
15. NEW frame-time and idle budget:
    - p95 frame time for drag, maximize and animation at 1920x1080;
    - memory per window;
    - host CPU while idle.

    Then 0018's Mirror rows, re-recorded on this desktop (0018-D5's reference frame and 0018-D12's click targets).

Exits:
- In VMware, resizing the window or entering full screen changes the mode within 1 s (proposed), for example from 1920x1080 to 2560x1440 and back, with no border. A QEMU `-vga vmware` case automates mode set and UPDATE. Resize over TCLO is checked by hand.
- Capture checks with pixel comparisons:
  - 8 windows are open at once;
  - rounded corners, shadows and antialiased text at the nine type roles match reference crops;
  - maximize, snap left and right, and resize down to 240x150 work.
- Up/Down, PgUp/PgDn, F1-F12, Super and the wheel reach a test client, checked by smoke and capture.
- Text copied on the host pastes into Notes, and back (by hand).
- On the G11 VM, measured by 0027c's hooks and recorded in a G11 section of `docs/perf/budgets.md`:
  - p95 frame time for drag and maximize at 1920x1080 is at most 16.7 ms (proposed);
  - a desktop left idle for 60 s uses at most 2% of one host core in `vmware-vmx.exe` (proposed).
- The toolkit gallery is fully keyboard-navigable with visible focus rings, and its element-tree dump matches the screen.
- Killing a test app shows the crash notice and writes one log entry.

### Phase 5: Power (3 specs, about 53)

Specs, in order:
1. 0057.
2. 0058, with the shell's power object (reboot and power-off rights), and filesystem sync and the clean-pool marker as hooks only.
3. NEW 0087c, split from 0087:
   - TCLO `OS_Halt` and `OS_Reboot`;
   - `OS_Resume`: wall-clock resync, timer rearm, and a resume event that 0071 uses for DHCP renewal;
   - heartbeat and guest info.

Exits: each path below ends in a clean power-off or reboot with a clean-shutdown marker on serial.
- Shut Down and Restart from the taskbar.
- The ACPI power button. QEMU `system_powerdown` automates it, and VMware is checked by hand.
- VMware's Shut Down Guest and Restart Guest (`vmrun stop soft`, `vmrun reset soft`) through the lane. Whether VMware sends these to a guest without Tools is expected to depend on 0087a and 0087c reporting Tools as running. This is unverified.
- After `vmrun suspend` and `start`, the wall clock is within 2 s of the host's, and a pending 1 s timer fires.

### Phase 6: Files that survive reboot (14 specs, about 67)

Specs, in order:
1. NEW minimal tool runner: a text window that runs boot-bundle tools with arguments and shows their output. It also ships minimal `no_std` mirror-rt tools in the boot bundle: `pool create|status|replace` (owned with 0063), `write` and `cat` (with 0064), and `tz set` (with 0056b). Phase 7's 0072 and 0074 add `resolve`, `get <https-url>` and `clock set`. 0080 and 0082 later replace the runner, and 0081 replaces the tools (G10).
2. 0060b, split from 0060: MSI and MSI-X, INTx through `_PRT`, asynchronous completion, and a QEMU `-machine pc` (i440fx/PIIX) profile. VMware's virtual chipset is the 440BX/PIIX4, while the tests run q35.
3. 0061, adding GPT, boot-device identification and a buffer cache. Its I12 floor stays provisional.
4. 0062.
5. NEW AHCI QEMU boot image: QEMU boots from AHCI, so Mirror can read its own disk. It supersedes 0015-D10.
6. 0019b, split from 0019: C3 telemetry, now with multi-sector AHCI reads.
7. The storage stack, in order:
   1. 0067: its host tool copies the ESP and the ext2 system partition to every pool member (RD15).
   2. 0066a: the VFS and file-capability interface.
   3. 0063.
   4. 0064, with `large_file`.
   5. 0065.
   6. 0068, with locale formats and a human-seat write interface whose changes go to the system log (RD25).
   7. 0066b: persisted system log and telemetry; file change notification; the Home, Documents, Downloads and Images layout; filename search.
   8. 0056b: time zones.
8. 0034's crash-record path, after 0062.

Exits:
- A file written through the tool runner survives Shut Down. After `vmrun stop hard`, the journal replays at boot and the file is intact.
- With one pool VMDK removed, the system boots degraded and logs it. Once the disk returns, rebuild brings the member event counters back in step. The desktop notice arrives with Phase 8's notifications.
- 0067's host tool replaces the boot disk. The boot menu then offers the previous kernel, and it boots.
- The time zone persists across a reboot.
- QEMU boots from AHCI, and Mirror reads a marker file from its own ext2 system partition.
- A kernel panic leaves a crash record that appears in the system log on the next boot.

### Phase 7: Network (8 specs, about 75)

Specs, in order:
1. 0035.
2. 0036, with the seed persisted from day one.
3. 0070.
4. 0071, plus NEW socket objects (listen and accept, loopback).
5. 0072 and 0074, with the `resolve`, `get <https-url>` and `clock set` tools; 0075a, split from 0075: network capability objects.

Exits, on VMware NAT and in QEMU user-mode networking for the automated cases:
- A DHCP lease appears in the log, and it renews after suspend and resume.
- DNS resolves a public name.
- An HTTPS GET validates against the Mozilla roots, and a host with an expired certificate is refused.
- SNTP corrects a clock set 5 minutes off to within 1 s.
- Loopback listen and accept round-trips 1 MiB.

### Phase 8: Kimi's desktop shell (9 specs, about 84)

Specs, in order:
1. NEW toolkit widgets: menus, popovers, tooltips, tabs, switches, sliders, lists, progress and skeleton states, and drag sources and drop targets.
2. NEW scaling: 100, 125, 150 and 200%, crisp 1 px hairlines, cursor scaling, and a default scale derived from the output's size.
3. NEW notification service and center: a cap of 3, a 6 s auto-dismiss, and do-not-disturb with classes that always get through (A8a, G8d, A9a).
4. NEW app registry and file associations: app manifests for the start menu, dock and palette; file types; recent documents.
5. NEW open/save chooser (powerbox): it hands one file capability to an app.
6. NEW chrome:
   - a top bar with a clock;
   - the dock and a start menu with search;
   - a palette over apps, file names and commands;
   - quick settings without brightness or volume;
   - the switcher and three workspaces.

   **desktop-shell.html is buildable once items 1-5 exist, except its volume control, which waits for HD Audio (Phase 9) or goes under RS4's G12 exception, and glass (5.2). This spec builds it.**
7. NEW static boot splash with phase-based progress.
8. 0040: owner login and auto-login, plus screen lock.
9. NEW first-run setup: owner and password, time zone, keyboard layout (US until 0084b), and pool creation through the pool tool.

Exits:
- A fresh G11 VM runs first-run setup end to end. The next boot shows the login screen, or auto-logs in if that was chosen.
- Login leads to Kimi's dark desktop with every surface in item 6. Capture checks compare each surface against reference crops, which omit the volume slider and icon.
- Capture checks at 100, 125, 150 and 200%: hairlines stay 1 physical pixel, and text matches the reference crops at each scale.
- A degraded-pool notice gets through do-not-disturb, and a fourth notification queues behind the cap of 3.
- An app with no file grants opens a user-chosen file through the chooser, and no other file.
- The full toolkit gallery is keyboard-navigable, and its element-tree dump matches the screen.

### Phase 9: Apps and tools (12 specs, about 96)

Specs, in order:
1. 0089's core stage with user exception delivery, then its filesystem and network stages.
2. 0044b: the rest of 0044 (operation registry, streaming, cancel).
3. 0080, 0081. 0081 replaces the minimal tools from Phases 6 and 7 (G10).
4. 0084b (layouts, dead keys) and 0086b (fallback, CJK, emoji, shaping), both before 0082, 0083 and 0085.

   **From here, apps.html is buildable. 0082, 0085 and 0088 build it.**
5. 0082, 0085, 0088.
6. NEW HD Audio output, mixer and volume, if Cameron schedules it (RS4). It adds the volume slider to quick settings and the volume icon to the top bar.
7. NEW screenshots: PNG encode and decode, a screenshot tool on 0027a's capture API, and image preview in Files.
8. 0087: the HGFS remainder.

Exits, as named operations:
1. Terminal, opened from the start menu:
   - runs `ls`, `cp`, `mv`, `rm`, `mkdir`, `find`, `search`, `date`, `ps`, `kill` and pool status;
   - fetches an HTTPS file into Downloads;
   - Ctrl-C cancels a running fetch.
2. Editor: opens a file through the chooser, edits it with arrows and selection, copies and pastes with the host, and saves. The change survives a reboot.
3. Files: browses Home, opens a .txt in the editor by association, drags a file into the editor, renames and deletes a file, and previews a PNG.
4. Settings: switches to Light (S21) and changes scale and time zone. All three persist across a reboot.
5. Task monitor: lists processes with CPU and memory, and ends one.
6. A host shared folder appears in Files, and a file copies both ways (by hand).
7. Typing works with a non-US layout and dead keys, and CJK and emoji render in the editor.
8. A screenshot lands in Images as a PNG, and a capture request from a client that does not hold the human seat's capture right is refused.
9. If audio is scheduled, the volume slider changes the level of a test tone (QEMU `intel-hda` with a WAV audiodev).

### Phase 10: Harness product layer (12 specs, about 108)

Specs, in order:
1. 0041, 0042.
2. 0043, persisted from the start on 0066a's VFS.
3. 0045, 0046, 0047.
4. 0048: task objects, task grants and paused resume. Conversation persistence waits for 0098.
5. 0075b (agent egress grants, per-preset scope and audit), 0049, 0083, 0076, 0073.

Exits:
- An external agent pairs with a code shown on screen and approved in the trusted prompt, which answers only to the human seat. The agent gets a read-only principal.
- Its typed operations move a visible agent cursor, and the human's focus does not move.
- A capture request from an agent seat is refused.
- Pause, stop and revoke each take effect: the agent's next operation fails.
- After a reboot, the audit viewer shows the hash-chained log, and an offline edit to the log is detected.
- The assistant panel opens and closes, and an MCP client lists Mirror's operations through the adapter.

### Phase 11: AI and acceptance (18 specs, about 126)

Specs, in order:
1. 0054b: pressure notifications, before 0093.
2. 0055b: the rest of 0055.
3. M8 in its confirmed order. 0048's conversation persistence lands beside 0098.
4. 0015b, re-drafted and re-confirmed (RD1).
5. 0199.

Exit: 0199 passes. That covers:
- the four G7 gates against Linux and Windows in ULM on 0018's reference VM;
- the I15 local-inference floors and input latency under inference on the G11 VM;
- the Phase 4 frame-time and idle budgets;
- the daily-drive checklist.

### Risks the phases carry

- Phases 1-3 (about 25 specs) change little on screen. Each phase still ends in a check that can be run in VMware.
- 0030a and 0051 rewrite the core of `userspace.rs` under the full smoke suite. They need small steps and a full run per spec.
- Phase 4 replaces the demo shell that much of the tooling targets:
  - the captures and the hygiene probe;
  - 0017c's pointer helpers;
  - 0018-D5 and 0018-D12.

  Rewriting that harness is hidden work inside 0027a and 0027c.
- CPU-only composition at 1920x1080 has not been measured. The Phase 4 budget may force design trims: opaque surfaces and no glass.
- Some VMware behavior has no QEMU model. This includes TCLO resize, any Tools-running requirement, Shut Down Guest without Tools, and `restrict_backdoor` under ULM. The vmrun lane covers part of it, and the rest stays manual.
- There is one development agent. About 126 specs at roughly 15 minutes per full suite, plus manual checks, is a long elapsed time.

## 5. What changes from the confirmed plan

The map grows from about 88 remaining specs to about 126. 23 NEW specs fill gaps that no spec owned, and splits add 15. 0034 is staged across two phases, not split.

### 5.1 Moves

| Spec | Confirmed place | Proposed place |
| --- | --- | --- |
| 0015b | M0 | Phase 11, re-drafted (RD1) |
| 0019 | M0, last | 0019a, 0019c and 0019d open Phase 1; frame hooks in 0027c; 0019b after 0062 in Phase 6 |
| 0050, 0051, 0052, 0053, 0055a | M4 | Phase 1 |
| 0054 | M4 | 0054a in Phase 1 (with the S18 reserve and OOM order); 0054b in Phase 11 |
| 0021 | M1, before 0050 | Phase 2, after 0050, per-CPU from the start |
| 0056 | M4 | 0056a in Phase 2; 0056b in Phase 6 |
| 0030-0034 | M2 | Phase 3, before 0027a |
| 0027a | M1 | Phase 4, protocol only |
| 0027b | M2 | Phase 4, right after 0027a and 0084a |
| 0039, 0044a | M2, M3 | Phase 4, before the toolkit core |
| 0060a, 0069 | M5 | Phase 4 |
| 0087a, 0087b | M7 | Phase 4 (0087a moves to Phase 0 if 0017c check 7 needs it) |
| 0084a, 0086a | M7 | Phase 4 |
| 0057, 0058 | M4 | Phase 5 |
| 0067, 0066a | M5 | Before 0063 and 0064 respectively |
| 0035, 0036 | M2 | Phase 7 |
| 0075 | M6 | 0075a in Phase 7; 0075b in Phase 10, after 0041 and 0043 |
| 0040 | M3 | Phase 8 |
| 0044b | M3 | Phase 9, before 0080 |
| 0089's core stage | M7 | Start of Phase 9 (Phase 3 under RS5's alternative) |
| 0041-0043, 0045-0049, 0083, 0076, 0073 | M3, M6, M7 | Phase 10 |
| 0048's conversation persistence | M3 | With 0098, Phase 11 |
| 0018's Mirror rows | After 0019, M0 | End of Phase 4, re-recorded on the new desktop |
| G11 profile in `docs/vm-settings.md` | 0067 | Phase 0, docs-only |

### 5.2 Cuts and deferrals

- 0029-D9's geometry is withdrawn (its frame colors shipped in part 1), and the 38 px titlebar moves to 0027e.
- There is no more work on the demo renderer, and no further restyling of the kernel console.
- The animated boot sequence becomes a static splash (review note 7).
- Glass waits for the Phase 4 budget, and then covers at most the dock and panels.
- The brightness slider is dropped.
- The agent-demo build retires at 0030a (RS6).
- The items in section 7 go on the record as after v1 or unscheduled.

### 5.3 New specs and splits

| NEW spec | Phase | Owns |
| --- | --- | --- |
| vmrun lane: 0015c host lane and 0015d VMware cases | 0 | Headless VMware boot, restart, and suspend/resume checks |
| 0059 kernel heap v2 | 1 | On-demand growth, slab allocator, kernel memory charged to its owner |
| High-resolution timers | 2 | One-shot or TSC-deadline LAPIC, tickless idle, timer objects |
| ELF loader v2 | 3 | Images over 2 MiB, PT_TLS, stack size, shared text, room for PIE |
| Driver placement and display object note | 4 | RD12 |
| 0027c renderer | 4 | Composition, the shared path rasterizer, frame hooks |
| 0027d frame clock | 4 | Easing, reduced motion, event-driven frames |
| Toolkit core | 4 | Basic widgets, focus model, element trees |
| 0027e window manager | 4 | Windows, resize, snap, layers, cursor shapes, crash notice |
| Frame-time and idle budget | 4 | p95 frame times, memory per window, host CPU while idle |
| Minimal tool runner | 6 | A command surface and minimal boot-bundle tools (pool, write, cat, tz) |
| AHCI QEMU boot image | 6 | Mirror reads its own disk in tests |
| Socket objects | 7 | Listen and accept, loopback |
| Toolkit widgets | 8 | The rest of Kimi's controls, drag sources and drop targets |
| Scaling | 8 | 100-200% |
| Notification service and center | 8 | Cap, timeout, do-not-disturb classes |
| App registry and file associations | 8 | Manifests, file types, recent documents |
| Open/save chooser | 8 | File capabilities handed to apps |
| Chrome | 8 | Top bar, dock, start menu, palette, quick settings, switcher, workspaces |
| Static boot splash | 8 | Phase-based progress |
| First-run setup | 8 | Owner, password, time zone, layout, pool |
| HD Audio output, mixer, volume | 9 | Recommended under G12 (RS4) |
| Screenshots | 9 | PNG codec, capture tool, image preview |

| Split | Parts |
| --- | --- |
| 0019 | 0019a perf configuration, milestones, memory and delivery (Phase 1); 0019c finite input capture (Phase 1); 0019d burn (Phase 1); 0019b C3 telemetry (Phase 6) |
| 0030 | 0030a table and scheduler core (Phase 1); 0030 process model (Phase 3) |
| 0034 (staged, not split) | log in memory (Phase 3); crash-record path (Phase 6, after 0062) |
| 0044 | 0044a element-tree contract (Phase 4); 0044b registry, streaming, cancel (Phase 9) |
| 0054 | 0054a allocator, limits, reserve, OOM order (Phase 1); 0054b pressure notifications (Phase 11) |
| 0055 | 0055a TSC and catch-all vectors, plus x2APIC if needed (Phase 1 or 0); 0055b the rest (Phase 11) |
| 0056 | 0056a RTC as UTC (Phase 2); 0056b time zones (Phase 6) |
| 0058 | 0058a reboot without AML (Phase 0); 0058 orderly shutdown (Phase 5) |
| 0060 | 0060a device objects and MMIO (Phase 4); 0060b MSI and INTx (Phase 6) |
| 0066 | 0066a VFS and file-capability interface (Phase 6, before 0064); 0066b persisted system log and telemetry, file change notification, Home layout, filename search (Phase 6) |
| 0075 | 0075a network capability objects (Phase 7); 0075b agent egress grants, per-preset scope and audit (Phase 10, after 0041 and 0043, before 0076) |
| 0084 | 0084a keycodes, key set, wheel (Phase 4); 0084b layouts, dead keys (Phase 9) |
| 0086 | 0086a core text and icons (Phase 4); 0086b fallback, CJK, emoji, shaping (Phase 9) |
| 0087 | 0087a TCLO core (Phase 4); 0087b clipboard (Phase 4); 0087c power and resume (Phase 5); 0087 HGFS (Phase 9) |

## 6. Decisions for Cameron

RS and RD are this proposal's numbers. They are not ledger IDs. A row Cameron has confirmed says so in its Due cell, with the date. Every other row stays proposed.

### 6.1 Strategic

| # | Decision | Recommended | Why | Due |
| --- | --- | --- | --- | --- |
| RS1 | Foundations before the harness product layer: replace M1-M9, the README spec map and the preamble "The harness follows the process model (A15)" (decisions.md:165) with Phases 0-11 | Yes. v1's contents in 0000-north-star do not change, except as RS4 and RD18 record (detail in RD2). Ledger S6 is reworded: the ABI reset (0020-0025) follows the Phase 1 kernel core. Phase 1 adds no syscall and changes only the thread-protocol and vm-protocol limits. Its new test code extends init's existing battery | Otherwise the M1-M3 harness UI is built on a 5x7 font and the demo input path, and is likely redone in M7 | Before Phase 1; confirmed (2026-10-11) |
| RS2 | Storage (Phase 6) and network (Phase 7) before Kimi's shell (Phase 8) | Yes. If Cameron wants the shell sooner, Phase 8 items 1-3, 6 and 7 can follow Phase 5, with the start menu and dock reading manifests from the boot bundle and the palette covering apps and commands only. Items 4, 5, 8 and 9 (app registry, chooser, 0040 and first-run setup) still follow Phases 6 and 7, because 0040 needs 0036 (S14) and the chooser needs 0066a | Matches G4 and readme.md:5. The shell and apps are then written once, against real files and settings | Before Phase 6; confirmed (2026-10-11) |
| RS3 | Withdraw 0029-D9's geometry | Yes. Merge part 1 as committed. Withdraw the 38 px titlebar, the 38x28 caption buttons, the app tile, the drag region and the moved click points, and move the titlebar geometry to 0027e. Its frame colors (border-strong focused, border-subtle unfocused) stay, since part 1 ships them. Update 0029's tests 2, 4 and 6 and the D9 geometry lines of its Interfaces | 0029-D9's geometry restyles a renderer that Phase 4 replaces, and it ties the demo shell into 0017c's and 0018's captures | Now, with the 0029 merge; confirmed (2026-10-11) |
| RS4 | Audio and the other unowned G12 basics | Schedule NEW HD Audio output, mixer and volume after 0060b (Phase 9). Multiple monitors, screen reader, IME and non-English UI go to After v1 (RD18). Drop the brightness slider. Alternative for audio: an explicit G12 exception under "Not in v1" | G12 says Mirror provides what every modern OS has. VMware's HD Audio and QEMU's `intel-hda` make audio testable | Before Phase 8; confirmed (2026-10-11) |
| RS5 | Runtime target for system code | The compositor, toolkit, text stack and services are `no_std` + alloc on mirror-rt. 0089's std core stage opens Phase 9, before the tools (S8 holds). Alternative: the std core stage in Phase 3 | `no_std` crates link into std programs unchanged, so nothing is rewritten. Porting std means adding a Mirror platform layer to std's source, which is XL work, and this keeps it off the desktop's critical path | Before Phase 3 |
| RS6 | Retire the agent-demo build | Yes, when 0030a replaces `run_desktop` (Phase 1). AGENTS.md drops `test.ps1 -AgentDemo` (P9) until Phase 10 brings agents back. Alternative: keep it until 0023 (Phase 2) | 0030a rewrites the session loop it runs in, 0023 replaces its two fixed channels, and each run costs 15-20 minutes | Before Phase 1; confirmed (2026-10-12) |

### 6.2 Detailed

| # | Decision | Recommended | Why | Due |
| --- | --- | --- | --- | --- |
| RD1 | M0 order (README.md:28) | Keep 0015a, 0013 and 0016a-0017c as confirmed. Move 0015b to Phase 11, and split 0019 (RD8). Add the vmrun lane after 0015a (it yields to 0013 if not yet confirmed), the G11 doc commit, and NEW 0058a after 0017c. Drop the README's "its D9 lands with 0017c". Moving 0015b past 0062 re-opens it: before Phase 11, 0015b is re-drafted and re-confirmed. Its `-Iso` case runs under V1, with 0062 relaxing the VirtIO smoke assert (kernel/src/main.rs:206-209) as 0014's follow-up says. It also states which RD14 assets (root store, tzdata, CJK fonts) the live ISO carries, or that the live system runs without them. Alternative: keep 0015b in Phase 0 right after 0013, before 0062 | 0015b is not needed for daily VMware use. 0013 is needed for every multi-CPU VMware boot | The 0015b move before 0015a ends; the rest before 0013 ends. The move was confirmed (2026-10-11). The vmrun lane, the G11 doc commit and 0058a were confirmed (2026-10-12), and the 0019 split was confirmed with RD8. 0015b's re-draft content (`-Iso` under V1, the RD14 assets) is decided with the re-draft |
| RD2 | A15, A9a | Split them. The kernel primitives stay early: S6e grants (0022), suspend and a principal field (0030), seat and provenance (0027a), and the trusted layer reserved in 0027a and 0027c. The product layer goes to Phase 10, and 0043 is persisted from the start. A21 is handled in RD25 | The harness's kernel hooks are designed once, and its UI is built once | Before Phase 1; confirmed (2026-10-12) |
| RD3 | S6f | 0027a as in Phase 4, item 2. It retires syscalls 13-16 and the 1280x800 cap, as S1c already says. 0027b retires 17-22. New 0027c, 0027d and 0027e. 0084a lands right after 0027a and before 0027b and any toolkit code, and replaces the envelope payload. Only the ported shell ever uses the old payload | No protocol revision is needed for scaling or drag and drop, and the shell keeps a working screen path at every step | Before Phase 4 |
| RD4 | Toolkit before the window manager | 0039 and 0044a move to Phase 4. The NEW toolkit core comes before 0027e, and the rest of the widgets come in Phase 8 | Phase 4's apps and window chrome are written once | Before Phase 4 |
| RD5 | S11 | Split 0086 into 0086a (Phase 4) and 0086b (Phase 9), both before 0082, 0083 and 0085. 0086a owns icons, drawn on 0027c's path rasterizer | Real text and icons exist before any chrome is built | Before Phase 4 |
| RD6 | S12, I6, S18 | 0053 goes in Phase 1, before 0028. 0054a carries the S18 reserve and OOM order. 0054b keeps only pressure notifications | Phase 1 lifts the limits, so a runaway app could otherwise starve the compositor | Before Phase 1; confirmed (2026-10-12) |
| RD7 | S15, S16, S17 | 0056a in Phase 2 and 0056b in Phase 6. 0058a in Phase 0. 0057 and 0058 in Phase 5, before storage, with 0063 and 0064 owning the sync and clean-pool tests. The S17 crash screen in 0051. The app-crash notice and its log entry in 0027e | Reboot needs no AML, and each piece lands where its parts exist | Before Phase 0's 0058a; confirmed (2026-10-12) |
| RD8 | 0019 split (Pending item 1) and a daily resolution pin | 0019a opens Phase 1, and 0019b follows 0062. If a daily pin is wanted, 0019a pins 1280x800 (0019's draft already plans to write Limine's `resolution:` for the perf image, 0019:109), 0027a moves the pin to 1920x1080, and 0069 removes it. Limine falls back to another mode when the requested one is missing, and `MIRROR_FRAMEBUFFER_READY` shows which. 0015a builds as confirmed. If the pin is taken, 0019a amends 0015a's confirmed lines "The default image pins no mode" and "Daily boots keep the default image, which pins no mode", re-confirmed with 0019a's decisions | Every latency, jitter and frame-time exit has its instrumentation in place before it is measured | Before Phase 1; confirmed (2026-10-12) without the daily pin |
| RD9 | S14 | 0035 and 0036 go to Phase 7, still before 0040 and 0043 | Nothing consumes them before the network and login | Before Phase 7 |
| RD10 | S1c, Pending item 4 | Option (a): 0087a, with reset and ping, `tools.set.version` and capability registration, comes before 0069. 0069 then depends on 0060a only. If 0017c check 7 needs a Tools-running state for the ungrab, 0087a moves into Phase 0 | VMware sends resize requests only over TCLO and may expect Tools to be running first (expected, unverified) | Before Phase 4, or at 0017c check 7 |
| RD11 | x2APIC handoff | If 0013's tests 14 and 16 show an x2APIC handoff, 0055a takes it and moves into Phase 0, after 0013 | Mirror fails closed on `x2apic_active` (apic.rs:338-339), which would keep VMware on one CPU | At 0013's VMware checks |
| RD12 | Driver placement (new ledger entry) | v1 drivers stay in the kernel and export capability objects: scanout memory, raw block and packet. S13's block, filesystem and network services run above them in userspace | Mirror's VMs enable no virtual IOMMU. VMware's "Virtualize IOMMU" option and QEMU's `intel-iommu` exist, but they are off, and Mirror has no IOMMU driver. A userspace bus-master driver would therefore be root-equivalent (A9, O7). An IOMMU path is post-v1 | Before Phase 4 |
| RD13 | 0015-D10 | 0015a is built as confirmed. The NEW AHCI boot image supersedes 0015-D10 in Phase 6 | Mirror cannot read its own disk in QEMU until then | Before Phase 6 |
| RD14 | System assets and the purpose of the ext2 root (G9) | Latin fonts and icons ship in 0031's boot bundle. CJK and emoji fonts, tzdata and the root store live on the boot disk's ext2 system partition. Mirror reads them after 0064, and 0067's tool replaces them along with the kernel | Gives 0015a's ext2 root a job, and keeps large fonts out of boot RAM (gates 1 and 2) | Before Phase 6 |
| RD15 | G8d vs S2a | 0067's host tool copies the ESP and the ext2 system partition to every pool member, so any disk boots with RD14's assets and Mirror never writes FAT32. Reword G8d to match | Removes a conflict between two confirmed entries before 0063 | Before Phase 6 |
| RD16 | Where G11's profile lives | The G11 VM, with `rtc.diffFromUTC = "0"`, enters `docs/vm-settings.md` in Phase 0. It carries every reference-VM key (`hpet0.present`, no USB controller, `uuid.action`, `msg.autoAnswer`, the serial-file answer) and 0018's RemoteDisplay.vnc keys. 0067 keeps the pool disks. G11's "(0067)", S15's "later 0067's daily-drive profile", 0015a's non-goal at 0015a:18 and 0015-D14's "and, later, 0067's daily-drive profile" are reworded to match (docs-only) | Phases 0, 1 and 4 check their exits on it | With RD1; confirmed (2026-10-12) |
| RD17 | G7 | No change. The frame-time and idle budgets are acceptance budgets in the Phase 4 spec, and 0199 checks them again | The gates stay as confirmed | Before Phase 4 |
| RD18 | Unowned basics, on the record | Screen reader after v1 (the A19 element tree serves as the accessibility tree). IME after v1. English-only UI for v1. Locale formats (12/24-hour, date order) in 0068. Section 7's items join README's lists | Under G12, silence counts as a gap | Before Phase 8 |
| RD19 | 0048 and A18 | Tasks, task grants and paused resume in Phase 10. Conversation persistence with 0098 | CLM context objects do not exist before 0098 | Before Phase 10 |
| RD20 | Kimi's Ctrl+Alt shortcuts | 0084a adds a manual VMware check. If VMware's Ctrl+Alt hot-key prefix takes them, the defaults move to Super | Whether they reach the guest is unverified | Before Phase 4 |
| RD21 | README lines for 0050 and 0021 | 0021 is written per-CPU from the start, after 0050, and 0050 drops its rework clause | 0021 is built once | Before Phase 2; confirmed (2026-10-12) |
| RD22 | User exception delivery | Joins 0089's core stage (Phase 9, or Phase 3 under RS5's alternative) | std needs it to detect stack overflow, and nothing earlier does | With RS5 |
| RD23 | Kimi's pages | Drop the brightness slider. Update "Where each part lands" in `docs/design/README.md` to the new owners (docs-only, P2a) | A VM has no backlight, and the page names the old owners | After confirmation |
| RD24 | NEW G12 owners | Approve the NEW specs and splits in 5.3. Cameron assigns the numbers | Each one fills a gap no spec owned | Phase by phase; confirmed (2026-10-12) for the vmrun lane as 0015c and 0015d, 0019's Phase 1 part as 0019a, 0019c and 0019d, 0030a, and kernel heap v2 as 0059; RD6 and RD7, confirmed the same day, confirm the 0054, 0056 and 0058 splits by name |
| RD25 | A21 | 0068 lands in Phase 6, before principals, the operation registry and seats. It ships a human-seat write interface whose changes go to the system log. 0044b registers those writes as typed operations and 0043 audits them, in Phases 9-10. Alternative: move 0068 after 0044b | Confirmed A21 requires human-seat typed operations and an audit of every change, and neither exists in Phase 6 | Before Phase 6 |

## 7. Explicitly after v1 or unscheduled

After v1 (add to README.md:128-137):
- Multiple monitors. 0027a still models N outputs, and 0069 clamps topology requests to display 0.
- Screen reader (the A19 element tree is its basis), and input methods (IME) for CJK.
- UI languages other than English.
- The in-Mirror installer. S5a already decides this, but it is missing from the list.
- Third-party app installation (the premise of S9).
- Drag and drop between host and guest.
- Printing.
- Audio input (microphone).
- NVMe (S1a says "later"), VMXNET3, PVSCSI and the memory balloon.
- USB beyond the boot probe: keyboards, hubs and mass storage, in addition to the listed HID pointing.
- Audio, only if Cameron takes the G12 exception in RS4.

Unscheduled (add to README.md:139-150):
- Secure Boot.
- Swap, a page cache and file-backed mappings.
- At-rest encryption of user data. A16b covers only the keyring.
- KASLR and user ASLR. If user ASLR is pulled into v1, 0036's CSPRNG core moves to Phase 2, and ELF loader v2 randomizes PIE.
- A virtual IOMMU and userspace drivers (RD12).

## 8. Next steps

1. **Close 0029.** Both suites passed on `claude/0029-ui-tokens` (2654a75, 7c32fdb). Cameron reviews 0029's test 6 screenshots and merges the branch as committed. RS3 was confirmed on 2026-10-11. The 0029 update that withdraws D9's geometry (its status, tests 2, 4 and 6, and an implementation note in its Interfaces) goes to `main` with the `claude/0029-ui-tokens` merge, so no separate docs-only commit is needed. Local `main` (a8a2581, with the 0010 stack merged) is not pushed yet; push it when Cameron asks.
2. **Start 0015a now**, on `claude/0015a-disk-image`. It depends only on 0014 and 0010, which are both merged. 0015a builds as confirmed. After it merges, the Phase 0 G11 docs commit (RD16) also rewrites 0015a's Non-goals G11 bullet and 0015-D14's "and, later, 0067's daily-drive profile". The ledger's G11 and S15 rows were reworded on 2026-10-12.
3. **Then 0013, as confirmed.**
   - The first part of RD1, confirmed on 2026-10-11, moved 0015b to Phase 11 (re-drafted and re-confirmed before then), so 0013 follows 0015a directly.
   - 0013 comes next because Mirror has never booted on VMware. Static analysis predicts one CPU on the PIC until 0013 lands, and every later exit is checked in VMware.
4. **In parallel, as docs-only tasks:**
   - Draft NEW 0030a, so Phase 1 starts on a confirmed spec. Kimi drafted it, and Cameron confirmed its decisions on 2026-10-12.
   - Draft the vmrun lane and 0058a if Cameron wants them in Phase 0. Cameron confirmed both for Phase 0 on 2026-10-12 (RD1, RD7). Codex drafted the lane, which becomes 0015c and 0015d (RD24), and Cameron confirmed its decisions the same day.
5. **What waits for Cameron:**
   - Before 0013 ends: the rest of RD1 (the vmrun lane, the G11 doc commit, 0058a and the 0019 split) and RD16. The lane, 0058a and the G11 doc commit can then slot into Phase 0. Cameron confirmed the rest of RD1 and RD16 on 2026-10-12, with RD7, so all three join Phase 0.
   - Before Phase 1: RS6, RD2, RD6 and RD8, 0019a's proposed decisions (Pending item 1), and the 0030a draft. Cameron confirmed RS6, RD2, RD6, RD8 (without the daily resolution pin) and 0030a's decisions on 2026-10-12, with 0050's and kernel heap v2's decisions and RD24's numbers for the vmrun lane (0015c, 0015d), 0019a, 0019c, 0019d, 0030a and kernel heap v2 (0059). The 0019a, 0019c and 0019d drafts' own decisions are still proposed, and so are 0054a's.
   - Settled by checks rather than discussion: RD11 by 0013's tests 14 and 16, and 0087a's placement (RD10) by 0017c check 7.
   - RD21, due before Phase 2, was confirmed early, on 2026-10-12. Everything else is decided when its phase comes up, as the Due column says.

## 9. Critic items not taken

Every critic item is taken. Two are taken with a change:
- The vmrun lane goes right after 0015a, but it yields to 0013 if it is not confirmed by then, so 0013 is never blocked.
- User ASLR is listed as unscheduled. 0036's CSPRNG core stays in Phase 7 unless Cameron pulls user ASLR into v1.

## Appendix: the three approaches

Scores are out of 5, in this order: dependency correctness, time to a usable desktop, time to a Kimi-capable desktop, risk (5 = low), and fit with the north star.

| Approach | Shape | Strength | Weakness | Score |
| --- | --- | --- | --- | --- |
| 1. Kimi-shell path | Kernel core, objects, platform, compositor, SVGA, text and toolkit, then the shell (about 43 specs); storage, harness, network and apps after it | Fastest route to the design | Builds the look before files, power and network. 0053 waits until near the end, so AVX2 goes untested for about 60 specs | 4/2/5/2/2 = 15 |
| 2. Foundation first | Strict dependency order: kernel, objects, time and power, drivers and storage, then display, network, desktop, apps, harness, AI | Most correct order; best storage internals | Display arrives last (about 60 specs). 0069 waits for AML and all of 0060, although SVGA II can be polled | 5/2/2/3/5 = 17 |
| 3. Daily driver (chosen) | Every phase ends in something Cameron can do in VMware | The only one with a window-sized desktop and real text right after the kernel | It put the compositor before 0030, 0031 and 0033, put 0069 outside the driver model, and put the window manager before the toolkit. All three are fixed here | 3/5/3/3/4 = 18 |

What the chosen spine takes from the others:
- From approach 1: the process model before the compositor, the 0060a split, 0084a's payload right after 0027a, and 0044a before the toolkit.
- From approach 2: the storage internals (0067 before 0063, 0066a before 0064, `large_file`, and GPT in 0061), network before the shell, std before the apps, and persistence before 0035, 0036 and 0040.
