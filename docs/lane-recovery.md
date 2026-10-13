# Host lane packaging and recovery (0015c)

The opt-in lane builds a tokenless default package from one clean Git commit. It does not start, provision or reconfigure a VM. VMware cases and VMware recovery stay disabled together pending 0015d's D11 investigation. Synthetic host tests are not VMware evidence or performance measurements.

Run from your own task checkout. Use a LaneRoot outside all Git worktrees and Cameron's checkout; reparse aliases and parent traversal are rejected. The controller acquires one of the shared `qemu`/`qemu2` slots itself. Optional `-LaneSlot qemu|qemu2` restricts acquisition to that exact slot, with no fallback; recovery must agree with its recorded slot. Do not acquire a manual slot first.

```powershell
.\scripts\package.ps1 -Lane -LaneRoot C:\Mirror-Lane -LockOwner codex/0015c -LimineArchive <existing-verified-limine-binary.zip>
.\scripts\package.ps1 -Lane -Release -LaneRoot C:\Mirror-Lane -LockOwner codex/0015c -LimineArchive <existing-verified-limine-binary.zip>
```

`-CargoHome` selects an existing config-free warm Cargo cache; otherwise the operator's `CARGO_HOME` or `%USERPROFILE%\.cargo` is used. The default Limine path is the calling checkout's `.tools/limine/v12.9.3/limine-binary.zip`. No dependencies or bootloader files are downloaded. External ancestor Cargo configuration is rejected; the exported project's own `.cargo/config.toml` stays intact. Workers receive a recorded environment allowlist, locked offline Cargo resolution and Rust 1.99.0. Source, compiler, flags and output overrides are not inherited.

Each package has a fresh `<LaneRoot>/packages/<package-id>` directory. Its durable journal records the operator, source checkout, fixed paths, controller PID/creation time, job identity, worker identities and command evidence. All export/build workers enter the controller's kill-on-close Windows job while suspended; the journal is flushed before they can execute. A final source inventory detects persistent changes to committed inputs; it is not hostile-process isolation.

The package manifest is published after both image files. A matching `final.json` with `finalized-valid`, verified hashes, cleanup completion and no live build is required before staging. An old manifest alone is insufficient. Ordinary and performance packaging invalidate their destination's old manifest before replacing image files and never create lane provenance.

## Recovery

Use the printed package ID and original LaneRoot from your own current checkout:

```powershell
.\scripts\package.ps1 -Lane -Recover <package-id> -LaneRoot C:\Mirror-Lane -LockOwner codex/0015c
```

Recovery checks the original operator/run/root, prior controller PID and creation time, every recorded worker and the bound job. An inaccessible identity, reused PID or live worker blocks cleanup and release. Age never authorizes takeover. Recovery does not depend on the old checkout to remove an interrupted scratch tree and never writes to another checkout. Revalidating an already valid package may read its recorded output; missing or changed publication cannot remain valid.

An incomplete package becomes `recovered-invalid`; recovery never resumes a build or republishes partial output. A previously durable valid verdict survives only after hash and no-live-build revalidation. Scratch deletion denial records the exact abandoned path. With no live build proved, the operation can release its lease; a failed normal cleanup never becomes valid. Preserve the journal and final record even when old bulk evidence is removed.

## Shared lease protocol

Unreleased directories remain busy for new work, even for the same operator. Recovery can hold an existing unreleased lane lease only for the exact recorded run/operator/root, through the exclusive owner handle. Absent or validly released generations always require a fresh lease. The owner file keeps the readable nick/task/worktree/start fields and adds versioned lane identity.

Release first persists an eligible final state and release intent. Through the still-held owner handle it appends the run/lease identity and final `released=<UTC>` line and flushes them. It then records `lock_release=owner-record` in the journal and final record before closing the handle. Normal package completion also attempts the identity-bound detachment described below and records `lock_release=detached` with its sibling path and identity. A denied rename leaves the durable owner-record release intact. Post `lock released` in xros_chat with the slot/run identity and any detached sibling. Never delete files at the shared acquisition pathname.

A later lane invocation can move a verified released generation to a unique non-existing sibling using the open directory's identity and a no-replace handle rename. It cannot rename a successor after a race. The detacher records the sibling's exact path and identity before trying to acquire a new generation. Rename denial leaves the released record intact and acquisition fails busy. Deletion of the detached sibling is optional; denial leaves a reported cleanup artifact and does not block the new lease. The detacher owns any later identity-checked cleanup of that sibling. A crash before durable release needs Recover; a crash after release never authorizes reusing that lease.

Existing manual scripts still acquire their slots as documented in AGENTS. Outside the lane tooling, only Claude performs the existing manual cleanup of an owner-released folder and posts `lock free`; other agents wait for that post. No tool may be used to bypass an action rejected by automatic approval review. An unreleased foreign owner is never adopted.

## Named replacement

The library's replacement recovery entry requires Cameron's explicit authorization naming the run, LaneRoot, original operator, replacement operator and scope. Its caller must verify that authorization before supplying the capability; the controller durably records both operators and the authorization reference before recovery actions. The lease preserves the original owner history and appends the current replacement operator/reference. Arbitrary command-line text is not authorization. The ordinary package entry only supports the recorded operator; a named replacement uses the verified native recovery API, not a general takeover switch.

Before enabling a replacement operation, record the authorization in xros_chat and LaneRoot and apply the same no-live-worker and finalization checks. This exception changes neither the prohibition on impersonation nor the ban on editing another agent's checkout.

## Host checks

```powershell
cargo fmt --all --manifest-path tools/perf-probe/Cargo.toml -- --check
cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc --manifest-path tools/perf-probe/Cargo.toml -- -D warnings
cargo test --locked --target x86_64-pc-windows-msvc --manifest-path tools/perf-probe/Cargo.toml
powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File scripts/test-lane.ps1
```

These host checks require no machine lease. Actual package acceptance and the full repository QEMU suite do. Keep scripts sequential within each checkout and at most two heavy runs on the machine.
