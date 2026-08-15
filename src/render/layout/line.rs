//! Line fitting — break fragments into lines that fit within a max width.

use crate::render::dimension::Pt;
use crate::render::layout::fragment::Fragment;

/// A fitted line — a slice of fragments that fit within the available width.
#[derive(Debug)]
pub struct FittedLine {
    /// Indices into the fragment list: [start, end).
    pub start: usize,
    pub end: usize,
    /// Total width of all fragments in this line.
    pub width: Pt,
    /// Maximum height of any fragment in this line.
    pub height: Pt,
    /// Maximum height of text-only fragments on this line.
    /// §17.3.1.33: Auto line spacing multiplier applies to text metrics,
    /// not to inline image heights.
    pub text_height: Pt,
    /// Portion of `text_height` that automatic line-spacing multipliers may
    /// scale. Numbering labels are natural-height-only.
    pub auto_text_height: Pt,
    /// Maximum ascent of any text fragment in this line.
    pub ascent: Pt,
    /// Whether this line ends with an explicit line break.
    pub has_break: bool,
    /// Width of the one trailing punctuation scalar that was actually allowed
    /// past the text extent. The glyph remains part of `width` for painting;
    /// alignment and justification subtract this consumed exception.
    pub hanging_punct_width: Pt,
}

/// Break fragments into lines that fit within `max_width`.
///
/// Breaks at the last whitespace/hyphen boundary when a line overflows.
/// A single fragment wider than `max_width` gets its own line (no infinite loop).
///
/// `first_line_width`: if provided, the first line uses this narrower width
/// (e.g., to account for first-line indent). Subsequent lines use `max_width`.
pub fn fit_lines(fragments: &[Fragment], max_width: Pt) -> Vec<FittedLine> {
    fit_lines_with_first(
        fragments,
        max_width,
        max_width,
        crate::render::layout::paragraph::PTabGeometry {
            max_width,
            indent_left: Pt::ZERO,
            indent_first_line: Pt::ZERO,
            content_width: max_width,
            float_left: Pt::ZERO,
            float_right: Pt::ZERO,
        },
    )
}

/// Line fitting with separate first-line and remaining-line widths.
///
/// `ptab_geometry` is the paragraph geometry §17.3.1.30 position tabs resolve
/// against. Fitting needs it because a tab whose alignment point lies behind
/// the pen advances to the next line, and only fitting can create one.
pub fn fit_lines_with_first(
    fragments: &[Fragment],
    first_line_width: Pt,
    remaining_width: Pt,
    ptab_geometry: crate::render::layout::paragraph::PTabGeometry,
) -> Vec<FittedLine> {
    fit_lines_with_first_and_hanging(
        fragments,
        first_line_width,
        remaining_width,
        ptab_geometry,
        &[],
        false,
    )
}

