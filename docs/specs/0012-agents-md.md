# 0012: AGENTS.md shared conventions

Status: confirmed (2026-10-09). Kimi's UI branch name is still to be supplied; AGENTS.md carries the confirmed placeholder `<name supplied by Cameron>` until then (decisions.md "Pending (resume here)").
Depends on: none. Precondition: `docs/specs/` is committed to `main` first (confirmed M0 order).
Ledger: P5b, P1, P2, P3, P6, P7, P7a, P8, P9 (the text also restates G3a, P4, P5a, P5c, S3, S7, S7a, S7b, O1, O6)

## Goal
A root-level `AGENTS.md` exists, and every agent working on Mirror (Claude, GPT Astra, Kimi K3) reads it first. In under 120 lines it tells an agent with no prior context where the goal and ledger live, how the spec process gates work, how to build and test without corrupting shared state, which tests to run when, which coding conventions the code already follows, the references policy, and how branches work. A root `CLAUDE.md` imports it, and the readme's three driver-porting paragraphs (readme.md:694, :698 and :700) are reworded to match the references policy (P6). The full AGENTS.md text is below. No code changes. In the confirmed M0 order, 0012 lands first, right after `docs/specs/` is committed. From then on, every spec or change that alters something AGENTS.md describes updates those lines in the same commit (P9, a permanent rule).

