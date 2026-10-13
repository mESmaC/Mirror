//! The shared path rasterizer (0027c-D4): flattened paths rasterize to 8-bit
//! coverage masks by exact scanline area accumulation, all in fixed point.
//! Strokes are expanded to outline paths with round joins and butt or round
//! caps, then filled. 0086a rasterizes glyph outlines through `rasterize`.

use crate::fixed::{self, Fixed, ONE, TOLERANCE};
use crate::path::{Path, PathBuilder, Point, Subpath};
use alloc::vec::Vec;

/// An 8-bit alpha mask: one coverage byte per pixel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mask {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl Mask {
    pub fn new(width: u32, height: u32) -> Mask {
        Mask {
            width,
            height,
            data: alloc::vec![0; width as usize * height as usize],
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The coverage at (x, y), or 0 outside the mask.
    pub fn get(&self, x: i32, y: i32) -> u8 {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return 0;
        }
        self.data[y as usize * self.width as usize + x as usize]
    }

    pub(crate) fn row_mut(&mut self, y: u32) -> &mut [u8] {
        let start = y as usize * self.width as usize;
        &mut self.data[start..start + self.width as usize]
    }

    /// Set the coverage at (x, y); outside is a no-op.
    pub fn set(&mut self, x: i32, y: i32, value: u8) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        self.data[y as usize * self.width as usize + x as usize] = value;
    }
}

/// Rasterize a path into a mask sized to the path's pixel bounds (min
/// floored, max ceiled). Returns the mask and its origin: mask pixel (0,0)
/// is path coordinate `origin`.
pub fn rasterize(path: &Path) -> (Mask, (i32, i32)) {
    let Some(((min_x, min_y), (max_x, max_y))) = path.bounds() else {
        return (Mask::new(0, 0), (0, 0));
    };
    let ox = fixed::floor(min_x);
    let oy = fixed::floor(min_y);
    let width = ((max_x + ONE - 1) >> 16) - ox;
    let height = ((max_y + ONE - 1) >> 16) - oy;
    if width <= 0 || height <= 0 {
        return (Mask::new(0, 0), (ox, oy));
    }
    let mut mask = Mask::new(width as u32, height as u32);
    rasterize_into(&mut mask, path, -ox, -oy);
    (mask, (ox, oy))
}

