# 0027c: Renderer

Status: draft
Depends on: 0029 (mirror-ui-tokens)
Ledger: RD2 and RD3 (the NEW renderer spec), A9a (layer reservation only), P2a (the design pages are references)

## Goal

A `no_std` + `alloc` software drawing crate, `mirror-render` (`crates/render`), that can draw Mirror's design language: premultiplied RGBA surfaces, antialiased shapes on a shared path rasterizer, rounded rectangles, cached soft shadows, dithered gradients, clipping, filtered scaled blits and damage-limited composition. It carries the six alpha tokens 0029 could not represent (ring, shadow-1..3, glass, wallpaper), the frame-time hooks the Phase-4 frame-budget measurements need, and the reserved trusted-top layer id (RD2). 0086a's fonts and icons rasterize through the same path rasterizer. The crate is not wired into the kernel or shell yet — today's palette renderer keeps drawing the screen until 0027a and 0027e adopt this crate — so it lands with host tests that render scenes and compare them byte-exact against checked-in golden images.

## Non-goals

- No kernel, userspace or protocol changes, and no script changes beyond the `test.ps1` host-test line; nothing on screen changes in this spec.
- Text, glyph loading and icons (0086a consumes the rasterizer); easing and the frame clock (0027d); window management (0027e); the compositor protocol (0027a).
- Backdrop blur: design review note 10 (`docs/design/README.md`) defers glass blur until after the Phase-4 frame budget. `GLASS` ships as a plain translucent fill.
- GPU, SIMD/AVX, rotation and projective transforms: v1 blits are axis-aligned, scaled only.
- The light theme on screen (0029-D2): light values are defined and tested, dark only is drawn.

## Decisions

