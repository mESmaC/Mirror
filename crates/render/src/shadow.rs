//! Soft shadows (0027c-D5): the CSS box-shadow model — offset, blur radius,
//! color — drawn by blurring the shape's coverage mask with three separable
//! box-blur passes, with a bounded LRU cache of blurred masks.

use crate::clip::Clip;
use crate::color::Rgba;
use crate::frame::{FrameProbe, Span};
use crate::paint::Paint;
use crate::path::Path;
use crate::raster::{self, Mask};
use crate::surface::Surface;
use alloc::vec::Vec;

/// One CSS box-shadow: offset (dx, dy), blur radius and color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shadow {
    pub dx: i32,
    pub dy: i32,
    pub blur: u32,
    pub color: Rgba,
}

const fn shadow(dx: i32, dy: i32, blur: u32, r: u8, g: u8, b: u8, a: u8) -> Shadow {
    Shadow {
        dx,
        dy,
        blur,
        color: Rgba::from_rgba(r, g, b, a),
    }
}

/// Elevation 1: cards. Dark theme (`design-system.html:85`).
pub const SHADOW_1_DARK: &[Shadow] = &[shadow(0, 1, 2, 0, 0, 0, 89)];
/// Elevation 1: cards. Light theme (`design-system.html:53`).
pub const SHADOW_1_LIGHT: &[Shadow] = &[
    shadow(0, 1, 2, 15, 23, 38, 18),
    shadow(0, 1, 3, 15, 23, 38, 13),
];
/// Elevation 2: popovers, menus, toasts. Dark theme (`design-system.html:86`).
pub const SHADOW_2_DARK: &[Shadow] = &[
    shadow(0, 4, 14, 0, 0, 0, 115),
    shadow(0, 1, 3, 0, 0, 0, 102),
];
/// Elevation 2. Light theme (`design-system.html:54`).
pub const SHADOW_2_LIGHT: &[Shadow] = &[
    shadow(0, 4, 12, 15, 23, 38, 26),
    shadow(0, 2, 4, 15, 23, 38, 15),
];
/// Elevation 3: windows. Dark theme (`design-system.html:87`).
pub const SHADOW_3_DARK: &[Shadow] = &[
    shadow(0, 18, 48, 0, 0, 0, 153),
    shadow(0, 4, 12, 0, 0, 0, 115),
];
/// Elevation 3. Light theme (`design-system.html:55`).
pub const SHADOW_3_LIGHT: &[Shadow] = &[
    shadow(0, 16, 40, 15, 23, 38, 41),
    shadow(0, 4, 10, 15, 23, 38, 20),
];

/// The box radius of each of the three blur passes for a CSS blur radius.
/// A CSS blur of `b` approximates a Gaussian with sigma b/2; three boxes of
/// radius max(1, b/2) approximate it.
fn box_radius(blur: u32) -> usize {
    ((blur / 2) as usize).max(1)
}

/// Blur a mask in place: three separable box passes, edge-clamped windows.
pub fn blur_mask(mask: &mut Mask, blur: u32, probe: &mut dyn FrameProbe) {
    probe.begin(Span::Blur);
    let r = box_radius(blur);
    let (w, h) = (mask.width() as usize, mask.height() as usize);
    if w > 0 && h > 0 {
        let mut tmp = alloc::vec![0u8; w * h];
        for _ in 0..3 {
            box_pass_h(mask, &mut tmp, r);
            box_pass_v(&tmp, mask, r);
        }
    }
    probe.end(Span::Blur);
}

/// One horizontal box pass from `src` into `dst`; windows clamp at the
/// edges and divide by their true size.
fn box_pass_h(src: &Mask, dst: &mut [u8], r: usize) {
    let (w, h) = (src.width() as usize, src.height() as usize);
    for y in 0..h {
        let row = &src.data()[y * w..(y + 1) * w];
        let right0 = r.min(w - 1);
        let mut sum: u32 = row[..=right0].iter().map(|&v| v as u32).sum();
        for x in 0..w {
            if x > 0 {
                if x + r < w {
                    sum += row[x + r] as u32;
                }
                if x > r {
                    sum -= row[x - r - 1] as u32;
                }
            }
            let left = x.saturating_sub(r);
            let right = (x + r).min(w - 1);
            let size = (right - left + 1) as u32;
            dst[y * w + x] = ((sum + size / 2) / size) as u8;
        }
    }
}

