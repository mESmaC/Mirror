//! Golden-image scene tests (0027c-D10): each scene renders byte-exact
//! against its checked-in `golden/<name>.rgba`. Regenerate with
//! `MIRROR_RENDER_BLESS=1`; on a mismatch the actual pixels dump to
//! `golden/actual-<name>.png` for review.

use super::*;
use crate::clip::Rect;
use crate::shadow::ShadowCache;
use crate::tokens::{GLASS_DARK, RING_DARK, WALLPAPER_DARK};
use std::vec;
use std::vec::Vec;

const BLESS: &str = "MIRROR_RENDER_BLESS";

fn golden_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("golden")
}

/// Golden format: 8-byte magic, then width and height as u32 LE, then
/// width*height premultiplied RGBA bytes.
fn encode(surface: &Surface) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"MRRGBA8\0");
    out.extend_from_slice(&surface.width().to_le_bytes());
    out.extend_from_slice(&surface.height().to_le_bytes());
    for p in surface.pixels() {
        out.extend_from_slice(&[p.r, p.g, p.b, p.a]);
    }
    out
}

fn decode(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    assert!(
        bytes.len() >= 16 && &bytes[..8] == b"MRRGBA8\0",
        "golden magic"
    );
    let w = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let h = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    assert_eq!(bytes.len() - 16, (w * h * 4) as usize, "golden size");
    (w, h, bytes[16..].to_vec())
}

fn check(name: &str, surface: &Surface) {
    let path = golden_dir().join(std::format!("{name}.rgba"));
    let actual = encode(surface);
    let blessed = std::env::var(BLESS).is_ok();
    match std::fs::read(&path) {
        Ok(expected) if expected == actual => {}
        Ok(expected) if !blessed => {
            let (w, h, _) = decode(&expected);
            assert_eq!(
                (w, h),
                (surface.width(), surface.height()),
                "{name}: golden dimensions differ; re-bless with {BLESS}=1"
            );
            let dump = golden_dir().join(std::format!("actual-{name}.png"));
            std::fs::write(&dump, png(surface)).unwrap();
            let diffs = expected
                .iter()
                .zip(actual.iter())
                .filter(|(a, b)| a != b)
                .count();
            panic!(
                "{name}: {diffs} bytes differ from the golden; actual pixels at {}",
                dump.display()
            );
        }
        _ => {
            std::fs::create_dir_all(golden_dir()).unwrap();
            std::fs::write(&path, &actual).unwrap();
            assert!(blessed, "{name}: golden written; rerun tests to verify");
        }
    }
}

// ---------- scenes ----------

fn scene_wallpaper() -> Surface {
    let mut s = Surface::new(1280, 800);
    let rect = Rect::new(0, 0, 1280, 800);
    for paint in WALLPAPER_DARK.paints(rect) {
        draw::fill_rect(&mut s, rect, &paint, &Clip::none());
    }
    s
}

fn scene_window_shadow() -> Surface {
    let mut s = Surface::filled(400, 300, Rgba::opaque(DARK.base));
    let mut probe = NoProbe;
    let mut cache = ShadowCache::new(1 << 20);
    let path = rounded(60, 50, 280, 180, 12);
    shadow::draw_shadow(
        &mut s,
        &path,
        SHADOW_3_DARK,
        &Clip::none(),
        &mut cache,
        &mut probe,
    );
    draw::fill_path(
        &mut s,
        &path,
        &Paint::Solid(Rgba::opaque(DARK.surface)),
        &Clip::none(),
        &mut probe,
    );
    draw::stroke(
        &mut s,
        &path,
        fixed::ONE,
        raster::Cap::Butt,
        &Paint::Solid(Rgba::opaque(DARK.border_strong)),
        &Clip::none(),
        &mut probe,
    );
    s
}

