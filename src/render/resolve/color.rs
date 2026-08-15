//! Color resolution — Color::Auto to RGB, theme color index to RGB.

use crate::model::{Color, DocumentBackground, Theme, ThemeColorIndex, ThemeColorScheme};

use super::drawing_color::{hsl_to_rgb, rgba_to_hsl, Rgba};

/// Resolved RGB color (0xRRGGBB).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };
    pub const WHITE: Self = Self {
        r: 255,
        g: 255,
        b: 255,
    };
}

/// Context for resolving Color::Auto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorContext {
    /// Text color — Auto means black.
    Text,
    /// Background/fill color — Auto means white.
    Background,
}

/// Resolve a model Color to concrete RGB.
pub fn resolve_color(color: Color, context: ColorContext) -> RgbColor {
    match color {
        Color::Rgb(v) => rgb_from_u32(v),
        Color::Auto => match context {
            ColorContext::Text => RgbColor::BLACK,
            ColorContext::Background => RgbColor::WHITE,
        },
    }
}

/// Resolve a theme color index to RGB via the color scheme.
pub fn resolve_theme_color(index: ThemeColorIndex, scheme: &ThemeColorScheme) -> RgbColor {
    rgb_from_u32(scheme.resolve(index))
}

/// Resolve the solid-color page background selected for print-layout output.
///
/// The cached `w:color` is the fallback. A usable `w:themeColor` overrides it,
/// and its byte tint/shade is applied to HSL luminance using WordprocessingML
/// semantics. When both transforms are present Word uses tint.
pub fn resolve_document_background(
    background: Option<&DocumentBackground>,
    theme: Option<&Theme>,
    display_background_shape: bool,
) -> Option<RgbColor> {
    if !display_background_shape {
        return None;
    }
    let background = background?;
    let cached = resolve_color(background.color, ColorContext::Background);

    let (Some(theme_index), Some(theme)) = (background.theme_color, theme) else {
        return Some(cached);
    };
    let theme_color = resolve_theme_color(theme_index, &theme.color_scheme);
    Some(if let Some(tint) = background.theme_tint {
        apply_word_luminance_transform(theme_color, tint, true)
    } else if let Some(shade) = background.theme_shade {
        apply_word_luminance_transform(theme_color, shade, false)
    } else {
        theme_color
    })
}

/// WordprocessingML tint/shade transforms the HSL luminance by a byte factor.
/// `tint=true`: `L' = 1 - (1-L)*f`; shade: `L' = L*f`.
fn apply_word_luminance_transform(color: RgbColor, raw: u8, tint: bool) -> RgbColor {
    let rgba = Rgba {
        r: color.r as f32 / 255.0,
        g: color.g as f32 / 255.0,
        b: color.b as f32 / 255.0,
        a: 1.0,
    };
    let (h, s, l) = rgba_to_hsl(rgba);
    let factor = raw as f32 / 255.0;
    let transformed_l = if tint {
        1.0 - (1.0 - l) * factor
    } else {
        l * factor
    };
    let (r, g, b) = hsl_to_rgb(h, s, transformed_l);
    rgb_from_u32(Rgba { r, g, b, a: 1.0 }.to_rgb24())
}

