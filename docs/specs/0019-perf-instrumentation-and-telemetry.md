# 0019: Mirror perf instrumentation and telemetry

Status: draft
Depends on: 0010 (flavors, silent Console WRITE, hygiene probe), 0011 (lints), 0012 (AGENTS.md), 0014 (`V1` profile, AHCI fixture), 0015a (SMBIOS config, `package.ps1`, `mirror-image`), 0017a, 0017b and 0017c (the PS/2 and vmmouse paths the input trace hooks; see Open question 7); 0013 only for acceptance 6. Because it builds on 0017c, it lands last in the M0 sequence. Consumers: 0018's Mirror rows, 0027a (which keeps D6's `frame` and `shell_ready` hooks when it replaces the present and input syscalls, ledger S6f), 0045, C2's placement policy (post-v1).
Ledger: S7b, C3, G7 (related: S1, S1a, S6f, S7, S7a, C1, C2, C4). Implements 0018-D9 and 0010-D4's exception.

## Goal

Every kernel build, in every flavor, carries the perf instrumentation, and a boot without a `mirror.perf=` token prints none of it. With a valid token, the kernel prints content-free `MIRROR_PERF_*` serial lines: boot milestones on the HPET timeline, from kernel entry to the shell's first complete frame; 0018-D6 idle-memory samples; 0018-D7 input-to-present timestamps; 0018's flip calibration; and C3 telemetry, versioned records of measured devices and links (memory copy and AHCI reads now, the NIC later). `run.ps1`, `capture.ps1` and `package.ps1` pass tokens with `-Perf`, and the scripts check each family and prove that boots without a token print no `MIRROR_PERF_` line.

## Non-goals

- The host harness, gate methods and targets (0018, O3). The agent bench client (0045).
- A router or placement policy (C1, C2, C4), or any runtime use of telemetry.
- Persisting records: 0066 does that once the filesystem exists (ledger C3, amended 2026-10-09). NIC telemetry (0070). Multi-sector or queued reads (0061, 0062). VirtIO telemetry (S1).
- AP measurements. The kernel console fallback (`run_console`, `main.rs:457`), which draws without a present and is neither traced nor sampled. Changes to the boot-smoke battery.

## Decisions