fn scene_toast_shadow() -> Surface {
    let mut s = Surface::filled(360, 120, Rgba::opaque(DARK.base));
    let mut probe = NoProbe;
    let mut cache = ShadowCache::new(1 << 20);
    let toast = rounded(25, 30, 310, 52, 10);
    shadow::draw_shadow(
        &mut s,
        &toast,
        SHADOW_2_DARK,
        &Clip::none(),
        &mut cache,
        &mut probe,
    );
    draw::fill_path(
        &mut s,
        &toast,
        &Paint::Solid(Rgba::opaque(DARK.elevated)),
        &Clip::none(),
        &mut probe,
    );
    // The 3 px accent bar and two text lines as rounded bars.
    draw::fill_rect(
        &mut s,
        Rect::new(25, 31, 3, 50),
        &Paint::Solid(Rgba::opaque(DARK.accent)),
        &Clip::rect(Rect::new(25, 30, 310, 52)),
    );
    draw::fill_rounded_rect(
        &mut s,
        Rect::new(44, 46, 120, 8),
        fixed::px(4),
        &Paint::Solid(Rgba::opaque(DARK.text_primary)),
        &Clip::none(),
        &mut probe,
    );
    draw::fill_rounded_rect(
        &mut s,
        Rect::new(44, 62, 180, 6),
        fixed::px(3),
        &Paint::Solid(Rgba::opaque(DARK.text_secondary)),
        &Clip::none(),
        &mut probe,
    );
    s
}

fn scene_ring_button() -> Surface {
    let mut s = Surface::filled(200, 120, Rgba::opaque(DARK.surface));
    let mut probe = NoProbe;
    // The CSS ring: a solid 3 px halo at accent/32% (no blur).
    draw::fill_rounded_rect(
        &mut s,
        Rect::new(57, 41, 86, 38),
        fixed::px(9),
        &Paint::Solid(RING_DARK),
        &Clip::none(),
        &mut probe,
    );
    draw::fill_rounded_rect(
        &mut s,
        Rect::new(60, 44, 80, 32),
        fixed::px(6),
        &Paint::Solid(Rgba::opaque(DARK.accent)),
        &Clip::none(),
        &mut probe,
    );
    s
}

fn scene_glass_pill() -> Surface {
    let mut s = Surface::new(300, 100);
    let rect = Rect::new(0, 0, 300, 100);
    for paint in WALLPAPER_DARK.paints(rect) {
        draw::fill_rect(&mut s, rect, &paint, &Clip::none());
    }
    let mut probe = NoProbe;
    let pill = rounded(50, 30, 200, 40, 20);
    draw::fill_path(
        &mut s,
        &pill,
        &Paint::Solid(GLASS_DARK),
        &Clip::none(),
        &mut probe,
    );
    draw::stroke(
        &mut s,
        &pill,
        fixed::ONE,
        raster::Cap::Butt,
        &Paint::Solid(Rgba::opaque(DARK.border_subtle)),
        &Clip::none(),
        &mut probe,
    );
    s
}

fn scene_stroke_gallery() -> Surface {
    let mut s = Surface::filled(300, 200, Rgba::opaque(DARK.base));
    let mut probe = NoProbe;
    let accent = Paint::Solid(Rgba::opaque(DARK.accent));
    let secondary = Paint::Solid(Rgba::opaque(DARK.text_secondary));
    // Open polyline with round caps.
    let mut b = PathBuilder::new();
    b.move_to(fixed::px(20), fixed::px(40))
        .line_to(fixed::px(80), fixed::px(20))
        .line_to(fixed::px(140), fixed::px(40));
    draw::stroke(
        &mut s,
        &b.build(),
        fixed::px(6),
        raster::Cap::Round,
        &accent,
        &Clip::none(),
        &mut probe,
    );
    // A stroked cubic.
    let mut b = PathBuilder::new();
    b.move_to(fixed::px(20), fixed::px(90)).cubic_to(
        fixed::px(60),
        fixed::px(60),
        fixed::px(100),
        fixed::px(120),
        fixed::px(160),
        fixed::px(90),
    );
    draw::stroke(
        &mut s,
        &b.build(),
        fixed::px(4),
        raster::Cap::Round,
        &secondary,
        &Clip::none(),
        &mut probe,
    );
    // A stroked triangle (closed, round joins) filled outline.
    let mut b = PathBuilder::new();
    b.move_to(fixed::px(210), fixed::px(150))
        .line_to(fixed::px(260), fixed::px(60))
        .line_to(fixed::px(290), fixed::px(150))
        .close();
    draw::stroke(
        &mut s,
        &b.build(),
        fixed::px(5),
        raster::Cap::Butt,
        &accent,
        &Clip::none(),
        &mut probe,
    );
    s
}

