# Mirror specs

Small, self-contained specs. Each one covers one bounded piece of work that a single agent can pick up cold.

- [0000-north-star.md](0000-north-star.md): what Mirror is for and what v1 means.
- [decisions.md](decisions.md): the decision ledger. Every spec decision traces back to an entry here.

## Spec map (v1)

M0's specs are drafted in this folder (see each file's Status). M1-M9 specs are planned, not yet drafted, except side spec 0029, which is drafted. Each one is sized for one agent and cites the ledger IDs it implements.

**M0: Hygiene, VM boot images, absolute pointing, baselines**
- 0010 Boot hygiene: test battery and fixture asserts behind `boot-smoke`, user-content logging and test oracles behind `test-trace`, opt-in perf exception (S7, S7a, S7b)
- 0011 Toolchain pin, workspace lints, and test-log clearing (P5a)
- 0012 AGENTS.md, CLAUDE.md, and the readme references wording (P5b, P6, P7, P7a, P8, P9)
- 0013 Firmware and PCI tolerance for VMware: per-slot MADT capacity, the 32-function PCI cap, IOAPIC ID tolerance (0013-D9) and per-entry root-table handling; duplicate-NMI and HPET-descriptor leniency (0013-D3, D5) as general hardening
- 0014 Common device-set QEMU profiles: AHCI, e1000 (82545EM), PS/2, framebuffer; LegacyVirtio profile until 0062 (S1a, S1b)
- 0015a Disk image: GPT, FAT32 ESP, minimal ext2 root, VMDK, QEMU boot switch, VM settings doc (S5, S2, S1b)
- 0015b Live ISO and its test case (S5, S5a)
- 0016a Hardening: guarded boot-CPU stacks, NMI/#MC stacks
- 0016b Hardening: SMEP/SMAP/UMIP, EFER.SCE, CPU vulnerability reporting (S9)
- 0017a PS/2: separate keyboard and mouse loss handling, mouse coalescing and resync
- 0017b PS/2: time-based init timeouts and init order
- 0017c Absolute pointing on VMware: vmmouse protocol (S1a, S1d)
- 0018 Performance baseline harness: the four gates, measured on Linux and Windows VMs in the host's ULM monitor mode (G7, G7a, O3)
- 0019 Mirror perf instrumentation and telemetry: kernel side of 0018-D9 and device/link telemetry (S7b, C3)

M0 execution order (confirmed): commit `docs/specs`, then 0012 first. Then 0011, 0014, 0010, 0015a, 0015b, 0013, 0016a, 0016b, 0017a and 0017b, then 0017c, then 0019, one at a time, each updating the AGENTS.md lines it changes in the same commit (P9). 0018's baseline track runs in parallel; its Mirror rows follow 0015a, 0013 (without 0013-D9 a VMware boot falls back to one CPU on the legacy PIC), 0017c and 0019.

**M1: ABI reset, kernel objects, compositor protocol, userspace target**
- 0020 `mirror-sys`: one syscall and error table (S8); UTF-8 strings (S11); reserves realtime-clock and random-bytes calls (S14, S15)
- 0021 SYSCALL entry and personality selector (S6a); records its boot-CPU-only kernel-stack assumption, with the per-CPU entry rework in 0050
- 0022 Kernel objects, rights and handles (S6b); revocable grant objects (S6e)
- 0023 Channels v2: creation and handle transfer (S6b)
- 0024 Waiting on many objects (S6b)
- 0025 Shared memory objects (S6c)
- 0026 `mirror-rt`: entry, allocator, handles, threads, panic output (S8)
- 0027a Compositor protocol: shared-memory surfaces, any display size and live resize, clipboard object, versioned input envelope with seat and provenance (S6d, S6f, S1c, S11, A20; coordinate with Kimi)
- 0028 Userspace target: hard-float, SSE2 baseline, AVX2 by runtime dispatch, build-std on the pinned compiler (S12, S8)
- 0029 UI tokens on the current renderer: Kimi's design tokens on today's shell, apps and boot screen (P2a, G12; side spec, before 0027a)

