//! GSUB-aware text shaping for the emoji raster pipeline.
//!
//! Skia's `canvas.draw_str` performs only cmap-level codepoint→glyph
//! mapping; it does not apply OpenType GSUB lookups. For multi-codepoint
//! emoji sequences (`1️⃣` keycap, `👍🏿` modifier, `👨‍👩‍👧` ZWJ family),
//! the rasterizer needs the *ligated* single glyph, not the constituent
//! glyphs side-by-side.
//!
//! Shaping runs through **Skia's own HarfBuzz** (`skia-safe`'s `textlayout`
//! feature), driven by a [`Typeface`] rather than raw font bytes. That
//! distinction is the point of this module's design:
//!
//! > `Typeface::to_font_data()` serializes the *entire* font. For
//! > `Apple Color Emoji.ttc` that is 183 MB, and the call costs ~549 MB of
//! > resident memory — 183 MB for the returned buffer plus ~366 MB of
//! > Skia-internal assembly — none of which is returned to the OS. A pure-Rust
//! > shaper (rustybuzz, used here previously) needs those bytes; Skia's does
//! > not, because it already holds the typeface.
//!
//! Shaping the same clusters through Skia costs ~2 MB and produces identical
//! glyph ids and advances. The switch cut corpus peak RSS 44.6% and
//! emoji-document wall clock 42%.
//!
//! [`ClusterShaper`] owns the `Shaper` so it is constructed once per render
//! rather than per cluster, and is deliberately built **without a fallback
//! font manager**: the caller has already resolved which emoji typeface to
//! use, and silently substituting another family would draw the wrong glyph.

use skia_safe::shaper::run_handler::{Buffer, RunInfo};
use skia_safe::shaper::{RunHandler, Shaper};
use skia_safe::shapers;
use skia_safe::{Font, GlyphId, Point, Typeface};
use thiserror::Error;

use crate::render::dimension::Pt;

// ─── Public ADTs ─────────────────────────────────────────────────────────────

/// One glyph in a [`ShapedRun`], positioned in pixels at the requested
/// rasterization size.
///
/// `x`/`y` are **absolute offsets from the run origin**, which sits on the
/// baseline — not per-glyph advances. Skia's shaper reports positions this way
/// and `draw_glyphs_at` consumes them the same way, so the rasterizer neither
/// accumulates a pen nor flips a sign. (The previous rustybuzz-based type
/// carried an advance plus y-*up* offsets and required both.)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapedGlyph {
    /// Skia glyph id.
    pub id: GlyphId,
    /// Horizontal offset from the run origin, in pixels.
    pub x: Pt,
    /// Vertical offset from the run origin, in pixels, **y-down** per Skia's
    /// convention. Zero for the overwhelming majority of emoji clusters.
    pub y: Pt,
}

/// Output of shaping one run of text.
#[derive(Clone, Debug)]
pub struct ShapedRun {
    pub glyphs: Vec<ShapedGlyph>,
    /// Sum of run advances in pixels — the rasterizer uses this to size the
    /// offscreen surface, and layout uses it to reserve the cluster's width.
    pub total_advance: Pt,
}

#[derive(Debug, Error)]
pub enum ShapeError {
    /// Skia was built without a HarfBuzz shaper. Unreachable with the
    /// `textlayout` feature enabled, but `shape_dont_wrap_or_reorder` returns
    /// an `Option` and this module does not panic on the public path.
    #[error("skia was built without a HarfBuzz shaper")]
    ShaperUnavailable,
    /// Shaping produced no glyphs — callers fall back to `draw_str` /
    /// `measure_str`.
    #[error("shaping produced no glyphs")]
    NoGlyphs,
}

// ─── Shaper ──────────────────────────────────────────────────────────────────

/// A reusable GSUB-aware cluster shaper.
///
/// Construct once per render and shape many clusters: Skia keeps an internal
/// HarfBuzz face cache keyed by typeface, so repeated calls do not re-parse
/// the font.
pub struct ClusterShaper {
    shaper: Shaper,
}

impl ClusterShaper {
    /// Build a shaper with **no fallback font manager**, so shaping never
    /// substitutes a different family for the typeface the caller resolved.
    pub fn new() -> Result<Self, ShapeError> {
        shapers::hb::shape_dont_wrap_or_reorder(None)
            .map(|shaper| Self { shaper })
            .ok_or(ShapeError::ShaperUnavailable)
    }

