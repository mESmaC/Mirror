# Mirror host lane

A host-only Rust 1.99.0 workspace with no third-party dependencies. It provides 0015c's package controller, Windows process containment, identity-based leases, durable records, bounded RFB/frame/QPC primitives and generic VM/staging controllers exercised with synthetic adapters.

See [lane packaging and recovery](../../docs/lane-recovery.md) for commands, provenance, lease publication and recovery. The benchmark CLI from 0018 is not included. No other-OS benchmark or VMware adapter is enabled here.

The root workspace excludes this tool. Use explicit `--manifest-path` and `--target x86_64-pc-windows-msvc` for host tests and clippy. The root `scripts/test.ps1` runs those checks and the PowerShell entry fixtures.

The `job_fixture` binary is a synthetic containment-test helper. It cannot operate a VM or a real team lease.
