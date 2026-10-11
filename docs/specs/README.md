# Mirror specs

Small, self-contained specs. Each one covers one bounded piece of work that a single agent can pick up cold.

- [0000-north-star.md](0000-north-star.md): what Mirror is for and what v1 means.
- [decisions.md](decisions.md): the decision ledger. Every spec decision traces back to an entry here.

## Spec map (v1)

The former M0's specs are drafted in this folder (see each file's Status), as is side spec 0029: most sit in Phase 0, 0015b now sits in Phase 11, and 0019 is proposed to split across Phases 1 and 6. Phase 0's NEW vmrun lane and 0058a, and the specs of later phases, are planned, not yet drafted. Each one is sized for one agent and cites the ledger IDs it implements.

The phases come from the roadmap re-plan (`roadmap-replan.md`), adopted on 2026-10-11 (decisions.md, Phases). Items marked NEW or split are the re-plan's proposals: each is confirmed, and numbered or lettered, when its phase comes up (RD24). The re-plan's other RS and RD decisions are taken when its Due column says.

**Done**
- 0012 AGENTS.md, CLAUDE.md, and the readme references wording (P5b, P6, P7, P7a, P8, P9)
- 0011 Toolchain pin, workspace lints, and test-log clearing (P5a)
- 0014 Common device-set QEMU profiles: AHCI, e1000 (82545EM), PS/2, framebuffer; LegacyVirtio profile until 0062 (S1a, S1b)
- 0010 Boot hygiene: test battery and fixture asserts behind `boot-smoke`, user-content logging and test oracles behind `test-trace`, opt-in perf exception (S7, S7a, S7b)
- 0029 UI tokens on the current renderer: Kimi's design tokens on today's shell, apps and boot screen (P2a, G12, A3b; side spec, before 0027a). Complete; 0029-D9's geometry is withdrawn (RS3)

**Phase 0: Mirror boots well in VMware**
- 0015a Disk image: GPT, FAT32 ESP, minimal ext2 root, VMDK, QEMU boot switch, VM settings doc (S5, S2, S1b). Done (2026-10-11)
- NEW: vmrun lane: headless VMware boot, restart and suspend/resume checks (right after 0015a if confirmed by then, otherwise after 0013)
- 0013 Firmware and PCI tolerance for VMware: per-slot MADT capacity, the 32-function PCI cap, IOAPIC ID tolerance (0013-D9) and per-entry root-table handling; duplicate-NMI and HPET-descriptor leniency (0013-D3, D5) as general hardening. Done (2026-10-11)
- Docs-only: the G11 daily VM (12 vCPUs, 20 GB, NAT) in `docs/vm-settings.md` (RD16)
- 0016a Hardening: guarded boot-CPU stacks, NMI/#MC stacks. Done (2026-10-11)
- 0016b Hardening: SMEP/SMAP/UMIP, EFER.SCE, CPU vulnerability reporting (S9)
- 0017a PS/2: separate keyboard and mouse loss handling, mouse coalescing and resync
- 0017b PS/2: time-based init timeouts and init order
- 0017c Absolute pointing on VMware: vmmouse protocol (S1a, S1d)
- 0058a (split): reboot without AML (FADT reset register, then 8042, then 0xCF9, then triple fault) and a console `restart` command
- 0018 Performance baseline harness: the four gates, measured on Linux and Windows VMs in the host's ULM monitor mode (G7, G7a, O3). Its Linux and Windows baseline track runs in parallel; its Mirror rows follow Phase 4
- 0013 showed VMware handing off in xAPIC mode (2026-10-11), so 0055a stays in Phase 1 (RD11). If 0017c check 7 needs a Tools-running state, 0087a moves here (RD10).

**Phase 1: Kernel core, built once**
- 0019a (split): perf markers: boot milestones, idle memory, the input-to-present trace and a `burn` perf item (0019 split, Pending item 1 and RD8)
- NEW: 0030a: a global process and thread table, run queues, wait queues instead of polling, wakeup preemption, more than 4 threads per process
- 0050 Per-CPU data and per-thread kernel stacks; per-CPU SYSCALL entry rework (swapgs, KERNEL_GS_BASE); it now lands before 0021, so this clause is reworded under RD21 (proposed, before Phase 2). Adds a kernel VA allocator for mappings made after the paging seal
- NEW: kernel heap v2: grows on demand, slab allocator, kernel memory charged to the owning process
- 0054a (split): frame allocator v2, per-frame refcounts, dynamic per-process limits, and the S18 reserve and OOM order
- 0051 Userspace on all CPUs: locking, user TLB shootdowns; NMI stop of all CPUs on panic (S17). Adds the S17 crash screen
- 0052 Scheduler v2: work-conserving, priority classes, accounting (S4)
- 0053 XSAVE/AVX2 user state (I6)
- 0055a (split): a TSC time source calibrated against the HPET, catch-all vector handlers, and the x2APIC handoff if it did not move to Phase 0

