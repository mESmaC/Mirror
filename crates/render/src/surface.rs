//! Owned pixel surfaces.

use crate::color::Rgba;
use alloc::vec;
use alloc::vec::Vec;

/// A row-major premultiplied RGBA8 pixel buffer (0027c-D2).
pub struct Surface {
    width: u32,
    height: u32,
    pixels: Vec<Rgba>,
}

impl Surface {
    /// A transparent surface of the given size.
    pub fn new(width: u32, height: u32) -> Surface {
        Surface {
            width,
            height,
            pixels: vec![Rgba::TRANSPARENT; width as usize * height as usize],
        }
    }

    /// A surface filled with one color.
    pub fn filled(width: u32, height: u32, color: Rgba) -> Surface {
        Surface {
            width,
            height,
            pixels: vec![color; width as usize * height as usize],
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn pixels(&self) -> &[Rgba] {
        &self.pixels
    }

    /// The pixel at (x, y), or transparent outside the surface.
    pub fn get(&self, x: i32, y: i32) -> Rgba {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return Rgba::TRANSPARENT;
        }
        self.pixels[y as usize * self.width as usize + x as usize]
    }

    /// Source-over one premultiplied pixel at (x, y); outside is a no-op.
    pub(crate) fn blend(&mut self, x: i32, y: i32, src: Rgba) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let i = y as usize * self.width as usize + x as usize;
        self.pixels[i] = src.over(self.pixels[i]);
    }

    /// Overwrite one pixel at (x, y); outside is a no-op.
    pub fn set(&mut self, x: i32, y: i32, src: Rgba) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        self.pixels[y as usize * self.width as usize + x as usize] = src;
    }

    /// Fill the whole surface with one color.
    pub fn clear(&mut self, color: Rgba) {
        self.pixels.fill(color);
    }
}
