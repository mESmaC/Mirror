//! Exclusive, allocation-free drawing into an optional Limine boot framebuffer.
//!
//! This preserves the firmware-selected mode; it is not a GPU modesetting or
//! acceleration driver. Device writes are volatile bytes or aligned RGB32 words;
//! no framebuffer slice/reference or read is created, including for initially
//! uninitialized VRAM. The one read is the boot-smoke crash screen's readback
//! of its reason row (spec 0051, point 11).
//!
//! The crash screen is the documented exception to `Driver::from_limine`'s
//! sole-writer contract: boot publishes the framebuffer's base and layout, and
//! the crash owner builds a second, raw driver over them once every other CPU
//! has stopped for good (`crash_screen`).
//! Protocol: https://github.com/Limine-Bootloader/limine-protocol/blob/trunk/PROTOCOL.md
//! Binding: https://docs.rs/limine/0.5.0/limine/framebuffer/struct.Framebuffer.html

use core::fmt;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use core::{marker::PhantomData, ptr::NonNull};
use limine::framebuffer::{Framebuffer, MemoryModel};
use mirror_ui_tokens::{CURSOR, CURSOR_HEIGHT, CURSOR_WIDTH, DEFAULT, Rgb};

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

pub const MAX_SURFACE_TILE: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlitError {
    InvalidRegion,
    InvalidDimensions,
    InvalidLength,
    OutOfBounds,
}

