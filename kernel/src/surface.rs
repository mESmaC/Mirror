//! Bounded native pixel transport over an exclusively borrowed framebuffer.
//!
//! The syscall layer resolves authority and snapshots user bytes first. This
//! module supplies only a resource extent and pixel transfer: all image content
//! belongs to userspace, with no application layout, fonts, or widget commands.

use crate::framebuffer::{BlitError, Driver, SurfaceRegion};

pub const INFO_SIZE: usize = 24;
pub const MAX_TILE: usize = crate::framebuffer::MAX_SURFACE_TILE;
pub const MAX_PACKET: usize = 16 + MAX_TILE * MAX_TILE * 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceError {
    InvalidPacket,
    Blit(BlitError),
    CounterOverflow,
}

pub struct Session<'borrow, 'fb> {
    driver: &'borrow mut Driver<'fb>,
    region: SurfaceRegion,
    tiles: u64,
    pixels: u64,
}

impl<'borrow, 'fb> Session<'borrow, 'fb> {
    pub fn new(driver: &'borrow mut Driver<'fb>) -> Result<Self, SurfaceError> {
        let info = driver.info();
        let width = info.width.min(256);
        let height = info.height.min(128);
        let region = SurfaceRegion::new(
            info,
            (info.width - width) / 2,
            (info.height - height) / 2,
            width,
            height,
        )
        .map_err(SurfaceError::Blit)?;
        Ok(Self {
            driver,
            region,
            tiles: 0,
            pixels: 0,
        })
    }

    /// Explicit little-endian u32 wire fields, independent of Rust layout:
    /// version, owned width/height, RGBX format, maximum tile edge, reserved.
    pub fn info(&self) -> [u8; INFO_SIZE] {
        let (width, height) = self.region.dimensions();
        let fields = [1, width as u32, height as u32, 1, MAX_TILE as u32, 0];
        let mut bytes = [0u8; INFO_SIZE];
        for (destination, field) in bytes.chunks_exact_mut(4).zip(fields) {
            destination.copy_from_slice(&field.to_le_bytes());
        }
        bytes
    }

    /// Packet: x,y,width,height as four LE u32s, then exact packed RGBX bytes.
    /// Success returns pixel count; failures preserve both pixels and totals.
    pub fn present(&mut self, packet: &[u8]) -> Result<usize, SurfaceError> {
        if !(20..=MAX_PACKET).contains(&packet.len()) {
            return Err(SurfaceError::InvalidPacket);
        }
        let field = |offset: usize| {
            u32::from_le_bytes(
                packet[offset..offset + 4]
                    .try_into()
                    .expect("checked header length"),
            ) as usize
        };
        let (x, y, width, height) = (field(0), field(4), field(8), field(12));
        if !(1..=MAX_TILE).contains(&width) || !(1..=MAX_TILE).contains(&height) {
            return Err(SurfaceError::InvalidPacket);
        }
        let pixels = width * height;
        if packet.len() != 16 + pixels * 4 {
            return Err(SurfaceError::InvalidPacket);
        }
        let next_tiles = self
            .tiles
            .checked_add(1)
            .ok_or(SurfaceError::CounterOverflow)?;
        let next_pixels = self
            .pixels
            .checked_add(pixels as u64)
            .ok_or(SurfaceError::CounterOverflow)?;
        self.driver
            .blit_rgbx(self.region, x, y, width, height, &packet[16..])
            .map_err(SurfaceError::Blit)?;
        self.tiles = next_tiles;
        self.pixels = next_pixels;
        Ok(pixels)
    }

    pub fn totals(&self) -> (u64, u64) {
        (self.tiles, self.pixels)
    }
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    crate::framebuffer::test_surface_upload();
    crate::serial::println!("MIRROR_SURFACE_TESTS_OK");
}
