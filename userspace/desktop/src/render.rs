//! Userspace palette scene and dirty-tile RGBX transport. The cursor is an
//! upload-time overlay, so moving it cannot damage the retained scene pixels.

use crate::{SYS_SURFACE_PRESENT, font, require, syscall};
use mirror_ui_tokens::{
    CURSOR, CURSOR_HEIGHT, CURSOR_WIDTH, DEFAULT, FOCUS_RING_OFFSET, FOCUS_RING_WIDTH, Rgb, Theme,
};
use mirror_window_protocol::{FRAME_BYTES, HEIGHT, MAX_APPS, WIDTH};

pub const MAX_WIDTH: usize = 1280;
pub const MAX_HEIGHT: usize = 800;
const MAX_PIXELS: usize = MAX_WIDTH * MAX_HEIGHT;
const EDGE: usize = 16;
const DIRTY_WORDS: usize = (MAX_WIDTH / EDGE * (MAX_HEIGHT / EDGE)).div_ceil(64);

// Scene pixels are palette indices (0029-D4): the 27 solid colors of a theme
// in token-crate order, then the two cursor colors. Only the tokens the
// desktop draws are named; the check below ties each name to its token.
pub const BASE: u8 = 0;
pub const SURFACE: u8 = 1;
pub const ELEVATED: u8 = 2;
pub const OVERLAY: u8 = 3;
pub const BORDER_SUBTLE: u8 = 4;
pub const BORDER_STRONG: u8 = 5;
pub const TEXT_PRIMARY: u8 = 6;
pub const TEXT_SECONDARY: u8 = 7;
pub const TEXT_TERTIARY: u8 = 8;
pub const ACCENT: u8 = 9;
pub const ON_ACCENT: u8 = 12;
pub const ACCENT_SUBTLE: u8 = 13;
pub const ACCENT_SUBTLE_TEXT: u8 = 14;
pub const WARNING: u8 = 17;
pub const DANGER: u8 = 19;
pub const FOCUS_RING: u8 = 26;
const THEME_COLORS: usize = 27;
const CURSOR_FILL: u8 = 27;
const CURSOR_EDGE: u8 = 28;
const PALETTE_SIZE: usize = 29;
type Palette = [[u8; 4]; PALETTE_SIZE];

const _: () = {
    const fn same(left: Rgb, right: Rgb) -> bool {
        let left = left.rgbx();
        let right = right.rgbx();
        left[0] == right[0] && left[1] == right[1] && left[2] == right[2] && left[3] == right[3]
    }
    let colors = DEFAULT.colors();
    assert!(colors.len() == THEME_COLORS && THEME_COLORS == CURSOR_FILL as usize);
    let named = [
        (BASE, DEFAULT.base),
        (SURFACE, DEFAULT.surface),
        (ELEVATED, DEFAULT.elevated),
        (OVERLAY, DEFAULT.overlay),
        (BORDER_SUBTLE, DEFAULT.border_subtle),
        (BORDER_STRONG, DEFAULT.border_strong),
        (TEXT_PRIMARY, DEFAULT.text_primary),
        (TEXT_SECONDARY, DEFAULT.text_secondary),
        (TEXT_TERTIARY, DEFAULT.text_tertiary),
        (ACCENT, DEFAULT.accent),
        (ON_ACCENT, DEFAULT.on_accent),
        (ACCENT_SUBTLE, DEFAULT.accent_subtle),
        (ACCENT_SUBTLE_TEXT, DEFAULT.accent_subtle_text),
        (WARNING, DEFAULT.warning),
        (DANGER, DEFAULT.danger),
        (FOCUS_RING, DEFAULT.focus_ring),
    ];
    let mut index = 0;
    while index < named.len() {
        assert!(same(colors[named[index].0 as usize], named[index].1));
        index += 1;
    }
};

/// The upload table for `theme`. A later theme switch replaces the renderer's
/// table and damages the whole screen; the indexed scene stays as it is.
fn palette(theme: &Theme) -> Palette {
    let mut palette = [[0; 4]; PALETTE_SIZE];
    for (entry, color) in palette.iter_mut().zip(theme.colors()) {
        *entry = color.rgbx();
    }
    palette[usize::from(CURSOR_FILL)] = [255, 255, 255, 0];
    palette[usize::from(CURSOR_EDGE)] = [5, 8, 13, 0];
    palette
}

static mut PIXELS: [u8; MAX_PIXELS] = [0; MAX_PIXELS];
// Two application snapshots plus the indexed scene stay below the 2 MiB ELF
// mapping limit. Neither full-size RGBX scanout nor a stack framebuffer exists.
static mut TEXTURES: [[u8; FRAME_BYTES]; MAX_APPS] = [[0; FRAME_BYTES]; MAX_APPS];
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
    textures: &'static mut [[u8; FRAME_BYTES]; MAX_APPS],
    width: usize,
    height: usize,
    columns: usize,
    dirty: [u64; DIRTY_WORDS],
    cursor_x: usize,
    cursor_y: usize,
    palette: Palette,
}

