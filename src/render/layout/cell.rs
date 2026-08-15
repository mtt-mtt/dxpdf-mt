//! Cell layout — narrows constraints by cell margins, lays out child blocks.
//!
//! Uses the shared `stack_blocks` function from `section.rs` so that table
//! cells get the same features as body content (floating images, spacing
//! collapse, contextual spacing, etc.).

use crate::render::dimension::Pt;
use crate::render::geometry::PtEdgeInsets;

use super::section::{
    stack_blocks, stack_cell_blocks, CellLine, LayoutBlock, LayoutFootnote, PageParity,
};
use crate::model::TextDirection;

/// A footnote reference positioned in cell-box coordinates.
#[derive(Clone, Debug)]
pub struct CellFootnote {
    /// Top of the line containing the reference, including the cell's top margin.
    pub top_y: Pt,
    /// The complete note body, grouped by its single reference.
    pub footnote: LayoutFootnote,
    /// Height of the note body at page width. Filled by the paginated table path.
    pub page_height: Pt,
}

/// Result of laying out a cell.
#[derive(Debug)]
pub struct CellLayout {
    /// Draw commands relative to the cell's top-left origin.
    pub commands: Vec<super::draw_command::DrawCommand>,
    /// Content height (without margins).
    pub content_height: Pt,
    /// §17.4.1 row-split cut model, in cell-content coordinates (i.e. *not*
    /// shifted by the cell margins — the splitter adds `margin_top`). Empty for
    /// a cell that can't be safely bisected; see [`CellLine`].
    pub lines: Vec<CellLine>,
    /// Footnotes referenced by this cell, in document order.
    pub footnotes: Vec<CellFootnote>,
}

/// Lay out blocks inside a table cell.
///
/// Receives the full cell width (from column sizing), deflates by margins,
/// lays out each block sequentially using `stack_blocks`, returns total
/// content height.
pub fn layout_cell(
    blocks: &[LayoutBlock],
    cell_width: Pt,
    margins: &PtEdgeInsets,
    default_line_height: Pt,
    measure_text: super::paragraph::MeasureTextFn<'_>,
) -> CellLayout {
    layout_cell_impl(
        blocks,
        cell_width,
        margins,
        default_line_height,
        measure_text,
        None,
    )
}

/// Lay out an ordinary (unrotated) cell while preserving its physical border
/// origin for `layoutInCell` drawings. `extra_left` is the portion of the
/// resolved left border wider than the cell's left margin; table emission adds
/// it back after this cell-local layout.
pub(crate) fn layout_cell_with_left_border_inset(
    blocks: &[LayoutBlock],
    cell_width: Pt,
    margins: &PtEdgeInsets,
    extra_left: Pt,
    default_line_height: Pt,
    measure_text: super::paragraph::MeasureTextFn<'_>,
) -> CellLayout {
    layout_cell_impl(
        blocks,
        cell_width,
        margins,
        default_line_height,
        measure_text,
        Some(margins.left + extra_left),
    )
}

fn layout_cell_impl(
    blocks: &[LayoutBlock],
    cell_width: Pt,
    margins: &PtEdgeInsets,
    default_line_height: Pt,
    measure_text: super::paragraph::MeasureTextFn<'_>,
    cell_content_left_inset: Option<Pt>,
) -> CellLayout {
    let content_width = (cell_width - margins.horizontal()).max(Pt::ZERO);

    // §20.4.3.1: a cell is measured before its table is paginated — a row can
    // split across pages, and the split is decided from these measurements —
    // so the page this cell lands on, and its parity, do not exist yet. An
    // `inside`/`outside` float inside a table therefore takes the odd-page
    // reading. That is the Tier-0 the page and header paths no longer need;
    // removing it here means making cell measurement page-aware, which is a
    // far larger change than the anchor resolution itself.
    let result = match cell_content_left_inset {
        Some(inset) => stack_cell_blocks(
            blocks,
            content_width,
            default_line_height,
            measure_text,
            PageParity::Odd,
            inset,
        ),
        None => stack_blocks(
            blocks,
            content_width,
            default_line_height,
            measure_text,
            PageParity::Odd,
        ),
    };

    // Shift all commands by cell margins.
    let commands = result
        .commands
        .into_iter()
        .map(|mut cmd| {
            cmd.shift(margins.left, margins.top);
            cmd
        })
        .collect();

    CellLayout {
        commands,
        content_height: result.height,
        // Cut points stay in content coordinates; the shift above applies to
        // draw commands only. `split.rs` accounts for `margin_top` when
        // partitioning against them.
        lines: result.lines,
        footnotes: result
            .footnotes
            .into_iter()
            .map(|mut footnote| {
                footnote.top_y += margins.top;
                footnote
            })
            .collect(),
    }
}

