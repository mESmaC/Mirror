extern crate std;

use super::*;
use crate::fixed::{self, ONE};
use crate::paint::GradientStop;
use crate::shadow::{SHADOW_1_DARK, SHADOW_2_DARK, SHADOW_3_DARK, Shadow};
use crate::shadow::{SHADOW_1_LIGHT, SHADOW_2_LIGHT, SHADOW_3_LIGHT};
use crate::tokens::{GLASS_DARK, GLASS_LIGHT, RING_DARK, RING_LIGHT};
use crate::tokens::{WALLPAPER_DARK, WALLPAPER_LIGHT};
use mirror_ui_tokens::DARK;
use std::string::{String, ToString};
use std::vec;
use std::vec::Vec;

/// The page the alpha tokens are copied from (0027c-D5, D6, D10).
const DESIGN: &str = include_str!("../../../docs/design/design-system.html");

// ---------- color ----------

#[test]
fn premultiply_rounds() {
    let c = Rgba::from_rgb(
        mirror_ui_tokens::Rgb {
            r: 200,
            g: 100,
            b: 50,
        },
        128,
    );
    assert_eq!(
        c,
        Rgba {
            r: 100,
            g: 50,
            b: 25,
            a: 128
        }
    );
}

#[test]
fn source_over() {
    let opaque = Rgba::from_rgb(DARK.accent, 255);
    let any = Rgba::from_rgba(12, 34, 56, 78);
    assert_eq!(opaque.over(any), opaque);
    assert_eq!(Rgba::TRANSPARENT.over(any), any);
    // Half-transparent white over opaque black.
    let half = Rgba::from_rgb(
        mirror_ui_tokens::Rgb {
            r: 255,
            g: 255,
            b: 255,
        },
        128,
    );
    let black = Rgba::from_rgb(DARK.base, 255);
    let out = half.over(black);
    assert_eq!(out.a, 255);
    let expected = 128u16 + (black.r as u16 * 127 + 127) / 255;
    assert_eq!(out.r, expected as u8);
}

// ---------- fixed ----------

#[test]
fn fixed_sqrt() {
    assert_eq!(fixed::sqrt32(4 << 32), 2 << 16);
    assert_eq!(fixed::sqrt32(0), 0);
    // sqrt(2) = 1.41421356 -> 1.4142 * 65536 = 92682.4
    let s = fixed::sqrt32(2 << 32);
    assert!((92680..=92684).contains(&s));
}

#[test]
fn fixed_mul_div() {
    assert_eq!(fixed::mul(ONE, ONE), ONE);
    assert_eq!(fixed::mul(fixed::px(3), fixed::px(3)), fixed::px(9));
    assert_eq!(fixed::div(fixed::px(9), fixed::px(3)), fixed::px(3));
    assert_eq!(fixed::div(ONE, 0), 0);
}

// ---------- paths and rasterization ----------

fn rounded(x: i32, y: i32, w: i32, h: i32, r: i32) -> Path {
    let mut b = PathBuilder::new();
    b.rounded_rect(
        fixed::px(x),
        fixed::px(y),
        fixed::px(w),
        fixed::px(h),
        fixed::px(r),
    );
    b.build()
}

#[test]
fn flatten_is_deterministic() {
    let mut b = PathBuilder::new();
    b.move_to(0, 0)
        .cubic_to(
            fixed::px(10),
            0,
            fixed::px(10),
            fixed::px(10),
            fixed::px(20),
            fixed::px(10),
        )
        .quad_to(fixed::px(30), fixed::px(10), fixed::px(40), 0)
        .close();
    let p = b.build();
    assert_eq!(p.flatten(), p.flatten());
}

#[test]
fn rasterize_rect_is_exact() {
    let mut b = PathBuilder::new();
    b.rect(fixed::px(2), fixed::px(2), fixed::px(4), fixed::px(4));
    let (mask, origin) = raster::rasterize(&b.build());
    assert_eq!(origin, (2, 2));
    assert_eq!((mask.width(), mask.height()), (4, 4));
    assert!(mask.data().iter().all(|&v| v == 255));
}

