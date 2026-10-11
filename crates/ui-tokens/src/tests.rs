extern crate std;

use super::*;
use std::{format, string::String, vec::Vec};

/// The page the colors are copied from (0029-D3).
const DESIGN: &str = include_str!("../../../docs/design/design-system.html");

/// The CSS names of the 27 solid tokens, in `Theme` field order.
const NAMES: [&str; 27] = [
    "base",
    "surface",
    "elevated",
    "overlay",
    "border-subtle",
    "border-strong",
    "text-primary",
    "text-secondary",
    "text-tertiary",
    "accent",
    "accent-hover",
    "accent-pressed",
    "on-accent",
    "accent-subtle",
    "accent-subtle-text",
    "success",
    "success-bg",
    "warning",
    "warning-bg",
    "danger",
    "danger-bg",
    "danger-hover",
    "danger-pressed",
    "on-danger",
    "info",
    "info-bg",
    "focus-ring",
];

/// The alpha tokens the crate documents as not representable.
const ALPHA: [&str; 6] = [
    "ring",
    "shadow-1",
    "shadow-2",
    "shadow-3",
    "wallpaper",
    "glass",
];

/// `--ring` is accent at 32% in both theme blocks.
const RING_ALPHA: f64 = 0.32;

/// The design's targets (`design-system.html:1234`), which 0029-D11 adopts as
/// its floor: 4.5:1 for text and 3:1 for UI boundaries.
const TEXT: f64 = 4.5;
const UI: f64 = 3.0;

/// Theme label, CSS selector and tokens.
const THEMES: [(&str, &str, Theme); 2] = [("light", ".t-light", LIGHT), ("dark", ".t-dark", DARK)];

/// A token by its CSS name, read from the field rather than from `colors()`.
fn token(theme: &Theme, name: &str) -> Rgb {
    match name {
        "base" => theme.base,
        "surface" => theme.surface,
        "elevated" => theme.elevated,
        "overlay" => theme.overlay,
        "border-subtle" => theme.border_subtle,
        "border-strong" => theme.border_strong,
        "text-primary" => theme.text_primary,
        "text-secondary" => theme.text_secondary,
        "text-tertiary" => theme.text_tertiary,
        "accent" => theme.accent,
        "accent-hover" => theme.accent_hover,
        "accent-pressed" => theme.accent_pressed,
        "on-accent" => theme.on_accent,
        "accent-subtle" => theme.accent_subtle,
        "accent-subtle-text" => theme.accent_subtle_text,
        "success" => theme.success,
        "success-bg" => theme.success_bg,
        "warning" => theme.warning,
        "warning-bg" => theme.warning_bg,
        "danger" => theme.danger,
        "danger-bg" => theme.danger_bg,
        "danger-hover" => theme.danger_hover,
        "danger-pressed" => theme.danger_pressed,
        "on-danger" => theme.on_danger,
        "info" => theme.info,
        "info-bg" => theme.info_bg,
        "focus-ring" => theme.focus_ring,
        _ => panic!("unknown token --{name}"),
    }
}

/// The custom-property declarations of one theme block (`selector {` up to the
/// next `}`), as name and value pairs in source order.
fn declarations(selector: &str) -> Vec<(&'static str, &'static str)> {
    let open = format!("{selector} {{");
    assert_eq!(
        DESIGN.matches(open.as_str()).count(),
        1,
        "one {selector} block"
    );
    let start = DESIGN.find(open.as_str()).expect("the theme block") + open.len();
    let end = start + DESIGN[start..].find('}').expect("a closed theme block");
    DESIGN[start..end]
        .split(';')
        .map(str::trim)
        .filter(|declaration| !declaration.is_empty())
        .map(|declaration| {
            let (name, value) = declaration.split_once(':').expect("a declaration");
            let name = name.trim().strip_prefix("--").expect("a custom property");
            (name, value.trim())
        })
        .collect()
}

/// A CSS `#RRGGBB` value.
fn parse_hex(value: &str) -> Option<Rgb> {
    let digits = value.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(hex(u32::from_str_radix(digits, 16).ok()?))
}

/// A CSS `rgba(r, g, b, a)` value.
fn parse_rgba(value: &str) -> Option<(Rgb, f64)> {
    let inner = value.strip_prefix("rgba(")?.strip_suffix(')')?;
    let mut parts = inner.split(',').map(str::trim);
    let r = parts.next()?.parse().ok()?;
    let g = parts.next()?.parse().ok()?;
    let b = parts.next()?.parse().ok()?;
    let alpha = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((Rgb { r, g, b }, alpha))
}

