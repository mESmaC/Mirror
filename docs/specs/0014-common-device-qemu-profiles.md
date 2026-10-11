# 0014: Common device-set QEMU profiles

Status: done (2026-10-10, branch `claude/0014-device-profiles`: d04e9fc). All acceptance tests pass. Both suites report 118 smoke boots plus the captures. The six `LegacyVirtio` cases match the pre-change `MIRROR_BLOCK_` lines in both builds. No V1 stderr log has "has no peer". `scripts/test.ps1` took 11m48s, against 12m30s before the change. The Interfaces section keeps its pre-change citations.
Depends on: 0012 and 0011, which land first in the M0 order (this spec updates the AGENTS.md lines it changes). 0010, 0015a, 0015b, 0013, 0016a/b, 0017a/b, 0017c and 0019 land later and build on these profiles, as does 0018's QEMU lane.
Ledger: S1, S1a, S1b, G3a (related: S1c, S1d, S5, P3, P8)

## Goal

Every automated QEMU boot uses a named device profile. The profiles are defined once in `scripts/common.ps1` and shared by `run.ps1` and `capture.ps1`. The default profile, `V1`, presents these v1 devices (0014-D1):
- the q35 built-in ICH9 AHCI controller, with the 1 MiB test fixture as a SATA disk;
- an Intel 82545EM e1000 NIC;
- the i8042 PS/2 keyboard and mouse;
- the standard VGA device, whose UEFI GOP framebuffer Limine passes to the kernel.

Smoke cases, manual `run.ps1` boots and the debug and release normal-kernel captures all use `V1`. The legacy VirtIO block fixture remains only in a six-case `LegacyVirtio` profile, which 0015b's `-Iso` case also uses. The boot medium stays on a virtio-blk device that only firmware reads (S1b). Every smoke boot and every capture asserts that PCI discovery found exactly one 82545EM. Suite wall time stays within 5% of today's, which is about 20 minutes.

## Non-goals

- The e1000 driver, link bring-up and packet I/O. Those belong to spec 0070 (M6). This spec only attaches the NIC and asserts that PCI discovery finds it.
- Changing the boot medium (0014-D7, S1b). The disk image and the QEMU boot switch belong to 0015a; the live ISO and its `-Iso` case belong to 0015b.
- An AHCI write path (0062) and a runtime driver model (0060).
- PCI and firmware fixes for VMware (0013), and PS/2 robustness (0017a, 0017b).
- Changing the display adapter, and any SVGA II work. Since S1a's 2026-10-09 amendment the VMware SVGA II adapter is in the v1 device set, and its driver is 0069 (S1c). These profiles keep `-vga std` (see Open questions).
- Parallel test execution, CI (deferred, ledger P3) and a harness rewrite. Clearing stale, renamed logs from `build/test-logs` is 0011-D7.
- xHCI/USB cases. They stay as variants on top of `V1`, because USB input is not in the v1 device set.

## Decisions