**M2: Processes, services, system log, crypto and entropy**
- 0027b App launch out of the kernel, completing S6d (S6d, S6f; after 0030 and 0031)
- 0030 Process model: table, spawn from image, wait, kill, exit status; suspend and resume for pause (S6e)
- 0031 Boot bundle as the source of program images
- 0032 Service manager with restart policy
- 0033 Service directory: per-process namespaces at spawn, restart and reconnect rules (S13)
- 0034 System log: kernel ring buffer, per-service channels, crash records (S17)
- 0035 Shared crypto crate: RustCrypto and rustls, SHA-256 for the audit chain (S14; before 0043)
- 0036 Entropy and kernel CSPRNG: RDSEED/RDRAND, jitter, persisted seed; QEMU test CPUs add `+rdrand,+rdseed` (S14; before 0040)
- 0039 Screen/input fallback design note, design-only (A1a; before 0044 and 0047)

**M3: Agent harness core**
- 0040 Principals: owner/root and agents (A11); owner login and auto-login (A11a)
- 0041 Capability presets and grants: read-only/full, grant sources, delegation (A4-A7); development rule full access = root (A5a), ceiling per O7; clipboard access as a grantable, audited capability (A20)
- 0042 Immutable core and experiment build profile (A9, A10)
- 0043 Audit log v1: in memory, hash chain, viewer, TTL (A8, A8a, S14)
- 0044 Typed operation registry (A1, A17); element trees, streaming and cancel (A19)
- 0045 Agent API, native protocol (A12, A13); I5 validation for every client (A19, I5)
- 0046 Human override: pause, stop, revoke (A9); pause = kernel suspend (S6e)
- 0047 Dual input seats, provenance, agent cursor (A3, A3a); per-seat clipboards (A20)
- 0048 Agent tasks: task objects, task-scoped grants, persistence and paused resume (A18)
- 0049 Trusted approval surface (A9a, A4; with Kimi)

