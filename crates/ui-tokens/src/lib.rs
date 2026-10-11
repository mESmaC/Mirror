#![no_std]
#![forbid(unsafe_code)]
//! Mirror's UI design tokens (spec 0029): the solid colors of the dark and
//! light themes, the chrome geometry and the type roles, shared by the kernel
//! boot screen and console, the desktop shell and its apps.
//!
//! The colors are copied from the `.t-dark` and `.t-light` blocks of
//! `docs/design/design-system.html` (0029-D3), and the host tests check each
//! one against that file. Dark is the default and the only theme drawn
//! (0029-D2, ledger S21); light is defined and tested but not drawn.
//!
//! Today's renderer draws opaque, palette-indexed rectangles, so six design
//! tokens are not representable here: `--ring` (accent at 32%; the focus ring
//! is the solid `focus_ring` instead, 0029-D7), `--shadow-1`, `--shadow-2`,
//! `--shadow-3` (elevation is border-only), `--glass` and `--wallpaper`.

/// An opaque sRGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// The renderers' pixel bytes: red, green, blue, then an unused byte of 0.
    pub const fn rgbx(self) -> [u8; 4] {
        [self.r, self.g, self.b, 0]
    }
}

/// A color written as its CSS hex value, `0xRRGGBB`.
const fn hex(value: u32) -> Rgb {
    assert!(value <= 0xFF_FFFF, "a CSS hex color has three bytes");
    Rgb {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    }
}

/// The 27 solid colors of one theme, in the CSS order of
/// `design-system.html`'s theme blocks. Apps never pick raw colors, so a theme
/// switch is a table swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Desktop and windowed-app backdrop.
    pub base: Rgb,
    /// Windows, cards and inputs.
    pub surface: Rgb,
    /// Menus, toasts and popovers.
    pub elevated: Rgb,
    /// Hover fills and pressed tracks.
    pub overlay: Rgb,
    /// Card edges and separators.
    pub border_subtle: Rgb,
    /// Control outlines and window frames.
    pub border_strong: Rgb,
    /// Headings, values and body text.
    pub text_primary: Rgb,
    /// Descriptions and labels.
    pub text_secondary: Rgb,
    /// Placeholders and captions.
    pub text_tertiary: Rgb,
    /// Primary actions, focus and the active app.
    pub accent: Rgb,
    /// An accent fill under the pointer.
    pub accent_hover: Rgb,
    /// An accent fill while pressed.
    pub accent_pressed: Rgb,
    /// Text and glyphs on an accent fill.
    pub on_accent: Rgb,
    /// Accent-tinted tiles and badges.
    pub accent_subtle: Rgb,
    /// Text and glyphs on accent-subtle.
    pub accent_subtle_text: Rgb,
    /// Completion and healthy state.
    pub success: Rgb,
    /// The subtle background behind success text.
    pub success_bg: Rgb,
    /// Caution and recoverable issues.
    pub warning: Rgb,
    /// The subtle background behind warning text.
    pub warning_bg: Rgb,
    /// Errors and destructive actions.
    pub danger: Rgb,
    /// The subtle background behind danger text.
    pub danger_bg: Rgb,
    /// A danger fill under the pointer.
    pub danger_hover: Rgb,
    /// A danger fill while pressed.
    pub danger_pressed: Rgb,
    /// Text and glyphs on a danger fill.
    pub on_danger: Rgb,
    /// Neutral notices and updates.
    pub info: Rgb,
    /// The subtle background behind info text.
    pub info_bg: Rgb,
    /// The focus ring around every keyboard-focusable control.
    pub focus_ring: Rgb,
}

