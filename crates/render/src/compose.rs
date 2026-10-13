//! Damage-limited composition (0027c-D8): a back-to-front layer list drawn
//! into a target, computing only pixels inside the damage rectangles. The
//! layer id space reserves [`TRUSTED_TOP`] for the compositor-owned trusted
//! prompt and experiment marker (RD2, A9a); 0049 draws what goes there.

use crate::clip::{Clip, Rect};
use crate::draw::{self, Filter};
use crate::frame::{FrameProbe, Span};
use crate::surface::Surface;
use alloc::vec::Vec;

/// The reserved layer id above every app layer (RD2, A9a). Only the
/// compositor's trusted content may use it.
pub const TRUSTED_TOP: u16 = u16::MAX;

/// One composited layer: a surface drawn into a destination rectangle
/// (scaled when the rectangle differs from the surface size).
pub struct Layer<'a> {
    /// Z-order; larger draws on top. [`TRUSTED_TOP`] is reserved.
    pub id: u16,
    pub surface: &'a Surface,
    pub dest: Rect,
}

/// Compose `layers` (any order; sorted by id) into `target`, repainting
/// only the damage rectangles. Damaged pixels start from transparent black:
/// the caller's bottom layer is the backdrop.
pub fn compose(
    target: &mut Surface,
    layers: &[Layer],
    damage: &[Rect],
    probe: &mut dyn FrameProbe,
) {
    probe.begin(Span::Compose);
    let mut order: Vec<usize> = (0..layers.len()).collect();
    order.sort_by_key(|&i| layers[i].id);
    let bounds = Rect::new(0, 0, target.width(), target.height());
    for &d in damage {
        let Some(area) = d.intersect(&bounds) else {
            continue;
        };
        let clip = Clip::rect(area);
        // Damage starts transparent; layers rebuild it back to front.
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                target.set(x, y, crate::color::Rgba::TRANSPARENT);
            }
        }
        for &i in &order {
            draw::blit(
                target,
                layers[i].surface,
                layers[i].dest,
                Filter::Bilinear,
                &clip,
                probe,
            );
        }
    }
    probe.end(Span::Compose);
}
