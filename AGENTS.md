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
- Start the branch from the current `main` and name it `<agent>/<spec>-<short-name>`, where `<agent>` is `claude`, `astra` or `kimi`, for example `claude/0011-toolchain-and-lints`. A task without a spec uses `<agent>/<short-name>`.
- Kimi's UI branch, `design/design-system` (in Kimi's own clone; its designs are in `docs/design/`), belongs to Kimi. Other agents never check it out, rebase it, merge it, cherry-pick from it, push to it or delete it.

## Build
Run commands from the repository root in PowerShell.
- `.\scripts\build.ps1 [-Release] [-Trace] [-Smoke] [-AgentDemo]` builds the four userspace programs and the kernel and stages `build\esp\`. `-Trace`, `-Smoke` and `-AgentDemo` enable the kernel features `test-trace`, `boot-smoke` and `agent-demo`.
- The ledger (S7a) defines three kernel flavors: default (clean, for daily use), `test-trace` (adds test serial markers, for captures) and `boot-smoke` (`test-trace` plus the self-test and fault battery). Only `boot-smoke` runs init's test battery and the test-fixture checks; default and trace kernels start the shell directly.
- Trap: `.cargo\config.toml` links every `x86_64-unknown-none` crate with `kernel\linker.ld`. `build.ps1` overrides this per program through `CARGO_ENCODED_RUSTFLAGS`: the program's own `userspace\<pkg>\linker.ld`, the small code model, and release LTO off. Never build userspace with `cargo build --workspace` or `cargo build -p mirror-<pkg>`: the images come out wrongly linked. `cargo check` and `cargo clippy` are safe because they do not link.
- Use Rust 1.99.0, pinned in `rust-toolchain.toml`. `build.ps1` and `test.ps1` first run the `Assert-MirrorToolchain` preflight (`scripts\common.ps1`). It fails unless `rustc --version` reports the pinned version and the workspace `rust-version` in `Cargo.toml` is its major.minor, because a `RUSTUP_TOOLCHAIN` variable, a `rustup override` or another `rustc` on `PATH` silently replaces the pin. Change the pin only in a dedicated bump commit that changes both values, following the bump procedure in spec 0011.

## Test
- `.\scripts\test.ps1` is the full suite: rustfmt `--check`, host unit tests, and clippy `-D warnings` with no features, with `--features test-trace` and with all features. Then, for debug and for release: a smoke build and every smoke boot; a trace build and its normal-boot capture; a default build; a binary scan of the trace and default `kernel.elf` for strings each flavor must or must not contain; and, without `-AgentDemo`, the hygiene capture. It takes about 20 minutes and stops at the first failure. `test.ps1 -AgentDemo` reruns the whole suite with `agent-demo`, about 20 minutes more.
- Host tests need an explicit target because the default target is bare metal: `cargo test --locked -p <crate> --target x86_64-pc-windows-msvc`. This needs the MSVC build tools. Only the `crates\` packages have host tests.
- One boot: `.\scripts\run.ps1 -Smoke [-Release] [-DeviceProfile <V1|LegacyVirtio>] [-Fault <Name>]`. `V1`, the default, attaches the v1 devices: the AHCI test disk, an e1000 NIC, PS/2 and VGA. `LegacyVirtio` attaches the legacy VirtIO test disk instead, and the `Block*` faults need it. Without `-Smoke` it boots interactively and never exits on its own, so always pass `-Smoke`. Pass `-NoBuild` only if the staged kernel already matches the requested profile and features. Nothing checks this.
- `.\scripts\capture.ps1` builds a trace kernel (`build.ps1 -Trace`), boots it, drives it with QMP input and checks screenshots. With `-NoBuild` the staged kernel must be a trace build; without `MIRROR_TRACE_ON` the capture fails fast. `capture.ps1 -NoBuild -Hygiene` instead probes a staged default kernel without `agent-demo`: no trace or init markers, no fixture checks, and no serial output after typing or pointer moves.
- Run the build, run, capture and test scripts one at a time, across all agents and terminals. They share `build\` (the staged ESP and its `limine.conf`, firmware variables, serial logs), so a second command silently corrupts the first.
- QEMU and OVMF are found in `.tools\qemu\` or on `PATH`, or set through `QEMU_PATH`, `OVMF_CODE` and `OVMF_VARS`.
- A failed boot leaves its output in `build\serial.log` and `build\qemu.stderr.log`. `test.ps1` clears its log directory, `build\test-logs\` (or `build\test-agent-logs\` with `-AgentDemo`), when it starts, then copies in only passing cases, so after a failed run it holds just the cases that passed before the failure.

### When to test
- Before each commit, run the targeted cases for what it touches. `test.ps1` lists every case; copy the matching commands from it:
  - Any Rust change: `cargo fmt --all -- --check` and the three clippy commands. A change under `crates\` also runs that crate's host tests.
  - Kernel or userspace code: `build.ps1 -Smoke`, then `run.ps1 -Smoke -NoBuild` with no `-Fault`, plus every case whose switches or `-Fault` name exercise the touched code (for example the `-Xhci` cases for xHCI code, or the `LegacyVirtio` cases for VirtIO block code). To find them, search `run.ps1` and `common.ps1` for the markers that code prints. Add `-AgentDemo` to the build and runs if you touched agent-demo code.
  - Desktop, console, input or display code: also `build.ps1 -Trace`, then `capture.ps1 -NoBuild -CpuCount 4`; then `build.ps1`, then `capture.ps1 -NoBuild -Hygiene -CpuCount 4`.
  - A script change: the cases that execute the changed lines.
  - Docs only: nothing to run.
- When the spec is complete, run the full `test.ps1` before Cameron merges the branch, plus `test.ps1 -AgentDemo` if you touched agent-demo code.

## Coding conventions
- Every `unsafe` block has a `// SAFETY:` comment that says why its preconditions hold, and every `unsafe fn` documents its caller contract, normally under `# Safety`. Clippy enforces the comments in every crate: the root `Cargo.toml` denies `unsafe_op_in_unsafe_fn` and `clippy::undocumented_unsafe_blocks` in `[workspace.lints]`, and every member inherits that table. Never `allow` or `expect` either lint. The protocol crates, `mirror-acpi` and `mirror-ui-tokens` forbid `unsafe` with `#![forbid(unsafe_code)]`.
- Fail closed. Fully validate untrusted input before acting on it: ELF images, firmware tables, device descriptors, syscall arguments and the boot command line. Reject anything unknown or malformed, and bound every loop and wait.
- Roll back on failure. An operation that fails partway undoes its partial effects before it returns the error. A smoke test proves this, usually by comparing allocated frames before and after.
- DMA buffers have no `Drop`. Release them only after the device is reset or quiesced, and quarantine them if that fails. Report cleanup in markers such as `dma_reclaimed=1`.
- Never add serial output that carries user content (typed text, key codes, file or document data, pointer state), or a per-input marker, to a default build. Gate it behind `test-trace`, which `boot-smoke` implies. The one exception (ledger S7b) is the opt-in timing markers enabled by a valid `mirror.perf=` boot token (spec 0019); they carry no user content. Content-free lifecycle and device markers may print after user actions.
- Put test batteries and checks on test-fixture data behind `boot-smoke`, so a default build never panics on a test outcome or on disk contents.

