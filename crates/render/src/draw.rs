//! Drawing operations: fills, strokes and scaled blits, all under a clip
//! and all source-over.

use crate::clip::{Clip, Rect};
use crate::fixed::{self, Fixed};
use crate::frame::{FrameProbe, Span};
use crate::paint::Paint;
use crate::path::{Path, PathBuilder};
use crate::raster::{self, Cap, Mask};
use crate::surface::Surface;

/// Fill a path with a paint, clipped.
pub fn fill_path(
    dst: &mut Surface,
    path: &Path,
    paint: &Paint,
    clip: &Clip,
    probe: &mut dyn FrameProbe,
) {
    probe.begin(Span::Raster);
    let (mask, origin) = raster::rasterize(path);
    probe.end(Span::Raster);
    paint_mask_at(dst, &mask, origin, paint, clip);
}

/// Fill an axis-aligned rectangle (fast path: no rasterization).
pub fn fill_rect(dst: &mut Surface, rect: Rect, paint: &Paint, clip: &Clip) {
    let Some(area) = rect.intersect_opt(clip) else {
        return;
    };
    let Some(area) = area.intersect(&Rect::new(0, 0, dst.width(), dst.height())) else {
        return;
    };
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cov = clip.coverage(x, y);
            if cov == 0 {
                continue;
            }
            let color = sample(paint, x, y).covered(cov);
            dst.blend(x, y, color);
        }
    }
}

/// Fill a rounded rectangle.
pub fn fill_rounded_rect(
    dst: &mut Surface,
    rect: Rect,
    radius: Fixed,
    paint: &Paint,
    clip: &Clip,
    probe: &mut dyn FrameProbe,
) {
    let mut b = PathBuilder::new();
    b.rounded_rect(
        fixed::px(rect.x),
        fixed::px(rect.y),
        fixed::px(rect.w as i32),
        fixed::px(rect.h as i32),
        radius,
    );
    fill_path(dst, &b.build(), paint, clip, probe);
}

/// Stroke a path with a paint (0027c-D4: butt or round caps, round joins).
pub fn stroke(
    dst: &mut Surface,
    path: &Path,
    width: Fixed,
    cap: Cap,
    paint: &Paint,
    clip: &Clip,
    probe: &mut dyn FrameProbe,
) {
    let outline = raster::stroke_outline(path, width, cap);
    fill_path(dst, &outline, paint, clip, probe);
}

/// Paint a coverage mask with a paint, its (0,0) pixel at `origin` in the
/// surface's coordinates.
pub(crate) fn paint_mask_at(
    dst: &mut Surface,
    mask: &Mask,
    origin: (i32, i32),
    paint: &Paint,
    clip: &Clip,
) {
    let bounds = Rect::new(origin.0, origin.1, mask.width(), mask.height());
    let Some(area) = bounds.intersect_opt(clip) else {
        return;
    };
    let Some(area) = area.intersect(&Rect::new(0, 0, dst.width(), dst.height())) else {
        return;
    };
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cov = coverage_mul(mask.get(x - origin.0, y - origin.1), clip.coverage(x, y));
            if cov == 0 {
                continue;
            }
            let color = sample(paint, x, y).covered(cov);
            dst.blend(x, y, color);
        }
    }
}

/// Blit `src` scaled into `dest` on `dst` (0027c-D8: axis-aligned only).
/// `filter` selects nearest or bilinear sampling.
pub fn blit(
    dst: &mut Surface,
    src: &Surface,
    dest: Rect,
    filter: Filter,
    clip: &Clip,
    probe: &mut dyn FrameProbe,
) {
    probe.begin(Span::Blit);
    let Some(area) = dest.intersect_opt(clip) else {
        probe.end(Span::Blit);
        return;
    };
    let Some(area) = area.intersect(&Rect::new(0, 0, dst.width(), dst.height())) else {
        probe.end(Span::Blit);
        return;
    };
    if dest.w == 0 || dest.h == 0 {
        probe.end(Span::Blit);
        return;
    }
    // Map a destination pixel center back into source coordinates.
    let scale_x = ((src.width() as i64) << 16) / dest.w as i64;
    let scale_y = ((src.height() as i64) << 16) / dest.h as i64;
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cov = clip.coverage(x, y);
            if cov == 0 {
                continue;
            }
            let mut sx = (((x - dest.x) as i64 * 65536 + 32768) * scale_x / 65536 - 32768) as i32;
            let mut sy = (((y - dest.y) as i64 * 65536 + 32768) * scale_y / 65536 - 32768) as i32;
            // Clamp to the source edge, so border pixels sample the edge.
            sx = sx.clamp(0, ((src.width() as i32 - 1).max(0)) << 16);
            sy = sy.clamp(0, ((src.height() as i32 - 1).max(0)) << 16);
            let color = match filter {
                Filter::Nearest => src.get((sx + 32768) >> 16, (sy + 32768) >> 16),
                Filter::Bilinear => bilinear(src, sx, sy),
            };
            if color.a == 0 {
                continue;
            }
            dst.blend(x, y, color.covered(cov));
        }
    }
    probe.end(Span::Blit);
}

/// Sampling filter for scaled blits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    Nearest,
    #[default]
    Bilinear,
}

/// Bilinear sample of a premultiplied surface at 16.16 (x, y). Sampling
/// premultiplied colors keeps the filter correct under translucency.
fn bilinear(src: &Surface, x: Fixed, y: Fixed) -> crate::color::Rgba {
    let x0 = x >> 16;
    let y0 = y >> 16;
    let fx = (x & 0xFFFF) as i64;
    let fy = (y & 0xFFFF) as i64;
    let c00 = src.get(x0, y0);
    let c10 = src.get(x0 + 1, y0);
    let c01 = src.get(x0, y0 + 1);
    let c11 = src.get(x0 + 1, y0 + 1);
    let w00 = (65536 - fx) * (65536 - fy);
    let w10 = fx * (65536 - fy);
    let w01 = (65536 - fx) * fy;
    let w11 = fx * fy;
    let blend = |a: u8, b: u8, c: u8, d: u8| -> u8 {
        ((a as i64 * w00 + b as i64 * w10 + c as i64 * w01 + d as i64 * w11 + (1 << 31)) >> 32)
            as u8
    };
    crate::color::Rgba {
        r: blend(c00.r, c10.r, c01.r, c11.r),
        g: blend(c00.g, c10.g, c01.g, c11.g),
        b: blend(c00.b, c10.b, c01.b, c11.b),
        a: blend(c00.a, c10.a, c01.a, c11.a),
    }
}

/// Sample a paint at a pixel's center.
fn sample(paint: &Paint, x: i32, y: i32) -> crate::color::Rgba {
    paint.sample((x << 16) + 32768, (y << 16) + 32768, x as u32, y as u32)
}

/// Multiply two coverages (0..255 each) into one.
pub(crate) fn coverage_mul(a: u8, b: u8) -> u8 {
    ((a as u16 * b as u16 + 127) / 255) as u8
}

/// Intersect a rectangle with a clip's bounds; an unbounded clip keeps the
/// rectangle.
trait IntersectClip {
    fn intersect_opt(&self, clip: &Clip) -> Option<Rect>;
}

impl IntersectClip for Rect {
    fn intersect_opt(&self, clip: &Clip) -> Option<Rect> {
        match clip.bounds() {
            None => Some(*self),
            Some(b) => self.intersect(&b),
        }
    }
}
