//! Procedural tile rendering into the application's private offscreen surface.
//! Every commit follows a complete frame: no app-owned pixel buffer is needed.

use crate::{App, SYS_SURFACE_COMMIT, SYS_SURFACE_PRESENT, font, require, syscall};
use mirror_window_protocol::TILE;
pub use mirror_window_protocol::{HEIGHT, WIDTH};

pub const TEXT_X: usize = 16;
pub const TEXT_Y: usize = 72;
pub const TEXT_COLUMNS: usize = 48;
pub const TEXT_ADVANCE: usize = 6;
pub const TEXT_LINE: usize = 16;

type Color = [u8; 4];
const INK: Color = [34, 48, 57, 0];
const MUTED: Color = [100, 116, 124, 0];
const PAPER: Color = [250, 251, 245, 0];
const NOTE_BACKGROUND: Color = [231, 237, 227, 0];
const NOTE_ACCENT: Color = [46, 118, 101, 0];
const COUNTER_BACKGROUND: Color = [28, 39, 61, 0];
const COUNTER_PANEL: Color = [37, 52, 79, 0];
const COUNTER_TEXT: Color = [229, 238, 252, 0];
const COUNTER_MUTED: Color = [157, 179, 205, 0];
const COUNTER_ACCENT: Color = [110, 180, 245, 0];

fn rect(x: usize, y: usize, left: usize, top: usize, width: usize, height: usize) -> bool {
    x >= left && y >= top && x - left < width && y - top < height
}

fn text_pixel(x: usize, y: usize, left: usize, top: usize, text: &[u8], scale: usize) -> bool {
    if x < left || y < top || (y - top) / scale >= 7 {
        return false;
    }
    let column = (x - left) / scale;
    let Some(&character) = text.get(column / 6) else {
        return false;
    };
    let bit = column % 6;
    bit < 5 && font::glyph(character)[(y - top) / scale] & (1 << (4 - bit)) != 0
}

fn note_pixel(app: &App, x: usize, y: usize) -> Color {
    if text_pixel(x, y, 16, 16, b"NOTES", 2) {
        return INK;
    }
    if text_pixel(x, y, 16, 40, b"YOUR OWN SPACE TO WRITE", 1) {
        return MUTED;
    }
    if rect(x, y, 12, 60, 296, 76) {
        if x == 12 || x == 307 || y == 60 || y == 135 {
            return if app.focused { NOTE_ACCENT } else { MUTED };
        }
        for (row, text) in app.document.text().chunks(TEXT_COLUMNS).enumerate() {
            if text_pixel(x, y, TEXT_X, TEXT_Y + row * TEXT_LINE, text, 1) {
                return INK;
            }
        }
        let caret = app.document.caret();
        if app.focused
            && rect(
                x,
                y,
                TEXT_X + caret % TEXT_COLUMNS * TEXT_ADVANCE,
                TEXT_Y + caret / TEXT_COLUMNS * TEXT_LINE,
                1,
                9,
            )
        {
            return NOTE_ACCENT;
        }
        return PAPER;
    }
    if text_pixel(x, y, 16, 151, b"96 CHARACTERS / CLICK TO PLACE CARET", 1)
        || text_pixel(x, y, 16, 169, b"HOME  END  BACKSPACE  DELETE", 1)
    {
        return MUTED;
    }
    NOTE_BACKGROUND
}

fn counter_pixel(app: &App, x: usize, y: usize, number: &[u8]) -> Color {
    if text_pixel(x, y, 16, 16, b"COUNTER", 2) {
        return COUNTER_TEXT;
    }
    if text_pixel(
        x,
        y,
        16,
        40,
        b"ELAPSED TIMER TICKS / ABOUT 100 PER SECOND",
        1,
    ) {
        return COUNTER_MUTED;
    }
    if rect(x, y, 12, 60, 296, 64) {
        if y == 60 || y == 123 || x == 12 || x == 307 {
            return if app.focused {
                COUNTER_ACCENT
            } else {
                COUNTER_MUTED
            };
        }
        if text_pixel(x, y, 24, 83, number, 2) {
            return COUNTER_ACCENT;
        }
        return COUNTER_PANEL;
    }
    if rect(x, y, 16, 140, 112, 28) {
        if text_pixel(x, y, 42, 150, b"RESET", 1) {
            return COUNTER_BACKGROUND;
        }
        return COUNTER_ACCENT;
    }
    if text_pixel(x, y, 146, 151, b"OR PRESS ENTER", 1) {
        return COUNTER_MUTED;
    }
    COUNTER_BACKGROUND
}

pub fn paint(app: &App, surface: u64) {
    let mut number = [0u8; 20];
    let number = crate::decimal(app.elapsed, &mut number);
    let mut packet = [0u8; 16 + TILE * TILE * 4];
    packet[8..12].copy_from_slice(&(TILE as u32).to_le_bytes());
    packet[12..16].copy_from_slice(&(TILE as u32).to_le_bytes());
    for tile_y in (0..HEIGHT).step_by(TILE) {
        packet[4..8].copy_from_slice(&(tile_y as u32).to_le_bytes());
        for tile_x in (0..WIDTH).step_by(TILE) {
            packet[..4].copy_from_slice(&(tile_x as u32).to_le_bytes());
            for row in 0..TILE {
                for column in 0..TILE {
                    let color = if app.busy && tile_x + column == 0 && tile_y + row == 0 {
                        [255, 128, 0, 0]
                    } else if app.kind == 0 {
                        note_pixel(app, tile_x + column, tile_y + row)
                    } else {
                        counter_pixel(app, tile_x + column, tile_y + row, number)
                    };
                    let offset = 16 + (row * TILE + column) * 4;
                    packet[offset..offset + 4].copy_from_slice(&color);
                }
            }
            require(
                syscall(
                    SYS_SURFACE_PRESENT,
                    surface,
                    packet.as_ptr() as u64,
                    packet.len() as u64,
                ) == packet.len() as i64,
                50,
            );
        }
    }
    require(syscall(SYS_SURFACE_COMMIT, surface, 0, 0) == 0, 51);
}

pub fn note_position(x: usize, y: usize) -> Option<usize> {
    if !rect(x, y, 12, 60, 296, 76) {
        return None;
    }
    let column = ((x.saturating_sub(TEXT_X) + TEXT_ADVANCE / 2) / TEXT_ADVANCE).min(TEXT_COLUMNS);
    let row = y.saturating_sub(TEXT_Y) / TEXT_LINE;
    Some(row * TEXT_COLUMNS + column)
}

pub fn reset_hit(x: usize, y: usize) -> bool {
    rect(x, y, 16, 140, 112, 28)
}
