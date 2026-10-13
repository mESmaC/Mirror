//! The six design tokens 0029 could not represent (0027c-D5, D6, D11): the
//! translucent ring and glass, the three elevation shadows (in
//! [`crate::shadow`]) and the wallpaper gradients. Values are copied from
//! `docs/design/design-system.html` and checked against it by the tests.

use crate::color::Rgba;
use crate::fixed::{self, Fixed};
use crate::paint::{GradientStop, Paint};
use alloc::vec;
use alloc::vec::Vec;
use mirror_ui_tokens::{DARK, LIGHT};

/// A CSS percentage alpha written as an 8-bit value: (.32 * 255) rounds to
/// 82. Percent is an integer hundredths value, rounding half up.
const fn alpha(percent: u16) -> u8 {
    ((percent * 255 + 50) / 100) as u8
}

/// The focus ring halo: accent at 32% (`design-system.html:83`).
pub const RING_DARK: Rgba = Rgba::from_rgb(DARK.accent, alpha(32));
/// Light theme (`design-system.html:51`).
pub const RING_LIGHT: Rgba = Rgba::from_rgb(LIGHT.accent, alpha(32));

/// Translucent panel fill (blur is deferred, `docs/design/README.md` note
/// 10). Dark: surface at 66% (`design-system.html:91`).
pub const GLASS_DARK: Rgba = Rgba::from_rgb(DARK.surface, alpha(66));
/// Light: white at 72% (`design-system.html:59`).
pub const GLASS_LIGHT: Rgba = Rgba::from_rgb(
    mirror_ui_tokens::Rgb {
        r: 255,
        g: 255,
        b: 255,
    },
    alpha(72),
);

/// A three-layer wallpaper: two elliptical radial glows over a linear base,
/// positioned by percentages of the painted rectangle with absolute CSS
/// pixel radii (`design-system.html:56-58`, `:88-90`).
pub struct Wallpaper {
    /// (percent x, percent y, radius x, radius y, glow color) per glow.
    pub glows: [(i32, i32, u32, u32, Rgba); 2],
    /// The linear base's angle in CSS degrees and its two colors.
    pub base_degrees: u32,
    pub base_colors: (Rgba, Rgba),
}

/// Dark wallpaper (`design-system.html:88-90`).
pub const WALLPAPER_DARK: Wallpaper = Wallpaper {
    glows: [
        (10, -10, 1100, 620, Rgba::opaque(rgb(0x12, 0x3A, 0x35))),
        (108, 112, 950, 640, Rgba::opaque(rgb(0x1A, 0x23, 0x40))),
    ],
    base_degrees: 160,
    base_colors: (
        Rgba::opaque(rgb(0x0B, 0x0F, 0x16)),
        Rgba::opaque(rgb(0x0E, 0x14, 0x20)),
    ),
};

/// Light wallpaper (`design-system.html:56-58`).
pub const WALLPAPER_LIGHT: Wallpaper = Wallpaper {
    glows: [
        (12, -10, 1100, 600, Rgba::opaque(rgb(0xD9, 0xED, 0xE9))),
        (105, 110, 900, 620, Rgba::opaque(rgb(0xDD, 0xE4, 0xF5))),
    ],
    base_degrees: 160,
    base_colors: (
        Rgba::opaque(rgb(0xF4, 0xF6, 0xFA)),
        Rgba::opaque(rgb(0xE9, 0xEE, 0xF5)),
    ),
};

const fn rgb(r: u8, g: u8, b: u8) -> mirror_ui_tokens::Rgb {
    mirror_ui_tokens::Rgb { r, g, b }
}

/// The CSS gradient-line direction for `degrees`, in 16.16: 0 points up and
/// angles run clockwise. Both token wallpapers use 160 degrees, whose sine
/// and negated cosine are the constants below; other angles are rejected
/// until a token needs them.
fn direction(degrees: u32) -> (Fixed, Fixed) {
    assert!(degrees == 160, "the token wallpapers use only 160 degrees");
    // sin(160) = 0.342020..., -cos(160) = 0.939693...
    (22414, 61584)
}

impl Wallpaper {
    /// The three paints (base first, then the glows) for a rectangle.
    pub fn paints(&self, rect: crate::clip::Rect) -> Vec<Paint> {
        let (w, h) = (rect.w as i32, rect.h as i32);
        let (dx, dy) = direction(self.base_degrees);
        // CSS gradient line: through the center, length |w*sin| + |h*cos|.
        let cx = fixed::px(rect.x) + fixed::px(w) / 2;
        let cy = fixed::px(rect.y) + fixed::px(h) / 2;
        let len = fixed::mul(fixed::px(w), dx).abs() / 2 + fixed::mul(fixed::px(h), dy).abs() / 2;
        let base = Paint::Linear {
            x0: cx - fixed::mul(len, dx),
            y0: cy - fixed::mul(len, dy),
            x1: cx + fixed::mul(len, dx),
            y1: cy + fixed::mul(len, dy),
            stops: vec![
                GradientStop {
                    t: 0,
                    color: self.base_colors.0,
                },
                GradientStop {
                    t: u16::MAX,
                    color: self.base_colors.1,
                },
            ],
        };
        let mut paints = vec![base];
        // CSS backgrounds paint the FIRST listed layer frontmost, so the
        // glows go on the back-to-front list in reverse (review P6).
        for &(px_, py_, rx, ry, color) in self.glows.iter().rev() {
            // "color 0%, transparent 55%": the glow fades to nothing.
            let stops = vec![
                GradientStop { t: 0, color },
                GradientStop {
                    t: (u16::MAX as u32 * 55 / 100) as u16,
                    color: Rgba::TRANSPARENT,
                },
                GradientStop {
                    t: u16::MAX,
                    color: Rgba::TRANSPARENT,
                },
            ];
            paints.push(Paint::Radial {
                cx: fixed::px(rect.x)
                    + fixed::mul(fixed::px(w), fixed::div(fixed::px(px_), fixed::px(100))),
                cy: fixed::px(rect.y)
                    + fixed::mul(fixed::px(h), fixed::div(fixed::px(py_), fixed::px(100))),
                rx: fixed::px(rx as i32),
                ry: fixed::px(ry as i32),
                stops,
            });
        }
        paints
    }
}
