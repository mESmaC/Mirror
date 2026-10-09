# Mirror specs

Small, self-contained specs. Each one covers one bounded piece of work that a single agent can pick up cold.

- [0000-north-star.md](0000-north-star.md): what Mirror is for and what v1 means.
- [decisions.md](decisions.md): the decision ledger. Every spec decision traces back to an entry here.

## Spec map (v1)

Planned specs, not yet drafted. Each one is sized for one agent and cites the ledger IDs it implements.

**M0: Hygiene, VM targets, baselines**
- 0010 Boot hygiene: test battery and fixture asserts behind `boot-smoke`, user-content logging and test oracles behind `test-trace`, opt-in perf exception (S7, S7a, S7b)
- 0011 Toolchain pin, workspace lints, and test-log clearing (P5a)
- 0012 AGENTS.md, CLAUDE.md, and the readme references wording (P5b, P6, P7, P7a, P8)
- 0013 Firmware and PCI tolerance for VMware/VirtualBox: duplicate NMI records, HPET register width, per-entry ACPI table handling, the 32-function PCI cap
- 0014 Common device-set QEMU profiles: AHCI, e1000 (82545EM), PS/2, framebuffer; LegacyVirtio profile until 0062 (S1a, S1b)
- 0015a Disk image: GPT, FAT32 ESP, minimal ext2 root, VMDK, QEMU boot switch, VM settings doc (S5, S2, S1b)
- 0015b Live ISO and its test case (S5, S5a)
- 0016a Hardening: guarded boot-CPU stacks, NMI/#MC stacks
- 0016b Hardening: SMEP/SMAP/UMIP, EFER.SCE, CPU vulnerability reporting (S9)
- 0017a PS/2: separate keyboard and mouse loss handling, mouse coalescing and resync
- 0017b PS/2: time-based init timeouts and init order
- 0018 Performance baseline harness: the four gates, measured on Linux and Windows VMs (G7, O3)
- 0019 Mirror perf instrumentation and telemetry: kernel side of 0018-D9 and device/link telemetry (S7b, C3)

M0 execution order (confirmed): commit `docs/specs`, then 0012 first. Then 0011, 0014, 0010, 0015a, 0015b, 0013, 0016a, 0016b, 0017a and 0017b, then 0019, one at a time, each updating the AGENTS.md lines it changes. 0018's baseline track runs in parallel once VMware is installed; its Mirror rows follow 0015a, 0013 and 0019.

**M1: ABI reset**
- 0020 `mirror-sys`: one syscall and error table (S8)
- 0021 SYSCALL entry and personality selector (S6a)
- 0022 Kernel objects, rights and handles (S6b)
- 0023 Channels v2: creation and handle transfer (S6b)
- 0024 Waiting on many objects (S6b)
- 0025 Shared memory objects (S6c)
- 0026 `mirror-rt`: entry, allocator, handles, threads, panic output (S8)
- 0027 Compositor protocol out of the kernel (S6d; coordinate with Kimi)

**M2: Processes and services**
- 0030 Process model: table, spawn from image, wait, kill, exit status
- 0031 Boot bundle as the source of program images
- 0032 Service manager with restart policy

**M3: Agent harness core**
- 0040 Principals: owner/root and agents (A11)
- 0041 Capability presets and grants: read-only/full, grant sources, delegation (A4-A7)
- 0042 Immutable core and experiment build profile (A9, A10)
- 0043 Audit log v1: in memory, hash chain, viewer, TTL (A8)
- 0044 Typed operation registry (A1, A17)
- 0045 Agent API, native protocol (A12, A13)
- 0046 Human override: pause, stop, revoke (A9)
- 0047 Dual input seats, provenance, agent cursor (A3, A3a)

**M4: SMP and compute**
- 0050 Per-CPU data and per-thread kernel stacks
- 0051 Userspace on all CPUs: locking, user TLB shootdowns
- 0052 Scheduler v2: work-conserving, priority classes, accounting (S4)
- 0053 XSAVE/AVX2 user state (I6)
- 0054 Large memory: frame allocator v2, per-frame metadata, dynamic limits (I6)
- 0055 Interrupt robustness: catch-all vector handlers, boot-CPU-only external NMI, PCAT_COMPAT=0, HPET-based timer calibration, x2APIC handoff, non-HPET time source (S10)

**M5: Storage**
- 0060 Runtime driver model: PCI device model, MMIO mapping, MSI, driver lifetime
- 0061 Block layer
- 0062 AHCI read/write at runtime
- 0063 Mirrored pool (G8a, G8b)
- 0064 ext2 read/write
- 0065 ext3 journaling
- 0066 Filesystem capabilities and VFS; persisted audit log (A8)

**M6: Network**
- 0070 e1000 driver
- 0071 TCP/IP stack (native or crate, per S3)
- 0072 DNS and TLS
- 0073 MCP adapter (A13)

**M7: Shell, tools, panel**
- 0080 Typed shell (A17)
- 0081 Native core tools
- 0082 Terminal app
- 0083 Assistant panel (A14; with Kimi)
- 0084 Input v2: device-independent events, keycodes, shortcuts, keyboard LED and lock-state sync

**M8: AI**
- 0090 Keyring and credential broker (A16, A16a)
- 0091 Provider adapters: OpenAI, Anthropic, OpenAI-compatible (I2)
- 0092 ChatGPT sign-in (I2, O4)
- 0093 Inference engine core (I3, I4)
- 0094 Engine safety hooks and tool-call validation (I5)
- 0095 Built-in assistant (A12)
- 0096 Sharded model loading (I7)
- 0097 TurboQuant library: shared lossy vector quantization for KV caches and vector indexes, with stages chosen by measured quality (I8, I8a, I11, I11b)
- 0098 CLM context objects: typed edit operations, origin-tagged segments, policy, audit (I10)
- 0100 Suffix Cache Reuse in the engine KV cache (I10a)
- 0101 Embedding model support (I11a, I11c)
- 0102 OS semantic index (I11, I11a)
- 0103 Agent memory (I11, I11a)

**M9: v1 acceptance**
- 0199 Performance gates and daily-drive checklist (G7)

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