/// A display resource extent, independent of application layout or widgets.
/// Coordinates supplied to a blit are relative to this owned rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceRegion {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl SurfaceRegion {
    pub fn new(
        info: Info,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> Result<Self, BlitError> {
        if width == 0
            || height == 0
            || x.checked_add(width).is_none_or(|end| end > info.width)
            || y.checked_add(height).is_none_or(|end| end > info.height)
        {
            return Err(BlitError::InvalidRegion);
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    pub fn dimensions(self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// The rectangle's top-left framebuffer pixel.
    pub fn origin(self) -> (usize, usize) {
        (self.x, self.y)
    }
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

    /// A solid theme token; its padding X byte is dropped.
    const fn token(color: Rgb) -> Self {
        let [red, green, blue, _] = color.rgbx();
        Self::rgb(red, green, blue)
    }
}

// The boot screen and console draw from the shared theme (spec 0029-D5), so
// the base around a centered desktop matches the shell's own backdrop.
const BACKGROUND: Color = Color::token(DEFAULT.base);
const PANEL: Color = Color::token(DEFAULT.surface);
const BORDER: Color = Color::token(DEFAULT.border_strong);
const TEXT: Color = Color::token(DEFAULT.text_primary);
const MUTED: Color = Color::token(DEFAULT.text_secondary);
const ACCENT: Color = Color::token(DEFAULT.accent);
// The crash screen draws on the danger background (spec 0051, point 11).
const CRASH_BACKGROUND: Color = Color::token(DEFAULT.danger_bg);
const CRASH_TITLE: Color = Color::token(DEFAULT.danger);

/// The boot framebuffer, published for the crash screen (spec 0051, point
/// 11): its base address, whose provenance `publish_for_crash` exposed, or 0
/// until then. The fields below hold the description its driver validated,
/// which the crash path validates again. They are stored before the base's
/// Release and read after its Acquire.
static CRASH_BASE: AtomicUsize = AtomicUsize::new(0);
/// `width << 32 | height`.
static CRASH_EXTENT: AtomicU64 = AtomicU64::new(0);
static CRASH_PITCH: AtomicU64 = AtomicU64::new(0);
/// `bpp | red_shift << 16 | green_shift << 24 | blue_shift << 32`.
static CRASH_FORMAT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Clip {
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
}

impl Clip {
    fn intersect(self, other: Self) -> Self {
        Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        }
    }

    fn is_empty(self) -> bool {
        self.left >= self.right || self.top >= self.bottom
    }
}

#[derive(Clone, Copy)]
struct ConsoleGeometry {
    large: bool,
    scale: u8,
    cell: usize,
    row_height: usize,
    margin: usize,
    inset: usize,
    width: usize,
    height: usize,
    x: i64,
    content_width: usize,
    columns: usize,
    prompt_y: usize,
    history_y: usize,
    history_rows: usize,
}

impl ConsoleGeometry {
    fn new(info: Info) -> Self {
        let large = info.width >= 640 && info.height >= 400;
        let scale = if large { 2 } else { 1 };
        let cell = 6 * usize::from(scale);
        let row_height = 12 * usize::from(scale);
        let margin = if large { 24 } else { 0 };
        let inset = if large { 24 } else { 4 };
        let width = info.width.saturating_sub(margin * 2);
        let height = info.height.saturating_sub(margin * 2);
        let content_width = width.saturating_sub(inset * 2);
        let prompt_y = margin + height.saturating_sub(if large { 80 } else { 28 });
        let history_y = margin + if large { 112 } else { 40 };
        Self {
            large,
            scale,
            cell,
            row_height,
            margin,
            inset,
            width,
            height,
            x: (margin + inset) as i64,
            content_width,
            columns: content_width / cell,
            prompt_y,
            history_y,
            history_rows: prompt_y.saturating_sub(history_y + row_height) / row_height,
        }
    }

    fn input_start(self, console: &crate::console::Console) -> usize {
        console
            .cursor()
            .saturating_sub(self.columns.saturating_sub(3))
    }
}

/// A sole-writer handle. It is intentionally neither Clone nor Send/Sync.
pub struct Driver<'a> {
    address: NonNull<u8>,
    layout: Layout,
    clip: Option<Clip>,
    _exclusive: PhantomData<&'a mut ()>,
}

impl Driver<'static> {
    /// Publish the boot framebuffer for the crash screen (spec 0051, point
    /// 11). Once, on the BSP, for the driver `from_limine` validated; only
    /// `crash_screen` reads what this stores.
    pub fn publish_for_crash(&self) {
        let info = self.layout.info;
        CRASH_EXTENT.store(
            (info.width as u64) << 32 | info.height as u64,
            Ordering::Relaxed,
        );
        CRASH_PITCH.store(info.pitch as u64, Ordering::Relaxed);
        CRASH_FORMAT.store(
            u64::from(info.bpp)
                | u64::from(self.layout.red_shift) << 16
                | u64::from(self.layout.green_shift) << 24
                | u64::from(self.layout.blue_shift) << 32,
            Ordering::Relaxed,
        );
        let base = self.address.as_ptr().expose_provenance();
        assert_eq!(
            CRASH_BASE.swap(base, Ordering::Release),
            0,
            "crash framebuffer published twice"
        );
    }
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
            clip: None,
            _exclusive: PhantomData,
        })
    }

    pub fn info(&self) -> Info {
        self.layout.info
    }

    /// Paint a fully checked, opaque RGBX tile without reading video memory.
    /// RGBX is four source bytes R,G,B,X; X is ignored, not an alpha channel.
    /// Invalid geometry or source length leaves the entire display unchanged.
    #[allow(clippy::too_many_arguments)] // Keep resource, tile geometry, and source explicit.
    pub fn blit_rgbx(
        &mut self,
        region: SurfaceRegion,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        pixels: &[u8],
    ) -> Result<(), BlitError> {
        // A region can have been created for a different display. Revalidate
        // against this driver before trusting either its extent or origin.
        SurfaceRegion::new(self.info(), region.x, region.y, region.width, region.height)?;
        if !(1..=MAX_SURFACE_TILE).contains(&width) || !(1..=MAX_SURFACE_TILE).contains(&height) {
            return Err(BlitError::InvalidDimensions);
        }
        if x.checked_add(width).is_none_or(|end| end > region.width)
            || y.checked_add(height).is_none_or(|end| end > region.height)
        {
            return Err(BlitError::OutOfBounds);
        }
        // Width/height are bounded above, so this multiplication cannot wrap.
        if pixels.len() != width * height * 4 {
            return Err(BlitError::InvalidLength);
        }
        let origin_x = region.x + x;
        let origin_y = region.y + y;
        for (row, source) in pixels.chunks_exact(width * 4).enumerate() {
            for (column, pixel) in source.as_chunks::<4>().0.iter().enumerate() {
                let written = self.pixel(
                    origin_x + column,
                    origin_y + row,
                    Color::rgb(pixel[0], pixel[1], pixel[2]),
                );
                debug_assert!(written, "validated surface pixel escaped framebuffer");
            }
        }
        Ok(())
    }

    /// Restore the scene beneath an old software cursor without reading VRAM
    /// or scanning out a full frame. The previous damage clip is restored before
    /// returning; the driver remains exclusively owned by normal kernel context.
    pub fn restore_cursor(&mut self, console: &crate::console::Console, x: usize, y: usize) {
        let info = self.info();
        let damage = Clip {
            left: x.min(info.width),
            top: y.min(info.height),
            right: x.saturating_add(CURSOR_WIDTH).min(info.width),
            bottom: y.saturating_add(CURSOR_HEIGHT).min(info.height),
        };
        let previous = self.clip;
        let damage = previous.map_or(damage, |clip| clip.intersect(damage));
        if damage.is_empty() {
            return;
        }
        self.clip = Some(damage);
        self.render_console(console);
        self.clip = previous;
    }

    /// Draw the shared pointer sprite (0029-D12) over the current scene.
    /// Positions outside the framebuffer are ignored; a pointer at its
    /// right/bottom edges is clipped.
    pub fn draw_cursor(&mut self, x: usize, y: usize) {
        if x >= self.layout.info.width || y >= self.layout.info.height {
            return;
        }
        for (row, pixels) in CURSOR.iter().enumerate() {
            for (column, &pixel) in pixels.iter().enumerate() {
                let color = match pixel {
                    b'X' => Color::rgb(3, 6, 10),
                    b'O' => TEXT,
                    _ => continue,
                };
                self.fill_rect((x + column) as i64, (y + row) as i64, 1, 1, color);
            }
        }
    }

    /// Map a hit in the visible input row to its bounded insertion index.
    /// The prompt symbol, history, notices, and tiny overlapping layouts do not
    /// accept clicks. The scroll origin is shared with the actual renderer.
    pub fn console_caret_at(
        &self,
        console: &crate::console::Console,
        x: usize,
        y: usize,
    ) -> Option<usize> {
        let info = self.info();
        let geometry = ConsoleGeometry::new(info);
        if x >= info.width
            || y >= info.height
            || geometry.columns < 3
            || geometry.prompt_y < geometry.history_y
        {
            return None;
        }
        let left = geometry.x as usize + 2 * geometry.cell;
        let right = geometry.x as usize + geometry.content_width;
        if x < left
            || x >= right
            || y < geometry.prompt_y
            || y >= geometry.prompt_y + geometry.row_height
        {
            return None;
        }
        let visible = geometry.columns.saturating_sub(3);
        let column = ((x - left) / geometry.cell).min(visible);
        Some((geometry.input_start(console) + column).min(console.input().len()))
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

    fn clipped_rectangle(&self, x: i64, y: i64, width: u64, height: u64) -> Clip {
        // i128 accommodates every i64 coordinate plus every u64 extent, so even
        // deliberately enormous/offscreen rectangles clip without wrapping.
        let horizontal = clipped_span(x, width, self.layout.info.width);
        let vertical = clipped_span(y, height, self.layout.info.height);
        let rectangle = Clip {
            left: horizontal.0,
            right: horizontal.1,
            top: vertical.0,
            bottom: vertical.1,
        };
        match self.clip {
            Some(clip) => rectangle.intersect(clip),
            None => rectangle,
        }
    }

    fn fill_rect(&mut self, x: i64, y: i64, width: u64, height: u64, color: Color) {
        let rectangle = self.clipped_rectangle(x, y, width, height);
        if rectangle.is_empty() {
            return;
        }
        let horizontal = (rectangle.left, rectangle.right);
        let vertical = (rectangle.top, rectangle.bottom);
        let pixels = horizontal.1 - horizontal.0;
        let bytes = self.layout.encode(color);
        let pitch = self.layout.info.pitch;
        let pixel_bytes = self.layout.bytes_per_pixel;
        let address = self.address.as_ptr();
        if pixel_bytes == 4 && address as usize & 3 == 0 && pitch & 3 == 0 {
            let color = u32::from_le_bytes(bytes);
            for row in vertical.0..vertical.1 {
                let offset = row * pitch + horizontal.0 * 4;
                let destination = address.wrapping_add(offset).cast::<u32>();
                for column in 0..pixels {
                    // SAFETY: Clipping bounds rows/columns to the validated
                    // surface, whose pitch covers the complete row. The base,
                    // pitch, and four-byte pixel offset are aligned, so every
                    // store is an aligned, fully mapped RGB32 pixel. No padding
                    // or adjacent memory is written. Construction grants sole
                    // access; wrapping_add supports MMIO without a Rust slice.
                    unsafe { destination.wrapping_add(column).write_volatile(color) };
                }
            }
        } else {
            for row in vertical.0..vertical.1 {
                let offset = row * pitch + horizontal.0 * pixel_bytes;
                let destination = address.wrapping_add(offset);
                for column in 0..pixels {
                    for (index, &byte) in bytes.iter().take(pixel_bytes).enumerate() {
                        // SAFETY: The same clipped rectangle and validated
                        // pitch bound each complete pixel. Byte stores need no
                        // alignment and preserve padding for RGB24, unaligned
                        // RGB32 bases, and RGB32 modes with an odd pitch.
                        unsafe {
                            destination
                                .wrapping_add(column * pixel_bytes + index)
                                .write_volatile(byte)
                        };
                    }
                }
            }
        }
    }

    fn text(&mut self, x: i64, y: i64, text: &str, scale: u8, color: Color) {
        let scale = u64::from(scale.clamp(1, 8));
        // Cursor damage normally intersects at most one text row. Skip whole
        // strings outside the clip rather than visiting every glyph bit.
        if self
            .clipped_rectangle(x, y, text.len().min(128) as u64 * 6 * scale, 7 * scale)
            .is_empty()
        {
            return;
        }
        // Bound work even if a future boot-status caller supplies a long string.
        for (index, byte) in text.bytes().take(128).enumerate() {
            let origin = x.saturating_add(index as i64 * 6 * scale as i64);
            self.draw_glyph(origin, y, byte, scale, color);
        }
    }

    /// Draw one 5x7 glyph's set bits at `scale`, which the caller bounded.
    fn draw_glyph(&mut self, x: i64, y: i64, byte: u8, scale: u64, color: Color) {
        for (row, bits) in glyph(byte).into_iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) != 0 {
                    self.fill_rect(
                        x.saturating_add(column * scale as i64),
                        y.saturating_add(row as i64 * scale as i64),
                        scale,
                        scale,
                        color,
                    );
                }
            }
        }
    }

    /// Read one pixel's bytes back (boot-smoke crash screen readback only).
    /// Outside the framebuffer it returns `None`.
    #[cfg(feature = "boot-smoke")]
    fn read_pixel(&self, x: usize, y: usize) -> Option<[u8; 4]> {
        if x >= self.layout.info.width || y >= self.layout.info.height {
            return None;
        }
        let offset = y * self.layout.info.pitch + x * self.layout.bytes_per_pixel;
        let mut bytes = [0_u8; 4];
        for (index, byte) in bytes
            .iter_mut()
            .take(self.layout.bytes_per_pixel)
            .enumerate()
        {
            // SAFETY: As in `pixel`: the bounds and the validated pitch and
            // length keep each byte inside the framebuffer, which this
            // driver alone accesses. A volatile byte read of the boot
            // framebuffer or of test RAM has no side effect.
            *byte = unsafe {
                self.address
                    .as_ptr()
                    .wrapping_add(offset + index)
                    .read_volatile()
            };
        }
        Some(bytes)
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

    /// Render the main-context diagnostic editor. The console snapshot contains
    /// bounded ASCII only; IRQ handlers never read or draw through this driver.
    /// This immediate renderer has no compositor, windows, or display flipping.
    pub fn render_console(&mut self, console: &crate::console::Console) {
        let info = self.info();
        let geometry = ConsoleGeometry::new(info);
        let ConsoleGeometry {
            large,
            scale,
            margin,
            inset,
            width,
            height,
            x,
            content_width,
            history_y,
            ..
        } = geometry;

        self.fill_rect(0, 0, info.width as u64, info.height as u64, BACKGROUND);
        self.fill_rect(
            margin as i64,
            margin as i64,
            width as u64,
            height as u64,
            PANEL,
        );
        self.fill_rect(margin as i64, margin as i64, width as u64, 1, BORDER);
        self.fill_rect(
            margin as i64,
            (margin + height.saturating_sub(1)) as i64,
            width as u64,
            1,
            BORDER,
        );
        self.fill_rect(margin as i64, margin as i64, 1, height as u64, BORDER);
        self.fill_rect(
            (margin + width.saturating_sub(1)) as i64,
            margin as i64,
            1,
            height as u64,
            BORDER,
        );
        self.fill_rect(
            margin as i64 + 1,
            margin as i64 + 1,
            3,
            height.saturating_sub(2) as u64,
            ACCENT,
        );
        self.text(x, (margin + inset) as i64, "Mirror", scale * 2, TEXT);
        self.text(
            x,
            (margin + if large { 64 } else { 23 }) as i64,
            "Kernel diagnostic console",
            scale,
            ACCENT,
        );
        if large {
            self.text(
                x,
                (margin + 88) as i64,
                "Native keyboard and mouse / help / status / echo / clear",
                1,
                MUTED,
            );
        }
        self.fill_rect(
            x,
            (history_y.saturating_sub(12)) as i64,
            content_width as u64,
            1,
            BORDER,
        );

        self.draw_console_history(console, geometry);
        self.draw_console_input(console, geometry);
    }

    /// Refresh command output and editor text while retaining the static header.
    /// Call after submission or input loss, following an initial render_console.
    pub fn update_console_content(&mut self, console: &crate::console::Console) {
        let geometry = ConsoleGeometry::new(self.info());
        // Tiny layouts can overlap the header or clip the prompt outside its
        // inner column area. Repaint everything rather than leave stale pixels.
        if geometry.columns < 3 || geometry.prompt_y < geometry.history_y {
            self.render_console(console);
            return;
        }
        self.fill_rect(
            geometry.x,
            geometry.history_y as i64,
            geometry.content_width as u64,
            geometry.prompt_y.saturating_sub(geometry.history_y) as u64,
            PANEL,
        );
        self.draw_console_history(console, geometry);
        self.update_console_input(console);
    }

    /// Refresh only the prompt, caret, and notice after line editing. This keeps
    /// per-key redraw work independent of the framebuffer's full height.
    pub fn update_console_input(&mut self, console: &crate::console::Console) {
        let geometry = ConsoleGeometry::new(self.info());
        if geometry.columns < 3 || geometry.prompt_y < geometry.history_y {
            self.render_console(console);
            return;
        }
        self.fill_rect(
            geometry.x,
            geometry.prompt_y as i64,
            geometry.content_width as u64,
            (geometry.margin + geometry.height).saturating_sub(geometry.prompt_y + 1) as u64,
            PANEL,
        );
        self.draw_console_input(console, geometry);
    }

    fn draw_console_history(
        &mut self,
        console: &crate::console::Console,
        geometry: ConsoleGeometry,
    ) {
        let ConsoleGeometry {
            history_rows,
            history_y,
            row_height,
            columns,
            x,
            cell,
            scale,
            ..
        } = geometry;
        let history = console.history();
        let first = history.len().saturating_sub(history_rows);
        for (index, line) in history[first..].iter().enumerate() {
            let y = (history_y + index * row_height) as i64;
            let (indent, color) = if line.prompt {
                self.text(x, y, ">", scale, ACCENT);
                (2, MUTED)
            } else {
                (0, TEXT)
            };
            let text = line.text.as_str();
            let visible = text.len().min(columns.saturating_sub(indent));
            self.text(
                x + (indent * cell) as i64,
                y,
                &text[..visible],
                scale,
                color,
            );
        }
    }

    fn draw_console_input(&mut self, console: &crate::console::Console, geometry: ConsoleGeometry) {
        let ConsoleGeometry {
            x,
            prompt_y,
            content_width,
            scale,
            columns,
            cell,
            row_height,
            large,
            ..
        } = geometry;
        self.fill_rect(
            x,
            prompt_y.saturating_sub(12) as i64,
            content_width as u64,
            1,
            BORDER,
        );
        self.text(x, prompt_y as i64, ">", scale, ACCENT);
        // Reserve the prompt and one cursor cell. Scroll by ASCII byte cells so
        // a full line still exposes the caret on narrow framebuffer modes.
        let visible = columns.saturating_sub(3);
        let start = geometry.input_start(console);
        let end = console.input().len().min(start + visible);
        self.text(
            x + (2 * cell) as i64,
            prompt_y as i64,
            &console.input()[start..end],
            scale,
            TEXT,
        );
        if columns >= 3 {
            let cursor_x = x + ((2 + console.cursor() - start) * cell) as i64;
            self.fill_rect(
                cursor_x,
                (prompt_y + 8 * usize::from(scale)) as i64,
                (5 * usize::from(scale)) as u64,
                u64::from(scale),
                ACCENT,
            );
        }
        let notice = console.notice();
        self.text(
            x,
            (prompt_y + row_height + if large { 16 } else { 2 }) as i64,
            &notice[..notice.len().min(columns)],
            scale,
            MUTED,
        );
    }
}

