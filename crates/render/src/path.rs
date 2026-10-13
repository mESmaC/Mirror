//! Vector paths and flattening (0027c-D4). Coordinates are 16.16 fixed
//! point; curves flatten to line segments at a fixed 1/16 px tolerance, so
//! rasterization is identical on every host.

use crate::fixed::{self, Fixed};
use alloc::vec::Vec;

/// A 16.16 point.
pub type Point = (Fixed, Fixed);

/// One path verb with its points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    MoveTo(Point),
    LineTo(Point),
    QuadTo(Point, Point),
    CubicTo(Point, Point, Point),
    Close,
}

/// An immutable path: a list of verbs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Path {
    verbs: Vec<Verb>,
}

impl Path {
    pub fn verbs(&self) -> &[Verb] {
        &self.verbs
    }

    /// The bounding box of every control point, as (min, max) in 16.16,
    /// or None for an empty path.
    pub fn bounds(&self) -> Option<(Point, Point)> {
        let mut min: Option<Point> = None;
        let mut max: Point = (0, 0);
        let mut grow = |p: Point| match min {
            None => {
                min = Some(p);
                max = p;
            }
            Some(m) => {
                min = Some((m.0.min(p.0), m.1.min(p.1)));
                max = (max.0.max(p.0), max.1.max(p.1));
            }
        };
        for verb in &self.verbs {
            match *verb {
                Verb::MoveTo(p) | Verb::LineTo(p) => grow(p),
                Verb::QuadTo(c, p) => {
                    grow(c);
                    grow(p);
                }
                Verb::CubicTo(c1, c2, p) => {
                    grow(c1);
                    grow(c2);
                    grow(p);
                }
                Verb::Close => {}
            }
        }
        min.map(|m| (m, max))
    }

    /// Append the path's line-segment outline to `out`, flattening curves at
    /// the fixed tolerance. Closed subpaths repeat their start point at the
    /// end. The output is a list of polylines, one per subpath, each marked
    /// open or closed.
    pub fn flatten(&self) -> Vec<Subpath> {
        let mut out = Vec::new();
        let mut current: Vec<Point> = Vec::new();
        let mut pen: Point = (0, 0);
        let mut start: Point = (0, 0);
        for verb in &self.verbs {
            match *verb {
                Verb::MoveTo(p) => {
                    if current.len() > 1 {
                        out.push(Subpath::open(core::mem::take(&mut current)));
                    }
                    current.clear();
                    current.push(p);
                    pen = p;
                    start = p;
                }
                Verb::LineTo(p) => {
                    current.push(p);
                    pen = p;
                }
                Verb::QuadTo(c, p) => {
                    flatten_quad(pen, c, p, 0, &mut current);
                    pen = p;
                }
                Verb::CubicTo(c1, c2, p) => {
                    flatten_cubic(pen, c1, c2, p, 0, &mut current);
                    pen = p;
                }
                Verb::Close => {
                    if current.len() > 1 {
                        if *current.last().unwrap_or(&start) != start {
                            current.push(start);
                        }
                        out.push(Subpath::closed(core::mem::take(&mut current)));
                    }
                    pen = start;
                }
            }
        }
        if current.len() > 1 {
            out.push(Subpath::open(current));
        }
        out
    }
}

/// One flattened subpath.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subpath {
    pub points: Vec<Point>,
    pub closed: bool,
}

impl Subpath {
    fn open(points: Vec<Point>) -> Subpath {
        Subpath {
            points,
            closed: false,
        }
    }

    fn closed(points: Vec<Point>) -> Subpath {
        Subpath {
            points,
            closed: true,
        }
    }
}

/// Builds paths.
#[derive(Default)]
pub struct PathBuilder {
    verbs: Vec<Verb>,
}

impl PathBuilder {
    pub fn new() -> PathBuilder {
        PathBuilder { verbs: Vec::new() }
    }

    pub fn move_to(&mut self, x: Fixed, y: Fixed) -> &mut Self {
        self.verbs.push(Verb::MoveTo((x, y)));
        self
    }

    pub fn line_to(&mut self, x: Fixed, y: Fixed) -> &mut Self {
        self.verbs.push(Verb::LineTo((x, y)));
        self
    }

    pub fn quad_to(&mut self, cx: Fixed, cy: Fixed, x: Fixed, y: Fixed) -> &mut Self {
        self.verbs.push(Verb::QuadTo((cx, cy), (x, y)));
        self
    }

