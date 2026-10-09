# 0018: Performance baseline harness

Status: confirmed (2026-10-09)
Depends on: Linux and Windows baselines: VMware Workstation Pro installed on the host (owner; not installed today). Mirror rows: 0010 (no init battery at boot), 0013 (VMware firmware and PCI tolerance), 0015a (disk image), 0019 (perf markers); the agent row also needs 0045. Used once landed: 0011 (toolchain preflight), 0014 (QEMU device profile).
Ledger: G7, O3, M0; uses G3a, S1a, S3, S7, S7b

## Goal
A repeatable method, with tooling, that measures the four v1 performance gates on Ubuntu, Windows 11 and Mirror in one reference VMware VM, with the same host-side instruments for all three. When done, the baseline VMs are documented and rebuildable, `scripts/perf.ps1` and a native host tool (`tools/perf-probe`) run the measurements mostly unattended, and `docs/perf/baselines.md` holds the first Ubuntu and Windows numbers for all four gates, the input for O3. Mirror rows use the same method once 0010, 0013, 0015a and 0019 land.

## Non-goals
- Setting target numbers (O3 is answered afterwards, in a ledger update and 0199).
- Emitting Mirror's perf markers, parsing `mirror.perf=`, and the `shell_ready` source: 0019 implements the kernel side of D9 against the marker contract below. Device and link telemetry (ledger C3) is also 0019's.
- Optimizing Mirror; throughput gates (disk, network, tokens/s) or app launch time.
- Gate numbers from VirtualBox or QEMU (QEMU runs a non-gating check lane only).
- True photon latency; scripted OS installation (a manual recipe is used); CI (P3).

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| 0018-D1 | The four gates are boot to desktop, idle memory, input latency and agent action latency, measured against Linux and Windows in the same VM configuration. | confirmed (ledger G7) |
| 0018-D2 | VMware is the primary hypervisor and QEMU is the automated test platform. | confirmed (ledger G3a) |
| 0018-D3 | Reference VM, identical for all three OSes: VMware Workstation Pro (build recorded and frozen per comparison) on Cameron's Windows 11 host, headless (`vmrun start <vmx> nogui`); 4 vCPUs (1 socket), 8 GiB RAM, UEFI with Secure Boot off (Limine-booted Mirror is unsigned), a vTPM (Windows 11 requires one), one 64 GiB preallocated SATA disk, one NAT NIC `ethernet0.virtualDev = "e1000"` (82545EM, as in 0014), 1280x800 display, no sound or printer. Accepted exception: Mirror's VM boots 0015a's `mirror.vmdk`, which is 1 GiB under 0015-D5, not 64 GiB; everything else is identical. Rationale: Windows 11 runs representatively at this size, S1a keeps Mirror's drivers usable, preallocation cuts disk variance. Main alternative: the I4b daily-drive size (20-24 GiB RAM), or the Windows 11 minimum (2 vCPUs, 4 GiB). | confirmed (2026-10-09) |
| 0018-D4 | Baseline images: stock Ubuntu Desktop 26.04 LTS, default install (GNOME on Wayland, default apps, open-vm-tools); stock Windows 11 Pro, current GA feature update, default install with VMware Tools, a local account, Defender real-time protection on. Both fully updated, then updates paused for the measurement window; auto-login; caret blink off. 3D acceleration on (VMware's default for these guests; Mirror's VM gets it too and ignores it). Rationale: compare against what a user runs, not a tuned build. Main alternative: Fedora Workstation or a lighter desktop, or baselines cut to Mirror's device set. | confirmed (2026-10-09) |
| 0018-D5 | Boot to desktop is observed from the host for every OS. Start: host timestamp just before power-on. End: the first VNC frame whose masked difference from the image's reference desktop frame (clock, cursor, watermark masked) is below a threshold. Guest-reported times are diagnostics only. Rationale: only the host sees firmware and loader time the same way for all three; `systemd-analyze` has none under GRUB, Windows event 100 excludes firmware, and Mirror's HPET zero is at `clock::init` (kernel/src/main.rs:136; counter reset at kernel/src/clock.rs:228). Main alternative: a guest-side "session ready" signal per OS. | confirmed (2026-10-09) |
| 0018-D6 | Idle memory = the VM's configured RAM minus what the OS reports as available without paging: Linux MemAvailable; Windows `\Memory\Available Bytes`; Mirror free frames (recycled plus never allocated, as `MIRROR_FRAME_POOL` sums them, kernel/src/main.rs:126-132). Visible totals and commit charge are recorded, not gated. Rationale: visible totals differ by OS (Linux MemTotal omits the kernel image, linux Documentation/filesystems/proc.rst:1122-1124; Mirror's pool omits bootloader-reclaimable memory it never frees, kernel/src/memory.rs:1-6), so a fixed base is fairer. Main alternative: host-side resident memory of the VM process. | confirmed (2026-10-09) |
| 0018-D7 | Input-latency proxy: the host injects the key or click as an RFB event into the hypervisor's built-in VNC server and timestamps the first framebuffer update that changes a probe rectangle. Pointer motion is not measured (with VMware Tools the host draws the cursor). The number includes the hypervisor display path, which the user sees, and excludes the host compositor and monitor. Main alternative: host `SendInput` into the VMware window plus DXGI Desktop Duplication (closer to photon latency, but needs an interactive foreground session and is quantized to the host refresh rate). | confirmed (2026-10-09) |
| 0018-D8 | Agent-action baseline. Action: read the target document's full text, replace it with a fixed 32-character string, receive confirmation; the target reference is obtained before timing. Baselines are measured through the guest accessibility API (Windows UI Automation, Linux AT-SPI) and through pixel-based computer use driven from the host over VNC. The pixel number is the mechanical floor (capture, input, confirming frame) and excludes model and OCR time. Main alternative: include one call to a reference vision model, whose latency comes from the provider, not the OS. | confirmed (2026-10-09) |
| 0018-D9 | Mirror perf markers are compiled into every build; the heavier ones (memory samples, input trace, display calibration, agent bench) are switched on by `mirror.perf=` command-line tokens, so daily and measured boots run the same kernel binary. No marker carries user content (S7). Ledger S7b records these opt-in markers as the one exception to 0010-D4's rule that default-build serial output never depends on user input. 0019 implements the kernel side. Main alternative: emit them only from a separate build (`test-trace` or a `perf` feature). | confirmed (2026-10-09) |
| 0018-D10 | Tooling: a std-only native Rust host tool, `tools/perf-probe`, with its own minimal RFB client, driven by `scripts/perf.ps1` and `vmrun`. Rationale: S3 favors native, and the RFB subset needed (security None, Raw encoding, key and pointer events, incremental updates) is small. Main alternative: Python with vncdotool (MIT) and Pillow (HPND). | confirmed (2026-10-09) |

