# 0020: Shared native syscall and error table

Depends on: 0030a, 0050, 0051
Ledger: S8, S11, S14, S15

## Goal

The kernel and every native program use one allocation-free `no_std`
`mirror-sys` crate for syscall numbers and negative error results. Existing
programs retain their ABI. Native ABI strings are length-delimited UTF-8.

## Design in brief

- Define the existing syscall numbers 1 through 35 and every existing native
  error in `crates/sys`. Keep the current `u64` numbers and negative `i64`
  errors. The protocol crates re-export their old names for source compatibility.
- Reserve 36 (`SYS_REALTIME_GET`) for UTC clock reads and 37
  (`SYS_RANDOM_BYTES`) for kernel random bytes. They return `ENOSYS` without
  reading or writing user memory until 0056a and 0036 implement their contracts.
  Zero and unknown numbers also return `ENOSYS`.
- Keep the `int 0x80` entry: RAX is the number/result, and RDI, RSI, RDX are
  the three arguments. Ordinary calls preserve other general registers;
  successful thread join additionally returns RDX, and successful address wait
  returns RSI and RDX. No calling-convention or scheduling changes.
- Pass strings as an address and a byte count, without an implicit NUL
  terminator. Validate the complete range and complete UTF-8 sequence before
  acting. `WRITE` already enforces this; test multi-byte strings and malformed
  encodings. Binary packets and random output are byte arrays, not strings.
- Rust and assembly clients take constants from the shared table. The kernel
  uses the same constants in dispatch, copy preflight, and error translation.

No third-party dependencies or new runtime services.

## Tests

- Host tests freeze all existing syscall/error values, ensure the reserved
  calls do not collide, and exercise valid and invalid UTF-8 encodings.
- Init's smoke battery checks both reserved calls with invalid pointers and
  sentinel output, plus multi-byte/invalid UTF-8 writes. Existing register,
  thread, IPC and checked-copy probes continue to run.
- Run root formatting, all three Clippy configurations, affected protocol
  host tests, and the complete `scripts/test.ps1` debug/release suite including
  desktop captures and default-kernel hygiene before merge.