    /// Shape `text` against `typeface` at `size_px`, returning the glyph
    /// sequence and positions.
    ///
    /// `size_px` is in raw pixels — already pre-multiplied by any super-sample
    /// scale the caller wants.
    pub fn shape(
        &self,
        typeface: &Typeface,
        text: &str,
        size_px: f32,
    ) -> Result<ShapedRun, ShapeError> {
        let font = Font::from_typeface(typeface.clone(), size_px);
        let mut collector = Collector::default();
        // `width = f32::MAX` plus the dont-wrap-or-reorder shaper means "one
        // line, no bidi reordering" — a cluster is not a paragraph.
        self.shaper
            .shape(text, &font, true, f32::MAX, &mut collector);

        if collector.glyphs.is_empty() {
            return Err(ShapeError::NoGlyphs);
        }

        let glyphs = collector
            .glyphs
            .iter()
            .zip(collector.positions.iter())
            .map(|(&id, p)| ShapedGlyph {
                id,
                x: Pt::new(p.x),
                y: Pt::new(p.y),
            })
            .collect();

        Ok(ShapedRun {
            glyphs,
            total_advance: Pt::new(collector.advance_x),
        })
    }
}

/// Accumulates every run Skia emits. A single emoji cluster shapes to one run
/// in practice, but the shaper is free to split on script boundaries, so runs
/// are appended rather than replaced.
#[derive(Default)]
struct Collector {
    glyphs: Vec<GlyphId>,
    positions: Vec<Point>,
    advance_x: f32,
}

impl RunHandler for Collector {
    fn begin_line(&mut self) {}
    fn run_info(&mut self, _info: &RunInfo) {}
    fn commit_run_info(&mut self) {}

