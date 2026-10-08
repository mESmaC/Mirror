//! Userspace palette scene and dirty-tile RGBX transport. The cursor is an
//! upload-time overlay, so moving it cannot damage the retained scene pixels.

use crate::{SYS_SURFACE_PRESENT, font, require, syscall};

pub const MAX_WIDTH: usize = 1280;
pub const MAX_HEIGHT: usize = 800;
const MAX_PIXELS: usize = MAX_WIDTH * MAX_HEIGHT;
const EDGE: usize = 16;
const DIRTY_WORDS: usize = (MAX_WIDTH / EDGE * (MAX_HEIGHT / EDGE)).div_ceil(64);
const CURSOR_WIDTH: usize = 12;
const CURSOR_HEIGHT: usize = 20;

pub const DESKTOP: u8 = 0;
pub const TOPBAR: u8 = 1;
pub const PANEL: u8 = 2;
pub const RAISED: u8 = 3;
pub const BORDER: u8 = 4;
pub const TEXT: u8 = 5;
pub const MUTED: u8 = 6;
pub const ACCENT: u8 = 7;
pub const ACCENT_DARK: u8 = 8;
pub const FIELD: u8 = 9;
pub const BUTTON: u8 = 10;
pub const SHADOW: u8 = 11;
const WHITE: u8 = 12;
const BLACK: u8 = 13;
pub const WARNING: u8 = 14;

const PALETTE: [[u8; 4]; 15] = [
    [13, 20, 33, 0],
    [18, 28, 44, 0],
    [25, 38, 57, 0],
    [31, 47, 67, 0],
    [55, 75, 97, 0],
    [235, 243, 252, 0],
    [149, 171, 194, 0],
    [73, 215, 199, 0],
    [23, 91, 96, 0],
    [13, 24, 39, 0],
    [59, 194, 182, 0],
    [6, 11, 20, 0],
    [255, 255, 255, 0],
    [5, 8, 13, 0],
    [250, 195, 94, 0],
];

static mut PIXELS: [u8; MAX_PIXELS] = [0; MAX_PIXELS];
static mut CLAIMED: bool = false;

#[derive(Clone, Copy)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Rect {
    pub const fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(self, x: usize, y: usize) -> bool {
        x >= self.x && y >= self.y && x - self.x < self.width && y - self.y < self.height
    }
}

pub struct Renderer {
    pixels: &'static mut [u8; MAX_PIXELS],
    width: usize,
    height: usize,
    columns: usize,
    dirty: [u64; DIRTY_WORDS],
    cursor_x: usize,
    cursor_y: usize,
}

impl Renderer {
    /// This application has one thread and creates exactly one renderer. The
    /// static scene remains in BSS, rather than materializing a megabyte on stack.
    pub fn new(width: usize, height: usize) -> Self {
        require(
            (1..=MAX_WIDTH).contains(&width) && (1..=MAX_HEIGHT).contains(&height),
            80,
        );
        // SAFETY: The sole userspace thread is the only caller. CLAIMED guards
        // against a second mutable reference; no signal/interrupt code runs here.
        let pixels = unsafe {
            require(!(&raw const CLAIMED).read(), 81);
            (&raw mut CLAIMED).write(true);
            let pointer = &raw mut PIXELS;
            &mut *pointer
        };
        Self {
            pixels,
            width,
            height,
            columns: width.div_ceil(EDGE),
            dirty: [0; DIRTY_WORDS],
            cursor_x: width / 2,
            cursor_y: height / 2,
        }
    }

    fn mark(&mut self, rect: Rect) {
        let right = rect.x.saturating_add(rect.width).min(self.width);
        let bottom = rect.y.saturating_add(rect.height).min(self.height);
        if rect.x >= right || rect.y >= bottom {
            return;
        }
        for row in rect.y / EDGE..=(bottom - 1) / EDGE {
            for column in rect.x / EDGE..=(right - 1) / EDGE {
                let index = row * self.columns + column;
                self.dirty[index / 64] |= 1 << (index % 64);
            }
        }
    }