/// Rasterize a path into an existing mask, translating every path
/// coordinate by (tx, ty) whole pixels.
///
/// The fill rule is true nonzero winding (0027c-D4): per scanline, edge
/// crossings are sorted and filled intervals are found by the running
/// winding number, and only then is each interval's coverage integrated.
/// Opposite-winding regions therefore cannot cancel each other's coverage.
pub fn rasterize_into(mask: &mut Mask, path: &Path, tx: i32, ty: i32) {
    let width = mask.width as i32;
    let height = mask.height as i32;
    if width == 0 || height == 0 {
        return;
    }
    let shift_x = tx << 16;
    let shift_y = ty << 16;
    // Normalize every segment to y-ascending once; the sign is the winding.
    let mut segments: Vec<(Point, Point, i32)> = Vec::new();
    for subpath in path.flatten() {
        for pair in subpath.points.windows(2) {
            let (mut x0, mut y0) = (pair[0].0 + shift_x, pair[0].1 + shift_y);
            let (mut x1, mut y1) = (pair[1].0 + shift_x, pair[1].1 + shift_y);
            if y0 == y1 {
                continue;
            }
            let sign = if y0 < y1 { 1 } else { -1 };
            if y0 > y1 {
                core::mem::swap(&mut x0, &mut x1);
                core::mem::swap(&mut y0, &mut y1);
            }
            segments.push(((x0, y0), (x1, y1), sign));
        }
    }
    let mut spans: Vec<EdgeSpan> = Vec::new();
    let mut cuts: Vec<Fixed> = Vec::new();
    let mut totals = alloc::vec![0i64; width as usize];
    for row in 0..height {
        let row_top = row << 16;
        let row_bottom = row_top + ONE;
        spans.clear();
        for &((x0, y0), (x1, y1), sign) in &segments {
            let ya = y0.max(row_top);
            let yb = y1.min(row_bottom);
            if ya >= yb {
                continue;
            }
            spans.push(EdgeSpan {
                sign,
                ya,
                xa: lerp_x(x0, y0, x1, y1, ya),
                yb,
                xb: lerp_x(x0, y0, x1, y1, yb),
            });
        }
        if spans.is_empty() {
            continue;
        }
        // Split the row at every span endpoint and every edge crossing, so
        // within a sub-span exactly the spans covering it participate and
        // their order is fixed; every filled interval is a true trapezoid.
        cuts.clear();
        cuts.push(row_top);
        cuts.push(row_bottom);
        for span in &spans {
            if span.ya > row_top && span.ya < row_bottom {
                cuts.push(span.ya);
            }
            if span.yb > row_top && span.yb < row_bottom {
                cuts.push(span.yb);
            }
        }
        for i in 0..spans.len() {
            for j in i + 1..spans.len() {
                if let Some(y) = intersection_y(&spans[i], &spans[j])
                    && y > row_top
                    && y < row_bottom
                {
                    cuts.push(y);
                }
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        totals.iter_mut().for_each(|t| *t = 0);
        for pair in cuts.windows(2) {
            let (s0, s1) = (pair[0], pair[1]);
            if s0 >= s1 {
                continue;
            }
            let mid = (s0 + s1) / 2;
            let mut active: Vec<&EdgeSpan> =
                spans.iter().filter(|e| e.ya <= s0 && e.yb >= s1).collect();
            active.sort_by_key(|e| e.x_at(mid));
            let mut winding: i32 = 0;
            for k in 0..active.len().saturating_sub(1) {
                winding += active[k].sign;
                if winding != 0 {
                    add_interval(&mut totals, width, active[k], active[k + 1], s0, s1);
                }
            }
        }
        let out = mask.row_mut(row as u32);
        for (col, cell) in out.iter_mut().enumerate() {
            let clamped = totals[col].clamp(0, 1i64 << 32) as u64;
            *cell = ((clamped * 255 + (1 << 31)) >> 32) as u8;
        }
    }
}

/// An edge clipped to a scanline: x is linear in y from (ya, xa) to
/// (yb, xb), ya < yb; `sign` is the winding direction.
struct EdgeSpan {
    sign: i32,
    ya: Fixed,
    xa: Fixed,
    yb: Fixed,
    xb: Fixed,
}

impl EdgeSpan {
    fn x_at(&self, y: Fixed) -> Fixed {
        lerp_x(self.xa, self.ya, self.xb, self.yb, y)
    }
}

/// The y where two row-clipped edges cross, if they cross strictly inside
/// both spans.
fn intersection_y(a: &EdgeSpan, b: &EdgeSpan) -> Option<Fixed> {
    let (na, da) = ((a.xb - a.xa) as i64, (a.yb - a.ya) as i64);
    let (nb, db) = ((b.xb - b.xa) as i64, (b.yb - b.ya) as i64);
    let denom = na * db - nb * da;
    if denom == 0 {
        return None;
    }
    // Solve xa + na*(y-ya)/da = xb + nb*(y-yb)/db for y.
    let num = (b.xa - a.xa) as i64 * da * db + na * a.ya as i64 * db - nb * b.ya as i64 * da;
    let y = (num / denom) as i32;
    let lo = a.ya.max(b.ya);
    let hi = a.yb.min(b.yb);
    if y > lo && y < hi { Some(y) } else { None }
}

/// x at y along the segment (x0,y0)-(x1,y1), with y0 != y1.
fn lerp_x(x0: Fixed, y0: Fixed, x1: Fixed, y1: Fixed, y: Fixed) -> Fixed {
    (x0 as i64 + ((x1 - x0) as i64 * (y - y0) as i64) / (y1 - y0) as i64) as Fixed
}

/// Accumulate the unsigned per-pixel area of one filled interval: the
/// trapezoid between edges `l` and `r` over the sub-span [s0, s1].
fn add_interval(totals: &mut [i64], width: i32, l: &EdgeSpan, r: &EdgeSpan, s0: Fixed, s1: Fixed) {
    let (l0, l1) = (l.x_at(s0), l.x_at(s1));
    let (r0, r1) = (r.x_at(s0), r.x_at(s1));
    let lo = l0.min(l1);
    let hi = r0.max(r1);
    let first = (lo >> 16).max(0);
    let last = ((hi - 1) >> 16).min(width - 1);
    for col in first..=last {
        let right = integrate_clamped(r0, r1, s0, s1, col);
        let left = integrate_clamped(l0, l1, s0, s1, col);
        totals[col as usize] += (right - left).max(0);
    }
}

/// The integral of clamp(f(y) - col, 0, 1) dy over [s0, s1], where f goes
/// linearly from x0 to x1. Exact for a linear f, in 32.32 units.
fn integrate_clamped(x0: Fixed, x1: Fixed, s0: Fixed, s1: Fixed, col: i32) -> i64 {
    let cell = col << 16;
    let f0 = x0 - cell;
    let f1 = x1 - cell;
    let h = s1 - s0;
    if f0 == f1 {
        return f0.clamp(0, ONE) as i64 * h as i64;
    }
    // Cut where f crosses the clamp thresholds, so every piece is either
    // fully saturated or exactly linear.
    let mut cuts = [0, 0, 0, ONE];
    let mut n = 1usize; // cuts[0] = 0, cuts[3] = ONE
    for &threshold in &[0, ONE] {
        let lo = f0.min(f1);
        let hi = f0.max(f1);
        if lo < threshold && threshold < hi {
            let t = (((threshold - f0) as i64) << 16) / (f1 - f0) as i64;
            cuts[n] = t as i32;
            n += 1;
        }
    }
    cuts[n] = ONE;
    cuts[1..n].sort_unstable();
    let mut area: i64 = 0;
    let mut prev_t = 0;
    let mut prev_f = f0.clamp(0, ONE);
    for &t in &cuts[1..=n] {
        let f_t = (f0 as i64 + (f1 - f0) as i64 * t as i64 / ONE as i64) as i32;
        let f_t = f_t.clamp(0, ONE);
        let dt = t - prev_t;
        // Piece height times the average clamped value, in 32.32.
        let piece_h = fixed::mul(h, dt) as i64;
        area += (prev_f + f_t) as i64 * piece_h / 2;
        prev_t = t;
        prev_f = f_t;
    }
    area
}

/// Line cap style for strokes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cap {
    /// The stroke ends at the endpoint.
    #[default]
    Butt,
    /// A semicircle of the stroke width extends past the endpoint.
    Round,
}

/// Expand a stroked path into its outline fill path (0027c-D4): every
/// subpath offsets by half `width` on both sides, interior vertices get
/// round joins, and open ends get the chosen caps. Arcs are built by
/// angle-bisector subdivision, so no trigonometry is needed.
pub fn stroke_outline(path: &Path, width: Fixed, cap: Cap) -> Path {
    let half = width / 2;
    let mut builder = PathBuilder::new();
    if half <= 0 {
        return builder.build();
    }
    for subpath in path.flatten() {
        stroke_subpath(&mut builder, &subpath, half, cap);
    }
    builder.build()
}

fn stroke_subpath(builder: &mut PathBuilder, subpath: &Subpath, half: Fixed, cap: Cap) {
    let points = &subpath.points;
    // Drop zero-length segments first so normals are well defined.
    let mut pts: Vec<Point> = Vec::with_capacity(points.len());
    for &p in points {
        if pts.last() != Some(&p) {
            pts.push(p);
        }
    }
    if pts.len() < 2 {
        return;
    }
    if subpath.closed && pts.first() == pts.last() {
        pts.pop();
    }
    let n = pts.len();
    let seg_count = if subpath.closed { n } else { n - 1 };
    if seg_count == 0 || n < 2 {
        return;
    }
    // Unit directions and left normals (y-down: left of (dx,dy) is (dy,-dx)).
    let mut dirs: Vec<Point> = Vec::with_capacity(seg_count);
    let mut lefts: Vec<Point> = Vec::with_capacity(seg_count);
    for i in 0..seg_count {
        let d = (pts[(i + 1) % n].0 - pts[i].0, pts[(i + 1) % n].1 - pts[i].1);
        let (ux, uy) = fixed::scale_to(d.0, d.1, ONE);
        if (ux, uy) == (0, 0) {
            return;
        }
        dirs.push((ux, uy));
        lefts.push((uy, -ux));
    }
    let offset = |p: Point, normal: Point, scale: Fixed| -> Point {
        (
            p.0 + fixed::mul(normal.0, scale),
            p.1 + fixed::mul(normal.1, scale),
        )
    };
    let mut left_chain: Vec<Point> = Vec::with_capacity(2 * n);
    let mut right_chain: Vec<Point> = Vec::with_capacity(2 * n);
    for i in 0..n {
        if !subpath.closed && i == 0 {
            left_chain.push(offset(pts[0], lefts[0], half));
            right_chain.push(offset(pts[0], lefts[0], -half));
            continue;
        }
        if !subpath.closed && i == n - 1 {
            left_chain.push(offset(pts[i], lefts[seg_count - 1], half));
            right_chain.push(offset(pts[i], lefts[seg_count - 1], -half));
            continue;
        }
        let prev = if subpath.closed {
            (i + seg_count - 1) % seg_count
        } else {
            i - 1
        };
        let next = i % seg_count;
        let cross = fixed::mul(dirs[prev].0, dirs[next].1) - fixed::mul(dirs[prev].1, dirs[next].0);
        if cross > 0 {
            // Turning right (y-down): the left side bulges.
            left_chain.push(offset(pts[i], lefts[prev], half));
            arc(&mut left_chain, pts[i], half, lefts[prev], lefts[next]);
            right_chain.push(offset(pts[i], lefts[prev], -half));
            right_chain.push(offset(pts[i], lefts[next], -half));
        } else if cross < 0 {
            left_chain.push(offset(pts[i], lefts[prev], half));
            left_chain.push(offset(pts[i], lefts[next], half));
            right_chain.push(offset(pts[i], lefts[prev], -half));
            arc(
                &mut right_chain,
                pts[i],
                half,
                (-lefts[prev].0, -lefts[prev].1),
                (-lefts[next].0, -lefts[next].1),
            );
        } else {
            left_chain.push(offset(pts[i], lefts[next], half));
            right_chain.push(offset(pts[i], lefts[next], -half));
        }
    }
    // Assemble: left chain forward, end cap, right chain backward, start
    // cap. For a closed subpath the right chain winds the other way, so the
    // fill keeps its hole.
    let first = left_chain[0];
    builder.move_to(first.0, first.1);
    for &p in &left_chain[1..] {
        builder.line_to(p.0, p.1);
    }
    if subpath.closed {
        builder.close();
        let reversed: Vec<Point> = right_chain.iter().rev().copied().collect();
        builder.move_to(reversed[0].0, reversed[0].1);
        for &p in &reversed[1..] {
            builder.line_to(p.0, p.1);
        }
        builder.close();
        return;
    }
    match cap {
        Cap::Butt => {
            for &p in right_chain.iter().rev() {
                builder.line_to(p.0, p.1);
            }
        }
        Cap::Round => {
            let end_left = lefts[seg_count - 1];
            arc_cap(
                builder,
                pts[n - 1],
                half,
                end_left,
                (-end_left.0, -end_left.1),
                dirs[seg_count - 1],
            );
            for &p in right_chain.iter().rev().skip(1) {
                builder.line_to(p.0, p.1);
            }
            arc_cap(
                builder,
                pts[0],
                half,
                (-lefts[0].0, -lefts[0].1),
                lefts[0],
                (-dirs[0].0, -dirs[0].1),
            );
        }
    }
    builder.close();
}

/// A join arc: bisector subdivision from `u0` to `u1` (unit directions from
/// `center`), appended as points.
fn arc(out: &mut Vec<Point>, center: Point, r: Fixed, u0: Point, u1: Point) {
    let mut points = Vec::new();
    arc_points(&mut points, center, r, u0, u1, (0, 0), 0);
    out.extend(points);
}

/// A cap arc: like `arc`, but emitted as line verbs, with `fallback_mid`
/// orienting the 180-degree case (the two cap directions are opposite).
fn arc_cap(builder: &mut PathBuilder, center: Point, r: Fixed, u0: Point, u1: Point, out: Point) {
    let mut points = Vec::new();
    arc_points(&mut points, center, r, u0, u1, out, 0);
    for p in points {
        builder.line_to(p.0, p.1);
    }
}

fn arc_points(
    out: &mut Vec<Point>,
    center: Point,
    r: Fixed,
    u0: Point,
    u1: Point,
    fallback_mid: Point,
    depth: u32,
) {
    let dot = fixed::mul(u0.0, u1.0) + fixed::mul(u0.1, u1.1);
    // Flat enough when r * (1 - cos(theta/2)) <= TOLERANCE, tested without
    // trigonometry as cos(theta) >= 2*(1 - TOLERANCE/r)^2 - 1.
    let t = fixed::div(TOLERANCE, r.max(1));
    let inner = fixed::mul(ONE - t, ONE - t);
    let flat = dot > 0 && dot >= 2 * inner - ONE;
    if depth >= 8 || flat {
        out.push((
            center.0 + fixed::mul(u1.0, r),
            center.1 + fixed::mul(u1.1, r),
        ));
        return;
    }
    let sum = (u0.0 + u1.0, u0.1 + u1.1);
    let mid = if fixed::length(sum.0, sum.1) < ONE / 64 {
        fallback_mid
    } else {
        fixed::scale_to(sum.0, sum.1, ONE)
    };
    arc_points(out, center, r, u0, mid, fallback_mid, depth + 1);
    arc_points(out, center, r, mid, u1, fallback_mid, depth + 1);
}
