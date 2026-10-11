//! Procedural tile rendering into the application's private offscreen surface.
//! Every commit follows a complete frame: no app-owned pixel buffer is needed.

use crate::{App, SYS_SURFACE_COMMIT, SYS_SURFACE_PRESENT, font, require, syscall};
use mirror_ui_tokens::{DEFAULT, FOCUS_RING_OFFSET, FOCUS_RING_WIDTH, Role};
use mirror_window_protocol::TILE;
pub use mirror_window_protocol::{HEIGHT, WIDTH};

pub const TEXT_X: usize = 16;
pub const TEXT_Y: usize = 72;
pub const TEXT_COLUMNS: usize = 48;
pub const TEXT_ADVANCE: usize = 6;
pub const TEXT_LINE: usize = 16;

// Type roles on the 5x7 font (0029-D10). Every string keeps today's scale and
// position; with one weight, emphasis is color.
const HEADING: Role = Role::Title1;
const READOUT: Role = Role::Title2;
const DESCRIPTION: Role = Role::Body;
const DOCUMENT: Role = Role::Body;
const HINT: Role = Role::Caption;
const BUTTON: Role = Role::Label;
// The caret, TEXT_ADVANCE and note_position assume one pixel per font cell.
const _: () = assert!(DOCUMENT.scale() == 1);

// Apps never pick raw colors (0029-D6): every color is a DEFAULT token.
type Color = [u8; 4];
const SURFACE: Color = DEFAULT.surface.rgbx();
const BORDER_STRONG: Color = DEFAULT.border_strong.rgbx();
const TEXT_PRIMARY: Color = DEFAULT.text_primary.rgbx();
const TEXT_SECONDARY: Color = DEFAULT.text_secondary.rgbx();
const ACCENT: Color = DEFAULT.accent.rgbx();
const ON_ACCENT: Color = DEFAULT.on_accent.rgbx();
const FOCUS_RING: Color = DEFAULT.focus_ring.rgbx();
/// Published only by the smoke-gated busy probe as a frame fence; deliberately
/// not a token, so no themed pixel can satisfy the desktop's wait for it.
const BUSY_MARKER: Color = [255, 128, 0, 0];

fn rect(x: usize, y: usize, left: usize, top: usize, width: usize, height: usize) -> bool {
    x >= left && y >= top && x - left < width && y - top < height
}

fn edge(x: usize, y: usize, left: usize, top: usize, width: usize, height: usize) -> bool {
    rect(x, y, left, top, width, height)
        && (x == left || y == top || x == left + width - 1 || y == top + height - 1)
}

/// The focus ring of 0029-D7 around a control at (left, top, width, height):
/// FOCUS_RING_WIDTH nested 1 px borders after a FOCUS_RING_OFFSET px gap. The
/// control must sit at least FOCUS_RING_OFFSET + FOCUS_RING_WIDTH px inside
/// the surface.
fn focus_ring(x: usize, y: usize, left: usize, top: usize, width: usize, height: usize) -> bool {
    (FOCUS_RING_OFFSET + 1..=FOCUS_RING_OFFSET + FOCUS_RING_WIDTH).any(|grow| {
        edge(
            x,
            y,
            left - grow,
            top - grow,
            width + 2 * grow,
            height + 2 * grow,
        )
    })
}

/// A field or the Counter box: border-strong, or accent while the app is focused.
fn outline(app: &App) -> Color {
    if app.focused { ACCENT } else { BORDER_STRONG }
}

fn text_pixel(x: usize, y: usize, left: usize, top: usize, text: &[u8], role: Role) -> bool {
    let scale = role.scale();
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
    if text_pixel(x, y, 16, 16, b"Notes", HEADING) {
        return TEXT_PRIMARY;
    }
    if text_pixel(x, y, 16, 40, b"Your own space to write", DESCRIPTION) {
        return TEXT_SECONDARY;
    }
    if rect(x, y, 12, 60, 296, 76) {
        // A focused field keeps its accent border inside the ring (0029-D7).
        if x == 12 || x == 307 || y == 60 || y == 135 {
            return outline(app);
        }
        for (row, text) in app.document.text().chunks(TEXT_COLUMNS).enumerate() {
            if text_pixel(x, y, TEXT_X, TEXT_Y + row * TEXT_LINE, text, DOCUMENT) {
                return TEXT_PRIMARY;
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
            return ACCENT;
        }
        return SURFACE;
    }
    // A text field shows its ring whenever focused, like CSS :focus-visible
    // on inputs (0029-D7): columns 8-9 and 310-311, rows 56-57 and 138-139.
    if app.focused && focus_ring(x, y, 12, 60, 296, 76) {
        return FOCUS_RING;
    }
    if text_pixel(x, y, 16, 151, b"96 characters / click to place caret", HINT)
        || text_pixel(x, y, 16, 169, b"Home  End  Backspace  Delete", HINT)
    {
        return TEXT_SECONDARY;
    }
    SURFACE
}

fn counter_pixel(app: &App, x: usize, y: usize, number: &[u8]) -> Color {
    if text_pixel(x, y, 16, 16, b"Counter", HEADING) {
        return TEXT_PRIMARY;
    }
    if text_pixel(
        x,
        y,
        16,
        40,
        b"Elapsed timer ticks / about 100 per second",
        DESCRIPTION,
    ) {
        return TEXT_SECONDARY;
    }
    if rect(x, y, 12, 60, 296, 64) {
        if y == 60 || y == 123 || x == 12 || x == 307 {
            return outline(app);
        }
        if text_pixel(x, y, 24, 83, number, READOUT) {
            return TEXT_PRIMARY;
        }
        return SURFACE;
    }
    // Reset is the primary button: accent fill, on-accent label (0029-D6).
    if rect(x, y, 16, 140, 112, 28) {
        if text_pixel(x, y, 42, 150, b"Reset", BUTTON) {
            return ON_ACCENT;
        }
        return ACCENT;
    }
    if text_pixel(x, y, 146, 151, b"or press Enter", HINT) {
        return TEXT_SECONDARY;
    }
    SURFACE
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
                        BUSY_MARKER
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
