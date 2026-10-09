# 0013: Firmware and PCI tolerance for VMware/VirtualBox

Status: confirmed (2026-10-09)
Depends on: 0014, whose `V1` profile and computed boot count the QEMU cases use. 0015a, only for the manual VMware/VirtualBox checks (tests 14-16) and its VM settings doc. The code and host tests depend on nothing.
Ledger: G3a, S1, S1a (related: G3, S1b)

## Goal
Mirror's boot-time platform discovery accepts the ACPI tables and PCI topology that VMware Workstation and VirtualBox present. On both hypervisors a normal boot reaches xAPIC mode with every vCPU online, and the AHCI controller and e1000 NIC behind VMware's bridges are visible to driver binding. A redundant or surplus MADT record costs at most that record, and an unused table costs at most that table. Neither sends the machine to single-CPU PIC/PIT mode. Boots without an HPET (VirtualBox's default) keep SMP and AHCI; 0014's `V1` NoHpet-CpuCount4 case covers them in QEMU.

Verified defects this spec removes:
- A type-4 wildcard local NMI (UID 0xFF) plus a type-10 wildcard (UID 0xFFFFFFFF) on the same LINT returns `Error::Duplicate` (crates/acpi/src/lib.rs:597-604). The MADT is then discarded (kernel/src/acpi.rs:139-166), which leaves no APIC, no LAPIC timer and no SMP (kernel/src/apic.rs:192, kernel/src/smp.rs:206-208). tests.rs:546 locks in a related case: a specific type-4 UID plus a type-10 wildcard.
- More than 64 usable CPU records, or more than 64 local NMI records, returns `Error::Capacity` from `push` (lib.rs:20-23, 676-685) and discards the MADT in the same way. CPU records with neither availability bit set are already ignored (lib.rs:564-566). NMI records are not.
- The HPET parser rejects a GAS with nonzero bit width, bit offset or access size (lib.rs:185-191; tests.rs:156-163 assert that widths 32 and 64 fail). It also rejects a block ID with REV_ID 0 or bit 14 set (lib.rs:179-184). The driver additionally requires the table's block ID to equal GCAP_ID[31:0] (kernel/src/clock.rs:163).
- Root-table entries are read with `?` (kernel/src/acpi.rs:118, 134-135). One unreadable entry aborts all discovery, the MADT and HPET included. So does an unused table whose header length falls outside 36 bytes to 256 KiB (lib.rs:117). `Root::parse` also rejects a null or repeated entry (lib.rs:237-244).
- The PCI inventory keeps the first 32 functions (kernel/src/pci.rs:33, 139-144), and every claim searches only those (pci.rs:231-235, 403-407, 572-574). A controller past entry 32 is reported as absent (`MIRROR_AHCI_SKIPPED reason=no_disk`, kernel/src/main.rs:847).

## Non-goals
- Interrupt robustness, which belongs to 0055 (M4): legacy-free platforms with PCAT_COMPAT=0 (apic.rs:193-195, after which timer.rs:120-122 unmasks IRQ0 on a PIC the firmware says is absent) or no PIT, LAPIC timer calibration against the HPET (timer.rs:176-199 uses only the PIT), catch-all vector handlers (which also make the BSP's missing IRR check harmless, apic.rs:221 compared with the AP path at apic.rs:567), LINT1 NMI on the boot CPU only, and x2APIC handoff (apic.rs:338-339, smp.rs:271-272).
- SMP with more than 64 enabled CPUs. After D4 the MADT parses, but smp.rs:157-161 still skips SMP. No v1 VM needs that many CPUs.
- A time source other than the HPET (Open question 2). Without an HPET, SYS_CLOCK_GET returns -19 (kernel/src/userspace.rs:2998-3001), and AHCI waits are bounded only by POLL_LIMIT (kernel/src/ahci.rs:74, 654-659). 0015a's reference VirtualBox settings turn the HPET on.
- The PCI device model, which belongs to 0060: BAR sizing, ECAM, MSI, several controllers of one class, and enabling decode that firmware left off (pci.rs:509-511). The e1000 driver belongs to 0070, and QEMU device profiles belong to 0014.
- General relaxation of MADT reserved-bit checks (Open question 3). Only the cases listed here change.
- Physical hardware (ledger G3).

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| 0013-D1 | The tolerance targets are VMware Workstation (primary) and VirtualBox. QEMU carries the automated coverage, and the two hypervisors are checked by hand. | confirmed (ledger G3a) |
| 0013-D2 | The controllers that must stay visible are those in the v1 device set: AHCI SATA and the e1000-family NIC. PS/2 and the UEFI framebuffer need no PCI binding. VirtIO is not a target. | confirmed (ledger S1, S1a) |
| 0013-D3 | Redundant local NMI records never reject the MADT. Two records match when they have the same LINT and the same target, meaning one UID (in either record type) or the wildcard. Their modes are compared after resolving "conforms" to active-high/edge, as apic.rs:313-314 does; the stored records keep their raw flags. A later matching record is dropped. It counts as merged if the modes are equal and as a conflict if they are not, and in both cases the first record wins. A UID-specific record and a wildcard on the same LINT are both kept, and the specific record applies to its CPU regardless of table order. Rationale: ACPI defines both 0xFF (type 4) and 0xFFFFFFFF (type 10) as "all processors", so firmware that emits both is describing one connection twice. A disagreement about the NMI pin's mode is not worth losing SMP. Alternatives: still reject the MADT on a real conflict; or, on a conflict, prefer the record whose encoding matches the CPU's record type. | confirmed (2026-10-09) |
| 0013-D4 | MADT capacity. When there are more than MAX_CPUS (64) usable CPU records, enabled records take priority over online-capable ones, and the retained CPUs stay in table order. A UID-specific local NMI record whose UID matches no retained CPU is dropped. MAX_LOCAL_NMIS becomes 2 x MAX_CPUS + 2, so the filtered set always fits. IOAPIC overflow (more than 8) and NMI source overflow (more than 16) still reject the MADT. Every drop is counted. Rationale: surplus CPU and NMI records are safe to drop. A dropped IOAPIC would leave pins that firmware programmed unmanaged, and both targets are assumed to have one IOAPIC (Open question 1). Alternative: truncate every array, IOAPICs included. | confirmed (2026-10-09) |
| 0013-D5 | The HPET table supplies only the register address. The parser ignores GAS bit width, bit offset and access size, and it stores the Event Timer Block ID without validating it. The driver still checks REV_ID and bit 14 in the hardware register. The parser still requires the system-memory space ID and a valid address. The driver treats the hardware GCAP_ID as authoritative: a mismatch with the table's block ID is logged and is not fatal. Rationale: firmware commonly encodes width 64, and Linux checks neither field. Alternative: keep requiring the table's vendor ID (bits 31:16) to match the hardware. | confirmed (2026-10-09) |
| 0013-D6 | Root-table entries are handled one at a time. A null entry is skipped. A repeated address is visited once. An entry whose signature cannot be read is skipped with a diagnostic. Tables other than APIC and HPET are not read past their signature. Only failures of the RSDP and the root table abort discovery. MADT and HPET failures stay isolated from each other, as they are today. This deliberately reverses the fail-closed root discovery documented at readme.md:219 and in the comment at acpi.rs:112-116; this spec updates both. Rationale: an unused table must not cost the MADT. Alternative: keep failing closed on unreadable signatures, since such an entry might be a second MADT, and skip only null entries and unknown tables. | confirmed (2026-10-09) |
| 0013-D7 | PCI retention grows from 32 to 256 functions in the existing fixed array (pci.rs:33, 109). Binding, the serial listing and the console `devices` pages therefore cover every function up to 256, and `omitted` counts only functions beyond that. The inventory stays allocation-free, at about 4 KiB, in kmain's frame on the 256 KiB boot stack (boot.rs:52). Rationale: VMware's bus 0 alone has more than 32 functions (Open question 1). An allocation-free inventory also keeps the boot-smoke heap tests valid: heap.rs:680-687 (run at main.rs:210) and heap.rs:593 (run at main.rs:357) both assert an empty heap while the inventory is alive. Alternative: an unbounded heap `Vec`, which would require reworking those two heap tests. | confirmed (2026-10-09) |
| 0013-D8 | Raw MADT and HPET tables are captured from a Linux live guest on the reference VMware and VirtualBox VMs and committed as host-test fixtures in crates/acpi. The capture reads `/sys/firmware/acpi/tables/`, or uses `acpidump -b`, which also yields the XSDT. Rationale: the next strictness problem gets caught without booting a hypervisor. Alternative: synthetic fixtures only, with the manual boots as the only real-firmware check. | confirmed (2026-10-09) |

## Interfaces
There is no syscall or ABI change. Line numbers are at HEAD 4614249; 0014, 0010, 0015a and 0015b land first (M0 execution order) and may shift them. Files touched:
- `crates/acpi/src/lib.rs`:
  - `Topology::parse` (lib.rs:340-508) walks the records in separate passes. CPU records (types 0 and 9) come first, using D4's priority, and the rest follow, so that NMI records can be matched against the retained CPUs.
  - `add_cpu` (lib.rs:561-591) truncates instead of failing in `push`. `add_local_nmi` (lib.rs:593-606) implements D3 and D4 instead of returning `Duplicate`.
  - `MAX_LOCAL_NMIS` (lib.rs:23) becomes `2 * MAX_CPUS + 2`.
  - New `pub struct MadtDiagnostics { omitted_cpus, merged_nmis, conflicting_nmis, unmatched_nmis }` (all `u32`) and `Topology::diagnostics()`.
  - `Hpet::parse` drops the checks at lib.rs:179-184 and lib.rs:191 (D5).
  - `Root::parse` (lib.rs:220-246) stops rejecting null and repeated entries, and `addresses()` (lib.rs:248-256) skips them (D6). `addresses()` is then no longer an `ExactSizeIterator`. The 256-entry cap stays.
  - `Error::Duplicate` remains for CPU UID or APIC ID collisions, IOAPICs, ISA overrides, NMI sources and the LAPIC address override.
  - Captured fixtures (D8) from the reference VMs, with the hypervisor version recorded next to each.
- `kernel/src/acpi.rs:111-138`: per-entry handling (D6), which replaces the fail-closed comment at acpi.rs:112-116.
  - New marker `MIRROR_ACPI_TABLE_SKIPPED index=<n> address=<hex> reason=<error>`.
  - `MIRROR_ACPI_READY` (acpi.rs:141-150) gains the trailing fields `omitted_cpus= nmi_merged= nmi_conflicts= nmi_unmatched= skipped_tables=`. The scripts/run.ps1:495 regex only requires whitespace after `enabled=N`, so it still matches.
- `kernel/src/apic.rs:305-319` `lint_configuration`: apply wildcard records first, then UID-specific ones, so that the result does not depend on table order. The BSP (apic.rs:224) and the APs (apic.rs:573) both use it.
- `kernel/src/clock.rs`: `identify` (clock.rs:162-167) stops comparing against the table's block ID. `prepare` (clock.rs:132) logs `MIRROR_CLOCK_ID_MISMATCH firmware=<hex> hardware=<hex>` when the two differ. The hardware value must still have a nonzero REV_ID, a clear bit 14, a 64-bit counter and a period within range (clock.rs:163-178).
- `kernel/src/pci.rs`: `MAX_RETAINED` (pci.rs:33) becomes 256, and the doc comments at pci.rs:108, 567-568 and 813-816 are updated to match. The three `discover` functions, `drivers.rs:57-99` and `console.rs:345-393` need no change, because they already search or list the whole retained array. The fields of `MIRROR_PCI_READY` are unchanged.
- `scripts/run.ps1`:
  - New `-CrowdedPci` switch that adds 32 root-port functions at 00:15.0-00:18.7, the slots where VMware puts its root ports. Each one is added like `-device pcie-root-port,bus=pcie.0,addr=0x15.0x0,multifunction=on,chassis=1,id=rp1,io-reserve=0`, with a unique `chassis` and `id` per port. `io-reserve=0` stops OVMF from exhausting the 64 KiB I/O space across 32 hotplug ports. It applies on top of any 0014 profile.
  - Near run.ps1:492-497, assert that no `MIRROR_ACPI_TABLE_SKIPPED` line appears and that every new `MIRROR_ACPI_READY` counter is 0.
- `scripts/common.ps1:31`: the `retained -gt 32` bound becomes 256.
- `scripts/test.ps1`: add test 11 to both build profiles, two boots in all. 0014 already computes the boot count (test.ps1:150).
- `readme.md` (or the `docs/` page that holds these sections under P5c):
  - :215: duplicate and surplus local NMI records and surplus CPU records no longer reject the topology (D3, D4), and the bounds list says 130 local NMI records.
  - :219: the HPET sentence drops the event-timer identity and descriptor-width checks (D5). The sentence "Invalid root resources or unreadable table identities still reject discovery" is replaced: only an invalid RSDP or root table rejects discovery, and null, repeated or unreadable root entries are skipped with `MIRROR_ACPI_TABLE_SKIPPED` (D6).
  - :299 and :333: 32 retained functions become 256 (D7).
- `AGENTS.md` (0012): update any line this spec makes stale, such as a list of `run.ps1` device switches.

## Acceptance tests
Host tests (`cargo test -p mirror-acpi`, which scripts/test.ps1:15 already runs):
1. Dual wildcard NMI: `[local_nmi(0xff, 1, 0), x2nmi(u32::MAX, 1, 5)]` parses, with `local_nmis().len() == 1` and `merged_nmis == 1`. The four tests.rs:544-547 cases each gain a CPU record with UID 4. They then parse with 1, 2, 2 and 1 local NMIs respectively, and `merged_nmis` of 1, 0, 0 and 1. Other tests with UID-specific NMI records and no matching CPU record (tests.rs:511, 687-696) gain CPU records or new expectations, because D4 drops unmatched records.
2. NMI conflict: `[local_nmi(0xff, 1, 0xd), x2nmi(u32::MAX, 1, 5)]` parses, keeps the first record (level-triggered) and reports `conflicting_nmis == 1`.
3. VMware-style per-slot records:
   - 128 type-0 records (4 enabled, 124 with flags 0) followed by 128 type-4 records (UIDs 0-127, LINT1, flags 5) give 4 CPUs, 4 local NMIs and `unmatched_nmis == 124`. Today the same input returns `Error::Capacity`.
   - With the 124 slots marked online-capable (flags 2) instead, the result is 64 CPUs including all 4 enabled ones, `omitted_cpus == 64`, 64 local NMIs and `unmatched_nmis == 64`.
4. Truncation:
   - 65 enabled CPU records give 64 CPUs and `omitted_cpus == 1`.
   - 60 online-capable records followed by 10 enabled ones give all 10 enabled CPUs plus the first 54 online-capable ones, in table order, with `omitted_cpus == 6`.
   - IOAPIC and NMI source overflow still return `Error::Capacity` (tests.rs:660-707 adjusted).
5. HPET: GAS bit widths 32 and 64, access sizes 1-4, a nonzero bit offset, and block IDs with REV_ID 0 or bit 14 set all parse to the same address. Nonzero space IDs still return `Error::Address` (tests.rs:138-163 adjusted).
6. Root: a null entry is skipped, and a repeated address yields one address. 257 entries still return `Error::Capacity` (tests.rs:379-400 adjusted).
7. Fixtures (D8): each captured VMware and VirtualBox MADT and HPET table parses. The test asserts the CPU, IOAPIC and local NMI counts and the diagnostics observed in tests 14 and 15.

Boot-smoke self-tests:
8. `apic::self_test` (apic.rs:886-913): `lint_configuration` returns the same values for [specific, wildcard] and [wildcard, specific] on one LINT.
9. `clock::policy_self_test` (clock.rs:340-361): the case at clock.rs:344 is accepted and reported as a mismatch. A hardware value with REV_ID 0 or bit 14 set is still rejected.
10. `pci::self_test`:
    - A synthetic 40-function scan whose last function is AHCI-class retains all 40 and finds the AHCI function with the `is_ahci` predicate.
    - The crowded scan (pci.rs:964-973) retains 256 functions with `omitted == 7936`.

QEMU cases (scripts/test.ps1, debug and release):
11. `run.ps1 -Smoke -CrowdedPci`, on 0014's default `V1` profile:
    - `MIRROR_PCI_READY` reports `functions=39 bridges=32` and `omitted=0`. That is `V1`'s 7 functions plus the 32 root ports.
    - `V1`'s e1000 sits at a bus-0 slot below 00:15, so the `ahci-sata` record (00:1f.2) is the 38th `MIRROR_DRIVER_DEVICE` record.
    - `MIRROR_AHCI_READY` and `MIRROR_AHCI_READ_OK` appear, and 0014's catalog check finds the one `8086:100f` record.
    - Before changing the kernel, run this case once against the current kernel. The run fails, and its log shows `retained=32 omitted=7` and `MIRROR_AHCI_SKIPPED reason=no_disk`, which proves that the case exercises the cap.
12. No new case for HPET-less boots. 0014's `V1` NoHpet-CpuCount4 case matches VirtualBox's default clock configuration, and its log shows `MIRROR_ACPI_HPET_SKIPPED reason=missing`, `MIRROR_CLOCK_SKIPPED reason=no_hpet`, `MIRROR_SMP_READY` with `online=4`, and `MIRROR_AHCI_READ_OK`.
13. Every existing case still passes, including the new run.ps1 assertion: no `MIRROR_ACPI_TABLE_SKIPPED` line, and every new counter at 0. `MIRROR_CLOCK_ID_MISMATCH` never appears; common.ps1:335-337 already throws on any unexpected clock marker.

Manual checks, after 0015a. Use a default-flavor build (S7a), as 0015a packages it, because smoke builds assert a VirtIO disk (main.rs:202-205). Use the reference VM settings in 0015a's VM settings doc: hypervisor versions, VMware virtual hardware version, VirtualBox chipset, and the VirtualBox HPET turned on. Each VM has UEFI firmware, a SATA (AHCI) disk holding the 0015a image, an e1000 (82545EM) NIC, and COM1 sent to a file. Keep the logs in `build/vm-logs/`.
14. VMware Workstation at 1 and 4 vCPUs. Expected:
    - `MIRROR_ACPI_READY` with `enabled=<vCPUs>`, and neither `MIRROR_ACPI_SKIPPED` nor `MIRROR_ACPI_TABLE_SKIPPED`.
    - `MIRROR_APIC_READY mode=xapic`, `MIRROR_SMP_READY` with `online=<vCPUs>`, and `MIRROR_TIMER_READY source=lapic`.
    - `MIRROR_PCI_READY ... omitted=0`, one `driver=ahci-sata` record and one `class=02:00:00` record.
    - `MIRROR_AHCI_READY` followed by `MIRROR_AHCI_PROBE_OK`.
    - `MIRROR_CLOCK_READY`, or a `MIRROR_CLOCK_SKIPPED` reason recorded under Open question 1.
15. VirtualBox at 1 and 4 vCPUs: the same markers as test 14. Then boot once with `VBoxManage modifyvm <vm> --hpet off`, VirtualBox's default, and expect the same markers except that the clock reports `MIRROR_CLOCK_SKIPPED reason=no_hpet`.
16. Record every nonzero `MIRROR_ACPI_READY` counter and any `MIRROR_CLOCK_ID_MISMATCH` from tests 14 and 15 under Open question 1.

## Dependencies
No crates. Tools:
- QEMU 11.1.0 in `.tools/qemu` (GPL-2.0) and its bundled edk2 OVMF (BSD-2-Clause-Patent), which the scripts already use. Its `pcie-root-port` device has the `io-reserve` property.
- VMware Workstation Pro and VirtualBox, at the reference versions in 0015a's VM settings doc.
- For D8, a Linux live ISO in a disposable guest on those reference VMs. `acpidump` from ACPICA (BSD-3-Clause OR GPL-2.0) is optional, because `/sys/firmware/acpi/tables/` already exposes the MADT and HPET. Nothing from either is linked or shipped.

## Open questions
1. Assumptions about VMware and VirtualBox firmware, to be confirmed by tests 14-16 and the D8 capture:
   - VMware emits one type-0 and one type-4 (LINT1) record for each of 128 processor slots (acpi_id 0x00-0x7f), most of them not present. 128 NMI records exceed today's 64-entry limit, so VMware's MADT is probably rejected now.
   - VMware's bus 0 holds 32 root ports (00:15.0-00:18.7) plus about 8 other functions, so its SATA controller and NIC, on bus 2 or higher, fall past the current cap. VirtualBox's bus 0 has about 12 functions. Neither VM comes close to 256 functions.
   - Both hypervisors:
     - set PCAT_COMPAT and have one IOAPIC;
     - emit no type-3 NMI source records (apic.rs:196-198 refuses them);
     - hand off in xAPIC mode;
     - place the RSDP, XSDT, MADT and HPET in memory that Limine reports as ACPI reclaimable, ACPI NVS or bootloader reclaimable (acpi.rs:270-273 refuses everything else).
   - VMware's HPET has a 64-bit counter and a period of 1-100 ns. VirtualBox's HPET, when enabled, may be 32-bit only, in which case clock.rs:171-173 skips it.
   - Firmware leaves each AHCI controller with memory decode on, MSI off and a 32-bit non-prefetchable ABAR (pci.rs:509-511, 516, 537-538).

   A finding that contradicts an interrupt-controller assumption (PCAT_COMPAT, x2APIC handoff) goes to 0055.
2. A time source other than the HPET (the TSC calibrated against the PIT, or the ACPI PM timer through the FADT) has no owner in the spec map. The reference VMs do not need it, because 0015a's VirtualBox settings turn the HPET on. A VirtualBox VM left at its default does: SYS_CLOCK_GET, the AHCI and SMP deadlines, and xHCI device attach (xhci.rs:405-408) all need a clock. Add a spec, or fold it into 0055?
3. A reserved-bit leniency pass, needed only if the D8 captures set MADT bits that Mirror rejects. It would get its own follow-up spec.
4. Should `-CrowdedPci` later become a permanent VMware-like device profile next to 0014's `V1` and `LegacyVirtio`, instead of one separate case?