/// Open the crash screen (spec 0051, point 11) over the boot framebuffer
/// that boot published, or `None` when none was published or its stored
/// description no longer validates.
///
/// # Safety
///
/// Only the crash owner calls this, once every other CPU that ran has
/// stopped for good, so this screen is the framebuffer's only writer. That is
/// the documented exception to `Driver::from_limine`'s contract: the boot
/// driver may still exist, but it never runs again, because it lives on a
/// stopped CPU or below the crash on this one, and the crash path never
/// returns. No other driver over the framebuffer may be used afterwards.
pub(crate) unsafe fn crash_screen() -> Option<CrashScreen<'static>> {
    let base = CRASH_BASE.load(Ordering::Acquire);
    if base == 0 {
        return None;
    }
    let extent = CRASH_EXTENT.load(Ordering::Relaxed);
    let format = CRASH_FORMAT.load(Ordering::Relaxed);
    let mask = |shift: u64| Mask {
        size: 8,
        shift: (format >> shift) as u8,
    };
    let description = Description {
        width: extent >> 32,
        height: extent & u64::from(u32::MAX),
        pitch: CRASH_PITCH.load(Ordering::Relaxed),
        bpp: format as u16,
        red: mask(16),
        green: mask(24),
        blue: mask(32),
    };
    let layout = Layout::validate(base, description).ok()?;
    let driver = Driver {
        address: NonNull::new(core::ptr::with_exposed_provenance_mut::<u8>(base))?,
        layout,
        clip: None,
        _exclusive: PhantomData,
    };
    Some(CrashScreen::new(driver))
}

/// Rows a reason block may take, and the record of its first row.
const REASON_ROWS: usize = 6;
#[cfg(feature = "boot-smoke")]
const RECORDED_CELLS: usize = 256;

