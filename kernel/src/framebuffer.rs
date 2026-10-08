//! Exclusive, allocation-free drawing into an optional Limine boot framebuffer.
//!
//! This preserves the firmware-selected mode; it is not a GPU modesetting or
//! acceleration driver. All device writes are volatile bytes, and no framebuffer
//! slice/reference or read is created, including for initially uninitialized VRAM.
//! Protocol: https://github.com/Limine-Bootloader/limine-protocol/blob/trunk/PROTOCOL.md
//! Binding: https://docs.rs/limine/0.5.0/limine/framebuffer/struct.Framebuffer.html

use core::{marker::PhantomData, ptr::NonNull};
use limine::framebuffer::{Framebuffer, MemoryModel};

const MAX_DIMENSION: u64 = 8192;
const MAX_BYTE_LEN: u64 = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FramebufferError {
    UnsupportedMemoryModel,
    UnsupportedPixelFormat,
    InvalidMasks,
    InvalidDimensions,
    InvalidPitch,
    BufferTooLarge,
    NullAddress,
    AddressOverflow,
    NonCanonicalAddress,
    #[cfg(feature = "boot-smoke")]
    BufferTooSmall,
}

#[derive(Clone, Copy, Debug)]
pub struct Info {
    pub width: usize,
    pub height: usize,
    pub pitch: usize,
    pub bpp: u16,
    pub byte_len: usize,
}

#[derive(Clone, Copy)]
struct Mask {
    size: u8,
    shift: u8,
}

#[derive(Clone, Copy)]
struct Description {
    width: u64,
    height: u64,
    pitch: u64,
    bpp: u16,
    red: Mask,
    green: Mask,
    blue: Mask,
}

#[derive(Clone, Copy)]
struct Layout {
    info: Info,
    bytes_per_pixel: usize,
    red_shift: u8,
    green_shift: u8,
    blue_shift: u8,
}

impl Layout {
    fn validate(address: usize, description: Description) -> Result<Self, FramebufferError> {
        if !matches!(description.bpp, 24 | 32) {
            return Err(FramebufferError::UnsupportedPixelFormat);
        }
        if description.width == 0
            || description.height == 0
            || description.width > MAX_DIMENSION
            || description.height > MAX_DIMENSION
        {
            return Err(FramebufferError::InvalidDimensions);
        }
        let mut occupied = 0u32;
        for mask in [description.red, description.green, description.blue] {
            if mask.size != 8 || u16::from(mask.shift) + 8 > description.bpp {
                return Err(FramebufferError::InvalidMasks);
            }
            let bits = 0xffu32 << mask.shift;
            if occupied & bits != 0 {
                return Err(FramebufferError::InvalidMasks);
            }
            occupied |= bits;
        }
        let bytes_per_pixel = u64::from(description.bpp / 8);
        let row_bytes = description
            .width
            .checked_mul(bytes_per_pixel)
            .ok_or(FramebufferError::BufferTooLarge)?;
        if description.pitch < row_bytes {
            return Err(FramebufferError::InvalidPitch);
        }
        let byte_len = description
            .pitch
            .checked_mul(description.height)
            .filter(|&length| length <= MAX_BYTE_LEN)
            .ok_or(FramebufferError::BufferTooLarge)?;
        let byte_len = usize::try_from(byte_len).map_err(|_| FramebufferError::BufferTooLarge)?;
        if address == 0 {
            return Err(FramebufferError::NullAddress);
        }
        let end = address
            .checked_add(byte_len - 1)
            .ok_or(FramebufferError::AddressOverflow)?;
        // Mirror currently supports four-level x86-64 page tables only. Check
        // both ends so a valid start cannot straddle the noncanonical hole.
        let canonical = |value: usize| (value as u64) >> 47 == 0 || (value as u64) >> 47 == 0x1ffff;
        if !canonical(address) || !canonical(end) {
            return Err(FramebufferError::NonCanonicalAddress);
        }
        Ok(Self {
            info: Info {
                width: description.width as usize,
                height: description.height as usize,
                pitch: description.pitch as usize,
                bpp: description.bpp,
                byte_len,
            },
            bytes_per_pixel: bytes_per_pixel as usize,
            red_shift: description.red.shift,
            green_shift: description.green.shift,
            blue_shift: description.blue.shift,
        })
    }

    fn encode(self, color: Color) -> [u8; 4] {
        (u32::from(color.red) << self.red_shift
            | u32::from(color.green) << self.green_shift
            | u32::from(color.blue) << self.blue_shift)
            .to_le_bytes()
    }
}