/// Internal fitter entry point for §17.3.1.21 hanging punctuation.
/// `hanging_tail_widths` is parallel to `fragments`; each non-zero entry is the
/// precisely measured marginal width of that text fragment's final eligible
/// Unicode scalar. Public wrappers pass an empty slice so their behaviour is
/// unchanged.
pub(crate) fn fit_lines_with_first_and_hanging(
    fragments: &[Fragment],
    first_line_width: Pt,
    remaining_width: Pt,
    ptab_geometry: crate::render::layout::paragraph::PTabGeometry,
    hanging_tail_widths: &[Pt],
    allow_overflow_punctuation: bool,
) -> Vec<FittedLine> {
    if fragments.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let mut line_start = 0;
    let mut line_width = Pt::ZERO;
    // Width used only for overflow decisions. Unlike `line_width`, this
    // excludes the complete trailing-whitespace suffix even when that suffix
    // spans several run fragments. Word lets such spaces hang past the right
    // margin; measuring only the current fragment's trimmed width can turn an
    // earlier whitespace-only run into a spurious extra line.
    let mut line_trimmed_width = Pt::ZERO;
    // §17.3.1.30: where the pen actually *is*, as opposed to how much width
    // the line has accumulated. The two differ only across a position tab,
    // which jumps the pen to its anchor while contributing a nominal width.
    // Tracking it separately keeps `line_width` — and therefore every
    // non-ptab paragraph's fitting — bit-for-bit unchanged.
    let line_pen_start = |is_first_line: bool| {
        ptab_geometry.indent_left
            + if is_first_line {
                ptab_geometry.indent_first_line
            } else {
                Pt::ZERO
            }
    };
    let mut pen_x = line_pen_start(true);
    // Margin-relative position tabs use the physical pen rather than the
    // accumulated line width. Track an equivalent pen with the complete
    // trailing-space suffix removed so this path follows the same overflow
    // rule as ordinary lines.
    let mut pen_trimmed_x = pen_x;
    // §17.3.1.30: `relativeTo="margin"` measures against the full text area,
    // so once such a tab has placed content this line may legitimately use the
    // space a paragraph's own right indent excludes. Until then the ordinary
    // `content_width` bound applies.
    let mut margin_span_active = false;
    let mut line_height = Pt::ZERO;
    let mut line_text_height = Pt::ZERO;
    let mut line_auto_text_height = Pt::ZERO;
    let mut line_ascent = Pt::ZERO;
    let mut last_break_point = None; // index after which we can break
                                     // Snapshot the exception actually consumed after each accepted fragment.
                                     // If overflow later rolls back to an earlier legal break, the snapshot at
                                     // that exact boundary is authoritative.
    let mut hanging_after = vec![Pt::ZERO; fragments.len() + 1];
    let mut line_hanging_punct_width = Pt::ZERO;

    let mut i = 0;
    while i < fragments.len() {
        let frag = &fragments[i];

        // Explicit line break — emit current line including the break fragment.
        // A text-wrapping LineBreak carries a separately measured text-height
        // base so an empty visual line includes the run font's real leading.
        // Page/column breaks keep their existing fragment-height behaviour.
        if frag.is_line_break() {
            line_height = line_height.max(frag.height());
            let break_text_height = match frag {
                Fragment::LineBreak { text_height, .. } => *text_height,
                _ => frag.height(),
            };
            line_text_height = line_text_height.max(break_text_height);
            line_auto_text_height = line_auto_text_height.max(break_text_height);
            lines.push(FittedLine {
                start: line_start,
                end: i + 1,
                width: line_width,
                height: line_height,
                text_height: line_text_height,
                auto_text_height: line_auto_text_height,
                ascent: line_ascent,
                has_break: true,
                hanging_punct_width: line_hanging_punct_width,
            });
            line_start = i + 1;
            line_width = Pt::ZERO;
            line_trimmed_width = Pt::ZERO;
            pen_x = line_pen_start(lines.is_empty());
            pen_trimmed_x = pen_x;
            margin_span_active = false;
            line_height = Pt::ZERO;
            line_text_height = Pt::ZERO;
            line_auto_text_height = Pt::ZERO;
            line_ascent = Pt::ZERO;
            last_break_point = None;
            line_hanging_punct_width = Pt::ZERO;
            i += 1;
            continue;
        }

        // §17.3.1.30: a position tab whose alignment point lies behind the
        // pen advances to that point on the *next* line. Decided here rather
        // than at emission because it is a line break, and emission cannot
        // create one.
        if let Fragment::PTab {
            align, relative_to, ..
        } = frag
        {
            // Fitting cannot bound the zone by the line end the way emission
            // does — the lines do not exist yet — so it scans to the end of the
            // fragment list. Where the two disagree (a zone fitting then splits
            // for width) emission is authoritative for the final x; this only
            // decides whether the tab can be honoured on this line.
            let end = crate::render::layout::paragraph::zone_end(fragments, i, fragments.len());
            let placement = crate::render::layout::paragraph::resolve_ptab(
                *align,
                *relative_to,
                ptab_geometry,
                pen_x,
                || crate::render::layout::paragraph::zone_width(fragments, i + 1, end),
            );
            match placement {
                crate::render::layout::paragraph::PTabPlacement::Placed(at) => {
                    pen_x = at;
                    pen_trimmed_x = at;
                    margin_span_active |=
                        matches!(relative_to, crate::model::PTabRelativeTo::Margin);
                }
                crate::render::layout::paragraph::PTabPlacement::AdvancesToNextLine { .. } => {
                    // Only break when doing so can help. A tab already first
                    // on its line would find the same anchor behind the same
                    // pen on the next one — acting on a condition the action
                    // cannot change is how this engine's pagination loops have
                    // historically become infinite.
                    if line_start < i {
                        let m = measure_range(fragments, line_start, i);
                        lines.push(FittedLine {
                            start: line_start,
                            end: i,
                            width: m.width,
                            height: m.height,
                            text_height: m.text_height,
                            auto_text_height: m.auto_text_height,
                            ascent: m.ascent,
                            has_break: false,
                            hanging_punct_width: hanging_after[i],
                        });
                        line_start = i;
                        line_width = Pt::ZERO;
                        line_trimmed_width = Pt::ZERO;
                        pen_x = line_pen_start(lines.is_empty());
                        pen_trimmed_x = pen_x;
                        margin_span_active = false;
                        line_height = Pt::ZERO;
                        line_text_height = Pt::ZERO;
                        line_auto_text_height = Pt::ZERO;
                        line_ascent = Pt::ZERO;
                        last_break_point = None;
                        line_hanging_punct_width = Pt::ZERO;
                        // Re-evaluate this tab against the fresh line.
                        continue;
                    }
                }
            }
        }

        let frag_width = frag.width();
        let new_width = line_width + frag_width;
        let new_trimmed_width = match frag {
            Fragment::Text { text, .. }
                if text
                    .chars()
                    .all(crate::render::layout::fragment::is_trimmable_trailing_whitespace) =>
            {
                line_trimmed_width
            }
            // Bookmarks are zero-width structural markers. In particular,
            // they must not reintroduce a preceding trailing-space suffix
            // into the extent used for fitting.
            Fragment::Bookmark { .. } => line_trimmed_width,
            Fragment::Text { .. } => line_width + frag.trimmed_width(),
            _ => new_width,
        };
        let new_pen_x = if matches!(frag, Fragment::PTab { .. }) {
            pen_x
        } else {
            pen_x + frag_width
        };
        let new_trimmed_pen_x = match frag {
            Fragment::Text { text, .. }
                if text
                    .chars()
                    .all(crate::render::layout::fragment::is_trimmable_trailing_whitespace) =>
            {
                pen_trimmed_x
            }
            Fragment::Bookmark { .. } => pen_trimmed_x,
            Fragment::Text { .. } => pen_x + frag.trimmed_width(),
            _ => new_pen_x,
        };

        // Use first-line width for line 0, remaining width for subsequent lines.
        let current_max = if lines.is_empty() {
            first_line_width
        } else {
            remaining_width
        };

        // For overflow checking, use the width with the line's entire
        // trailing-whitespace suffix removed. The suffix may span multiple
        // OOXML runs with different formatting.
        let check_width = new_trimmed_width;

        // Check if adding this fragment overflows. Once a margin-relative tab
        // has placed content, the line's real right edge is the margin, and
        // the pen — not the accumulated width sum — says where we are.
        let (raw_extent, extent_limit) = if margin_span_active {
            (new_trimmed_pen_x, ptab_geometry.max_width)
        } else {
            (check_width, current_max)
        };
        let tail_width = if allow_overflow_punctuation {
            trailing_hanging_tail_width(fragments, hanging_tail_widths, line_start, i + 1)
        } else {
            Pt::ZERO
        };
        let consumed_hanging = if tail_width > Pt::ZERO
            && raw_extent > extent_limit
            && raw_extent - tail_width <= extent_limit
        {
            tail_width
        } else {
            Pt::ZERO
        };
        let overflows = raw_extent > extent_limit && consumed_hanging == Pt::ZERO;
        if overflows && line_start < i {
            // Overflow — break at last break point, or before this fragment.
            let break_at = last_break_point.unwrap_or(i);
            let m = measure_range(fragments, line_start, break_at);
            lines.push(FittedLine {
                start: line_start,
                end: break_at,
                width: m.width,
                height: m.height,
                text_height: m.text_height,
                auto_text_height: m.auto_text_height,
                ascent: m.ascent,
                has_break: false,
                hanging_punct_width: hanging_after[break_at],
            });
            line_start = break_at;
            line_width = Pt::ZERO;
            line_trimmed_width = Pt::ZERO;
            pen_x = line_pen_start(lines.is_empty());
            pen_trimmed_x = pen_x;
            margin_span_active = false;
            line_height = Pt::ZERO;
            line_text_height = Pt::ZERO;
            line_auto_text_height = Pt::ZERO;
            line_ascent = Pt::ZERO;
            last_break_point = None;
            line_hanging_punct_width = Pt::ZERO;
            // Refit every fragment after the chosen break point. When the
            // last legal break is earlier than `i`, merely re-evaluating the
            // current fragment skips the intervening fragments from width and
            // height accounting even though emission still draws them.
            i = break_at;
            continue;
        }

        // If this is the first fragment on the line and it overflows,
        // allow it (it will be the only fragment on this line). The
        // paragraph renderer will clip/overflow as needed.
        line_width = new_width;
        line_trimmed_width = new_trimmed_width;
        // A position tab has already jumped the pen to its anchor. These
        // values are assigned only after the overflow decision, so refitting a
        // fragment starts from the correct pre-fragment position.
        pen_x = new_pen_x;
        pen_trimmed_x = new_trimmed_pen_x;
        line_height = line_height.max(frag.height());
        let metrics = fragment_line_metrics(frag);
        line_text_height = line_text_height.max(metrics.text_height);
        line_auto_text_height = line_auto_text_height.max(metrics.auto_text_height);
        line_ascent = line_ascent.max(metrics.ascent);

        // Track break opportunity: only after fragments that end with whitespace,
        // or non-text fragments (tabs, images). Text fragments without trailing
        // whitespace are mid-word continuations (e.g., a word split across runs)
        // and must not be broken.
        let is_break_point = match frag {
            Fragment::Text { text, .. } => {
                crate::render::layout::fragment::text_allows_line_break_after(text)
            }
            _ => true, // tabs, images, line breaks are always break points
        };
        if is_break_point {
            last_break_point = Some(i + 1);
        }

        line_hanging_punct_width = consumed_hanging;
        hanging_after[i + 1] = consumed_hanging;

        i += 1;
    }

    // Emit remaining fragments as the last line.
    if line_start < fragments.len() {
        lines.push(FittedLine {
            start: line_start,
            end: fragments.len(),
            width: line_width,
            height: line_height,
            text_height: line_text_height,
            auto_text_height: line_auto_text_height,
            ascent: line_ascent,
            has_break: false,
            hanging_punct_width: line_hanging_punct_width,
        });
    }

    lines
}