/// The crash screen (spec 0051, point 11): a title, the reason, the stop
/// line, the backtrace's frames and the final action, in the 5x7 font on
/// `danger_bg`, top to bottom. Every glyph is clipped to the framebuffer, and
/// a block that would take more rows than it may is cut off. Nothing here
/// allocates or locks.
pub(crate) struct CrashScreen<'a> {
    driver: Driver<'a>,
    left: i64,
    right: i64,
    y: i64,
    /// The body text's scale: 2 on a screen of at least 640x400, else 1.
    body: u8,
    /// The first row of the first reason block, which the boot-smoke
    /// readback checks.
    #[cfg(feature = "boot-smoke")]
    reason: ReasonRow,
}

/// What the first reason row drew: its cells' bytes, where and how.
#[cfg(feature = "boot-smoke")]
struct ReasonRow {
    bytes: [u8; RECORDED_CELLS],
    len: usize,
    x: i64,
    y: i64,
    scale: u64,
    color: Color,
    done: bool,
}

impl<'a> CrashScreen<'a> {
    /// Clear the whole framebuffer to `danger_bg` and draw the title.
    fn new(mut driver: Driver<'a>) -> Self {
        let info = driver.info();
        driver.fill_rect(
            0,
            0,
            info.width as u64,
            info.height as u64,
            CRASH_BACKGROUND,
        );
        let large = info.width >= 640 && info.height >= 400;
        let margin: i64 = if large { 48 } else { 8 };
        let mut screen = Self {
            driver,
            left: margin,
            right: (info.width as i64 - margin).max(margin + 1),
            y: margin,
            body: if large { 2 } else { 1 },
            #[cfg(feature = "boot-smoke")]
            reason: ReasonRow {
                bytes: [0; RECORDED_CELLS],
                len: 0,
                x: 0,
                y: 0,
                scale: 1,
                color: TEXT,
                done: false,
            },
        };
        screen.block(
            screen.body * 2,
            CRASH_TITLE,
            1,
            false,
            format_args!("Mirror stopped"),
        );
        screen.block(
            screen.body,
            MUTED,
            2,
            false,
            format_args!("A fatal kernel error stopped every CPU."),
        );
        screen
    }

    /// The crash's reason, in up to `REASON_ROWS` rows.
    pub(crate) fn reason(&mut self, args: fmt::Arguments<'_>) {
        self.block(self.body, TEXT, REASON_ROWS, true, args);
    }

    /// A detail line, such as the stop line or the backtrace summary.
    pub(crate) fn detail(&mut self, args: fmt::Arguments<'_>) {
        self.block(self.body, MUTED, 2, false, args);
    }

    /// The final action.
    pub(crate) fn action(&mut self, args: fmt::Arguments<'_>) {
        self.block(self.body, TEXT, 2, false, args);
    }

    /// The backtrace's return addresses in up to four columns.
    pub(crate) fn frames(&mut self, rips: &[u64]) {
        if rips.is_empty() {
            return;
        }
        let scale = u64::from(self.body);
        // "31 ffffffff80123456" and two cells of space.
        let column_width = 21 * 6 * scale as i64;
        let columns = ((self.right - self.left) / column_width).clamp(1, 4) as usize;
        let rows = rips.len().div_ceil(columns);
        for (index, rip) in rips.iter().enumerate() {
            let x = self
                .left
                .saturating_add((index / rows) as i64 * column_width);
            let y = self
                .y
                .saturating_add((index % rows) as i64 * line_height(scale));
            let mut pen = Pen {
                driver: &mut self.driver,
                left: x,
                right: x.saturating_add(column_width),
                x,
                y,
                scale,
                color: TEXT,
                row: 0,
                max_rows: 1,
                #[cfg(feature = "boot-smoke")]
                record: None,
            };
            let _ = fmt::Write::write_fmt(&mut pen, format_args!("{index:02} {rip:016x}"));
        }
        self.y = self
            .y
            .saturating_add(rows as i64 * line_height(scale) + block_gap(self.body));
    }

    /// Draw `args` from the left margin at the current row, wrapping at the
    /// right margin and at newlines, in at most `max_rows` rows, then move
    /// below it. `record` keeps the first reason row for the readback.
    fn block(
        &mut self,
        scale: u8,
        color: Color,
        max_rows: usize,
        record: bool,
        args: fmt::Arguments<'_>,
    ) {
        let scale = u64::from(scale);
        #[cfg(feature = "boot-smoke")]
        let record = (record && !self.reason.done).then(|| {
            self.reason.x = self.left;
            self.reason.y = self.y;
            self.reason.scale = scale;
            self.reason.color = color;
            self.reason.done = true;
            &mut self.reason
        });
        #[cfg(not(feature = "boot-smoke"))]
        let _ = record;
        let mut pen = Pen {
            driver: &mut self.driver,
            left: self.left,
            right: self.right,
            x: self.left,
            y: self.y,
            scale,
            color,
            row: 0,
            max_rows,
            #[cfg(feature = "boot-smoke")]
            record,
        };
        let _ = fmt::Write::write_fmt(&mut pen, args);
        let rows = (pen.row + 1).min(max_rows);
        self.y = self.y.saturating_add(
            (rows as i64)
                .saturating_mul(line_height(scale))
                .saturating_add(block_gap(self.body)),
        );
    }

    /// Read the first reason row back from the framebuffer (boot-smoke):
    /// every pixel of each of its cells must be the glyph's color where the
    /// 5x7 font sets a bit and `danger_bg` elsewhere, and the row must light
    /// at least one pixel. False when any of it lies off the screen.
    #[cfg(feature = "boot-smoke")]
    pub(crate) fn verify_reason(&self) -> bool {
        let row = &self.reason;
        let layout = self.driver.layout;
        let (Ok(left), Ok(top), Ok(scale)) = (
            usize::try_from(row.x),
            usize::try_from(row.y),
            usize::try_from(row.scale),
        ) else {
            return false;
        };
        let foreground = layout.encode(row.color);
        let background = layout.encode(CRASH_BACKGROUND);
        let bytes = layout.bytes_per_pixel;
        // Drain any write-combining buffer, so the reads see every write.
        core::sync::atomic::fence(Ordering::SeqCst);
        let mut lit = 0_usize;
        for (index, &byte) in row.bytes[..row.len].iter().enumerate() {
            let bits = glyph(byte);
            let origin = left + index * 6 * scale;
            for dy in 0..7 * scale {
                for dx in 0..6 * scale {
                    let column = dx / scale;
                    let on = column < 5 && bits[dy / scale] & (1 << (4 - column)) != 0;
                    let expected = if on { foreground } else { background };
                    match self.driver.read_pixel(origin + dx, top + dy) {
                        Some(actual) if actual[..bytes] == expected[..bytes] => {}
                        _ => return false,
                    }
                    lit += usize::from(on);
                }
            }
        }
        lit > 0
    }
}

/// The distance from one text row's top to the next one's.
fn line_height(scale: u64) -> i64 {
    11 * scale as i64
}

/// The space below a block.
fn block_gap(body: u8) -> i64 {
    8 * i64::from(body)
}

/// Draws formatted text for the crash screen: printable ASCII as itself, a
/// tab as a space, anything else as '?'. It wraps before a cell that would
/// cross `right` and at a newline, and drops what falls past `max_rows`.
struct Pen<'p, 'a> {
    driver: &'p mut Driver<'a>,
    left: i64,
    right: i64,
    x: i64,
    y: i64,
    scale: u64,
    color: Color,
    row: usize,
    max_rows: usize,
    #[cfg(feature = "boot-smoke")]
    record: Option<&'p mut ReasonRow>,
}

impl Pen<'_, '_> {
    fn next_row(&mut self) {
        self.row += 1;
        self.x = self.left;
        self.y = self.y.saturating_add(line_height(self.scale));
    }