#[test]
fn rasterize_half_pixel_diagonal() {
    // The triangle (0,0)-(8,0)-(8,8) covers half the square: 32 px^2.
    let mut b = PathBuilder::new();
    b.move_to(0, 0)
        .line_to(fixed::px(8), 0)
        .line_to(fixed::px(8), fixed::px(8))
        .close();
    let (mask, _) = raster::rasterize(&b.build());
    let sum: u64 = mask.data().iter().map(|&v| v as u64).sum();
    // Exact area 32 px^2 = 8160; per-pixel quantization to 1/255 steps
    // drifts the sum by a few units along the diagonal.
    assert!(
        (8160 - 30..=8160 + 30).contains(&sum),
        "triangle area sum {sum}"
    );
    // The diagonal pixel (0,0) is half covered.
    assert!((120..=135).contains(&mask.get(0, 0)));
}

#[test]
fn rasterize_circle_area() {
    // Four quarter arcs, radius 20 at (30,30): area pi r^2 = 1256.6 px^2.
    let mut b = PathBuilder::new();
    let (c, r) = (fixed::px(30), fixed::px(20));
    const K: i32 = 36195;
    let k = fixed::mul(r, K);
    b.move_to(c + r, c)
        .cubic_to(c + r, c + k, c + k, c + r, c, c + r)
        .cubic_to(c - k, c + r, c - r, c + k, c - r, c)
        .cubic_to(c - r, c - k, c - k, c - r, c, c - r)
        .cubic_to(c + k, c - r, c + r, c - k, c + r, c)
        .close();
    let (mask, _) = raster::rasterize(&b.build());
    let sum: u64 = mask.data().iter().map(|&v| v as u64).sum();
    let area = sum as f64 / 255.0;
    let expect = core::f64::consts::PI * 20.0 * 20.0;
    assert!(
        (area - expect).abs() < expect * 0.02,
        "circle area {area} vs {expect}"
    );
}

#[test]
fn stroke_butt_rect_dimensions() {
    // A horizontal 20px line at y=5, width 4, butt caps: rows 3..7, 80 px^2.
    let mut b = PathBuilder::new();
    b.move_to(0, fixed::px(5))
        .line_to(fixed::px(20), fixed::px(5));
    let outline = raster::stroke_outline(&b.build(), fixed::px(4), raster::Cap::Butt);
    let (mask, _) = raster::rasterize(&outline);
    let sum: u64 = mask.data().iter().map(|&v| v as u64).sum();
    assert_eq!(sum, 80 * 255);
}

#[test]
fn stroke_round_cap_adds_semicircles() {
    let mut b = PathBuilder::new();
    b.move_to(fixed::px(10), fixed::px(10))
        .line_to(fixed::px(30), fixed::px(10));
    let outline = raster::stroke_outline(&b.build(), fixed::px(4), raster::Cap::Round);
    let (mask, _) = raster::rasterize(&outline);
    let sum: u64 = mask.data().iter().map(|&v| v as u64).sum();
    // 20x4 bar plus two semicircles of radius 2: 80 + pi*4 = 92.57 px^2.
    let area = sum as f64 / 255.0;
    assert!((90.0..=95.0).contains(&area), "round-cap area {area}");
}

#[test]
fn stroke_closed_ring_keeps_its_hole() {
    // A stroked 20x20 square at (10,10), width 4: the ring spans 8..32
    // outside and 12..28 inside (24*24 - 16*16 = 320 px^2), less the four
    // round-join corner savings of (4 - pi) px^2 each: about 316.6.
    let mut b = PathBuilder::new();
    b.rect(fixed::px(10), fixed::px(10), fixed::px(20), fixed::px(20));
    let outline = raster::stroke_outline(&b.build(), fixed::px(4), raster::Cap::Butt);
    let (mask, origin) = raster::rasterize(&outline);
    let sum: u64 = mask.data().iter().map(|&v| v as u64).sum();
    let area = sum as f64 / 255.0;
    assert!((308.0..=325.0).contains(&area), "ring area {area}");
    let cx = 20 - origin.0;
    let cy = 20 - origin.1;
    assert_eq!(mask.get(cx, cy), 0, "ring center stays transparent");
}

// ---------- blur and shadows ----------

