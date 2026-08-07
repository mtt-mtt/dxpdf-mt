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
    /// Maximum ascent of any text fragment in this line.
    pub ascent: Pt,
    /// Whether this line ends with an explicit line break.
    pub has_break: bool,
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
    let mut line_ascent = Pt::ZERO;
    let mut last_break_point = None; // index after which we can break

    let mut i = 0;
    while i < fragments.len() {
        let frag = &fragments[i];

        // Explicit line break — emit current line including the break fragment.
        // LineBreak height already includes leading (from default_line_height).
        if frag.is_line_break() {
            line_height = line_height.max(frag.height());
            line_text_height = line_text_height.max(frag.height());
            lines.push(FittedLine {
                start: line_start,
                end: i + 1,
                width: line_width,
                height: line_height,
                text_height: line_text_height,
                ascent: line_ascent,
                has_break: true,
            });
            line_start = i + 1;
            line_width = Pt::ZERO;
            line_trimmed_width = Pt::ZERO;
            pen_x = line_pen_start(lines.is_empty());
            pen_trimmed_x = pen_x;
            margin_span_active = false;
            line_height = Pt::ZERO;
            line_text_height = Pt::ZERO;
            line_ascent = Pt::ZERO;
            last_break_point = None;
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
                            ascent: m.ascent,
                            has_break: false,
                        });
                        line_start = i;
                        line_width = Pt::ZERO;
                        line_trimmed_width = Pt::ZERO;
                        pen_x = line_pen_start(lines.is_empty());
                        pen_trimmed_x = pen_x;
                        margin_span_active = false;
                        line_height = Pt::ZERO;
                        line_text_height = Pt::ZERO;
                        line_ascent = Pt::ZERO;
                        last_break_point = None;
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
        let overflows = if margin_span_active {
            new_trimmed_pen_x > ptab_geometry.max_width
        } else {
            check_width > current_max
        };
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
                ascent: m.ascent,
                has_break: false,
            });
            line_start = break_at;
            line_width = Pt::ZERO;
            line_trimmed_width = Pt::ZERO;
            pen_x = line_pen_start(lines.is_empty());
            pen_trimmed_x = pen_x;
            margin_span_active = false;
            line_height = Pt::ZERO;
            line_text_height = Pt::ZERO;
            line_ascent = Pt::ZERO;
            last_break_point = None;
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
        // §17.3.1.33: text_height is the Auto line spacing base — use
        // line_height() (includes leading) for text, glyph height for tabs.
        match frag {
            Fragment::Text { metrics, .. } => {
                line_text_height = line_text_height.max(metrics.line_height());
                line_ascent = line_ascent.max(metrics.ascent);
            }
            Fragment::Image { .. } | Fragment::InlineGraphic { .. } => {}
            // Inline graphics, like images, don't contribute to text_height.
            _ => {
                line_text_height = line_text_height.max(frag.height());
            }
        }

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
            ascent: line_ascent,
            has_break: false,
        });
    }

    lines
}

/// Measurements for a range of fragments.
struct RangeMeasure {
    width: Pt,
    height: Pt,
    text_height: Pt,
    ascent: Pt,
}

/// Measure total width, max height, text height, and ascent for a range of fragments.
fn measure_range(fragments: &[Fragment], start: usize, end: usize) -> RangeMeasure {
    let mut m = RangeMeasure {
        width: Pt::ZERO,
        height: Pt::ZERO,
        text_height: Pt::ZERO,
        ascent: Pt::ZERO,
    };
    for frag in &fragments[start..end] {
        m.width += frag.width();
        m.height = m.height.max(frag.height());
        match frag {
            Fragment::Text { metrics, .. } => {
                m.text_height = m.text_height.max(metrics.line_height());
                m.ascent = m.ascent.max(metrics.ascent);
            }
            Fragment::Image { .. } => {}
            _ => {
                m.text_height = m.text_height.max(frag.height());
            }
        }
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
                line_height: Pt::new(14.0),
                font: Rc::new(FontProps {
                    family: Rc::from("Test"),
                    size: Pt::new(12.0),
                    bold: false,
                    italic: false,
                    underline: false,
                    char_spacing: Pt::ZERO,
                    text_scale: 1.0,
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
}