## Serial markers
- The kernel reports results as single serial lines of the form `MIRROR_<AREA>_<EVENT> key=value ...`. In trace and smoke builds, userspace console output is escaped and prefixed with `user: `, so userspace cannot forge a marker; default builds print none of it.
- The marker-family checks in `scripts\common.ps1` (driver catalog, xHCI, USB, AHCI, clock) are closed-world:
  - an unknown name in a checked family fails the run;
  - each expected marker appears exactly once, with its full field list, in the checked order.
- The generic marker lists in `scripts\run.ps1` (`$markers`) and `scripts\capture.ps1` check presence only (capture also requires the init battery's markers to be absent). They miss duplicates, extra fields and order.
- `run.ps1` rejects a scenario-specific marker in every other scenario.
- So a new or changed marker needs the matching assertion change in the same commit. Values derived from other fields are recomputed in the script, never trusted as reported.

## Fault injection
Guest-side faults exist only in `boot-smoke` kernels. To add one:
1. Add a `mirror.test=<kebab-name>` token to `smoke_mode()` in `kernel\src\boot.rs`. Map it to a new `SmokeMode` variant, or to `SmokeMode::User(n)` for a probe that runs in `userspace\init` (raise init's `probe` bound in `userspace\init\src\main.rs`). Unknown or repeated tokens must still panic.
2. For a `User(n)` probe that faults, add its expected `(vector, error, cr2)` arm to `validate_process` in `kernel\src\userspace.rs`. Without it the boot panics with `unexpected probe`.
3. Add the PascalCase name to `-Fault` in `scripts\run.ps1`: both its `ValidateSet` and the `$testMode` switch. A `User(n)` probe also gets its number in the `$scenario` map and, unless it expects vector 14, an entry in the `$userVector` map. An expected fatal kernel fault joins `$fatalKernelFault`.
4. Assert the scenario's marker in that case, and reject the marker in every other case.
5. Add the case to `scripts\test.ps1`, which runs it in both build profiles, debug and release.
A passing smoke boot exits QEMU with code 33. An expected fatal fault exits with 35 and an exact diagnostic. Host-side faults are QEMU options in `run.ps1`: blkdebug errors, removed devices, CPU models and CPU counts.

## References
The local reference trees (linux, nt5src, freebsd-src and colibri) are best-practice references only. Read them to learn how others solved a problem. Mirror code is written independently, against Mirror's own interfaces. The trees live on the development machine and their paths vary by machine, so ask Cameron for a tree's location instead of assuming one.

## Docs
- `readme.md` is being split into `docs\` gradually. Put new long-form material under `docs\`, and keep edits to `readme.md` short.
- Keep this file current: a change to a command, path or convention described here updates the matching lines in the same commit. This rule is permanent (ledger P9).