#[test]
fn blur_approximately_conserves_energy() {
    let mut mask = Mask::new(100, 100);
    let (blob, origin) = raster::rasterize(&rounded(30, 30, 40, 40, 8));
    raster_blit_mask(&mut mask, &blob, origin.0, origin.1);
    let before: u64 = mask.data().iter().map(|&v| v as u64).sum();
    let mut probe = NoProbe;
    shadow::blur_mask(&mut mask, 8, &mut probe);
    let after: u64 = mask.data().iter().map(|&v| v as u64).sum();
    let ratio = after as f64 / before as f64;
    assert!(
        (0.95..=1.05).contains(&ratio),
        "blur energy before={before} after={after}"
    );
}

/// Stamp a mask into another at a position (tests only).
fn raster_blit_mask(dst: &mut Mask, src: &Mask, x: i32, y: i32) {
    for sy in 0..src.height() as i32 {
        for sx in 0..src.width() as i32 {
            let v = src.get(sx, sy);
            if v == 0 {
                continue;
            }
            dst.set(x + sx, y + sy, v);
        }
    }
}

#[test]
fn shadow_cache_hits_and_evicts() {
    let (mask, _) = raster::rasterize(&rounded(0, 0, 20, 20, 4));
    let mut cache = shadow::ShadowCache::new(1 << 20);
    let mut probe = NoProbe;
    let first = cache.blurred(&mask, 8, &mut probe).clone();
    let second = cache.blurred(&mask, 8, &mut probe).clone();
    assert_eq!(first, second);
    assert_eq!(cache.len(), 1);
    // A tiny budget evicts the old entry when a new one arrives.
    let mut cache = shadow::ShadowCache::new(first.data().len() + 1);
    cache.blurred(&mask, 8, &mut probe);
    let (other, _) = raster::rasterize(&rounded(0, 0, 24, 24, 4));
    cache.blurred(&other, 8, &mut probe);
    assert_eq!(cache.len(), 1);
}

// ---------- clip ----------

#[test]
fn clip_intersection() {
    let a = clip::Rect::new(0, 0, 10, 10);
    let b = clip::Rect::new(5, 5, 10, 10);
    assert_eq!(a.intersect(&b), Some(clip::Rect::new(5, 5, 5, 5)));
    assert_eq!(a.intersect(&clip::Rect::new(20, 20, 5, 5)), None);
    let mut c = Clip::none();
    c.and_rect(a);
    c.and_rect(b);
    assert_eq!(c.bounds(), Some(clip::Rect::new(5, 5, 5, 5)));
    assert_eq!(c.coverage(5, 5), 255);
    assert_eq!(c.coverage(4, 4), 0);
}

// ---------- blits and composition ----------

#[test]
fn bilinear_is_exact_at_pixel_centers() {
    let mut src = Surface::new(2, 2);
    src.set(0, 0, Rgba::from_rgba(255, 0, 0, 255));
    src.set(1, 0, Rgba::from_rgba(0, 255, 0, 255));
    src.set(0, 1, Rgba::from_rgba(0, 0, 255, 255));
    src.set(1, 1, Rgba::from_rgba(255, 255, 255, 255));
    let mut dst = Surface::new(4, 4);
    let mut probe = NoProbe;
    draw::blit(
        &mut dst,
        &src,
        clip::Rect::new(0, 0, 4, 4),
        draw::Filter::Bilinear,
        &Clip::none(),
        &mut probe,
    );
    // Corner pixels sample the source corners exactly.
    assert_eq!(dst.get(0, 0).r, 255);
    assert_eq!(dst.get(3, 0).g, 255);
    assert_eq!(dst.get(0, 3).b, 255);
}

#[test]
fn compose_respects_damage_and_layer_order() {
    let bottom = Surface::filled(8, 8, Rgba::from_rgb(DARK.base, 255));
    let mut top = Surface::new(8, 8);
    for y in 0..8 {
        for x in 0..8 {
            top.set(x, y, Rgba::from_rgb(DARK.accent, 255));
        }
    }
    let mut target = Surface::filled(8, 8, Rgba::from_rgba(1, 2, 3, 255));
    let layers = [
        compose::Layer {
            id: TRUSTED_TOP,
            surface: &top,
            dest: clip::Rect::new(2, 2, 4, 4),
        },
        compose::Layer {
            id: 0,
            surface: &bottom,
            dest: clip::Rect::new(0, 0, 8, 8),
        },
    ];
    let mut probe = NoProbe;
    compose::compose(
        &mut target,
        &layers,
        &[clip::Rect::new(0, 0, 4, 8)],
        &mut probe,
    );
    // Right half is outside the damage: untouched.
    assert_eq!(target.get(6, 6), Rgba::from_rgba(1, 2, 3, 255));
    // Inside damage: bottom repainted, top layer wins where it overlaps.
    assert_eq!(target.get(1, 1).g, DARK.base.g);
    assert_eq!(target.get(3, 3), Rgba::from_rgb(DARK.accent, 255));
}