| ID | Decision | Status |
| --- | --- | --- |
| 0014-D1 | The QEMU test profiles use the v1 device set: AHCI SATA storage, an Intel e1000-family NIC, PS/2 keyboard and mouse, and the UEFI boot framebuffer. (Amended 2026-10-09: S1a now also lists the VMware SVGA II adapter (S1c, spec 0069), the vmmouse absolute-pointer extension (S1d, spec 0017c) and the VMware guest RPC channel (A20, S20, spec 0087), and its last sentence reads: "The QEMU test profiles switch to this set where QEMU emulates it: `-vga std` stays until 0069 decides on SVGA II, and the guest RPC channel (0087) has no QEMU model and is checked by hand on VMware." These profiles keep `-vga std`. q35's default `vmport`, which `run.ps1:320` turns off only for `-NoKeyboard`, already presents QEMU's vmmouse.) | confirmed (ledger S1a) |
| 0014-D2 | VirtIO is not a v1 target, so the default profile attaches no VirtIO test fixture. | confirmed (ledger S1, S1a) |
| 0014-D3 | VMware is the only VM target before physical hardware, and the primary daily-drive and development setup. QEMU is the automated test platform. VirtualBox is not a target. (Amended 2026-10-09: VirtualBox dropped, G3a amended.) | confirmed (ledger G3a) |
| 0014-D4 | The standard NIC is the Intel 82545EM (8086:100F), attached in QEMU as `-device e1000-82545em`. Rationale: it is the model VMware presents as its default `e1000` adapter (VMware's `e1000e` is the 82574L), and QEMU emulates it as `e1000-82545em`. Main alternative: QEMU's plain `e1000`, which is the 82540EM (8086:100E); VMware does not offer it. (Amended 2026-10-09: VirtualBox removed from the rationale, G3a amended; the rationale now names VMware's default adapter and its `e1000e` alternative.) | confirmed (2026-10-09) |
| 0014-D5 | Keep the legacy VirtIO block cases as a `LegacyVirtio` profile with 6 cases per build profile, until 0062 adds AHCI write and flush cases. Rationale: AHCI has no write path (`kernel/src/main.rs:796`), so these cases are the suite's only write, flush, write-error and dirty-shutdown checks (`run.ps1:641`). Alternative: retire them now, which saves about 100 s per suite run and leaves that gap open until 0062. | confirmed (2026-10-09) |
| 0014-D6 | Assert e1000 detection on the host, using the existing unmatched catalog record (`driver=none match=none`). `kernel/src/drivers.rs` gets no e1000 candidate until the 0070 driver exists. Rationale: a candidate means that an implementation recognizes the device (`kernel/src/drivers.rs:3-6`). Alternative: add an `e1000` candidate now, so that the catalog and the `devices` console page name the NIC. | confirmed (2026-10-09) |
| 0014-D7 | The boot medium stays as it is until 0015a replaces it: the host ESP over read-only vvfat on a modern-only virtio-blk device. Mirror never drives that device; only the firmware reads it, which ledger S1b records as consistent with S1/S1a. Rationale: 0015a's GPT image (ledger S5) replaces this medium anyway, and the smoke kernel asserts that the VirtIO boot disk exists (`kernel/src/main.rs:202-205`). Moving the ESP to AHCI now would need a writable overlay for QEMU's ATA model (`run.ps1:353-354`), and it would add a second ATA disk to the driver's port selection (`kernel/src/ahci.rs:238-242`). Alternative: block 0014 until 0015a lands. | confirmed (2026-10-09) |

## Interfaces

**Current state (verified at HEAD 4614249).**
- `run.ps1:318-335` and `capture.ps1:271-285` each build their own QEMU command line. Both use `q35,accel=tcg`, `-vga std` and `-nic none`, and expose the ESP as read-only vvfat on `virtio-blk-pci,...,disable-legacy=on,bootindex=0`.
- The default fixture is the legacy VirtIO disk (`run.ps1:360-361`, `capture.ps1:282-285`).
- `-AhciDisk` attaches a fresh copy, `build/ahci-test.img`, as `ide-hd,bus=ide.0` on the built-in AHCI controller (`run.ps1:352-361`), and hash-checks it after boot (`run.ps1:464-467`).
- `test.ps1:35-157` runs 60 cases per build profile.
  - Five fault modes need the VirtIO fixture: BlockIoError, BlockWrite, BlockWriteError, BlockFlushError and BlockNoFlush (`run.ps1:203-209`).
  - Four cases pass `-AhciDisk` (`test.ps1:53-60`).
- `Assert-MirrorDriverCatalog` (`common.ps1:41-119`):
  - It accepts only the driver names `virtio-blk-legacy`, `ahci-sata`, `xhci-host` and `none` (`:73`).
  - It requires exactly one AHCI controller and one modern VirtIO boot device (`:115`).
- `run.ps1:648-650` requires `virtio_block>=1`. That check stays (0014-D7, S1b).
- PCI inventory and paging:
  - Today's q35 inventory has 7 functions with the VirtIO fixture and 6 without it (`build/test-logs/debug-None.serial.log`, `debug-AhciDisk-None.serial.log`). AHCI is at `00:1f.2`, ID `8086:2922`, class `01:06:01`.
  - `V1` has 7 functions, well under the 32-entry cap (`kernel/src/pci.rs:33`).
  - At 4 devices per page (`kernel/src/console.rs:17`), the capture's `devices 2` page (`capture.ps1:517-523`) still exists.
- The local QEMU 11.1.0 (`.tools/qemu/VERSION`) includes the `e1000-82545em` model, the user-network `restrict=on|off` option and `libslirp-0.dll`.

**Profiles.** The two profiles differ only in the storage fixture.

| Element | `V1` (default) | `LegacyVirtio` | VMware reference |
| --- | --- | --- | --- |
| Machine | `q35,accel=tcg`, i8042 and HPET on | same | EFI firmware |
| Storage fixture | fresh `build/ahci-test.img`, `ide-hd,bus=ide.0,rerror=report` | legacy `virtio-blk-pci,disable-modern=on,disable-legacy=off` | SATA (AHCI) disk on VMware's own AHCI controller, `15ad:07e0`, class `01:06:01` |
| NIC | `-netdev user,id=mirror-net,restrict=on` and `-device e1000-82545em,netdev=mirror-net,romfile=` | same | `ethernet0.virtualDev = "e1000"` (`8086:100f`) |
| Keyboard and mouse | q35 i8042 PS/2 | same | PS/2 |
| Display | `-vga std`, OVMF GOP | same | SVGA II (`15ad:0405`); Mirror uses the GOP framebuffer until 0069 |
| Boot medium | modern virtio-blk, read only by firmware (0014-D7, S1b): vvfat ESP, then 0015a's image | same | 0015a VMDK or 0015b ISO |

NIC and variant details:
- `-nic none` stays in place to suppress QEMU's default NIC.
- The user-mode backend prevents QEMU's "has no peer" warning, and `restrict=on` keeps the guest off the host network.
- `romfile=` keeps the iPXE option ROM out of firmware startup.
- The variant switches (`-NoTestDisk`, `-NoKeyboard`, `-NoDisplayDevice`, `-NoHpet`, `-Xhci*`, `-ExtendedCpu`, `-LegacyPic`, `-PitTimer`, `-CpuCount`) apply on top of either profile. 0017c later adds `-NoVmport`.

Later M0 specs on these profiles:
- 0015a replaces the vvfat ESP with its disk image on the same firmware-only virtio-blk device (0015-D10, S1b), in both profiles. The catalog's one-modern-VirtIO rule (`common.ps1:115`) and `run.ps1:648-650` are unchanged by it.
- 0015b's `-Iso` case attaches the ISO as an ATAPI CD on `ide.2`, which the AHCI driver skips (`kernel/src/ahci.rs:238-245`). It has no modern virtio-blk boot disk, so only the legacy fixture satisfies the smoke assert (`kernel/src/main.rs:202-205`): `-Iso` requires `-DeviceProfile LegacyVirtio`, and the `test.ps1` `-Iso` case runs under that profile, on top of its six cases. 0015b owns these script changes.
- The VMware column holds the reference settings for 0015a's manual VMware boot check, and 0015a's VM settings doc holds the full settings. That check also confirms this device set in the serial log: one `driver=ahci-sata` record, one `id=8086:100f class=02:00:00` record, and `MIRROR_KEYBOARD_READY`, `MIRROR_MOUSE_READY` and `MIRROR_FRAMEBUFFER_READY`. On VMware the `driver=ahci-sata` record names `id=15ad:07e0 class=01:06:01`. That is VMware's own AHCI controller, not an Intel ICH ID like q35's `8086:2922`, and Mirror binds it by class alone (`kernel/src/pci.rs:82-84`, `kernel/src/drivers.rs:40`), so no ID list needs it. (Amended 2026-10-10 from the VMware Workstation Pro 26.0.1 check: AHCI.ROM's PCI header and vmware-vmx's config-space template.)

**Script changes.**
- `common.ps1`:
  - Add `Get-MirrorDeviceArguments -Profile <V1|LegacyVirtio> -RepositoryRoot <path> [-NoTestDisk]`. It returns the NIC arguments, the fixture's `-device` string and the fixture image path. For `V1` it also recreates `build/ahci-test.img`.
  - Callers still choose the fixture backend: a plain `-drive`, or, in `run.ps1`, the blkdebug and writable `-blockdev` nodes (`run.ps1:362-407`).
  - In `Assert-MirrorDriverCatalog`, require exactly one `id=8086:100f class=02:00:00 driver=none match=none` record and no other record with class `02:`. The error message names `8086:100f`. The catalog arithmetic does not change, and the NIC counts as unsupported.
- `run.ps1`:
  - Add `[ValidateSet('V1','LegacyVirtio')] $DeviceProfile = 'V1'` and remove `-AhciDisk`.
  - Validation: Block* faults require `LegacyVirtio`. AhciIoError and AhciInitFailure require `V1`. `LegacyVirtio` combined with `-NoTestDisk` is rejected.
  - Replace every `$AhciDisk` test (`:204-211, 352-361, 391-407, 464-467, 519-520, 629`) with `$ahciFixture = $DeviceProfile -eq 'V1' -and -not $NoTestDisk`. At `:519`, `-LegacyDisk` becomes `LegacyVirtio` without `-NoTestDisk`.
  - The success line (`:807`) prints `profile=` instead of `ahci-disk=`.
- `capture.ps1`:
  - Attach the `V1` devices. After QEMU exits, hash-check `ahci-test.img` in the same way as `run.ps1:464-467`.
  - In the required marker list (`:422`), replace `MIRROR_BLOCK_READY`, `MIRROR_BLOCK_READ_OK` and `MIRROR_BLOCK_CLEANUP_OK` with `MIRROR_AHCI_READY`, `MIRROR_AHCI_READ_OK`, `MIRROR_AHCI_CLEANUP_OK` and `MIRROR_BLOCK_SKIPPED`. 0010, which lands next, then runs captures on trace builds without fixture recognition (0010-D5) and swaps `MIRROR_AHCI_READ_OK` for `MIRROR_AHCI_PROBE_OK fixture=0 reads=1`.
  - At `:59-60`, pass `-LegacyDisk $false` and `-Attached $true`.
- `test.ps1`:
  - Apply the case matrix below.
  - Name logs `<build>-<case>` for `V1` cases and `<build>-LegacyVirtio-<case>` for `LegacyVirtio` cases, where `<build>` is `debug` or `release`.
  - Compute the boot count in the summary line (`:150`) instead of hard-coding 120.
- `readme.md`: update the run examples (`:33-36`), the `-AhciDisk` paragraph (`:359`) and the boot counts (`:9, 23, 209`).
- `AGENTS.md` (written by 0012): the one-boot line names `-DeviceProfile` instead of device switches, and any line that names `-AhciDisk`, the VirtIO fixture or a fixed boot count is updated. 0012's text names none of these.

**Case matrix (per build profile).**

| Group | Today | After |
| --- | --- | --- |
| Fault loop (`test.ps1:45`) | 27, VirtIO fixture | 22 non-Block faults, `V1` |
| `LegacyVirtio` | 0 | 6: None, BlockIoError, BlockWrite, BlockWriteError, BlockFlushError, BlockNoFlush |
| AHCI-specific (`test.ps1:53-60`) | 4: None, AhciIoError, AhciInitFailure, CpuCount4 | 2: AhciIoError and AhciInitFailure. None and CpuCount4 duplicate `V1` base cases (`test.ps1:45`, `:89`). |
| xHCI, CPU, SMP, timer, clock, PIC and device-removal cases (`test.ps1:64-140`) | 29 | 29, now on `V1` |
| Total | 60 (120 boots) | 59 (118 boots) |

## Acceptance tests

1. **Suite.** `scripts/test.ps1` passes, and its summary reports 118 smoke boots plus the debug and release captures: the 120 at HEAD 4614249 minus 2. 0012 and 0011, which land first, add no boots. `scripts/test.ps1 -AgentDemo` also passes, because the changed matrix and captures also run in agent-demo builds (P8).
2. **Catalog.** `Assert-MirrorDriverCatalog` enforces the following in every smoke boot and in both captures:
   - exactly one `id=8086:100f class=02:00:00 driver=none match=none` record;
   - exactly one `driver=ahci-sata` record;
   - no `virtio-blk-legacy` record in `V1` boots, and exactly one in `LegacyVirtio` boots.
3. **AHCI by default.** Every `V1` smoke boot that has the fixture, and both captures, report each of these markers exactly once:
   - `MIRROR_AHCI_READY sectors=2048 readonly=1 port=0`
   - `MIRROR_AHCI_READ_OK sectors=2048 reads=262 bounds_rejected=2`
   - `MIRROR_AHCI_CLEANUP_OK dma_reclaimed=1`
   - `MIRROR_BLOCK_SKIPPED reason=no_supported_device`

   After 0010, the captures print `MIRROR_AHCI_PROBE_OK fixture=0 reads=1` instead of `MIRROR_AHCI_READ_OK`; smoke boots are unchanged. After each boot and each capture, `build/ahci-test.img` is byte-identical to the fixture. Most configurations have never run AHCI with a disk attached. NoHpet-CpuCount4, NoHpet-LegacyPic and ClockActivation-CpuCount4 have no clock, so their AHCI waits rely on the 50,000,000-poll bound instead (`kernel/src/ahci.rs:74, 654-659`).
4. **LegacyVirtio parity.** Before making the change, copy `build/test-logs` outside `build/`, because `test.ps1` empties it at start (0011-D7). For each of the six `LegacyVirtio` cases, the `^MIRROR_BLOCK_` lines match those in the saved log for the same fault and build. The writable-image reconstruction (`common.ps1:749-788`) still passes.
5. **Argument validation.** In each of these cases the script throws and QEMU does not start:
   - `run.ps1 -Smoke -NoBuild -Fault BlockWrite`. The error names `-DeviceProfile LegacyVirtio`.
   - `run.ps1 -Smoke -NoBuild -DeviceProfile LegacyVirtio -Fault AhciIoError`
   - `run.ps1 -Smoke -NoBuild -DeviceProfile LegacyVirtio -NoTestDisk`
   - `run.ps1 -AhciDisk`, which is rejected at parameter binding.
6. **The NIC rule can fail.** Dot-source `common.ps1` and run `Assert-MirrorDriverCatalog -Smoke $true -LegacyDisk $false` on a passing `V1` `debug-None.serial.log`, once for each of these edits to the NIC record:
   - change the ID to `8086:100e`;
   - change the class to `02:80:00`;
   - change the class to `ff:00:00`, so that no NIC is present.

   Each call throws an error that names `8086:100f`. Record the result in the commit message.
7. **No NIC warnings.** No `*.stderr.log` from a `V1` case contains `has no peer`.
8. **Runtime.** On Cameron's machine, measure `scripts/test.ps1` with `Measure-Command` before and after the change, and record both times in the commit message. The after time is at most the before time plus 5%. Reference per-case times, from consecutive log mtimes in `build/test-logs`:
   - AHCI cases: 8.3-9.6 s debug, 5.4-6.4 s release.
   - VirtIO fault-loop cases: 8.9-12.9 s debug, 5.0-6.6 s release.

## Dependencies

None: no crates and no new tools. The profiles use the QEMU 11.1.0 already in `.tools/qemu` (GPL-2.0, a host tool) and its bundled libslirp (BSD-3-Clause) for the user-mode network backend.

## Open questions

- For 0070: should the driver also accept the 82540EM (8086:100E)? It is QEMU's plain `e1000`, and it shares the 8254x register model. If so, 0070 adds one `V1` variant case that uses that model.
- For 0062: delete the `LegacyVirtio` profile, or replace its cases, when it adds AHCI write and flush cases (0014-D5). 0015b now lands after 0062 (moved to Phase 11 and re-drafted, 2026-10-11), so 0062 has no `-Iso` case to rehome. 0015b's re-draft places it, and under `V1` the case needs the smoke assert at `kernel/src/main.rs:202-205` to accept a boot with no VirtIO block device.
- For 0069: this spec keeps `-vga std`. The VMware SVGA II adapter joined the v1 device set when S1a was amended (2026-10-09, S1c), and 0069 drives it for mode setting and live resize. (Amended 2026-10-10 with S1c's rewording: VMware sends resize requests only over the guest RPC channel, so live resize also needs that channel. Whether an RPC core moves ahead of 0069 or live resize moves to 0087 is pending in decisions.md. Either way QEMU has no model of the channel.) Until 0069 the kernel uses only the GOP framebuffer, so no M0 code depends on the QEMU display choice. Should 0069 switch the profiles to QEMU's `-vga vmware`, an SVGA II model (`vgabios-vmware.bin` ships locally), or add it as a `V1` variant case?
- Suggested follow-up spec: per-case work directories, so that cases can run in parallel. Fixed paths force serial runs: `run.ps1:308-316`, `:357` and `common.ps1:744`. Parallel runs are the main way to cut the 20-minute suite time. README.md lists it under the unscheduled follow-ups.
