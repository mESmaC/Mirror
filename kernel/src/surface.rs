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
    /// Init's small probe surface. Only smoke kernels run init (spec 0010-D3).
    #[cfg(feature = "boot-smoke")]
    pub fn new(driver: &'borrow mut Driver<'fb>) -> Result<Self, SurfaceError> {
        Self::with_extent(driver, 256, 128)
    }

    /// A persistent desktop receives a larger resource extent, using the same
    /// bounded tile transport. Window/layout/composition policy stays outside
    /// the kernel; this grants only a centered rectangle in the current mode.
    pub fn desktop(driver: &'borrow mut Driver<'fb>) -> Result<Self, SurfaceError> {
        Self::with_extent(driver, 1280, 800)
    }

    fn with_extent(
        driver: &'borrow mut Driver<'fb>,
        max_width: usize,
        max_height: usize,
    ) -> Result<Self, SurfaceError> {
        let info = driver.info();
        let width = info.width.min(max_width);
        let height = info.height.min(max_height);
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
        for (destination, field) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(fields) {
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
    // Thin guarded surfaces exercise each desktop extent limit and centering
    // without reserving a full multi-megabyte framebuffer on the boot stack.
    for (width, height) in [(1282, 2), (2, 802), (2, 2)] {
        let mut bytes = [0xa5u8; 10_300];
        let pitch = width * 4 + 4;
        let end = 8 + pitch * height;
        let owned_width = width.min(1280);
        let owned_height = height.min(800);
        let origin_x = (width - owned_width) / 2;
        let origin_y = (height - owned_height) / 2;
        {
            let mut driver =
                crate::framebuffer::test_rgb32_driver(&mut bytes[8..end], width, height, pitch)
                    .unwrap();
            {
                let initial = Session::new(&mut driver).unwrap().info();
                assert_eq!(
                    u32::from_le_bytes(initial[4..8].try_into().unwrap()),
                    width.min(256) as u32
                );
                assert_eq!(
                    u32::from_le_bytes(initial[8..12].try_into().unwrap()),
                    height.min(128) as u32
                );
            }
            let mut desktop = Session::desktop(&mut driver).unwrap();
            let info = desktop.info();
            let fields = [1, owned_width as u32, owned_height as u32, 1, 16, 0];
            for (actual, expected) in info.as_chunks::<4>().0.iter().zip(fields) {
                assert_eq!(*actual, expected.to_le_bytes());
            }
            let mut packet = [0u8; 20];
            packet[8..12].copy_from_slice(&1u32.to_le_bytes());
            packet[12..16].copy_from_slice(&1u32.to_le_bytes());
            packet[16..20].copy_from_slice(&[11, 22, 33, 0xff]);
            assert_eq!(desktop.present(&packet), Ok(1));
            packet[..4].copy_from_slice(&((owned_width - 1) as u32).to_le_bytes());
            packet[4..8].copy_from_slice(&((owned_height - 1) as u32).to_le_bytes());
            assert_eq!(desktop.present(&packet), Ok(1));
            assert_eq!(desktop.totals(), (2, 2));
        }
        for (index, &actual) in bytes.iter().enumerate() {
            let expected = if (8..end).contains(&index) {
                let offset = index - 8;
                let row = offset / pitch;
                let column_byte = offset % pitch;
                let column = column_byte / 4;
                if column_byte < width * 4
                    && ((column, row) == (origin_x, origin_y)
                        || (column, row)
                            == (origin_x + owned_width - 1, origin_y + owned_height - 1))
                {
                    [33, 22, 11, 0][column_byte % 4]
                } else {
                    0xa5
                }
            } else {
                0xa5
            };
            assert_eq!(
                actual, expected,
                "desktop extent touched unexpected byte {index}"
            );
        }
    }
    crate::serial::println!("MIRROR_SURFACE_TESTS_OK");
}