#[derive(Clone, Copy)]
struct Color {
    red: u8,
    green: u8,
    blue: u8,
}

impl Color {
    const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

const BACKGROUND: Color = Color::rgb(9, 14, 21);
const PANEL: Color = Color::rgb(17, 25, 36);
const BORDER: Color = Color::rgb(38, 52, 67);
const TEXT: Color = Color::rgb(230, 239, 245);
const MUTED: Color = Color::rgb(141, 161, 177);
const ACCENT: Color = Color::rgb(89, 221, 202);

/// A sole-writer handle. It is intentionally neither Clone nor Send/Sync.
pub struct Driver<'a> {
    address: NonNull<u8>,
    layout: Layout,
    _exclusive: PhantomData<&'a mut ()>,
}

impl<'a> Driver<'a> {
    /// Use the address already supplied by Limine; do not add the HHDM again.
    ///
    /// # Safety
    /// The caller must exclusively own all framebuffer writes for `'a`, reserve
    /// the backing framebuffer region, and retain its writable device mapping
    /// and cache attributes for the entire lifetime of the returned driver.
    /// Limine's reported `pitch * height` bytes must exist at the reported
    /// address. Metadata validation cannot prove the MMIO mapping exists.
    /// Do not invoke this twice for an overlapping framebuffer, or share access
    /// with an interrupt, another CPU, firmware, or a second display renderer.
    pub unsafe fn from_limine(framebuffer: &Framebuffer<'a>) -> Result<Self, FramebufferError> {
        if framebuffer.memory_model() != MemoryModel::RGB {
            return Err(FramebufferError::UnsupportedMemoryModel);
        }
        let description = Description {
            width: framebuffer.width(),
            height: framebuffer.height(),
            pitch: framebuffer.pitch(),
            bpp: framebuffer.bpp(),
            red: Mask {
                size: framebuffer.red_mask_size(),
                shift: framebuffer.red_mask_shift(),
            },
            green: Mask {
                size: framebuffer.green_mask_size(),
                shift: framebuffer.green_mask_shift(),
            },
            blue: Mask {
                size: framebuffer.blue_mask_size(),
                shift: framebuffer.blue_mask_shift(),
            },
        };
        let layout = Layout::validate(framebuffer.addr() as usize, description)?;
        Ok(Self {
            address: NonNull::new(framebuffer.addr()).ok_or(FramebufferError::NullAddress)?,
            layout,
            _exclusive: PhantomData,
        })
    }

    pub fn info(&self) -> Info {
        self.layout.info
    }

    fn pixel(&mut self, x: usize, y: usize, color: Color) -> bool {
        if x >= self.layout.info.width || y >= self.layout.info.height {
            return false;
        }
        let offset = y * self.layout.info.pitch + x * self.layout.bytes_per_pixel;
        let bytes = self.layout.encode(color);
        debug_assert!(offset + self.layout.bytes_per_pixel <= self.layout.info.byte_len);
        for (index, &byte) in bytes.iter().take(self.layout.bytes_per_pixel).enumerate() {
            // SAFETY: Construction guarantees the mapping and sole-writer
            // ownership. Pixel bounds plus validated pitch/length keep every
            // byte within it. wrapping_add avoids imposing a Rust-allocation
            // provenance requirement on MMIO; volatile writes never read VRAM.
            unsafe {
                self.address
                    .as_ptr()
                    .wrapping_add(offset + index)
                    .write_volatile(byte)
            };
        }
        true
    }

    fn fill_rect(&mut self, x: i64, y: i64, width: u64, height: u64, color: Color) {
        // i128 accommodates every i64 coordinate plus every u64 extent, so even
        // deliberately enormous/offscreen rectangles clip without wrapping.
        let horizontal = clipped_span(x, width, self.layout.info.width);
        let vertical = clipped_span(y, height, self.layout.info.height);
        for row in vertical.0..vertical.1 {
            for column in horizontal.0..horizontal.1 {
                self.pixel(column, row, color);
            }
        }
    }

    fn text(&mut self, x: i64, y: i64, text: &str, scale: u8, color: Color) {
        let scale = u64::from(scale.clamp(1, 8));
        // Bound work even if a future boot-status caller supplies a long string.
        for (index, byte) in text.bytes().take(128).enumerate() {
            let origin = x.saturating_add(index as i64 * 6 * scale as i64);
            for (row, bits) in glyph(byte.to_ascii_uppercase()).into_iter().enumerate() {
                for column in 0..5 {
                    if bits & (1 << (4 - column)) != 0 {
                        self.fill_rect(
                            origin.saturating_add(column * scale as i64),
                            y.saturating_add(row as i64 * scale as i64),
                            scale,
                            scale,
                            color,
                        );
                    }
                }
            }
        }
    }

