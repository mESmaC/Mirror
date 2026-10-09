# 0010: Boot hygiene

Status: confirmed (2026-10-09)
Depends on: 0014 (the `V1` capture profile and its AHCI fixture), 0012 (AGENTS.md)
Ledger: S7, S7a, S7b

## Goal

A default kernel build (no Cargo features) goes from device bring-up straight to the shell, or to the desktop with `agent-demo`. It never runs init's self-test battery and never recognizes or asserts on test-fixture disk contents, so neither a test outcome nor disk contents can panic it. Its serial output never depends on user input, except for the opt-in `mirror.perf=` timing markers (S7b), which 0019 specifies. The `boot-smoke` suite keeps every existing check. A new non-default kernel feature, `test-trace`, carries the console-text oracles and per-input logs that `scripts/capture.ps1` and `scripts/capture-shell.ps1` synchronize on, so the captures keep passing on a trace build while the default build stays clean. A hygiene probe in QEMU and a binary scan prove the default build's properties.

## Non-goals

- Redesigning init, or adding the process model, boot bundle or service manager (0030, 0031, 0032). `init.elf` stays one of the four required boot modules (`kernel/src/boot.rs:77`), loaded but unused outside smoke builds. This spec does not edit `userspace/init`.
- Probe-then-teardown drivers, the xHCI 520-command workload (`main.rs:677-686`) and USB enumeration at boot. These belong to the runtime driver model (0060).
- Kernel-invariant asserts, such as DMA reclamation (`main.rs:733-737`, `877-881`, `969-973`). They check kernel state, not test outcomes or device data, and they stay.
- A typed test-event channel that replaces the string oracles (see Open questions).
- Removing log calls from userspace programs. They stay, and in default builds they reach nothing.
- The `mirror.perf=` markers, their boot-option parsing and their checks (0018-D9, 0019).
- Serial driver cost (polled 16550 with interrupts off, `kernel/src/serial.rs:51-55`), beyond the per-input lines this spec removes.

## Decisions

| ID | Decision | Status |
| --- | --- | --- |
| 0010-D1 | Init's test battery and production asserts on test fixtures move behind `boot-smoke`. Serial logging of user content and console-text test oracles move behind `boot-smoke` or `test-trace`. A normal (default) boot can never panic on a test outcome or leak typed text. | confirmed (ledger S7) |
| 0010-D2 | Gate mechanism: a new non-default kernel feature `test-trace`, with `boot-smoke = ["test-trace"]`, giving the three flavors of S7a. The init battery and fixture checks are `boot-smoke` only. User-content logs and console-text oracles are `test-trace`. Captures build with `test-trace` and without `boot-smoke`. A trace build runs the same code as a default build and adds only serial output and the bookkeeping that feeds it. Alternative (not chosen): build a typed test-event channel now, a syscall granted only in test builds, and replace every string oracle. Rationale: this is the smallest change that keeps both capture scripts working, and a typed channel would design syscall ABI ahead of 0020 and 0022. | confirmed (2026-10-09) |
| 0010-D3 | Default and trace builds run no init program. `run_init`, `run_pair` and `validate_process` compile only under `boot-smoke`, and the shell (or the `agent-demo` desktop) is the first userspace process. The kernel already starts it directly (`main.rs:326-347`). Alternatives (not chosen): a minimal init stub that exits 0, or keep the battery but make its failures non-fatal. Rationale: init starts no other program. Every smoke boot already runs the battery, and at boot its tick-bounded busy loops add to boot-to-desktop time (G7). 0032 defines the real first process. | confirmed (2026-10-09) |
| 0010-D4 | In default builds, serial output never depends on user input. User content means typed characters and key codes, document and text-field contents, pointer coordinates and buttons, and any bytes user processes write to the console. Console WRITE (syscall 1) keeps its checks (`userspace.rs:2917-2954`) and its return value, but it prints nothing and matches no strings. Content-free per-input markers also move to `test-trace`: console and pointer updates, clicks, and shell and desktop frames. Lifecycle and device markers stay. As a consequence, user programs have no serial diagnostics in default builds, though `MIRROR_USER_FAULT` still reports faults. Exception (ledger S7b): opt-in timing markers enabled by the `mirror.perf=` boot option (0018-D9) are allowed in default builds, including per-input ones. They carry no user content (no characters, key codes, coordinates or text); 0019 specifies them. Without a `mirror.perf=` token, nothing a default boot prints depends on user input. Alternative (not chosen): strip only the content fields, and keep content-free per-input markers and content-free console writes. Rationale: one kernel choke point covers every current and future app. Keystroke timing is sensitive too, so it appears only when a boot opts in, and each polled line runs with interrupts off (`serial.rs:51-55`), which works against the input-latency gate (G7). | confirmed (2026-10-09) |
| 0010-D5 | Default and trace builds keep the VirtIO block and AHCI boot probes, reduced to one sector-0 read with no comparison. Alternative (not chosen): skip block and AHCI probing outside smoke builds until 0060. Rationale: this keeps driver bring-up and the existing device markers in the captures, and removing probe-then-teardown is 0060's job. | confirmed (2026-10-09) |