impl Theme {
    /// All 27 colors in field (CSS) order. The desktop palette's first 27
    /// indices are these, in this order (0029-D4).
    pub const fn colors(&self) -> [Rgb; 27] {
        [
            self.base,
            self.surface,
            self.elevated,
            self.overlay,
            self.border_subtle,
            self.border_strong,
            self.text_primary,
            self.text_secondary,
            self.text_tertiary,
            self.accent,
            self.accent_hover,
            self.accent_pressed,
            self.on_accent,
            self.accent_subtle,
            self.accent_subtle_text,
            self.success,
            self.success_bg,
            self.warning,
            self.warning_bg,
            self.danger,
            self.danger_bg,
            self.danger_hover,
            self.danger_pressed,
            self.on_danger,
            self.info,
            self.info_bg,
            self.focus_ring,
        ]
    }
}

/// The dark theme, from `.t-dark` in `design-system.html`.
pub const DARK: Theme = Theme {
    base: hex(0x0B0F16),
    surface: hex(0x121826),
    elevated: hex(0x1A2334),
    overlay: hex(0x232E44),
    border_subtle: hex(0x26314A),
    border_strong: hex(0x3B4A6B),
    text_primary: hex(0xECF1F9),
    text_secondary: hex(0xA6B2C7),
    text_tertiary: hex(0x71809A),
    accent: hex(0x4FD8C2),
    accent_hover: hex(0x71E3D1),
    accent_pressed: hex(0x38BFA9),
    on_accent: hex(0x052B25),
    accent_subtle: hex(0x123A35),
    accent_subtle_text: hex(0x7CE7D4),
    success: hex(0x34D399),
    success_bg: hex(0x0F2E25),
    warning: hex(0xFBBF24),
    warning_bg: hex(0x2E2410),
    danger: hex(0xF87171),
    danger_bg: hex(0x381A1F),
    danger_hover: hex(0xFA8A8A),
    danger_pressed: hex(0xE05D5D),
    on_danger: hex(0x2B0B0B),
    info: hex(0x60A5FA),
    info_bg: hex(0x16283F),
    focus_ring: hex(0x71E3D1),
};

/// The light theme, from `.t-light` in `design-system.html`. Not drawn until
/// the theme preference (0068) and the Settings app (0088) ship it.
pub const LIGHT: Theme = Theme {
    base: hex(0xF3F5F9),
    surface: hex(0xFFFFFF),
    elevated: hex(0xFFFFFF),
    overlay: hex(0xEEF2F7),
    border_subtle: hex(0xE2E8F1),
    border_strong: hex(0xC6D0DF),
    text_primary: hex(0x0F1726),
    text_secondary: hex(0x475569),
    text_tertiary: hex(0x5D6B82),
    accent: hex(0x0B7C6F),
    accent_hover: hex(0x0A6C61),
    accent_pressed: hex(0x085A50),
    on_accent: hex(0xFFFFFF),
    accent_subtle: hex(0xE2F3EE),
    accent_subtle_text: hex(0x0B5F55),
    success: hex(0x047857),
    success_bg: hex(0xE7F7F0),
    warning: hex(0xB45309),
    warning_bg: hex(0xFBF0DC),
    danger: hex(0xC81E1E),
    danger_bg: hex(0xFCEBEB),
    danger_hover: hex(0xB01B1B),
    danger_pressed: hex(0x971717),
    on_danger: hex(0xFFFFFF),
    info: hex(0x2563EB),
    info_bg: hex(0xEBF1FE),
    focus_ring: hex(0x0B7C6F),
};

/// The theme every surface draws (0029-D2).
pub const DEFAULT: Theme = DARK;

// Geometry, in pixels at whole-number scale, from `design-system.html`.

/// Spacing scale sp-1 (section 03). The scale is on the 4 px grid.
pub const SP_1: usize = 4;
/// Spacing scale sp-2.
pub const SP_2: usize = 8;
/// Spacing scale sp-3.
pub const SP_3: usize = 12;
/// Spacing scale sp-4.
pub const SP_4: usize = 16;
/// Spacing scale sp-5.
pub const SP_5: usize = 24;
/// Spacing scale sp-6.
pub const SP_6: usize = 32;
/// Spacing scale sp-7.
pub const SP_7: usize = 48;
/// Spacing scale sp-8.
pub const SP_8: usize = 64;