| ID | Decision | Status |
| --- | --- | --- |
| 0019-D1 | Perf markers are compiled into every build. The heavier ones (memory samples, input trace, display calibration, agent bench) are switched on by `mirror.perf=` tokens, so daily and measured boots run the same kernel binary. No marker carries user content. Timing markers that depend on input appear only with a valid `mirror.perf=` token. (Reworded 2026-10-09 with ledger S7b: the last sentence replaces "In default builds, these opt-in markers are the only serial output that may depend on user input".) | confirmed (ledger S7b; 0018-D9) |
| 0019-D2 | v1 records device and link telemetry, so the placement policy has real data when unified capacity arrives. It builds no router. | confirmed (ledger C3) |
| 0019-D3 | Every `MIRROR_PERF_` line except D4's `MIRROR_PERF_OFF` needs a valid token, the boot milestones included, so a boot with no `mirror.perf=` token prints none. Rationale: the hygiene proof becomes a zero count, and 0018-D5 observes the boot gate from the host, so the milestones are diagnostics a perf run asks for. If chosen, 0018-D9 ("the heavier ones") and 0018's marker contract ("always on") change and need re-confirmation. Alternative: milestones always on, as 0018 says today. S7b allows it (they do not depend on user input), but every log then carries boot timings. | proposed |
| 0019-D4 | Token grammar as in Interfaces. An invalid token turns perf off for the boot and prints one `MIRROR_PERF_OFF reason=invalid`. Boot-smoke's strict parser still rejects it, because it skips a perf token only when the whole perf configuration is valid. Alternative: ignore unknown items and enable the known ones, which tolerates a newer harness but hides typos in measured runs. | proposed |
| 0019-D5 | Kernel entry is timed with the TSC, read by `perf::init` at the top of `_start`. The TSC and HPET are read together when `clock::init` publishes the clock (`clock.rs:71`), and again at the next milestone. The TSC rate between those pairs converts the entry-to-HPET-zero interval into `since_entry_ns`. The host arrival time of the first serial line (`main.rs:53`) stays 0018's cross-check. Alternative: no guest-side entry time, only that host arrival time. | proposed |
| 0019-D6 | `frame` and `shell_ready` come from syscalls the kernel already handles, not from console strings. `frame` is the first user process's first successful display-surface present. `shell_ready` is its first input syscall after `frame`, whatever the result: the shell and the agent-demo desktop both render a complete first frame before their first input poll. Both survive 0010's removal of `Mirror shell: ready` and `MIRROR_SHELL_FRAME`. Alternative: a frame-complete flag on present, or a ready syscall, which designs ABI ahead of 0020. | proposed |
| 0019-D7 | Input trace. An input starts at a key press, or a mouse packet that changes the button state, after the last traced present, stamped at the IRQ that delivered its first byte. It ends at the next display-surface present. Lines wait in a fixed 64-entry ring and are written at the scheduler's idle point. Serial writes are polled with IF clear (`serial.rs:51-55`), so no write sits between a traced input and its present (an input arriving during a write waits, which 0018's host-side number includes). Alternative: write each line at the present, which delays the rest of that frame's tiles. | proposed |
| 0019-D8 | Telemetry records use the perf line format: `MIRROR_PERF_DEV` is a device (a node of C2's map), `MIRROR_PERF_TEL` a measurement from `src` to `dst` (an edge). Each carries `v=1`, which changes whenever a field is added, removed or renamed. The same lines become the file format after M5. Alternative: a fixed binary little-endian record, like `crates/vm-protocol`'s VmStats, hex-encoded on serial: compact, but unreadable without a decoder. | proposed |
| 0019-D9 | M0 benchmarks run only under `telemetry` and only with HPET, on the BSP, before SMP. Memory copy is measured at a cache-sized span and a DRAM-sized span. AHCI reads are depth 1 and 512 bytes, sequential and random, read-only and time-capped. `read_sector` (`ahci.rs:364`) reads one sector at a time, so storage bandwidth is a depth-1, 512-byte figure until 0061 and 0062. Alternative: record only memory in M0, and start storage telemetry once the block layer has multi-sector queued reads. | proposed |
| 0019-D10 | M0 output is serial only: the kernel keeps no records, and the host saves captured lines under `docs/perf/telemetry/`. After M5, a follow-up spec writes the same lines to a file. Alternative: also keep the last 64 records in a kernel ring shown by a console `perf` command, which helps a VM without a serial file at the cost of the ring and a console page. (Note 2026-10-09: amended ledger C3 now says serial capture during perf runs before 0066 and persistence to the pool in 0066, which may settle this; Cameron confirms it with the rest of 0019.) | proposed |
| 0019-D11 | The grammar, names, line formatter and statistics live in a new `no_std` crate, `crates/perf-protocol`, with host tests. 0018's host tools may reuse its parser. Alternative: everything in `kernel/src/perf.rs`, tested only by QEMU boots (the kernel has `test = false`, `kernel/Cargo.toml:11`). | proposed |
| 0019-D12 | Token delivery. QEMU: `run.ps1 -Perf <items>` and `capture.ps1 -Perf <items>` add `mirror.perf=<items>` to 0015-D7's SMBIOS config. VMware: `package.ps1 -Perf <items>` writes a perf image under `build/dist/perf/`, with the token in `limine.conf`, its own fixed CID, and a kernel and modules byte-identical to the default image's. Alternative: a second Limine menu entry chosen over VNC, which keeps one image but adds a timed menu to every boot (today `timeout: 0`). (Reworded 2026-10-10 with 0015-D9's amendment: "disk UUID" became "CID", the descriptor identity VMware checks.) | proposed |

## Interfaces

Line numbers are at HEAD 4614249. The rest of the M0 sequence lands first and shifts some of them; 0016a also moves the rest of `_start` into a continuation on the BSP's guarded stack, after `perf::init` has run. If confirmed, D5 (the `clock` event), D6 (the `frame` event) and D7 (the `irq=` field) extend 0018's marker contract as well, and 0018 is updated to match.

### Token grammar (D4)

```
perf-token = "mirror.perf=" item *("," item)
item       = "boot" / "1" / "flip" / "telemetry" / "agent"
```

- **Lexical rules.** Tokens split on ASCII whitespace, as in `boot.rs:126`. Matching is exact and case-sensitive. At most one perf token, no empty or repeated items, at most 64 bytes.
- **Items.** Any valid token enables the milestones. `boot`: milestones only. `1`: memory samples and the input trace (0018's `mirror.perf=1`). `flip`: flip calibration. `telemetry`: C3 records. `agent`: reserved for 0045; until then the kernel prints `MIRROR_PERF_SKIPPED item=agent reason=unavailable`.
- **Parsing.** `perf::init()` runs right after `arch::disable_interrupts()` in `_start` (`main.rs:51`), before `serial::init`. It reads the TSC, then the Limine command line without `command_line()`'s `expect` (`boot.rs:137-143`); it never panics, and a missing command line means perf is off. The result is stored once in a static, and each perf site tests it with one branch.
- **Existing parsers.** `legacy_pic()` and `pit_timer()` (`boot.rs:124-135`) are unchanged. `smoke_mode()`'s filter (`boot.rs:171-175`) also skips a `mirror.perf=` token when `perf::config()` is valid, so it composes with one fault selector; an invalid or second perf token still panics with `unknown smoke-test command line`.

### Line format

- Each line is `MIRROR_PERF_<FAMILY>` followed by space-separated `name=value` fields. A value is a decimal `u64`, `none`, or an identifier this spec names, compiled into the kernel. Lines that can follow user input (`INPUT`, `MEM`) hold only decimals and `none` (0010 acceptance 3.6).
- `ns` is HPET time since `clock::init`, from `clock::snapshot` (`clock.rs:295-316`), or `none` after `MIRROR_CLOCK_SKIPPED`. Every timestamp and every emitter runs on the BSP with IF clear.

| Line | Needs | When |
| --- | --- | --- |
| `MIRROR_PERF_ON v=1 mem= input= flip= telemetry= agent=` (each `0` or `1`) | valid token | once, after the banner (`main.rs:53`) |
| `MIRROR_PERF_OFF reason=invalid` | invalid token | once, after the banner |
| `MIRROR_PERF_T event= ns=` | valid token | milestones, below |
| `MIRROR_PERF_MEM ns= available_kib= pool_kib=` | `1` | at `userspace`, then every 10 s |
| `MIRROR_PERF_INPUT seq= irq= irq_ns= present_ns=` | `1` | per traced input (D7) |
| `MIRROR_PERF_FLIP seq= ns=` | `flip` | 100 times, before the first user process |
| `MIRROR_PERF_DEV`, `MIRROR_PERF_TEL` | `telemetry` | below |
| `MIRROR_PERF_SKIPPED item= reason=` | `agent`; `flip` or `telemetry` without HPET | once per item, `reason=unavailable` or `no_clock` |

### Milestones (D5, D6)

Each `MIRROR_PERF_T` event prints at most once, in this order. A milestone that never happens is not printed: `clock` without HPET, `display` without a framebuffer, `frame` and `shell_ready` without a shell.

1. `clock ns=0 since_entry_ns=<n|none>`: HPET zero (`clock.rs:228`), printed just before the next milestone, which supplies D5's second TSC/HPET pair. `since_entry_ns` is `none` without a TSC (CPUID.01H:EDX[4]).
2. `display`: at `MIRROR_FRAMEBUFFER_READY` (`main.rs:160`).
3. `input`: after PS/2 init and IRQ enabling (`main.rs:253-279`).
4. `userspace`: the kernel first resumes the shell, or the agent-demo desktop. In default and trace builds this is the first user process (0010-D3).
5. `frame`: that process's first successful display-surface present (`userspace.rs:2645-2650`).
6. `shell_ready`: that process's first input syscall after `frame` (15 or 16 on the Input object, `userspace.rs:3070-3096`). Shell: `shell.rs:867-871`. Desktop: `userspace/desktop/src/main.rs:332-342`; its input checks at `:151-174` precede `frame` and do not count.

### Memory samples (0018-D6)

`available_kib` is 4 × (recycled + never-allocated frames) and `pool_kib` is 4 × total frames, both from `frames.stats()`, the sums `MIRROR_FRAME_POOL` prints (`main.rs:125-132`); a failed read prints `none`. Samples are taken in the `run_shell` and `run_desktop` scheduler loops (`userspace.rs:1914`, `1170`): on the first pass after `userspace`, then on the first pass after each further 10 s of HPET time (1000 ticks and `ns=none` without HPET).

### Input trace (D7)

- **Start**, when no traced input is pending: a key press (break codes start nothing), or a mouse packet whose decoded buttons differ from the previous packet's (pointer motion is not traced, 0018-D7). The time is taken on entry to the `ps2::handle_irq` call (`interrupts.rs:877`) that drained the event's first byte. `irq=` is the source device's IRQ, 1 or 12.
- **End.** The next successful display-surface present sets `present_ns`.
- **Output.** A static ring, written just before each idle `sti; hlt` (`userspace.rs:1215`, `1986`). `seq` counts every traced input, so a full ring shows up as gaps. 0084 keeps the line.

### Flip calibration (0018)

After the `after_userspace` heap growth (`main.rs:311-317`) and before the first user process: 100 full-screen fills through the framebuffer driver, alternating black and white, 500 ms apart on the HPET. Each is followed by `MIRROR_PERF_FLIP seq= ns=`, with `ns` read after the fill's last store and a store fence. IF is clear here, so ticks do not advance; without HPET, flip prints `SKIPPED item=flip reason=no_clock`.

### Telemetry (C3; D8-D10)

- `MIRROR_PERF_DEV v=1 dev=<n> class=<ram|block> ...`
  - `ram` is always `dev=0`. Its `bytes=` is the usable memory from the memory map (`main.rs:62-66`).
  - `block` is the AHCI disk, `dev=1`, adding `driver=ahci-sata` (the catalog name, `drivers.rs:21`) `bdf=<bus*256+device*8+function> port= sata_gen= sector_bytes= sectors=`. `sata_gen` is PxSSTS.SPD, bits 7:4, which `ahci.rs:240` masks out today, or `none` when it reads 0.
- `MIRROR_PERF_TEL v=1 src= dst= op=<copy|read> pattern=<seq|rand> request_bytes= depth= span_bytes= count= elapsed_ns= min_ns= p50_ns= p95_ns= max_ns=`
  - Copies are `src=0 dst=0`; reads are `src=1 dst=0`. Throughput is count × request_bytes / elapsed_ns. Copies are too short to time singly, so their per-request fields are `none`. Consumers reject an unknown `v`.
- **Memory benchmark**, just before `probe_ahci_device` (`main.rs:217`), with one CPU running. It copies 4 KiB pages through the direct map from source to destination frames, allocated one at a time and listed in the source frames themselves, so it uses no heap; all are freed afterwards. Two spans: 32 KiB, and min(64 MiB, free memory / 8). Each span gets one untimed warm-up pass, then is copied repeatedly until 256 MiB have been copied or 1 s has passed.
- **Storage benchmark**, in the AHCI probe after its existing reads succeed and before shutdown (`main.rs:861-872`): 256 sequential reads from LBA 0, then 256 reads at fixed-seed xorshift64 LBAs over the capacity, each timed. Each pattern stops at 2 s, and `count` records the reads done. Reads go to a stack buffer and allocate no frame, so the DMA accounting assert (`main.rs:877-881`) holds.
- **Without HPET**, both benchmarks print `SKIPPED item=telemetry reason=no_clock`; DEV records still print. Without `telemetry`, each site costs one branch.

### Files

- **`crates/perf-protocol`** (`mirror-perf-protocol`, D11): `#![no_std]` and `#![forbid(unsafe_code)]` (0011-D6), `[lints] workspace = true` (0011-D5), no dependencies. It holds the token parser, family and identifier names, a formatter for each line into a fixed buffer, min/p50/p95/max over a slice, and a line parser for tests and host tools. It joins the root `members` (`Cargo.toml:2`) and the host-test command (`test.ps1:39`).
- **`kernel/src/perf.rs`**: config, milestone hooks, input ring, sampler, flip and benchmarks. Hooks: `main.rs` (milestone, benchmark and flip sites), `clock.rs:71` (the TSC/HPET pair), `ahci.rs` (SPD and BDF getters), `framebuffer.rs` (a full-screen fill), `ps2.rs`, `keyboard.rs` and `mouse.rs` (start stamps), `userspace.rs` (present, input syscall, idle point, sampler), `boot.rs` (smoke filter, non-panicking command-line read).
- **Scripts (D12):**
  - A `-Perf` capture boots the SMBIOS config instead of the image's embedded one. `run.ps1` checks `-Perf` against D4's grammar; under `-Smoke`, an invalid list expects `MIRROR_PERF_OFF`, the `unknown smoke-test command line` panic and exit 35.
  - `package.ps1 -Perf` stages a copy of `build/esp` with the token in `limine.conf`, plus `resolution: 1920x1080` for 0018's reference display (0018-D3, ledger S1c; the mechanism 0015a confirms, 2026-10-09), and runs `mirror-image` with a second fixed descriptor `CID` (a new option; the default stays 0015-D9's).
  - `common.ps1` adds `Assert-MirrorPerfMarkers -Items` and `Assert-MirrorNoPerf`. `AGENTS.md` gains the grammar and `-Perf`.

## Acceptance tests

1. **Host tests.** `test.ps1:39` adds `-p mirror-perf-protocol`.
   - Grammar: accepts `boot`, `1`, `1,flip`, `telemetry,boot` and `agent`, and ignores other tokens (`mirror.test=page-fault`, `mirror.irq=pic`). Rejects `mirror.perf=`, `1,`, `,1`, `BOOT`, `1,1`, `2`, two perf tokens, a 65-byte token and a non-ASCII byte.
   - Lines: every family round-trips through format and parse, every value fits the vocabulary, and INPUT and MEM lines hold only decimals and `none`.
   - Statistics on known vectors: odd and even lengths, a single sample, all values equal.
2. **Off by default.** `Assert-MirrorNoPerf` requires zero lines starting with `MIRROR_PERF_` in every smoke and capture log without `-Perf`, including 0010's `-Hygiene` probe, in debug and release, with and without `-AgentDemo`. Existing marker sets and counts are unchanged.
3. **Hygiene with perf** (0010 acceptance 3.6). In `test.ps1`, after 0010's probe, `capture.ps1 -NoBuild -Hygiene -CpuCount 4 -Perf 1`, debug and release, requires:
   - one `MIRROR_PERF_ON v=1 mem=1 input=1 flip=0 telemetry=0 agent=0` and no `MIRROR_PERF_OFF`;
   - the six milestones, once each and in order, with numeric `ns` that never decreases from `clock ns=0`;
   - MEM lines with `available_kib` ≤ `pool_kib`, consecutive ones at least 10 s apart by `ns`;
   - after L0 and L1, only INPUT and MEM lines, all decimal;
   - at least one `irq=1` INPUT line after L0, no `irq=12` line after L0, and every INPUT line with `irq_ns` ≤ `present_ns` and `seq` strictly increasing;
   - 0010's step 5 still holding.
4. **Smoke composition** (`test.ps1`, debug and release, 6 boots).
   - `run.ps1 -Smoke -Perf boot,1,telemetry` passes every existing smoke assertion and also requires: the ON line; the milestones `clock` through `userspace`, in order; at least one MEM line; `DEV dev=0 class=ram` and one `class=block driver=ahci-sata` record with `sectors=2048 sector_bytes=512`; two `op=copy` and two `op=read` TEL lines, each with `count` ≥ 1 and numeric times; an unchanged `build/ahci-test.img` hash.
   - `run.ps1 -Smoke -Fault PageFault -Perf boot` exits 35 with the existing page-fault diagnostic.
   - `run.ps1 -Smoke -Perf bogus` prints one `MIRROR_PERF_OFF reason=invalid`, then exits 35 with `unknown smoke-test command line`.
5. **Flip, one-off** (in the commit message, not in `test.ps1`). `run.ps1 -Smoke -Perf flip -TimeoutSeconds 120` prints 100 FLIP lines, with `seq` 1-100 and `ns` at least 500 ms apart.
6. **VMware, manual**, after 0013 and 0015a, on the 0018-D3 VM (or `docs/vm-settings.md` settings until it exists). Run every step twice, with a debug image and a release image (`-Release`), and label each result with its profile, per 0018-D11.
   - `package.ps1 -Perf boot,telemetry` passes `mirror-image verify`, and its `kernel.elf` and modules hash-equal those in `build/esp`.
   - Its serial file has the ON line (on the 0018-D3 VM, whose serial port is a named pipe, the serial file is the log that `perf-probe serial` writes); the six milestones, with numeric `ns` and `since_entry_ns`; `DEV class=ram` and `DEV class=block driver=ahci-sata`; two copy and two read TEL lines. The commit message records `sata_gen` (Open question 1), and for each file the commit, profile and features, by hand until build identity exists (0067). Save the files as `docs/perf/telemetry/<date>-vmware-<debug|release>.log`.
   - The default `package.ps1` image prints zero `MIRROR_PERF_` lines.
7. **Clippy** passes with `-D warnings` in the default, `test-trace` and `--all-features` configurations (0010 acceptance 5).

## Dependencies

None. `crates/perf-protocol` is in-tree, with no external crate.

## Open questions

External facts, checked in acceptance 6:
1. Does VMware's AHCI port report PxSSTS.SPD? If not, `sata_gen=none` there. Static evidence (26.0.1, 2026-10-10): VMware advertises CAP.ISS = 3 (the `ahci.iss` default), so expect `sata_gen=3`; `none` means SPD read 0.
2. Is VMware's virtual TSC steady through early boot? If not, `since_entry_ns` is unreliable there, and D5's alternative applies on VMware. Under Host VBS mode (the user-level monitor, which the development host runs: `Monitor Mode: ULM`, scratch power-on 2026-10-10), also record the vmware.log `Monitor Mode:` line and the `TimeTrackerULMStats` `ulmTrapRDTSC` count, and compare `since_entry_ns` with the 0018 serial-arrival cross-check. In that mode VMware can trap RDTSC and serve an emulated linear TSC, and it always traps when syncing the WHP TSCs fails; the scratch power-on logged `Syncing WHP TSCs took 11 us`, a successful sync.

Follow-ups, outside this spec:

3. After M5 (0064, 0066): records written to a file, plus mirrored-pool member records (G8b). 0070: `class=nic` records. 0061 and 0062: `op=write`, multi-sector and queued reads in the TEL format.
4. 0045: the bench client behind `agent`, and a path for its numeric lines to reach serial, since Console WRITE prints nothing in default builds (0010-D4).
5. 0018: a `perf-probe telemetry` subcommand that collects TEL lines into `docs/perf/telemetry/`.

Design questions, for Cameron with D3-D12:

6. Flip calibration in boot-smoke builds. The Flip calibration section runs it after the `after_userspace` heap growth (`main.rs:311-317`) and before the first user process. In default and trace builds the shell is that process (0010-D3), so both hold. In boot-smoke builds init's battery runs first, from `userspace::run_init` at `main.rs:303`, before the `after_userspace` rendezvous (`main.rs:311`), so no point satisfies both, and acceptance 5 runs exactly that build. Options: in smoke builds, run flip before `run_init`, ahead of that heap growth; or after the heap growth and before the shell, after init's battery has run.
7. Clicks under 0017c (added 2026-10-09). 0017c's vmmouse is active by default on q35 and VMware, and 0018-D12 drives Mirror's clicks through it. While it is active, PS/2 packets are doorbells whose buttons are discarded and button changes arrive in vmmouse DATA packets (0017-D13, confirmed 2026-10-09), so D7's start rule ("a mouse packet whose decoded buttons differ", Input trace) never fires for a click, and 0018's Mirror click rows would get no INPUT lines. Proposed with D7: a click also starts a trace when a drained vmmouse event's buttons differ from the previous event's; the stamp is still taken on entry to the IRQ 12 `ps2::handle_irq` call that drained it, `irq=12`; and `vmmouse.rs` joins the start-stamp hooks under Files.
