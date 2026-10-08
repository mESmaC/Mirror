# Mirror

Mirror is a proposed operating system, written primarily in Rust, designed for people and AI agents to use the same computer efficiently. Its goals are to run selected Windows and Linux applications, expose fast and controllable automation, and coordinate local inference with the rest of the system.

**Status:** the freestanding Rust kernel boots through UEFI/Limine in QEMU with its own exception descriptors and page tables, reusable physical pages, a 1 MiB kernel heap, and timer interrupts. Debug and optimized boot tests cover these features, including allocation while interrupts run and fatal exception diagnostics. Native userspace, broader driver ports, application compatibility, unified resource management, and inference services remain future work. The broader designs below are proposals; performance improvements must be demonstrated with measurements.

## Build and boot the kernel

The kernel targets `x86_64-unknown-none` using stable Rust (minimum 1.88; developed with 1.96.0). Rustup installs the target and components declared in `rust-toolchain.toml`. Cargo dependencies are locked, and the boot scripts download Limine v12.9.3 from its official release and verify the archive's pinned SHA-256. The Rust protocol bindings are pinned to 0.5.0, which supports stable Rust and Limine base revision 3.

From the repository root in PowerShell:

```powershell
# Compile the kernel and stage its UEFI boot files.
.\scripts\build.ps1

# Run formatting, Clippy, and all six debug/release boot cases.
.\scripts\test.ps1

# Build the smoke-test kernel, boot it, and verify its serial results and exit code.
.\scripts\run.ps1 -Smoke

# Verify the optimized kernel, including LTO.
.\scripts\run.ps1 -Smoke -Release

# Verify fatal exception diagnostics (intentional failures inside the guest).
.\scripts\run.ps1 -Smoke -Fault PageFault
.\scripts\run.ps1 -Smoke -Fault InvalidOpcode

# Boot normally; diagnostics precede an idle loop woken by timer IRQs. Ctrl+C stops QEMU.
.\scripts\run.ps1
```

Running requires `qemu-system-x86_64.exe` and a matching pair of EDK II/OVMF firmware files. The runner checks `.tools/qemu/`, then PATH; alternatively pass `-QemuPath`, `-FirmwareCode`, and `-FirmwareVars`, or set `QEMU_PATH`, `OVMF_CODE`, and `OVMF_VARS`. QEMU's bundled `edk2-x86_64-code.fd` pairs with `edk2-i386-vars.fd`. Obtain Windows builds through the [official QEMU download page](https://www.qemu.org/download/#windows); QEMU is a local development prerequisite, not a checked-in dependency.

`build/esp/` contains `EFI/BOOT/BOOTX64.EFI`, `limine.conf`, and `kernel.elf`. QEMU exposes this directory as a virtual FAT boot disk; the scripts do not format physical disks. Firmware variables are copied into `build/` before use. `.tools/`, `build/`, and `target/` are ignored by Git. This produces an EFI boot tree, not an installer or standalone disk image.

The smoke test has a 30-second boot timeout. It checks frame ownership, alignment, reuse, rejected invalid frees, physical-page contents, returning breakpoints, and mapping/unmapping. Heap checks cover mixed layouts, data preservation, exhaustion, hole reuse/coalescing, rejected oversized layouts, and actual `Box`/`Vec` allocation without leaks. Timer checks require repeated hardware IRQ delivery and acknowledgement. A separate allocation loop requires timer progress while verifying that the heap lock restores enabled interrupts. The final unmapped access must reach the expected page-fault handler. The runner requires each subsystem's success marker, `MIRROR_BOOT_OK`, and QEMU exit status 33.

`-Fault PageFault` and `-Fault InvalidOpcode` select intentional unexpected exceptions through the smoke kernel's boot command line. These cases require the matching exception vector, error code, page-fault address where applicable, and QEMU exit status 35. A panic, timeout, or ordinary successful boot cannot satisfy them. The normal kernel excludes these fault triggers and QEMU exit-port writes.

Serial and QEMU diagnostic logs are saved under `build/`. `scripts/test.ps1` runs all three cases in debug with 128 MiB and release with 256 MiB, retains their logs under `build/test-logs/`, and leaves a normal kernel staged afterward. Build and boot commands should run one at a time because they share the staged files.

The current kernel runs one CPU. Its GDT, TSS, 32 exception gates, 16 legacy IRQ gates, and dedicated double-fault stack are installed before interrupts are enabled. The QEMU q35 timer uses the legacy PIC/PIT at approximately 100 Hz. Only IRQ0 is unmasked; its handler increments a counter and acknowledges the interrupt without allocation, logging, or locking. The normal idle loop wakes on IRQs. Ticks measure delivered interrupts, not calibrated elapsed time; long critical sections can lose ticks. APIC/IOAPIC support and hardware discovery remain future work.