    pub fn fill(&mut self, rect: Rect, color: u8) {
        let right = rect.x.saturating_add(rect.width).min(self.width);
        let bottom = rect.y.saturating_add(rect.height).min(self.height);
        if rect.x >= right || rect.y >= bottom {
            return;
        }
        require(usize::from(color) < PALETTE.len(), 82);
        for y in rect.y..bottom {
            let start = y * self.width + rect.x;
            self.pixels[start..start + right - rect.x].fill(color);
        }
        self.mark(rect);
    }

    pub fn border(&mut self, rect: Rect, color: u8) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.fill(Rect::new(rect.x, rect.y, rect.width, 1), color);
        self.fill(
            Rect::new(rect.x, rect.y + rect.height - 1, rect.width, 1),
            color,
        );
        self.fill(Rect::new(rect.x, rect.y, 1, rect.height), color);
        self.fill(
            Rect::new(rect.x + rect.width - 1, rect.y, 1, rect.height),
            color,
        );
    }

    pub fn text(&mut self, x: usize, y: usize, bytes: &[u8], scale: usize, color: u8) {
        for (index, &byte) in bytes.iter().enumerate() {
            let rows = font::glyph(byte);
            for (row, bits) in rows.into_iter().enumerate() {
                for column in 0..5 {
                    if bits & (1 << (4 - column)) != 0 {
                        self.fill(
                            Rect::new(
                                x + (index * 6 + column) * scale,
                                y + row * scale,
                                scale,
                                scale,
                            ),
                            color,
                        );
                    }
                }
            }
        }
    }

    pub fn move_cursor(&mut self, x: usize, y: usize) {
        let x = x.min(self.width - 1);
        let y = y.min(self.height - 1);
        if (x, y) == (self.cursor_x, self.cursor_y) {
            return;
        }
        self.mark(Rect::new(
            self.cursor_x,
            self.cursor_y,
            CURSOR_WIDTH,
            CURSOR_HEIGHT,
        ));
        self.cursor_x = x;
        self.cursor_y = y;
        self.mark(Rect::new(x, y, CURSOR_WIDTH, CURSOR_HEIGHT));
    }

    pub fn flush(&mut self, surface: u64) -> bool {
        let mut packet = [0u8; 16 + EDGE * EDGE * 4];
        let mut changed = false;
        for word in 0..DIRTY_WORDS {
            let mut pending = core::mem::take(&mut self.dirty[word]);
            while pending != 0 {
                let tile = word * 64 + pending.trailing_zeros() as usize;
                pending &= pending - 1;
                let x = tile % self.columns * EDGE;
                let y = tile / self.columns * EDGE;
                let width = (self.width - x).min(EDGE);
                let height = (self.height - y).min(EDGE);
                for (index, value) in [x, y, width, height].into_iter().enumerate() {
                    packet[index * 4..index * 4 + 4].copy_from_slice(&(value as u32).to_le_bytes());
                }
                for row in 0..height {
                    for column in 0..width {
                        let px = x + column;
                        let py = y + row;
                        let base = self.pixels[py * self.width + px];
                        let color = if px >= self.cursor_x && py >= self.cursor_y {
                            cursor_pixel(px - self.cursor_x, py - self.cursor_y).unwrap_or(base)
                        } else {
                            base
                        };
                        let offset = 16 + (row * width + column) * 4;
                        packet[offset..offset + 4].copy_from_slice(&PALETTE[usize::from(color)]);
                    }
                }
                let length = 16 + width * height * 4;
                require(
                    syscall(
                        SYS_SURFACE_PRESENT,
                        surface,
                        packet.as_ptr() as u64,
                        length as u64,
                    ) == length as i64,
                    83,
                );
                changed = true;
            }
        }
        changed
    }
}

fn cursor_pixel(x: usize, y: usize) -> Option<u8> {
    if x >= CURSOR_WIDTH || y >= CURSOR_HEIGHT {
        return None;
    }
    if y <= 14 && x <= y / 2 {
        return Some(if x == 0 || x == y / 2 || y == 14 {
            BLACK
        } else {
            WHITE
        });
    }
    if (4..=7).contains(&x) && (11..=19).contains(&y) {
        return Some(if x == 4 || x == 7 || y == 19 {
            BLACK
        } else {
            WHITE
        });
    }
    None
}
