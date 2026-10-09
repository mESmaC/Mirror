# 0017b: PS/2 init timing and order

Status: confirmed (2026-10-09)
Depends on: 0017a (M0 order only; both edit `kernel/src/ps2.rs`). The manual VMware/VirtualBox check also needs 0013 and 0015a's disk image; the QEMU tests need neither.
Ledger: S1a, G1, G3a (related: G7, S7a)

## Goal
PS/2 init waits are bounded by elapsed time (HPET, else PIT) using the Linux budgets, instead of by a count of status reads whose duration depends on how fast the platform answers port I/O. Init no longer runs the controller self-test, and a scancode buffered at boot can no longer disable the keyboard. The keyboard and mouse come up on every cold boot in VMware and VirtualBox (stock VirtualBox VMs have HPET disabled), and a silent device costs a bounded delay that serial reports. This keeps input usable for daily use (G1) on the v1 device set (S1a) and bounds the init cost inside boot-to-desktop (G7). Loss handling is 0017a.

## Non-goals
- Per-device loss, mouse coalescing and framing resync (0017a).
- A translated Set 1 keyboard fallback: deferred to the post-v1 hardware work (D9, G3).
- Keyboard LEDs (0xED), at init or after a loss: spec 0084.
- Typematic, hotplug, suspend/resume, ACPI PNP0303/IAPC_BOOT_ARCH gating, physical hardware (G3).

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| 0017-D5 | Init waits are bounded by elapsed time: HPET (`clock::snapshot`) when it is published, else polled PIT channel 0. An iteration backstop is kept. Budgets follow Linux: controller IBF/OBF 500 ms (linux `drivers/input/serio/i8042.c:230-249` with `i8042.h:34`), device ACK 200 ms, reset ACK 1000 ms, response 500 ms, reset BAT 4000 ms (`libps2.c:320, 338`). Rationale: init runs with IF clear (ps2.rs:218), so ticks do not advance, and stock VirtualBox VMs have HPET disabled. Rejected alternative: keep iteration counts and raise them about 40x. | confirmed (2026-10-09) |
| 0017-D8 | Init skips the controller self-test (0xAA) and keeps the port tests (0xAB, 0xA9). Rationale: Linux on x86 runs it at probe only when `i8042.reset` is forced (`i8042.c:57, 1541`), and in Mirror it can only abort init or reset the config. Rejected alternative: keep it. | confirmed (2026-10-09) |
| 0017-D9 | Keep untranslated Set 2 with the F0 00 readback. A failed readback still skips the keyboard with a reason. A translated Set 1 fallback is deferred to the post-v1 hardware work (G3). Rationale: VMs are the v1 target, and a Set 1 path needs a second decode table. If test 6 shows VMware or VirtualBox failing the readback, the rejected alternative becomes required. Rejected alternative: add the translated Set 1 fallback now. | confirmed (2026-10-09) |

Decisions in 0017a that also govern this spec:
- 0017-D1: v1 keyboard and pointer input is the PS/2 keyboard and mouse behind the i8042 (ledger S1a).
- 0017-D2: VMware (primary) and VirtualBox are checked by hand; QEMU runs the automated tests (ledger G3a).

## Interfaces
**Init timing** (`kernel/src/timer.rs`, `ps2.rs`)
- New `timer::pit_counter() -> u16` latches and reads PIT channel 0. timer.rs:85-88 programs that channel in mode 2 at about 100 Hz, whichever tick source is selected. The call requires IF clear on the BSP. New pure `timer::pit_elapsed(previous, current) -> u32` returns `(previous - current) mod PIT_DIVISOR`. A private `ps2::Deadline` sums it and must sample more often than every 10 ms. A missed wrap, for example while the VM is descheduled, can only lengthen a wait.
- `wait_input_empty` and `read_response` (ps2.rs:374-386, 425-441) take a deadline. In `device_command` (ps2.rs:478-503), each attempt gets one ACK deadline, which its stale-byte discard shares. `POLL_LIMIT` (ps2.rs:25) becomes the backstop at 80,000,000 or more status reads, so it cannot fire before 4 s at 50 ns per read.
- Cost: a device that never answers costs one 200 ms ACK wait, so init with both devices silent takes less than 1 s longer. A mouse that ACKs reset but never completes BAT adds 4 s.

**Markers** (lifecycle and device markers, no user content)
- `MIRROR_PS2_INIT elapsed_ms=<n> clock=hpet|pit selftest=0` prints after init in every build flavor.
- On a timeout, `MIRROR_PS2_TIMEOUT wait=controller|ack|response|bat elapsed_ms=<n> clock=hpet|pit` prints before the existing SKIPPED line (main.rs:253-279). Here elapsed_ms is that wait's own duration.

