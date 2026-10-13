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
pub fn rasterize_into(mask: &mut Mask, path: &Path, tx: i32, ty: i32) {
    let width = mask.width as i32;
    let height = mask.height as i32;
    if width == 0 || height == 0 {
        return;
    }
    let mut cover = alloc::vec![0i32; width as usize];
    let mut area = alloc::vec![0i64; width as usize];
    let shift_x = tx << 16;
    let shift_y = ty << 16;
    let subpaths = path.flatten();
    for row in 0..height {
        let row_top = row << 16;
        for subpath in &subpaths {
            for pair in subpath.points.windows(2) {
                let (mut x0, mut y0) = (pair[0].0 + shift_x, pair[0].1 + shift_y);
                let (mut x1, mut y1) = (pair[1].0 + shift_x, pair[1].1 + shift_y);
                if y0 == y1 {
                    continue;
                }
                // Normalize to y-ascending; the sign carries the winding.
                let sign: i32 = if y0 < y1 { 1 } else { -1 };
                if y0 > y1 {
                    core::mem::swap(&mut x0, &mut x1);
                    core::mem::swap(&mut y0, &mut y1);
                }
                let ya = y0.max(row_top);
                let yb = y1.min(row_top + ONE);
                if ya >= yb {
                    continue;
                }
                let xa = lerp_x(x0, y0, x1, y1, ya);
                let xb = lerp_x(x0, y0, x1, y1, yb);
                add_trapezoid(&mut cover, &mut area, width, sign, (xa, ya), (xb, yb));
            }
        }
        // Sweep the row: running cover plus local area give each pixel's
        // coverage. The sweep zeroes the cells for the next row.
        let mut c: i32 = 0;
        let out = mask.row_mut(row as u32);
        for (col, cell) in out.iter_mut().enumerate() {
            c += cover[col];
            let a = area[col] - ((c as i64) << 16);
            let clamped = a.unsigned_abs().min(1u64 << 32);
            *cell = ((clamped * 255 + (1 << 31)) >> 32) as u8;
            cover[col] = 0;
            area[col] = 0;
        }
    }
}

/// x at y along the segment (x0,y0)-(x1,y1), with y0 != y1.
fn lerp_x(x0: Fixed, y0: Fixed, x1: Fixed, y1: Fixed, y: Fixed) -> Fixed {
    (x0 as i64 + ((x1 - x0) as i64 * (y - y0) as i64) / (y1 - y0) as i64) as Fixed
}

/// y at x along the segment; only called with x0 != x1.
fn lerp_y(x0: Fixed, y0: Fixed, x1: Fixed, y1: Fixed, x: Fixed) -> Fixed {
    (y0 as i64 + ((y1 - y0) as i64 * (x - x0) as i64) / (x1 - x0) as i64) as Fixed
}

/// Accumulate one edge's in-row trapezoid into the cover/area cells.
/// `(xa, ya)` and `(xb, yb)` are the row-clipped endpoints with ya < yb, and
/// `sign` is the winding direction. Edges left of the mask still add their
/// winding to every mask pixel; edges right of it affect nothing.
fn add_trapezoid(cover: &mut [i32], area: &mut [i64], width: i32, sign: i32, a: Point, b: Point) {
    let (xa, ya) = a;
    let (xb, yb) = b;
    let dy = yb - ya;
    if xa == xb {
        let col = xa >> 16;
        if col < 0 {
            cover[0] += sign * dy;
        } else if col < width {
            let fx = (xa & 0xFFFF) as i64;
            cover[col as usize] += sign * dy;
            area[col as usize] += sign as i64 * fx * dy as i64;
        }
        return;
    }
    let (lo, hi) = if xa < xb { (xa, xb) } else { (xb, xa) };
    let first = lo >> 16;
    let last = (hi - 1) >> 16;
    let mut left_cover: i32 = 0;
    for col in first..=last.min(width - 1) {
        let cell_left = col << 16;
        let cl = lo.max(cell_left);
        let cr = hi.min(cell_left + ONE);
        if cl >= cr {
            continue;
        }
        let yl = lerp_y(xa, ya, xb, yb, cl);
        let yr = lerp_y(xa, ya, xb, yb, cr);
        // The piece height is order-independent: the column walk crosses
        // down-left edges in reverse, so take the absolute value.
        let piece_dy = (yr - yl).abs();
        if col < 0 {
            left_cover += sign * piece_dy;
            continue;
        }
        let fx0 = (cl - cell_left) as i64;
        let fx1 = (cr - cell_left) as i64;
        cover[col as usize] += sign * piece_dy;
        area[col as usize] += sign as i64 * (fx0 + fx1) * piece_dy as i64 / 2;
    }
    cover[0] += left_cover;
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