## Interfaces

Line numbers are at HEAD 4614249. 0011 and 0014 land first (M0 order) and shift some script lines.

**Current state.**
- `kernel/src/main.rs:303` calls `userspace::run_init` on every boot. Non-smoke `run_init` calls `run_pair` once (`userspace.rs:371-379`).
- `run_pair` panics if init uses its 128-tick budget (`userspace.rs:36`, `666-678`). It then calls `validate_process` (`749-750`), which asserts exact counters (`2189-2197`) and panics on any outcome other than `Exit(0)` (`2289`). The non-smoke panic handler halts (`main.rs:1350-1351`).
- Any 2048-sector disk with `MIRROR\0\x01` at bytes 8..16 of sector 0 is treated as the fixture and checked with `assert!`/`assert_eq!` (`main.rs:888-892`, `995-1000`, `1046-1053`).
- Console WRITE echoes user text to serial (`userspace.rs:2955`) and string-matches it as a test oracle (`2956-2979`; `service_message` at `2508-2570`).
- The kernel console logs submitted command text (`console.rs:109-119`), clicks (`main.rs:544`) and pointer coordinates (`581-590`).
- Notes logs its document (`userspace/app/src/main.rs:361-363`). The Scratchpad logs submissions and pointer positions (`userspace/desktop/src/main.rs:363-368`), and the shell logs pointer state (`userspace/desktop/src/shell.rs:177-188`).
- After 0014, `capture.ps1` attaches the `V1` devices: the AHCI fixture `build/ahci-test.img` (a copy of `build/block-test.img`), no VirtIO fixture, and the boot medium on a firmware-only virtio-blk device (S1b). Its `:422` marker list holds `MIRROR_AHCI_READY`, `MIRROR_AHCI_READ_OK sectors=2048 reads=262 bounds_rejected=2`, `MIRROR_AHCI_CLEANUP_OK` and `MIRROR_BLOCK_SKIPPED`, and it hash-checks `ahci-test.img` after QEMU exits.

**Build flavors (S7a).** `agent-demo` combines with each one.

| Flavor | Kernel features | Used by |
| --- | --- | --- |
| default | none | `run.ps1` (normal), boot artifacts (0015a disk image, 0015b ISO), 0018 perf runs, daily use |
| trace | `test-trace` | `capture.ps1`, `run.ps1 -Trace` |
| smoke | `boot-smoke` (implies `test-trace`) | `run.ps1 -Smoke`, `test.ps1` |

**Kernel gating.** `kernel/Cargo.toml:23-25` gains `test-trace = []`, and `boot-smoke` becomes `["test-trace"]`.

| Item | Code | default | trace | smoke |
| --- | --- | --- | --- | --- |
| Init battery: `run_init`, `run_pair`, `validate_process` | `main.rs:289-310`; `userspace.rs:341-384`, `527-799`, `2090-2292` | absent | absent | runs |
| Fixture recognition and checks | `main.rs:888-924`, `995-1053` | absent | absent | runs |
| Init console-text oracles | `userspace.rs:2956-2978` | absent | absent | runs |
| `user:` echo of Console WRITE | `userspace.rs:2955` | absent | prints | prints |
| Shell, desktop and agent oracles and their markers | `userspace.rs:2979`, `2508-2570` | absent | runs | runs |
| `MIRROR_CONSOLE_COMMAND` and `MIRROR_CONSOLE_INPUT` | `console.rs:109-119` | absent | prints | n/a |
| `MIRROR_POINTER_CLICK`, `MIRROR_CONSOLE_UPDATED`, `MIRROR_POINTER_UPDATED` | `main.rs:544`, `573-578`, `581-590` | absent | prints | n/a |
| `MIRROR_TRACE_ON`, printed once in `_start` before PCI discovery | new | absent | prints | prints |