**Init order** (ps2.rs:253-335)
- D8: remove 0xAA and the block that reapplies the config after it (ps2.rs:260-268). The initial drain (ps2.rs:256) and the quiet config write (ps2.rs:259) stay.
- Keyboard-init fix: `prepare_keyboard` order becomes 0xAE, write_config, F5, verify_config, then the F0 02 / F0 00 readback. Today verify_config runs before F5 (ps2.rs:322-326), while the keyboard may already be scanning. The F5 ACK wait discards up to 32 buffered scan bytes. This closes the window that the comment at ps2.rs:283-285 describes.

**Smoke faults** (`kernel/src/boot.rs` `SmokeMode` and `smoke_mode`, `scripts/run.ps1 -Fault`; boot-smoke flavor only, S7a)
- `mirror.test=ps2-bat-timeout` (`Ps2BatTimeout`): during the mouse BAT wait only, mouse-source bytes are read and dropped.
- `mirror.test=ps2-stale-scancode` (`Ps2StaleScancode`): the first keyboard-source read after 0xAE returns an injected 0x1c before any real controller data, and the kernel prints `MIRROR_PS2_INJECTED stale=1`.

**Files touched:** `kernel/src/{ps2,timer,boot,main}.rs`, `scripts/run.ps1`, `scripts/test.ps1`, `readme.md:315` (init waits are bounded by elapsed time).

## Acceptance tests
1. `timer::policy_self_test` (boot-smoke, MIRROR_TIMER_POLICY_OK): `pit_elapsed` cases with no wrap, one wrap, equal samples, and the divisor boundary (previous 1, current PIT_DIVISOR gives 1).

QEMU integration (`scripts/run.ps1`, `scripts/test.ps1`):
2. Every smoke boot that has an i8042, except the `Ps2BatTimeout` cases, asserts `MIRROR_PS2_INIT ... selftest=0` with elapsed_ms < 250 and no `MIRROR_PS2_TIMEOUT`. The `-NoKeyboard` case still ends in both SKIPPED markers and prints no timeout.
3. `-Fault Ps2BatTimeout`, run once by default (clock=hpet) and once with `-NoHpet` (clock=pit). Expected: MIRROR_KEYBOARD_READY, `MIRROR_MOUSE_SKIPPED reason=OutputTimeout`, and `MIRROR_PS2_TIMEOUT wait=bat` with 4000 <= elapsed_ms <= 4500. run.ps1:618-620 expects MOUSE_SKIPPED instead of MOUSE_READY only for this fault.
4. `-Fault Ps2StaleScancode`: MIRROR_KEYBOARD_READY, MIRROR_MOUSE_READY and `MIRROR_PS2_INJECTED stale=1`. The implementer lands the fault before the reorder and records that it then gives `MIRROR_KEYBOARD_SKIPPED reason=Configuration`.
5. `scripts/test.ps1` runs cases 3 (both clocks) and 4 in each profile, which adds 3 smoke boots per profile. Before merge (ledger P8), the full `scripts/test.ps1` passes.

Manual check on VMware Workstation and VirtualBox. Each VM boots 0015a's disk image with the reference settings in `docs/vm-settings.md` (0015-D11), which turn VirtualBox's HPET on, and uses a PS/2 keyboard and a PS/2 mouse with no USB mouse or tablet (VirtualBox: `VBoxManage modifyvm <vm> --mouse ps2 --keyboard ps2`):
6. Over 10 cold boots per hypervisor, the serial log shows `MIRROR_KEYBOARD_READY ... set=2`, MIRROR_MOUSE_READY, no MIRROR_PS2_TIMEOUT, and `MIRROR_PS2_INIT elapsed_ms` < 1000 with clock=hpet. VirtualBox then repeats the 10 boots with `VBoxManage modifyvm <vm> --hpet off`, its default, as 0013 test 15 does, and shows the same with clock=pit. Record every value.

## Dependencies
No crates. QEMU (`.tools/qemu`, GPL-2.0) is already used. The manual check needs VMware Workstation (proprietary, no cost) and the VirtualBox 7.x base package (GPL-3.0). Neither is installed at its default path on the current development host.

## Open questions
1. External fact, checked by test 6: whether VMware and VirtualBox pass the F0 00 readback. If either fails, D9's rejected alternative (the translated Set 1 fallback) becomes required. That changes a confirmed decision, so it goes back to Cameron first (README rule 5), and the Set 1 work then joins this spec.