**Phase 2: ABI, kernel objects and timers**
- 0020 `mirror-sys`: one syscall and error table (S8); UTF-8 strings (S11); reserves realtime-clock and random-bytes calls (S14, S15)
- 0056a (split): the RTC read as UTC (Mirror's VMs set `rtc.diffFromUTC = "0"`) and the realtime syscall
- 0021 SYSCALL entry and personality selector (S6a). It now lands after 0050 (Phase 1), so its old plan, a boot-CPU-only kernel stack that 0050 reworks, no longer fits; RD21 proposes writing it per-CPU from the start and dropping 0050's rework clause, decided before Phase 2
- 0022 Kernel objects, rights and handles (S6b); revocable grant objects (S6e)
- 0023 Channels v2: creation and handle transfer (S6b)
- 0024 Waiting on many objects (S6b)
- 0025 Shared memory objects (S6c)
- NEW: high-resolution timers: one-shot or TSC-deadline LAPIC, tickless idle, sub-millisecond deadlines, timer objects on 0024

**Phase 3: Userspace platform**
- 0026 `mirror-rt`: entry, allocator, handles, threads, panic output (S8)
- 0028 Userspace target: hard-float, SSE2 baseline, AVX2 by runtime dispatch, build-std on the pinned compiler (S12, S8)
- NEW: ELF loader v2: images over 2 MiB, PT_TLS, a configurable main stack, shared read-only text, room for PIE
- 0030 Process model: spawn from image, wait, kill, exit status; suspend and resume for pause (S6e). The process table is proposed for NEW 0030a in Phase 1; if 0030a is not confirmed, 0030 keeps it
- 0031 Boot bundle as the source of program images
- 0032 Service manager with restart policy. Includes the session model: the desktop restarts, and Esc stops being one-way
- 0033 Service directory: per-process namespaces at spawn, restart and reconnect rules (S13)
- 0034 System log: kernel ring buffer, per-service channels, crash records (S17). The log lands in memory here; its crash-record path follows 0062 in Phase 6

**Phase 4: A modern desktop at the VM window's size**
- NEW: design note: driver placement and the display object (RD12)
- 0027a Compositor protocol: shared-memory surfaces, any display size and live resize, clipboard object, versioned input envelope with seat and provenance (S6d, S6f, S1c, S11, A20; coordinate with Kimi). Protocol only, with output and buffer scale, a drag-and-drop data offer, cursor-shape requests and a human-seat capture API from day one; lands with a scanout object and an input object, retiring syscalls 13-16 and the 1280x800 cap
- 0084a (split): keycodes and the full PS/2 key set, the IntelliMouse and vmmouse wheel, LEDs and shortcut routing; defines the 0027a envelope payload
- 0027b App launch out of the kernel, completing S6d (S6d, S6f; after 0030 and 0031). Moves here, right after 0027a and 0084a, and retires syscalls 17-22
- NEW: 0027c renderer: premultiplied RGBA, antialiased shapes, soft shadows, gradients, clipping, a shared path rasterizer, compositor frame-time hooks
- NEW: 0027d frame clock: easing, reduced motion, event-driven frames
- 0086a (split): OpenType outlines on the shared rasterizer, a Latin UI font and a mono font, the type roles, and the icon set
- 0039 Screen/input fallback design note, design-only (A1a; before 0044 and 0047)
- 0044a (split): the A19 element-tree contract
- NEW: toolkit core: layout, button, text field, scroll view, focus model with focus-visible rings, theming from `mirror-ui-tokens`, element trees
- NEW: 0027e window manager: any number of windows, resize, maximize, snap, z-layers, cursor shapes, the titlebar geometry, a minimal taskbar, an app-crash notice
- 0060a (split): device objects, BAR sizing, runtime MMIO with write-combining, polled lifetime
- 0087a (split): the VMware guest RPC (TCLO) core: reset and ping, `tools.set.version`, capability registration
- 0069 VMware SVGA II display driver: mode setting, and live resize driven by guest RPC (TCLO) resize requests (S1a, S1c; after 0060). Placement is decided when 0069 comes up (decisions.md Pending): a GuestRpc/TCLO core split out of 0087 and placed before 0069, or mode setting only, with live resize moving to 0087. Adds a FIFO-aware kernel console and crash screen, a QEMU `-vga vmware` profile, and clamps multi-display topology requests to display 0
- 0087b (split): host text clipboard
- NEW: frame-time and idle budget: p95 frame times at 1920x1080, memory per window, host CPU while idle; then 0018's Mirror rows

**Phase 5: Power**
- 0057 ACPI namespace: AML interpreter, `_S5`, `_PRT`, power button (S16)
- 0058 Shutdown and reboot: service stop order, filesystem sync, clean-pool marker, reset methods (S16). Filesystem sync and the clean-pool marker are hooks here; 0063 and 0064 own their tests
- 0087c (split): VMware OS_Halt, OS_Reboot and OS_Resume (clock resync, timer rearm, DHCP renewal), heartbeat, guest info

**Phase 6: Files that survive reboot**
- NEW: minimal tool runner: a text window that runs boot-bundle tools (pool, write, cat, tz) until 0080-0082 replace it
- 0060b (split): MSI and MSI-X, INTx through `_PRT`, asynchronous completion, and a QEMU `-machine pc` profile
- 0061 Block layer; read-throughput floor for expert streaming (I12)
- 0062 AHCI read/write at runtime; read-throughput floor for expert streaming (I12)
- NEW: AHCI QEMU boot image: QEMU boots from AHCI, so Mirror reads its own disk in tests (supersedes 0015-D10, RD13)
- 0019b (split): C3 telemetry
- 0067 Daily VM layout and host-side updates: data on the pool, update tool, previous-kernel fallback, format versions, build identity, daily-drive profile with `rtc.diffFromUTC = "0"` (G9, G11, S15; before 0064). Before 0063
- 0066a (split): the VFS and file-capability interface, before 0064
- 0063 Mirrored pool (G8, G8a, G8b, G8d)
- 0064 ext2 read/write; boot-time checker after an unclean shutdown (S2, S2b); after 0067
- 0065 ext3 journaling (S2, S2b): daily-driving persistent files waits for it
- 0068 Persisted settings store (A21). Confirmed A21 requires human-seat typed operations (0044b, Phase 9) and an audit of every change (0043, Phase 10); how 0068 meets it this early is RD25, decided before Phase 6
- 0066b (split): persisted system log, audit and telemetry; file change notification; the Home folder layout; filename search
- 0056b (split): time zones and who sets the clock

**Phase 7: Network**
- 0035 Shared crypto crate: RustCrypto and rustls, SHA-256 for the audit chain (S14; before 0043)
- 0036 Entropy and kernel CSPRNG: RDSEED/RDRAND, jitter, persisted seed; QEMU test CPUs add `+rdrand,+rdseed` (S14; before 0040)
- 0070 e1000 driver
- 0071 TCP/IP stack (native or crate, per S3); DHCPv4, static override, IPv6 SLAAC (S19). Plus NEW socket objects: listen and accept, loopback
- 0072 DNS and TLS; Mozilla root store, certificate time checks, SNTP, private endpoints (S14, S15, I2)
- 0074 HTTP/1.1 client and server, SSE, JSON (S19; before 0073)
- 0075a (split): network capability objects and egress scope

**Phase 8: Kimi's desktop shell**
- NEW: toolkit widgets: menus, popovers, tooltips, tabs, switches, sliders, lists, progress, drag sources and drop targets
- NEW: scaling: 100, 125, 150 and 200%
- NEW: notification service and center
- NEW: app registry and file associations
- NEW: open/save chooser (powerbox)
- NEW: chrome: top bar with clock, dock, start menu with search, palette, quick settings, switcher, workspaces
- NEW: static boot splash with phase-based progress
- 0040 Principals: owner/root and agents (A11); owner login and auto-login (A11a). Plus screen lock
- NEW: first-run setup: owner and password, time zone, keyboard layout, pool creation

**Phase 9: Apps and tools**
- 0089 Rust std port for Mirror's userspace target (S8, S12; staged as services land; before 0081). Its core stage, with user exception delivery, opens this phase, then its filesystem and network stages
- 0044b (split): the typed operation registry, streaming and cancel
- 0080 Typed shell; session contract (A17, A17a)
- 0081 Native core tools: the G10 tool list, fetch/put (G10, S20, A17)
- 0084b (split): keyboard layouts and dead keys
- 0086b (split): font fallback, CJK, emoji and shaping
- 0082 Terminal app (after 0084 and 0086)
- 0085 Text editor with typed document operations, the gate 3/4 target (G10)
- 0088 Desktop apps: file manager, settings, system and task monitor (G10; with Kimi)
- NEW: HD Audio output, mixer and volume (S22)
- NEW: screenshots: PNG codec, a capture tool on 0027a's capture API, image preview
- 0087 VMware guest services: RPC channel, host clipboard sharing, shared folders (A20, S20); its RPC core may move ahead of 0069 (S1c, see 0069). The HGFS shared-folder remainder

**Phase 10: Harness product layer**
- 0041 Capability presets and grants: read-only/full, grant sources, delegation (A4-A7); development rule full access = root (A5a), ceiling per O7; clipboard access as a grantable, audited capability (A20)
- 0042 Immutable core and experiment build profile (A9, A10)
- 0043 Audit log v1: hash chain, viewer, TTL (A8, A8a, S14). Persisted from the start, on 0066a's VFS
- 0045 Agent API, native protocol (A12, A13); I5 validation for every client (A19, I5)
- 0046 Human override: pause, stop, revoke (A9); pause = kernel suspend (S6e)
- 0047 Dual input seats, provenance, agent cursor (A3, A3a, A3b); per-seat clipboards (A20)
- 0048 Agent tasks: task objects, task-scoped grants, persistence and paused resume (A18). Its conversation persistence lands with 0098
- 0075b (split): agent egress grants, per-preset scope and audit
- 0049 Trusted approval surface (A9a, A4; with Kimi)
- 0083 Assistant panel; hosts the trusted approval prompt (A14, A9a; with Kimi; after 0084 and 0086)
- 0076 Remote agent access: host-facing listener, pairing, tokens (A13a); decides the adapter, host-only (VMnet1) or a NAT forward plus a host firewall rule
- 0073 MCP adapter: Mirror as an MCP server (A13, A13a; after 0074 and 0076)

**Phase 11: AI and v1 acceptance**
- 0054b (split): memory pressure notifications, before 0093
- 0055b (split): the rest of 0055
- 0090 Keyring and credential broker (A16, A16a); at-rest protection by owner password (A16b); completion budgets (I14)
- 0091 Provider adapters: OpenAI, Anthropic, OpenAI-compatible; rate limits and retries (I2, I14)
- 0104 AI evaluation benchmark (I15, I8a; before 0093)
- 0093 Inference engine core (I3, I4); text-only (I4d); opens with the reuse measurement spike (I4c); owns the expert cache (I12); after 0104
- 0094 Engine monitoring hooks (I5; tool-call validation lives in 0045 per A19)
- 0095 Built-in assistant (A12); provider routing and offline fallback (I14); after 0098
- 0096 Sharded model loading (I7)
- 0097 TurboQuant library: shared lossy vector quantization for KV caches and vector indexes, with stages chosen by measured quality (I8, I8a, I11, I11b)
- 0098 CLM context objects: typed edit operations, origin-tagged segments, policy, audit (I10); persistence across reboot (A18), origin propagation (I13)
- 0099 Model store: Mirror-native expert files, host-side conversion, hash manifest (I12, I4a, I7)
- 0100 Suffix Cache Reuse in the engine KV cache (I10a)
- 0101 Embedding model support (I11a, I11c)
- 0102 OS semantic index (I11, I11a); grant-filtered results, local-only content (I13)
- 0103 Agent memory (I11, I11a); grant-filtered results, local-only content (I13)
- 0015b Live ISO and its test case (S5, S5a). Moved here on 2026-10-11 (RD1); re-drafted and re-confirmed before this phase
- 0199 Performance gates and daily-drive checklist; local-inference floors and input latency under inference (G7, G7a, I15)

Execution order (confirmed 2026-10-11): Phase 0 runs 0015a (done), then 0013 (done), then 0016a (done), 0016b, 0017a, 0017b and 0017c, one at a time, each updating the AGENTS.md lines it changes in the same commit (P9). The vmrun lane, the G11 docs commit and 0058a join Phase 0 once confirmed (RD1, RD16). 0018's baseline track runs in parallel. Later phases run in the order listed, except where a line's own note orders it after a later line (0095 runs after 0098). Done earlier: 0012, 0011, 0014, 0010 and side spec 0029.

**Specs the re-plan splits**, with their original scope (split specs keep their decision IDs, rule 1):
- 0019 Mirror perf instrumentation and telemetry: kernel side of 0018-D9 and device/link telemetry (S7b, C3)
- 0044 Typed operation registry (A1, A17); element trees, streaming and cancel (A19)
- 0054 Large memory: frame allocator v2, per-frame metadata, dynamic limits (I6); memory reserve, pressure notifications, OOM order (S18)
- 0055 Interrupt robustness: catch-all vector handlers, boot-CPU-only external NMI, PCAT_COMPAT=0, HPET-based timer calibration, x2APIC handoff, non-HPET time source (S10)
- 0056 Wall clock: RTC as UTC (Mirror's VMs set `rtc.diffFromUTC = "0"`), realtime syscall, time zones, who sets the clock (S15)
- 0058 Shutdown and reboot: service stop order, filesystem sync, clean-pool marker, reset methods (S16)
- 0060 Runtime driver model: PCI device model, MMIO mapping, MSI, driver lifetime; INTx routing through `_PRT` (S16)
- 0066 Filesystem capabilities and VFS; persisted audit log (A8); file-authority model, system log and C3 telemetry persistence (S17, C3)
- 0075 Network capabilities and egress scope (S20, O7)
- 0084 Input v2: device-independent events, keycodes, shortcuts, keyboard layouts and dead keys, LED and lock-state sync (S11; replaces the 0027a envelope payload; before 0082 and 0083)
- 0086 Text stack: OpenType fonts, rasterizer, fallback, shaping (S11; with Kimi; before 0082, 0083, 0085)
- 0087 VMware guest services: RPC channel, host clipboard sharing, shared folders (A20, S20); its RPC core may move ahead of 0069 (S1c, see 0069)

## After v1 (planned, not in v1)

Decided scope that lands after v1 acceptance. Only 0092 keeps a spec number.

- 0092 ChatGPT sign-in (I2, O4)
- In-OS self-update (G9)
- Cloud retention copies of the audit log (A8a)
- Local vision (I4d)
- USB HID pointing (S1d)
- Screen/input fallback implementation (A1a)

## Unscheduled follow-ups (deferred, not yet in a phase)

Deferred work with no phase yet, kept here so it is not lost. This list replaces the deferred list that was in the decisions.md Pending section.

- KPTI/MDS mitigation spec (S9): a prerequisite before any third-party native code runs
- NX direct map and Limine lower-half audit
- Non-fatal NMI
- QEMU/OVMF version checks in the test scripts
- Per-case work directories for a parallel test suite
- A typed test-event channel
- The live ISO root filesystem
- `clippy --tests`
- The hygiene capture's launcher-click retry (`capture.ps1 -Hygiene`): when Notes' start marker arrives after the 3 s retry window, the retry launches a second Notes and the probe fails. Seen intermittently on 2026-10-11 with kernels from before and after 0016a (5 of 6 runs once, 0 of 3 another time), so it is a harness timing issue, not a kernel one.

Removed from the old deferred list on 2026-10-09: VirtualBox baselines (VirtualBox dropped), a default-build destination for user-program diagnostics (now 0034), and the disk update path and build identity (now 0067).

## Rules

1. One spec per file: `NNNN-short-name.md`, about 1-2 pages. If a spec grows past that, split it into `NNNNa-...` and `NNNNb-...`. Split specs keep their original decision IDs (`NNNN-Dn`), so the ledger and history stay traceable.
2. Every decision has an ID and a status:
   - `confirmed`: Cameron explicitly approved it.
   - `proposed`: drafted, awaiting Cameron's verification.
   - `open`: not yet decided.
3. **Nothing is built from a spec that still has `proposed` or `open` decisions in its scope.**
4. Each spec states its dependencies (`Depends on`). Third-party crates are listed with their licenses; the license choice for Mirror itself is deferred.
5. When a spec changes a confirmed decision, update `decisions.md` first and get it re-confirmed.

## Template

```markdown
# NNNN: Title

Status: draft | confirmed | in progress (owner) | done
Depends on: NNNN, NNNN
Ledger: <decision IDs from decisions.md this spec implements>

## Goal
One paragraph: what exists when this is done.

## Non-goals
What this spec deliberately does not do.

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| NNNN-D1 | ... | proposed |

## Interfaces
Syscalls, protocols, types, files touched.

## Acceptance tests
Concrete checks: smoke markers, host tests, measurements.

## Dependencies
Third-party crates (name, version, license) or "none".

## Open questions
```