/// Height of a default control (`.btn`).
pub const CONTROL_HEIGHT: usize = 32;
/// Height of a compact control (`.btn.sm`), opt-in per surface.
pub const CONTROL_HEIGHT_COMPACT: usize = 28;
/// Nothing interactive is smaller than this along its shortest side.
pub const MIN_TARGET: usize = 28;
/// Height of a window titlebar (`.titlebar`), excluding its bottom border.
pub const TITLEBAR_HEIGHT: usize = 38;
/// Width of a window caption button (`.cap`).
pub const CAPTION_BUTTON_WIDTH: usize = 38;
/// Height of a window caption button (`.cap`).
pub const CAPTION_BUTTON_HEIGHT: usize = 28;
/// Edge of the square app tile in a titlebar (`.app-tile`).
pub const APP_TILE: usize = 20;
/// Width of the focus ring (`.btn:focus-visible`).
pub const FOCUS_RING_WIDTH: usize = 2;
/// Gap between a control's edge and its focus ring (`.btn:focus-visible`).
pub const FOCUS_RING_OFFSET: usize = 2;

/// Pointer sprite width (0029-D12).
pub const CURSOR_WIDTH: usize = 15;
/// Pointer sprite height (0029-D12).
pub const CURSOR_HEIGHT: usize = 19;
/// The pointer (0029-D12): a CUA-style arrowhead without a tail, a dart whose
/// proportions and concave back follow the Cua driver cursor (trycua/cua).
/// `X` is the dark edge, `O` the light fill and `.` leaves the scene
/// untouched. The hot spot is the top-left pixel, at the tip. The kernel
/// console and the desktop draw this same sprite.
pub const CURSOR: [&[u8; CURSOR_WIDTH]; CURSOR_HEIGHT] = [
    b"XXX............",
    b"XOXXX..........",
    b"XOOOXXX........",
    b"XOOOOOXXXX.....",
    b"XXOOOOOOOXXX...",
    b".XOOOOOOOOOXXX.",
    b".XOOOOOOOOOOOXX",
    b".XOOOOOOOOOOOOX",
    b".XOOOOOOOOOOXXX",
    b".XXOOOOOOOOXX..",
    b"..XOOOOOOOXX...",
    b"..XOOOOOOXX....",
    b"..XOOOOOXX.....",
    b"..XOOOOXX......",
    b"..XXOOOX.......",
    b"...XOOXX.......",
    b"...XOOX........",
    b"...XOXX........",
    b"...XXX.........",
];

/// Type roles on the 5x7 font (0029-D10, `design-system.html` section 02).
/// The font has one weight, so emphasis is color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// 30/38 px in the design.
    Display,
    /// 24/32 px in the design.
    Title1,
    /// 20/28 px in the design.
    Title2,
    /// 16/24 px in the design.
    Title3,
    /// 14/21 px in the design.
    Body,
    /// 13/18 px in the design: buttons, tabs and field labels.
    Label,
    /// 12/16 px in the design: hints and secondary metadata.
    Caption,
    /// 11/14 px capitals in the design.
    Overline,
    /// 13/20 px monospace in the design: code, console and metrics.
    Mono,
}

impl Role {
    /// The whole-number scale of the 5x7 font for this role. Its cap heights
    /// of 7, 14 and 21 px equal Inter's (about 0.73 em) at font sizes of about
    /// 10, 19 and 29 px.
    pub const fn scale(self) -> usize {
        match self {
            Self::Display => 3,
            Self::Title1 | Self::Title2 | Self::Title3 => 2,
            Self::Body | Self::Label | Self::Caption | Self::Overline | Self::Mono => 1,
        }
    }
}

#[cfg(test)]
mod tests;