    fn decimal(&mut self, x: i64, y: i64, mut value: u64, scale: u8, color: Color) -> usize {
        let mut digits = [b'0'; 20];
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let digits = core::str::from_utf8(&digits[start..]).expect("decimal digits are ASCII");
        self.text(x, y, digits, scale, color);
        digits.len() * 6 * usize::from(scale.clamp(1, 8))
    }

    fn panel(&self) -> (i64, i64, usize, usize) {
        let width = self.layout.info.width;
        let height = self.layout.info.height;
        let horizontal_margin = if width >= 160 { 32 } else { 0 };
        let vertical_margin = if height >= 160 { 32 } else { 0 };
        let panel_width = width.saturating_sub(horizontal_margin * 2).min(720);
        let panel_height = height.saturating_sub(vertical_margin * 2).min(400);
        (
            ((width - panel_width) / 2) as i64,
            ((height - panel_height) / 2) as i64,
            panel_width,
            panel_height,
        )
    }

    /// Draw actual boot identity and available RAM, without allocating a backbuffer.
    /// Drawing is immediate; this boot-only path does not implement vblank or flips.
    pub fn render_boot(&mut self, usable_bytes: u64) {
        let info = self.info();
        self.fill_rect(0, 0, info.width as u64, info.height as u64, BACKGROUND);
        let (x, y, width, height) = self.panel();
        self.fill_rect(x, y, width as u64, height as u64, BORDER);
        self.fill_rect(
            x + 1,
            y + 1,
            width.saturating_sub(2) as u64,
            height.saturating_sub(2) as u64,
            PANEL,
        );
        self.fill_rect(x + 1, y + 1, 4, height.saturating_sub(2) as u64, ACCENT);
        self.text(x + 32, y + 32, "MIRROR", 5, TEXT);
        self.text(x + 34, y + 82, "CUSTOM RUST KERNEL", 2, ACCENT);
        self.text(x + 34, y + 116, "X86-64 / NATIVE USERSPACE", 1, MUTED);
        self.fill_rect(x + 32, y + 151, width.saturating_sub(64) as u64, 1, BORDER);
        self.text(x + 34, y + 173, "USABLE RAM", 1, MUTED);
        let advance = self.decimal(x + 34, y + 197, usable_bytes / (1024 * 1024), 3, TEXT);
        self.text(x + 34 + advance as i64 + 10, y + 209, "MIB", 1, MUTED);
        self.text(x + 34, y + 250, "BOOT FRAMEBUFFER / RGB", 1, MUTED);
        self.update_status("KERNEL SERVICES STARTING", 35);
    }

    /// Replace the status line and progress bar; values above 100 are clamped.
    pub fn update_status(&mut self, status: &str, progress: u8) {
        let (x, y, width, height) = self.panel();
        let content_width = width.saturating_sub(64);
        let status_y = y + height.saturating_sub(82) as i64;
        self.fill_rect(x + 32, status_y, content_width as u64, 52, PANEL);
        self.text(x + 34, status_y, status, 1, TEXT);
        self.fill_rect(
            x + 34,
            status_y + 26,
            content_width.saturating_sub(4) as u64,
            6,
            BORDER,
        );
        let filled = content_width.saturating_sub(4) * usize::from(progress.min(100)) / 100;
        self.fill_rect(x + 34, status_y + 26, filled as u64, 6, ACCENT);
    }
}

fn clipped_span(start: i64, length: u64, limit: usize) -> (usize, usize) {
    let first = (start as i128).clamp(0, limit as i128) as usize;
    let last = (start as i128 + length as i128).clamp(0, limit as i128) as usize;
    (first, last)
}