// ---------- frame probe ----------

#[test]
fn probe_sees_paired_spans() {
    struct Log(Vec<(&'static str, bool)>);
    impl FrameProbe for Log {
        fn begin(&mut self, span: Span) {
            self.0.push(("begin", matches!(span, Span::Raster)));
        }
        fn end(&mut self, span: Span) {
            self.0.push(("end", matches!(span, Span::Raster)));
        }
    }
    let mut log = Log(Vec::new());
    let mut dst = Surface::new(16, 16);
    draw::fill_path(
        &mut dst,
        &rounded(2, 2, 8, 8, 2),
        &Paint::Solid(Rgba::from_rgb(DARK.accent, 255)),
        &Clip::none(),
        &mut log,
    );
    let pairs: Vec<_> = log.0.iter().map(|(k, _)| *k).collect();
    assert_eq!(pairs, ["begin", "end"]);
}

// ---------- gradient dither ----------

#[test]
fn dither_breaks_up_banding() {
    // A subtle 0->16 gray ramp over 256 px: the ordered dither perturbs
    // quantization by pixel phase, so two rows 5 apart (different Bayer
    // phase) disagree somewhere even though the gradient is vertical-only.
    let paint = Paint::Linear {
        x0: 0,
        y0: 0,
        x1: fixed::px(256),
        y1: 0,
        stops: vec![
            GradientStop {
                t: 0,
                color: Rgba::from_rgba(0, 0, 0, 255),
            },
            GradientStop {
                t: u16::MAX,
                color: Rgba::from_rgba(16, 16, 16, 255),
            },
        ],
    };
    let row = |y: u32| -> Vec<u8> {
        (0..256u32)
            .map(|x| {
                paint
                    .sample(
                        fixed::px(x as i32) + 32768,
                        fixed::px(y as i32) + 32768,
                        x,
                        y,
                    )
                    .r
            })
            .collect()
    };
    assert_ne!(row(0), row(5), "the Bayer phase differs between rows");
    // A solid paint never dithers.
    let solid = Paint::Solid(Rgba::from_rgba(10, 20, 30, 200));
    assert_eq!(solid.sample(0, 0, 0, 0), solid.sample(0, 0, 3, 7));
}

// ---------- design token values (0027c-D10) ----------

/// Extract a theme block: from `.t-dark {` (or light) to its closing brace.
fn theme_block(selector: &str) -> String {
    let start = DESIGN
        .find(selector)
        .unwrap_or_else(|| panic!("{selector} in design-system.html"));
    let open = DESIGN[start..].find('{').unwrap() + start;
    let close = DESIGN[open..].find('}').unwrap() + open;
    DESIGN[open..close].to_string()
}

/// The value of a --custom property inside a block.
fn property<'a>(block: &'a str, name: &str) -> &'a str {
    let key = std::format!("--{name}:");
    let start = block.find(&key).unwrap_or_else(|| panic!("{key}")) + key.len();
    let end = block[start..].find(';').unwrap() + start;
    block[start..end].trim()
}

/// Parse rgba(r, g, b, .aa) with the alpha in CSS decimal form.
fn parse_rgba(value: &str) -> (u8, u8, u8, u16) {
    let open = value.find("rgba(").expect("rgba(") + 5;
    let close = value[open..].find(')').unwrap() + open;
    let inner = &value[open..close];
    let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
    assert_eq!(parts.len(), 4, "rgba parts in {value}");
    let alpha_percent = {
        let a = parts[3];
        let decimal = a.strip_prefix('.').unwrap_or(a);
        // ".4" = 40%, ".45" = 45%: right-pad to two digits.
        let mut d = decimal.parse::<u16>().unwrap();
        for _ in decimal.len()..2 {
            d *= 10;
        }
        d
    };
    (
        parts[0].parse().unwrap(),
        parts[1].parse().unwrap(),
        parts[2].parse().unwrap(),
        alpha_percent,
    )
}