#[derive(Clone, Copy)]
pub struct WindowView {
    pub slot: usize,
    /// Palette decoration inside this rectangle occludes lower content.
    pub outer: Rect,
    pub content: Rect,
    pub ready: bool,
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
        let (pixels, textures) = unsafe {
            require(!(&raw const CLAIMED).read(), 81);
            (&raw mut CLAIMED).write(true);
            let pointer = &raw mut PIXELS;
            let textures = &raw mut TEXTURES;
            (&mut *pointer, &mut *textures)
        };
        Self {
            pixels,
            textures,
            width,
            height,
            columns: width.div_ceil(EDGE),
            dirty: [0; DIRTY_WORDS],
            cursor_x: width / 2,
            cursor_y: height / 2,
            palette: palette(&DEFAULT),
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

    pub fn damage(&mut self, rect: Rect) {
        self.mark(rect);
    }

    /// Rebuild inexpensive palette decoration without uploading the whole
    /// screen. Existing cursor/texture damage survives the retained redraw.
    pub fn update_scene(&mut self, draw: impl FnOnce(&mut Self)) {
        let pending = self.dirty;
        draw(self);
        self.dirty = pending;
    }

    pub fn texture_tile(&mut self, slot: usize, rect: Rect, bytes: &[u8]) {
        require(
            slot < MAX_APPS
                && rect.x + rect.width <= WIDTH
                && rect.y + rect.height <= HEIGHT
                && bytes.len() == rect.width * rect.height * 4,
            86,
        );
        for row in 0..rect.height {
            let destination = ((rect.y + row) * WIDTH + rect.x) * 4;
            let source = row * rect.width * 4;
            self.textures[slot][destination..destination + rect.width * 4]
                .copy_from_slice(&bytes[source..source + rect.width * 4]);
        }
    }

    pub fn fill(&mut self, rect: Rect, color: u8) {
        let right = rect.x.saturating_add(rect.width).min(self.width);
        let bottom = rect.y.saturating_add(rect.height).min(self.height);
        if rect.x >= right || rect.y >= bottom {
            return;
        }
        require(usize::from(color) < self.palette.len(), 82);
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

    /// The focus ring of 0029-D7: FOCUS_RING_WIDTH nested 1 px borders after a
    /// FOCUS_RING_OFFSET px gap outside `rect`, in FOCUS_RING when `shown` and
    /// otherwise in `background`, so a partial repaint also clears an old ring.
    /// `Rect` is unsigned, so a ring next to the left or top screen edge
    /// saturates at x=0 or y=0.
    #[expect(
        dead_code,
        reason = "0029-D7 focus ring; only the retired Scratchpad drew it so far"
    )]
    pub fn focus_ring(&mut self, rect: Rect, shown: bool, background: u8) {
        let color = if shown { FOCUS_RING } else { background };
        for grow in FOCUS_RING_OFFSET + 1..=FOCUS_RING_OFFSET + FOCUS_RING_WIDTH {
            let x = rect.x.saturating_sub(grow);
            let y = rect.y.saturating_sub(grow);
            self.border(
                Rect::new(
                    x,
                    y,
                    rect.x + rect.width + grow - x,
                    rect.y + rect.height + grow - y,
                ),
                color,
            );
        }
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

    /// Views are ordered back to front. Every outer rectangle first restores
    /// the palette decoration at that pixel, hiding lower app textures.
    pub fn flush_windows(&mut self, surface: u64, windows: &[WindowView]) -> bool {
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
                        let base = self.palette[usize::from(self.pixels[py * self.width + px])];
                        let mut color = base;
                        for window in windows {
                            if window.outer.contains(px, py) {
                                color = base;
                                if window.ready && window.content.contains(px, py) {
                                    let source = ((py - window.content.y) * WIDTH + px
                                        - window.content.x)
                                        * 4;
                                    color.copy_from_slice(
                                        &self.textures[window.slot][source..source + 4],
                                    );
                                }
                            }
                        }
                        if px >= self.cursor_x
                            && py >= self.cursor_y
                            && let Some(cursor) =
                                cursor_pixel(px - self.cursor_x, py - self.cursor_y)
                        {
                            color = self.palette[usize::from(cursor)];
                        }
                        let offset = 16 + (row * width + column) * 4;
                        packet[offset..offset + 4].copy_from_slice(&color);
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

/// The shared pointer sprite (0029-D12) as palette indices.
fn cursor_pixel(x: usize, y: usize) -> Option<u8> {
    match CURSOR.get(y)?.get(x)? {
        b'X' => Some(CURSOR_EDGE),
        b'O' => Some(CURSOR_FILL),
        _ => None,
    }
}