/// Lay out a cell whose horizontal WordprocessingML flow is rotated into the
/// physical table cell. `physical_extent` is the row's declared height; Word
/// uses that axis as the line width for `btLr`/`tbRl` text.
pub fn layout_rotated_cell(
    blocks: &[LayoutBlock],
    physical_extent: Pt,
    physical_margins: &PtEdgeInsets,
    direction: TextDirection,
    default_line_height: Pt,
    measure_text: super::paragraph::MeasureTextFn<'_>,
) -> CellLayout {
    let Some((logical_margins, clockwise)) = rotated_logical_margins(physical_margins, direction)
    else {
        return layout_cell(
            blocks,
            physical_extent,
            physical_margins,
            default_line_height,
            measure_text,
        );
    };

    let mut result = layout_cell(
        blocks,
        physical_extent,
        &logical_margins,
        default_line_height,
        measure_text,
    );
    let logical_height = result.content_height + logical_margins.vertical();

    for command in &mut result.commands {
        rotate_cell_command(command, physical_extent, logical_height, clockwise);
    }

    // Rotated cell lines are atomic for now: the ordinary table splitter uses
    // horizontal line Y coordinates, which no longer describe legal cuts.
    result.lines.clear();
    result.content_height = (physical_extent - physical_margins.vertical()).max(Pt::ZERO);
    result
}

/// Estimate the physical row extent needed by an auto-height rotated cell.
/// Returns `None` for content whose intrinsic horizontal size is not safely
/// derivable without a full two-dimensional layout (notably floating objects).
pub fn intrinsic_rotated_cell_extent(
    blocks: &[LayoutBlock],
    physical_margins: &PtEdgeInsets,
    direction: TextDirection,
    default_line_height: Pt,
) -> Option<Pt> {
    let (logical_margins, _) = rotated_logical_margins(physical_margins, direction)?;
    let mut widest = Pt::ZERO;

    for block in blocks {
        match block {
            LayoutBlock::Paragraph {
                fragments,
                style,
                floating_images,
                floating_shapes,
                ..
            } => {
                if !floating_images.is_empty() || !floating_shapes.is_empty() {
                    return None;
                }
                let mut line_width = Pt::ZERO;
                let mut paragraph_width = Pt::ZERO;
                for fragment in fragments {
                    if matches!(
                        fragment,
                        super::fragment::Fragment::LineBreak { .. }
                            | super::fragment::Fragment::ColumnBreak
                            | super::fragment::Fragment::PageBreak { .. }
                    ) {
                        paragraph_width = paragraph_width.max(line_width);
                        line_width = Pt::ZERO;
                    } else {
                        line_width += fragment.width();
                    }
                }
                paragraph_width = paragraph_width.max(line_width);
                paragraph_width +=
                    style.indent_left + style.indent_right + style.indent_first_line.max(Pt::ZERO);
                widest = widest.max(paragraph_width);
            }
            LayoutBlock::Table { col_widths, .. } => {
                widest = widest.max(col_widths.iter().copied().sum());
            }
        }
    }

    Some(
        (widest + logical_margins.horizontal())
            .max(default_line_height + logical_margins.horizontal()),
    )
}

fn rotated_logical_margins(
    physical: &PtEdgeInsets,
    direction: TextDirection,
) -> Option<(PtEdgeInsets, f32)> {
    match direction {
        TextDirection::BottomToTopLeftToRight => Some((
            PtEdgeInsets::new(physical.left, physical.top, physical.right, physical.bottom),
            -90.0,
        )),
        TextDirection::TopToBottomRightToLeft => Some((
            PtEdgeInsets::new(physical.right, physical.bottom, physical.left, physical.top),
            90.0,
        )),
        _ => None,
    }
}