**M4: SMP, compute, power and time**
- 0050 Per-CPU data and per-thread kernel stacks; per-CPU SYSCALL entry rework (swapgs, KERNEL_GS_BASE) replacing 0021's boot-CPU-only kernel stack
- 0051 Userspace on all CPUs: locking, user TLB shootdowns; NMI stop of all CPUs on panic (S17)
- 0052 Scheduler v2: work-conserving, priority classes, accounting (S4)
- 0053 XSAVE/AVX2 user state (I6)
- 0054 Large memory: frame allocator v2, per-frame metadata, dynamic limits (I6); memory reserve, pressure notifications, OOM order (S18)
- 0055 Interrupt robustness: catch-all vector handlers, boot-CPU-only external NMI, PCAT_COMPAT=0, HPET-based timer calibration, x2APIC handoff, non-HPET time source (S10)
- 0056 Wall clock: RTC as UTC (Mirror's VMs set `rtc.diffFromUTC = "0"`), realtime syscall, time zones, who sets the clock (S15)
- 0057 ACPI namespace: AML interpreter, `_S5`, `_PRT`, power button (S16)
- 0058 Shutdown and reboot: service stop order, filesystem sync, clean-pool marker, reset methods (S16)

**M5: Storage, settings, VM updates, SVGA II display**
- 0060 Runtime driver model: PCI device model, MMIO mapping, MSI, driver lifetime; INTx routing through `_PRT` (S16)
- 0061 Block layer; read-throughput floor for expert streaming (I12)
- 0062 AHCI read/write at runtime; read-throughput floor for expert streaming (I12)
- 0063 Mirrored pool (G8, G8a, G8b, G8d)
- 0064 ext2 read/write; boot-time checker after an unclean shutdown (S2, S2b); after 0067
- 0065 ext3 journaling (S2, S2b): daily-driving persistent files waits for it
- 0066 Filesystem capabilities and VFS; persisted audit log (A8); file-authority model, system log and C3 telemetry persistence (S17, C3)
- 0067 Daily VM layout and host-side updates: data on the pool, update tool, previous-kernel fallback, format versions, build identity, daily-drive profile with `rtc.diffFromUTC = "0"` (G9, G11, S15; before 0064)
- 0068 Persisted settings store (A21)
- 0069 VMware SVGA II display driver: mode setting, and live resize driven by guest RPC (TCLO) resize requests (S1a, S1c; after 0060). Placement is decided when 0069 comes up (decisions.md Pending): a GuestRpc/TCLO core split out of 0087 and placed before 0069, or mode setting only, with live resize moving to 0087

**M6: Network, remote agent access, MCP adapter**
- 0070 e1000 driver
- 0071 TCP/IP stack (native or crate, per S3); DHCPv4, static override, IPv6 SLAAC (S19)
- 0072 DNS and TLS; Mozilla root store, certificate time checks, SNTP, private endpoints (S14, S15, I2)
- 0073 MCP adapter: Mirror as an MCP server (A13, A13a; after 0074 and 0076)
- 0074 HTTP/1.1 client and server, SSE, JSON (S19; before 0073)
- 0075 Network capabilities and egress scope (S20, O7)
- 0076 Remote agent access: host-facing listener, pairing, tokens (A13a); decides the adapter, host-only (VMnet1) or a NAT forward plus a host firewall rule

**M7: Shell, tools, text, apps, guest services, std port, panel**
- 0080 Typed shell; session contract (A17, A17a)
- 0081 Native core tools: the G10 tool list, fetch/put (G10, S20, A17)
- 0082 Terminal app (after 0084 and 0086)
- 0083 Assistant panel; hosts the trusted approval prompt from M7 (A14, A9a; with Kimi; after 0084 and 0086)
- 0084 Input v2: device-independent events, keycodes, shortcuts, keyboard layouts and dead keys, LED and lock-state sync (S11; replaces the 0027a envelope payload; before 0082 and 0083)
- 0085 Text editor with typed document operations, the gate 3/4 target (G10)
- 0086 Text stack: OpenType fonts, rasterizer, fallback, shaping (S11; with Kimi; before 0082, 0083, 0085)
- 0087 VMware guest services: RPC channel, host clipboard sharing, shared folders (A20, S20); its RPC core may move ahead of 0069 (S1c, see 0069)
- 0088 Desktop apps: file manager, settings, system and task monitor (G10; with Kimi)
- 0089 Rust std port for Mirror's userspace target (S8, S12; staged as services land; before 0081)

**M8: AI**
- 0090 Keyring and credential broker (A16, A16a); at-rest protection by owner password (A16b); completion budgets (I14)
- 0091 Provider adapters: OpenAI, Anthropic, OpenAI-compatible; rate limits and retries (I2, I14)
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
- 0104 AI evaluation benchmark (I15, I8a; before 0093)

**M9: v1 acceptance**
- 0199 Performance gates and daily-drive checklist; local-inference floors and input latency under inference (G7, G7a, I15)

## After v1 (planned, not in v1)

Decided scope that lands after v1 acceptance. Only 0092 keeps a spec number.

- 0092 ChatGPT sign-in (I2, O4)
- In-OS self-update (G9)
- Cloud retention copies of the audit log (A8a)
- Local vision (I4d)
- USB HID pointing (S1d)
- Screen/input fallback implementation (A1a)

## Unscheduled follow-ups (deferred, not yet in a milestone)

Deferred work with no milestone yet, kept here so it is not lost. This list replaces the deferred list that was in the decisions.md Pending section.

- KPTI/MDS mitigation spec (S9): a prerequisite before any third-party native code runs
- NX direct map and Limine lower-half audit
- Non-fatal NMI
- QEMU/OVMF version checks in the test scripts
- Per-case work directories for a parallel test suite
- A typed test-event channel
- The live ISO root filesystem
- `clippy --tests`

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