| ID | Decision | Status |
| --- | --- | --- |
| 0027c-D1 | New crate `crates/render`, package `mirror-render`: `#![no_std]`, `#![forbid(unsafe_code)]`, `alloc` only, workspace lints. Its one dependency is `mirror-ui-tokens`. Rationale: the compositor (userspace), the kernel crash screen and host tests all link it; forbidding `unsafe` keeps it auditable like the protocol crates. | proposed |
| 0027c-D2 | Pixels are premultiplied (alpha-associated) RGBA8, stored as R,G,B,A bytes. The only blend mode is source-over, with `(c * a + 127) / 255` rounding on every multiply. Straight-alpha input (CSS rgba, token + alpha) premultiplies once at the boundary. Rationale: premultiplied source-over is associative, so overlapping draws and layer composition need one code path; non-premultiplied pipelines double the blend cases. | proposed |
| 0027c-D3 | All rasterization and paint math is integer fixed point (16.16 geometry, 8-bit coverage); no `f32`/`f64` in the crate. Rationale: golden-image tests compare bytes exactly on any host, and pre-0053 targets pay no soft-float cost. Rejected: f32 rasterization (tiny-skia style), deterministic in practice on one architecture but not guaranteed, and slow under soft-float. | proposed |
| 0027c-D4 | The shared path rasterizer: `Path` (move/line/quadratic/cubic/close) flattened to segments at a fixed 1/16 px tolerance, scanline coverage accumulation into an 8-bit `Mask` with the nonzero winding rule. Strokes are expanded to outline paths and filled, with butt or round caps and round joins (the re-plan's "fills, strokes, round caps"). `rasterize` and `Mask` are public and need no features, so 0086a rasterizes glyph outlines through them unchanged. | proposed |
| 0027c-D5 | Soft shadows follow the CSS box-shadow model: a `Shadow` is (dx, dy, blur radius, color), drawn by rasterizing the shape to a mask, blurring it with three separable box-blur passes (integer accumulators), then painting the mask in the shadow color at the offset. A bounded LRU cache of blurred masks (keyed by shape hash, blur and size; fixed capacity in bytes) is the re-plan's "cached soft shadows". `SHADOW_1`, `SHADOW_2` and `SHADOW_3` for both themes are the CSS values of `design-system.html:53-55` and `:85-87`, each a list of one or two shadows. | proposed |
| 0027c-D6 | Gradients are linear or radial multi-stop, with stops interpolated in premultiplied space and an 8x8 Bayer ordered dither applied at paint time (the re-plan's "dithered gradients"). `WALLPAPER_DARK` and `WALLPAPER_LIGHT` are the three-layer values of `design-system.html:56-58` and `:88-90` (two radial glows over one linear base). | proposed |
| 0027c-D7 | Clipping is an intersect-only stack of axis-aligned rectangles, plus an optional A8 clip mask for path-shaped clips. Composition always clips to the damage list. Rejected: arbitrary clip-path stacks, which no design surface needs before the toolkit. | proposed |
| 0027c-D8 | Composition draws a back-to-front layer list into a target through bilinear fixed-point scaled blits, computing only pixels inside the caller's damage-rectangle list ("damage-limited composition"). Layer ids are a `u16` z-order; `layer::TRUSTED_TOP` (`u16::MAX`) is reserved above every app layer for the compositor-owned trusted prompt and experiment marker (RD2, A9a). This spec reserves the id and the ordering rule only; 0049 draws the prompt. | proposed |
| 0027c-D9 | Frame-time hooks: the crate owns no clock. Callers pass a `&mut dyn FrameProbe` (`begin(Span)` / `end(Span)`; `NoProbe` is the no-op default) around the spans `Raster`, `Blur`, `Blit` and `Compose`; the probe reads its own clock. The compositor wraps whole frames itself when it lands, and the Phase-4 budget spec measures through these hooks. | proposed |
| 0027c-D10 | Host tests compare rendered scenes byte-exact against golden images under `crates/render/golden/` (raw premultiplied RGBA8 with a 16-byte header). `MIRROR_RENDER_BLESS=1` regenerates goldens; on a mismatch the test writes a PNG dump of the actual pixels (test-only uncompressed-zlib writer) for review. Value tests parse `docs/design/design-system.html` for the shadow, ring, glass and wallpaper constants of both themes, the way `mirror-ui-tokens` tests parse the color blocks. | proposed |

## Interfaces

New crate `crates/render`:

- `color`: `Rgba` (premultiplied), `from_rgb(Rgb, alpha)`, `to_rgbx()`, source-over `over()`.
- `surface`: `Surface { width, height, pixels }`, clear and fill.
- `path`: `Path`, `PathBuilder` (move/line/quad/cubic/close), `rounded_rect`.
- `raster`: `Mask` (A8), `rasterize(&Path) -> Mask`, stroke expansion (caps, joins).
- `paint`: `Paint::{Solid, Linear, Radial}`, gradient stops, Bayer dither.
- `draw`: `fill_path`, `fill_rect`, `fill_rounded_rect`, `stroke`, `blit` (nearest and bilinear), all under a clip.
- `clip`: rectangle clip stack and optional mask clip.
- `shadow`: `Shadow`, `SHADOW_1..3` (both themes), `draw_shadow`, `ShadowCache`.
- `compose`: `Layer`, `layer::TRUSTED_TOP`, damage list, `compose`.
- `frame`: `FrameProbe`, `Span`, `NoProbe`.
- `tokens`: `RING_DARK/LIGHT` (accent at 32%), `GLASS_DARK/LIGHT`, `WALLPAPER_DARK/LIGHT`.

Edited files: workspace `Cargo.toml` (members), `Cargo.lock` (new package), `scripts/test.ps1` (host-test line and its failure text), `readme.md` (crate list and test line), `AGENTS.md` (the `forbid(unsafe_code)` crate list). No other script, kernel or userspace file changes.

## Acceptance tests

1. `cargo test --locked -p mirror-render --target x86_64-pc-windows-msvc` passes: blend-rounding, flattening determinism and bounds, nonzero winding, stroke caps and joins, blur energy conservation, cache hit and eviction, clip intersection, bilinear exactness at integer coordinates, damage-limited compose leaving pixels outside damage untouched, and a render-twice byte-equality determinism test; plus the D10 golden and CSS-value tests.
2. Golden scenes, dark theme: the wallpaper at 1280x800; a rounded window with `SHADOW_3`; a toast with `SHADOW_2`; a button with the 32% ring; a glass pill over wallpaper; a stroke gallery (round caps and joins); a linear-gradient dither case. Cameron reviews the PNG dumps.
3. `cargo fmt --all -- --check` and the three clippy profiles pass; the full `scripts/test.ps1` passes with `-p mirror-render` in its host-test line.
4. Blessing discipline: goldens change only with `MIRROR_RENDER_BLESS=1`, and a golden diff without a matching code change fails review.

## Dependencies

`mirror-ui-tokens` (first-party, no dependencies). No third-party crates.

## Open questions

1. RD2 reservation: is reserving `TRUSTED_TOP` and its ordering rule all 0027c does for the trusted layer, with 0049 drawing the prompt?
2. Are the `Raster`/`Blur`/`Blit`/`Compose` spans the right hook set for the Phase-4 frame budget, or should `Present`/`Upload` be defined now for the compositor?