/// WCAG 2.x relative luminance, with the sRGB threshold 0.03928 the design's
/// inspector uses (`accessibility.html:643-646`).
fn luminance(color: Rgb) -> f64 {
    let linear = |channel: u8| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

/// WCAG 2.x contrast ratio, lighter over darker.
fn ratio(first: Rgb, second: Rgb) -> f64 {
    let (first, second) = (luminance(first), luminance(second));
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

/// `color` at `alpha` over an opaque `background`, per channel, rounded to the
/// nearest byte.
fn blend(color: Rgb, alpha: f64, background: Rgb) -> Rgb {
    let mix = |top: u8, bottom: u8| {
        let value = (alpha * f64::from(top) + (1.0 - alpha) * f64::from(bottom)).round();
        assert!((0.0..=255.0).contains(&value));
        value as u8
    };
    Rgb {
        r: mix(color.r, background.r),
        g: mix(color.g, background.g),
        b: mix(color.b, background.b),
    }
}

fn pair(theme: &Theme, foreground: &str, background: &str) -> f64 {
    ratio(token(theme, foreground), token(theme, background))
}

#[test]
fn colors_match_the_design_file() {
    for (_, selector, theme) in THEMES {
        let declarations = declarations(selector);
        let solid: Vec<(&str, &str)> = declarations
            .iter()
            .copied()
            .filter(|(name, _)| !ALPHA.contains(name))
            .collect();
        let names: Vec<&str> = solid.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names, NAMES,
            "{selector}: its solid tokens or their order changed"
        );
        let colors = theme.colors();
        for (index, (name, value)) in solid.iter().enumerate() {
            assert_eq!(
                parse_hex(value),
                Some(token(&theme, name)),
                "{selector} --{name}: {value}"
            );
            assert_eq!(colors[index], token(&theme, name), "colors() at --{name}");
        }
        for alpha in ALPHA {
            let values: Vec<&str> = declarations
                .iter()
                .filter(|(name, _)| *name == alpha)
                .map(|(_, value)| *value)
                .collect();
            assert_eq!(values.len(), 1, "{selector} declares --{alpha} once");
            assert_eq!(
                parse_hex(values[0]),
                None,
                "{selector} --{alpha} is solid now"
            );
            if alpha == "ring" {
                assert_eq!(
                    parse_rgba(values[0]),
                    Some((theme.accent, RING_ALPHA)),
                    "{selector} --ring is accent at 32%"
                );
            }
        }
    }
}

#[test]
fn design_table_rows_reproduce() {
    // design-system.html:1238-1243 and accessibility.html:408-413, light then
    // dark, to one decimal. This checks the transcribed values.
    for (foreground, background, light, dark) in [
        ("text-primary", "base", "16.4", "16.9"),
        ("text-secondary", "base", "6.9", "9.0"),
        ("text-tertiary", "base", "4.9", "4.8"),
        ("accent", "base", "4.7", "10.9"),
        ("on-accent", "accent", "5.1", "8.7"),
        ("danger", "base", "5.3", "6.9"),
    ] {
        let shown = (
            format!("{:.1}", pair(&LIGHT, foreground, background)),
            format!("{:.1}", pair(&DARK, foreground, background)),
        );
        assert_eq!(
            shown,
            (String::from(light), String::from(dark)),
            "{foreground} on {background}"
        );
    }
    // :1244 and :414: success, warning and info on base span 4.6-11.5.
    let mut shown = Vec::new();
    for theme in [&LIGHT, &DARK] {
        for foreground in ["success", "warning", "info"] {
            let value: f64 = format!("{:.1}", pair(theme, foreground, "base"))
                .parse()
                .unwrap();
            assert!(
                (4.6..=11.5).contains(&value),
                "{foreground} on base {value}"
            );
            shown.push(value);
        }
    }
    let low = shown.iter().copied().fold(f64::INFINITY, f64::min);
    let high = shown.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    assert_eq!(format!("{low:.1}-{high:.1}"), "4.6-11.5");
}

#[test]
fn drawn_dark_pairs_meet_the_d11_floor() {
    // Every dark pair spec 0029 draws, to two decimals. border-strong and
    // border-subtle are D11's exceptions (open question 1), checked in
    // design_pairs_miss_only_the_known_misses.
    for (foreground, background, floor, expected) in [
        ("text-primary", "base", TEXT, "16.92"),
        ("text-primary", "surface", TEXT, "15.63"),
        ("text-primary", "elevated", TEXT, "13.88"),
        ("text-primary", "overlay", TEXT, "11.98"),
        ("text-secondary", "base", TEXT, "8.97"),
        ("text-secondary", "surface", TEXT, "8.29"),
        ("text-secondary", "elevated", TEXT, "7.36"),
        ("text-secondary", "overlay", TEXT, "6.35"),
        ("accent-subtle-text", "accent-subtle", TEXT, "8.46"),
        ("on-accent", "accent", TEXT, "8.66"),
        ("danger", "surface", TEXT, "6.41"),
        ("danger", "elevated", TEXT, "5.69"),
        ("warning", "surface", TEXT, "10.62"),
        ("warning", "elevated", TEXT, "9.43"),
        ("on-danger", "danger", TEXT, "6.57"),
        ("accent", "surface", TEXT, "10.08"),
        ("focus-ring", "base", UI, "12.44"),
        ("focus-ring", "surface", UI, "11.49"),
        ("accent", "overlay", UI, "7.72"),
        ("accent", "accent-subtle", UI, "7.09"),
        ("text-tertiary", "surface", UI, "4.44"),
        ("text-tertiary", "overlay", UI, "3.40"),
    ] {
        let value = pair(&DARK, foreground, background);
        assert_eq!(
            format!("{value:.2}"),
            expected,
            "{foreground} on {background}"
        );
        assert!(
            value >= floor,
            "{foreground} on {background} is under {floor}:1"
        );
    }
}

#[test]
fn design_pairs_miss_only_the_known_misses() {
    // The design's own pairs, against its own targets, in both themes: the
    // rows of design-system.html:1238-1245 (the semantic row expanded), the
    // borders on the surfaces they outline, and text-tertiary on surface.
    const PAIRS: [(&str, &str, f64); 20] = [
        ("text-primary", "base", TEXT),
        ("text-secondary", "base", TEXT),
        ("text-tertiary", "base", TEXT),
        ("accent", "base", TEXT),
        ("on-accent", "accent", TEXT),
        ("danger", "base", TEXT),
        ("success", "base", TEXT),
        ("warning", "base", TEXT),
        ("info", "base", TEXT),
        ("success", "success-bg", TEXT),
        ("warning", "warning-bg", TEXT),
        ("danger", "danger-bg", TEXT),
        ("info", "info-bg", TEXT),
        ("accent-subtle-text", "accent-subtle", TEXT),
        ("border-strong", "surface", UI),
        ("border-strong", "base", UI),
        ("border-strong", "elevated", UI),
        ("border-subtle", "surface", UI),
        ("border-subtle", "base", UI),
        ("text-tertiary", "surface", TEXT),
    ];
    let mut misses = Vec::new();
    for (label, _, theme) in THEMES {
        for (foreground, background, target) in PAIRS {
            let value = pair(&theme, foreground, background);
            if value < target {
                misses.push((label, foreground, background, format!("{value:.2}")));
            }
        }
        // --ring over surface, blended to opaque bytes before luminance.
        let ring = blend(theme.accent, RING_ALPHA, theme.surface);
        let value = ratio(ring, theme.surface);
        if value < UI {
            misses.push((label, "--ring", "surface", format!("{value:.2}")));
        }
    }
    // Spec 0029, acceptance test 1 and open question 1. A fixed token or a new
    // miss fails here until the spec, its open questions and this list agree.
    let mut expected: Vec<(&str, &str, &str, String)> = [
        ("dark", "border-strong", "surface", "2.01"),
        ("light", "border-strong", "surface", "1.56"),
        ("dark", "border-strong", "base", "2.17"),
        ("light", "border-strong", "base", "1.43"),
        ("dark", "border-strong", "elevated", "1.78"),
        ("light", "border-strong", "elevated", "1.56"),
        ("dark", "border-subtle", "surface", "1.37"),
        ("light", "border-subtle", "surface", "1.23"),
        ("dark", "border-subtle", "base", "1.48"),
        ("light", "border-subtle", "base", "1.13"),
        ("dark", "--ring", "surface", "2.13"),
        ("light", "--ring", "surface", "1.58"),
        ("dark", "text-tertiary", "surface", "4.44"),
        ("light", "warning", "warning-bg", "4.45"),
    ]
    .into_iter()
    .map(|(label, foreground, background, value)| {
        (label, foreground, background, String::from(value))
    })
    .collect();
    misses.sort();
    expected.sort();
    assert_eq!(misses, expected);
}

#[test]
fn default_theme_and_geometry() {
    assert_eq!(DEFAULT, DARK);
    let spacing = [SP_1, SP_2, SP_3, SP_4, SP_5, SP_6, SP_7, SP_8];
    assert_eq!(spacing, [4, 8, 12, 16, 24, 32, 48, 64]);
    assert!(spacing.iter().all(|step| step % 4 == 0), "the 4 px grid");
    assert_eq!(MIN_TARGET, 28);
    for size in [
        CONTROL_HEIGHT,
        CONTROL_HEIGHT_COMPACT,
        CAPTION_BUTTON_WIDTH,
        CAPTION_BUTTON_HEIGHT,
    ] {
        assert!(size >= MIN_TARGET, "an interactive size of {size} px");
    }
    let mut snippets = Vec::from([
        format!(
            "Default controls are <b>{CONTROL_HEIGHT} px</b> tall; window caption buttons are <b>{CAPTION_BUTTON_WIDTH} × {CAPTION_BUTTON_HEIGHT} px</b>"
        ),
        format!("Nothing interactive is smaller than <b>{MIN_TARGET} px</b>"),
        format!(".btn.sm {{ height: {CONTROL_HEIGHT_COMPACT}px;"),
        format!(".titlebar {{ height: {TITLEBAR_HEIGHT}px;"),
        format!(".cap {{ width: {CAPTION_BUTTON_WIDTH}px; height: {CAPTION_BUTTON_HEIGHT}px;"),
        format!(".app-tile {{ width: {APP_TILE}px; height: {APP_TILE}px;"),
        format!(
            "outline: {FOCUS_RING_WIDTH}px solid var(--focus-ring); outline-offset: {FOCUS_RING_OFFSET}px;"
        ),
    ]);
    for (index, step) in spacing.iter().enumerate() {
        snippets.push(format!(
            "<div class=\"lbl\">sp-{} · {step}</div>",
            index + 1
        ));
    }
    for snippet in snippets {
        assert!(
            DESIGN.contains(snippet.as_str()),
            "design-system.html lacks {snippet}"
        );
    }
}

#[test]
fn type_roles_follow_d10() {
    for (role, scale) in [
        (Role::Display, 3),
        (Role::Title1, 2),
        (Role::Title2, 2),
        (Role::Title3, 2),
        (Role::Body, 1),
        (Role::Label, 1),
        (Role::Caption, 1),
        (Role::Overline, 1),
        (Role::Mono, 1),
    ] {
        assert_eq!(role.scale(), scale, "{role:?}");
    }
}

#[test]
fn rgbx_matches_the_smoke_oracles() {
    // The shell's smoke oracles (0029 Interfaces): focused border, unfocused
    // border and ink.
    assert_eq!(DEFAULT.accent.rgbx(), [79, 216, 194, 0]);
    assert_eq!(DEFAULT.border_strong.rgbx(), [59, 74, 107, 0]);
    assert_eq!(DEFAULT.text_primary.rgbx(), [236, 241, 249, 0]);
}

#[test]
fn cursor_is_an_outlined_dart() {
    // 0029-D12: the hot spot is the dark tip, the outline is one pixel of edge
    // around a single light fill, and nothing hangs off it (no tail).
    let at = |x: isize, y: isize| -> u8 {
        if x < 0 || y < 0 || x >= CURSOR_WIDTH as isize || y >= CURSOR_HEIGHT as isize {
            b'.'
        } else {
            CURSOR[y as usize][x as usize]
        }
    };
    let neighbours = |x: isize, y: isize| {
        (-1..=1).flat_map(move |dy| (-1..=1).map(move |dx| at(x + dx, y + dy)))
    };
    assert_eq!(CURSOR[0][0], b'X', "the hot spot is the tip");
    let mut fill = Vec::new();
    for y in 0..CURSOR_HEIGHT as isize {
        for x in 0..CURSOR_WIDTH as isize {
            match at(x, y) {
                b'O' => {
                    assert!(
                        neighbours(x, y).all(|p| p != b'.'),
                        "fill at ({x}, {y}) is exposed"
                    );
                    fill.push((x, y));
                }
                b'X' => assert!(
                    neighbours(x, y).any(|p| p == b'O'),
                    "stray edge at ({x}, {y})"
                ),
                b'.' => {}
                other => panic!("unknown cursor pixel {other} at ({x}, {y})"),
            }
        }
    }
    // The fill is one 4-connected region.
    let mut reached = Vec::from([fill[0]]);
    let mut next = 0;
    while next < reached.len() {
        let (x, y) = reached[next];
        next += 1;
        for step in [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)] {
            if at(step.0, step.1) == b'O' && !reached.contains(&step) {
                reached.push(step);
            }
        }
    }
    assert_eq!(reached.len(), fill.len(), "the fill is split");
    // A dart has no tail: each row's fill is one run, and the widest row
    // is above the middle of the sprite.
    let runs = |row: &[u8; CURSOR_WIDTH]| {
        row.windows(2)
            .filter(|pair| pair[0] != b'O' && pair[1] == b'O')
            .count()
    };
    assert!(
        CURSOR.iter().all(|row| runs(row) <= 1),
        "a row's fill is split"
    );
    let widest = (0..CURSOR_HEIGHT)
        .max_by_key(|&y| CURSOR[y].iter().filter(|&&p| p == b'O').count())
        .unwrap();
    assert!(widest < CURSOR_HEIGHT / 2, "the widest row is at {widest}");
}