/// One vertical box pass.
fn box_pass_v(src: &[u8], dst: &mut Mask, r: usize) {
    let (w, h) = (dst.width() as usize, dst.height() as usize);
    for x in 0..w {
        let bottom0 = r.min(h - 1);
        let mut sum: u32 = (0..=bottom0).map(|y| src[y * w + x] as u32).sum();
        for y in 0..h {
            if y > 0 {
                if y + r < h {
                    sum += src[(y + r) * w + x] as u32;
                }
                if y > r {
                    sum -= src[(y - r - 1) * w + x] as u32;
                }
            }
            let top = y.saturating_sub(r);
            let bottom = (y + r).min(h - 1);
            let size = (bottom - top + 1) as u32;
            dst.row_mut(y as u32)[x] = ((sum + size / 2) / size) as u8;
        }
    }
}

/// Draw a path's shadows under the shape: rasterize, blur (cached), and
/// paint the mask in each shadow's color at its offset.
pub fn draw_shadow(
    dst: &mut Surface,
    path: &Path,
    shadows: &[Shadow],
    clip: &Clip,
    cache: &mut ShadowCache,
    probe: &mut dyn FrameProbe,
) {
    if shadows.is_empty() {
        return;
    }
    probe.begin(Span::Raster);
    let (mask, origin) = raster::rasterize(path);
    probe.end(Span::Raster);
    for shadow in shadows {
        let blurred = cache.blurred(&mask, shadow.blur, probe);
        let paint = Paint::Solid(shadow.color);
        crate::draw::paint_mask_at(
            dst,
            blurred,
            (origin.0 + shadow.dx, origin.1 + shadow.dy),
            &paint,
            clip,
        );
    }
}

/// A bounded LRU cache of blurred shadow masks, keyed by mask content,
/// blur and size (0027c-D5). The byte budget is a soft cap: a single entry
/// bigger than the budget still caches.
pub struct ShadowCache {
    entries: Vec<Entry>,
    byte_budget: usize,
    used: usize,
    clock: u64,
}

struct Entry {
    key: u64,
    mask: Mask,
    bytes: usize,
    tick: u64,
}

impl ShadowCache {
    pub fn new(byte_budget: usize) -> ShadowCache {
        ShadowCache {
            entries: Vec::new(),
            byte_budget,
            used: 0,
            clock: 0,
        }
    }

    /// FNV-1a over the mask bytes, mixed with blur and size.
    fn key(mask: &Mask, blur: u32) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in mask.data() {
            hash = (hash ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= ((blur as u64) << 32) | mask.width() as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        hash ^ ((mask.height() as u64) << 16)
    }

    /// The blurred mask for (mask, blur), computing and caching on a miss.
    pub fn blurred(&mut self, mask: &Mask, blur: u32, probe: &mut dyn FrameProbe) -> &Mask {
        let key = Self::key(mask, blur);
        self.clock += 1;
        if let Some(i) = self.entries.iter().position(|e| e.key == key) {
            self.entries[i].tick = self.clock;
            return &self.entries[i].mask;
        }
        let mut blurred = mask.clone();
        blur_mask(&mut blurred, blur, probe);
        let bytes = blurred.data().len();
        self.used += bytes;
        self.entries.push(Entry {
            key,
            mask: blurred,
            bytes,
            tick: self.clock,
        });
        // Evict least-recently-used entries, never the one just inserted.
        while self.used > self.byte_budget && self.entries.len() > 1 {
            let newest = self.entries.len() - 1;
            let oldest = self.entries[..newest]
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.tick)
                .map(|(i, _)| i);
            match oldest {
                Some(i) => {
                    self.used -= self.entries[i].bytes;
                    self.entries.remove(i);
                }
                None => break,
            }
        }
        &self.entries[self.entries.len() - 1].mask
    }

    /// The number of cached masks.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether any masks are cached.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
