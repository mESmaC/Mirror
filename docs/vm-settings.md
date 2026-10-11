# VMware VM settings

This page records the reference VMware Workstation VM: the VM the manual checks boot Mirror's disk image in. The manual VM checks in specs 0015a, 0015b, 0013, 0016a, 0016b, 0017a, 0017b and 0017c use it, and so does 0019's acceptance test 6 until 0018's own VM exists (0015-D11). It is not the daily-drive VM.

VMware is the only VM target before physical hardware (ledger G3a). QEMU runs the automated tests.

## Booting the disk image

`.\scripts\package.ps1` builds the default kernel flavor and copies `build\mirror.img` and its VMDK descriptor `build\mirror.vmdk` into `build\dist\`. The VM uses `build\dist\mirror.vmdk`, never the files under `build\` that test runs rewrite. The descriptor points VMware at the raw image beside it, with no conversion step.

To boot a newer build, power the VM off and run `package.ps1` again. The descriptor is byte-identical across rebuilds, so the VM needs no change (0015-D9).

## Creating the VM

Create the VM the way Workstation does, then set the keys under Settings. Use the New Virtual Machine wizard with "Other 64-bit" as the guest, or from a shell:

```
vmcli VM Create -n <name> -d <dir> -c other-64
```

Either one writes Workstation's defaults, which the checks depend on: `pciBridge0` plus `pciBridge4` to `pciBridge7`, each a `pcieRootPort` with 8 functions (32 root ports, so 42 PCI functions with the disk and NIC), `vmci0.present = "TRUE"`, `mks.enable3d = "TRUE"` and `hpet0.present = "TRUE"`. A hand-written `.vmx` without the extra root ports gives a smaller PCI layout and hides the 32-function inventory cap that spec 0013 removes. `vmcli` writes no `firmware` key, which means BIOS, so the `firmware` key below is required.

## Reference

Confirmed by spec 0015a's check 9 on 2026-10-11, on the install below, in a VM created as above with exactly the settings on this page. Changing a value means rerunning that check.

| Field | Value |
| --- | --- |
| VMware Workstation Pro | 26.0.1 (26H1u1), build 25688693 |
| Virtual hardware version | `virtualHW.version = "22"`, the newest and the default for a new VM (0015-D13) |
| Firmware | EFI20-64.ROM, version `VMW201.00V.25600138.B64.2607151225` (vmware.log: "The EFI ROM is EFI2.7 64-bit."); monitor mode ULM |
| Display mode at boot | 1024x768, 32 bpp (`MIRROR_FRAMEBUFFER_READY width=1024 height=768`), the mode VMware's firmware and Limine pick with no `resolution:` pin |

## Settings

These are literal `.vmx` keys, checked against Workstation Pro 26.0.1 build 25688693 on 2026-10-10. `<repo>` stands for the absolute path of the repository checkout.

**Guest and firmware.** The guest type is "Other 64-bit". A missing `firmware` key means BIOS, and the image has no BIOS boot stages (0015-D8).

```
guestOS = "other-64"
firmware = "efi"
uefi.secureBoot.enabled = "FALSE"
```

Give the VM 1 GiB of memory and 1 vCPU unless a check says otherwise:

```
memsize = "1024"
numvcpus = "1"
```

**Disk.** Attach the existing disk on SATA, and remove any NVMe or SCSI disk the new-VM wizard adds.

```
sata0.present = "TRUE"
sata0:0.present = "TRUE"
sata0:0.fileName = "<repo>\build\dist\mirror.vmdk"
```

**Network.** An Intel e1000 adapter (0014-D4):

```
ethernet0.virtualDev = "e1000"
```

**Input.** A PS/2 keyboard and mouse, and no USB controller, so no USB mouse or tablet. Set these keys to `"FALSE"`, or leave them out:

```
usb.present = "FALSE"
ehci.present = "FALSE"
usb_xhci.present = "FALSE"
```

**HPET.** Set it explicitly, not left to the creation default (ledger S10). Workstation writes this key for new and upgraded VMs at virtual hardware 8 and later, but vmware-vmx treats a missing key as `FALSE`.

```
hpet0.present = "TRUE"
```

**Display.** Use the SVGA II adapter at its default settings, and do not set `svga.autodetect = "FALSE"`. If a host forces it, keep `svga.maxWidth` at 1920 or more, `svga.maxHeight` at 1080 or more and `svga.vramSize` at 8294400 or more, because VMware's GOP drops modes above these limits.

**Serial.** The serial port writes to a file under `build\vm-logs\`, one per check. VMware asks whether to append to or replace an existing file, so the answer is set here (0015-D14); deleting the file before each boot also works. Add no printer, which would take a serial slot, and leave `efi.serialconsole.enabled` unset.

```
serial0.present = "TRUE"
serial0.fileType = "file"
serial0.fileName = "<repo>\build\vm-logs\<check>.serial.log"
answer.msg.serial.file.open = "Replace"
```

**Unattended start and RTC** (0015-D14). `uuid.action` keeps a moved or copied VM from asking whether it was moved, and `msg.autoAnswer` answers any other power-on question, so a headless `vmrun -T ws start <vmx> nogui` never stalls. `rtc.diffFromUTC` makes the RTC hold UTC (ledger S15); when it is unset, VMware starts an `other-64` VM's RTC at host local time.

```
uuid.action = "keep"
msg.autoAnswer = "TRUE"
rtc.diffFromUTC = "0"
```

**Left unset.** `keyboard.queryScancodeSet`, which must not be `"FALSE"` (0017b, open question 1), and `chipset.motherboardLayout`.

## Display

Mirror supports any display size (ledger S1c). The default image pins no display mode. Until spec 0069, Mirror uses the GOP mode that firmware and Limine pick, and until 0027a lifts the cap the desktop is capped at a centered 1280x800 (`kernel/src/surface.rs`).

## No USB controller

The VM has no USB controller (confirmed 2026-10-09) until a USB spec targets VMware's xHCI. With a USB 3.x (xHCI) controller present, every boot in every kernel flavor runs the xHCI probe (`probe_xhci_controller` in `kernel/src/main.rs`) against a controller no spec has tested, and Mirror's pointing is PS/2 plus vmmouse (0017c). 0018-D3 applies the same rule to its three VMs.

## Perf boot entry

The boot entry that 0018 measures sets its 1920x1080 reference resolution (0018-D3, ledger S1c) with Limine's `resolution:` option (confirmed 2026-10-09). Spec 0019's `package.ps1 -Perf` writes it; 0015a does not build it. How the entry is delivered, as a perf copy of the image or as a boot menu entry, is 0019-D12, still proposed. Daily boots keep the default image, which pins no mode. `MIRROR_FRAMEBUFFER_READY` confirms the mode, because Limine picks another mode when the requested one is unavailable.
