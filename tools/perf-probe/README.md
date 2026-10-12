# perf-probe work in progress

Host-only, std-only Rust primitives for spec 0018. On 2026-10-11 Cameron
deferred other-OS downloading and benchmarking until he explicitly requests it.
This work is preserved on `astra/0018-baseline-harness`; it is not a completed
baseline harness and is not approved for measurement publication.

Implemented: bounded RFB 3.8 with security None, Raw and DesktopSize; RGB PPM
frames and masks; QPC timestamps; strict CSV parsing; an explicitly selected
nearest-rank calculation; initial `snapshot` and `boot` commands. All protocol
fixtures are synthetic. No VMware capture or guest baseline has validated them.

Still outstanding: input, pixel-agent, calibrate, serial and summarize commands;
`scripts/perf.ps1`; host-check integration; VM setup and actual measurement
acceptance. The boot difference threshold and the measurement p95 convention
were sent to Claude for Cameron's decision before the benchmark deferral.
No threshold or percentile convention has been selected for gate results.

## Host checks

Run from the repository root:

```powershell
cargo fmt --all --manifest-path tools/perf-probe/Cargo.toml -- --check
cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc --manifest-path tools/perf-probe/Cargo.toml -- -D warnings
cargo test --locked --target x86_64-pc-windows-msvc --manifest-path tools/perf-probe/Cargo.toml
```

The tool pins the workspace's Rust 1.99 toolchain and uses no added crates.
Other-OS guest helper drafts are preserved under `tools/perf-guest`.

## Initial command contracts

All options are `--name value` pairs. Duplicate and unknown options are errors.
Output files must not already exist. A failed CSV retains its partial evidence
and lacks the final `run_state=complete` comment. Do not interpret such files
as successful measurements.

Both commands take `--address 127.0.0.1:5901`, `--os mirror|ubuntu|windows`
and optional `--hypervisor vmware|qemu` (default VMware). Only loopback VNC
connections are accepted. VMware desktop/reference frames must be 1920x1080.

- `snapshot`: requires `--output frame.ppm` and `--csv frame.csv`. Optional
  `--masks` is a semicolon-separated list of `x,y,width,height` rectangles.
  `--launcher`, `--document` and `--close` record `x,y` positions;
  `--response-rect` records a rectangle. `--pointer x,y` performs one absolute
  pointer move before capture; for Mirror it requires `--serial-log` containing
  `MIRROR_VMMOUSE_READY mode=absolute`. It never infers coordinates from pixels.
- `boot`: requires `--reference frame.ppm`, `--start-file start.txt`,
  `--output boot.csv`, `--end-frame end.ppm`, and explicit `--max-changed N`.
  The start file must be published atomically immediately before power-on and
  contain two decimal integers: absolute host QPC ticks and QPC frequency.
  The command does not start or stop the VM. Optional `--masks`,
  `--channel-tolerance` (0..255, default 0), `--timeout-seconds` (default 180)
  and `--phase warmup|measured` (default measured) are recorded/validated.
  A matching frame must be fully received, with all size changes tracked.

The numeric pixel threshold is an explicit input, not a confirmed benchmark
default. QEMU command results are tooling checks only, never gate measurements.