Paging ownership is established by deep-copying Limine's four-level hierarchy, retaining existing mappings and huge-page leaves, and switching CR3. New heap and scratch mappings are supervisor-only, writable, and non-executable; unmapping invalidates the local TLB. NX and supervisor write protection are enabled, but dedicated execute/write-protection fault tests are not yet implemented. Double-fault stack installation is also not yet tested with an induced double fault. Existing aliases are retained; this ownership transition does not establish complete kernel memory protection. Five-level paging is explicitly rejected for now.

Physical allocation begins with a bump allocator, then tracks usable pages with a dynamically sized two-bit bitmap. Previously allocated page tables remain tracked, bitmap pages stay reserved, and released pages can be reused. Metadata currently needs one contiguous usable span. Freeing a page requires ending its uses and withdrawing non-direct-map aliases first; checks reject reserved, misaligned, never-allocated, metadata, and already-free pages. They cannot detect a stale ownership claim after a page has been reallocated. The bootstrap stack and boot responses still belong to Limine, so bootloader memory remains reserved.

The fixed 1 MiB heap has unmapped guard pages and uses `linked_list_allocator` 0.10.6 with an interrupt-state-preserving lock. Blocks are reused and coalesced; heap backing pages stay mapped. Unsupported sizes/alignments return allocation failure before entering the dependency. Interrupt and exception handlers must not allocate. Heap growth, bounded allocation latency, page-table reclamation, address-space isolation, scheduling, userspace, and a general device framework remain future work.

Kernel entry is in [kernel/src/main.rs](kernel/src/main.rs), boot requests in [kernel/src/boot.rs](kernel/src/boot.rs), physical allocation in [kernel/src/memory.rs](kernel/src/memory.rs), paging in [kernel/src/paging.rs](kernel/src/paging.rs), and exception handling in [kernel/src/interrupts.rs](kernel/src/interrupts.rs). The heap is in [kernel/src/heap.rs](kernel/src/heap.rs) and the first timer driver in [kernel/src/timer.rs](kernel/src/timer.rs). See Rust's [bare-metal target documentation](https://doc.rust-lang.org/rustc/platform-support/x86_64-unknown-none.html) and the [Limine bootloader](https://github.com/Limine-Bootloader/Limine) for the underlying build and handoff contracts.

## Goals

- A responsive desktop with a small baseline resource footprint and optional background services.
- Windows and Linux application compatibility, expanded through a published, tested application matrix.
- Structured application operations that agents can discover and invoke without repeatedly interpreting screenshots.
- OS-enforced limits on agent access to files, processes, networks, devices, credentials, and compute.
- A common resource policy for RAM, VRAM, storage, and cache locality, aware of the differences between them.
- Local inference that cooperates with interactive applications over memory, compute, bandwidth, power, and scheduling.
- Storage pooling that uses available devices and transfer paths while maintaining explicit durability guarantees.

Rust is the primary implementation language. Existing compatibility and inference components may use other languages. Language choice alone does not establish speed, isolation, or compatibility.

## Proposed architecture

Development begins with the custom Mirror kernel and then builds its native userspace. The first executable milestone is a kernel booting on virtual hardware in QEMU. Resource policies and inference services will be implemented against Mirror's own memory, scheduling, and device interfaces as those become available; a simulator or hosted operating-system prototype is not the starting point.

| Component | Responsibility |
| --- | --- |
| Kernel | Address spaces, threads, scheduling mechanisms, interrupts, memory mappings, IPC, capability enforcement, and device isolation where hardware supports it. |
| System services | Filesystems, networking, device management, application lifecycle, and desktop services. Place services outside the kernel where practical. |
| Agent broker | Discover typed operations, validate arguments and authority, apply quotas, dispatch actions, and record results. |
| Capacity manager | Account for resources, choose object placement, reserve transfers, and coordinate reclamation and prefetching. |
| Inference services | Execute models through replaceable backends and supply workload hints to the capacity manager and scheduler. |
| Storage service | Manage a logical pool, extent placement, checksums, redundancy, recovery, and background rebalancing. |
| Compatibility environments | Provide the Linux ABI and a host substrate for Windows compatibility components, with their own isolation boundaries. |

The kernel supplies bounded mechanisms and enforces authority. Higher-level resource policy and model execution belong in restartable services. A model crash or unavailable cloud provider must not stop the machine from booting or being used manually.

Initial development target: **x86-64, UEFI, and QEMU**, using Limine for the boot handoff, followed by a deliberately small hardware support list. QEMU provides a repeatable machine to boot the actual Mirror kernel. GPU and target-application requirements will guide the first physical hardware targets.

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

Loading an executable is only one part of compatibility. Applications also depend on system calls, libraries, threading, graphics, filesystem behavior, services, and sometimes kernel drivers.

