# 0011: Toolchain pin and workspace lints

Status: done (2026-10-10, branch `claude/0011-toolchain-and-lints`: c26d5c7 and 1cdd954). All acceptance tests pass on Rust 1.99.0. Test 7: `scripts/test.ps1` in 12m30s and `-AgentDemo` in 15m48s, 120 smoke boots and both captures each. Test 8: 276 and 268 fresh logs, none older than the run. The "Current state" section keeps citations from HEAD 4614249. (Pin amended to 1.99.0 on 2026-10-10, ledger P5a.)
Depends on: 0012 (lands first in the M0 order; this spec updates the AGENTS.md lines it changes). 0016b, which lands later, also edits `userspace/init/src/main.rs` (see Coordination).
Ledger: P5a (related: P3, P5b, P7, P8, P9, S12)

## Goal

Every build and test uses Rust 1.99.0. The scripts refuse a different compiler, and a toolchain change happens only through a deliberate bump commit. One `[workspace.lints]` table holds the unsafe-code lint policy, and all 12 workspace members inherit it. As a result, `mirror-init` and `mirror-sync` follow the same documented-unsafe rule as the kernel, and the existing violations in `mirror-init` are fixed. `scripts/test.ps1` starts each run with an empty log directory.

## Non-goals

- CI (deferred, ledger P3). Running host tests on non-Windows hosts (the MSVC triple is hard-coded at `scripts/test.ps1:28`).
- Lints beyond the two the kernel already denies, such as `clippy::pedantic` or `clippy::multiple_unsafe_ops_per_block`.
- Linting `#[cfg(test)]` code. Clippy runs without `--tests` (`scripts/test.ps1:30-32`).
- Pinning QEMU or OVMF versions, adding a build-identity banner, and fixing the `-Tkernel/linker.ld` rustflags trap in `.cargo/config.toml:4-8`.
- Testing an MSRV older than the pinned toolchain.
- The `rust-src` component and `build-std`. Spec 0028 (ledger S12) adds them later, on this same pinned compiler, to rebuild `core` and `alloc` for Mirror's userspace target. There is no second toolchain.

## Decisions

