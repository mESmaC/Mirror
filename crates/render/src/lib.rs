#![no_std]
#![forbid(unsafe_code)]
//! Mirror's software renderer (spec 0027c): premultiplied RGBA surfaces,
//! antialiased shapes on a shared path rasterizer, cached soft shadows,
//! dithered gradients, clipping, filtered scaled blits and damage-limited
//! composition, plus the frame-time hooks the compositor measures with and
//! the reserved trusted-top layer id.
//!
//! All geometry and paint math is integer fixed point (16.16 coordinates,
//! 8-bit coverage), so renders are byte-exact on every host (0027c-D3) and
//! golden-image tests can compare pixels exactly. The path rasterizer
//! (`raster`) is shared with the text stack: 0086a rasterizes glyph outlines
//! through it unchanged (0027c-D4).
//!
//! The crate draws nothing on its own and owns no clock: callers wrap spans
//! with a [`frame::FrameProbe`] when they want timings (0027c-D9).

extern crate alloc;

pub mod clip;
pub mod color;
pub mod compose;
pub mod draw;
pub mod fixed;
pub mod frame;
pub mod paint;
pub mod path;
pub mod raster;
pub mod shadow;
pub mod surface;
pub mod tokens;

pub use clip::Clip;
pub use color::Rgba;
pub use compose::{Layer, TRUSTED_TOP};
pub use frame::{FrameProbe, NoProbe, Span};
pub use paint::{GradientStop, Paint};
pub use path::{Path, PathBuilder};
pub use raster::Mask;
pub use surface::Surface;

#[cfg(test)]
mod tests;