    fn put(&mut self, character: char) {
        if self.row >= self.max_rows {
            return;
        }
        if character == '\n' {
            self.next_row();
            return;
        }
        let scale = self.scale as i64;
        if self.x > self.left && self.x.saturating_add(5 * scale) > self.right {
            self.next_row();
            // A wrapped row starts at its first visible character.
            if self.row >= self.max_rows || character == ' ' {
                return;
            }
        }
        let byte = match character {
            ' '..='~' => character as u8,
            '\t' => b' ',
            _ => b'?',
        };
        self.driver
            .draw_glyph(self.x, self.y, byte, self.scale, self.color);
        #[cfg(feature = "boot-smoke")]
        if self.row == 0
            && let Some(record) = self.record.as_deref_mut()
            && record.len < RECORDED_CELLS
        {
            record.bytes[record.len] = byte;
            record.len += 1;
        }
        self.x = self.x.saturating_add(6 * scale);
    }
}

impl fmt::Write for Pen<'_, '_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for character in text.chars() {
            self.put(character);
        }
        Ok(())
    }
}

fn clipped_span(start: i64, length: u64, limit: usize) -> (usize, usize) {
    let first = (start as i128).clamp(0, limit as i128) as usize;
    let last = (start as i128 + length as i128).clamp(0, limit as i128) as usize;
    (first, last)
}

// Original hand-authored 5x7 printable ASCII font. Rows use bits 4..0
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
        b'a' => [0, 0, 14, 1, 15, 17, 15],
        b'b' => [16, 16, 30, 17, 17, 17, 30],
        b'c' => [0, 0, 14, 17, 16, 17, 14],
        b'd' => [1, 1, 15, 17, 17, 17, 15],
        b'e' => [0, 0, 14, 17, 31, 16, 14],
        b'f' => [6, 9, 8, 28, 8, 8, 8],
        b'g' => [0, 0, 15, 17, 15, 1, 14],
        b'h' => [16, 16, 30, 17, 17, 17, 17],
        b'i' => [4, 0, 12, 4, 4, 4, 14],
        b'j' => [2, 0, 6, 2, 2, 18, 12],
        b'k' => [16, 16, 18, 20, 24, 20, 18],
        b'l' => [12, 4, 4, 4, 4, 4, 14],
        b'm' => [0, 0, 26, 21, 21, 21, 21],
        b'n' => [0, 0, 30, 17, 17, 17, 17],
        b'o' => [0, 0, 14, 17, 17, 17, 14],
        b'p' => [0, 0, 30, 17, 30, 16, 16],
        b'q' => [0, 0, 15, 17, 15, 1, 1],
        b'r' => [0, 0, 22, 25, 16, 16, 16],
        b's' => [0, 0, 15, 16, 14, 1, 30],
        b't' => [8, 8, 28, 8, 8, 9, 6],
        b'u' => [0, 0, 17, 17, 17, 19, 13],
        b'v' => [0, 0, 17, 17, 17, 10, 4],
        b'w' => [0, 0, 17, 17, 21, 21, 10],
        b'x' => [0, 0, 17, 10, 4, 10, 17],
        b'y' => [0, 0, 17, 17, 15, 1, 14],
        b'z' => [0, 0, 31, 2, 4, 8, 31],
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
        b'!' => [4, 4, 4, 4, 4, 0, 4],
        b'"' => [10, 10, 10, 0, 0, 0, 0],
        b'#' => [10, 10, 31, 10, 31, 10, 10],
        b'$' => [4, 15, 20, 14, 5, 30, 4],
        b'%' => [25, 25, 2, 4, 8, 19, 19],
        b'&' => [12, 18, 20, 8, 21, 18, 13],
        b'\'' => [4, 4, 8, 0, 0, 0, 0],
        b'(' => [2, 4, 8, 8, 8, 4, 2],
        b')' => [8, 4, 2, 2, 2, 4, 8],
        b'*' => [0, 21, 14, 31, 14, 21, 0],
        b'+' => [0, 4, 4, 31, 4, 4, 0],
        b',' => [0, 0, 0, 0, 0, 4, 8],
        b'-' => [0, 0, 0, 31, 0, 0, 0],
        b'/' => [1, 1, 2, 4, 8, 16, 16],
        b':' => [0, 4, 4, 0, 4, 4, 0],
        b';' => [0, 4, 4, 0, 4, 4, 8],
        b'<' => [2, 4, 8, 16, 8, 4, 2],
        b'=' => [0, 0, 31, 0, 31, 0, 0],
        b'>' => [8, 4, 2, 1, 2, 4, 8],
        b'?' => [14, 17, 1, 2, 4, 0, 4],
        b'@' => [14, 17, 23, 21, 23, 16, 14],
        b'[' => [14, 8, 8, 8, 8, 8, 14],
        b'\\' => [16, 16, 8, 4, 2, 1, 1],
        b']' => [14, 2, 2, 2, 2, 2, 14],
        b'^' => [4, 10, 17, 0, 0, 0, 0],
        b'.' => [0, 0, 0, 0, 0, 4, 4],
        b'_' => [0, 0, 0, 0, 0, 0, 31],
        b'`' => [8, 4, 2, 0, 0, 0, 0],
        b'{' => [2, 4, 4, 8, 4, 4, 2],
        b'|' => [4, 4, 4, 4, 4, 4, 4],
        b'}' => [8, 4, 4, 2, 4, 4, 8],
        b'~' => [0, 0, 9, 22, 0, 0, 0],
        b' ' => [0; 7],
        _ => [14, 17, 1, 2, 4, 0, 4],
    }
}

#[cfg(feature = "boot-smoke")]
pub(crate) fn test_rgb32_driver(
    bytes: &mut [u8],
    width: usize,
    height: usize,
    pitch: usize,
) -> Result<Driver<'_>, FramebufferError> {
    let description = Description {
        width: width as u64,
        height: height as u64,
        pitch: pitch as u64,
        bpp: 32,
        red: Mask { size: 8, shift: 16 },
        green: Mask { size: 8, shift: 8 },
        blue: Mask { size: 8, shift: 0 },
    };
    let layout = Layout::validate(bytes.as_mut_ptr() as usize, description)?;
    if layout.info.byte_len > bytes.len() {
        return Err(FramebufferError::BufferTooSmall);
    }
    // The slice grants exclusive initialized RAM ownership for the returned
    // lifetime, exactly as the guarded renderer fixtures in this module.
    Ok(Driver {
        address: NonNull::new(bytes.as_mut_ptr()).ok_or(FramebufferError::NullAddress)?,
        layout,
        clip: None,
        _exclusive: PhantomData,
    })
}