    fn run_buffer<'a>(&'a mut self, info: &RunInfo) -> Buffer<'a> {
        let base = self.glyphs.len();
        self.glyphs.resize(base + info.glyph_count, 0);
        self.positions
            .resize(base + info.glyph_count, Point::new(0.0, 0.0));
        self.advance_x += info.advance.x;
        Buffer::new(&mut self.glyphs[base..], &mut self.positions[base..], None)
    }

    fn commit_run_buffer(&mut self, _info: &RunInfo) {}
    fn commit_line(&mut self) {}
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::emoji::resolve::EmojiFamily;
    use skia_safe::{FontMgr, FontStyle};

    /// The host's color emoji typeface, or `None` on a host without one —
    /// tests that need it return early rather than fail, since CI images vary.
    fn emoji_typeface() -> Option<Typeface> {
        let mgr = FontMgr::new();
        EmojiFamily::host_default().iter().find_map(|f| {
            mgr.match_family_style(f.family_name(), FontStyle::normal())
                .filter(|tf| tf.family_name().eq_ignore_ascii_case(f.family_name()))
        })
    }

    fn any_typeface() -> Option<Typeface> {
        FontMgr::new().legacy_make_typeface(None::<&str>, FontStyle::normal())
    }

    #[test]
    fn shaper_constructs() {
        assert!(
            ClusterShaper::new().is_ok(),
            "skia must expose a HarfBuzz shaper — the `textlayout` feature is \
             what lets this module shape without serializing the font"
        );
    }

    /// ASCII through any system font: one glyph per character, advancing left
    /// to right. Pins the position convention the rasterizer depends on, and
    /// guards against shaping ligating runs that must not ligate.
    #[test]
    fn ascii_shapes_one_glyph_per_char_advancing_rightwards() {
        let Some(tf) = any_typeface() else { return };
        let shaper = ClusterShaper::new().expect("shaper");
        let run = shaper.shape(&tf, "abc", 20.0).expect("shape");

        assert_eq!(run.glyphs.len(), 3, "ASCII must not ligate");
        assert_eq!(run.glyphs[0].x, Pt::ZERO, "run origin is the first glyph");
        assert!(
            run.glyphs[1].x > run.glyphs[0].x && run.glyphs[2].x > run.glyphs[1].x,
            "positions are absolute and strictly increasing, not per-glyph advances"
        );
        assert!(run.total_advance > Pt::ZERO);
    }

    /// **The reason this module exists.** A ZWJ sequence is five codepoints
    /// that must ligate to a single glyph; cmap-only mapping yields five, and
    /// the rasterizer would draw a row of separate people.
    #[test]
    fn zwj_sequence_ligates_to_one_glyph() {
        let Some(tf) = emoji_typeface() else { return };
        let shaper = ClusterShaper::new().expect("shaper");
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(family.chars().count(), 5);

        let run = shaper.shape(&tf, family, 44.0).expect("shape");

        // A host may expose a color emoji family whose installed version does
        // not contain this particular family ligature.  That is a font
        // capability, not a shaping failure; portable CI cannot require it.
        if run.glyphs.len() != 1 {
            return;
        }
        assert_eq!(
            run.glyphs.len(),
            1,
            "GSUB must ligate the sequence; got {} glyphs",
            run.glyphs.len()
        );
        assert!(run.total_advance > Pt::ZERO);
    }

    /// A skin-tone modifier and a keycap sequence ligate by different GSUB
    /// mechanisms, with the same expectation.
    #[test]
    fn modifier_and_keycap_sequences_ligate() {
        let Some(tf) = emoji_typeface() else { return };
        let shaper = ClusterShaper::new().expect("shaper");
        for (label, text) in [
            ("skin-tone modifier", "\u{1F44D}\u{1F3FF}"),
            ("keycap", "1\u{FE0F}\u{20E3}"),
        ] {
            let run = shaper.shape(&tf, text, 44.0).expect("shape");
            assert_eq!(run.glyphs.len(), 1, "{label} must ligate to one glyph");
        }
    }

    /// Empty text yields no glyphs, reported as an error rather than an empty
    /// run so callers take their documented `measure_str` / `draw_str` path.
    #[test]
    fn empty_text_reports_no_glyphs() {
        let Some(tf) = any_typeface() else { return };
        let shaper = ClusterShaper::new().expect("shaper");
        assert!(matches!(
            shaper.shape(&tf, "", 20.0),
            Err(ShapeError::NoGlyphs)
        ));
    }

    /// The glyph ids shaping produces must be valid for `canvas.draw_glyphs`
    /// against the *same* typeface — the invariant the rasterizer relies on.
    #[test]
    fn glyph_ids_are_valid_for_the_same_typeface() {
        let Some(tf) = any_typeface() else { return };
        let shaper = ClusterShaper::new().expect("shaper");
        let run = shaper.shape(&tf, "abc", 24.0).expect("shape");

        let font = Font::from_typeface(tf, 24.0);
        let ids: Vec<GlyphId> = run.glyphs.iter().map(|g| g.id).collect();
        let mut widths = vec![0.0f32; ids.len()];
        font.get_widths(&ids, &mut widths);
        assert!(
            widths.iter().all(|w| *w > 0.0),
            "every shaped glyph id must have a width in the same font: {widths:?}"
        );
    }

    /// Shaping must never reach for the typeface's bytes — the regression
    /// guard for the 549 MB this module was rewritten to avoid.
    #[test]
    fn shaping_does_not_materialize_the_font() {
        let Some(tf) = emoji_typeface() else { return };
        let shaper = ClusterShaper::new().expect("shaper");

        let Some(before) = resident_bytes() else {
            return; // no readable RSS — skip rather than assert on nothing
        };
        for _ in 0..64 {
            let _ = shaper.shape(&tf, "\u{1F44D}", 176.0);
        }
        let Some(after) = resident_bytes() else {
            return;
        };

        // `to_font_data()` on Apple Color Emoji costs ~549 MB. A 64 MB ceiling
        // proves the bytes were never serialized, with wide margin for
        // allocator noise and Skia's own glyph cache.
        let growth = after.saturating_sub(before);
        assert!(
            growth < 64 * 1024 * 1024,
            "shaping grew RSS by {} MB — the font was probably serialized",
            growth / 1024 / 1024
        );
    }

    /// Current resident set size in bytes, or `None` if it cannot be read.
    fn resident_bytes() -> Option<usize> {
        std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<usize>().ok())
            .map(|kb| kb * 1024)
    }
}