/// Round a CSS percentage alpha to 8 bits the way tokens.rs does.
fn expected_alpha(percent: u16) -> u8 {
    ((percent * 255 + 50) / 100) as u8
}

#[test]
fn ring_matches_css() {
    for (selector, token) in [(".t-dark", RING_DARK), (".t-light", RING_LIGHT)] {
        let (r, g, b, a) = parse_rgba(property(&theme_block(selector), "ring"));
        assert_eq!(
            token,
            Rgba::from_rgba(r, g, b, expected_alpha(a)),
            "{selector} ring"
        );
    }
}

#[test]
fn glass_matches_css() {
    for (selector, token) in [(".t-dark", GLASS_DARK), (".t-light", GLASS_LIGHT)] {
        let (r, g, b, a) = parse_rgba(property(&theme_block(selector), "glass"));
        assert_eq!(
            token,
            Rgba::from_rgba(r, g, b, expected_alpha(a)),
            "{selector} glass"
        );
    }
}

/// Parse one --shadow-N line into its Shadow list.
fn parse_shadows(value: &str) -> Vec<Shadow> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(pos) = rest.find("rgba(") {
        // The numbers before "rgba(" are "dx dyp blurp" (px-suffixed).
        let before = rest[..pos].trim().trim_matches(',').trim();
        let mut nums = before
            .split_whitespace()
            .filter_map(|t| {
                t.strip_suffix("px")
                    .or(Some(t).filter(|s| s.parse::<i32>().is_ok()))
            })
            .map(|t| t.parse::<i32>().unwrap());
        let (dx, dy, blur) = (
            nums.next().unwrap_or(0),
            nums.next().unwrap_or(0),
            nums.next().unwrap_or(0),
        );
        let (r, g, b, a) = parse_rgba(&rest[pos..]);
        out.push(Shadow {
            dx,
            dy,
            blur: blur as u32,
            color: Rgba::from_rgba(r, g, b, expected_alpha(a)),
        });
        let close = rest[pos..].find(')').unwrap() + pos;
        rest = &rest[close + 1..];
    }
    out
}

#[test]
fn shadows_match_css() {
    let cases: [(&str, &str, &[Shadow]); 6] = [
        (".t-dark", "shadow-1", SHADOW_1_DARK),
        (".t-dark", "shadow-2", SHADOW_2_DARK),
        (".t-dark", "shadow-3", SHADOW_3_DARK),
        (".t-light", "shadow-1", SHADOW_1_LIGHT),
        (".t-light", "shadow-2", SHADOW_2_LIGHT),
        (".t-light", "shadow-3", SHADOW_3_LIGHT),
    ];
    for (selector, name, expected) in cases {
        let parsed = parse_shadows(property(&theme_block(selector), name));
        assert_eq!(parsed, expected, "{selector} {name}");
    }
}

#[test]
fn wallpaper_matches_css() {
    for (selector, wallpaper) in [(".t-dark", WALLPAPER_DARK), (".t-light", WALLPAPER_LIGHT)] {
        let block = theme_block(selector);
        let value = property(&block, "wallpaper").replace("\n", " ");
        let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
        // Two radial glows then one linear base.
        let mut rest = value.as_str();
        for (i, &(gx, gy, rx, ry, color)) in wallpaper.glows.iter().enumerate() {
            let start = rest.find("radial-gradient(").expect("radial") + 16;
            let close = rest[start..].find(')').unwrap() + start;
            let body = &rest[start..close];
            // "1100px 620px at 10% -10%, #123A35 0%, transparent 55%"
            let at = body.find(" at ").unwrap();
            let mut radii = body[..at].split_whitespace();
            let css_rx: u32 = radii
                .next()
                .unwrap()
                .trim_end_matches("px")
                .parse()
                .unwrap();
            let css_ry: u32 = radii
                .next()
                .unwrap()
                .trim_end_matches("px")
                .parse()
                .unwrap();
            let after_at = &body[at + 4..];
            let comma = after_at.find(',').unwrap();
            let mut pos = after_at[..comma].split_whitespace();
            let css_gx: i32 = pos.next().unwrap().trim_end_matches('%').parse().unwrap();
            let css_gy: i32 = pos.next().unwrap().trim_end_matches('%').parse().unwrap();
            let hex_start = body.find('#').unwrap();
            let css_color = &body[hex_start..hex_start + 7];
            let parsed = parse_hex(css_color);
            assert_eq!((css_rx, css_ry), (rx, ry), "{selector} glow {i} radii");
            assert_eq!((css_gx, css_gy), (gx, gy), "{selector} glow {i} position");
            assert_eq!(
                (parsed.r, parsed.g, parsed.b),
                (color.r, color.g, color.b),
                "{selector} glow {i} color"
            );
            assert!(body.contains("transparent 55%"), "{selector} glow {i} fade");
            rest = &rest[close + 1..];
        }
        let start = rest.find("linear-gradient(").expect("linear") + 16;
        let close = rest[start..].find(')').unwrap() + start;
        let body = &rest[start..close];
        let deg: u32 = body
            .split(',')
            .next()
            .unwrap()
            .trim()
            .trim_end_matches("deg")
            .parse()
            .unwrap();
        assert_eq!(deg, wallpaper.base_degrees, "{selector} base angle");
        let mut hexes = body.match_indices('#');
        let (h0, h1) = (hexes.next().unwrap().0, hexes.next().unwrap().0);
        let c0 = parse_hex(&body[h0..h0 + 7]);
        let c1 = parse_hex(&body[h1..h1 + 7]);
        assert_eq!((c0.r, c0.g, c0.b), {
            let c = wallpaper.base_colors.0;
            (c.r, c.g, c.b)
        });
        assert_eq!((c1.r, c1.g, c1.b), {
            let c = wallpaper.base_colors.1;
            (c.r, c.g, c.b)
        });
    }
}

