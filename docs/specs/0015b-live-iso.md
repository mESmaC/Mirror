# 0015b: Live ISO

Status: confirmed (2026-10-09)
Depends on: 0015a (`tools/mirror-image`, its `build.ps1` step, `package.ps1` and `docs/vm-settings.md`) and 0014 (the `LegacyVirtio` profile). It lands right after 0015a in the M0 order.
Ledger: S5, S5a, S2a, G3a (related: S1b, S7a, P3)

## Goal

`scripts/build.ps1` also writes `build/mirror.iso`, a UEFI-bootable live CD. Limine loads the kernel and the four native modules from the disc into memory, so Mirror runs with no disk. `tools/mirror-image` writes the ISO9660 and El Torito structures natively. A `run.ps1 -Iso` smoke case under the `LegacyVirtio` profile boots the ISO in QEMU in both build profiles, and the ISO boots to the Mirror shell in VMware Workstation with the reference settings in `docs/vm-settings.md`.

## Non-goals

- An installer, and FAT32 writes from inside Mirror (D2).
- A root filesystem for the live system (Open question 1).
- BIOS boot: no BIOS El Torito entry (0015-D8). Secure Boot signing, a USB-writable hybrid ISO, Rock Ridge and Joliet.
- Kernel command-line options on ISO boots. The ISO boots its embedded config, so fault, PIC and PIT cases stay on the disk image.

## Decisions

| ID | Decision | Status |
| --- | --- | --- |
| 0015-D2 | The ISO boots Mirror live, with no installer in v1. Mirror never writes the FAT32 partition; host tooling produces it. | confirmed (ledger S5a, S2a) |

Decisions in 0015a that also govern this spec:
- 0015-D1: two artifacts, a disk image and a bootable ISO, both running in VMware (ledger S5; amended 2026-10-09, VirtualBox dropped).
- 0015-D3: VMware is the only VM target before physical hardware, and QEMU runs the tests (ledger G3a; amended 2026-10-09, VirtualBox dropped).
- 0015-D4: the ISO9660/El Torito writer is hand-written in `tools/mirror-image`.
- 0015-D8: UEFI only, so the ISO has no BIOS boot entry.
- 0015-D11: the manual checks use the reference VMware VM settings in `docs/vm-settings.md` (amended 2026-10-09, VirtualBox dropped).

## Interfaces

**Command.** `mirror-image iso --esp build/esp --uefi-cd <limine-uefi-cd.bin> --out build/mirror.iso`. It follows 0015a's output rules: a fixed ISO9660 date, sorted directories, and a temporary name renamed at the end. `mirror-image verify` also takes `build/mirror.iso`. It checks the volume descriptors and boot catalog, then reads every file back through the ISO9660 structures and compares the bytes with `build/esp`.

**ISO layout.** The ISO uses Limine's UEFI CD layout, written natively:
- an ISO9660 primary volume descriptor at sector 16, an El Torito boot record at sector 17, then a terminator;
- a boot catalog with one no-emulation entry for platform 0xEF. It points at `limine-uefi-cd.bin` from the pinned archive verified at `build.ps1:49-76`, a 3 MiB FAT12 image holding only `EFI/BOOT/*.EFI`. The entry's sector count equals the image size in 512-byte sectors (6144 for 3 MiB), which host test 1 asserts. VMware's EFI firmware uses EDK2's El Torito partition driver, which sizes the boot partition from that count (inference, 2026-10-10);
- a flat root directory holding `LIMINE.CONF`, `KERNEL.ELF`, the four module files, `LICENSE` and `3RDPARTY.MD`, with ISO9660 level 2 uppercase names and `;1`;
- no Rock Ridge, Joliet, BIOS entry or hybrid partition table.

Limine scans the El Torito image first, which has no config, and then the whole disc, where it finds `LIMINE.CONF` (v12.9.3 `CONFIG.md`). Its ISO9660 driver reads only the primary volume descriptor. It compares non-Rock Ridge names case-insensitively and drops the `;1` suffix (`common/fs/iso9660.s2.c` at v12.9.3), so `boot/limine.conf` works unchanged. Limine loads the modules into memory, so the ISO runs live without a root filesystem.

