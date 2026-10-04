//! Color depth: true color detection and the 256-color fallback.
//!
//! Without true color, every `Color::Rgb` of the theme is drawn as the
//! nearest color of the xterm 256-color palette, indices 16 to 255 (the
//! 6x6x6 cube and the gray ramp; 0 to 15 are user-configurable, so they are
//! never picked).

use ratatui::style::Color;

/// Whether a `$COLORTERM` value promises 24-bit color.
pub fn truecolor_from_env(colorterm: Option<&str>) -> bool {
    colorterm.is_some_and(|v| v.eq_ignore_ascii_case("truecolor") || v.eq_ignore_ascii_case("24bit"))
}

/// `$COLORTERM`; tests always see true color, so snapshots and color
/// assertions do not depend on the environment they run in.
pub fn colorterm_env() -> Option<String> {
    if cfg!(test) { Some("truecolor".into()) } else { std::env::var("COLORTERM").ok() }
}

/// Cube levels of the xterm palette.
const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// RGB of palette index 16..=255; `None` for the 16 configurable colors.
pub fn xterm_rgb(index: u8) -> Option<(u8, u8, u8)> {
    match index {
        0..=15 => None,
        16..=231 => {
            let i = index - 16;
            Some((LEVELS[(i / 36) as usize], LEVELS[(i / 6 % 6) as usize], LEVELS[(i % 6) as usize]))
        }
        _ => {
            let v = 8 + 10 * (index - 232);
            Some((v, v, v))
        }
    }
}

/// The palette index (16..=255) closest to `(r, g, b)` by squared RGB
/// distance; ties go to the lower index (cube before gray ramp).
pub fn nearest_xterm256(r: u8, g: u8, b: u8) -> u8 {
    let dist2 = |(pr, pg, pb): (u8, u8, u8)| {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
        d(pr, r) + d(pg, g) + d(pb, b)
    };
    (16..=255u8).min_by_key(|&i| dist2(xterm_rgb(i).expect("index >= 16"))).expect("non-empty range")
}

/// `Rgb` as its nearest palette index; other colors unchanged.
pub fn to_ansi256(c: Color) -> Color {
    match c {
        Color::Rgb(r, g, b) => Color::Indexed(nearest_xterm256(r, g, b)),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_colors_map_to_themselves() {
        for i in 16..=255u8 {
            let (r, g, b) = xterm_rgb(i).unwrap();
            assert_eq!(nearest_xterm256(r, g, b), i);
        }
        assert_eq!(xterm_rgb(15), None);
        assert_eq!(xterm_rgb(0), None);
    }

    #[test]
    fn nearest_known_values() {
        assert_eq!(nearest_xterm256(0, 0, 0), 16);
        assert_eq!(nearest_xterm256(255, 255, 255), 231);
        assert_eq!(nearest_xterm256(128, 128, 128), 244);
        assert_eq!(nearest_xterm256(95, 95, 95), 59);
        for r in (0..=255u8).step_by(15) {
            for g in (0..=255u8).step_by(15) {
                for b in (0..=255u8).step_by(15) {
                    assert!(nearest_xterm256(r, g, b) >= 16);
                }
            }
        }
    }

    #[test]
    fn only_rgb_is_mapped() {
        assert_eq!(to_ansi256(Color::Red), Color::Red);
        assert_eq!(to_ansi256(Color::Indexed(3)), Color::Indexed(3));
        assert_eq!(to_ansi256(Color::Reset), Color::Reset);
        assert_eq!(to_ansi256(Color::Rgb(0, 255, 0)), Color::Indexed(46));
    }

    #[test]
    fn colorterm_detection() {
        for v in ["truecolor", "24bit", "TrueColor", "24BIT"] {
            assert!(truecolor_from_env(Some(v)), "{v}");
        }
        assert!(!truecolor_from_env(None));
        for v in ["", "256", "yes"] {
            assert!(!truecolor_from_env(Some(v)), "{v}");
        }
        assert!(truecolor_from_env(colorterm_env().as_deref()));
    }
}