/// Find the last substantive visible fragment in a candidate line. Trimmable
/// trailing whitespace and zero-width bookmarks do not displace punctuation;
/// every other fragment does. The parallel table contains at most one scalar's
/// marginal width per text fragment.
fn trailing_hanging_tail_width(
    fragments: &[Fragment],
    hanging_tail_widths: &[Pt],
    start: usize,
    end: usize,
) -> Pt {
    for idx in (start..end).rev() {
        match &fragments[idx] {
            Fragment::Bookmark { .. } => continue,
            Fragment::Text { text, .. }
                if text
                    .chars()
                    .all(crate::render::layout::fragment::is_trimmable_trailing_whitespace) =>
            {
                continue;
            }
            Fragment::Text { .. } => {
                return hanging_tail_widths.get(idx).copied().unwrap_or(Pt::ZERO);
            }
            _ => return Pt::ZERO,
        }
    }
    Pt::ZERO
}

/// Measurements for a range of fragments.
struct RangeMeasure {
    width: Pt,
    height: Pt,
    text_height: Pt,
    auto_text_height: Pt,
    ascent: Pt,
}

#[derive(Clone, Copy)]
struct FragmentLineMetrics {
    text_height: Pt,
    auto_text_height: Pt,
    ascent: Pt,
}

fn fragment_line_metrics(fragment: &Fragment) -> FragmentLineMetrics {
    use crate::render::layout::fragment::AutoLineSpacingContribution;

    let scaled = |height: Pt, font: &crate::render::layout::fragment::FontProps| {
        if font.auto_line_spacing == AutoLineSpacingContribution::Scaled {
            height
        } else {
            Pt::ZERO
        }
    };
    match fragment {
        Fragment::Text { font, metrics, .. } => FragmentLineMetrics {
            text_height: metrics.line_height(),
            auto_text_height: scaled(metrics.line_height(), font),
            ascent: metrics.ascent,
        },
        // Inline graphics, like images, don't contribute to text height.
        Fragment::Image { .. } | Fragment::InlineGraphic { .. } => FragmentLineMetrics {
            text_height: Pt::ZERO,
            auto_text_height: Pt::ZERO,
            ascent: Pt::ZERO,
        },
        Fragment::Tab {
            line_height, font, ..
        }
        | Fragment::PTab {
            line_height, font, ..
        } => FragmentLineMetrics {
            text_height: *line_height,
            auto_text_height: scaled(*line_height, font),
            ascent: Pt::ZERO,
        },
        Fragment::LineBreak { text_height, .. } => FragmentLineMetrics {
            text_height: *text_height,
            auto_text_height: *text_height,
            ascent: Pt::ZERO,
        },
        other => {
            let height = other.height();
            FragmentLineMetrics {
                text_height: height,
                auto_text_height: height,
                ascent: Pt::ZERO,
            }
        }
    }
}

