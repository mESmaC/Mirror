//! Clipping (0027c-D7): an intersect-only stack of axis-aligned rectangles,
//! plus an optional A8 clip mask for path-shaped clips.

use crate::raster::Mask;
use alloc::vec::Vec;

/// An axis-aligned pixel rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w as i32
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h as i32
    }

    /// The overlap of two rectangles, or None when they do not touch.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= x || bottom <= y {
            return None;
        }
        Some(Rect::new(x, y, (right - x) as u32, (bottom - y) as u32))
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }
}

/// The active clip: `bounds` of None means unclipped; a mask multiplies
/// coverage. A zero-size rectangle clips everything away.
#[derive(Clone, Debug, Default)]
pub struct Clip {
    bounds: Option<Rect>,
    mask: Option<Mask>,
    mask_origin: (i32, i32),
}

impl Clip {
    /// An unclipped state.
    pub fn none() -> Clip {
        Clip::default()
    }

    /// A single-rectangle clip.
    pub fn rect(rect: Rect) -> Clip {
        Clip {
            bounds: Some(rect),
            mask: None,
            mask_origin: (0, 0),
        }
    }

    /// Intersect with another rectangle.
    pub fn and_rect(&mut self, rect: Rect) {
        self.bounds = Some(match self.bounds {
            None => rect,
            Some(b) => b.intersect(&rect).unwrap_or(Rect::new(0, 0, 0, 0)),
        });
    }

    /// Multiply coverage by a mask placed at `origin` in the same
    /// coordinates as the clip rectangles.
    pub fn and_mask(&mut self, mask: Mask, origin: (i32, i32)) {
        self.mask = Some(mask);
        self.mask_origin = origin;
    }

    /// The clip bounds, or None when unclipped.
    pub fn bounds(&self) -> Option<Rect> {
        self.bounds
    }

    /// The clip's coverage at a pixel: 0, 255, or the mask's value.
    pub fn coverage(&self, x: i32, y: i32) -> u8 {
        let rect_ok = match &self.bounds {
            None => true,
            Some(b) => b.contains(x, y),
        };
        if !rect_ok {
            return 0;
        }
        match &self.mask {
            None => 255,
            Some(m) => m.get(x - self.mask_origin.0, y - self.mask_origin.1),
        }
    }

    /// True when nothing limits the clip.
    pub fn is_none(&self) -> bool {
        self.bounds.is_none() && self.mask.is_none()
    }
}

/// A stack of rectangles intersected into one, for nested clipping.
#[derive(Default)]
pub struct ClipStack {
    clip: Clip,
    saved: Vec<Option<Rect>>,
}

impl ClipStack {
    pub fn new(bounds: Rect) -> ClipStack {
        ClipStack {
            clip: Clip::rect(bounds),
            saved: Vec::new(),
        }
    }

    pub fn push(&mut self, rect: Rect) {
        self.saved.push(self.clip.bounds);
        self.clip.and_rect(rect);
    }

    pub fn pop(&mut self) {
        if let Some(bounds) = self.saved.pop() {
            self.clip.bounds = bounds;
        }
    }

    pub fn clip(&self) -> &Clip {
        &self.clip
    }
}
