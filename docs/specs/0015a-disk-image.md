# 0015a: Disk image: GPT, FAT32 ESP, ext2 root and VMDK

Status: confirmed (2026-10-09)
Depends on: 0014 (device profiles; `-AhciDisk` is gone) and 0010 (flavors; captures run `test-trace`), which land first in the M0 order. 0013 depends on this spec for its manual checks, not the reverse: checks 9 and 10 can run before 0013 lands, and firmware or PCI discovery failures go to 0013. 0015b builds on this spec.
Ledger: S5, S2, S2a, S1b, G3a (related: G3, S1, S1a, S3, S7a, P3, P5c)

## Goal

`scripts/build.ps1` writes `build/mirror.img`, a raw 1 GiB GPT disk with a FAT32 EFI system partition (Limine, the kernel, the four native modules, `limine.conf`) and a minimal ext2 root partition holding a marker file, plus `build/mirror.vmdk`, a descriptor that lets VMware and VirtualBox use the raw file directly. A native Rust host tool, `tools/mirror-image`, writes both on Windows, with no WSL and no external image tools. Every QEMU boot in `run.ps1` and `capture.ps1` uses the image instead of the vvfat host directory (0015b's `-Iso` case later boots the ISO). The image boots to the Mirror shell in VMware Workstation and VirtualBox, set up as `docs/vm-settings.md` records.

## Non-goals

- The live ISO and its `-Iso` case (0015b).
- Reading or writing the ext2 root from Mirror (0064). FAT32 writes from inside Mirror (S2a). An installer (S5a).
- BIOS boot (D8), Secure Boot signing, physical hardware (G3).
- Moving the QEMU boot disk off firmware-only virtio-blk (D10, S1b).
- VM definition files (`.vmx`, `.vbox`, VBoxManage scripts). The settings doc lists settings.
- Updating an existing daily-drive disk in place, parallel test runs, a build-identity banner, CI (P3).

## Decisions

| ID | Decision | Status |
| --- | --- | --- |
| 0015-D1 | Two artifacts: a GPT disk image (FAT32 boot partition plus ext2 root) and a bootable ISO. Both run in VMware and VirtualBox. The disk image replaces the host-directory boot for QEMU tests. This spec builds the disk image; 0015b builds the ISO. | confirmed (ledger S5) |
| 0015-D3 | VMware is the primary target, VirtualBox is also supported, and QEMU runs the automated tests. | confirmed (ledger G3a) |
| 0015-D4 | Tooling: a native Rust host tool, `tools/mirror-image`. It uses the `fatfs` crate for FAT32. The GPT, CRC32, ISO9660/El Torito (0015b) and VMDK-descriptor writers and the ext2 formatter (D6) are hand-written. Consequence: `build.ps1` needs the MSVC linker and Windows SDK, which today only the host tests need (`readme.md:13`, `build.ps1:14`). Rejected: WSL with sgdisk, mtools, mke2fs and xorriso; Cygwin/MSYS2 builds pinned in `.tools`. | confirmed (2026-10-09) |
| 0015-D5 | Layout: a 1 GiB disk, with a 256 MiB FAT32 ESP at 1 MiB and a 766 MiB root partition after it. Nothing persists before M5, so the size can grow then (Open questions 1-2). Rejected: a larger disk now (8-64 GiB), which needs sparse files or format conversion on Windows. | confirmed (2026-10-09) |
| 0015-D6 | Minimal ext2 now: `mirror-image` formats the root partition with a minimal ext2 filesystem (a small formatter, about 400 lines) holding a marker file. Mirror cannot read it until 0064. The M0 format binds nothing: every build rewrites the whole image, and 0064 chooses the feature set it reads and writes, changing the formatter if needed. Rejected: a typed, named, zero-filled partition until 0064. | confirmed (2026-10-09) |
| 0015-D7 | `run.ps1` passes its per-run Limine config as an SMBIOS type 11 OEM string with the `limine:config:` prefix. Limine v12.9.3 `CONFIG.md` says such a config takes precedence over every config file, and that on UEFI `boot():` is then Limine's boot device. The config is `boot/limine.conf` plus the `mirror.test`, `mirror.irq` and `mirror.timer` arguments (built today at `run.ps1:303-308`). The image is attached read-only and stays byte-identical across all cases. Rejected: rewriting `limine.conf` inside the image before each run, which changes the artifact under test. | confirmed (2026-10-09) |
| 0015-D8 | UEFI only: no Limine BIOS stages on the disk or on 0015b's ISO. The BIOS stages would go untested, because every QEMU case boots OVMF and S1a names the UEFI boot framebuffer as the v1 display. Rejected: `limine.exe bios-install`, `limine-bios.sys` and `limine-bios-cd.bin` from the pinned archive. | confirmed (2026-10-09) |
| 0015-D9 | VM disk: a `monolithicFlat` VMDK descriptor over the raw image, with a fixed `ddb.uuid.image`. VMware and VirtualBox both use it. There is no VDI and no conversion step, so VirtualBox keeps accepting a rebuilt disk without re-registering it. Rejected: `qemu-img convert` to sparse VMDK or VDI, which gives a new UUID on every rebuild. | confirmed (2026-10-09) |
| 0015-D10 | In QEMU the image stays on today's modern-only virtio-blk device (`run.ps1:334`). Only the firmware reads it; Mirror never binds it (`common.ps1:72-75`), and Mirror's drivers see only the v1 device set. Rejected: the image as an AHCI `ide-hd` with `snapshot=on`, which breaks the smoke VirtIO assert (`kernel/src/main.rs:202-205`), the AHCI driver's first-port choice (`kernel/src/ahci.rs:238-245`) and the fixture's `port=0` check (`common.ps1:259`). | confirmed (2026-10-09, ledger S1b) |
| 0015-D11 | VM settings doc: `docs/vm-settings.md` records the reference VMware Workstation and VirtualBox versions, the VMware virtual hardware version, the VirtualBox chipset, and the required VM settings for both hypervisors, including VirtualBox with HPET enabled (it ships disabled). The manual VM checks in this spec, 0015b, 0013, 0016a/b and 0017a/b use these settings. | confirmed (2026-10-09) |
| 0015-D12 | The ext2 on-disk layout code (superblock, group descriptor, inode and directory-entry structures, their little-endian encoding, block-group geometry) lives in a shared `no_std` crate, `crates/ext2-layout`, with host tests, as `crates/acpi` does for ACPI tables. `tools/mirror-image` path-depends on it now, and 0064 reuses and extends it. It holds format definitions only: no allocation, no I/O. Rationale: one definition of the format, so the formatter and Mirror's reader cannot drift apart. Alternative: keep the ext2 code private to `tools/mirror-image` (host-only, `std`), and let 0064 write Mirror's layout code and extract a shared crate then. That avoids fixing a crate API before 0064 designs its driver, at the cost of two definitions until 0064. | confirmed (2026-10-09) |
| 0015-D13 | Reference values that D11's doc records and the ledger does not fix: VirtualBox uses the PIIX3 chipset, its default for this guest type, and VMware uses the default virtual hardware version of the reference Workstation release. Rationale: a fresh VM gets these values. Mirror reads PCI configuration through ports 0xCF8/0xCFC with no ECAM (`kernel/src/pci.rs:4, 25`), so PIIX3's lack of PCIe costs nothing today. Alternative: ICH9 in VirtualBox, which matches QEMU's q35 more closely (PCIe, MSI) but is not VirtualBox's default for this guest type. | confirmed (2026-10-09) |

## Interfaces

**Current state (HEAD 4614249; 0014 and 0010 shift line numbers).** `build.ps1:77-86` stages `build/esp/` (about 11 MiB for a debug build). `run.ps1:333-334` and `capture.ps1:277-278` expose it to OVMF as a read-only `fat:` vvfat drive. `readme.md:165` says no standalone disk image exists.

**Host tool `tools/mirror-image/`.** A separate host-only Cargo workspace with its own `[workspace]` table and committed `Cargo.lock`. The root `Cargo.toml` lists it under `exclude`, not as a member (`Cargo.toml:2`), because `cargo clippy --workspace` (`test.ps1:17`) builds for `x86_64-unknown-none`. Build and test it with `--locked --target x86_64-pc-windows-msvc`, as the host tests are (`test.ps1:15`); the explicit target overrides `build.target` in the root `.cargo/config.toml:2`. Its `rust-version` is the pinned major.minor, like the root's, and each toolchain bump changes both (0011-D2, D3). Under D12, `crates/ext2-layout` joins the root `members`, with `[lints] workspace = true` like every member (0011-D5). Commands:
- `mirror-image disk --esp build/esp --out build/mirror.img --vmdk build/mirror.vmdk`
- `mirror-image verify --esp build/esp build/mirror.img`: checks both GPT headers and their CRCs, reads every ESP file back through the image's own FAT32 structures and compares the bytes with `build/esp`, and reads the ext2 superblock, group descriptors and marker file back. 0015b extends `verify` to the ISO.

The output is byte-reproducible: fixed disk and partition GUIDs, FAT volume ID and ext2 UUID; fixed timestamps (1980-01-01 on FAT and ext2); sorted directory order. Errors, such as ESP content larger than the partition, fail the build. Each output is written to a temporary name and renamed at the end, so a failed run never leaves a partial artifact.

**Disk layout (D5)**, 512-byte sectors:

| LBA | Content |
| --- | --- |
| 0 | Protective MBR with one 0xEE entry |
| 1-33 | Primary GPT header and 128 entries |
| 2048-526335 | Partition 1, `MIRROR-ESP`, type C12A7328-F81F-11D2-BA4B-00A0C93EC93B (EFI system). FAT32, label `MIRROR`, 2 KiB clusters: about 130,500 clusters (FAT32 needs at least 65,525). |
| 526336-2095103 | Partition 2, `mirror-root`, type 0FC63DAF-8483-4772-8E79-3D69D8477DE4 (Linux filesystem data). Minimal ext2 (D6). |
| 2097119-2097151 | Backup GPT entries and header |

**ESP.** `build/esp` as staged today (`/EFI/BOOT/BOOTX64.EFI`, `/limine.conf`, `/kernel.elf`, `/init.elf`, `/desktop.elf`, `/agent.elf`, `/app.elf`), plus Limine's `LICENSE` and `3RDPARTY.md` from the pinned archive under `/LICENSES/limine/`, because the image redistributes Limine binaries (BSD-2-Clause). Limine finds `/limine.conf` on the volume holding its EFI executable, and `boot():` resolves to that partition, so `boot/limine.conf:1-14` is unchanged.

**ext2 root (D6).** Revision 1 (dynamic) with no compat, incompat or ro_compat features; state clean; volume name `mirror-root`. 4 KiB blocks and 32,768 blocks per group give 196,096 blocks in 6 groups, each with a superblock and descriptor copy (no `sparse_super`), two bitmaps and an inode table of 8,192 256-byte inodes. The root directory (inode 2) holds `lost+found` (inode 11, which `e2fsck` expects) and the marker file `mirror-root` (inode 12) containing the line `mirror-root 1`. Everything else is zero.

**VMDK (D9).** A text descriptor, `createType="monolithicFlat"`, with one `RW 2097152 FLAT "mirror.img" 0` extent and the fixed `ddb.uuid.image`.

**Scripts and docs.**
- `build.ps1`: after staging, builds `mirror-image`, writes `mirror.img` and `mirror.vmdk` for whichever flavor it built (default, `-Trace` or `-Smoke`), then runs `verify`.
- `run.ps1`: replace the `fat:` drive (`:333`) with `file=build/mirror.img,format=raw,readonly=on` on the same device. Replace the staged-config write (`:308`) with `build/limine-smbios.txt`, holding `limine:config:` followed by the config, passed as `-smbios type=11,path=...` (the bundled QEMU lists `type=11[,value=str][,path=filename]`), with commas escaped by `ConvertTo-QemuOptionValue` (`common.ps1:600-604`). The staged-file check (`:256-260`) checks the image instead.
- `capture.ps1`, every mode including 0010's `-Hygiene`: boots the image (`:277`) with its embedded config and no SMBIOS string, checks the image instead of the tree (`:22-26`) and drops the config copy (`:27`).
- `common.ps1`: unchanged. The one-modern-VirtIO catalog rule (`:78`) and `run.ps1:648-650` still hold, since the image sits on that device.
- `test.ps1`: adds `cargo fmt --check`, clippy with `-D warnings` and the host tests for `mirror-image` (plus `crates/ext2-layout` under D12). The boot count is unchanged.
- New `scripts/package.ps1 [-Release]`: runs a default-flavor build (S7a) and copies `mirror.img` and `mirror.vmdk` to `build/dist/` (0015b adds `mirror.iso`), so VMs never share files with test runs.
- `docs/vm-settings.md` (D11, D13):
  - Reference table: VMware Workstation Pro version and build, virtual hardware version, VirtualBox version and chipset, filled in from the installs used for checks 9 and 10. Changing a value means rerunning those checks.
  - VMware: guest "Other 64-bit"; UEFI, Secure Boot off; 1 GiB and 1 vCPU unless a check says otherwise; `build/dist/mirror.vmdk` as an existing SATA disk; `ethernet0.virtualDev = "e1000"` (0014-D4); PS/2 keyboard and mouse, no USB mouse or tablet; default SVGA II display; serial port to a file.
  - VirtualBox: type "Other/Unknown (64-bit)"; Enable EFI on, Secure Boot off; the D13 chipset; `VBoxManage modifyvm <vm> --hpet on`; 1 GiB and 1 vCPU; `mirror.vmdk` on a SATA (AHCI) controller; `--nic-type1 82545EM`; `--keyboard ps2 --mouse ps2`; VMSVGA; serial port in Raw File mode.
- `readme.md:165` (or `docs/building.md`, under P5c) describes the artifacts and links the doc. `AGENTS.md`: `build.ps1` writes the disk image and VMDK and needs the MSVC linker and Windows SDK (D4); the shared-`build\` line names the image and `build/limine-smbios.txt` instead of the staged `limine.conf`; VM checks use `docs/vm-settings.md`.

## Acceptance tests

1. Host tests (`cargo test --locked --target x86_64-pc-windows-msvc --manifest-path tools/mirror-image/Cargo.toml`, run from `test.ps1`) cover:
   - primary and backup GPT headers with valid CRC32s and the D5 boundaries;
   - an ESP that is FAT32 by cluster count, with every input file reading back byte-identical through `fatfs`;
   - an ext2 superblock, descriptors, bitmaps and free counts that agree, with `lost+found` and the marker reading back;
   - a VMDK extent equal to the image size divided by 512;
   - byte-identical output from two runs with identical inputs;
   - an ESP tree over 256 MiB failing with an error and leaving no output file.
2. `build.ps1` completes `verify` for debug and release in the default, trace and smoke flavors.
3. Every smoke case passes booting `build/mirror.img`, and `git grep -n "fat:" -- scripts` finds nothing. Fault cases prove the SMBIOS config reached the kernel: `-Fault PageFault` passes only with QEMU exit 35 (`run.ps1:472-473`).
4. In `test.ps1`, `build/mirror.img` has the same SHA-256 before and after each build profile's smoke cases.
5. Debug and release `capture.ps1`, including the `-Hygiene` capture, pass while booting the embedded config.
6. ext2 cross-check, now and after any formatter change: in a Linux guest, `e2fsck -fn` on partition 2 of a copy of `build/dist/mirror.img` exits 0, and a read-only mount shows `lost+found` and the marker. Record the e2fsprogs version and output in the commit message.
7. Measurements in the commit message: disk and verify time per `build.ps1` run (5 s or less on the dev machine, together with 0015b's ISO); artifact sizes; full `test.ps1` wall time before and after (at most 10% growth).
8. `docs/vm-settings.md` fills every reference field from the installs actually used, lists the settings above (VirtualBox HPET on included), and checks 9 and 10 ran with exactly those settings. The readme (or `docs/building.md`) and `AGENTS.md` link it.
9. Manual check, VMware Workstation, reference settings, `package.ps1` build: boots through Limine to the Mirror shell. The serial file has `MIRROR_BOOT_OK`, no `MIRROR_TRACE_ON`, one `driver=ahci-sata` and one `id=8086:100f class=02:00:00` record, and `MIRROR_KEYBOARD_READY`, `MIRROR_MOUSE_READY` and `MIRROR_FRAMEBUFFER_READY` (0014). Typing and pointer movement work.
10. Manual check, VirtualBox: the same. Then run `package.ps1` again and boot without re-registering the disk (D9).

The artifact part of checks 9 and 10 must pass for this spec to be done: firmware starts Limine from the ESP, and Limine loads the kernel and modules. Later failures in firmware tables or PCI discovery go to 0013, with the serial log kept in `build/vm-logs/`; rerun the checks once 0013 lands.

## Dependencies

- `fatfs` =0.3.6 (MIT), default features off, `std` and `alloc` on (`alloc` is needed for long names such as `limine.conf`). Transitive: byteorder 1.x (Unlicense OR MIT), bitflags 1.x (MIT OR Apache-2.0), log 0.4 (MIT OR Apache-2.0). `tools/mirror-image/Cargo.lock` records exact versions. No ext2 crate (D6, D12).
- The MSVC linker and Windows SDK, already a host-test prerequisite (`readme.md:13`), become a build prerequisite (D4).
- Limine v12.9.3 binary release (BSD-2-Clause), already pinned by SHA-256 (`build.ps1:48-49`); this spec also ships its `LICENSE` and `3RDPARTY.md`.
- QEMU 11.1.0 in `.tools/qemu` (GPL-2.0), already used.
- Manual checks only, nothing linked or shipped: VMware Workstation Pro and VirtualBox at the doc's reference versions; e2fsprogs in a Linux guest (check 6).

## Open questions

1. Follow-up spec, before 0064 stores data: once the root partition holds data, a rebuild replaces the whole disk. Add `mirror-image update-esp` (rewrite only partition 1 while the VM is off) or an update path inside Mirror.
2. Root size and location, decided with 0063: does the daily root live on partition 2 of the boot disk or on the mirrored pool (G8a, G8b)? The answer decides whether D5's 766 MiB must grow, and how.
3. VMware with a flat extent not named `*-flat.vmdk`: confirm in check 9. If VMware rejects it, name the raw file `mirror-flat.vmdk` and keep QEMU on `format=raw`.
