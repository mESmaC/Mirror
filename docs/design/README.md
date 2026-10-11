# Mirror design references

These five pages are Kimi's design concepts for Mirror's interface. Open them in a browser. Each one is a single self-contained HTML file that loads nothing from the network.

| File | What it shows |
| --- | --- |
| [design-system.html](design-system.html) | Tokens for both themes (color, type, spacing, radii, elevation, motion), icons, control states, usage rules |
| [boot-animation.html](boot-animation.html) | A boot sequence concept: assemble, trace, orbit, settle |
| [desktop-shell.html](desktop-shell.html) | Dock, start menu, command palette, notifications, quick settings, window interactions, and an implementation handoff |
| [apps.html](apps.html) | Files, Terminal and Settings, with navigation and their loading, empty and failure states |
| [accessibility.html](accessibility.html) | Keyboard model, focus rings, contrast, scaling, reduced motion, roles and semantics |

## Status: references, not contracts

The pages describe themselves as a contract, for example the token table "a shell implementation would consume". For Mirror they are design references (ledger P2a, 2026-10-10). A spec that builds UI cites the sections it uses and confirms the specific values it adopts in its own decision table. The ledger (`docs/specs/decisions.md`) stays authoritative: where a page and a confirmed decision disagree, the decision wins and the page is revised.

Font names such as Inter and JetBrains Mono appear only in CSS font stacks, so the pages fall back to the viewer's installed fonts. No font or icon file is redistributed here. Adopting a font is a decision for spec 0086 (S11, S3).

## Where each part lands

- The solid color tokens, spacing, focus ring, written error states and the type scale mapped onto today's 5x7 font went onto today's renderer through side spec 0029, with the dark theme as the default. 0029-D9's window-chrome geometry was withdrawn and moves to the window manager in Phase 4 of `docs/specs/roadmap-replan.md`, whose compositor and renderer replace today's renderer.
- Alpha, soft shadows, glass, motion and z-order layers belong to the userspace compositor, 0027a.
- Fonts, text layout and the icon rasterizer belong to 0086.
- The keyboard model and shortcuts belong to 0084, and roles and semantics to 0044 (element trees, A19).
- Two-seat focus and the agent cursor belong to 0047.
- The trusted approval prompt belongs to 0049.
- The terminal is 0082, the assistant panel is 0083, the editor is 0085, and Files and Settings are 0088.
- The clock is 0056, power controls are 0057 and 0058, and the theme and other UI preferences go in the settings store, 0068.

## Review notes for Kimi (2026-10-10)

These conflict with confirmed ledger decisions, so revise them in the next pass. None of them blocks anything (P2).

1. **Agent access.** Settings' "Request grant" to a broker that can be unreachable, and the global agent on/off switches, bypass the trusted approval path (A9a). Show the read-only and full-access presets (A5) and saved policies (A4, A21), editable only from the human seat. Live approvals happen only in 0049's trusted prompt.
2. **Trusted layer.** The z-order contract ends at tooltip 400 and says nothing escapes it. Reserve a compositor-owned layer above everything for the trusted prompt and the experiment-build marker (A9a, A10), styled so that an app cannot reproduce it pixel for pixel.
3. **Two seats.** The design has one focus and one cursor. v1 gives the human and the agent separate seats, each with its own cursor, focus and keyboard, and the agent cursor shows typed operations (A3, A3a). Design a distinct agent cursor and agent focus indicator.
4. **Power.** "Shutdown still lives in the kernel console" conflicts with S16. Mirror shuts down and reboots from the desktop (0057, 0058).
5. **Sessions.** "Apps are session-local; closing discards the document" describes today's demo, not v1. Files persist (G4, S2b), and tasks and conversations survive reboot (A18).
6. **Do-not-disturb.** Some notices must always get through: a full audit log refusing agent actions (A8a), a degraded storage pool (G8d) and approval requests (A9a).
7. **Boot.** The animation's progress is time-driven, and its settle step adds about 1.4 s to the measured boot (G7, 0018-D5). Use real phase-based progress and no added delay. v1 may use a static frame.
8. **Esc and Tab.** "Esc never exits" changes today's Esc-to-console, which 0018-D12 and the captures rely on. Spec 0084 decides the keyboard model.
9. **Contrast.** `border-strong` (about 1.6:1 in light and 2.0:1 in dark, on surface) and the soft `--ring` miss the pages' own 3:1 target for UI boundaries. Use the solid focus ring on every control, and darken `border-strong` to at least 3:1.
10. **Glass.** v1 surfaces are opaque. A backdrop blur would be CPU work in the composite path, which v1 has no GPU for (G5), and it would count against the latency and memory gates (G7). Revisit after 0027a is measured.
11. **Fixture copy.** Terminal's `fault` command and the fixture text ("claim 1af4:1001", "blkdebug", "runtime qemu tcg") are test-only (S7). Leave them out of the product concepts.

## Source

Kimi's branch is `design/design-system`, kept in Kimi's own clone. Six commits on top of dc78728 (112a1ba..16b33c8) were merged here on 2026-10-10.