// Original hand-authored 5x7 cell font for this boot panel. Rows use bits 4..0
// from left to right. No external font, bitmap, or licensed asset is embedded.
fn glyph(character: u8) -> [u8; 7] {
    match character {
        b'A' => [14, 17, 17, 31, 17, 17, 17],
        b'B' => [30, 17, 17, 30, 17, 17, 30],
        b'C' => [14, 17, 16, 16, 16, 17, 14],
        b'D' => [30, 17, 17, 17, 17, 17, 30],
        b'E' => [31, 16, 16, 30, 16, 16, 31],
        b'F' => [31, 16, 16, 30, 16, 16, 16],
        b'G' => [14, 17, 16, 23, 17, 17, 14],
        b'H' => [17, 17, 17, 31, 17, 17, 17],
        b'I' => [14, 4, 4, 4, 4, 4, 14],
        b'J' => [7, 2, 2, 2, 18, 18, 12],
        b'K' => [17, 18, 20, 24, 20, 18, 17],
        b'L' => [16, 16, 16, 16, 16, 16, 31],
        b'M' => [17, 27, 21, 21, 17, 17, 17],
        b'N' => [17, 25, 21, 19, 17, 17, 17],
        b'O' => [14, 17, 17, 17, 17, 17, 14],
        b'P' => [30, 17, 17, 30, 16, 16, 16],
        b'Q' => [14, 17, 17, 17, 21, 18, 13],
        b'R' => [30, 17, 17, 30, 20, 18, 17],
        b'S' => [15, 16, 16, 14, 1, 1, 30],
        b'T' => [31, 4, 4, 4, 4, 4, 4],
        b'U' => [17, 17, 17, 17, 17, 17, 14],
        b'V' => [17, 17, 17, 17, 17, 10, 4],
        b'W' => [17, 17, 17, 21, 21, 27, 17],
        b'X' => [17, 17, 10, 4, 10, 17, 17],
        b'Y' => [17, 17, 10, 4, 4, 4, 4],
        b'Z' => [31, 1, 2, 4, 8, 16, 31],
        b'0' => [14, 17, 19, 21, 25, 17, 14],
        b'1' => [4, 12, 4, 4, 4, 4, 14],
        b'2' => [14, 17, 1, 2, 4, 8, 31],
        b'3' => [30, 1, 1, 14, 1, 1, 30],
        b'4' => [2, 6, 10, 18, 31, 2, 2],
        b'5' => [31, 16, 16, 30, 1, 1, 30],
        b'6' => [14, 16, 16, 30, 17, 17, 14],
        b'7' => [31, 1, 2, 4, 8, 8, 8],
        b'8' => [14, 17, 17, 14, 17, 17, 14],
        b'9' => [14, 17, 17, 15, 1, 1, 14],
        b'-' => [0, 0, 0, 31, 0, 0, 0],
        b'/' => [1, 1, 2, 4, 8, 16, 16],
        b':' => [0, 4, 4, 0, 4, 4, 0],
        b'.' => [0, 0, 0, 0, 0, 4, 4],
        b'_' => [0, 0, 0, 0, 0, 0, 31],
        b' ' => [0; 7],
        _ => [14, 17, 1, 2, 4, 0, 4],
    }
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    fn surface(bytes: &mut [u8], description: Description) -> Result<Driver<'_>, FramebufferError> {
        let layout = Layout::validate(bytes.as_mut_ptr() as usize, description)?;
        if layout.info.byte_len > bytes.len() {
            return Err(FramebufferError::BufferTooSmall);
        }
        // A mutable slice provides precisely the exclusive writable storage
        // required by the same raw-pointer pixel writer used for the device.
        Ok(Driver {
            address: NonNull::new(bytes.as_mut_ptr()).expect("nonempty test storage"),
            layout,
            _exclusive: PhantomData,
        })
    }
    fn rejected(description: Description, error: FramebufferError) {
        assert!(matches!(Layout::validate(0x1000, description), Err(actual) if actual == error));
    }

    let rgb24 = Description {
        width: 3,
        height: 2,
        pitch: 11,
        bpp: 24,
        red: Mask { size: 8, shift: 0 },
        green: Mask { size: 8, shift: 8 },
        blue: Mask { size: 8, shift: 16 },
    };
    let mut guarded = [0xa5u8; 38];
    {
        let mut driver = surface(&mut guarded[8..30], rgb24).expect("RGB24 surface");
        driver.fill_rect(0, 0, 3, 2, Color::rgb(0x12, 0x34, 0x56));
        driver.fill_rect(-1, -1, 2, 2, Color::rgb(1, 2, 3));
        driver.fill_rect(2, 1, u64::MAX, u64::MAX, Color::rgb(7, 8, 9));
        driver.fill_rect(i64::MAX, i64::MAX, u64::MAX, u64::MAX, TEXT);
        driver.fill_rect(i64::MIN, i64::MIN, 1, 1, TEXT);
        assert!(!driver.pixel(3, 0, TEXT));
        assert!(!driver.pixel(0, 2, TEXT));
    }
    assert!(
        guarded[..8]
            .iter()
            .chain(&guarded[30..])
            .all(|&byte| byte == 0xa5)
    );
    assert_eq!(
        &guarded[8..17],
        &[1, 2, 3, 0x12, 0x34, 0x56, 0x12, 0x34, 0x56]
    );
    assert_eq!(
        &guarded[19..28],
        &[0x12, 0x34, 0x56, 0x12, 0x34, 0x56, 7, 8, 9]
    );
    assert_eq!(&guarded[17..19], &[0xa5; 2]);
    assert_eq!(&guarded[28..30], &[0xa5; 2]);

    let bgr32 = Description {
        pitch: 16,
        bpp: 32,
        red: Mask { size: 8, shift: 16 },
        blue: Mask { size: 8, shift: 0 },
        ..rgb24
    };
    let mut guarded = [0xa5u8; 48];
    {
        let mut driver = surface(&mut guarded[8..40], bgr32).expect("BGR32 surface");
        assert!(driver.pixel(2, 1, Color::rgb(0x12, 0x34, 0x56)));
    }
    for (index, &byte) in guarded.iter().enumerate() {
        let expected = match index {
            32 => 0x56,
            33 => 0x34,
            34 => 0x12,
            35 => 0,
            _ => 0xa5,
        };
        assert_eq!(byte, expected);
    }

    // Exercise the complete panel, glyphs, clamped progress, and tiny-screen
    // clipping through the same renderer while guarding both rows and edges.
    let description = Description {
        width: 19,
        height: 18,
        pitch: 80,
        ..bgr32
    };
    let mut canvas = [0xa5u8; 1456];
    {
        let mut driver = surface(&mut canvas[8..1448], description).expect("test panel");
        driver.render_boot(512 * 1024 * 1024);
        driver.update_status("READY", 255);
        driver.text(17, 16, "M", 8, TEXT);
        driver.text(i64::MAX, i64::MIN, "OFFSCREEN", 255, TEXT);
    }
    assert!(
        canvas[..8]
            .iter()
            .chain(&canvas[1448..])
            .all(|&byte| byte == 0xa5)
    );
    for row in 0..18 {
        assert_eq!(&canvas[8 + row * 80 + 76..8 + row * 80 + 80], &[0xa5; 4]);
    }
    assert_eq!(&canvas[1356..1360], &[245, 239, 230, 0]);
    assert_eq!(&canvas[1440..1444], &[245, 239, 230, 0]);
    assert!(canvas[8..1448].iter().any(|&byte| byte != 0xa5));
    assert_eq!(clipped_span(i64::MIN, u64::MAX, 19), (0, 19));
    assert_eq!(clipped_span(19, u64::MAX, 19), (19, 19));

    rejected(
        Description { width: 0, ..rgb24 },
        FramebufferError::InvalidDimensions,
    );
    rejected(
        Description {
            height: MAX_DIMENSION + 1,
            ..rgb24
        },
        FramebufferError::InvalidDimensions,
    );
    rejected(
        Description { bpp: 16, ..rgb24 },
        FramebufferError::UnsupportedPixelFormat,
    );
    rejected(
        Description { pitch: 8, ..rgb24 },
        FramebufferError::InvalidPitch,
    );
    rejected(
        Description {
            pitch: u64::MAX,
            ..rgb24
        },
        FramebufferError::BufferTooLarge,
    );
    rejected(
        Description {
            pitch: MAX_BYTE_LEN,
            ..rgb24
        },
        FramebufferError::BufferTooLarge,
    );
    rejected(
        Description {
            red: Mask { size: 5, shift: 0 },
            ..rgb24
        },
        FramebufferError::InvalidMasks,
    );
    rejected(
        Description {
            blue: Mask { size: 8, shift: 17 },
            ..rgb24
        },
        FramebufferError::InvalidMasks,
    );
    rejected(
        Description {
            green: Mask { size: 8, shift: 4 },
            ..rgb24
        },
        FramebufferError::InvalidMasks,
    );
    assert!(matches!(
        Layout::validate(0, rgb24),
        Err(FramebufferError::NullAddress)
    ));
    assert!(matches!(
        Layout::validate(usize::MAX - 2, rgb24),
        Err(FramebufferError::AddressOverflow)
    ));
    assert!(matches!(
        Layout::validate(0x0000_8000_0000_0000, rgb24),
        Err(FramebufferError::NonCanonicalAddress)
    ));
    assert!(matches!(
        Layout::validate(0x0000_7fff_ffff_fffc, rgb24),
        Err(FramebufferError::NonCanonicalAddress)
    ));
    assert!(matches!(
        surface(&mut [0u8; 21], rgb24),
        Err(FramebufferError::BufferTooSmall)
    ));
    crate::serial::println!("MIRROR_FRAMEBUFFER_TESTS_OK");
}
