//! Premultiplied RGBA8 colors (0027c-D2). Every channel is stored
//! premultiplied by alpha; the only blend mode is source-over.

use mirror_ui_tokens::Rgb;

/// Round `(value * alpha) / 255` to the nearest integer.
const fn premul(channel: u8, alpha: u8) -> u8 {
    ((channel as u16 * alpha as u16 + 127) / 255) as u8
}

/// A premultiplied RGBA color: each channel is already scaled by `a`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    /// Fully transparent black.
    pub const TRANSPARENT: Rgba = Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    /// An opaque token color.
    pub const fn opaque(rgb: Rgb) -> Rgba {
        Rgba {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
            a: 255,
        }
    }

    /// A token color at the given alpha, premultiplied once at the boundary.
    pub const fn from_rgb(rgb: Rgb, alpha: u8) -> Rgba {
        Rgba {
            r: premul(rgb.r, alpha),
            g: premul(rgb.g, alpha),
            b: premul(rgb.b, alpha),
            a: alpha,
        }
    }

    /// Straight-alpha CSS rgba() components, premultiplied.
    pub const fn from_rgba(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        Rgba {
            r: premul(r, a),
            g: premul(g, a),
            b: premul(b, a),
            a,
        }
    }

    /// Source-over: `self` blended over `dst`.
    pub const fn over(self, dst: Rgba) -> Rgba {
        let inv = 255 - self.a;
        Rgba {
            r: self.r + premul(dst.r, inv),
            g: self.g + premul(dst.g, inv),
            b: self.b + premul(dst.b, inv),
            a: self.a + premul(dst.a, inv),
        }
    }

    /// Every channel scaled by `coverage` (0-255), for painting through a
    /// coverage mask.
    pub const fn covered(self, coverage: u8) -> Rgba {
        Rgba {
            r: premul(self.r, coverage),
            g: premul(self.g, coverage),
            b: premul(self.b, coverage),
            a: premul(self.a, coverage),
        }
    }
}