fn scene_dither_gradient() -> Surface {
    let mut s = Surface::new(256, 64);
    let paint = Paint::Linear {
        x0: 0,
        y0: 0,
        x1: fixed::px(256),
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
    draw::fill_rect(&mut s, Rect::new(0, 0, 256, 64), &paint, &Clip::none());
    s
}

fn scene_compose() -> Surface {
    // Wallpaper backdrop with a scaled window surface on top, repaired
    // through one damage rectangle.
    let mut backdrop = Surface::new(320, 200);
    let rect = Rect::new(0, 0, 320, 200);
    for paint in WALLPAPER_DARK.paints(rect) {
        draw::fill_rect(&mut backdrop, rect, &paint, &Clip::none());
    }
    let window = Surface::filled(100, 60, Rgba::opaque(DARK.surface));
    let mut target = Surface::filled(320, 200, Rgba::TRANSPARENT);
    let layers = [
        compose::Layer {
            id: 0,
            surface: &backdrop,
            dest: Rect::new(0, 0, 320, 200),
        },
        compose::Layer {
            id: 1,
            surface: &window,
            dest: Rect::new(110, 70, 200, 120),
        },
    ];
    let mut probe = NoProbe;
    compose::compose(
        &mut target,
        &layers,
        &[Rect::new(40, 30, 240, 140)],
        &mut probe,
    );
    target
}

macro_rules! golden {
    ($name:ident) => {
        #[test]
        fn $name() {
            check(stringify!($name), &paste_scene(stringify!($name)));
        }
    };
}

fn paste_scene(name: &str) -> Surface {
    match name {
        "wallpaper" => scene_wallpaper(),
        "window_shadow" => scene_window_shadow(),
        "toast_shadow" => scene_toast_shadow(),
        "ring_button" => scene_ring_button(),
        "glass_pill" => scene_glass_pill(),
        "stroke_gallery" => scene_stroke_gallery(),
        "dither_gradient" => scene_dither_gradient(),
        "compose" => scene_compose(),
        _ => panic!("unknown scene {name}"),
    }
}

golden!(wallpaper);
golden!(window_shadow);
golden!(toast_shadow);
golden!(ring_button);
golden!(glass_pill);
golden!(stroke_gallery);
golden!(dither_gradient);
golden!(compose);

/// Render every scene to `golden/review-<name>.png` when
/// `MIRROR_RENDER_DUMP=1` — for design review, not a check.
#[test]
fn dump_scenes_for_review() {
    if std::env::var("MIRROR_RENDER_DUMP").is_err() {
        return;
    }
    for name in [
        "wallpaper",
        "window_shadow",
        "toast_shadow",
        "ring_button",
        "glass_pill",
        "stroke_gallery",
        "dither_gradient",
        "compose",
    ] {
        let surface = paste_scene(name);
        std::fs::write(
            golden_dir().join(std::format!("review-{name}.png")),
            png(&surface),
        )
        .unwrap();
    }
}

// ---------- PNG dump (test-only, uncompressed zlib) ----------

fn png(surface: &Surface) -> Vec<u8> {
    let (w, h) = (surface.width(), surface.height());
    let mut raw = Vec::with_capacity((w * h * 4 + h) as usize);
    for y in 0..h {
        raw.push(0); // filter: none
        for x in 0..w {
            let p = surface.get(x as i32, y as i32);
            raw.extend_from_slice(&[p.r, p.g, p.b, p.a]);
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    // zlib: header 0x78 0x01, stored deflate blocks, adler32.
    let mut z = std::vec![0x78u8, 0x01];
    let mut rest = raw.as_slice();
    while !rest.is_empty() {
        let n = rest.len().min(65535);
        let last = n == rest.len();
        z.push(if last { 1 } else { 0 });
        z.extend_from_slice(&(n as u16).to_le_bytes());
        z.extend_from_slice(&(!(n as u16)).to_le_bytes());
        z.extend_from_slice(&rest[..n]);
        rest = &rest[n..];
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(data);
    let mut crc_data = Vec::with_capacity(4 + data.len());
    crc_data.extend_from_slice(name);
    crc_data.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_data).to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &v in data {
        a = (a + v as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