fn parse_hex(hex: &str) -> mirror_ui_tokens::Rgb {
    let v = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap();
    mirror_ui_tokens::Rgb {
        r: (v >> 16) as u8,
        g: (v >> 8) as u8,
        b: v as u8,
    }
}

// ---------- review regressions (0027c-4085cb1) ----------

#[test]
fn shadow_spreads_beyond_the_shape() {
    // P1: a solid rect's shadow must extend past the rect's bounds.
    let mut s = Surface::filled(200, 200, Rgba::opaque(DARK.base));
    let mut cache = shadow::ShadowCache::new(1 << 20);
    let mut probe = NoProbe;
    let rect = rounded(60, 60, 40, 40, 0);
    shadow::draw_shadow(
        &mut s,
        &rect,
        &[shadow::Shadow {
            dx: 0,
            dy: 8,
            blur: 12,
            color: Rgba::from_rgba(0, 0, 0, 128),
        }],
        &Clip::none(),
        &mut cache,
        &mut probe,
    );
    // Outside the shape's own bounds (which end at y=100): blurred tail.
    assert!(s.get(80, 106).a > 0, "shadow tail below the shape");
    assert!(s.get(56, 80).a > 0, "shadow tail left of the shape");
}

#[test]
fn nonzero_winding_does_not_cancel() {
    // P2: two half-pixel rects, one clockwise and one counterclockwise,
    // tile one pixel: full coverage, no cancellation.
    let mut b = PathBuilder::new();
    let h = fixed::ONE / 2;
    // Clockwise in y-down over [0, 0.5]x[0, 1].
    b.move_to(0, 0)
        .line_to(h, 0)
        .line_to(h, fixed::ONE)
        .line_to(0, fixed::ONE)
        .close();
    // Counterclockwise over [0.5, 1]x[0, 1].
    b.move_to(h, 0)
        .line_to(h, fixed::ONE)
        .line_to(fixed::ONE, fixed::ONE)
        .line_to(fixed::ONE, 0)
        .close();
    let (mask, _) = raster::rasterize(&b.build());
    assert_eq!(mask.get(0, 0), 255, "tiled half-rects cover fully");

    // Same-winding identical rects still count once.
    let mut b = PathBuilder::new();
    b.rect(0, 0, fixed::ONE, fixed::ONE);
    b.rect(0, 0, fixed::ONE, fixed::ONE);
    let (mask, _) = raster::rasterize(&b.build());
    assert_eq!(mask.get(0, 0), 255, "overlaps do not double-count");

    // A counterclockwise hole inside a clockwise rect stays empty.
    let mut b = PathBuilder::new();
    b.rect(0, 0, fixed::px(4), fixed::px(4));
    b.move_to(fixed::px(1), fixed::px(1))
        .line_to(fixed::px(1), fixed::px(3))
        .line_to(fixed::px(3), fixed::px(3))
        .line_to(fixed::px(3), fixed::px(1))
        .close();
    let (mask, _) = raster::rasterize(&b.build());
    assert_eq!(mask.get(2, 2), 0, "opposite-winding hole stays empty");
    assert_eq!(mask.get(0, 0), 255);
}