"n/a" means the code does not exist in smoke builds: `run_console` is `cfg(not(feature = "boot-smoke"))` (`main.rs:456`). 0019's `MIRROR_PERF_*` markers are outside this table: they follow 0018-D9 and D4's exception in every flavor.

- **Probes (D5):** open the device, read sector 0, compare nothing, shut down, and print the existing `MIRROR_BLOCK_PROBE_OK fixture=0 reads=1` or `MIRROR_AHCI_PROBE_OK fixture=0 reads=1` (`main.rs:982`, `867`). A read error prints the existing `MIRROR_*_FAILED phase=read` (`main.rs:983`, `868`), and boot continues.
- **Dead code:** items left unused in default or trace builds, such as init-only `Process` counters, `BootInfo::init` and the `inject_error` paths, are gated together with their users, so Clippy `-D warnings` passes in every flavor.
- **Unchanged in every flavor:**
  - device and catalog markers;
  - `MIRROR_CONSOLE_READY`, `MIRROR_POINTER_READY` (its position comes from geometry, not input) and `MIRROR_CONSOLE_INPUT_LOST` (`main.rs:486-499`, `525`);
  - `MIRROR_APP_*`, `MIRROR_SHELL_CLEANUP_OK` and `MIRROR_USER_FAULT`;
  - the `after_userspace` rendezvous and heap-growth phase (`main.rs:311-317`), so the capture's SMP and heap phase lists stay valid.

**Scripts.**
- `scripts/build.ps1:36-39`: a new `-Trace` switch adds `--features test-trace`.
- `scripts/run.ps1:251` passes `-Trace` through. The smoke assertions (`run.ps1:515-522`) also require exactly one `MIRROR_TRACE_ON`.
- `scripts/capture.ps1`:
  - `:15` builds with `-Trace`. The capture fails fast if `MIRROR_TRACE_ON` is missing.
  - The `:422` marker list drops `MIRROR_USERSPACE_OK scenario=0`, `MIRROR_USER_CLEANUP_OK`, `MIRROR_SCHEDULER_OK processes=2`, `MIRROR_IPC_OK messages=3`, `MIRROR_USER_SURFACE_OK` and `MIRROR_USER_SURFACE_CLEANUP_OK`, and requires them to be absent. `MIRROR_AHCI_READ_OK sectors=2048 reads=262 bounds_rejected=2` becomes `MIRROR_AHCI_PROBE_OK fixture=0 reads=1`. `MIRROR_AHCI_READY`, `MIRROR_AHCI_CLEANUP_OK`, `MIRROR_BLOCK_SKIPPED` and the fixture hash checks stay.
  - A new `-Hygiene` switch attaches the decoy disk as the AHCI fixture, in place of `build/ahci-test.img`, passes no `mirror.perf=` token and runs Acceptance test 3.
- `scripts/common.ps1`: the non-smoke `$userCount` becomes 0 in `Assert-MirrorClock` (`:320`) and `Assert-MirrorHeapGrowth` (`:528`). The heap bracket check (`:552-554`) runs only when `$userCount` is greater than 0.
- `scripts/test.ps1`:
  - Clippy (`:17-20`) adds `cargo clippy --locked -p mirror-kernel --features test-trace -- -D warnings`.
  - `:131` builds with `-Trace`.
  - After each capture, the suite builds the default flavor (with `-AgentDemo` if the suite uses it) and runs the binary scan. Without `-AgentDemo`, it also runs `capture.ps1 -NoBuild -Hygiene -CpuCount 4`. The final default build at `:149` stays.
- `readme.md` (capture section, lines 143-163) documents the three flavors.
- `AGENTS.md`: the user-content rule names `test-trace` instead of `boot-smoke` and the `mirror.perf=` exception (S7b), the build line lists `-Trace`, and captures are described as trace builds.

## Acceptance tests

