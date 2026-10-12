# 0017a: PS/2 loss domains, mouse coalescing and resync

Status: confirmed (2026-10-09)
Depends on: 0013 and 0015a, only for the manual VMware check (it boots 0015a's disk image); the QEMU tests need neither. In the M0 order this spec lands before 0017b, and both edit `kernel/src/ps2.rs`. 0017c (absolute pointing) follows both.
Ledger: S1a, G1, G3a, G7 (related: S7b, S1d)

## Goal
A mouse fault or a full mouse ring never changes keyboard state: held modifiers, Caps Lock, the next keystroke and an unsent console line or Scratchpad draft all survive. A keyboard fault never discards pointer events. While the shell is starved, mouse motion is coalesced instead of overflowing the ring, and a mouse whose packet framing slipped resynchronizes after a 500 ms gap. This protects daily use (G1) and the input-latency gate (G7) on the v1 device set (S1a) in VMware (G3a). Init timing and order are 0017b.

## Non-goals
- Keyboard LED sync (0xED) and lock-state agreement with the VM: spec 0084. Mirror never sends 0xED today, so a keyboard loss, which still clears Caps Lock, can leave the VM's LED disagreeing until 0084. After D3 a mouse loss no longer does this.
- Keycodes, shortcuts, new keys, wheel and extra buttons, the window-protocol key event and a device-independent input layer (all 0084). Input provenance and seats (0047). USB HID is outside the v1 device set (S1a); USB HID pointing comes with physical hardware after v1 (S1d).
- Absolute pointing through the VMware vmmouse protocol: spec 0017c (ledger S1d). The relative PS/2 mouse this spec handles stays the fallback.
- Scheduler changes. The 32-of-100-tick service throttle (`kernel/src/userspace.rs:43-44`) belongs to 0052 (ledger S4).
- Init timeouts, the controller self-test, scan-set selection and init order (0017b).

## Decisions
| ID | Decision | Status |
| --- | --- | --- |
| 0017-D1 | v1 keyboard and pointer input is the PS/2 keyboard and mouse behind the i8042. (Amended 2026-10-09 with S1a: the mouse gains the VMware vmmouse absolute-pointer extension, spec 0017c; relative PS/2 stays the fallback.) | confirmed (ledger S1a) |
| 0017-D2 | VMware is checked by hand; QEMU runs the automated tests. (Amended 2026-10-09: VirtualBox dropped, G3a amended.) | confirmed (ledger G3a) |
| 0017-D3 | Loss is tracked per device. A keyboard loss resets only keyboard state (held keys, Caps Lock, recovery gate). A mouse loss resets only pointer state. Each loss is a marker in its own device's stream, ordered against the other device by the shared sequence. It never discards the other device's queued events. Rationale: a mouse-only loss drops no keyboard byte, and every KeyEvent already carries a full modifier snapshot. Rejected alternative: keep the global barrier (`kernel/src/ps2.rs:185-188`). | confirmed (2026-10-09) |
| 0017-D4 | The KIND_LOST record (kind 3) keeps its kind and carries a device mask in word 2: bit 0 keyboard, bit 1 pointer. The mask is nonzero and all other bits are zero. The session's initial record sets both bits. Rationale: one additive field in a record format that 0084 replaces. Rejected alternative: add a new kind 4 for pointer loss. | confirmed (2026-10-09) |
| 0017-D6 | When the 64-entry mouse ring is full, a packet with the same button levels as the newest queued event is added into it (checked i16 sum), and the merged event takes the new packet's sequence. A button change or an i16 overflow on a full ring is a pointer loss. Rationale: delivery stays 1:1 except under starvation, and starvation loses no motion. Rejected alternative: always merge same-button motion into an unconsumed tail. | confirmed (2026-10-09) |
| 0017-D7 | Mouse framing recovery: if a partial packet's previous byte arrived more than 500 ms (50 ticks) earlier, the partial packet is dropped silently and the new byte is treated as a candidate header (linux `psmouse-base.c:375-376` uses HZ/2). A motion-overflow packet stays a pointer loss followed by its zero-motion button snapshot, which consumers now receive because of 0017-D3. Rejected alternative: keep header-bit-3 framing only (`mouse.rs:68-70`). | confirmed (2026-10-09) |

## Interfaces
**Loss events** (`kernel/src/ps2.rs`, `keyboard.rs`, `mouse.rs`)
- `ps2::InputEvent::Lost` becomes `Lost { keyboard: bool, pointer: bool }`. `ps2::next_event` (ps2.rs:179-203) sets exactly one flag per marker.
- In each device state, `lost: Option<u64>` replaces `overflow: bool`. It holds the sequence that `ps2::next_sequence()` returned at the first loss since the consumer last took that device's marker. `peek_sequence` reports `lost` ahead of the queue head, and `next_event` merges both heads by sequence. The cross-device `discard_input` calls in `next_event` are removed; `discard_input` stays only for `State::stop_routing` (ps2.rs:106-112).
- Behaviour change: events a device queues after its own loss (for example after keyboard recovery completes) are delivered after its marker. Today's barrier discards them (ps2.rs:625-627).
- Keyboard: `Decoder::lose_input` (keyboard.rs:139-143) runs only on keyboard faults or keyboard ring overflow. The recovery gate (keyboard.rs:189-194) is unchanged.
- Mouse: `Decoder::discard_partial` (mouse.rs:108-116) runs only from `stop_routing`. Coalescing (D6) replaces the full-ring branch (mouse.rs:170-175). `State::accept` takes the current `timer::ticks()` value as a parameter for the gap check (D7), next to the header-bit-3 check (mouse.rs:68-70).

**Lost record** (`kernel/src/input.rs:12, 31, 97`): words are `[1, 3, mask, 0, 0, 0, 0, 0]` with mask 1 (keyboard), 2 (pointer) or 3 (initial session record only).

**Consumers**
- Kernel console (`kernel/src/main.rs:629-635`): a keyboard loss calls only `console.input_lost()` (console.rs:191-195); a pointer loss calls only `pointer.input_lost()` (pointer.rs:64-67). The marker becomes `MIRROR_CONSOLE_INPUT_LOST device=keyboard|pointer`, which `capture.ps1:207` and `:278` still match. It carries no user content and stays in every build flavor, as 0010 lists it (S7b).
- Desktop decoder (`userspace/desktop/src/main.rs:128-131`): returns `Event::Lost { keyboard, pointer }` and rejects mask 0 or any bit above bit 1. main.rs:175 expects the initial mask 3.
- Shell (`userspace/desktop/src/shell.rs:432-443`): a pointer loss resets capture, buttons and click arming, then sends Reset to each app. A keyboard loss sends apps nothing, because app Reset and Focus clear only pointer and focus state (`userspace/app/src/main.rs:211-220`). Behaviour change: the per-window `pending` reset (shell.rs:436-439) is removed, so a loss no longer drops events already queued for apps.
- Scratchpad UI (`userspace/desktop/src/ui.rs:185-191`): only a keyboard loss clears the draft and sets the notice. A pointer loss resets buttons and click arming.

**Files touched:** `kernel/src/{ps2,keyboard,mouse,input,main}.rs`, `userspace/desktop/src/{main,shell,ui}.rs`, `readme.md:349` (the global loss barrier becomes per-device loss and coalescing) and `readme.md:524` ("loss records clear every payload field" becomes the device mask).

## Acceptance tests
Boot-smoke self-tests; the existing markers must still print.
1. `ps2::dispatch_self_test` (MIRROR_PS2_TESTS_OK) replaces the global-barrier cases (ps2.rs:617-658):
   - With Shift held, Caps Lock on and a queued `a` make, run each mouse-only loss in turn: an AUX RECEIVE_ERROR byte, a motion-overflow packet, and a button change on a full ring. Expected order: Key(a), Lost{pointer}, then the button snapshot (overflow case only). The next 0x1c make is delivered from its first byte, with shift and caps_lock both true.
   - Keyboard RECEIVE_ERROR between two mouse packets: the order is mouse, Lost{keyboard}, mouse, with no packet dropped. The keyboard recovery gate still applies.
   - Both devices lost: two markers, in loss-sequence order.
2. `mouse::self_test` (MIRROR_MOUSE_TESTS_OK):
   - 64 + 1000 same-button packets with dx=1 and no consumer: no loss, 64 events, total dx 1064.
   - The merged event carries the newest packet's sequence, so a sequence allocated between two merged packets (a key) sorts before it.
   - A button change on a full ring, or an i16 overflow, produces a loss.
   - Header 0x09, then the test tick advanced by 51, then `08 05 00`: the result is dx=5, buttons 0. With an advance of 49, the bytes complete the stale packet `09 08 05` instead.
3. `input::self_test` (MIRROR_INPUT_TESTS_OK): keyboard, pointer and initial records encode to `[1,3,1,..]`, `[1,3,2,..]` and `[1,3,3,..]`.
4. Desktop smoke: shell.rs:1211 `policy_checks` (`run.ps1 -Smoke`, gated by MIRROR_SHELL_TESTS_OK) checks that a pointer-only loss keeps a queued key for the focused app. ui.rs:551 `self_test` (`-Smoke -AgentDemo`, gated by MIRROR_DESKTOP_TESTS_OK) checks that a pointer-only loss keeps the draft and a keyboard loss clears it, as ui.rs:597-600 already expects.
5. Before merge (ledger P8): `scripts/test.ps1` and `scripts/test.ps1 -AgentDemo` pass, since the Scratchpad is agent-demo code. This spec adds no smoke boots.

Manual check on VMware Workstation. The VM boots 0015a's disk image with the settings 0015a documents, and uses a PS/2 keyboard and a PS/2 mouse with no USB controller, so no USB mouse or tablet:
6. In the default shell, focus Notes, turn Caps Lock on and type `a`. Drag the other window by its title bar in fast circles for 10 s, release, and type `b` in Notes. Expected: Notes shows `AB` with nothing dropped, and the dragged window ends where the pointer was released.

## Dependencies
No crates. QEMU (`.tools/qemu`, GPL-2.0) is already used. The manual check needs VMware Workstation (proprietary, no cost), installed: Workstation Pro 26.0.1 (26H1u1) build 25688693 (2026-10-10).

## Open questions
1. Resolved 2026-10-09: absolute pointing is spec 0017c (ledger S1d), the VMware vmmouse protocol in M0, so the pointer is not grabbed. QEMU q35 enables `vmport` (and its vmmouse) by default, and `run.ps1:349` turns it off only for `-NoKeyboard`, so 0017c tests the protocol in QEMU.
2. Follow-up suggestion: move the keyboard and mouse decoders into a `no_std` crate with host cargo tests. This spec keeps boot-smoke tests only.