## Non-goals
- Fixing the traps the text documents: the workspace linker flags, the missing lock on `build/`, the uncleared log directories (0011-D7 clears them), and the unchecked `-NoBuild` staging. Follow-up specs fix the rest.
- Pinning the toolchain or adding workspace lints (0011). Moving the test battery and user-content logging behind `boot-smoke` and `test-trace` (0010).
- Rewording any readme paragraph other than readme.md:694, :698 and :700. Splitting readme.md (P5c), CI (P3), LICENSE or CONTRIBUTING (P4), and architecture or ABI docs.

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| 0012-D1 | A root `AGENTS.md` is the shared entry point for every agent. It covers spec rules, test commands, coding conventions and the references policy. | confirmed (ledger P5b) |
| 0012-D2 | AGENTS.md restates the spec process: specs live in `docs/specs/`, and nothing is built from `proposed` or `open` decisions. | confirmed (ledger P1) |
| 0012-D3 | AGENTS.md restates the roles and workflow: Cameron dispatches, Claude proposes side tasks on request, Kimi stays on UI. Work is mostly one agent at a time, with no CI. | confirmed (ledger P2, P3) |
| 0012-D4 | Add a root `CLAUDE.md` whose only content is the import line `@AGENTS.md`. Claude Code loads CLAUDE.md automatically, and an import avoids keeping a second copy in sync. | confirmed (2026-10-09) |
| 0012-D5 | References policy: the local trees are best-practice references only, and Mirror code is written independently against Mirror's own interfaces. Their paths vary by machine, so agents ask Cameron for a tree's location rather than assuming one. This spec rewords readme.md:694 ("Reuse device-specific source where licensing permits") to match. (Amended 2026-10-09: it also rewords readme.md:700, which opens with "Porting source" and lists licensing in each driver's assessment, and readme.md:698, where "port drivers for named physical hardware" becomes "write drivers for named physical hardware" and "each port has a bounded interface" becomes "each driver has a bounded interface". The replacement wording of all three lines under Interfaces is confirmed.) | confirmed (2026-10-09, ledger P6) |
| 0012-D6 | Each agent task runs on its own branch, and Cameron merges it. Agents never force-push or rewrite pushed history, and push only when asked. Exception: spec and documentation-only commits (`docs/specs/` and other docs-only changes) go directly to `main` and are pushed, with no task branch. | confirmed (2026-10-09, ledger P7, P7a) |
| 0012-D7 | Each commit runs the targeted test cases for what it touches, chosen by the rule under "When to test" below. The full `scripts/test.ps1` (plus `-AgentDemo` when agent-demo code is touched) runs when a spec is complete, before its branch is merged. | confirmed (2026-10-09, ledger P8) |
| 0012-D8 | Task branches start from the current `main` and are named `<agent>/<spec>-<short-name>`, where `<agent>` is `claude`, `astra` or `kimi` (for example `claude/0011-toolchain-and-lints`). A task without a spec uses `<agent>/<short-name>`. Kimi's existing UI branch keeps its name. Rationale: Cameron can tell who made a branch and which spec it implements without opening it, and a cold-start agent needs no extra instruction. Alternative: each dispatch prompt names the branch and its base. | confirmed (2026-10-09) |

## Interfaces
Files, all at the repository root:
- `AGENTS.md` (new): the text below. Lines tagged `[D8]` depend on 0012-D8. Drop the tag when writing the file. Kimi's UI branch appears as the confirmed placeholder `<name supplied by Cameron>` until Cameron supplies the name; replacing it is then a docs-only commit to `main` (P7a, P9).
- `CLAUDE.md` (new): exactly `@AGENTS.md` (0012-D4).
- `readme.md`: replace three single-line paragraphs (0012-D5). No other readme line changes. The replacement wording of all three is confirmed (2026-10-09).
  - readme.md:694, which begins "Port drivers into Mirror through a documented platform adaptation layer.", becomes:

    > Build drivers on a documented platform adaptation layer: Mirror's own interfaces for MMIO, DMA mappings and buffer lifetimes, interrupts, allocation, synchronization, timers, and work queues. Each driver is written independently against those interfaces. Other operating systems' drivers, including the local reference trees, serve as best-practice references for device behavior, initialization order, and error handling. Device discovery, power management, firmware loading, and recovery also need explicit integration.

  - readme.md:698, which begins "Begin with serial diagnostics and a small set of virtual devices", becomes the paragraph below. Only two phrases change: "port drivers for named physical hardware" becomes "write drivers for named physical hardware", and "so each port has a bounded interface" becomes "so each driver has a bounded interface". The last sentence, about a userspace library's own port, is unchanged.

    > Begin with serial diagnostics and a small set of virtual devices, then establish the device framework and write drivers for named physical hardware. Keep architecture-specific operations and device-independent services separate from device logic so each driver has a bounded interface to implement and test. Userspace libraries that expect a particular kernel driver ABI need a matching compatibility interface or their own port.

  - readme.md:700, which begins "Porting source does not mean Mirror can directly load", becomes the paragraph below. It keeps the point that other kernels' modules and binary drivers cannot be loaded, states that nothing is copied from the references, and drops licensing from the per-device assessment; dependency licenses stay recorded in the spec that adopts them (P4, S3).

    > Mirror copies no driver source from other operating systems, and it cannot directly load Linux kernel modules or closed-source Windows/Linux binary drivers, which depend on their original kernel's ABI and internal behavior. Before a device becomes part of the support plan, assess its hardware documentation, firmware requirements, and dependencies, and identify the reference implementations that show how production systems handle it.

````markdown
# AGENTS.md

Read this before doing anything. It applies to every agent on Mirror: Claude, GPT Astra and Kimi K3.

## Goal and decisions
Mirror is an agent harness that is also a complete, fast, independent x86_64 operating system, written in Rust plus assembly. It boots through UEFI and Limine. VMware is the VM target for daily use and development, and QEMU runs the automated tests. VirtualBox is not a target.
- Goal and v1 definition: `docs/specs/0000-north-star.md`.
- Decision ledger: `docs/specs/decisions.md`. It is authoritative.

## Spec process
- Work comes from small specs, `docs/specs/NNNN-name.md`, written to the template in `docs/specs/README.md`, which also gives the spec map and execution order.
- Decision status: `confirmed` means Cameron approved it, `proposed` means it awaits Cameron, and `open` means it is undecided.
- Never build from a spec that has `proposed` or `open` decisions in your task's scope. Stop and ask Cameron.
- Never contradict a confirmed ledger entry. To change one, update `decisions.md` first and get it re-confirmed.
- If the task needs a design choice that no confirmed decision covers, ask Cameron instead of choosing.
- Dependencies are decided case by case, favoring native code. The spec that adds a crate or tool records its exact version and license. Pin crates with `=` and build with `--locked`.

## Roles and branches
- Cameron dispatches agents and confirms decisions. Claude proposes non-blocking side tasks only when asked. Kimi works on UI only.
- Work is mostly one agent at a time. There is no CI, so the test scripts are the gate (see "When to test").
- Each task runs on its own branch, and only Cameron merges it. Commit to your task branch, never to `main`. Never force-push or rewrite pushed history. Push only when asked.
- Exception: spec and documentation-only commits (`docs\specs\` and other docs-only changes) go directly to `main` and are pushed, with no task branch.
- [D8] Start the branch from the current `main` and name it `<agent>/<spec>-<short-name>`, where `<agent>` is `claude`, `astra` or `kimi`, for example `claude/0011-toolchain-and-lints`. A task without a spec uses `<agent>/<short-name>`.
- Kimi's UI branch, `<name supplied by Cameron>`, belongs to Kimi. Other agents never check it out, rebase it, merge it, cherry-pick from it, push to it or delete it.

## Build
Run commands from the repository root in PowerShell.
- `.\scripts\build.ps1 [-Release] [-Smoke] [-AgentDemo]` builds the four userspace programs and the kernel and stages `build\esp\`. `-Smoke` and `-AgentDemo` enable the kernel features `boot-smoke` and `agent-demo`.
- The ledger (S7a) defines three kernel flavors: default (clean, for daily use), `test-trace` (adds test serial markers, for captures; spec 0010 adds it) and `boot-smoke` (`test-trace` plus the self-test and fault battery).
- Trap: `.cargo\config.toml` links every `x86_64-unknown-none` crate with `kernel\linker.ld`. `build.ps1` overrides this per program through `CARGO_ENCODED_RUSTFLAGS`: the program's own `userspace\<pkg>\linker.ld`, the small code model, and release LTO off. Never build userspace with `cargo build --workspace` or `cargo build -p mirror-<pkg>`: the images come out wrongly linked. `cargo check` and `cargo clippy` are safe because they do not link.
- Use the toolchain that `rust-toolchain.toml` selects. Change it only in a dedicated commit.

## Test
- `.\scripts\test.ps1` is the full suite: rustfmt `--check`, host unit tests, and clippy `-D warnings` with no features and with all features. Then, for debug and for release, a smoke build, every smoke boot and a normal-boot capture. It takes about 20 minutes and stops at the first failure. `test.ps1 -AgentDemo` reruns the whole suite with `agent-demo`, about 20 minutes more.
- Host tests need an explicit target because the default target is bare metal: `cargo test --locked -p <crate> --target x86_64-pc-windows-msvc`. This needs the MSVC build tools. Only the `crates\` packages have host tests.
- One boot: `.\scripts\run.ps1 -Smoke [-Release] [-Fault <Name>] [device switches]`. Without `-Smoke` it boots interactively and never exits on its own, so always pass `-Smoke`. Pass `-NoBuild` only if the staged kernel already matches the requested profile and features. Nothing checks this.
- `.\scripts\capture.ps1` boots a normal kernel, drives it with QMP input and checks screenshots.
- Run the build, run, capture and test scripts one at a time, across all agents and terminals. They share `build\` (the staged ESP and its `limine.conf`, firmware variables, serial logs), so a second command silently corrupts the first.
- QEMU and OVMF are found in `.tools\qemu\` or on `PATH`, or set through `QEMU_PATH`, `OVMF_CODE` and `OVMF_VARS`.
- A failed boot leaves its output in `build\serial.log` and `build\qemu.stderr.log`. `build\test-logs\` and `build\test-agent-logs\` receive only passing cases and are never cleared, so check timestamps before you trust them.

### When to test
- Before each commit, run the targeted cases for what it touches. `test.ps1` lists every case; copy the matching commands from it:
  - Any Rust change: `cargo fmt --all -- --check` and both clippy commands. A change under `crates\` also runs that crate's host tests.
  - Kernel or userspace code: `build.ps1 -Smoke`, then `run.ps1 -Smoke -NoBuild` with no `-Fault`, plus every case whose switches or `-Fault` name exercise the touched code (for example the `-Xhci` cases for xHCI code). To find them, search `run.ps1` and `common.ps1` for the markers that code prints. Add `-AgentDemo` to the build and runs if you touched agent-demo code.
  - Desktop, console, input or display code: also `build.ps1`, then `capture.ps1 -NoBuild -CpuCount 4`.
  - A script change: the cases that execute the changed lines.
  - Docs only: nothing to run.
- When the spec is complete, run the full `test.ps1` before Cameron merges the branch, plus `test.ps1 -AgentDemo` if you touched agent-demo code.

## Coding conventions
- Every `unsafe` block has a `// SAFETY:` comment that says why its preconditions hold, and every `unsafe fn` documents its caller contract, normally under `# Safety`. Clippy enforces the comments in the kernel, desktop, app and agent. Follow the rule in `mirror-init` and `mirror-sync` too, where the lint is not on yet. The protocol crates and `mirror-acpi` forbid `unsafe`.
- Fail closed. Fully validate untrusted input before acting on it: ELF images, firmware tables, device descriptors, syscall arguments and the boot command line. Reject anything unknown or malformed, and bound every loop and wait.
- Roll back on failure. An operation that fails partway undoes its partial effects before it returns the error. A smoke test proves this, usually by comparing allocated frames before and after.
- DMA buffers have no `Drop`. Release them only after the device is reset or quiesced, and quarantine them if that fails. Report cleanup in markers such as `dma_reclaimed=1`.
- Never add serial output that carries user content (typed text, file data, pointer state), or a per-input marker, to a default build. Gate it behind `boot-smoke` until spec 0010 adds `test-trace`.

## Serial markers
- The kernel reports results as single serial lines of the form `MIRROR_<AREA>_<EVENT> key=value ...`. Userspace console output is escaped and prefixed with `user: `, so userspace cannot forge a marker.
- The marker-family checks in `scripts\common.ps1` (driver catalog, xHCI, USB, AHCI, clock) are closed-world:
  - an unknown name in a checked family fails the run;
  - each expected marker appears exactly once, with its full field list, in the checked order.
- The generic marker lists in `scripts\run.ps1` (`$markers`) and `scripts\capture.ps1` check presence only. They miss duplicates, extra fields and order.
- `run.ps1` rejects a scenario-specific marker in every other scenario.
- So a new or changed marker needs the matching assertion change in the same commit. Values derived from other fields are recomputed in the script, never trusted as reported.

## Fault injection
Guest-side faults exist only in `boot-smoke` kernels. To add one:
1. Add a `mirror.test=<kebab-name>` token to `smoke_mode()` in `kernel\src\boot.rs`. Map it to a new `SmokeMode` variant, or to `SmokeMode::User(n)` for a probe that runs in `userspace\init` (raise init's `probe` bound in `userspace\init\src\main.rs`). Unknown or repeated tokens must still panic.
2. For a `User(n)` probe that faults, add its expected `(vector, error, cr2)` arm to `validate_process` in `kernel\src\userspace.rs`. Without it the boot panics with `unexpected probe`.
3. Add the PascalCase name to `-Fault` in `scripts\run.ps1`: both its `ValidateSet` and the `$testMode` switch. A `User(n)` probe also gets its number in the `$scenario` map and, unless it expects vector 14, an entry in the `$userVector` map. An expected fatal kernel fault joins `$fatalKernelFault`.
4. Assert the scenario's marker in that case, and reject the marker in every other case.
5. Add the case to `scripts\test.ps1`, which runs it in both profiles.
A passing smoke boot exits QEMU with code 33. An expected fatal fault exits with 35 and an exact diagnostic. Host-side faults are QEMU options in `run.ps1`: blkdebug errors, removed devices, CPU models and CPU counts.

## References
The local reference trees (linux, nt5src, freebsd-src and colibri) are best-practice references only. Read them to learn how others solved a problem. Mirror code is written independently, against Mirror's own interfaces. The trees live on the development machine and their paths vary by machine, so ask Cameron for a tree's location instead of assuming one.

## Docs
- `readme.md` is being split into `docs\` gradually. Put new long-form material under `docs\`, and keep edits to `readme.md` short.
- Keep this file current: a change to a command, path or convention described here updates the matching lines in the same commit. This rule is permanent (ledger P9).
````

Facts the text relies on (checked at HEAD 4614249):
- Linker trap: .cargo/config.toml:4-8. Per-package override: scripts/build.ps1:16-35 (flags at :25). Default member is the kernel: Cargo.toml:3. Bins have no host tests: kernel/Cargo.toml:11.
- Suite: gates scripts/test.ps1:13-20, host target :15, per-profile loop :24-146 (smoke build :31, cases :34-129, `-Xhci` cases :53-68, capture :131-132). Stops at first failure: scripts/common.ps1:2 sets `$ErrorActionPreference = 'Stop'`, and run.ps1 throws (scripts/run.ps1:470, :476). Logs are copied only after a case returns: test.ps1:35-37, directories :22.
- Interactive non-smoke boot: scripts/run.ps1:415-420. Shared paths: run.ps1:308, :312-316, scripts/capture.ps1:27-30. One at a time: readme.md:209. `-NoBuild` checks only that files exist: run.ps1:256-260. QEMU/OVMF discovery: common.ps1:617-668.
- Unsafe lints: kernel/Cargo.toml:27-31, userspace/desktop/Cargo.toml:18-22, userspace/app/Cargo.toml:17-21, userspace/agent/Cargo.toml:17-21. None in userspace/init/Cargo.toml; crates/sync/Cargo.toml:11-12 has only `unsafe_op_in_unsafe_fn`; crates/acpi/Cargo.toml:8-9 forbids `unsafe_code`. Init block without SAFETY: userspace/init/src/main.rs:127. `# Safety`: kernel/src/dma.rs:104-108; contract without the heading: kernel/src/vm.rs:323-324.
- Fail closed: kernel/src/boot.rs:213, :215; kernel/src/elf.rs:96-240; kernel/src/acpi.rs:112-116. Bounded waits: kernel/src/ps2.rs:25-28, :375.
- Rollback: kernel/src/vm.rs:304-316, :324. Frame-count proof: kernel/src/userspace.rs:387-422.
- DMA: kernel/src/dma.rs:1-6, :27, :101-114. AHCI quarantine: kernel/src/ahci.rs:603-606. Cleanup marker: kernel/src/main.rs:738.
- Unforgeable markers: kernel/src/userspace.rs:2952-2955. Closed-world families: common.ps1:14 (catalog), :122 (xHCI), :197 (USB), :274 (AHCI), :336 (clock); exactly once with full fields :125-131; ordering :137-143. Presence only: the run.ps1 `$markers` list (:481-492 and later appends) checked at :802-806; the capture.ps1 list at :422-424. Cross-scenario rejection: run.ps1:760-783. Recomputation: common.ps1:383, :467-471.
- Fault plumbing: run.ps1:22, :265-301; boot.rs:146-164, :176-215; init probe bound userspace/init/src/main.rs:1097, dispatch :1236. Expected user-fault arms: kernel/src/userspace.rs:2247-2279 in `validate_process` (:2090), with the `unexpected probe` panic at :2279. run.ps1 maps: `$scenario` :676-698, `$userVector` :733-741 (default 14), `$fatalKernelFault` :472. Exit codes: kernel/src/arch.rs:141-149, run.ps1:472-477; fatal diagnostic run.ps1:790-797.
- VM targets: ledger G3a (amended 2026-10-09: VMware only, QEMU for automated tests, VirtualBox dropped).
- Branches: `git for-each-ref` shows only `main`, `origin/main`, Claude worktree branches and `refs/codex/` checkpoints (re-checked 2026-10-09); Kimi's UI branch is not in the local clone, so AGENTS.md carries the confirmed placeholder. Docs-only commits go straight to `main`: ledger P7a. Same-commit AGENTS.md updates: ledger P9. Reference trees: linux, nt5src, freebsd-src and colibri (ledger P6, O1, O6); their paths vary by machine, so the text names none. readme.md:696 already says the references guide independently implemented Mirror interfaces.

Lines that later M0 specs make stale. Under P9, each spec updates them in the same commit as the change that makes them stale:
- 0011: the toolchain line names the 1.99.0 pin and the preflight; the unsafe rule says clippy enforces it in every crate; `test.ps1` clears its log directory at start.
- 0014: the one-boot line names `-DeviceProfile` instead of device switches.
- 0010: the flavor line drops "spec 0010 adds it"; `build.ps1 -Trace`; captures are trace builds; the user-content rule names `test-trace` and the `mirror.perf=` exception (ledger S7b); the suite line lists the `--features test-trace` clippy run, and "When to test" says "the three clippy commands" instead of "both clippy commands".
- 0015a: `build.ps1` also writes the disk image and VMDK and needs the MSVC linker and Windows SDK; the shared-`build\` line names the image and its SMBIOS config instead of the staged `limine.conf`; VM checks use `docs/vm-settings.md`. The test lines add host tests, fmt and clippy for the `tools\` workspaces, starting with `tools\mirror-image`, which `cargo fmt --all` and `clippy --workspace` do not reach. `package.ps1` joins the one-script-at-a-time rule.
- 0015b: `build.ps1` also writes the ISO; `run.ps1 -Iso`.
- 0013: the fail-closed convention line notes the firmware-table tolerance 0013 adopts: duplicate or surplus MADT records are dropped, and unreadable or unused root-table entries are skipped, instead of rejecting the table (0013-D3 to D6); an IOAPIC ID register that disagrees with its MADT record is reported with `MIRROR_APIC_IOAPIC_ID`, not rejected (0013-D9). Its cases follow the procedure above.
- 0016b: the one-boot line names `-BaselineCpu`; the host-side fault line says the default smoke CPU is `qemu64,+smep,+smap`.
- 0018: a "When to test" line for `tools\perf-probe`: a change there runs its fmt check, clippy and host tests through `--manifest-path`, because the root workspace does not reach it; `perf.ps1` joins the one-script-at-a-time rule in the commit that adds it.
- 0019: the `mirror.perf=` grammar and `-Perf`.
- 0017c: the "Desktop, console, input or display code" line also runs `capture.ps1 -NoBuild -RelativePointer`, and the suite line counts the relative capture. Its new cases (`-NoVmport`, `-Fault VmmouseError`) follow the procedure above, and `-NoVmport` joins any list of `run.ps1` device switches.
- 0016a, 0017a and 0017b add cases and fault names through the procedure above and are not expected to change AGENTS.md lines.

## Acceptance tests
1. `AGENTS.md` exists at the repository root and `(Get-Content AGENTS.md).Count` is at most 120. The text matches the version above, including the Kimi branch placeholder.
2. Every path it names exists: `Test-Path` succeeds for `scripts\{build,run,test,capture,common}.ps1`, `.cargo\config.toml`, `kernel\linker.ld`, `userspace\{init,desktop,agent,app}\linker.ld`, `kernel\src\{boot,userspace}.rs`, `userspace\init\src\main.rs`, `rust-toolchain.toml` and `docs\specs\{0000-north-star,decisions,README}.md`. The text names no reference-tree path (ledger P6). `Select-String -SimpleMatch` finds `-Smoke`, `-Release`, `-NoBuild`, `-Fault`, `-AgentDemo`, `-Xhci`, `$testMode`, `$scenario`, `$userVector`, `$fatalKernelFault` and `$markers` in `scripts\run.ps1`, `-NoBuild` and `-CpuCount` in `scripts\capture.ps1`, `fn validate_process` and `unexpected probe` in `kernel\src\userspace.rs`, and `QEMU_PATH`, `OVMF_CODE` and `OVMF_VARS` in `scripts\run.ps1` and `scripts\test.ps1`. The text names no VirtualBox setting or case.
3. The AGENTS.md References section contains none of the words license, legal, copyright, provenance, clean-room or leak.
4. readme edit: lines 694, 698 and 700 of `readme.md` equal the three replacement paragraphs above, `(Get-Content readme.md).Count` is unchanged (800 at HEAD 4614249), `Select-String readme.md -Pattern 'licensing permits','Porting source','port drivers'` finds nothing, and none of the three new paragraphs contains any of the words in test 3.
5. This spec changes only documentation, so under ledger P7a it is committed directly to `main`, after the commit that adds `docs/specs/`, and pushed, with no task branch. `git show --stat` for that commit lists only `AGENTS.md`, `CLAUDE.md` and `readme.md` (three lines changed: 694, 698 and 700).
6. `CLAUDE.md` contains exactly `@AGENTS.md`. In a fresh Claude Code session at the root, the question "Which script must build userspace?" gets the answer `scripts\build.ps1` with no file-read tool call in the transcript.
7. Per P8, the full `scripts\test.ps1` passes once the spec is complete. No code changes, so this also records the M0 baseline.
8. Manual cold-start check for each agent (Claude, GPT Astra, Kimi K3). In a fresh session that has read only AGENTS.md, each question gets the expected answer:
   - May you build from a `proposed` decision? No; stop and ask Cameron.
   - Why not run `cargo build --workspace`? Userspace would link with `kernel\linker.ld`.
   - May you run `run.ps1` while `test.ps1` runs? No; they share `build\`.
   - What edits does a new guest fault need? The boot.rs token; a `validate_process` arm for a user probe that faults; run.ps1's `-Fault` set and `$testMode`, plus `$scenario` and `$userVector` for a user probe (`$fatalKernelFault` for an expected fatal kernel fault); marker assertions; the test.ps1 case.
   - Which VM targets does Mirror have? VMware for daily use and development, QEMU for automated tests; VirtualBox is not a target.
   - Where do you commit, and who merges? On your own task branch; Cameron merges. Spec and docs-only commits go directly to `main`.
   - When do AGENTS.md lines change? In the same commit as the change that makes them stale.
   - Where is the linux reference tree? Ask Cameron; paths vary by machine.
   - What runs before each commit, and what before merge? The targeted cases for what it touches; the full `test.ps1`, plus `-AgentDemo` if agent-demo code changed.
   - May a non-Kimi agent rebase Kimi's branch? No.

## Dependencies
None.

## Open questions
- Partly resolved (2026-10-09): Kimi's UI branch name. It is not in the local clone, so AGENTS.md carries the confirmed placeholder `<name supplied by Cameron>`. The name itself is still awaited (decisions.md "Pending (resume here)").
- Does each agent's harness load AGENTS.md automatically? Codex does, and the repo has `refs/codex/` checkpoints, which suggests GPT Astra runs under it. Kimi K3's harness is unconfirmed. If it does not, its dispatch prompt must point to AGENTS.md.
- Resolved (2026-10-09): readme.md:700, which opens with "Porting source" and lists licensing in each driver's assessment, and readme.md:698, which says "port drivers" and "each port", are reworded here along with :694. The replacement wording of all three under Interfaces is confirmed.
- Resolved (2026-10-09): the AGENTS.md update rule is permanent, not M0-only, and applies per commit (ledger P9). The "Keep this file current" line says so.
- Follow-up specs:
  - (a) Remove the linker trap, for example by moving the kernel linker flags out of the workspace-wide target rustflags.
  - (b) Add a lock file on `build/` so that concurrent script runs fail fast.
  - (c) Clear the log directories when the suite starts. This is 0011-D7, not a new spec.
  - (d) Print a build identity at boot and have `run.ps1 -NoBuild` reject a mismatch. Build identity now belongs to spec 0067 (ledger G9); the `-NoBuild` check would build on it. 0011 suggests the same follow-up; write it once.
  - (e) Write `docs/testing.md` on the marker protocol (P5c).