## Interfaces
### Gate methods
Host times use QueryPerformanceCounter (QPC). Every result reports N, median, p95, min and max.

1. **Boot (D5).** `perf-base` is a powered-off snapshot taken after a full shutdown (Windows: `shutdown /s /t 0`, so Fast Startup does not apply), after at least 3 settling boots. Each boot starts from `vmrun revertToSnapshot <vmx> perf-base`; 1 discarded boot, then 10 measured. `perf-probe boot` polls VNC from power-on. Diagnostics: Mirror, the host arrival time of the first serial line (kernel/src/main.rs:53) and `MIRROR_PERF_T`; Ubuntu, `systemd-analyze` and `critical-chain`; Windows, Diagnostics-Performance event 100 (BootTime, MainPathBootTime, BootPostBootTime).
2. **Idle memory (D6).** No input after the boot end event; a sample every 10 s from minute 10 to 12; a boot's value is its median; 5 boots. Mirror: `MIRROR_PERF_MEM`. Ubuntu: a shell loop over /proc/meminfo (also MemTotal, Committed_AS). Windows: `typeperf` (Available Bytes, Committed Bytes) and `Win32_OperatingSystem.TotalVisibleMemorySize`. Guest samplers start with `vmrun runProgramInGuest`, results return with `CopyFileFromGuestToHost`, and their own footprint is recorded.
3. **Input latency (D7).** Target: a focused plain-text document in the default editor (Notepad, GNOME Text Editor, Mirror's text app at the time). perf-probe keeps one incremental update request outstanding.
   - Keypress: from writing the RFB KeyEvent(down) to the first update in which at least K pixels (default 16) of a probe rectangle at the caret differ from the pre-input frame; K and the rectangle go in the CSV header. Keys alternate `a` and BackSpace with random 250-750 ms gaps; 20 warm-up and 200 measured keys per session; 3 sessions.
   - Click: pointer settled on the launcher (Start button, GNOME Activities, Mirror's equivalent); from button-down to the first changed frame in the launcher's response rectangle; an untimed Escape closes it. 100 clicks per session; 3 sessions.
4. **Agent action (D8).** 20 warm-up and 200 measured iterations per session (pixel path: 50); 3 sessions; the text alternates between two fixed strings.
   - Mirror: the Agent API (0045) bench client that `mirror.perf=agent` starts (0019) does a typed Read then Replace, timed with `SYS_CLOCK_GET` (crates/clock-protocol/src/lib.rs:10) or its `mirror-sys` successor.
   - Windows: C# compiled in the guest by PowerShell 5.1 `Add-Type` (inbox .NET Framework 4.8.x): `TextPattern.DocumentRange.GetText(-1)`, then `ValuePattern.SetValue`, timed with `Stopwatch`.
   - Ubuntu: Python with pyatspi: `Text.getText(0, -1)`, then `EditableText.setTextContents`, timed with `time.perf_counter_ns`.
   - Pixel path, from the host: read = receive one full frame; act = the app's select-all shortcut, then 32 key presses; confirm = the first frame whose document rectangle matches the expected-text template captured at setup.

### Mirror marker contract (D9; emitted by 0019)
The harness reads these serial lines. `ns` is HPET time since `clock::init`, read with `clock::snapshot` (kernel/src/clock.rs:295-316, BSP with IF clear only), or `none` after `MIRROR_CLOCK_SKIPPED`.
- `MIRROR_PERF_T event=<display|input|userspace|shell_ready> ns=`, always on, each once, in order: framebuffer ready (today kernel/src/main.rs:160); PS/2 init done (kernel/src/main.rs:253-279); first user process started (the shell, after 0010-D3); the shell's first complete frame, from a source 0019 defines that survives 0010 (today the only signal is a console string, userspace/desktop/src/shell.rs:867-868, matched at kernel/src/userspace.rs:2510-2520, which 0010 removes from default builds).
- `mirror.perf=1`: `MIRROR_PERF_MEM ns= available_kib= pool_kib=` at enable and every 10 s; `MIRROR_PERF_INPUT seq= irq_ns= present_ns=` (keyboard IRQ to first present; may wait for 0084).
- `mirror.perf=flip`: before userspace, 100 alternating full-screen fills 500 ms apart, each followed by `MIRROR_PERF_FLIP seq= ns=`. `mirror.perf=agent`: starts the bench client (after 0045), which prints numeric `MIRROR_PERF_AGENT seq= ns=` only.

### Files and tools
- `tools/perf-probe/`: its own `[workspace]`, leaving the root members list (Cargo.toml:2) alone. `.cargo/config.toml:1-2` defaults repo cargo runs to x86_64-unknown-none, so it uses `--target x86_64-pc-windows-msvc` like the host tests (scripts/test.ps1:15). Subcommands: `boot`, `input`, `pixel-agent`, `calibrate` (VNC update cadence; Mirror serial-to-frame delay under `mirror.perf=flip`), `snapshot` (reference frame and mask), `serial` (stamp each line from a VMware named-pipe serial port with QPC), `summarize` (CSV to baselines.md rows).
- `scripts/perf.ps1 -Os mirror|ubuntu|windows -Gate boot|idle|input|agent|all [-Hypervisor vmware|qemu] [-Vmx <path>]` (`-Vmx` required for VMware). Guest credentials and the VM encryption password come from the untracked `build/perf/guests.json`. Before it builds perf-probe, it runs 0011's `Assert-MirrorToolchain` once 0011 has landed.
- `tools/perf-guest/windows/` (typeperf sampler, UIA agent) and `tools/perf-guest/linux/` (meminfo sampler, AT-SPI agent), copied in with `vmrun CopyFileFromHostToGuest`.
- `docs/perf/vm-setup.md`: per-VM manual recipe. First step: install VMware Workstation Pro and record its build. Then: installer name and SHA-256; the VMX hardware block (D3), including the VM encryption VMware requires for a vTPM (TPM-files-only if offered; `vmrun` then takes `-vp`); D4 settings and the Windows activation state (Open question 3); the VNC server (`RemoteDisplay.vnc.*`, fixed port, no password, blocked for non-loopback traffic by a host firewall rule); Mirror's VM on 0015a's `mirror.vmdk` with its serial port as a named pipe; the `perf-base` snapshot.
- `docs/perf/baselines.md`: one table per gate; rows give OS and build, date, method version, N, median, p95, min, max. Header: host CPU, RAM, OS build, host hypervisor state, power mode, VMware build, VMX hash.
- `docs/perf/runs/<date>-<os>-<gate>.csv`: raw samples. Frames go to `build/perf/` (ignored by `/build/`, .gitignore:2).
- QEMU lane (non-gating): perf-probe against QEMU `-vnc 127.0.0.1:<n>`, ending with QMP `quit` as capture.ps1 does (scripts/capture.ps1:524-526). Devices come from 0014's `Get-MirrorDeviceArguments -Profile V1` once 0014 has landed, and the boot medium from 0015a's disk image once 0015a has landed, so perf.ps1 keeps no QEMU device list of its own. QEMU runs TCG (scripts/run.ps1:319), so the lane checks the tooling, not timings.

### Known measurement limits
- Mirror draws to the UEFI framebuffer, so VMware must detect its changes; the baselines' SVGA drivers send explicit updates. The user sees this, so it counts; flip calibration measures Mirror's share.
- Changes are seen at the VNC update cadence; random input phase and N ≥ 200 keep medians stable; the cadence is reported.
- This host runs Hyper-V, so VMware runs on the Windows Hypervisor Platform. Host state shifts absolute numbers, so one comparison uses one recorded host state, on AC power, with no other VM running.
- Overhead outside the timed calls (PowerShell, Python, VNC) is excluded; Python's per-call cost beside D-Bus round trips is a small known bias.

## Acceptance tests
1. Host tests in the host-test step of scripts/test.ps1: `cargo test --locked --manifest-path tools/perf-probe/Cargo.toml --target x86_64-pc-windows-msvc`, covering the RFB 3.8 handshake (security None) and Raw update parsing from recorded byte fixtures, the masked-diff threshold, template matching, median and p95 on known vectors, and serial line stamping.
2. QEMU lane: `scripts/perf.ps1 -Os mirror -Gate boot -Hypervisor qemu` boots the current normal build 3 times and reports 3 boot times, each end frame saved under `build/perf/`, with no timeouts.
3. Probe sanity on VMware, per baseline OS: on an idle desktop the input probe sees no change in 60 s; `perf-probe calibrate` reports the VNC update cadence into baselines.md.
4. Repeatability on VMware, per baseline OS: boot coefficient of variation ≤ 10% over 10 boots; idle-memory max − min ≤ 5% of the median over 5 boots; each session median within ±10% of the pooled median for input and agent action; a rerun on another day reproduces every median within 10%.
5. docs/perf/baselines.md has Ubuntu and Windows rows for all four gates (agent action on both the accessibility and pixel paths), the environment header, and raw CSVs under docs/perf/runs/. Each host-observed boot time is at least the guest-reported diagnostic.
6. Mirror rows: once 0010, 0013, 0015a and 0019 have landed, `scripts/perf.ps1 -Os mirror -Gate all` yields boot, idle and input rows (with flip calibration), and after 0045 the agent row, with no manual step beyond starting it.

## Dependencies
- Crates: none (perf-probe is std only).
- Host tools, not linked: VMware Workstation Pro with `vmrun` (proprietary, Broadcom, no-cost license); QEMU 11.1.0 in `.tools/qemu` (GPL-2.0), already in use. Neither VMware Workstation Pro nor VirtualBox is installed on the host today (no install directory; `vmrun` and `VBoxManage` not on PATH), so the baseline track is blocked until the owner installs them. This spec's gate numbers need only VMware Workstation Pro; VirtualBox serves the VirtualBox follow-up (Open question 8).
- Guest images: Ubuntu Desktop 26.04 LTS (open source); Windows 11 Pro (proprietary; activation per Open question 3).
- In the guests: PowerShell 5.1, .NET Framework 4.8.x UIAutomationClient, typeperf (inbox); python3 (PSF-2.0) and python3-pyatspi (LGPL), expected on a default Ubuntu desktop for Orca; vm-setup verifies it or installs it and records that.

## Open questions
External facts, checked while building the VMs:
1. Does the installed VMware build still offer the per-VM VNC server (`RemoteDisplay.vnc.*`), with correct frames under 3D acceleration? If not, D5, D7 and D8's pixel path need another frame source (D7's alternative, DXGI Desktop Duplication), which goes back to the owner.
2. Do Windows 11 Notepad (`ValuePattern.SetValue`) and GNOME Text Editor (GTK4 AT-SPI `EditableText`) accept accessibility writes? If not, use a minimal app on the stock text widget (WinForms TextBox, GTK3 TextView) and record it.
3. Windows activation: the owner activates the VM with a license or leaves it unactivated when building it. The method works either way (D5 masks the watermark); vm-setup.md and the baselines.md header record the state, and one comparison keeps one state.
4. Does Windows 11 have an inbox driver for VMware's `e1000` (82545EM)? If not, the owner picks Intel's driver or VMware's Windows default `e1000e`, which breaks identical hardware (D3).

Follow-up suggestions, outside this spec:

5. QEMU lane on WHPX for meaningful automated timings (this spec keeps TCG).
6. A one-time 240 fps camera measurement to bound D7's offset from true photon latency.
7. Before 0045, an early Mirror agent-action proxy through the agent demo's typed Read and Replace (crates/desktop-protocol/src/lib.rs:132-145).
8. VirtualBox baselines with the same method.