fn rotate_cell_command(
    command: &mut super::draw_command::DrawCommand,
    logical_width: Pt,
    logical_height: Pt,
    clockwise: f32,
) {
    use super::draw_command::DrawCommand;

    let rotate_point = |point: &mut crate::render::geometry::PtOffset| {
        let (x, y) = (point.x, point.y);
        if clockwise < 0.0 {
            point.x = y;
            point.y = logical_width - x;
        } else {
            point.x = logical_height - y;
            point.y = x;
        }
    };
    let rotate_rect = |rect: &mut crate::render::geometry::PtRect| {
        let (x, y, width, height) = (
            rect.origin.x,
            rect.origin.y,
            rect.size.width,
            rect.size.height,
        );
        if clockwise < 0.0 {
            rect.origin.x = y;
            rect.origin.y = logical_width - x - width;
        } else {
            rect.origin.x = logical_height - y - height;
            rect.origin.y = x;
        }
        rect.size.width = height;
        rect.size.height = width;
    };

    match command {
        DrawCommand::Text {
            position,
            rotation_degrees,
            ..
        } => {
            rotate_point(position);
            *rotation_degrees += clockwise;
        }
        DrawCommand::Underline { line, .. } | DrawCommand::Line { line, .. } => {
            rotate_point(&mut line.start);
            rotate_point(&mut line.end);
        }
        DrawCommand::Rect { rect, .. }
        | DrawCommand::LinkAnnotation { rect, .. }
        | DrawCommand::InternalLink { rect, .. } => rotate_rect(rect),
        DrawCommand::NamedDestination { position, .. } => rotate_point(position),
        DrawCommand::Outline(_) => {}
        // The first supported corpus cases contain text and underline only.
        // Other drawing kinds remain in their logical position until the
        // command model grows a general group transform.
        _ => {
            log::warn!("§17.4.70: non-text content inside a rotated table cell is not rotated yet")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::layout::draw_command::DrawCommand;
    use crate::render::layout::fragment::{FontProps, Fragment, TextMetrics};
    use crate::render::layout::paragraph::ParagraphStyle;
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

    fn simple_block(text: &str, width: f32) -> LayoutBlock {
        LayoutBlock::Paragraph {
            fragments: vec![text_frag(text, width)],
            style: ParagraphStyle::default(),
            page_break_before: false,
            footnotes: vec![],
            floating_images: vec![],
            floating_shapes: vec![],
        }
    }

    #[test]
    fn empty_cell_zero_height() {
        let result = layout_cell(
            &[],
            Pt::new(200.0),
            &PtEdgeInsets::ZERO,
            Pt::new(14.0),
            None,
        );
        assert_eq!(result.content_height.raw(), 0.0);
        assert!(result.commands.is_empty());
    }

    #[test]
    fn single_paragraph_in_cell() {
        let blocks = vec![simple_block("hello", 30.0)];
        let result = layout_cell(
            &blocks,
            Pt::new(200.0),
            &PtEdgeInsets::ZERO,
            Pt::new(14.0),
            None,
        );
        assert_eq!(result.content_height.raw(), 14.0);
        assert!(!result.commands.is_empty());
    }

    #[test]
    fn margins_offset_content() {
        let blocks = vec![simple_block("text", 30.0)];
        let margins = PtEdgeInsets::new(
            Pt::new(5.0),  // top
            Pt::new(10.0), // right
            Pt::new(5.0),  // bottom
            Pt::new(10.0), // left
        );
        let result = layout_cell(&blocks, Pt::new(200.0), &margins, Pt::new(14.0), None);

        // Text should be shifted right by left margin
        if let Some(DrawCommand::Text { position, .. }) = result.commands.first() {
            assert_eq!(position.x.raw(), 10.0, "left margin applied");
            assert!(position.y.raw() >= 5.0, "top margin applied");
        } else {
            panic!("expected Text command");
        }
    }

    #[test]
    fn margins_narrow_available_width() {
        // Cell is 100 wide, margins eat 60 (left=30, right=30), leaving 40 for content
        // Two fragments of 30 each = 60 > 40, so they should wrap
        let blocks = vec![LayoutBlock::Paragraph {
            fragments: vec![text_frag("aa ", 30.0), text_frag("bb", 30.0)],
            style: ParagraphStyle::default(),
            page_break_before: false,
            footnotes: vec![],
            floating_images: vec![],
            floating_shapes: vec![],
        }];
        let margins = PtEdgeInsets::new(Pt::ZERO, Pt::new(30.0), Pt::ZERO, Pt::new(30.0));
        let result = layout_cell(&blocks, Pt::new(100.0), &margins, Pt::new(14.0), None);

        // Should wrap to 2 lines → height = 28
        assert_eq!(result.content_height.raw(), 28.0);
    }

    #[test]
    fn cell_border_anchor_cancels_padding_and_excess_left_border_inset() {
        use crate::model::ImageFormat;
        use crate::render::geometry::PtSize;
        use crate::render::layout::section::{
            FloatingImage, FloatingImageX, FloatingImageY, WrapMode,
        };
        use crate::render::resolve::images::MediaEntry;

        for margin_left in [0.0, 5.4, 12.0] {
            let image = FloatingImage {
                image_data: MediaEntry {
                    data: std::sync::Arc::from(&b""[..]),
                    format: ImageFormat::Png,
                },
                size: PtSize::new(Pt::new(10.0), Pt::new(10.0)),
                src_rect: None,
                x: FloatingImageX::CellBorderOffset(Pt::new(40.0)),
                y: FloatingImageY::RelativeToParagraph(Pt::ZERO),
                wrap_mode: WrapMode::None,
                dist_top: Pt::ZERO,
                dist_bottom: Pt::ZERO,
                dist_left: Pt::ZERO,
                dist_right: Pt::ZERO,
                behind_doc: false,
                relative_height: 0,
            };
            let block = LayoutBlock::Paragraph {
                fragments: vec![],
                style: ParagraphStyle::default(),
                page_break_before: false,
                footnotes: vec![],
                floating_images: vec![image],
                floating_shapes: vec![],
            };
            let margins = PtEdgeInsets::new(Pt::ZERO, Pt::ZERO, Pt::ZERO, Pt::new(margin_left));
            let extra_left = Pt::new(7.0);
            let result = layout_cell_with_left_border_inset(
                &[block],
                Pt::new(100.0),
                &margins,
                extra_left,
                Pt::new(14.0),
                None,
            );
            let Some(DrawCommand::Image { rect, .. }) = result.commands.first() else {
                panic!("expected the anchored image")
            };
            assert_eq!(
                rect.origin.x + extra_left,
                Pt::new(40.0),
                "margin={margin_left}: table emission adds extra_left back"
            );
        }
    }

    #[test]
    fn two_paragraphs_stack_vertically() {
        let blocks = vec![simple_block("first", 30.0), simple_block("second", 40.0)];
        let result = layout_cell(
            &blocks,
            Pt::new(200.0),
            &PtEdgeInsets::ZERO,
            Pt::new(14.0),
            None,
        );
        assert_eq!(result.content_height.raw(), 28.0, "14 + 14");

        let text_cmds: Vec<_> = result
            .commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::Text { position, text, .. } => Some((text.clone(), position.y)),
                _ => None,
            })
            .collect();
        assert_eq!(text_cmds.len(), 2);
        assert!(
            text_cmds[1].1 > text_cmds[0].1,
            "second paragraph should be below first"
        );
    }

    #[test]
    fn bottom_to_top_cell_uses_declared_row_height_as_line_width() {
        let blocks = vec![simple_block("vertical heading", 50.0)];
        let result = layout_rotated_cell(
            &blocks,
            Pt::new(92.0),
            &PtEdgeInsets::ZERO,
            TextDirection::BottomToTopLeftToRight,
            Pt::new(14.0),
            None,
        );

        assert_eq!(result.content_height.raw(), 92.0);
        assert!(result.lines.is_empty(), "rotated rows are atomic");
        let DrawCommand::Text {
            position,
            rotation_degrees,
            ..
        } = &result.commands[0]
        else {
            panic!("expected rotated text");
        };
        assert_eq!(*rotation_degrees, -90.0);
        assert!(position.y.raw() > 0.0 && position.y.raw() <= 92.0);
    }

    #[test]
    fn auto_height_rotated_cell_uses_unwrapped_fragment_width() {
        let blocks = vec![simple_block("Context", 42.0)];
        let extent = intrinsic_rotated_cell_extent(
            &blocks,
            &PtEdgeInsets::ZERO,
            TextDirection::TopToBottomRightToLeft,
            Pt::new(14.0),
        )
        .expect("simple text has an intrinsic extent");

        assert_eq!(extent.raw(), 42.0);
    }
}
