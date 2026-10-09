# Decision ledger

Status: `confirmed` = Cameron explicitly chose it. `proposed` = inferred by Claude, awaiting Cameron's verification. `open` = undecided.
Source interview: 2026-10-09.

## Goal and scope (G)

| ID | Decision | Status |
| --- | --- | --- |
| G1 | The 6-month proof is that Mirror is daily-drivable. | confirmed |
| G2 | The first user is Cameron, as a power user. | confirmed |
| G3 | The daily-drive target is VMs (portable across Cameron's systems, no dependence on one physical box). Real hardware comes after v1. | confirmed |
| G3a | VMware is the primary daily-drive hypervisor. VirtualBox is also supported. QEMU is the automated test platform. | confirmed |
| G4 | v1 must-haves: persistent files, network and internet, terminal with native CLI tools, AI agent. | confirmed |
| G5 | Outside v1: web browser (arrives via compatibility layers, never written from scratch), dev tools such as git and compilers, Windows and Linux app compatibility, GPU, unified capacity tiering, inference-aware scheduling, physical hardware, cloud agent desktops. | confirmed |
| G6 | Eventual daily use: code/dev, browsing plus AI, local models, specific Windows apps. | confirmed |
| G7 | v1 performance gates: boot to desktop, idle memory, input latency, agent action latency, measured against Linux and Windows in the same VM. | confirmed |
| G8 | Storage pooling gets a v1 slice across two or more virtual disks. | confirmed |
| G8a | The v1 pool is RAID-1-like (mirrored) for redundancy, not striped. | confirmed |
| G8b | The pool is a block layer beneath the filesystem. Every write goes to all members; reads are spread across members, so each disk connection serves as its own bandwidth pipe. A failed member degrades the pool without losing data. | confirmed |
| G8c | Long-term preferred hardware is several NVMe drives plus a hard drive for important storage. This is future context for pooling and tiering, not a v1 requirement. | confirmed |

## Agent harness (A)

| ID | Decision | Status |
| --- | --- | --- |
| A1 | Agent computer use is typed operations first, with a screen/input fallback for apps that lack typed operations. | confirmed |
| A1a | The fallback is designed now and ships with the compatibility layers. No v1 app needs it. | confirmed |
| A2 | v1 modes: human only, together, agent alone (local). Cloud agent desktops come later. | confirmed |
| A3 | Typed operations never touch the human's focus or input. Human and agent each have their own input seat (cursor, focus, keyboard) and never fight for control. The human can interact with the agent's window while watching it work. | confirmed |
| A3a | In v1 the agent's cursor visualizes typed operations: it moves to the element an operation targets. Real synthetic clicks belong to the fallback (A1a). | confirmed |
| A4 | Grant sources: live human approval, saved human policies, agent to sub-agent delegation (attenuating only), apps sharing their own objects. | confirmed |
| A5 | Default agent authority is configurable. v1 ships two presets: read-only and full access. | confirmed |
| A6 | Presets are bundles of kernel-enforced capabilities, never a global mode flag. | confirmed |
| A7 | There are no hard-coded "always confirm" action classes. Confirmation rules are configurable policy, tuned by experiment and the audit logs. | confirmed |
| A8 | Audit log: immutable. Neither humans nor agents can edit it; both can view it. The user sets its TTL and expiry is the only way entries leave. TTL changes are themselves logged. Optional retention copies go to local or cloud storage. | confirmed |
| A9 | Immutable core: compiled into the kernel and protected against runtime tampering in v1. It holds audit integrity, the capability core and its ceilings, human override, and input provenance (contents may change). No signing path runs code in it, and the user experience cannot reach it. It does not limit root's normal authority. | confirmed |
| A10 | Experiment builds: a compile-time profile leaves out the immutable core and makes its rules runtime-configurable. A persistent marker appears on screen and in every audit entry. | confirmed |
| A11 | v1 principals: one owner, who is root (a full capability bundle, below the immutable core), plus agent principals. More human accounts come later. | confirmed |
| A12 | There is one Agent API. The built-in assistant is a client of it, and external agents connect through the same API under the same policy. | confirmed |
| A13 | The Agent API uses a Mirror-native protocol, with an MCP adapter for interop. | confirmed |
| A14 | The human talks to and supervises the agent through an assistant panel that opens and closes at will. | confirmed |
| A15 | The harness is built early, right after the kernel object and process model. The audit log stays in memory until the filesystem exists. | confirmed |
| A16 | Credentials use a standard keyring plus broker. | confirmed |
| A16a | The broker makes provider calls itself, so agent processes never see raw keys or tokens. They hold only a capability to request completions. | confirmed |
| A17 | The shell is typed and agent-native: commands are typed operations from the same registry agents use, with structured output. | confirmed |

## Inference (I)

| ID | Decision | Status |
| --- | --- | --- |
| I1 | Both cloud and local inference ship in v1. | confirmed |
| I2 | v1 providers: OpenAI API key, ChatGPT sign-in, Anthropic API key, any OpenAI-compatible URL. | confirmed |
| I3 | Local inference is Mirror's own engine, using Colibri as a reference. It runs CPU-only in the VM. | confirmed |
| I4 | The first model family is Qwen3 MoE, following Colibri's Qwen3.6 engine: Qwen3.6-35B-A3B (thinking and tools) and Qwen3-Coder-30B-A3B (tools). Tool calling is required for the agent. OLMoE is excluded because it has no tool calling. | confirmed |
| I4a | Expert tiering lives in the engine in v1, as in Colibri: an engine-owned expert cache reads weights from ext2 files on demand across the mirrored pool. OS-managed residency (unified capacity) stays post-v1, with this cache as its first consumer. | confirmed |
| I4b | Sizing: the host has 32-48 GB RAM, so the daily-drive VM gets about 20-24 GB. The model is partly resident, and expert streaming from disk is required. | confirmed |
| I7 | Sharded models in v1: the engine loads models split across many weight files. Disk placement belongs to the storage pool (mirrored in v1), not the engine. | confirmed |
| I8 | TurboQuant KV-cache compression in v1, about 3-4 bits per value, starting with the PolarQuant stage as Colibri does (rotation plus polar quantization, no calibration). | confirmed |
| I8a | Which TurboQuant stages ship is decided by measured quality: whichever configuration doesn't hurt quality on Mirror's own benchmark. PolarQuant alone is the starting point; QJL is enabled only if measurements show it helps, since an independent replication found it can hurt. | confirmed |
| I9 | PLE (per-layer embedding) models come after v1. Their embedding tables stay on disk and are read a few rows per token, as in Colibri's Qwen3.8 engine. | confirmed |
| I10 | CLM ([arXiv 2609.37725](https://arxiv.org/abs/2609.37725)) context-as-file in v1. Each agent task's context is a Mirror OS object, not a plain file: edits are typed operations recorded in the audit log, every segment carries an origin tag (human, tool, system, model), and policy can stop a model from rewriting human or system segments. This addresses the self-planted-instruction risk the paper leaves open. | confirmed |
| I10a | Suffix Cache Reuse is a v1 engine requirement. KV cache for text that survives a CLM edit is reused and re-positioned in session-private slots, with re-prefill as the fallback. Keys are stored in a form that can be re-positioned under TurboQuant (for example, quantized before the position encoding is applied). | confirmed |
| I11 | TurboQuant is an OS-wide facility, used wherever approximate vectors are acceptable: all model KV caches (a shared library or service, not one engine's private code), an OS semantic index (files, apps, UI trees, audit history), agent memory, and later the capacity tiers. | confirmed |
| I11a | Both the OS semantic index and agent memory ship in v1, which adds an embedding model to the v1 engine. | confirmed |
| I11b | Hard rule: TurboQuant, which is lossy, is never used for exact data: file contents, process memory, the page cache, or audit log contents. It applies only to vectors where approximation is acceptable. | confirmed |
| I11c | The embedding model is the Qwen3-Embedding family, which shares the Qwen3 architecture with the I4 engine. | proposed |
| I5 | Engine safety: agent operations are managed at kernel level, the OS validates every tool call against the registry and the active grant, and the engine has built-in monitoring hooks. | confirmed |
| I6 | Local inference makes three kernel features v1 requirements: XSAVE/AVX2 user state, userspace on all CPUs, and multi-GB per-process memory. | confirmed |

## Capacity and data movement (C)

| ID | Decision | Status |
| --- | --- | --- |
| C1 | Dijkstra (and its widest-path variant for bulk transfers) is the router: it picks the route for transfers between memory and storage locations. It is not the placement policy that decides what lives where, what to evict, or when to move. | confirmed |
| C2 | Three layers: a telemetry map of devices and links, a router, and a cost-aware placement policy that uses the router's cost as its eviction price, with concurrent transfers split by bandwidth. | confirmed |
| C3 | v1 records device and link telemetry through spec 0019 (Mirror perf instrumentation and telemetry, reworded 2026-10-09 from "the performance harness, spec 0018") but builds no router, so the placement policy has real data when the unified-capacity pillar arrives. | confirmed |
| C4 | The router is built once the hardware has two or more genuinely different transfer paths. | confirmed |

## System (S)

| ID | Decision | Status |
| --- | --- | --- |
| S1 | The v1 drivers target the device set that VMware, VirtualBox and QEMU all emulate. VirtIO is not a v1 target. | confirmed |
| S1a | The v1 device set is AHCI SATA storage (NVMe later), an Intel e1000-family NIC, PS/2 keyboard and mouse, and the UEFI boot framebuffer for display (VMware SVGA II later). The QEMU test profiles switch to this set. | confirmed |
| S1b | Clarifies S1a: in QEMU tests the boot image may sit on a virtio-blk device that only firmware reads. Mirror never binds it, and Mirror's drivers see only the v1 device set. | confirmed |
| S2 | The filesystem is ext2 first, then ext3 journaling. A native filesystem comes later. | confirmed |
| S2a | v1 does not write FAT32. The boot partition is produced by host tooling. | confirmed |
| S3 | Dependencies are decided case by case, favoring native unless it badly hurts the schedule. Each spec lists its crates and their licenses. | confirmed |
| S4 | The general scheduler becomes work-conserving and gets basic priority classes (interactive above normal above background) in v1, replacing the hard 32%-of-one-core cap. This is ordinary OS scheduling, not the inference-aware pillar; it protects the input-latency gate while a local model runs. | confirmed |
| S5 | Two boot artifacts: a disk image (GPT: FAT32 boot partition plus ext2 root) for development, and a bootable ISO. Both run in VMware and VirtualBox, and the disk image is also used for QEMU tests. They replace the host-directory-only boot. | confirmed |
| S5a | The v1 ISO boots Mirror live. An installer that writes a disk from inside Mirror needs FAT32 writes (S2a), so it comes after v1. | confirmed |
| S6 | ABI reset, done before more code depends on the current ABI. | confirmed |
| S6a | Fast SYSCALL entry with 6 register arguments and a per-process personality selector (native, later Linux and Windows tables). INT 0x80 stays as a legacy shim. | confirmed |
| S6b | Kernel object model: reference-counted objects with rights, handle transfer over channels, channel creation on demand, and waiting on many objects at once. It replaces the fixed 8 handles and 2 channels. | confirmed |
| S6c | Shared memory objects that can be mapped between processes (surfaces, large buffers, model weights), replacing copy-everything syscalls. | confirmed |
| S6d | Window, app-launch and desktop policy moves out of kernel syscalls into a userspace compositor protocol. | confirmed |
| S7 | Boot hygiene: init's test battery and production asserts on test fixtures move behind `boot-smoke`, and serial logging of user content and console-text test oracles move behind `boot-smoke` or `test-trace`. A normal boot can never panic on a test outcome or leak typed text. (Reworded 2026-10-09 per spec 0010-D2.) | confirmed |
| S7a | Three kernel build flavors: default (clean, what you daily-drive), `test-trace` (default plus test serial markers, used by captures), and `boot-smoke` (includes test-trace, plus the full self-test and fault battery). | confirmed |
| S7b | In default builds, serial output never depends on user input. The one exception is opt-in timing markers enabled by the `mirror.perf=` boot option (spec 0018-D9), which carry no user content. | confirmed |
| S9 | v1 speculative-execution policy: CPU vulnerability status is detected and reported, with no KPTI. This rests on the premise that only Mirror-built native code runs inside Mirror in v1 (external agents connect through the Agent API). A KPTI/MDS mitigation spec is a prerequisite before any third-party native code runs. | confirmed |
| S8 | A native userspace runtime comes before native tools: `mirror-sys` (one shared table of syscall numbers and errors) and `mirror-rt` (allocator, handle types, threads, panic output). A Rust `std` port follows later. | confirmed |

## Process (P)

| ID | Decision | Status |
| --- | --- | --- |
| P1 | Specs live in `docs/specs/` as small numbered files following the template in README.md. Nothing is built from unconfirmed decisions. | confirmed |
| P2 | Cameron dispatches agents. Claude proposes non-blocking side tasks on request. Kimi stays on UI. | confirmed |
| P3 | CI is deferred. Work is mostly one agent at a time. | confirmed |
| P4 | The license is deferred while the repo is private. Every adopted dependency's license is recorded. | confirmed |
| P5a | Pin the Rust toolchain to 1.96.0 and bump it deliberately. | confirmed |
| P5b | Add an `AGENTS.md` that every agent reads: spec rules, test commands, coding conventions, references policy. | confirmed |
| P5c | Gradually split the readme into `docs/` (building, testing, architecture, ABI), leaving a short readme. | confirmed |
| P6 | References policy: the local reference trees (linux, nt5src, freebsd-src, colibri) are best-practice references only, and Mirror code is written independently against Mirror's own interfaces. They live on the development machine; exact paths vary by machine, so agents ask Cameron for a tree's location rather than assuming one. readme.md:694 ("reuse device-specific source where licensing permits") is reworded to match. | confirmed |
| P7 | Each agent task runs on its own branch, and Cameron merges it. No force-pushes or rewriting of pushed history. Push only when asked. | confirmed |
| P7a | Exception to P7: spec and documentation commits (`docs/specs/` and other docs-only changes) go directly to `main` and are pushed, with no task branch. | confirmed |
| P8 | Each commit runs the targeted test cases for what it touches. The full `scripts/test.ps1` (and `-AgentDemo` when agent-demo code is touched) runs when a spec is complete, before its branch is merged. | confirmed |

## Milestones (M)

v1 order. The harness follows the process model (A15), and harness code is written SMP-safe from the start.

| ID | Milestone | Status |
| --- | --- | --- |
| M0 | Hygiene, VM boot images, performance baselines | confirmed |
| M1 | ABI reset and `mirror-sys`/`mirror-rt` | confirmed |
| M2 | Processes and service manager | confirmed |
| M3 | Agent harness core | confirmed |
| M4 | SMP userspace, scheduler, AVX2, large memory | confirmed |
| M5 | Storage: runtime drivers, mirrored pool, ext2 then ext3 | confirmed |
| M6 | Network: e1000, TCP/IP, DNS, TLS, plus the MCP adapter | confirmed |
| M7 | Typed shell, native tools, assistant panel | confirmed |
| M8 | AI: keyring broker, providers, Mirror's inference engine, built-in assistant | confirmed |
| M9 | Performance gates pass, completing v1 | confirmed |

## Open questions (O)

| ID | Question |
| --- | --- |
| O1 | Resolved: Colibri is github.com/JustVugg/colibri (Apache-2.0), cloned on the development machine (see P6 for paths). |
| O2 | Which specific Windows apps Cameron returns to Windows for (the post-v1 compatibility target list). |
| O3 | Performance gate target numbers, set after baseline measurements of Linux and Windows in the same VM. |
| O4 | ChatGPT sign-in: which OAuth or device flows OpenAI supports for third-party clients. Needs research. |
| O5 | Resolved: see I4, I4a, I4b. |
| O6 | Resolved: FreeBSD is github.com/freebsd/freebsd-src, cloned on the development machine (see P6 for paths). |