| ID | Decision | Status |
| --- | --- | --- |
| 0011-D1 | `rust-toolchain.toml` sets `channel = "1.99.0"`. The profile (minimal), the components (rustfmt, clippy) and the target (`x86_64-unknown-none`) stay as they are. (Amended 2026-10-10: 1.96.0 became 1.99.0 with ledger P5a.) | confirmed (ledger P5a; amended 2026-10-10) |
| 0011-D2 | Bumps (elaborating P5a's "bump it deliberately") happen when there is a reason: a needed language or library feature, a compiler fix, or a lint worth adopting. There is no fixed cadence. Each bump is its own commit and must pass both `scripts/test.ps1` and `scripts/test.ps1 -AgentDemo`. Rationale: lint churn stays out of feature commits, and each bump costs two full suite runs. Rejected alternative: follow every stable release on the six-week cycle. | confirmed (2026-10-09) |
| 0011-D3 | The workspace `rust-version` in `Cargo.toml` equals the pinned major.minor (`"1.99"`) and changes in the same commit as each bump. Rationale: a one-line edit that keeps Cargo's clear error on older compilers and keeps clippy's MSRV-gated lints in step with the compiler actually used. Rejected alternative: remove `rust-version` from the workspace and from all 12 manifests (each has `rust-version.workspace = true` at line 5). (Amended 2026-10-10: `"1.96"` became `"1.99"` with the pin.) | confirmed (2026-10-09; amended 2026-10-10) |
| 0011-D4 | `scripts/build.ps1` and `scripts/test.ps1` run a preflight that fails unless `rustc --version` reports the pinned channel and `rust-version` matches its major.minor. Rationale: a `RUSTUP_TOOLCHAIN` variable, a `rustup override set` directory override, or a non-rustup `rustc` on PATH each silently replace the toolchain file. Rejected alternative: rely on rustup alone. | confirmed (2026-10-09) |
| 0011-D5 | The root `[workspace.lints]` table denies `unsafe_op_in_unsafe_fn` (rustc) and `clippy::undocumented_unsafe_blocks`. All 12 members set `[lints] workspace = true` and have no package-level lint tables. Code never uses `allow` or `expect` on these two lints. Rejected alternative: keep per-crate tables and add the missing entries to init and sync. | confirmed (2026-10-09) |
| 0011-D6 | The six unsafe-free crates (acpi, clock-, desktop-, thread-, vm- and window-protocol) keep their guarantee as `#![forbid(unsafe_code)]` in `src/lib.rs`, because Cargo rejects package lint entries next to `workspace = true`. None of them has doctests or a `tests/` directory, so the crate attribute covers everything the manifest entry covered. Rejected alternative: leave those six crates out of workspace inheritance and keep their current `[lints.rust]` tables. | confirmed (2026-10-09) |
| 0011-D7 | `scripts/test.ps1` empties its log directory (`build/test-logs`, or `build/test-agent-logs` with `-AgentDemo`) before the first check runs. Chosen explicitly over creating a new timestamped directory for each run. | confirmed (2026-10-09) |

## Interfaces

**Current state (verified at HEAD 4614249; toolchain facts re-checked at 83d94b4 on 2026-10-10).**
- `rust-toolchain.toml:2` is `channel = "stable"`, and `scripts/test.ps1:17-19` runs clippy with `-D warnings`.
- `readme.md:13` says "declared minimum 1.88; latest validation uses 1.96.0". `Cargo.toml:8` declares `rust-version = "1.88"`, and nothing tests it.
- Two environments:
  - The environment that produced commits 3a84077 through 4614249 had one installed toolchain, `stable-x86_64-pc-windows-msvc`, which was `rustc 1.96.0 (ac68faa20 2026-05-25)`. "Latest validation uses 1.96.0" (`readme.md:13`) refers to it.
  - Cameron's development machine has stable `rustc 1.99.0` (since 2026-10-08). There, on 2026-10-09 with the code at 4614249, both clippy commands fail with `-D warnings` on the new lint `clippy::chunks_exact_to_as_chunks` at `kernel/src/usb.rs:538` (`bytes[6..].chunks_exact(3)`). Everything else passes: rustfmt, the 52 host tests, the 120 smoke boots and the debug and release captures.
  - On 2026-10-10 at 83d94b4, with `rust-version` temporarily set to `"1.99"`, both clippy commands, run with `--keep-going` over all 12 crates, still fail only at `usb.rs:538`. Raising the MSRV from 1.88 enables no other lint. On 1.99, `core::arch::x86_64::__cpuid` is a safe function, and `AtomicU64::fetch_update` is deprecated in favor of `try_update`.
  - The pin is 1.99.0 (ledger P5a, amended 2026-10-10): the compiler that already produced the green suite on the development machine, plus the `usb.rs:538` change below.
- There is no `[workspace.lints]` table and no `clippy.toml`, so clippy's default comment-placement rules apply.
- The lint tables differ by crate:
  - Both lints are denied in `kernel/Cargo.toml:27-31`, `userspace/agent/Cargo.toml:17-21`, `userspace/app/Cargo.toml:17-21` and `userspace/desktop/Cargo.toml:18-22`.
  - `crates/sync/Cargo.toml:11-12` denies only `unsafe_op_in_unsafe_fn`.
  - `userspace/init/Cargo.toml` has no `[lints]` table.
  - The six protocol and ACPI crates set `unsafe_code = "forbid"` at lines 8-9 of their manifests.
- No crate root has a lint attribute; each `lib.rs` starts with `#![no_std]` on line 1. No source file suppresses either lint.

**Files changed.**
- `rust-toolchain.toml:2`: set `channel = "1.99.0"`.
- `Cargo.toml:8`: set `rust-version = "1.99"`. Add the shared lint table:
  ```toml
  [workspace.lints.rust]
  unsafe_op_in_unsafe_fn = "deny"

  [workspace.lints.clippy]
  undocumented_unsafe_blocks = "deny"
  ```
- The 12 member manifests: replace the existing `[lints.*]` tables with `[lints]` / `workspace = true`. For `userspace/init/Cargo.toml`, add that table.
- The six unsafe-free crates: add `#![forbid(unsafe_code)]` after `#![no_std]` in `src/lib.rs`.
- `scripts/common.ps1`: add `Assert-MirrorToolchain [-RustcVersion <string>]`, next to the existing `Assert-Mirror*` helpers.
  - It reads the channel from `rust-toolchain.toml` with `(?m)^channel\s*=\s*"(\d+\.\d+\.\d+)"` and throws if that does not match (for example `"stable"`). It reads `rust-version` from `Cargo.toml`.
  - It runs `rustc --version` from the repository root unless `-RustcVersion` is supplied.
  - It throws unless the output starts with `rustc <channel> ` and `rust-version` equals the channel's major.minor.
  - The error message names the expected and actual versions and mentions `RUSTUP_TOOLCHAIN` and `rustup override`.
- `scripts/build.ps1` and `scripts/test.ps1` call the preflight first, right after `Push-Location` (`build.ps1:12`, `test.ps1:11`). Every cargo invocation in the scripts goes through one of these two files (`run.ps1:251` and `capture.ps1:15` call `build.ps1`). 0018's new `scripts/perf.ps1` calls the same preflight before it builds its host tool.
- `scripts/test.ps1:22-23`: move the log-directory setup to just after the preflight and ahead of `cargo fmt` (line 13). If the directory exists, remove it recursively, then recreate it.
- `kernel/src/usb.rs:538`: `bytes[6..].chunks_exact(3)` becomes `bytes[6..].as_chunks::<3>().0` (`clippy::chunks_exact_to_as_chunks`, new in 1.99; `as_chunks` is stable since 1.88). Both forms ignore a trailing partial chunk, so the loop body is unchanged.
- Remove the Rust 1.88 workarounds, since the MSRV becomes 1.99 and no comment may name the old minimum:
  - `__cpuid` is safe on 1.99. Drop the `#[allow(unused_unsafe)]` attributes, their 1.88 comments, and the `unsafe` blocks and `SAFETY` comments that exist only for the old signature at `kernel/src/fpu.rs:179-186`, `kernel/src/paging.rs:1678-1685`, `kernel/src/smp.rs:492-497` and `kernel/src/apic.rs:993-1000` (`cpuid_edx`, found during implementation). At `kernel/src/interrupts.rs:225` drop the attribute: the `unsafe` block in `tls_ready` stays for the CR4 read. As implemented, the two CPUID calls move out of that block, so it wraps only the CR4 read and its SAFETY comment covers only that; order and behavior are unchanged.
  - `kernel/src/capability.rs:161-163`: replace the deprecated `fetch_update` with `try_update` (same arguments and result) and drop `#[allow(deprecated)]` and the 1.88 comment.
- `readme.md:13`: replace the version clause with "Rust 1.99.0, pinned in `rust-toolchain.toml`".
- `AGENTS.md` (written by 0012, which lands first; updated in the same commit, P9): the toolchain line names the 1.99.0 pin and the `Assert-MirrorToolchain` preflight, the unsafe rule says clippy enforces it in every crate, dropping the "not on yet" clause for `mirror-init` and `mirror-sync`, and the test-log line says `test.ps1` clears its log directory at start (D7) instead of "never cleared".

**Violations the uniform policy surfaces.** These counts come from reading each `unsafe` site. Clippy was not run.
- `mirror-sync`: 0. All four sites have `SAFETY` comments (`crates/sync/src/lib.rs:108, 156, 254, 262`).
- `mirror-init`: 55 `unsafe` tokens against 28 `SAFETY` comments. Seven tokens are `#[unsafe(no_mangle)]` or `unsafe extern` blocks, which the lint does not check; the kernel already passes with undocumented ones at `kernel/src/interrupts.rs:209` and `:820`. That leaves 48 linted sites and an expected **20 undocumented blocks**:
  - `main.rs:127`, `:958` and `:1123`: these have comments, but the comments do not start with `SAFETY:`.
  - `main.rs:1238-1279`: 13 fault-probe match arms. Each arm needs its own `// SAFETY:` line, because the comment at `:1234-1235` sits above the `match`, not above the arms.
  - `threads.rs:317` and `:321`.
  - `tls.rs:163` and `:183`.
- A likely 21st: `clock.rs:107`. Its `SAFETY` comment (`:105`) sits above the enclosing `require(` call, not above the line holding the block. Move it inside the call, directly above `unsafe`, if clippy flags it.
- `unsafe_op_in_unsafe_fn`: 0 violations in init, which has no `unsafe fn` bodies.
- Fixes rewrite or add `// SAFETY:` comments that justify each block. Code does not change.
- Raising `rust-version` from 1.88 to 1.99 can also enable MSRV-gated clippy lints in any crate. Measured on 2026-10-10 (Current state), it enables none besides the `usb.rs:538` one; any that appears at build time is fixed in the same change.

**Coordination.** The M0 order (README) builds this spec right after 0012 and before 0014, 0010, 0016a and 0016b, so the `userspace/init` lines cited above still hold when it is built.
- 0016b (SMEP/SMAP/UMIP, EFER.SCE, CPU probes) edits `userspace/init/src/main.rs`: it makes the register-preservation probe (`:961-993`) set and check RFLAGS.AC, adds user probe arms 21 and 22 next to `:1309-1315`, and raises the probe bound at `:1100`. Because it lands after this spec, it writes those blocks under the workspace lint: the `:961` comment this spec rewrites keeps its `SAFETY:` prefix, and each new match arm gets its own `// SAFETY:` line.
- 0016a (guarded stacks, NMI/#MC stacks, stack-overflow probe) does not edit `userspace/init`. Its probes are kernel smoke modes, which the kernel lints already cover.
- 0010 does not edit `userspace/init`. It adds a `--features test-trace` clippy run, which the workspace lint table also covers.

**Bump procedure (0011-D2).**
1. Use one commit, titled "Bump Rust toolchain to X.Y.Z", made from a clean tree on its own branch (P7).
2. Change `channel` and `rust-version` (X.Y) together.
3. Run `cargo fmt --all`, then fix the clippy findings for both feature sets. Large fixes that also pass on the old toolchain land first, as separate commits.
4. Run `scripts/test.ps1` and `scripts/test.ps1 -AgentDemo` to completion. A compiler change touches every crate, agent-demo code included, so both full runs apply under P8. `kernel/src/main.rs:1345` prints core's `PanicInfo` Display, and `scripts/run.ps1:144` matches its two-line layout, so the `SmpTlbTimeout` cases (`scripts/test.ps1:99`) catch a format change.
5. Update `readme.md:13`, and any version that `AGENTS.md` names, with the new version.

## Acceptance tests

All tests run on Cameron's development machine, on the pinned 1.99.0 toolchain (test 1 shows it is active). Test 5 passes only with the `kernel/src/usb.rs:538` change.

1. **Pin.** From the repository root, `rustup show active-toolchain` prints `1.99.0-x86_64-pc-windows-msvc (overridden by '...\rust-toolchain.toml')`.
2. **Preflight (host only).**
   - From the repository root, `. scripts/common.ps1; Assert-MirrorToolchain` returns without error.
   - `Assert-MirrorToolchain -RustcVersion 'rustc 1.98.0 (0000000 2026-08-20)'` throws, and the message names 1.99.0.
   - With `Cargo.toml` temporarily set to `rust-version = "1.88"`, `Assert-MirrorToolchain` throws. Revert the edit.
3. **Inheritance (static checks).**
   - `Select-String -Path kernel/Cargo.toml,userspace/*/Cargo.toml,crates/*/Cargo.toml -Pattern '^\[lints\.'` returns nothing.
   - The same `Select-String` with `-Pattern '^\[lints\]$' -Context 0,1` returns 12 matches, each followed by `workspace = true`.
   - `Select-String -Path crates/*/src/lib.rs -Pattern '^#!\[forbid\(unsafe_code\)\]'` matches exactly the six unsafe-free crates (not sync).
4. **No suppressions.** `Get-ChildItem kernel,userspace,crates -Recurse -Filter *.rs | Select-String '(allow|expect)\((clippy::)?(undocumented_unsafe_blocks|unsafe_op_in_unsafe_fn)'` returns nothing.
5. **Clean lint.** `cargo clippy --locked --workspace -- -D warnings` and the `--all-features` variant pass (`scripts/test.ps1:30-33`).
6. **The policy reaches init, sync and the unsafe-free crates.** Make each change below by hand, confirm the failure, then revert it.
   - Delete the `// SAFETY:` line at `userspace/init/src/clock.rs:24`: clippy fails with `undocumented_unsafe_blocks` in `mirror-init`.
   - Delete the comment at `crates/sync/src/lib.rs:252-253`: clippy fails with `undocumented_unsafe_blocks` in `mirror-sync`.
   - Add an `unsafe {}` block to a function in `crates/vm-protocol/src/lib.rs`: `cargo check -p mirror-vm-protocol` fails with the `unsafe_code` lint at forbid level.
7. **Full suite.** `scripts/test.ps1` ends with its summary line, "All 120 kernel/userspace/device smoke cases and debug/release desktop/console captures passed." at HEAD 4614249 (`scripts/test.ps1:161`). The count is whatever the suite runs when this spec lands; 0014, which lands later, makes it computed. `scripts/test.ps1 -AgentDemo` ends with the same line.
8. **Log hygiene.**
   - Create `build/test-logs/stale.serial.log` and add a trailing space to any `.rs` line. `scripts/test.ps1` fails at the formatting check, and `stale.serial.log` is gone. Revert the edit.
   - After a full default pass, no file under `build/test-logs` has a `LastWriteTime` earlier than the run's start. The file count equals what the script copies (276 at the time of writing: 2 profiles x (60 cases x 2 logs + 18 capture files)).
   - The eight stale files present today are gone: `debug-small-*` (four files, Oct 8) and `{debug,release}-Xhci-ConnectedKeyboard.*` (four files, Oct 9).
9. **Docs.** `readme.md:13` names 1.99.0 as pinned and no longer mentions 1.88, and `Select-String -Path kernel/src/*.rs -Pattern '1\.88'` returns nothing. `AGENTS.md` names the 1.99.0 pin and the preflight, and no longer says the log directories are never cleared or that the unsafe lint is off in `mirror-init` and `mirror-sync`.

## Dependencies

No crates. Tool: the Rust 1.99.0 toolchain through rustup (rustc, cargo, rustfmt, clippy; MIT OR Apache-2.0). It is the stable compiler already on Cameron's development machine. rustup tracks a pinned version separately from `stable`, so the first build after the pin still downloads `1.99.0-x86_64-pc-windows-msvc` once (minimal profile plus the components and target from `rust-toolchain.toml`). This needs network access once. The `rust-src` component is not added here (0028, S12).

## Open questions

Follow-ups outside this spec's scope; each would be its own owner decision and none blocks this spec:
- Should a later spec lint test code (`clippy --tests`) on the host target? Today the `#[cfg(test)]` modules in the protocol, ACPI and sync crates are not linted. It is on the README's unscheduled follow-ups list.
- Should the workspace table grow later, for example with `clippy::multiple_unsafe_ops_per_block` or `clippy::unnecessary_safety_comment`?
- Suggested follow-up: record and check the QEMU and OVMF versions (on the README's unscheduled follow-ups list), and print a build-identity line (git hash, profile, features) that `run.ps1 -NoBuild` verifies. Build identity now belongs to spec 0067 (ledger G9); a `-NoBuild` check would build on it. 0012 suggests the same follow-up; write it once.