#[test]
fn collinear_excursions_survive_flattening() {
    // P3: a quadratic (0,0)-(20,0)-(10,0) reaches x = 13 1/3 at t = 2/3.
    let mut b = PathBuilder::new();
    b.move_to(0, 0).quad_to(fixed::px(20), 0, fixed::px(10), 0);
    let max_x = b
        .build()
        .flatten()
        .iter()
        .flat_map(|s| s.points.iter().map(|p| p.0))
        .max()
        .unwrap();
    assert!(
        max_x > fixed::px(13),
        "quad excursion kept: {} px",
        max_x as f64 / 65536.0
    );
    // A cubic (0,0)-(20,0)-(20,0)-(10,0) has the same kind of excursion.
    let mut b = PathBuilder::new();
    b.move_to(0, 0)
        .cubic_to(fixed::px(20), 0, fixed::px(20), 0, fixed::px(10), 0);
    let max_x = b
        .build()
        .flatten()
        .iter()
        .flat_map(|s| s.points.iter().map(|p| p.0))
        .max()
        .unwrap();
    assert!(
        max_x > fixed::px(12),
        "cubic excursion kept: {} px",
        max_x as f64 / 65536.0
    );
}

#[test]
fn dither_preserves_opaque_alpha_and_constants() {
    // P4: opaque stops keep alpha 255 at every Bayer phase, and a gradient
    // between two equal stops is exact everywhere.
    let opaque = Paint::Linear {
        x0: 0,
        y0: 0,
        x1: fixed::px(64),
        y1: 0,
        stops: vec![
            GradientStop {
                t: 0,
                color: Rgba::opaque(DARK.accent),
            },
            GradientStop {
                t: u16::MAX,
                color: Rgba::opaque(DARK.danger),
            },
        ],
    };
    for y in 0..8u32 {
        for x in 0..64u32 {
            let c = opaque.sample(
                fixed::px(x as i32) + 32768,
                fixed::px(y as i32) + 32768,
                x,
                y,
            );
            assert_eq!(c.a, 255, "opaque gradient alpha at phase ({x},{y})");
        }
    }
    let constant = Paint::Linear {
        x0: 0,
        y0: 0,
        x1: fixed::px(64),
        y1: 0,
        stops: vec![
            GradientStop {
                t: 0,
                color: Rgba::from_rgba(10, 100, 200, 90),
            },
            GradientStop {
                t: u16::MAX,
                color: Rgba::from_rgba(10, 100, 200, 90),
            },
        ],
    };
    for y in 0..8u32 {
        for x in 0..64u32 {
            let c = constant.sample(
                fixed::px(x as i32) + 32768,
                fixed::px(y as i32) + 32768,
                x,
                y,
            );
            assert_eq!(
                c,
                Rgba::from_rgba(10, 100, 200, 90),
                "constant gradient at phase ({x},{y})"
            );
        }
    }
}

#[test]
fn zero_blur_is_identity() {
    // P5: blur 0 changes nothing.
    let mut mask = Mask::new(3, 1);
    mask.set(1, 0, 255);
    let before = mask.clone();
    let mut probe = NoProbe;
    shadow::blur_mask(&mut mask, 0, &mut probe);
    assert_eq!(mask, before);
}

#[test]
fn wallpaper_glows_paint_first_listed_on_top() {
    // P6: paints() is back-to-front, so the second CSS glow comes before
    // the first: dark glows are (1100, 620) then (950, 640) in the CSS.
    let rect = clip::Rect::new(0, 0, 300, 100);
    let paints = WALLPAPER_DARK.paints(rect);
    assert_eq!(paints.len(), 3);
    let rx_of = |p: &Paint| match p {
        Paint::Radial { rx, .. } => *rx,
        _ => 0,
    };
    assert_eq!(rx_of(&paints[1]), fixed::px(950));
    assert_eq!(rx_of(&paints[2]), fixed::px(1100));
}

// ---------- golden scenes (0027c-D10) ----------

mod golden;