#[cfg(feature = "boot-smoke")]
pub(crate) fn test_surface_upload() {
    use crate::surface::{INFO_SIZE, MAX_PACKET, MAX_TILE, Session};

    fn surface(bytes: &mut [u8], description: Description) -> Driver<'_> {
        let layout = Layout::validate(bytes.as_mut_ptr() as usize, description).unwrap();
        assert!(layout.info.byte_len <= bytes.len());
        Driver {
            address: NonNull::new(bytes.as_mut_ptr()).unwrap(),
            layout,
            clip: None,
            _exclusive: PhantomData,
        }
    }
    fn header(packet: &mut [u8], values: [u32; 4]) {
        for (destination, value) in packet[..16].as_chunks_mut::<4>().0.iter_mut().zip(values) {
            destination.copy_from_slice(&value.to_le_bytes());
        }
    }
    fn info_fields(info: [u8; INFO_SIZE]) -> [u32; 6] {
        let mut fields = [0; 6];
        for (value, bytes) in fields.iter_mut().zip(info.as_chunks::<4>().0) {
            *value = u32::from_le_bytes(*bytes);
        }
        fields
    }
    fn expected_byte(color: [u8; 3], byte: usize, bpp: u16) -> u8 {
        if bpp == 24 {
            color[byte]
        } else {
            [color[2], color[1], color[0], 0][byte]
        }
    }
    fn check_pixels(
        bytes: &[u8],
        description: Description,
        color_at: impl Fn(usize, usize) -> Option<[u8; 3]>,
    ) {
        let pitch = description.pitch as usize;
        let pixel_bytes = usize::from(description.bpp / 8);
        let end = 8 + pitch * description.height as usize;
        for (index, &actual) in bytes.iter().enumerate() {
            let expected = if (8..end).contains(&index) {
                let offset = index - 8;
                let row = offset / pitch;
                let byte_in_row = offset % pitch;
                if byte_in_row < description.width as usize * pixel_bytes {
                    color_at(byte_in_row / pixel_bytes, row)
                        .map(|color| {
                            expected_byte(color, byte_in_row % pixel_bytes, description.bpp)
                        })
                        .unwrap_or(0xa5)
                } else {
                    0xa5
                }
            } else {
                0xa5
            };
            assert_eq!(actual, expected, "surface changed unexpected byte {index}");
        }
    }

    for bpp in [24, 32] {
        let pixel_bytes = usize::from(bpp / 8);
        let description = Description {
            width: 40,
            height: 36,
            pitch: (40 * pixel_bytes + 3) as u64,
            bpp,
            red: Mask {
                size: 8,
                shift: if bpp == 24 { 0 } else { 16 },
            },
            green: Mask { size: 8, shift: 8 },
            blue: Mask {
                size: 8,
                shift: if bpp == 24 { 16 } else { 0 },
            },
        };
        let end = 8 + description.pitch as usize * description.height as usize;
        let mut canvas = [0xa5; 5920];
        let info = Layout::validate(0x1000, description).unwrap().info;
        let region = SurfaceRegion::new(info, 5, 7, 20, 18).unwrap();
        assert_eq!(region.dimensions(), (20, 18));
        for (x, y, width, height) in [
            (0, 0, 0, 1),
            (0, 0, 1, 0),
            (39, 0, 2, 1),
            (0, 35, 1, 2),
            (usize::MAX, 0, 1, 1),
            (0, usize::MAX, 1, 1),
        ] {
            assert!(SurfaceRegion::new(info, x, y, width, height).is_err());
        }
        {
            let mut driver = surface(&mut canvas[8..end], description);
            driver
                .blit_rgbx(
                    region,
                    1,
                    2,
                    2,
                    2,
                    &[1, 2, 3, 0, 4, 5, 6, 0xff, 7, 8, 9, 0x80, 10, 11, 12, 0x42],
                )
                .unwrap();
            for (x, y) in [(0, 0), (19, 0), (0, 17), (19, 17)] {
                driver
                    .blit_rgbx(region, x, y, 1, 1, &[13, 14, 15, 0xee])
                    .unwrap();
            }
        }
        check_pixels(&canvas, description, |x, y| match (x, y) {
            (6, 9) => Some([1, 2, 3]),
            (7, 9) => Some([4, 5, 6]),
            (6, 10) => Some([7, 8, 9]),
            (7, 10) => Some([10, 11, 12]),
            (5, 7) | (24, 7) | (5, 24) | (24, 24) => Some([13, 14, 15]),
            _ => None,
        });
        let before = canvas;
        for (x, y, width, height, length) in [
            (0, 0, 0, 1, 4),
            (0, 0, 1, 0, 4),
            (0, 0, 17, 1, 4),
            (0, 0, 1, 17, 4),
            (0, 0, usize::MAX, 1, 4),
            (usize::MAX, 0, 1, 1, 4),
            (0, usize::MAX, 1, 1, 4),
            (19, 0, 2, 1, 8),
            (0, 17, 1, 2, 8),
            (20, 0, 1, 1, 4),
            (0, 18, 1, 1, 4),
            (0, 0, 1, 1, 3),
            (0, 0, 1, 1, 5),
        ] {
            {
                let mut driver = surface(&mut canvas[8..end], description);
                assert!(
                    driver
                        .blit_rgbx(region, x, y, width, height, &[99; 8][..length])
                        .is_err()
                );
            }
            assert_eq!(canvas, before, "invalid tile changed display memory");
        }
        let foreign_region = SurfaceRegion::new(Info { width: 80, ..info }, 60, 0, 8, 8).unwrap();
        {
            let mut driver = surface(&mut canvas[8..end], description);
            assert_eq!(
                driver.blit_rgbx(foreign_region, 0, 0, 1, 1, &[0; 4]),
                Err(BlitError::InvalidRegion)
            );
        }
        assert_eq!(canvas, before);

        canvas.fill(0xa5);
        let mut packet = [0; MAX_PACKET];
        header(&mut packet, [2, 3, MAX_TILE as u32, MAX_TILE as u32]);
        for pixel in packet[16..].as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[17, 34, 51, 0xff]);
        }
        {
            let mut driver = surface(&mut canvas[8..end], description);
            let mut session = Session::new(&mut driver).unwrap();
            assert_eq!(info_fields(session.info()), [1, 40, 36, 1, 16, 0]);
            assert_eq!(session.totals(), (0, 0));
            assert_eq!(session.present(&packet), Ok(256));
            header(&mut packet, [39, 35, 1, 1]);
            packet[16..20].copy_from_slice(&[68, 85, 102, 0x77]);
            assert_eq!(session.present(&packet[..20]), Ok(1));
            assert_eq!(session.totals(), (2, 257));
            for invalid in [
                &[][..],
                &packet[..15],
                &packet[..16],
                &packet[..19],
                &packet[..21],
                &[0; MAX_PACKET + 1][..],
            ] {
                assert!(session.present(invalid).is_err());
                assert_eq!(session.totals(), (2, 257));
            }
            for values in [
                [0, 0, 0, 1],
                [0, 0, 1, 0],
                [0, 0, 17, 1],
                [0, 0, 1, 17],
                [u32::MAX, 0, 1, 1],
                [0, u32::MAX, 1, 1],
                [40, 0, 1, 1],
                [0, 36, 1, 1],
            ] {
                header(&mut packet, values);
                assert!(session.present(&packet[..20]).is_err());
                assert_eq!(session.totals(), (2, 257));
            }
            header(&mut packet, [0, 0, 16, 16]);
            assert!(session.present(&packet[..MAX_PACKET - 1]).is_err());
            assert_eq!(session.totals(), (2, 257));
        }
        check_pixels(&canvas, description, |x, y| {
            if (2..18).contains(&x) && (3..19).contains(&y) {
                Some([17, 34, 51])
            } else if (x, y) == (39, 35) {
                Some([68, 85, 102])
            } else {
                None
            }
        });

        // Thin surfaces exercise each centered, nonzero session origin without
        // allocating a screen-size buffer on the small bootstrap stack.
        for (width, height, origin_x, origin_y, owned_width, owned_height) in
            [(300, 4, 22, 0, 256, 4), (4, 140, 0, 6, 4, 128)]
        {
            canvas.fill(0xa5);
            let description = Description {
                width,
                height,
                pitch: width * pixel_bytes as u64 + 3,
                ..description
            };
            let end = 8 + description.pitch as usize * height as usize;
            {
                let mut driver = surface(&mut canvas[8..end], description);
                let mut session = Session::new(&mut driver).unwrap();
                assert_eq!(
                    info_fields(session.info()),
                    [1, owned_width, owned_height, 1, 16, 0]
                );
                header(&mut packet, [0, 0, 1, 1]);
                packet[16..20].copy_from_slice(&[7, 17, 27, 0xaa]);
                assert_eq!(session.present(&packet[..20]), Ok(1));
                header(&mut packet, [owned_width - 1, owned_height - 1, 1, 1]);
                assert_eq!(session.present(&packet[..20]), Ok(1));
                assert_eq!(session.totals(), (2, 2));
            }
            check_pixels(&canvas, description, |x, y| {
                if (x, y) == (origin_x, origin_y)
                    || (x, y)
                        == (
                            origin_x + owned_width as usize - 1,
                            origin_y + owned_height as usize - 1,
                        )
                {
                    Some([7, 17, 27])
                } else {
                    None
                }
            });
        }
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
            clip: None,
            _exclusive: PhantomData,
        })
    }
    fn rejected(description: Description, error: FramebufferError) {
        assert!(matches!(Layout::validate(0x1000, description), Err(actual) if actual == error));
    }
    // Expected BGR32 bytes of a theme color, spelled out independently of
    // `Layout::encode` so the oracles follow the tokens (spec 0029-D5).
    fn bgrx(color: Color) -> [u8; 4] {
        [color.blue, color.green, color.red, 0]
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

    // Force the aligned RGB32 fast path and both byte fallbacks (unaligned
    // base, non-word pitch). Check every byte, including untouched pixels,
    // row padding, and the prefix/suffix guards of the same backing storage.
    #[repr(align(4))]
    struct Aligned([u8; 64]);
    for start in [8, 9] {
        for pitch in [16, 13] {
            let mut storage = Aligned([0xa5; 64]);
            let end = start + pitch * 2;
            {
                let mut driver = surface(
                    &mut storage.0[start..end],
                    Description {
                        pitch: pitch as u64,
                        ..bgr32
                    },
                )
                .expect("aligned/unaligned RGB32 rectangle");
                assert_eq!(driver.address.as_ptr() as usize & 3, start & 3);
                driver.fill_rect(-1, 0, 3, u64::MAX, Color::rgb(0x12, 0x34, 0x56));
                driver.fill_rect(2, 1, u64::MAX, u64::MAX, Color::rgb(7, 8, 9));
            }
            for (index, &byte) in storage.0.iter().enumerate() {
                let expected = if (start..end).contains(&index) {
                    let offset = index - start;
                    let column_byte = offset % pitch;
                    if column_byte < 8 {
                        [0x56, 0x34, 0x12, 0][column_byte % 4]
                    } else if offset / pitch == 1 && column_byte < 12 {
                        [9, 8, 7, 0][column_byte % 4]
                    } else {
                        0xa5
                    }
                } else {
                    0xa5
                };
                assert_eq!(byte, expected);
            }
        }
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
    assert_eq!(&canvas[1356..1360], &bgrx(TEXT));
    assert_eq!(&canvas[1440..1444], &bgrx(TEXT));
    assert!(canvas[8..1448].iter().any(|&byte| byte != 0xa5));
    assert_eq!(clipped_span(i64::MIN, u64::MAX, 19), (0, 19));
    assert_eq!(clipped_span(19, u64::MAX, 19), (19, 19));

    // The console preserves case and supports every printable ASCII key,
    // rather than silently displaying lowercase input as uppercase/fallbacks.
    assert_ne!(glyph(b'M'), glyph(b'm'));
    for byte in b'!'..=b'~' {
        assert!(glyph(byte).iter().all(|&row| row < 32));
        assert!(byte == b'?' || glyph(byte) != glyph(b'?'));
    }
    let mut console = crate::console::Console::new();
    console.dispatch(crate::console::Operation::Echo(
        crate::console::Text::new("Mirror Aa!? []{}").unwrap(),
    ));
    {
        let mut driver = surface(&mut canvas[8..1448], description).expect("tiny console");
        driver.render_console(&console);
        driver.update_console_input(&console);
        driver.update_console_content(&console);
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

    let mut tiny_reference = [0xa5u8; 1456];
    {
        let mut driver =
            surface(&mut tiny_reference[8..1448], description).expect("tiny full reference");
        driver.render_console(&console);
    }
    assert_eq!(canvas, tiny_reference);

    // Include visible history, a full scrolled editor, and its caret on a
    // padded surface. Prefix/suffix guards and every row's padding must survive.
    let mut console_canvas = [0xa5u8; 38_816];
    let console_description = Description {
        width: 96,
        height: 100,
        pitch: 388,
        ..bgr32
    };
    for _ in 0..crate::console::LINE_CAPACITY {
        console.process(crate::keyboard::KeyEvent {
            key: crate::keyboard::Key::Character(b'x'),
            pressed: true,
            repeat: false,
            modifiers: crate::keyboard::Modifiers::default(),
        });
    }
    {
        let mut driver =
            surface(&mut console_canvas[8..38_808], console_description).expect("console surface");
        driver.render_console(&console);
        // A full 96-byte line is scrolled by 85 bytes on this small mode.
        assert_eq!(driver.console_caret_at(&console, 16, 72), Some(85));
        assert_eq!(driver.console_caret_at(&console, 46, 83), Some(90));
        assert_eq!(driver.console_caret_at(&console, 91, 72), Some(96));
        for position in [
            (15, 72),
            (92, 72),
            (16, 71),
            (16, 84),
            (usize::MAX, usize::MAX),
        ] {
            assert_eq!(
                driver.console_caret_at(&console, position.0, position.1),
                None
            );
        }
        assert!(console.place_cursor(90));
        assert_eq!(driver.console_caret_at(&console, 16, 72), Some(79));
        assert!(console.place_cursor(96));
    }
    assert!(
        console_canvas[..8]
            .iter()
            .chain(&console_canvas[38_808..])
            .all(|&byte| byte == 0xa5)
    );
    for row in 0..100 {
        assert_eq!(
            &console_canvas[8 + row * 388 + 384..8 + row * 388 + 388],
            &[0xa5; 4]
        );
    }
    // 14 text columns reserve two prompt cells and one caret cell, leaving
    // eleven visible bytes; the cursor after 96 input bytes scrolls to x=82.
    let caret = 8 + 80 * 388 + 82 * 4;
    assert_eq!(&console_canvas[caret..caret + 4], &bgrx(ACCENT));
    {
        let mut driver = surface(&mut console_canvas[8..38_808], console_description)
            .expect("partial console update");
        // Distinct markers prove an input-only refresh does not repaint the
        // header/history; a content refresh must erase old history afterward.
        assert!(driver.pixel(4, 4, Color::rgb(1, 2, 3)));
        assert!(driver.pixel(4, 40, Color::rgb(4, 5, 6)));
        console.process(crate::keyboard::KeyEvent {
            key: crate::keyboard::Key::Escape,
            pressed: true,
            repeat: false,
            modifiers: crate::keyboard::Modifiers::default(),
        });
        driver.update_console_input(&console);
        assert_eq!(driver.console_caret_at(&console, 16, 72), Some(0));
        assert_eq!(driver.console_caret_at(&console, 91, 72), Some(0));
    }
    let header_marker = 8 + 4 * 388 + 4 * 4;
    let history_marker = 8 + 40 * 388 + 4 * 4;
    assert_eq!(
        &console_canvas[header_marker..header_marker + 4],
        &[3, 2, 1, 0]
    );
    assert_eq!(
        &console_canvas[history_marker..history_marker + 4],
        &[6, 5, 4, 0]
    );
    assert_eq!(&console_canvas[caret..caret + 4], &bgrx(PANEL));
    let cleared_caret = 8 + 80 * 388 + 16 * 4;
    assert_eq!(
        &console_canvas[cleared_caret..cleared_caret + 4],
        &bgrx(ACCENT)
    );
    console.dispatch(crate::console::Operation::Clear);
    {
        let mut driver = surface(&mut console_canvas[8..38_808], console_description)
            .expect("history clear update");
        driver.update_console_content(&console);
    }
    assert_eq!(
        &console_canvas[header_marker..header_marker + 4],
        &[3, 2, 1, 0]
    );
    assert_eq!(
        &console_canvas[history_marker..history_marker + 4],
        &bgrx(PANEL)
    );
    console.input_lost();
    {
        let mut driver = surface(&mut console_canvas[8..38_808], console_description)
            .expect("cleared console surface");
        driver.update_console_content(&console);
        driver.fill_rect(0, 0, 18, 7, PANEL);
        driver.text(0, 0, "Mm!", 1, TEXT);
    }
    assert_eq!(&console_canvas[8..12], &bgrx(TEXT));
    // Uppercase M has top-row ink; lowercase m has two empty leading rows.
    assert_eq!(&console_canvas[8 + 6 * 4..8 + 7 * 4], &bgrx(PANEL));
    assert_eq!(&console_canvas[8 + 14 * 4..8 + 15 * 4], &bgrx(TEXT));
    assert!(
        console_canvas[..8]
            .iter()
            .chain(&console_canvas[38_808..])
            .all(|&byte| byte == 0xa5)
    );
    for row in 0..100 {
        assert_eq!(
            &console_canvas[8 + row * 388 + 384..8 + row * 388 + 388],
            &[0xa5; 4]
        );
    }

    // RGB24 and RGB32 cursor damage must reproduce the original scene exactly
    // without reading VRAM. RAM-backed reference pixels also prove restoration
    // leaves an unrelated marker outside the damage rectangle untouched.
    let mut cursor_reference = [0xa5u8; 16_656];
    let mut cursor_canvas = [0xa5u8; 16_656];
    for format in [rgb24, bgr32] {
        cursor_reference.fill(0xa5);
        let pixel_bytes = usize::from(format.bpp / 8);
        let pitch = 64 * pixel_bytes + 4;
        let cursor_description = Description {
            width: 64,
            height: 64,
            pitch: pitch as u64,
            ..format
        };
        let end = 8 + pitch * 64;
        {
            let mut driver = surface(&mut cursor_reference[8..end], cursor_description)
                .expect("cursor reference");
            driver.render_console(&console);
        }
        cursor_canvas.copy_from_slice(&cursor_reference);
        for position in [
            (0, 0),
            (4, 4),
            (5, 23),
            (18, 36),
            (60, 62),
            (usize::MAX, usize::MAX),
        ] {
            {
                let mut driver =
                    surface(&mut cursor_canvas[8..end], cursor_description).expect("cursor draw");
                driver.draw_cursor(position.0, position.1);
            }
            if position.0 < 64 {
                assert_ne!(cursor_canvas, cursor_reference);
            } else {
                assert_eq!(cursor_canvas, cursor_reference);
            }
            {
                let mut driver = surface(&mut cursor_canvas[8..end], cursor_description)
                    .expect("cursor restore");
                assert!(driver.pixel(63, 0, Color::rgb(11, 22, 33)));
                driver.restore_cursor(&console, position.0, position.1);
                assert_eq!(driver.clip, None);
            }
            let marker = 8 + 63 * pixel_bytes;
            let encoded = Layout::validate(0x1000, cursor_description)
                .unwrap()
                .encode(Color::rgb(11, 22, 33));
            assert_eq!(
                &cursor_canvas[marker..marker + pixel_bytes],
                &encoded[..pixel_bytes]
            );
            cursor_canvas[marker..marker + pixel_bytes]
                .copy_from_slice(&cursor_reference[marker..marker + pixel_bytes]);
            assert_eq!(cursor_canvas, cursor_reference);
        }
        // A preexisting inner damage clip is intersected, then preserved.
        {
            let mut driver = surface(&mut cursor_canvas[8..end], cursor_description)
                .expect("nested cursor clip");
            let clip = Clip {
                left: 3,
                top: 3,
                right: 8,
                bottom: 8,
            };
            driver.clip = Some(clip);
            driver.draw_cursor(2, 2);
            driver.restore_cursor(&console, 2, 2);
            assert_eq!(driver.clip, Some(clip));
            driver.clip = None;
            assert_eq!(driver.console_caret_at(&console, 16, 36), None);
        }
        assert_eq!(cursor_canvas, cursor_reference);
        assert!(
            cursor_canvas[..8]
                .iter()
                .chain(&cursor_canvas[end..])
                .all(|&byte| byte == 0xa5)
        );
        for row in 0..64 {
            assert_eq!(
                &cursor_canvas[8 + row * pitch + 64 * pixel_bytes..8 + (row + 1) * pitch],
                &[0xa5; 4]
            );
        }
    }

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
    crash_screen_self_test();
    crate::serial::println!("MIRROR_FRAMEBUFFER_TESTS_OK");
}

/// The crash screen (spec 0051, point 11) on RGB32 and RGB24 test storage:
/// the screen is cleared to `danger_bg`, the first reason row reads back,
/// changing one of its pixels fails the readback, a long reason keeps only
/// its first row's cells, and a screen too small for the layout clips
/// everything and fails the readback without faulting.
#[cfg(feature = "boot-smoke")]
fn crash_screen_self_test() {
    fn screen(storage: &mut [u8], width: u64, height: u64, bpp: u16) -> CrashScreen<'_> {
        let description = Description {
            width,
            height,
            pitch: width * u64::from(bpp / 8),
            bpp,
            red: Mask { size: 8, shift: 16 },
            green: Mask { size: 8, shift: 8 },
            blue: Mask { size: 8, shift: 0 },
        };
        let layout = Layout::validate(storage.as_mut_ptr() as usize, description)
            .expect("crash screen test layout");
        assert!(layout.info.byte_len <= storage.len());
        // The slice grants exclusive writable RAM for the screen's lifetime,
        // as in the other fixtures here.
        CrashScreen::new(Driver {
            address: NonNull::new(storage.as_mut_ptr()).expect("nonempty test storage"),
            layout,
            clip: None,
            _exclusive: PhantomData,
        })
    }

    for bpp in [32_u16, 24] {
        let pixel = usize::from(bpp / 8);
        let mut storage = alloc::vec![0x5a_u8; 160 * 100 * pixel];
        {
            let mut crash = screen(&mut storage, 160, 100, bpp);
            crash.reason(format_args!("crash screen test {}", 42));
            crash.reason(format_args!("at kernel/src/framebuffer.rs:1:1"));
            crash.detail(format_args!("CPU 0 crashed; 0 of 0 other CPUs stopped."));
            crash.frames(&[0xffff_ffff_8000_1234, 0xffff_ffff_8000_5678]);
            crash.action(format_args!("Halted."));
            assert_eq!(
                &crash.reason.bytes[..crash.reason.len],
                b"crash screen test 42"
            );
            assert!(crash.verify_reason(), "crash screen readback");
            // One wrong pixel in the reason row fails the readback.
            let (x, y) = (crash.reason.x as usize, crash.reason.y as usize);
            assert!(crash.driver.pixel(x + 1, y + 1, Color::rgb(1, 2, 3)));
            assert!(
                !crash.verify_reason(),
                "crash screen readback missed a pixel"
            );
        }
        // The corners keep danger_bg: the clear covered the whole screen.
        let background = Color::token(DEFAULT.danger_bg);
        let expected = [background.blue, background.green, background.red];
        for offset in [0, storage.len() - pixel] {
            assert_eq!(storage[offset..offset + 3], expected, "crash screen clear");
        }
    }
    // A long reason wraps at the right margin; the record keeps row 0's 24
    // cells (8 px margins and 6 px cells at scale 1 on 160 px).
    let mut storage = alloc::vec![0_u8; 160 * 100 * 4];
    {
        let mut crash = screen(&mut storage, 160, 100, 32);
        crash.reason(format_args!("{:-<80}", "wrapped"));
        assert_eq!(crash.reason.len, 24);
        assert!(crash.verify_reason());
    }
    // Too small for the margins: nothing is drawn and nothing faults.
    let mut tiny = alloc::vec![0_u8; 16 * 8 * 4];
    let mut crash = screen(&mut tiny, 16, 8, 32);
    crash.reason(format_args!("clipped"));
    crash.frames(&[0xffff_ffff_8000_1234; 32]);
    assert!(!crash.verify_reason(), "an off-screen reason row read back");
}