    pub fn cubic_to(
        &mut self,
        c1x: Fixed,
        c1y: Fixed,
        c2x: Fixed,
        c2y: Fixed,
        x: Fixed,
        y: Fixed,
    ) -> &mut Self {
        self.verbs
            .push(Verb::CubicTo((c1x, c1y), (c2x, c2y), (x, y)));
        self
    }

    pub fn close(&mut self) -> &mut Self {
        self.verbs.push(Verb::Close);
        self
    }

    /// An axis-aligned rectangle.
    pub fn rect(&mut self, x: Fixed, y: Fixed, w: Fixed, h: Fixed) -> &mut Self {
        self.move_to(x, y)
            .line_to(x + w, y)
            .line_to(x + w, y + h)
            .line_to(x, y + h)
            .close()
    }

    /// A rectangle with corner radius `r` (clamped to half the smaller
    /// side), built from four quarter-circle cubic arcs.
    pub fn rounded_rect(&mut self, x: Fixed, y: Fixed, w: Fixed, h: Fixed, r: Fixed) -> &mut Self {
        let r = r.min(w / 2).min(h / 2).max(0);
        if r == 0 {
            return self.rect(x, y, w, h);
        }
        // The quarter-circle cubic control distance, k * r with
        // k = 4/3 * (sqrt(2) - 1) = 0.5522847..., in 16.16.
        const K: Fixed = 36195;
        let k = fixed::mul(r, K);
        self.move_to(x + r, y)
            .line_to(x + w - r, y)
            .cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r)
            .line_to(x + w, y + h - r)
            .cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h)
            .line_to(x + r, y + h)
            .cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r)
            .line_to(x, y + r)
            .cubic_to(x, y + r - k, x + r - k, y, x + r, y)
            .close()
    }

    pub fn build(self) -> Path {
        Path { verbs: self.verbs }
    }
}

/// Squared distance of `p` from the chord `a`-`b`, scaled by the squared
/// chord length (so the comparison stays in integers): the curve is flat
/// when cross(p)^2 <= TOLERANCE^2 * len^2.
fn flat_enough(a: Point, p: Point, b: Point) -> bool {
    let dx = (b.0 - a.0) as i64;
    let dy = (b.1 - a.1) as i64;
    let len2 = dx * dx + dy * dy;
    let tol = fixed::TOLERANCE as i64;
    if len2 == 0 {
        // A degenerate chord flattens on the control distance alone.
        let ex = (p.0 - a.0) as i64;
        let ey = (p.1 - a.1) as i64;
        return ex * ex + ey * ey <= tol * tol;
    }
    let cross = dx * (p.1 - a.1) as i64 - dy * (p.0 - a.0) as i64;
    // |cross| <= tol * len, avoiding a cross*cross that overflows i64.
    let len = fixed::sqrt32(len2) as i64;
    cross.abs() <= tol * len
}

const MAX_DEPTH: u32 = 12;

fn flatten_quad(a: Point, c: Point, b: Point, depth: u32, out: &mut Vec<Point>) {
    if depth >= MAX_DEPTH || flat_enough(a, c, b) {
        out.push(b);
        return;
    }
    let (ab, bc) = (mid(a, c), mid(c, b));
    let m = mid(ab, bc);
    flatten_quad(a, ab, m, depth + 1, out);
    flatten_quad(m, bc, b, depth + 1, out);
}

fn flatten_cubic(a: Point, c1: Point, c2: Point, b: Point, depth: u32, out: &mut Vec<Point>) {
    if depth >= MAX_DEPTH || (flat_enough(a, c1, b) && flat_enough(a, c2, b)) {
        out.push(b);
        return;
    }
    let (ab, bc1) = (mid(a, c1), mid(c1, c2));
    let bc = mid(c2, b);
    let (m1, m2) = (mid(ab, bc1), mid(bc1, bc));
    let m = mid(m1, m2);
    flatten_cubic(a, ab, m1, m, depth + 1, out);
    flatten_cubic(m, m2, bc, b, depth + 1, out);
}

/// The midpoint of two 16.16 points (truncating, deterministic).
fn mid(a: Point, b: Point) -> Point {
    ((a.0 + b.0) / 2, (a.1 + b.1) / 2)
}
