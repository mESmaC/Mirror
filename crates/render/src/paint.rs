//! Paints: solid colors and dithered linear and elliptical-radial gradients
//! (0027c-D6). Gradient stops interpolate in premultiplied space; an 8x8
//! Bayer ordered dither kills banding.

use crate::color::Rgba;
use crate::fixed::{self, Fixed, ONE};
use alloc::vec::Vec;

/// One gradient stop: `t` in 0..=65535 and a premultiplied color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GradientStop {
    pub t: u16,
    pub color: Rgba,
}

/// A paint source sampled at 16.16 pixel-center coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Paint {
    Solid(Rgba),
    /// From point (x0,y0) to (x1,y1); stops outside the ends clamp.
    Linear {
        x0: Fixed,
        y0: Fixed,
        x1: Fixed,
        y1: Fixed,
        stops: Vec<GradientStop>,
    },
    /// An elliptical radial gradient: center (cx,cy), radii (rx,ry); t is
    /// the normalized ellipse distance.
    Radial {
        cx: Fixed,
        cy: Fixed,
        rx: Fixed,
        ry: Fixed,
        stops: Vec<GradientStop>,
    },
}

impl Paint {
    /// The color at 16.16 pixel coordinates (px, py), with ordered dither
    /// applied to gradients.
    pub fn sample(&self, x: Fixed, y: Fixed, dither_x: u32, dither_y: u32) -> Rgba {
        match self {
            Paint::Solid(color) => *color,
            Paint::Linear {
                x0,
                y0,
                x1,
                y1,
                stops,
            } => {
                let dx = x1 - x0;
                let dy = y1 - y0;
                let len2 = dx as i64 * dx as i64 + dy as i64 * dy as i64;
                let t = if len2 == 0 {
                    0
                } else {
                    let num = (x - x0) as i64 * dx as i64 + (y - y0) as i64 * dy as i64;
                    // num / len2, scaled to 0..65535; i128 keeps the
                    // intermediate exact at screen sizes.
                    ((num as i128 * 65535) / len2 as i128).clamp(0, 65535) as u16
                };
                sample_stops(stops, t, dither_x, dither_y)
            }
            Paint::Radial {
                cx,
                cy,
                rx,
                ry,
                stops,
            } => {
                // Normalized ellipse distance in 16.16.
                let ex = fixed::div(x - cx, (*rx).max(1));
                let ey = fixed::div(y - cy, (*ry).max(1));
                let d = fixed::sqrt32(ex as i64 * ex as i64 + ey as i64 * ey as i64);
                let t = ((d as i64 * 65535) / ONE as i64).clamp(0, 65535) as u16;
                sample_stops(stops, t, dither_x, dither_y)
            }
        }
    }
}

/// An 8x8 Bayer matrix, values 0..63, for the ordered dither.
#[rustfmt::skip]
const BAYER: [u8; 64] = [
     0, 32,  8, 40,  2, 34, 10, 42,
    48, 16, 56, 24, 50, 18, 58, 26,
    12, 44,  4, 36, 14, 46,  6, 38,
    60, 28, 52, 20, 62, 30, 54, 22,
     3, 35, 11, 43,  1, 33,  9, 41,
    51, 19, 59, 27, 49, 17, 57, 25,
    15, 47,  7, 39, 13, 45,  5, 37,
    63, 31, 55, 23, 61, 29, 53, 21,
];

/// Interpolate the stops at `t` and quantize with the Bayer threshold:
/// out = floor(v + threshold) with threshold = (2*cell+1)/128 in (0,1), the
/// standard ordered-dither quantization. Integer-valued channels (opaque
/// alpha, equal-color stops) are preserved exactly at every phase.
fn sample_stops(stops: &[GradientStop], t: u16, dx: u32, dy: u32) -> Rgba {
    if stops.is_empty() {
        return Rgba::TRANSPARENT;
    }
    if stops.len() == 1 || t <= stops[0].t {
        return stops[0].color;
    }
    if t >= stops[stops.len() - 1].t {
        return stops[stops.len() - 1].color;
    }
    let mut upper = 1;
    while upper < stops.len() && stops[upper].t < t {
        upper += 1;
    }
    let a = stops[upper - 1];
    let b = stops[upper];
    let span = (b.t - a.t).max(1) as u32;
    let f = (t - a.t) as u32;
    // Channel values in 16.16 of 0..255.
    let mix = |ca: u8, cb: u8| -> i64 {
        let ca = (ca as i64) << 16;
        let cb = (cb as i64) << 16;
        ca + (cb - ca) * f as i64 / span as i64
    };
    let dither = ((BAYER[(dy as usize % 8) * 8 + (dx as usize % 8)] as i64) * 2 + 1) << 9;
    let quant = |v: i64| -> u8 { ((v + dither) >> 16).clamp(0, 255) as u8 };
    Rgba {
        r: quant(mix(a.color.r, b.color.r)),
        g: quant(mix(a.color.g, b.color.g)),
        b: quant(mix(a.color.b, b.color.b)),
        a: quant(mix(a.color.a, b.color.a)),
    }
}