/// Convert a packed u32 (0xRRGGBB) to RgbColor.
pub fn rgb_from_u32(v: u32) -> RgbColor {
    RgbColor {
        r: ((v >> 16) & 0xFF) as u8,
        g: ((v >> 8) & 0xFF) as u8,
        b: (v & 0xFF) as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_from_u32_red() {
        let c = rgb_from_u32(0xFF0000);
        assert_eq!(c, RgbColor { r: 255, g: 0, b: 0 });
    }

    #[test]
    fn rgb_from_u32_white() {
        let c = rgb_from_u32(0xFFFFFF);
        assert_eq!(c, RgbColor::WHITE);
    }

    #[test]
    fn rgb_from_u32_black() {
        let c = rgb_from_u32(0x000000);
        assert_eq!(c, RgbColor::BLACK);
    }

    #[test]
    fn rgb_from_u32_mixed() {
        let c = rgb_from_u32(0x1A2B3C);
        assert_eq!(
            c,
            RgbColor {
                r: 0x1A,
                g: 0x2B,
                b: 0x3C
            }
        );
    }

    #[test]
    fn resolve_color_rgb_passes_through() {
        let c = resolve_color(Color::Rgb(0x336699), ColorContext::Text);
        assert_eq!(
            c,
            RgbColor {
                r: 0x33,
                g: 0x66,
                b: 0x99
            }
        );
    }

    #[test]
    fn resolve_color_auto_text_is_black() {
        let c = resolve_color(Color::Auto, ColorContext::Text);
        assert_eq!(c, RgbColor::BLACK);
    }

    #[test]
    fn resolve_color_auto_background_is_white() {
        let c = resolve_color(Color::Auto, ColorContext::Background);
        assert_eq!(c, RgbColor::WHITE);
    }

    #[test]
    fn resolve_theme_color_accent1() {
        let scheme = ThemeColorScheme {
            accent1: 0x4472C4,
            ..Default::default()
        };
        let c = resolve_theme_color(ThemeColorIndex::Accent1, &scheme);
        assert_eq!(
            c,
            RgbColor {
                r: 0x44,
                g: 0x72,
                b: 0xC4
            }
        );
    }

    #[test]
    fn resolve_theme_color_dark1() {
        let scheme = ThemeColorScheme {
            dark1: 0x000000,
            ..Default::default()
        };
        let c = resolve_theme_color(ThemeColorIndex::Dark1, &scheme);
        assert_eq!(c, RgbColor::BLACK);
    }

    #[test]
    fn resolve_theme_color_hyperlink() {
        let scheme = ThemeColorScheme {
            hyperlink: 0x0563C1,
            ..Default::default()
        };
        let c = resolve_theme_color(ThemeColorIndex::Hyperlink, &scheme);
        assert_eq!(
            c,
            RgbColor {
                r: 0x05,
                g: 0x63,
                b: 0xC1
            }
        );
    }

    #[test]
    fn document_background_requires_both_background_and_display_policy() {
        let background = DocumentBackground {
            color: Color::Rgb(0x123456),
            theme_color: None,
            theme_tint: None,
            theme_shade: None,
        };
        assert_eq!(resolve_document_background(None, None, true), None);
        assert_eq!(
            resolve_document_background(Some(&background), None, false),
            None
        );
        assert_eq!(
            resolve_document_background(Some(&background), None, true),
            Some(rgb_from_u32(0x123456))
        );
    }

    #[test]
    fn document_background_resolves_onlyoffice_theme_tint() {
        let background = DocumentBackground {
            color: Color::Rgb(0xDEADBE),
            theme_color: Some(ThemeColorIndex::Accent5),
            theme_tint: Some(0x66),
            theme_shade: None,
        };
        let theme = Theme {
            color_scheme: ThemeColorScheme {
                accent5: 0x4472C4,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            resolve_document_background(Some(&background), Some(&theme), true),
            Some(rgb_from_u32(0xB4C7E7))
        );
    }

    #[test]
    fn document_background_theme_shade_and_tint_precedence_match_word() {
        let theme = Theme {
            color_scheme: ThemeColorScheme {
                accent2: 0xC0504D,
                ..Default::default()
            },
            ..Default::default()
        };
        let shaded = DocumentBackground {
            color: Color::WHITE,
            theme_color: Some(ThemeColorIndex::Accent2),
            theme_tint: None,
            theme_shade: Some(0xBF),
        };
        let got = resolve_document_background(Some(&shaded), Some(&theme), true).unwrap();
        let word = rgb_from_u32(0x943634);
        assert!(
            got.r.abs_diff(word.r) <= 1
                && got.g.abs_diff(word.g) <= 1
                && got.b.abs_diff(word.b) <= 1,
            "floating-point HSL conversion must stay within one byte of Word: {got:?}"
        );

        let tint_wins = DocumentBackground {
            theme_tint: Some(0xFF),
            theme_shade: Some(0x00),
            ..shaded
        };
        assert_eq!(
            resolve_document_background(Some(&tint_wins), Some(&theme), true),
            Some(rgb_from_u32(0xC0504D)),
            "themeTint wins when both attributes are present"
        );
    }

    #[test]
    fn document_background_missing_theme_uses_cached_color_without_transform() {
        let background = DocumentBackground {
            color: Color::Rgb(0xB4C7E7),
            theme_color: Some(ThemeColorIndex::Accent5),
            theme_tint: Some(0x00),
            theme_shade: None,
        };
        assert_eq!(
            resolve_document_background(Some(&background), None, true),
            Some(rgb_from_u32(0xB4C7E7))
        );
    }
}