**Scripts and docs.**
- `build.ps1`: after 0015a's disk step, writes `mirror.iso` and verifies it together with the image.
- `run.ps1`, new `-Iso` switch:
  - Attach `build/mirror.iso` instead of the disk image: `-drive if=none,id=mirror-iso,media=cdrom,format=raw,readonly=on,...` with `-device ide-cd,drive=mirror-iso,bus=ide.2,bootindex=0` on q35's built-in AHCI controller. `ide.2` leaves `ide.0` free for the AHCI fixture. No SMBIOS config is passed.
  - `-Iso` requires `-DeviceProfile LegacyVirtio`. With no modern virtio-blk boot disk, the legacy fixture is the only device that satisfies the smoke assert (`kernel/src/main.rs:202-205`). 0014 already rejects `LegacyVirtio` with `-NoTestDisk`.
  - `-Iso` rejects any `-Fault` other than `None`, plus `-LegacyPic` and `-PitTimer`, because those need a kernel command line.
  - The kernel skips ATAPI ports (`ahci.rs:238-245`), so AHCI reports `MIRROR_AHCI_SKIPPED reason=no_disk` (`main.rs:847`).
- `common.ps1`: `Assert-MirrorDriverCatalog` gets a parameter for the expected number of modern VirtIO devices, which today is fixed at 1 (`common.ps1:112-115`). `run.ps1:519` passes 0 for `-Iso` and 1 otherwise. `run.ps1:648-650` is unchanged, because the legacy fixture satisfies it.
- `test.ps1`: adds one `-Iso` smoke case per build profile under `LegacyVirtio`, two boots in all. 0014's computed boot count includes them.
- `package.ps1` (0015a): also copies `mirror.iso` to `build/dist/`.
- `docs/vm-settings.md` (0015-D11): add the ISO steps. Remove the disk (delete the `sata0:0` lines, or set `sata0:0.present = "FALSE"`) and attach `build/dist/mirror.iso` on a SATA CD-ROM: `sata0:1.present = "TRUE"`, `sata0:1.deviceType = "cdrom-image"`, `sata0:1.fileName = "<abs>\build\dist\mirror.iso"`. Keep every other reference setting.
- `AGENTS.md`: list the `-Iso` case and its `LegacyVirtio` requirement wherever it lists `run.ps1` cases.

Line numbers are at HEAD 4614249. 0014, 0010 and 0015a land first and shift them.

## Acceptance tests

1. Host tests in `tools/mirror-image` cover:
   - volume descriptors at sectors 16-18;
   - a valid boot catalog checksum and one no-emulation entry for platform 0xEF, whose sector count is the size of `limine-uefi-cd.bin` in 512-byte sectors;
   - every ESP file reading back byte-identical under its level 2 name;
   - byte-identical output from two runs with identical inputs;
   - a missing `limine-uefi-cd.bin` failing with an error and leaving no output file.
2. `build.ps1` completes `mirror-image verify`, ISO included, for debug and release in the default, trace and smoke flavors.
3. `run.ps1 -Smoke -NoBuild -DeviceProfile LegacyVirtio -Iso` passes in debug and release with:
   - the default marker set;
   - zero modern VirtIO devices in the driver catalog;
   - `MIRROR_AHCI_SKIPPED reason=no_disk`.
4. Each of these fails before QEMU starts: `-Iso` under the default `V1` profile, and `-Iso` with `-Fault PageFault`, `-LegacyPic` or `-PitTimer`. `test.ps1` reports two more boots than before this spec.
5. Measurements, recorded in the commit message:
   - the ISO write and verify time, with 0015a's whole image step at 5 s or less;
   - the ISO size;
   - the full `test.ps1` wall time before and after.
6. Manual check, VMware Workstation. Use 0015a's reference settings with the disk removed and `build/dist/mirror.iso` on a SATA CD-ROM. It boots through Limine to the Mirror shell and passes the serial and input checks of 0015a check 9.
7. (removed: VirtualBox dropped)

As in 0015a, firmware must start Limine from the El Torito image, and Limine must load the kernel and modules from the disc. Later failures in firmware tables or PCI discovery go to 0013, with the serial log kept in `build/vm-logs/`.

## Dependencies

- No new crates. The ISO9660/El Torito writer is hand-written (0015-D4).
- From the pinned Limine v12.9.3 release (BSD-2-Clause, `build.ps1:49-50`): `limine-uefi-cd.bin`, `LICENSE` and `3RDPARTY.md`.
- QEMU 11.1.0 in `.tools/qemu` (GPL-2.0), already used for tests.
- Manual checks only: VMware Workstation Pro at the reference version in `docs/vm-settings.md`.

## Open questions

1. Follow-up spec, after M5: once the system needs a root filesystem, the live ISO needs one too, for example an ext2 image loaded into a RAM disk as a Limine module. This spec does not decide how. README.md lists it under the unscheduled follow-ups.
2. Follow-up for 0062: when it retires `LegacyVirtio` (0014-D5), it must rehome the `-Iso` case. Under `V1`, the smoke assert at `kernel/src/main.rs:202-205` must then accept a boot with no VirtIO block device.