/// Measure total width, max height, text height, and ascent for a range of fragments.
fn measure_range(fragments: &[Fragment], start: usize, end: usize) -> RangeMeasure {
    let mut m = RangeMeasure {
        width: Pt::ZERO,
        height: Pt::ZERO,
        text_height: Pt::ZERO,
        auto_text_height: Pt::ZERO,
        ascent: Pt::ZERO,
    };
    for frag in &fragments[start..end] {
        m.width += frag.width();
        m.height = m.height.max(frag.height());
        let metrics = fragment_line_metrics(frag);
        m.text_height = m.text_height.max(metrics.text_height);
        m.auto_text_height = m.auto_text_height.max(metrics.auto_text_height);
        m.ascent = m.ascent.max(metrics.ascent);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::layout::fragment::{FontProps, TextMetrics};
    use crate::render::resolve::color::RgbColor;
    use std::rc::Rc;

    fn text_frag(text: &str, width: f32) -> Fragment {
        Fragment::Text {
            text: text.into(),
            font: Rc::new(FontProps {
                family: Rc::from("Test"),
                size: Pt::new(12.0),
                bold: false,
                italic: false,
                underline: false,
                char_spacing: Pt::ZERO,
                text_scale: 1.0,
                auto_line_spacing: Default::default(),
                east_asian_language: None,
                underline_position: Pt::ZERO,
                underline_thickness: Pt::ZERO,
            }),
            color: RgbColor::BLACK,
            width: Pt::new(width),
            trimmed_width: Pt::new(width),
            metrics: TextMetrics {
                ascent: Pt::new(10.0),
                descent: Pt::new(4.0),
                leading: Pt::ZERO,
            },
            hyperlink_url: None,
            shading: None,
            border: None,
            baseline_offset: Pt::ZERO,
            text_offset: Pt::ZERO,
            is_footnote_ref: false,
        }
    }

    fn natural_only_text_frag(text: &str, width: f32) -> Fragment {
        let mut fragment = text_frag(text, width);
        let Fragment::Text { font, metrics, .. } = &mut fragment else {
            unreachable!();
        };
        Rc::make_mut(font).auto_line_spacing =
            crate::render::layout::fragment::AutoLineSpacingContribution::NaturalOnly;
        *metrics = TextMetrics {
            ascent: Pt::new(11.0),
            descent: Pt::new(4.0),
            leading: Pt::new(1.0),
        };
        fragment
    }

    fn text_frag_with_trimmed_width(text: &str, width: f32, trimmed_width: f32) -> Fragment {
        let mut fragment = text_frag(text, width);
        let Fragment::Text {
            trimmed_width: measured_trimmed_width,
            ..
        } = &mut fragment
        else {
            unreachable!();
        };
        *measured_trimmed_width = Pt::new(trimmed_width);
        fragment
    }

    fn manual_break(natural_height: f32, text_height: f32) -> Fragment {
        Fragment::LineBreak {
            line_height: Pt::new(natural_height),
            text_height: Pt::new(text_height),
        }
    }

    #[test]
    fn empty_fragments_no_lines() {
        let lines = fit_lines(&[], Pt::new(100.0));
        assert!(lines.is_empty());
    }

    #[test]
    fn single_fragment_fits() {
        let frags = vec![text_frag("hello", 30.0)];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].start, 0);
        assert_eq!(lines[0].end, 1);
        assert_eq!(lines[0].width.raw(), 30.0);
        assert_eq!(lines[0].height.raw(), 14.0);
    }

    #[test]
    fn natural_only_label_keeps_natural_metrics_but_not_auto_multiplier_metrics() {
        let frags = vec![natural_only_text_frag("o", 10.0), text_frag("body", 30.0)];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].height, Pt::new(15.0));
        assert_eq!(lines[0].text_height, Pt::new(16.0));
        assert_eq!(lines[0].auto_text_height, Pt::new(14.0));
        assert_eq!(lines[0].ascent, Pt::new(11.0));
    }

    #[test]
    fn overflow_rollback_preserves_natural_only_label_and_tab_metrics() {
        let label = natural_only_text_frag("o", 10.0);
        let label_font = label.font_props().unwrap().clone();
        let frags = vec![
            label,
            Fragment::Tab {
                line_height: Pt::new(16.0),
                font: Rc::new(label_font),
                color: RgbColor::BLACK,
                fitting_width: None,
            },
            text_frag("body", 95.0),
        ];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].start, lines[0].end), (0, 2));
        assert_eq!(lines[0].height, Pt::new(16.0));
        assert_eq!(lines[0].text_height, Pt::new(16.0));
        assert_eq!(lines[0].auto_text_height, Pt::ZERO);
        assert_eq!(lines[1].auto_text_height, Pt::new(14.0));
    }

    #[test]
    fn two_fragments_fit_on_one_line() {
        let frags = vec![text_frag("hello ", 35.0), text_frag("world", 30.0)];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].end, 2);
        assert_eq!(lines[0].width.raw(), 65.0);
    }

    #[test]
    fn trailing_whitespace_across_runs_does_not_create_an_extra_line() {
        let frags = vec![
            text_frag("label", 80.0),
            text_frag_with_trimmed_width("   ", 15.0, 0.0),
            text_frag_with_trimmed_width("   ", 15.0, 0.0),
        ];
        let lines = fit_lines(&frags, Pt::new(90.0));

        assert_eq!(lines.len(), 1);
        assert_eq!((lines[0].start, lines[0].end), (0, 3));
        assert_eq!(
            lines[0].width.raw(),
            110.0,
            "the spaces remain drawable even though they hang past the margin"
        );
    }

    #[test]
    fn non_breaking_spaces_are_not_removed_from_overflow_width() {
        for space in ["\u{00A0}", "\u{202F}"] {
            let frags = vec![
                text_frag("label", 80.0),
                text_frag_with_trimmed_width(space, 15.0, 15.0),
            ];
            let lines = fit_lines(&frags, Pt::new(90.0));

            assert_eq!(lines.len(), 2, "{space:?} must occupy layout width");
            assert_eq!((lines[0].start, lines[0].end), (0, 1));
            assert_eq!((lines[1].start, lines[1].end), (1, 2));
        }
    }

    #[test]
    fn margin_ptab_ignores_the_complete_trailing_space_suffix() {
        let ptab = Fragment::PTab {
            align: crate::model::PTabAlignment::Left,
            relative_to: crate::model::PTabRelativeTo::Margin,
            leader: crate::model::TabLeader::None,
            line_height: Pt::new(14.0),
            font: Rc::new(FontProps {
                family: Rc::from("Test"),
                size: Pt::new(12.0),
                bold: false,
                italic: false,
                underline: false,
                char_spacing: Pt::ZERO,
                text_scale: 1.0,
                auto_line_spacing: Default::default(),
                east_asian_language: None,
                underline_position: Pt::ZERO,
                underline_thickness: Pt::ZERO,
            }),
            color: RgbColor::BLACK,
        };
        let frags = vec![
            ptab,
            text_frag("label", 80.0),
            text_frag_with_trimmed_width("   ", 15.0, 0.0),
            text_frag_with_trimmed_width("   ", 15.0, 0.0),
            text_frag_with_trimmed_width("   ", 15.0, 0.0),
        ];
        let lines = fit_lines_with_first(
            &frags,
            Pt::new(80.0),
            Pt::new(80.0),
            crate::render::layout::paragraph::PTabGeometry {
                max_width: Pt::new(100.0),
                indent_left: Pt::ZERO,
                indent_first_line: Pt::ZERO,
                content_width: Pt::new(80.0),
                float_left: Pt::ZERO,
                float_right: Pt::ZERO,
            },
        );

        assert_eq!(lines.len(), 1);
        assert_eq!((lines[0].start, lines[0].end), (0, 5));
        assert_eq!(
            lines[0].width.raw(),
            126.0,
            "trailing spaces remain drawable after the margin-relative pTab"
        );
    }

    #[test]
    fn whitespace_between_text_runs_still_counts_towards_wrapping() {
        let frags = vec![
            text_frag("label", 70.0),
            text_frag_with_trimmed_width("   ", 20.0, 0.0),
            text_frag("value", 20.0),
        ];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].start, lines[0].end), (0, 2));
        assert_eq!((lines[1].start, lines[1].end), (2, 3));
    }

    #[test]
    fn overflow_breaks_at_boundary() {
        let frags = vec![
            text_frag("hello ", 60.0),
            text_frag("world ", 60.0),
            text_frag("end", 30.0),
        ];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].start, 0);
        assert_eq!(lines[0].end, 1); // "hello " on first line
        assert_eq!(lines[1].start, 1);
        assert_eq!(lines[1].end, 3); // "world " + "end" on second line
        assert_eq!(lines[1].width.raw(), 90.0);
    }

    #[test]
    fn refitting_after_an_earlier_break_counts_all_intervening_fragments() {
        let frags = vec![
            text_frag("prefix ", 60.0),
            text_frag("A", 20.0),
            text_frag("B", 20.0),
            text_frag("C", 20.0),
        ];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].start, lines[0].end), (0, 1));
        assert_eq!((lines[1].start, lines[1].end), (1, 4));
        assert_eq!(
            lines[1].width.raw(),
            60.0,
            "the new line must re-account fragments between the old break and overflow"
        );
    }

    #[test]
    fn cjk_fragments_break_between_characters_without_whitespace() {
        let frags = vec![
            text_frag("1. ", 20.0),
            text_frag("北", 20.0),
            text_frag("京", 20.0),
            text_frag("收", 20.0),
            text_frag("费", 20.0),
        ];
        let lines = fit_lines(&frags, Pt::new(70.0));

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].end, 3, "the first line keeps 1. 北京");
        assert_eq!(lines[1].start, 3);
    }

    #[test]
    fn non_breaking_hyphen_is_not_a_break_point() {
        let frags = vec![
            text_frag("prefix ", 45.0),
            text_frag("ID‑", 20.0),
            text_frag("001", 35.0),
        ];
        let lines = fit_lines(&frags, Pt::new(70.0));

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].end, 1);
        assert_eq!(lines[1].start, 1);
        assert_eq!(lines[1].end, 3);
    }

    #[test]
    fn oversized_fragment_gets_own_line() {
        let frags = vec![text_frag("verylongword", 200.0)];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 1, "oversized fragment still produces a line");
        assert_eq!(lines[0].end, 1);
    }

    #[test]
    fn line_break_forces_new_line() {
        let frags = vec![
            text_frag("before", 30.0),
            Fragment::LineBreak {
                line_height: Pt::new(14.0),
                text_height: Pt::new(14.0),
            },
            text_frag("after", 25.0),
        ];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].end, 2); // "before" + line break
        assert!(lines[0].has_break);
        assert_eq!(lines[1].start, 2);
        assert_eq!(lines[1].end, 3); // "after"
    }

    #[test]
    fn leading_and_consecutive_breaks_use_their_measured_text_line_boxes() {
        let frags = vec![
            manual_break(11.0, 14.5),
            manual_break(13.0, 16.25),
            text_frag("after", 25.0),
        ];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].height, Pt::new(11.0));
        assert_eq!(lines[0].text_height, Pt::new(14.5));
        assert_eq!(lines[1].height, Pt::new(13.0));
        assert_eq!(lines[1].text_height, Pt::new(16.25));
        assert_eq!(lines[2].text_height, Pt::new(14.0));
    }

    #[test]
    fn ordinary_single_break_does_not_inflate_same_font_text_line() {
        let frags = vec![
            text_frag("before", 30.0),
            manual_break(12.0, 14.0),
            text_frag("after", 25.0),
        ];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].height, Pt::new(14.0));
        assert_eq!(lines[0].text_height, Pt::new(14.0));
        assert_eq!(lines[1].height, Pt::new(14.0));
        assert_eq!(lines[1].text_height, Pt::new(14.0));
    }

    #[test]
    fn page_and_column_break_metrics_are_unchanged() {
        let frags = vec![
            Fragment::PageBreak {
                line_height: Pt::new(9.0),
            },
            Fragment::ColumnBreak,
        ];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].height, Pt::new(9.0));
        assert_eq!(lines[0].text_height, Pt::new(9.0));
        assert_eq!(lines[1].height, Pt::ZERO);
        assert_eq!(lines[1].text_height, Pt::ZERO);
    }

    #[test]
    fn exact_fit_no_overflow() {
        let frags = vec![text_frag("a", 50.0), text_frag("b", 50.0)];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].width.raw(), 100.0);
    }

    #[test]
    fn tab_uses_min_width_for_fitting() {
        let frags = vec![
            text_frag("text", 80.0),
            Fragment::Tab {
                line_height: Pt::new(18.0),
                font: Rc::new(FontProps {
                    family: Rc::from("Test"),
                    size: Pt::new(12.0),
                    bold: false,
                    italic: false,
                    underline: false,
                    char_spacing: Pt::ZERO,
                    text_scale: 1.0,
                    auto_line_spacing: Default::default(),
                    east_asian_language: None,
                    underline_position: Pt::ZERO,
                    underline_thickness: Pt::ZERO,
                }),
                color: RgbColor::BLACK,
                fitting_width: None,
            },
            text_frag("more", 30.0),
        ];
        // 80 + 12 (MIN_TAB_WIDTH) = 92, still fits 100
        // But + 30 = 122, doesn't fit
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].auto_text_height, Pt::new(18.0));
    }

    #[test]
    fn height_is_max_of_fragments() {
        let frags = vec![
            Fragment::Text {
                text: "small".into(),
                font: Rc::new(FontProps {
                    family: Rc::from("Test"),
                    size: Pt::new(10.0),
                    bold: false,
                    italic: false,
                    underline: false,
                    char_spacing: Pt::ZERO,
                    text_scale: 1.0,
                    auto_line_spacing: Default::default(),
                    east_asian_language: None,
                    underline_position: Pt::ZERO,
                    underline_thickness: Pt::ZERO,
                }),
                color: RgbColor::BLACK,
                width: Pt::new(20.0),
                trimmed_width: Pt::new(20.0),
                metrics: TextMetrics {
                    ascent: Pt::new(9.0),
                    descent: Pt::new(3.0),
                    leading: Pt::ZERO,
                },
                hyperlink_url: None,
                shading: None,
                border: None,
                baseline_offset: Pt::ZERO,
                text_offset: Pt::ZERO,
                is_footnote_ref: false,
            },
            Fragment::Text {
                text: "big".into(),
                font: Rc::new(FontProps {
                    family: Rc::from("Test"),
                    size: Pt::new(24.0),
                    bold: false,
                    italic: false,
                    underline: false,
                    char_spacing: Pt::ZERO,
                    text_scale: 1.0,
                    auto_line_spacing: Default::default(),
                    east_asian_language: None,
                    underline_position: Pt::ZERO,
                    underline_thickness: Pt::ZERO,
                }),
                color: RgbColor::BLACK,
                width: Pt::new(30.0),
                trimmed_width: Pt::new(30.0),
                metrics: TextMetrics {
                    ascent: Pt::new(22.0),
                    descent: Pt::new(6.0),
                    leading: Pt::ZERO,
                },
                hyperlink_url: None,
                shading: None,
                border: None,
                baseline_offset: Pt::ZERO,
                text_offset: Pt::ZERO,
                is_footnote_ref: false,
            },
        ];
        let lines = fit_lines(&frags, Pt::new(100.0));

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].height.raw(), 28.0, "max of 12 and 28");
        assert_eq!(lines[0].ascent.raw(), 22.0, "max of 9 and 22");
    }

    #[test]
    fn multiple_overflows_produce_multiple_lines() {
        let frags = vec![
            text_frag("a ", 40.0),
            text_frag("b ", 40.0),
            text_frag("c ", 40.0),
            text_frag("d ", 40.0),
            text_frag("e", 40.0),
        ];
        // max_width=70: "a " fits (40), +"b " = 80 > 70 → break
        let lines = fit_lines(&frags, Pt::new(70.0));

        assert!(lines.len() >= 3, "should produce at least 3 lines");
        // Each line should have at most 1 fragment since 40+40=80 > 70
    }

    #[test]
    fn first_line_narrower_than_remaining() {
        // first_line_width=60, remaining_width=100.
        // "a " (40pt) fits the narrow first line alone; "b " (40pt) + "c" (40pt)
        // = 80pt fit together on the full 100pt remaining line.
        let frags = vec![
            text_frag("a ", 40.0),
            text_frag("b ", 40.0),
            text_frag("c", 40.0),
        ];
        let lines = fit_lines_with_first(
            &frags,
            Pt::new(60.0),
            Pt::new(100.0),
            crate::render::layout::paragraph::PTabGeometry {
                max_width: Pt::new(100.0),
                indent_left: Pt::ZERO,
                indent_first_line: Pt::ZERO,
                content_width: Pt::new(100.0),
                float_left: Pt::ZERO,
                float_right: Pt::ZERO,
            },
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].end, 1, "only 'a ' fits the narrow first line");
        assert_eq!(lines[1].start, 1);
        assert_eq!(
            lines[1].end, 3,
            "'b ' + 'c' both fit on the full second line"
        );
    }

    fn fit_hanging(fragments: &[Fragment], tails: &[Pt], max_width: f32) -> Vec<FittedLine> {
        let max_width = Pt::new(max_width);
        fit_lines_with_first_and_hanging(
            fragments,
            max_width,
            max_width,
            crate::render::layout::paragraph::PTabGeometry {
                max_width,
                indent_left: Pt::ZERO,
                indent_first_line: Pt::ZERO,
                content_width: max_width,
                float_left: Pt::ZERO,
                float_right: Pt::ZERO,
            },
            tails,
            true,
        )
    }

    #[test]
    fn one_final_punctuation_scalar_may_extend_past_extent() {
        let fragments = vec![text_frag("中", 100.0), text_frag("。", 10.0)];
        let lines = fit_hanging(&fragments, &[Pt::ZERO, Pt::new(10.0)], 100.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].hanging_punct_width, Pt::new(10.0));

        let disabled = fit_lines(&fragments, Pt::new(100.0));
        assert_eq!(disabled.len(), 2, "without the exception punctuation wraps");
    }

    #[test]
    fn only_the_last_of_two_punctuation_scalars_can_hang() {
        let fragments = vec![
            text_frag("中", 100.0),
            text_frag("！", 10.0),
            text_frag("！", 10.0),
        ];
        let lines = fit_hanging(&fragments, &[Pt::ZERO, Pt::new(10.0), Pt::new(10.0)], 100.0);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].end, 2);
        assert_eq!(lines[0].hanging_punct_width, Pt::new(10.0));
        assert_eq!(lines[1].hanging_punct_width, Pt::ZERO);
    }

    #[test]
    fn trailing_spaces_and_bookmark_preserve_but_image_clears_candidate() {
        let whitespace_only = vec![
            text_frag("中", 100.0),
            text_frag_with_trimmed_width("   ", 15.0, 0.0),
            Fragment::Bookmark { name: "end".into() },
        ];
        assert_eq!(
            fit_lines(&whitespace_only, Pt::new(100.0)).len(),
            1,
            "a bookmark must not reintroduce the ignored trailing-space suffix"
        );

        let spaces = vec![
            text_frag("中", 100.0),
            text_frag("。", 10.0),
            text_frag_with_trimmed_width("   ", 15.0, 0.0),
            Fragment::Bookmark { name: "end".into() },
        ];
        let lines = fit_hanging(
            &spaces,
            &[Pt::ZERO, Pt::new(10.0), Pt::ZERO, Pt::ZERO],
            100.0,
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].hanging_punct_width, Pt::new(10.0));

        let image = vec![
            text_frag("中。", 90.0),
            Fragment::Image {
                size: crate::render::geometry::PtSize::new(Pt::new(10.0), Pt::new(10.0)),
                rel_id: "rId1".into(),
                image_data: None,
                src_rect: None,
            },
        ];
        let lines = fit_hanging(&image, &[Pt::new(10.0), Pt::ZERO], 100.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].hanging_punct_width, Pt::ZERO);
    }

    #[test]
    fn hard_break_resets_hanging_candidate_for_next_line() {
        let fragments = vec![
            text_frag("中", 100.0),
            text_frag("。", 10.0),
            Fragment::LineBreak {
                line_height: Pt::new(14.0),
                text_height: Pt::new(14.0),
            },
            text_frag("下一行", 80.0),
        ];
        let lines = fit_hanging(
            &fragments,
            &[Pt::ZERO, Pt::new(10.0), Pt::ZERO, Pt::ZERO],
            100.0,
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].hanging_punct_width, Pt::new(10.0));
        assert_eq!(lines[1].hanging_punct_width, Pt::ZERO);
    }

    #[test]
    fn rollback_to_earlier_break_keeps_that_ranges_consumed_tail() {
        let fragments = vec![
            text_frag("A ", 50.0),
            text_frag("中", 50.0),
            text_frag("。", 10.0),
            text_frag("B", 20.0),
        ];
        let lines = fit_hanging(
            &fragments,
            &[Pt::ZERO, Pt::ZERO, Pt::new(10.0), Pt::ZERO],
            100.0,
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].end, 3);
        assert_eq!(lines[0].hanging_punct_width, Pt::new(10.0));
    }

    #[test]
    fn margin_relative_ptab_uses_the_same_hanging_exception() {
        let font = match text_frag("x", 1.0) {
            Fragment::Text { font, .. } => font,
            _ => unreachable!(),
        };
        let fragments = vec![
            Fragment::PTab {
                align: crate::model::PTabAlignment::Left,
                relative_to: crate::model::PTabRelativeTo::Margin,
                leader: crate::model::TabLeader::None,
                line_height: Pt::new(14.0),
                font,
                color: RgbColor::BLACK,
            },
            text_frag("中", 100.0),
            text_frag("。", 10.0),
        ];
        let lines = fit_hanging(&fragments, &[Pt::ZERO, Pt::ZERO, Pt::new(10.0)], 100.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].hanging_punct_width, Pt::new(10.0));
    }
}