| Application class | Proposed route | Initial boundary |
| --- | --- | --- |
| Native Mirror applications | Rust services and applications using Mirror's APIs. | First supported environment. |
| Linux command-line programs | ELF loading and an explicitly bounded Linux ABI. | Start with small static binaries and the calls they actually require. |
| Larger Linux applications | Expand the ABI, dynamic loading, libraries, graphics, and service support. | Compatibility is verified per application and version. |
| Windows user applications | Port Wine onto a sufficient Mirror host substrate. | Start with named applications; PE loading alone is insufficient. |
| Applications needing a full guest OS | Investigate integrated Linux or Windows VMs later. | Requires a virtualization backend, guest integration, suitable hardware, and appropriate guest software licensing. |

[FreeBSD's Linux compatibility documentation](https://docs.freebsd.org/en/books/handbook/linuxemu/) illustrates both the feasibility of implementing a foreign ABI and the limits of that approach. POSIX source compatibility is not equivalent to Linux binary compatibility.

[Wine](https://www.winehq.org/about) supplies Windows API compatibility on supported hosts. [Proton](https://github.com/ValveSoftware/Proton) combines Wine and other components for Windows games on Linux; it is a potential source of reusable work, not a component that immediately makes a new kernel Windows-compatible. Kernel drivers, anti-cheat, DRM, and applications with deep platform dependencies need separate assessment.

GPU support is an early feasibility gate. For example, Mesa's RADV Vulkan driver depends on the Linux `amdgpu` kernel interface; a Vulkan API alone does not supply the required driver stack. CUDA likewise has supported OS and driver requirements. Mirror will pursue native driver ports for a selected hardware family and adapt the corresponding userspace graphics and compute stack. Accelerated inference is a milestone that depends on that working port. See [Mesa RADV](https://docs.mesa3d.org/drivers/radv.html) and [CUDA system requirements](https://docs.nvidia.com/cuda/cuda-installation-guide-linux/).

Booting Mirror inside QEMU does not give Mirror the ability to host accelerated guest operating systems. That is a separate project; [QEMU's accelerators](https://www.qemu.org/docs/master/system/introduction.html) rely on host facilities.

The first Windows application targets are still to be selected. Record application versions, dependencies, required workflows, graphics needs, and known failures in the compatibility matrix.

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
| 4. Device framework and driver ports | Platform adaptation layer, device discovery, DMA and interrupt support, VirtIO storage/network drivers, and selected physical-device ports. | Named devices work through Mirror's interfaces; DMA ownership and interrupt handling are checked; failure and device recovery paths are exercised. |
| 5. Native services, compatibility, and AI | Filesystem and shell, bounded Linux ABI, selected Wine host interfaces, typed-operation broker, CPU inference, and capacity-manager integration. | Selected native and static Linux programs run; named Windows workflows pass as Wine support arrives; agent permissions are enforced; inference preserves interactive headroom. |
| 6. GPU ports and accelerated inference | Selected GPU driver port, userspace graphics/compute integration, real RAM/VRAM placement, and responsiveness controls. | Correct rendering and model outputs on named hardware; reproducible mixed-load measurements; migration and resource-pressure recovery. Assess the port's dependencies during earlier stages. |
| 7. Usable desktop and durable pool | Human/agent desktop, installer/update recovery, storage pooling and recovery, expanded compatibility, and broader device testing. | Recovery after interrupted writes/updates and device failures, usable manual controls, application-specific workflow tests, and repeatable agent permission tests. |

Full guest-OS integration remains a separate later compatibility option. Universal Windows/Linux compatibility, arbitrary hardware support, and a fully optimized inference stack are outside the first milestone.

### First implementation slice

Build the freestanding Rust kernel and its EFI boot tree first. Limine supplies the initial boot environment and memory map; Mirror owns the kernel entry point, serial diagnostics, handoff validation, and panic behavior. This establishes the executable base for physical memory management and exception handling before entering native userspace.

The first boot slice's acceptance criteria are:

1. The kernel builds for a bare-metal x86-64 target without a host operating-system runtime.
2. The EFI boot tree reaches Mirror's entry point under QEMU and emits an identifiable serial boot message.
3. The kernel validates the required boot-protocol revision and memory-map response before inspecting entries; missing required data produces a diagnostic halt.
4. Memory-map accounting and frame allocation distinguish usable memory from reserved regions. Frame recycling, page-table ownership, and the initial heap are implemented; bootloader and page-table reclamation remain later work.
5. Panic and normal idle paths remain within Mirror. Fatal paths disable maskable interrupts and halt; normal idle enables them and wakes for timer IRQs.

This slice establishes boot and handoff behavior. It does not yet claim process isolation, driver compatibility, application execution, or inference capability.

### Performance evidence

Once real backends exist, record idle RAM and CPU use, boot and application launch time, interactive p95 latency, model load time, time to first token, inter-token latency, tokens per second, peak RAM/VRAM, transferred bytes, storage throughput, and recovery behavior.

Compare the same hardware, application workload, model, quantization, context length, concurrency, and warm/cold-cache conditions. Include both models that fit in VRAM and oversubscribed workloads. Evaluate a static placement baseline as well as existing operating-system/runtime behavior. Report throughput and responsiveness together, including regressions, before claiming Mirror is faster.