1. **Smoke unchanged.** Every smoke boot in `scripts/test.ps1` passes in debug and release, with and without `-AgentDemo`, with its existing markers. This spec adds no smoke boots, so the count is the one the suite computes after 0014. The only new requirement is exactly one `MIRROR_TRACE_ON` per log.
2. **Trace capture.** `capture.ps1 -NoBuild -CpuCount 4` passes on trace builds in debug and release, default and `-AgentDemo`, with the Interfaces marker list. The log has zero `MIRROR_USERSPACE_OK`, `MIRROR_SCHEDULER_OK`, `MIRROR_IPC_OK`, `MIRROR_USER_CLOCK_OK` and `MIRROR_AHCI_READ_OK` lines. `build/ahci-test.img` stays byte-identical to the fixture.
3. **Hygiene probe.** `capture.ps1 -NoBuild -Hygiene` runs on a default build without `agent-demo`, in debug and release, with 4 CPUs and no `mirror.perf=` token, so D4's exception is off. The AHCI fixture is replaced by a decoy: a copy of `build/block-test.img` with the byte at file offset 612 (sector 1, byte 100) inverted. Today's kernel accepts the decoy's signature (`main.rs:889`) and panics at `main.rs:1051` (`check_fixture_sector`, called from `check_ahci_data`). "Stable screen" below means two screendumps 1 s apart are identical, with a 10 s timeout. The probe:
   1. Waits for `MIRROR_BOOT_OK`. It requires `MIRROR_AHCI_PROBE_OK fixture=0 reads=1`, and it requires zero `MIRROR_PANIC`, `MIRROR_TRACE_ON` and `MIRROR_USERSPACE_OK`.
   2. Requires the first screendump to be 1280x800 (PNG IHDR), the size `capture-shell.ps1:122-124` assumes. Each attempt moves the pointer by (-2560,-1600) to clamp it at (0,0), then by (64,776) to the Notes launcher (`capture-shell.ps1:74-76`), and clicks. Attempts repeat every 3 s, at most 10 times, until `MIRROR_APP_STARTED pid=N kind=0` appears. A second `MIRROR_APP_STARTED` fails the probe.
   3. Waits for a stable screen S0 and records the serial length L0. It types the sentinel `zqxjvw` and requires a stable screen S1 that differs from S0. It then moves the pointer by (100,-100) and requires a stable screen S2 that differs from S1. It requires no serial bytes after L0.
   4. Presses Esc and waits for `MIRROR_SHELL_CLEANUP_OK` and `MIRROR_CONSOLE_READY`. It repeats step 3 on the kernel console, typing `echo zqxjvw` and Return, and requires no serial bytes after the new L1.
   5. Checks the whole log. It contains no `zqxjvw`, and zero lines match `^user: `, `^MIRROR_CONSOLE_(COMMAND|UPDATED|INPUT irq)`, `^MIRROR_POINTER_(UPDATED|CLICK)`, `^MIRROR_(SHELL|DESKTOP)_(READY|FRAME)` or `Mirror (init|app|shell|desktop|agent):`.
   6. D4's exception is checked by 0019, which reruns this probe with `mirror.perf=1`. There, the serial windows that steps 3 and 4 require to be empty may hold only lines that start with `MIRROR_PERF_` and whose fields are all `name=<decimal>` or `name=none`, and step 5 still holds.
4. **Binary scan.** `test.ps1` runs a byte search on `build/esp/kernel.elf`, debug and release.
   - The default kernel contains none of `MIRROR_TRACE_ON`, `MIRROR_CONSOLE_COMMAND operation=`, `MIRROR_POINTER_UPDATED`, `MIRROR_POINTER_CLICK`, `MIRROR_SHELL_FRAME`, `Mirror shell: ready`, `MIRROR_USERSPACE_OK`, `Mirror init: VM checks passed`, `fixture data mismatch` or `unexpected budget exhaustion`. `MIRROR_PERF_` strings (0019) are allowed under D4's exception.
   - The trace kernel contains `MIRROR_TRACE_ON` and `MIRROR_SHELL_FRAME`, and contains neither `MIRROR_USERSPACE_OK` nor `fixture data mismatch`.
5. **Clippy and host tests.** Clippy passes with `-D warnings` for the default, `--features test-trace` and `--all-features` configurations. No host tests are added, because the kernel crate has `test = false` (`kernel/Cargo.toml:11`). The existing host tests (`test.ps1:15`) still pass.

## Dependencies

None.

## Open questions

Follow-up spec suggestions, outside this spec's scope:
- 0015a and 0015b package default builds only, and may add an on-screen trace-build marker like A10's.
- A typed test-event channel that replaces the string oracles (`userspace.rs:2508-2570`) and the app and shell log lines, after 0020 and 0022.
- A destination for user-program diagnostics and `mirror-rt` panic output (S8, 0026) in default builds, since D4 silences Console WRITE.
- Removing probe-then-teardown and the xHCI workload from default boot, under 0060.
- Decoupling the hygiene probe and captures from fixed pixel coordinates (`capture-shell.ps1:74`, `:122-124`).
- A manual VMware serial-file check of the default build once 0015a's disk image exists.
