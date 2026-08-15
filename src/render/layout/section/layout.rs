//! Core section layout — the `layout_section()` function and its private state types.

use super::super::draw_command::{DrawCommand, LayeredDrawCommand, LayoutedPage};
use super::super::float;
use super::super::fragment::Fragment;
use super::super::header_footer::{HeaderFooterClearance, PageBodyBounds};
use super::super::page::PageConfig;
use super::super::paragraph::{
    layout_paragraph, place_paragraph, ListSpacingContext, ParagraphBorderStyle, ParagraphStyle,
    PlacedParagraph,
};
use super::super::table::{
    layout_table, layout_table_paginated_with_page_heights, measure_complete_table_height,
    measure_leading_table_group_height, TablePaginationHeights, TableRowInput, TableSlice,
};
use super::super::BoxConstraints;
use super::floating_table::{
    plan_floating_table_pages_with_page_tops, resolve_floating_anchor, FloatingTableAnchor,
    FloatingTablePagePlacement,
};
use super::helpers::{
    render_page_footnotes, split_at_column_breaks, split_at_page_breaks, table_x_offset,
};
use super::types::{
    ContinuationState, FloatingImage, FloatingImageY, FloatingShape, LayoutBlock, LayoutFootnote,
    PageParity, WrapMode,
};
use super::FLOAT_DEDUP_EPSILON_PT;
use super::FOOTNOTE_SEPARATOR_GAP;
use crate::model::StyleId;
use crate::render::dimension::Pt;
use crate::render::geometry::PtRect;

// ── Layout context and mutable page state ────────────────────────────────────

/// Read-only context passed to every section-layout helper.
/// Bundles the parameters that are constant for the lifetime of a `layout_section` call.
struct LayoutCtx<'cx> {
    config: &'cx PageConfig,
    clearance: &'cx HeaderFooterClearance,
    measure_text: super::super::paragraph::MeasureTextFn<'cx>,
    separator_indent: Pt,
    default_line_height: Pt,
}

impl LayoutCtx<'_> {
    fn page_bounds(&self, section_page_index: usize) -> PageBodyBounds {
        self.clearance.for_page(section_page_index)
    }
}

/// All mutable paging state threaded through `layout_section`.
/// Extracted from the function to make ownership and page-break resets explicit.
struct PageLayoutState<'doc> {
    /// Fully laid-out pages emitted so far.
    pages: Vec<LayoutedPage>,
    /// The page currently being assembled.
    current_page: LayoutedPage,
    /// Current vertical cursor position on the page.
    cursor_y: Pt,
    /// 0-based physical page index within the current section.
    page_index: usize,
    /// §17.10.6: logical number of this section's first page, with
    /// `w:pgNumType/@start` applied. With `page_index` it gives the current
    /// page's logical number, and so the §20.4.3.1 parity an `inside`/
    /// `outside` float mirrors on.
    logical_page_base: usize,
    /// Effective top boundary for the selected header slot on this page.
    page_top: Pt,
    /// §17.6.4: current column index (0-based).
    current_col: usize,
    /// §17.6.4: y at which columns start on the current page.
    column_top: Pt,
    /// Effective bottom boundary — reduced as footnotes are reserved.
    bottom: Pt,
    /// Footnotes accumulated for the current page.
    page_footnotes: Vec<PageFootnote<'doc>>,
    /// §17.3.1.33: true until the structural first content block is placed.
    first_on_section_page: bool,
    /// §17.3.1.9: space_after of the previous paragraph for spacing collapse.
    prev_space_after: Pt,
    /// §17.3.1.9: style_id of the previous paragraph for contextual spacing.
    prev_style_id: Option<StyleId>,
    /// §17.3.1.33: whether the previous paragraph's after spacing was automatic.
    prev_after_auto_spacing: bool,
    /// Effective numbering identity of the previous paragraph, for Word's
    /// contextual automatic spacing between peer list items.
    prev_list_spacing_context: Option<ListSpacingContext>,
    /// §17.3.1.24: borders of the previous paragraph for border grouping.
    prev_borders: Option<ParagraphBorderStyle>,
    /// Active floats on the current page (text wraps around these).
    page_floats: Vec<float::ActiveFloat>,
    /// Forward-scanned absolute floats from future paragraphs on this page.
    current_page_abs_floats: Vec<float::ActiveFloat>,
    /// Page-absolute TopAndBottom exclusions that have actually been
    /// registered on this physical page. They remain available after ordinary
    /// float pruning so a later column can reactivate the band.
    page_absolute_exclusions: Vec<float::ActiveFloat>,
    /// Sticky per-physical-page guard for side wraps and paragraph-relative
    /// TopAndBottom bands, which cannot be revived after pruning.
    column_reactivation_unsafe: bool,
    /// True when `current_page_abs_floats` needs rebuilding (e.g. after a page break).
    abs_floats_dirty: bool,
    /// Index of the first block on the current page (for forward scanning).
    page_start_block: usize,
    /// §17.4.38: style_id of the previous table for adjacent border collapse.
    prev_table_style_id: Option<StyleId>,
    /// §17.3.3.1: an inline page break at the end of a paragraph defers the
    /// page break to the start of the next block.
    pending_page_break: bool,
}

/// Stable reason codes for the opt-in pagination ledger.
///
/// These values are diagnostic only: passing a reason into `push_new_page`
/// must never change layout behavior.  Keeping the vocabulary here forces
/// every physical-page transition in the section stacker to identify its
/// source instead of leaving a collection of indistinguishable calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageBreakCause {
    DeferredInlineBreak,
    PageBreakBefore,
    KeepNextChain,
    ParagraphOverflow,
    BreakParagraphAfterTableOverflow,
    ExplicitColumnBreak,
    FloatingTableCollision,
    FloatingTableContinuation,
    TableContinuation,
}

impl<'doc> PageLayoutState<'doc> {
    fn new(
        config: &PageConfig,
        continuation: Option<ContinuationState>,
        bounds: PageBodyBounds,
        logical_page_base: usize,
    ) -> Self {
        let (
            current_page,
            cursor_y,
            page_top,
            current_col,
            column_top,
            bottom,
            page_floats,
            page_absolute_exclusions,
            column_reactivation_unsafe,
        ) = match continuation {
            Some(c) => {
                let same_columns = c.columns.len() == config.columns.len()
                    && c.columns.iter().zip(&config.columns).all(|(left, right)| {
                        left.x_offset == right.x_offset && left.width == right.width
                    });
                let same_flow_geometry = c.page_size == config.page_size
                    && c.page_top == bounds.top
                    && c.body_bottom == bounds.bottom
                    && same_columns;

                if same_flow_geometry {
                    // Preserve the real column. Using cursor_y as column_top
                    // makes a page-tail continuation look like a fresh,
                    // full-height column and permits silent overflow.
                    (
                        c.page,
                        c.cursor_y,
                        c.page_top,
                        c.current_col.min(config.num_columns().saturating_sub(1)),
                        c.column_top,
                        c.bottom.min(bounds.bottom),
                        c.page_floats,
                        c.page_absolute_exclusions,
                        c.column_reactivation_unsafe,
                    )
                } else {
                    // Changed geometry starts a new flow region on the shared
                    // physical page. It is not a full-height fresh column;
                    // overflow must advance rather than paint below the body.
                    // Full Word-style column balancing is handled separately.
                    let region_top = c.cursor_y.max(bounds.top);
                    (
                        c.page,
                        region_top,
                        bounds.top,
                        0,
                        region_top,
                        c.bottom.min(bounds.bottom),
                        c.page_floats,
                        c.page_absolute_exclusions,
                        c.column_reactivation_unsafe,
                    )
                }
            }
            None => (
                LayoutedPage::new(config.page_size),
                bounds.top,
                bounds.top,
                0,
                bounds.top,
                bounds.bottom,
                Vec::new(),
                Vec::new(),
                false,
            ),
        };
        PageLayoutState {
            pages: Vec::new(),
            column_top,
            current_page,
            cursor_y,
            page_index: 0,
            logical_page_base,
            page_top,
            current_col,
            bottom,
            page_footnotes: Vec::new(),
            first_on_section_page: true,
            prev_space_after: Pt::ZERO,
            prev_style_id: None,
            prev_after_auto_spacing: false,
            prev_list_spacing_context: None,
            prev_borders: None,
            page_floats,
            current_page_abs_floats: Vec::new(),
            page_absolute_exclusions,
            column_reactivation_unsafe,
            abs_floats_dirty: true,
            page_start_block: 0,
            prev_table_style_id: None,
            pending_page_break: false,
        }
    }

    /// Render accumulated footnotes onto the current page and clear the list.
    /// §20.4.3.1: parity of the page currently being assembled.
    fn parity(&self) -> PageParity {
        PageParity::of_page(self.logical_page_base + self.page_index)
    }

    /// Whether the cursor is at the top of a genuinely full-height column.
    /// A changed-geometry continuous section may begin a shorter flow region
    /// part-way down the page, where `cursor_y == column_top` is also true.
    fn at_full_column_top(&self) -> bool {
        self.cursor_y <= self.column_top && self.column_top <= self.page_top
    }

    fn archive_page_absolute_exclusion(&mut self, exclusion: float::ActiveFloat) {
        let duplicate = self.page_absolute_exclusions.iter().any(|existing| {
            (existing.page_x - exclusion.page_x).raw().abs() < FLOAT_DEDUP_EPSILON_PT
                && (existing.page_y_start - exclusion.page_y_start).raw().abs()
                    < FLOAT_DEDUP_EPSILON_PT
                && (existing.page_y_end - exclusion.page_y_end).raw().abs() < FLOAT_DEDUP_EPSILON_PT
        });
        if !duplicate {
            self.page_absolute_exclusions.push(exclusion);
        }
    }

    fn flush_footnotes(&mut self, ctx: &LayoutCtx<'_>) {
        if !self.page_footnotes.is_empty() {
            let footnotes: Vec<_> = self
                .page_footnotes
                .iter()
                .map(PageFootnote::as_borrowed)
                .collect();
            render_page_footnotes(
                &mut self.current_page,
                ctx.config,
                &footnotes,
                ctx.default_line_height,
                ctx.measure_text,
                ctx.separator_indent,
                ctx.page_bounds(self.page_index).bottom,
            );
            self.page_footnotes.clear();
        }
    }

    /// Commit the current page and start a fresh one, resetting all per-page state.
    /// Callers that also need `prev_space_after = Pt::ZERO` must set that separately.
    fn push_new_page(&mut self, block_idx: usize, ctx: &LayoutCtx<'_>, cause: PageBreakCause) {
        let body_commands = self.current_page.commands.len();
        let footnotes = self.page_footnotes.len();
        let floats = self.page_floats.len();
        self.flush_footnotes(ctx);
        log::debug!(
            target: "dxpdf::pagination",
            "page-break cause={cause:?} section_page={} logical_page={} block={} column={} cursor_pt={:.3} page_top_pt={:.3} bottom_pt={:.3} body_commands={} total_commands={} footnotes={} floats={}",
            self.page_index + 1,
            self.logical_page_base + self.page_index,
            block_idx,
            self.current_col,
            self.cursor_y.raw(),
            self.page_top.raw(),
            self.bottom.raw(),
            body_commands,
            self.current_page.commands.len(),
            footnotes,
            floats,
        );
        self.pages.push(std::mem::replace(
            &mut self.current_page,
            LayoutedPage::new(ctx.config.page_size),
        ));
        self.page_index += 1;
        let bounds = ctx.page_bounds(self.page_index);
        self.page_top = bounds.top;
        self.cursor_y = bounds.top;
        self.column_top = bounds.top;
        self.current_col = 0;
        self.bottom = bounds.bottom;
        self.page_start_block = block_idx;
        self.abs_floats_dirty = true;
        self.page_floats.clear();
        self.page_absolute_exclusions.clear();
        self.column_reactivation_unsafe = false;
    }

    /// Flush any remaining footnotes and either push the last page or preserve
    /// its exact flow state for a following `Continuous` section.
    fn finalize(mut self, ctx: &LayoutCtx<'_>, preserve_continuation: bool) -> SectionLayoutResult {
        self.flush_footnotes(ctx);
        if preserve_continuation {
            let body_bounds = ctx.page_bounds(self.page_index);
            let continuation = ContinuationState {
                page: self.current_page,
                cursor_y: self.cursor_y,
                page_size: ctx.config.page_size,
                page_top: self.page_top,
                body_bottom: body_bounds.bottom,
                columns: ctx.config.columns.clone(),
                current_col: self.current_col,
                column_top: self.column_top,
                bottom: self.bottom,
                page_floats: self.page_floats,
                page_absolute_exclusions: self.page_absolute_exclusions,
                column_reactivation_unsafe: self.column_reactivation_unsafe,
            };
            SectionLayoutResult {
                pages: self.pages,
                continuation: Some(continuation),
            }
        } else {
            self.pages.push(self.current_page);
            SectionLayoutResult {
                pages: self.pages,
                continuation: None,
            }
        }
    }

    /// The floats affecting text at the current cursor on the current
    /// page/column: registered page floats (§20.4.2) plus forward-scanned
    /// absolute floats from upcoming blocks on this page (rebuilt once per page
    /// while `abs_floats_dirty`), advancing the cursor past any full-width float
    /// that blocks all text (§17.4.56). Shared by the main block loop and the
    /// across-page split re-fit (§4) so a continuation wraps around whatever
    /// floats live on *its* page, not the paragraph's starting page.
    fn effective_floats_at_cursor(
        &mut self,
        blocks: &[LayoutBlock],
        relocated_absolute_float_blocks: &std::collections::HashSet<usize>,
        num_cols: usize,
        space_before: Pt,
        page_x: Pt,
        col_width: Pt,
    ) -> Vec<float::ActiveFloat> {
        let parity = self.parity();
        // Forward-scan absolute floats from upcoming paragraphs on the current
        // page. Only rescan when the page changes. The scan tracks inline
        // column/page boundaries (`scan_inline_page_boundary`) so a float past
        // an in-paragraph break isn't attributed to this page, and skips blocks
        // whose absolute float has been relocated to a later page (#86's
        // relocation replay), so wrapped text on the replayed page ignores it.
        if self.abs_floats_dirty {
            self.current_page_abs_floats.clear();
            let mut scan_col = self.current_col;
            for (fi_idx, future_block) in blocks[self.page_start_block..].iter().enumerate() {
                let future_block_idx = self.page_start_block + fi_idx;
                if relocated_absolute_float_blocks.contains(&future_block_idx) {
                    if fi_idx == 0 {
                        continue;
                    }
                    break;
                }
                if let LayoutBlock::Paragraph {
                    floating_images: fi_list,
                    page_break_before,
                    fragments,
                    ..
                } = future_block
                {
                    // Stop scanning at the next explicit page break (skip the
                    // first block — it may have triggered this page).
                    if *page_break_before && fi_idx > 0 {
                        break;
                    }
                    let boundary = scan_inline_page_boundary(fragments, &mut scan_col, num_cols);
                    if boundary != ForwardScanBoundary::BeforeParagraphFloat {
                        for fi in fi_list {
                            // §20.4.2.15/.18: only wrap-enabled modes narrow
                            // text. `TopAndBottom` is a full-width exclusion
                            // band and `None` is a pure overlay — matches the gate in
                            // `has_absolute_wrap_float` below.
                            if !fi.wrap_mode.registers_as_wrap_float()
                                && !fi.is_wrap_top_and_bottom()
                            {
                                continue;
                            }
                            if let FloatingImageY::Absolute(img_y) = fi.y {
                                let (float_x, float_width) = if fi.is_wrap_top_and_bottom() {
                                    (page_x, col_width)
                                } else {
                                    (
                                        fi.x.resolve(parity) - fi.dist_left,
                                        fi.size.width + fi.dist_left + fi.dist_right,
                                    )
                                };
                                self.current_page_abs_floats.push(float::ActiveFloat {
                                    page_x: float_x,
                                    page_y_start: img_y - fi.dist_top,
                                    page_y_end: img_y + fi.size.height + fi.dist_bottom,
                                    width: float_width,
                                    source: float::FloatSource::Image,
                                    vertical_exclusion: fi.is_wrap_top_and_bottom(),
                                    wrap_text: fi.wrap_mode.wrap_text().into(),
                                });
                            }
                        }
                    }
                    if boundary != ForwardScanBoundary::None {
                        break;
                    }
                }
            }
            self.abs_floats_dirty = false;
        }

        // Merge page_floats with forward-scanned absolute floats (dedup).
        // Ordinary side-wrapping floats become active at their own y;
        // TopAndBottom exclusions are retained ahead of the cursor so the
        // line fitter can detect a line box that crosses into the band.
        let mut effective_floats = self.page_floats.clone();
        let deduped: Vec<float::ActiveFloat> = self
            .current_page_abs_floats
            .iter()
            .chain(self.page_absolute_exclusions.iter())
            .filter(|af| {
                // A vertical exclusion may begin inside the upcoming line,
                // so the line fitter must see it even when its top is below
                // the current cursor. Ordinary side-wrapping floats keep the
                // historical threshold and become active at their own y.
                af.vertical_exclusion || af.page_y_start <= self.cursor_y + space_before
            })
            .filter(|af| {
                !effective_floats.iter().any(|pf| {
                    (pf.page_x - af.page_x).raw().abs() < FLOAT_DEDUP_EPSILON_PT
                        && (pf.page_y_start - af.page_y_start).raw().abs() < FLOAT_DEDUP_EPSILON_PT
                        && (pf.page_y_end - af.page_y_end).raw().abs() < FLOAT_DEDUP_EPSILON_PT
                })
            })
            .cloned()
            .collect();
        effective_floats.extend(deduped);

        // §17.4.56: advance past any full-width float that blocks all text.
        for ef in &effective_floats {
            if !ef.vertical_exclusion && ef.overlaps_y(self.cursor_y) && ef.width >= col_width {
                self.cursor_y = self.cursor_y.max(ef.page_y_end);
            }
        }
        float::prune_floats(&mut effective_floats, self.cursor_y);
        effective_floats
    }
}

#[derive(Clone)]
enum PageFootnote<'doc> {
    Borrowed(
        &'doc [super::super::fragment::Fragment],
        &'doc ParagraphStyle,
    ),
    Owned(Vec<super::super::fragment::Fragment>, ParagraphStyle),
}

impl PageFootnote<'_> {
    fn as_borrowed(&self) -> (&[super::super::fragment::Fragment], &ParagraphStyle) {
        match self {
            Self::Borrowed(fragments, style) => (fragments, style),
            Self::Owned(fragments, style) => (fragments, style),
        }
    }
}

struct ParagraphFloatCheckpoint {
    command_count: usize,
    behind_doc_commands: Vec<LayeredDrawCommand>,
    page_floats: Vec<float::ActiveFloat>,
    page_absolute_exclusions: Vec<float::ActiveFloat>,
    column_reactivation_unsafe: bool,
    cursor_y: Pt,
}

struct PageReplayCheckpoint<'doc> {
    current_page: LayoutedPage,
    cursor_y: Pt,
    page_index: usize,
    page_top: Pt,
    current_col: usize,
    column_top: Pt,
    bottom: Pt,
    page_footnotes: Vec<PageFootnote<'doc>>,
    first_on_section_page: bool,
    prev_space_after: Pt,
    prev_style_id: Option<StyleId>,
    prev_after_auto_spacing: bool,
    prev_list_spacing_context: Option<ListSpacingContext>,
    prev_borders: Option<ParagraphBorderStyle>,
    page_floats: Vec<float::ActiveFloat>,
    current_page_abs_floats: Vec<float::ActiveFloat>,
    page_absolute_exclusions: Vec<float::ActiveFloat>,
    column_reactivation_unsafe: bool,
    abs_floats_dirty: bool,
    page_start_block: usize,
    prev_table_style_id: Option<StyleId>,
    pending_page_break: bool,
}

impl<'doc> PageReplayCheckpoint<'doc> {
    fn capture(state: &PageLayoutState<'doc>) -> Self {
        Self {
            current_page: state.current_page.clone(),
            cursor_y: state.cursor_y,
            page_index: state.page_index,
            page_top: state.page_top,
            current_col: state.current_col,
            column_top: state.column_top,
            bottom: state.bottom,
            page_footnotes: state.page_footnotes.clone(),
            first_on_section_page: state.first_on_section_page,
            prev_space_after: state.prev_space_after,
            prev_style_id: state.prev_style_id.clone(),
            prev_after_auto_spacing: state.prev_after_auto_spacing,
            prev_list_spacing_context: state.prev_list_spacing_context,
            prev_borders: state.prev_borders.clone(),
            page_floats: state.page_floats.clone(),
            current_page_abs_floats: state.current_page_abs_floats.clone(),
            page_absolute_exclusions: state.page_absolute_exclusions.clone(),
            column_reactivation_unsafe: state.column_reactivation_unsafe,
            abs_floats_dirty: state.abs_floats_dirty,
            page_start_block: state.page_start_block,
            prev_table_style_id: state.prev_table_style_id.clone(),
            pending_page_break: state.pending_page_break,
        }
    }

    fn restore(&self, state: &mut PageLayoutState<'doc>) {
        state.current_page.clone_from(&self.current_page);
        state.cursor_y = self.cursor_y;
        state.page_index = self.page_index;
        state.page_top = self.page_top;
        state.current_col = self.current_col;
        state.column_top = self.column_top;
        state.bottom = self.bottom;
        state.page_footnotes.clone_from(&self.page_footnotes);
        state.first_on_section_page = self.first_on_section_page;
        state.prev_space_after = self.prev_space_after;
        state.prev_style_id.clone_from(&self.prev_style_id);
        state.prev_after_auto_spacing = self.prev_after_auto_spacing;
        state.prev_list_spacing_context = self.prev_list_spacing_context;
        state.prev_borders.clone_from(&self.prev_borders);
        state.page_floats.clone_from(&self.page_floats);
        state
            .current_page_abs_floats
            .clone_from(&self.current_page_abs_floats);
        state
            .page_absolute_exclusions
            .clone_from(&self.page_absolute_exclusions);
        state.column_reactivation_unsafe = self.column_reactivation_unsafe;
        state.abs_floats_dirty = self.abs_floats_dirty;
        state.page_start_block = self.page_start_block;
        state
            .prev_table_style_id
            .clone_from(&self.prev_table_style_id);
        state.pending_page_break = self.pending_page_break;
    }
}

fn refresh_page_replay_checkpoint<'doc>(
    state: &PageLayoutState<'doc>,
    block_idx: usize,
    checkpoint: &mut PageReplayCheckpoint<'doc>,
    checkpoint_page_index: &mut usize,
    replay_block_idx: &mut usize,
) {
    if state.page_index != *checkpoint_page_index {
        *checkpoint = PageReplayCheckpoint::capture(state);
        *checkpoint_page_index = state.page_index;
        *replay_block_idx = block_idx;
    }
}

impl ParagraphFloatCheckpoint {
    fn capture(state: &PageLayoutState<'_>) -> Self {
        Self {
            command_count: state.current_page.commands.len(),
            behind_doc_commands: state.current_page.behind_doc_commands.clone(),
            page_floats: state.page_floats.clone(),
            page_absolute_exclusions: state.page_absolute_exclusions.clone(),
            column_reactivation_unsafe: state.column_reactivation_unsafe,
            cursor_y: state.cursor_y,
        }
    }

    fn restore(&self, state: &mut PageLayoutState<'_>) {
        state.current_page.commands.truncate(self.command_count);
        state
            .current_page
            .behind_doc_commands
            .clone_from(&self.behind_doc_commands);
        state.page_floats.clone_from(&self.page_floats);
        state
            .page_absolute_exclusions
            .clone_from(&self.page_absolute_exclusions);
        state.column_reactivation_unsafe = self.column_reactivation_unsafe;
        state.cursor_y = self.cursor_y;
    }
}

fn push_floating_command(
    state: &mut PageLayoutState<'_>,
    behind_doc: bool,
    relative_height: u32,
    command: DrawCommand,
) {
    if behind_doc {
        state
            .current_page
            .push_behind_doc_command(relative_height, command);
    } else {
        state.current_page.commands.push(command);
    }
}

/// §17.17.1 / §20.1.2.1.1: emit a floating shape's `wps:txbx` text over its
/// fill. Each command is in shape-local coordinates; shift by the shape's
/// resolved page origin `(fs.x, shape_y)`. Mirrors the stacker's shape-text
/// emission (`section::stacker`) so body/page-anchored shapes render their text,
/// not just the fill/stroke.
fn emit_shape_text(state: &mut PageLayoutState<'_>, fs: &FloatingShape, shape_y: Pt) {
    let parity = state.parity();
    for mut cmd in fs.text_commands.iter().cloned() {
        cmd.shift(fs.x.resolve(parity), shape_y);
        push_floating_command(state, fs.behind_doc, fs.relative_height, cmd);
    }
}

fn register_paragraph_floats(
    state: &mut PageLayoutState<'_>,
    floating_images: &[FloatingImage],
    floating_shapes: &[FloatingShape],
    content_top: Pt,
    content_x: Pt,
    content_width: Pt,
) {
    let parity = state.parity();
    for fi in floating_images {
        let img_y = match fi.y {
            FloatingImageY::RelativeToParagraph(offset) => content_top + offset,
            FloatingImageY::Absolute(img_y) => img_y,
        };
        let y_start = img_y - fi.dist_top;
        let y_end = img_y + fi.size.height + fi.dist_bottom;
        if fi.is_wrap_top_and_bottom() {
            push_floating_command(
                state,
                fi.behind_doc,
                fi.relative_height,
                DrawCommand::Image {
                    rect: PtRect::from_xywh(
                        fi.x.resolve(parity),
                        img_y,
                        fi.size.width,
                        fi.size.height,
                    ),
                    image_data: fi.image_data.clone(),
                    src_rect: fi.src_rect,
                },
            );
            let exclusion = float::ActiveFloat {
                page_x: content_x,
                page_y_start: y_start,
                page_y_end: y_end,
                width: content_width,
                source: float::FloatSource::Image,
                vertical_exclusion: true,
                wrap_text: float::WrapTextSide::BothSides,
            };
            state.page_floats.push(exclusion.clone());
            if matches!(fi.y, FloatingImageY::Absolute(_)) {
                state.archive_page_absolute_exclusion(exclusion);
            } else {
                state.column_reactivation_unsafe = true;
            }
        } else if fi.wrap_mode.registers_as_wrap_float() {
            let float_entry = float::ActiveFloat {
                page_x: fi.x.resolve(parity) - fi.dist_left,
                page_y_start: y_start,
                page_y_end: y_end,
                width: fi.size.width + fi.dist_left + fi.dist_right,
                source: float::FloatSource::Image,
                vertical_exclusion: false,
                wrap_text: fi.wrap_mode.wrap_text().into(),
            };
            log::debug!(
                "[layout]   register image float: x={:.1} y={:.1}-{:.1} w={:.1}",
                float_entry.page_x.raw(),
                y_start.raw(),
                y_end.raw(),
                float_entry.width.raw()
            );
            state.page_floats.push(float_entry);
            state.column_reactivation_unsafe = true;
        }
    }

    for fs in floating_shapes {
        if matches!(fs.wrap_mode, WrapMode::None) {
            continue;
        }
        let shape_y = match fs.y {
            FloatingImageY::RelativeToParagraph(offset) => content_top + offset,
            FloatingImageY::Absolute(y) => y,
        };
        let y_start = shape_y - fs.dist_top;
        let y_end = shape_y + fs.size.height + fs.dist_bottom;
        if fs.is_wrap_top_and_bottom() {
            push_floating_command(
                state,
                fs.behind_doc,
                fs.relative_height,
                DrawCommand::Path {
                    origin: crate::render::geometry::PtOffset::new(fs.x.resolve(parity), shape_y),
                    rotation: fs.rotation,
                    flip_h: fs.flip_h,
                    flip_v: fs.flip_v,
                    extent: fs.size,
                    paths: fs.paths.clone(),
                    fill: fs.fill.clone(),
                    stroke: fs.stroke.clone(),
                    effects: fs.effects.clone(),
                },
            );
            emit_shape_text(state, fs, shape_y);
            let exclusion = float::ActiveFloat {
                page_x: content_x,
                page_y_start: y_start,
                page_y_end: y_end,
                width: content_width,
                source: float::FloatSource::Shape,
                vertical_exclusion: true,
                wrap_text: float::WrapTextSide::BothSides,
            };
            state.page_floats.push(exclusion.clone());
            if matches!(fs.y, FloatingImageY::Absolute(_)) {
                state.archive_page_absolute_exclusion(exclusion);
            } else {
                state.column_reactivation_unsafe = true;
            }
        } else {
            let float_entry = float::ActiveFloat {
                page_x: fs.x.resolve(parity) - fs.dist_left,
                page_y_start: y_start,
                page_y_end: y_end,
                width: fs.size.width + fs.dist_left + fs.dist_right,
                source: float::FloatSource::Shape,
                vertical_exclusion: false,
                wrap_text: fs.wrap_mode.wrap_text().into(),
            };
            log::debug!(
                "[layout]   register shape float: x={:.1} y={:.1}-{:.1} w={:.1} mode={:?}",
                float_entry.page_x.raw(),
                y_start.raw(),
                y_end.raw(),
                float_entry.width.raw(),
                fs.wrap_mode
            );
            state.page_floats.push(float_entry);
            state.column_reactivation_unsafe = true;
        }
    }
}

fn register_destination_paragraph_floats(
    state: &mut PageLayoutState<'_>,
    floating_images: &[FloatingImage],
    floating_shapes: &[FloatingShape],
    space_before: Pt,
    content_x: Pt,
    col_width: Pt,
) {
    let content_top = state.cursor_y + space_before;
    register_paragraph_floats(
        state,
        floating_images,
        floating_shapes,
        content_top,
        content_x,
        col_width,
    );
    for active_float in &state.page_floats {
        if active_float.overlaps_y(state.cursor_y) && active_float.width >= col_width {
            state.cursor_y = state.cursor_y.max(active_float.page_y_end);
        }
    }
    float::prune_floats(&mut state.page_floats, state.cursor_y);
}

/// WPS compatibility for a narrow TopAndBottom edge case. When a visible,
/// single-line owner fits wholly above its own exclusion band but its trailing
/// paragraph spacing would enter that band, WPS clears the owner below the
/// band and then applies `space_before` normally. This is deliberately not a
/// blanket owner-paragraph barrier: ordinary lines that overlap a band keep
/// using the standard per-line clearance path.
fn wps_trailing_space_clearance_start(
    current_y: Pt,
    body_bottom: Pt,
    style: &ParagraphStyle,
    placed: &PlacedParagraph<'_>,
    fragments: &[Fragment],
    floating_images: &[FloatingImage],
    floating_shapes: &[FloatingShape],
) -> Option<Pt> {
    let has_visible_owner_content = fragments.iter().any(|fragment| match fragment {
        Fragment::Text { text, .. } => !text.trim().is_empty(),
        Fragment::Image { .. } | Fragment::InlineGraphic { .. } | Fragment::Emoji { .. } => true,
        Fragment::Tab { .. }
        | Fragment::PTab { .. }
        | Fragment::LineBreak { .. }
        | Fragment::ColumnBreak
        | Fragment::PageBreak { .. }
        | Fragment::Bookmark { .. } => false,
    });
    if !has_visible_owner_content
        || placed.line_count() != 1
        || !floating_images.is_empty()
        || floating_shapes.len() != 1
        || style.space_after <= Pt::ZERO
    {
        return None;
    }

    let band = if let Some(shape) = floating_shapes.first() {
        if !shape.is_wrap_top_and_bottom() {
            return None;
        }
        let FloatingImageY::Absolute(y) = shape.y else {
            return None;
        };
        Some((
            y - shape.dist_top,
            y + shape.size.height + shape.dist_bottom,
        ))
    } else {
        None
    }?;

    let line_boxes_height = placed.line_box_height();
    let content_bottom = current_y + style.space_before + line_boxes_height;
    let paragraph_end = content_bottom + style.space_after;
    let (band_start, band_end) = band;
    if content_bottom > band_start || paragraph_end <= band_start || band_end <= current_y {
        return None;
    }

    let moved_paragraph_height = style.space_before + line_boxes_height + style.space_after;
    (band_end > current_y && band_end + moved_paragraph_height <= body_bottom).then_some(band_end)
}

fn has_absolute_wrap_float(floating_images: &[FloatingImage]) -> bool {
    floating_images.iter().any(|image| {
        matches!(image.y, FloatingImageY::Absolute(_))
            && (image.wrap_mode.registers_as_wrap_float() || image.is_wrap_top_and_bottom())
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForwardScanBoundary {
    None,
    BeforeParagraphFloat,
    AfterParagraphFloat,
}

fn scan_inline_page_boundary(
    fragments: &[super::super::fragment::Fragment],
    current_col: &mut usize,
    num_cols: usize,
) -> ForwardScanBoundary {
    for (index, fragment) in fragments.iter().enumerate() {
        match fragment {
            super::super::fragment::Fragment::PageBreak { .. } => {
                let has_following_content = fragments[index + 1..].iter().any(|fragment| {
                    !matches!(fragment, super::super::fragment::Fragment::PageBreak { .. })
                });
                return if has_following_content {
                    ForwardScanBoundary::BeforeParagraphFloat
                } else {
                    ForwardScanBoundary::AfterParagraphFloat
                };
            }
            super::super::fragment::Fragment::ColumnBreak => {
                if *current_col + 1 < num_cols {
                    *current_col += 1;
                } else {
                    *current_col = 0;
                    return ForwardScanBoundary::BeforeParagraphFloat;
                }
            }
            _ => {}
        }
    }

    ForwardScanBoundary::None
}

fn mark_absolute_float_relocation(
    paragraph_content_placed: bool,
    moves_to_new_page: bool,
    block_idx: usize,
    replay_block_idx: usize,
    floating_images: &[FloatingImage],
    relocated_blocks: &mut std::collections::HashSet<usize>,
) -> bool {
    !paragraph_content_placed
        && moves_to_new_page
        && block_idx > replay_block_idx
        && has_absolute_wrap_float(floating_images)
        && relocated_blocks.insert(block_idx)
}

fn paragraph_keep_next(block: &LayoutBlock) -> bool {
    matches!(block, LayoutBlock::Paragraph { style, .. } if style.keep_next)
}

/// Word exposes no table-level `keepNext` property.  At the body/table seam it
/// uses the first paragraph in the first cell of the last row as the table's
/// terminal paragraph mark.  Keep that deliberately narrow: another cell or a
/// later block in the first cell must not promote the whole table into a body
/// chain.
fn table_keep_next_sentinel(block: &LayoutBlock) -> bool {
    let LayoutBlock::Table {
        rows,
        float_info: None,
        ..
    } = block
    else {
        return false;
    };

    matches!(
        rows.last()
            .and_then(|row| row.cells.first())
            .and_then(|cell| cell.blocks.first()),
        Some(LayoutBlock::Paragraph { style, .. }) if style.keep_next
    )
}

/// Full-table prediction is intentionally limited to content whose measured
/// height is self-contained.  The ordinary table paginator remains the sole
/// authority for footnotes, nested tables, and anchored objects.
fn table_is_simple_keep_next_bridge(block: &LayoutBlock) -> bool {
    let LayoutBlock::Table { rows, .. } = block else {
        return false;
    };

    rows.iter().all(|row| {
        row.cells.iter().all(|cell| {
            cell.blocks.iter().all(|block| match block {
                LayoutBlock::Paragraph {
                    fragments,
                    page_break_before,
                    footnotes,
                    floating_images,
                    floating_shapes,
                    ..
                } => {
                    !*page_break_before
                        && footnotes.is_empty()
                        && floating_images.is_empty()
                        && floating_shapes.is_empty()
                        && !fragments.iter().any(|fragment| {
                            matches!(fragment, Fragment::PageBreak { .. } | Fragment::ColumnBreak)
                        })
                }
                LayoutBlock::Table { .. } => false,
            })
        })
    })
}

fn starts_hard_body_boundary(block: &LayoutBlock) -> bool {
    match block {
        LayoutBlock::Paragraph {
            fragments,
            page_break_before,
            floating_images,
            floating_shapes,
            ..
        } => {
            *page_break_before
                || !floating_images.is_empty()
                || !floating_shapes.is_empty()
                || fragments.iter().any(|fragment| {
                    matches!(fragment, Fragment::PageBreak { .. } | Fragment::ColumnBreak)
                })
        }
        LayoutBlock::Table { float_info, .. } => float_info.is_some(),
    }
}

fn paragraph_has_inline_page_break(block: &LayoutBlock) -> bool {
    matches!(
        block,
        LayoutBlock::Paragraph { fragments, .. }
            if fragments.iter().any(Fragment::is_page_break)
    )
}

fn starts_keep_next_chain(blocks: &[LayoutBlock], block_idx: usize) -> bool {
    paragraph_keep_next(&blocks[block_idx])
        && !paragraph_has_inline_page_break(&blocks[block_idx])
        && (block_idx == 0
            || matches!(
                blocks[block_idx],
                LayoutBlock::Paragraph {
                    page_break_before: true,
                    ..
                }
            )
            || !paragraph_keep_next(&blocks[block_idx - 1]))
}

fn keep_next_terminal_table(blocks: &[LayoutBlock], start: usize) -> Option<&LayoutBlock> {
    let mut index = start;

    while let Some(block) = blocks.get(index) {
        match block {
            LayoutBlock::Paragraph {
                fragments,
                style,
                page_break_before,
                ..
            } => {
                if fragments.iter().any(Fragment::is_page_break)
                    || (index > start && *page_break_before)
                {
                    return None;
                }
                if !style.keep_next {
                    return None;
                }
                index += 1;
            }
            LayoutBlock::Table {
                float_info: None, ..
            } => return Some(block),
            LayoutBlock::Table {
                float_info: Some(_),
                ..
            } => return None,
        }
    }

    None
}

fn fresh_page_spacing_overlap(
    block: &LayoutBlock,
    previous_style_id: &Option<StyleId>,
    previous_after_auto_spacing: bool,
    previous_list_spacing_context: Option<ListSpacingContext>,
) -> Pt {
    let LayoutBlock::Paragraph { style, .. } = block else {
        return Pt::ZERO;
    };
    let effective = style.clone_for_layout();
    effective.spacing_overlap_with_previous(
        Pt::ZERO,
        previous_style_id.as_ref(),
        previous_after_auto_spacing,
        previous_list_spacing_context,
    )
}

#[derive(Clone, Copy, Debug)]
struct KeepNextGroupMeasurement {
    body_height: Pt,
    footnote_height: Pt,
    has_footnotes: bool,
}

impl KeepNextGroupMeasurement {
    fn total_height(&self, separator_already_reserved: bool) -> Pt {
        self.body_height
            + self.footnote_height
            + if self.has_footnotes && !separator_already_reserved {
                FOOTNOTE_SEPARATOR_GAP
            } else {
                Pt::ZERO
            }
    }
}

#[derive(Clone, Copy, Debug)]
struct TableAwareKeepNextPrefix {
    group: KeepNextGroupMeasurement,
    /// First body block not represented by this complete-segment prefix.
    end_exclusive: usize,
}

fn measured_prefix_or_none(
    has_bridge_table: bool,
    group: KeepNextGroupMeasurement,
    end_exclusive: usize,
) -> Option<TableAwareKeepNextPrefix> {
    has_bridge_table.then_some(TableAwareKeepNextPrefix {
        group,
        end_exclusive,
    })
}

fn table_aware_prefix_already_admitted(block_idx: usize, admitted_through: usize) -> bool {
    block_idx < admitted_through
}

/// Measure the largest complete body-block prefix, starting at `start`, that
/// fits inside `max_height` and contains at least one table-level `keepNext`
/// bridge.
///
/// Paragraphs and complete bridge tables are admission segments.  Once a
/// bridge has been measured, a later oversized or unsupported segment ends the
/// safe prefix instead of invalidating it; this is Word's practical fallback
/// for a keepNext chain longer than one page.  Before the first bridge, every
/// unsupported condition returns `None` so the established paragraph/table
/// predictor remains authoritative.
fn measure_table_aware_keep_next_prefix(
    blocks: &[LayoutBlock],
    start: usize,
    constraints: &BoxConstraints,
    default_line_height: Pt,
    measure_text: super::super::paragraph::MeasureTextFn<'_>,
    max_height: Pt,
    initial_prev_table_style_id: Option<&StyleId>,
    stop_exclusive: Option<usize>,
) -> Option<TableAwareKeepNextPrefix> {
    let first = blocks.get(start)?;
    match first {
        LayoutBlock::Paragraph { style, .. } if style.keep_next => {}
        LayoutBlock::Table { .. }
            if table_keep_next_sentinel(first) && blocks.get(start + 1).is_some() => {}
        _ => return None,
    }

    let mut group = KeepNextGroupMeasurement {
        body_height: Pt::ZERO,
        footnote_height: Pt::ZERO,
        has_footnotes: false,
    };
    let mut previous_space_after = Pt::ZERO;
    let mut previous_style_id = None;
    let mut previous_after_auto_spacing = false;
    let mut previous_list_spacing_context = None;
    let mut previous_table_style_id = initial_prev_table_style_id.cloned();
    let mut has_bridge_table = false;
    let mut index = start;

    loop {
        if stop_exclusive == Some(index) {
            return measured_prefix_or_none(has_bridge_table, group, index);
        }

        let Some(block) = blocks.get(index) else {
            return measured_prefix_or_none(has_bridge_table, group, index);
        };
        match block {
            LayoutBlock::Paragraph {
                fragments,
                style,
                page_break_before,
                footnotes,
                floating_images,
                floating_shapes,
            } => {
                let hard_boundary = fragments.iter().any(|fragment| {
                    matches!(fragment, Fragment::PageBreak { .. } | Fragment::ColumnBreak)
                }) || (index > start && *page_break_before);
                let unsupported_anchor = !floating_images.is_empty() || !floating_shapes.is_empty();
                if hard_boundary || unsupported_anchor {
                    return measured_prefix_or_none(has_bridge_table, group, index);
                }

                let effective = style.clone_for_layout();
                let collapsed = effective.spacing_overlap_with_previous(
                    previous_space_after,
                    previous_style_id.as_ref(),
                    previous_after_auto_spacing,
                    previous_list_spacing_context,
                );
                let layout = layout_paragraph(
                    fragments,
                    constraints,
                    &effective,
                    default_line_height,
                    measure_text,
                );
                let mut candidate = group;
                candidate.body_height += layout.size.height - collapsed;
                for note in footnotes {
                    for (footnote_fragments, footnote_style) in &note.paragraphs {
                        let footnote = layout_paragraph(
                            footnote_fragments,
                            &BoxConstraints::tight_width(constraints.max_width, Pt::INFINITY),
                            footnote_style,
                            default_line_height,
                            measure_text,
                        );
                        candidate.footnote_height += footnote.size.height;
                        candidate.has_footnotes = true;
                    }
                }
                if candidate.total_height(false) > max_height {
                    return measured_prefix_or_none(has_bridge_table, group, index);
                }

                group = candidate;
                previous_space_after = effective.space_after;
                previous_style_id = effective.style_id.clone();
                previous_after_auto_spacing = effective.after_auto_spacing;
                previous_list_spacing_context = effective.list_spacing_context;
                previous_table_style_id = None;
                index += 1;
                if !effective.keep_next {
                    return measured_prefix_or_none(has_bridge_table, group, index);
                }
            }
            LayoutBlock::Table {
                rows,
                col_widths,
                cell_spacing,
                border_config,
                style_id,
                ..
            } => {
                let successor = blocks.get(index + 1);
                if !table_keep_next_sentinel(block)
                    || !table_is_simple_keep_next_bridge(block)
                    || successor.is_none_or(starts_hard_body_boundary)
                {
                    return measured_prefix_or_none(has_bridge_table, group, index);
                }

                let suppress_top = style_id.is_some() && *style_id == previous_table_style_id;
                let Some(table_height) = measure_complete_table_height(
                    rows,
                    col_widths,
                    *cell_spacing,
                    default_line_height,
                    border_config.as_ref(),
                    measure_text,
                    suppress_top,
                ) else {
                    return measured_prefix_or_none(has_bridge_table, group, index);
                };
                let mut candidate = group;
                candidate.body_height += table_height;
                if candidate.total_height(false) > max_height {
                    return measured_prefix_or_none(has_bridge_table, group, index);
                }

                group = candidate;
                has_bridge_table = true;
                previous_space_after = Pt::ZERO;
                previous_style_id = None;
                previous_after_auto_spacing = false;
                previous_list_spacing_context = None;
                previous_table_style_id = style_id.clone();
                index += 1;
            }
        }
    }
}

fn measure_keep_next_group(
    blocks: &[LayoutBlock],
    start: usize,
    constraints: &BoxConstraints,
    default_line_height: Pt,
    measure_text: super::super::paragraph::MeasureTextFn<'_>,
) -> Option<KeepNextGroupMeasurement> {
    let mut measurement = KeepNextGroupMeasurement {
        body_height: Pt::ZERO,
        footnote_height: Pt::ZERO,
        has_footnotes: false,
    };
    let mut previous_space_after = Pt::ZERO;
    let mut previous_style_id = None;
    let mut previous_after_auto_spacing = false;
    let mut previous_list_spacing_context = None;
    let mut index = start;

    while let Some(block) = blocks.get(index) {
        match block {
            LayoutBlock::Paragraph {
                fragments,
                style,
                page_break_before,
                footnotes,
                ..
            } => {
                if fragments.iter().any(Fragment::is_page_break)
                    || (index > start && *page_break_before)
                {
                    return None;
                }
                let effective = style.clone_for_layout();
                let collapsed = effective.spacing_overlap_with_previous(
                    previous_space_after,
                    previous_style_id.as_ref(),
                    previous_after_auto_spacing,
                    previous_list_spacing_context,
                );
                let layout = layout_paragraph(
                    fragments,
                    constraints,
                    &effective,
                    default_line_height,
                    measure_text,
                );
                measurement.body_height += layout.size.height - collapsed;
                for note in footnotes {
                    for (footnote_fragments, footnote_style) in &note.paragraphs {
                        let footnote = layout_paragraph(
                            footnote_fragments,
                            &BoxConstraints::tight_width(constraints.max_width, Pt::INFINITY),
                            footnote_style,
                            default_line_height,
                            measure_text,
                        );
                        measurement.footnote_height += footnote.size.height;
                        measurement.has_footnotes = true;
                    }
                }
                previous_space_after = effective.space_after;
                previous_style_id = effective.style_id.clone();
                previous_after_auto_spacing = effective.after_auto_spacing;
                previous_list_spacing_context = effective.list_spacing_context;
                if !effective.keep_next {
                    return Some(measurement);
                }
                index += 1;
            }
            LayoutBlock::Table { .. } => {
                return Some(measurement);
            }
        }
    }
    None
}

/// §17.3.1.15 (keepNext tightening): can the paragraph that *starts* a keepNext
/// chain be split so a widow-legal head stays on the current page while its tail
/// travels to the fresh page with the rest of the group?
///
/// keepNext forbids a page break only *between* a paragraph and the next block —
/// it does not pin the paragraph's earlier lines. So when the whole group would
/// otherwise be moved to a fresh page (leaving the current page under-filled),
/// a splittable leading paragraph can instead fill the current page and carry
/// its `>= 2`-line tail onward: the caller has already checked the whole group
/// fits on a fresh page, and the remainder after peeling a `>= 2`-line head is
/// strictly smaller, so it still fits there and no keepNext boundary is broken.
///
/// True only for a body paragraph that can actually split — no keepLines
/// (§17.3.1.14), no floating objects (they anchor to one page), footnotes only
/// on an unbroken chunk, and enough lines to leave `>= 2` on each side under
/// §17.3.1.44 widow control. A `false` result keeps the conservative
/// whole-group move.
fn leading_keep_next_paragraph_splittable(
    block: &LayoutBlock,
    constraints: &BoxConstraints,
    default_line_height: Pt,
    measure_text: super::super::paragraph::MeasureTextFn<'_>,
) -> bool {
    let LayoutBlock::Paragraph {
        fragments,
        style,
        footnotes,
        floating_images,
        floating_shapes,
        ..
    } = block
    else {
        return false;
    };
    let effective = style.clone_for_layout();
    // `single_chunk` is a whole-paragraph property, so it can be derived here
    // exactly as the placement gate derives it from its loop state: both
    // splitters are pure functions of the fragment list, and when the paragraph
    // has more than one page chunk the flag is false regardless of column
    // chunking.
    let single_chunk =
        split_at_page_breaks(fragments).len() == 1 && split_at_column_breaks(fragments).len() == 1;
    if !paragraph_breakable(
        &effective,
        footnotes,
        floating_images,
        floating_shapes,
        single_chunk,
    ) {
        return false;
    }
    let placed = place_paragraph(
        fragments,
        constraints,
        &effective,
        default_line_height,
        measure_text,
    );
    // Widow control (§17.3.1.44) needs `>= 2` lines on each side of the break;
    // without it a single-line head is legal. Deliberately stricter than the
    // placement gate's flat `>= 2`: this predicts whether a split would yield a
    // *useful* head, and under-predicting only costs a conservative move.
    let min_lines = if effective.widow_control { 4 } else { 2 };
    placed.line_count() >= min_lines
}

/// Can an unsplittable keepNext prefix stay on this page by placing a legal
/// head of its terminal paragraph after it?
///
/// Word may keep a one-line heading at the page tail together with the first
/// two lines of the following paragraph, then continue that paragraph on the
/// next page. Moving the entire group is unnecessarily conservative: the
/// keepNext boundary is already satisfied once the heading and terminal
/// paragraph share this page. This predictor handles only the deterministic
/// paragraph-only case; notes, floats, explicit breaks and table terminals keep
/// the existing whole-group fallback.
fn terminal_keep_next_paragraph_splittable_here(
    blocks: &[LayoutBlock],
    start: usize,
    current_available: Pt,
    constraints: &BoxConstraints,
    default_line_height: Pt,
    measure_text: super::super::paragraph::MeasureTextFn<'_>,
) -> bool {
    if current_available <= Pt::ZERO {
        return false;
    }

    let mut used = Pt::ZERO;
    let mut previous_space_after = Pt::ZERO;
    let mut previous_style_id = None;
    let mut previous_after_auto_spacing = false;
    let mut previous_list_spacing_context = None;
    let mut index = start;

    while let Some(block) = blocks.get(index) {
        let LayoutBlock::Paragraph {
            fragments,
            style,
            page_break_before,
            footnotes,
            floating_images,
            floating_shapes,
        } = block
        else {
            return false;
        };
        if fragments.iter().any(Fragment::is_page_break)
            || (index > start && *page_break_before)
            || !footnotes.is_empty()
            || !floating_images.is_empty()
            || !floating_shapes.is_empty()
        {
            return false;
        }

        let effective = style.clone_for_layout();
        let collapsed = effective.spacing_overlap_with_previous(
            previous_space_after,
            previous_style_id.as_ref(),
            previous_after_auto_spacing,
            previous_list_spacing_context,
        );
        let placed = place_paragraph(
            fragments,
            constraints,
            &effective,
            default_line_height,
            measure_text,
        );

        if effective.keep_next {
            used += placed.emit_full().size.height - collapsed;
            if used >= current_available {
                return false;
            }
            previous_space_after = effective.space_after;
            previous_style_id = effective.style_id.clone();
            previous_after_auto_spacing = effective.after_auto_spacing;
            previous_list_spacing_context = effective.list_spacing_context;
            index += 1;
            continue;
        }

        if effective.keep_lines {
            return false;
        }
        let total = placed.line_count();
        let min_head = if effective.widow_control { 2 } else { 1 };
        let min_tail = min_head;
        if total < min_head + min_tail {
            return false;
        }

        for head in (min_head..=total - min_tail).rev() {
            let segment = placed.emit_split_segment(0, head, true, false);
            if used + segment.size.height - collapsed <= current_available {
                return true;
            }
        }
        return false;
    }

    false
}

/// §17.3.1.14 / §17.3.1.15: the conditions under which a paragraph may be
/// broken across a page or column boundary, shared by the placement gate
/// (`can_split`) and the keepNext predictor above.
///
/// Extracted because the two disagreed: the predictor omitted the footnote
/// condition, so a keepNext-chain head carrying footnotes across several
/// page/column chunks was reported splittable, the whole-group move was
/// skipped, and the placement path then refused to split it and fell back to
/// atomic — legal output on an under-filled page, but not the pagination either
/// side intended.
///
/// Line counts are **not** included. The gate needs `>= 2`; the predictor needs
/// enough to leave a legal head *and* tail. Folding them together would either
/// weaken the predictor or tighten the gate, so each keeps its own.
fn paragraph_breakable(
    style: &crate::render::layout::paragraph::ParagraphStyle,
    footnotes: &[LayoutFootnote],
    floating_images: &[FloatingImage],
    floating_shapes: &[FloatingShape],
    single_chunk: bool,
) -> bool {
    if style.keep_lines || !floating_images.is_empty() || !floating_shapes.is_empty() {
        return false;
    }
    // Footnotes are reserved per segment, but only for a single unbroken chunk:
    // with explicit page/column breaks a reference's segment is ambiguous, so
    // those paragraphs keep the atomic reservation.
    footnotes.is_empty() || single_chunk
}

/// §17.3.1.14 / §17.3.1.44: how to break a paragraph across a page boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParagraphSplit {
    /// Emit all lines on the current page (they fit, or must overflow because no
    /// legal split exists and the paragraph already starts at the page top).
    All,
    /// Emit the first `head` lines here and carry the remaining `total - head`
    /// to the next page. Invariants: `1 <= head < total`; under widow control,
    /// additionally `head >= 2 && total - head >= 2`.
    Break { head: usize },
    /// No legal break at this fill level, and the paragraph does not start at the
    /// page top — move the whole paragraph to the next page and re-decide there.
    MoveWhole,
}

/// Decide how a paragraph whose first `n_fit` of `total` lines fit in the
/// remaining space should break (§17.3.1.14 keepLines is handled by the caller,
/// which only calls this for splittable paragraphs).
///
/// `widow_control` (§17.3.1.44) forbids leaving a single line stranded: a legal
/// break keeps `>= 2` lines on each side. `at_page_top` is true when the
/// paragraph begins at the very top of the page column; there, the remaining
/// space already equals a full page, so "move whole" cannot help — a paragraph
/// taller than a page must split ("where possible"), and one that cannot split
/// legally is emitted whole and allowed to overflow. This guarantees the stacker
/// always makes progress (never loops).
fn decide_paragraph_split(
    n_fit: usize,
    total: usize,
    widow_control: bool,
    at_page_top: bool,
) -> ParagraphSplit {
    debug_assert!(total >= 1, "a paragraph has at least one line");
    if n_fit >= total {
        return ParagraphSplit::All;
    }

    // The paragraph does not fully fit; find the largest legal head.
    let head = if widow_control {
        // Leave >= 2 lines for the tail (widow) and keep >= 2 here (orphan).
        let capped = n_fit.min(total.saturating_sub(2));
        if capped >= 2 {
            capped
        } else {
            0 // no widow/orphan-legal break at this fill level
        }
    } else {
        // Without widow control a single line on either side is allowed; place
        // as many as fit, leaving at least one for the tail (n_fit < total, so
        // n_fit <= total - 1 already holds).
        n_fit
    };

    if head >= 1 {
        return ParagraphSplit::Break { head };
    }

    // No legal break at this fill level.
    if at_page_top {
        // Remaining space is a full page and the paragraph still cannot fit or
        // split legally — emit it whole and let it overflow.
        ParagraphSplit::All
    } else {
        ParagraphSplit::MoveWhole
    }
}

/// Place a splittable paragraph, breaking its lines across pages as needed.
///
/// The caller establishes splittability — see the `can_split` gate at the call
/// site, which is the authority. It requires: no §17.3.1.14 `keepLines`, no
/// floating images or shapes (those anchor to one page), `>= 2` fitted lines,
/// and footnotes only within a single unbroken chunk (with explicit
/// page/column breaks a reference→segment mapping is ambiguous, so those keep
/// the atomic reservation).
///
/// Deliberately *not* required — each was allowed by a later change and this
/// list is the historical trip hazard:
/// - **Borders, shading and drop caps** split fine; they are drawn per segment
///   (`emit_segment_borders_and_shading`).
/// - **Multiple columns** split fine; §17.6.4 unequal-width columns work
///   because each segment re-fits against its own column's width.
///
/// Each iteration fits as many remaining lines as the current page holds,
/// applies §17.3.1.44 widow/orphan control via [`decide_paragraph_split`],
/// emits that segment, and page-breaks to continue. `line_start` advances by
/// `>= 1` on every emitted segment and a `MoveWhole` always lands on a fresh
/// page where progress is forced, so the loop terminates.
/// §17.11.23: reserve `footnotes` on the current page — measure each, subtract
/// its height (and the separator gap for the first footnote on the page) from
/// the available bottom, and queue it for rendering. Shared by the atomic
/// placement path and the per-segment split path.
fn reserve_footnotes<'doc>(
    state: &mut PageLayoutState<'doc>,
    ctx: &LayoutCtx<'_>,
    content_width: Pt,
    footnotes: &'doc [LayoutFootnote],
) {
    if footnotes.is_empty() {
        return;
    }
    let fn_constraints = BoxConstraints::tight_width(content_width, Pt::INFINITY);
    for note in footnotes {
        for (fn_frags, fn_style) in &note.paragraphs {
            let fn_para = layout_paragraph(
                fn_frags,
                &fn_constraints,
                fn_style,
                ctx.default_line_height,
                ctx.measure_text,
            );
            // Reserve separator space only for the first footnote paragraph on this page.
            if state.page_footnotes.is_empty() {
                state.bottom -= FOOTNOTE_SEPARATOR_GAP;
            }
            state.bottom -= fn_para.size.height;
            state
                .page_footnotes
                .push(PageFootnote::Borrowed(fn_frags, fn_style));
        }
    }
}

fn reserve_owned_footnotes<'doc>(
    state: &mut PageLayoutState<'doc>,
    ctx: &LayoutCtx<'_>,
    content_width: Pt,
    footnotes: Vec<LayoutFootnote>,
) {
    if footnotes.is_empty() {
        return;
    }
    let fn_constraints = BoxConstraints::tight_width(content_width, Pt::INFINITY);
    for note in footnotes {
        for (fragments, style) in note.paragraphs {
            let height = layout_paragraph(
                &fragments,
                &fn_constraints,
                &style,
                ctx.default_line_height,
                ctx.measure_text,
            )
            .size
            .height;
            if state.page_footnotes.is_empty() {
                state.bottom -= FOOTNOTE_SEPARATOR_GAP;
            }
            state.bottom -= height;
            state
                .page_footnotes
                .push(PageFootnote::Owned(fragments, style));
        }
    }
}

/// §17.6.4: advance the split cursor to the next column, or — when the last
/// column is full — to the top of a fresh page. A fresh column, like a fresh
/// page, offers the full column height, so the split fit treats both as "at the
/// column top".
fn advance_column_or_page(
    state: &mut PageLayoutState<'_>,
    block_idx: usize,
    ctx: &LayoutCtx<'_>,
    num_cols: usize,
    para_start_y: &mut Pt,
) {
    if state.current_col + 1 < num_cols {
        state.current_col += 1;
        state.cursor_y = state.column_top;
    } else {
        state.push_new_page(block_idx, ctx, PageBreakCause::ParagraphOverflow);
    }
    *para_start_y = state.cursor_y;
}

#[allow(clippy::too_many_arguments)] // stacker state + placement inputs are cohesive
fn emit_split_paragraph<'doc>(
    state: &mut PageLayoutState<'doc>,
    fragments: &[Fragment],
    style: &ParagraphStyle,
    block_idx: usize,
    config: &PageConfig,
    ctx: &LayoutCtx<'_>,
    para_start_y: &mut Pt,
    footnotes: &'doc [LayoutFootnote],
    content_width: Pt,
    blocks: &[LayoutBlock],
    relocated_absolute_float_blocks: &std::collections::HashSet<usize>,
) {
    let num_cols = config.num_columns();
    let col_x = |col: usize| config.margins.left + config.columns[col].x_offset;
    let widow_control = style.widow_control;

    // §4: the remainder to place on the current page, plus the style to place
    // it with. Both are re-derived per page/column so a continuation wraps
    // around whatever floats live on *its* page (not the paragraph's starting
    // page) and uses that page's width. Owned so they can be re-sliced/re-styled
    // across page breaks. The first page reuses the caller's `style` (which
    // already carries the starting page's floats/width).
    let mut remaining: Vec<Fragment> = fragments.to_vec();
    let mut cont_style: ParagraphStyle = style.clone();
    let mut first_segment = true;
    // §17.11.12: footnotes reserved in document order; this many are placed.
    let mut footnote_cursor = 0;

    loop {
        let col_width = config.columns[state.current_col].width;
        let page_height = (state.bottom - state.page_top).max(Pt::ZERO);
        let constraints = BoxConstraints::new(Pt::ZERO, col_width, Pt::ZERO, page_height);
        let placed = place_paragraph(
            &remaining,
            &constraints,
            &cont_style,
            ctx.default_line_height,
            ctx.measure_text,
        );
        let total = placed.line_count();
        if total == 0 {
            break;
        }

        // §17.6.4: a fresh column offers full height, like a fresh page.
        let at_page_top = state.at_full_column_top();
        let avail = (state.bottom - state.cursor_y).max(Pt::ZERO);
        // Count how many of this placement's lines fit, charging space_before
        // (first segment only, §17.3.1.33). Word does not use space_after to
        // decide whether the final visible line may remain at a page boundary;
        // emission still preserves it so the following block advances normally.
        // A plain Auto text line may also use its lowest baseline instead of
        // trailing line-box leading and glyph descent. Borders and every
        // non-plain line remain conservative.
        let mut used = if first_segment {
            cont_style.space_before
        } else {
            Pt::ZERO
        };
        let mut n_fit = 0;
        for i in 0..total {
            let is_final_line = i + 1 == total;
            let fit_height = if is_final_line && footnotes.is_empty() {
                placed
                    .final_line_baseline_fit_height(i)
                    .unwrap_or_else(|| placed.line_height(i))
            } else {
                placed.line_height(i)
            };
            let mut needed = used + fit_height;
            if is_final_line {
                needed += placed.bottom_border_space();
            }
            if needed > avail {
                break;
            }
            used += placed.line_height(i);
            n_fit += 1;
        }

        // §17.3.1.44 widow/orphan, then §17.3.1.11/§17.3.1.44 unbreakable-prefix
        // clamp (drop cap / float-wrapped run kept intact on the first segment).
        let head = match decide_paragraph_split(n_fit, total, widow_control, at_page_top) {
            ParagraphSplit::All => Some(total),
            ParagraphSplit::Break { head } => Some(head),
            ParagraphSplit::MoveWhole => None,
        }
        .and_then(|head| {
            prefix_adjusted_head(
                head,
                total,
                placed.unbreakable_prefix_lines(),
                first_segment,
                at_page_top,
            )
        });

        // No legal placement here: move the whole remainder to the next
        // column/page and re-fit at the top (nothing emitted, so the drop cap
        // and space_before are preserved for the real first segment).
        let Some(head) = head else {
            drop(placed);
            advance_column_or_page(state, block_idx, ctx, num_cols, para_start_y);
            cont_style = continuation_style(
                state,
                blocks,
                style,
                config,
                relocated_absolute_float_blocks,
                first_segment,
            );
            continue;
        };

        let is_last = head == total;
        let segment = placed.emit_split_segment(0, head, first_segment, is_last);
        if !(first_segment && is_last) {
            log::debug!(
                "[layout]   paragraph split: {head}/{total} lines at y={:.1}",
                state.cursor_y.raw()
            );
        }
        let segment_x = col_x(state.current_col);
        for mut cmd in segment.commands {
            cmd.shift_y(state.cursor_y);
            cmd.shift_x(segment_x);
            state.current_page.commands.push(cmd);
        }
        state.cursor_y += segment.size.height;

        // §17.11.12: reserve this segment's footnotes on the page its reference
        // marks landed on, before any page break.
        if !footnotes.is_empty() {
            let refs = placed.footnote_refs_in(0, head);
            let end = (footnote_cursor + refs).min(footnotes.len());
            reserve_footnotes(state, ctx, content_width, &footnotes[footnote_cursor..end]);
            footnote_cursor = end;
        }

        if is_last {
            break;
        }

        // §4: carry the unplaced fragments to the next column/page and re-fit
        // them there against that page's floats and width.
        let next_remaining = placed.fragments_from_line(head);
        drop(placed);
        first_segment = false;
        advance_column_or_page(state, block_idx, ctx, num_cols, para_start_y);
        cont_style = continuation_style(
            state,
            blocks,
            style,
            config,
            relocated_absolute_float_blocks,
            first_segment,
        );
        remaining = next_remaining;
    }
}

/// Build the paragraph style for a split segment after advancing to a new
/// column/page (§4 continuation re-fit). Refreshes the float set for the new
/// page (`effective_floats_at_cursor`, which may also advance the cursor past a
/// full-width float) and repositions the paragraph. When something has already
/// been emitted (`first_segment == false`), the continuation drops
/// `space_before` (§17.3.1.33), the drop cap (§17.3.1.11), and the first-line
/// indent (§17.3.1.12); a whole-paragraph move that emitted nothing keeps them
/// for the eventual real first segment.
fn continuation_style(
    state: &mut PageLayoutState<'_>,
    blocks: &[LayoutBlock],
    style: &ParagraphStyle,
    config: &PageConfig,
    relocated_absolute_float_blocks: &std::collections::HashSet<usize>,
    first_segment: bool,
) -> ParagraphStyle {
    let col = state.current_col;
    let col_width = config.columns[col].width;
    let page_x = config.margins.left + config.columns[col].x_offset;
    let space_before = if first_segment {
        style.space_before
    } else {
        Pt::ZERO
    };
    let floats = state.effective_floats_at_cursor(
        blocks,
        relocated_absolute_float_blocks,
        config.num_columns(),
        space_before,
        page_x,
        col_width,
    );
    let mut cs = style.clone();
    if !first_segment {
        cs.drop_cap = None;
        cs.indent_first_line = Pt::ZERO;
        cs.space_before = Pt::ZERO;
    }
    cs.page_floats = floats;
    cs.page_y = state.cursor_y;
    cs.page_x = page_x;
    cs.page_content_width = col_width;
    cs
}

/// Constrain a chosen split `head` so it never falls inside the paragraph's
/// unbreakable prefix (§17.3.1.11 drop cap and/or §17.3.1.44 float-wrapped run,
/// the first `prefix_lines` lines). Breaking there would tear a drop-cap glyph
/// or leave float-narrowed lines to be reused full-width on the next page.
///
/// Returns `Some(head)` — unchanged when it already clears the prefix — or
/// `None` meaning "move the whole paragraph to a fresh page" (a break inside the
/// prefix with room above). At the page top the prefix cannot move any higher,
/// so it is emitted whole (`Some(prefix_lines)`) and allowed to overflow. The
/// guard only applies to the first segment (the prefix lives at the start).
fn prefix_adjusted_head(
    head: usize,
    remaining: usize,
    prefix_lines: usize,
    first_segment: bool,
    at_page_top: bool,
) -> Option<usize> {
    let breaks_prefix =
        first_segment && prefix_lines > 1 && head < prefix_lines && head < remaining;
    if !breaks_prefix {
        return Some(head);
    }
    if at_page_top {
        Some(prefix_lines.min(remaining))
    } else {
        None
    }
}

fn is_plain_empty_spacer(block: &LayoutBlock) -> bool {
    matches!(
        block,
        LayoutBlock::Paragraph {
            fragments,
            style,
            page_break_before: false,
            footnotes,
            floating_images,
            floating_shapes,
        } if fragments.iter().all(|fragment| {
            matches!(fragment, Fragment::LineBreak { .. } | Fragment::Bookmark { .. })
        })
            && style.borders.is_none()
            && style.shading.is_none()
            && footnotes.is_empty()
            && floating_images.is_empty()
            && floating_shapes.is_empty()
    )
}

fn is_break_only_paragraph(block: &LayoutBlock) -> bool {
    matches!(
        block,
        LayoutBlock::Paragraph { fragments, .. }
            if fragments.iter().any(Fragment::is_page_break)
                && fragments.iter().all(|fragment| {
                    matches!(fragment, Fragment::PageBreak { .. } | Fragment::Bookmark { .. })
                })
    )
}

fn table_rows_have_footnotes(rows: &[TableRowInput]) -> bool {
    rows.iter()
        .flat_map(|row| row.cells.iter())
        .flat_map(|cell| cell.blocks.iter())
        .any(layout_block_has_footnotes)
}

fn layout_block_has_footnotes(block: &LayoutBlock) -> bool {
    match block {
        LayoutBlock::Paragraph { footnotes, .. } => !footnotes.is_empty(),
        LayoutBlock::Table { rows, .. } => table_rows_have_footnotes(rows),
    }
}

/// Cross-column table slices may only reuse a physical page whose wrapping
/// state can be reconstructed after the cursor returns to the column top.
/// Pure overlays do not affect flow. Page-absolute TopAndBottom exclusions are
/// the one supported wrapping case because the forward registry deliberately
/// keeps them alive for all columns; side wraps and paragraph-relative bands
/// retain the historical page-only table path.
fn layout_block_has_unsupported_column_float(block: &LayoutBlock) -> bool {
    let (floating_images, floating_shapes) = match block {
        LayoutBlock::Paragraph {
            floating_images,
            floating_shapes,
            ..
        } => (floating_images, floating_shapes),
        LayoutBlock::Table { float_info, .. } => return float_info.is_some(),
    };

    let image_is_unsupported = |image: &FloatingImage| {
        let affects_flow =
            image.wrap_mode.registers_as_wrap_float() || image.is_wrap_top_and_bottom();
        affects_flow
            && !(image.is_wrap_top_and_bottom() && matches!(image.y, FloatingImageY::Absolute(_)))
    };
    let shape_is_unsupported = |shape: &FloatingShape| {
        let affects_flow =
            shape.wrap_mode.registers_as_wrap_float() || shape.is_wrap_top_and_bottom();
        affects_flow
            && !(shape.is_wrap_top_and_bottom() && matches!(shape.y, FloatingImageY::Absolute(_)))
    };

    floating_images.iter().any(image_is_unsupported)
        || floating_shapes.iter().any(shape_is_unsupported)
}

fn page_heights_match(a: Pt, b: Pt) -> bool {
    (a - b).abs() <= Pt::new(0.01)
}

fn starts_with_inline_page_break(block: &LayoutBlock) -> bool {
    matches!(
        block,
        LayoutBlock::Paragraph { fragments, .. }
            if fragments
                .iter()
                .find(|fragment| !matches!(fragment, Fragment::Bookmark { .. }))
                .is_some_and(Fragment::is_page_break)
    )
}

/// Pure empty paragraphs immediately before an explicit page break are
/// page-tail padding in Word. They may consume the remaining body height, but
/// they do not create an intervening blank page before either
/// `pageBreakBefore` or a break-only paragraph.
fn followed_by_explicit_page_break(blocks: &[LayoutBlock], block_idx: usize) -> bool {
    blocks[block_idx + 1..]
        .iter()
        .find(|block| !is_plain_empty_spacer(block))
        .is_some_and(|block| {
            starts_with_inline_page_break(block)
                || matches!(
                    block,
                    LayoutBlock::Paragraph {
                        page_break_before: true,
                        ..
                    }
                )
        })
}

/// A break-only paragraph after a table's trailing empty paragraph still owns
/// a paragraph mark. If that mark starts beyond the body bottom, Word moves it
/// to the next page before applying the break, leaving that page blank. Plain
/// padding after ordinary body text follows the separate collapse rule above.
fn break_only_follows_table_spacer_chain(blocks: &[LayoutBlock], block_idx: usize) -> bool {
    if block_idx == 0 || !is_break_only_paragraph(&blocks[block_idx]) {
        return false;
    }
    let mut index = block_idx;
    let mut saw_spacer = false;
    while index > 0 {
        index -= 1;
        if is_plain_empty_spacer(&blocks[index]) {
            saw_spacer = true;
            continue;
        }
        return saw_spacer && matches!(blocks[index], LayoutBlock::Table { .. });
    }
    false
}

/// Where a section begins in the document's page sequence.
///
/// The three travel together because they answer one question — which page
/// this section's first page *is*. `continuation` says whether it shares a
/// page with the section before it, `clearance` how much of that page the
/// header and footer take, and `logical_page_base` what §17.10.6 calls it.
pub(crate) struct SectionStart<'a> {
    /// §17.6.22: an in-progress page to continue on, for a `Continuous` break.
    pub continuation: Option<ContinuationState>,
    /// Header/footer clearances, selected per physical page in the section.
    pub clearance: &'a HeaderFooterClearance,
    /// §17.10.6: logical number of the section's first page, with
    /// `w:pgNumType/@start` applied. Drives §20.4.3.1 float mirroring.
    pub logical_page_base: usize,
}

/// Document-level result. The public single-section API still returns pages;
/// the document orchestrator additionally consumes the exact terminal flow
/// state when the next section is continuous.
pub(crate) struct SectionLayoutResult {
    pub(crate) pages: Vec<LayoutedPage>,
    pub(crate) continuation: Option<ContinuationState>,
}

/// Lay out a sequence of blocks into pages.
///
/// If `continuation` is provided, the section starts on the given page at the
/// given cursor_y (for `SectionType::Continuous` sections).
pub fn layout_section(
    blocks: &[LayoutBlock],
    config: &PageConfig,
    measure_text: super::super::paragraph::MeasureTextFn<'_>,
    separator_indent: Pt,
    default_line_height: Pt,
    continuation: Option<ContinuationState>,
) -> Vec<LayoutedPage> {
    let clearance = HeaderFooterClearance::uniform(config);
    layout_section_with_clearance(
        blocks,
        config,
        measure_text,
        separator_indent,
        default_line_height,
        SectionStart {
            continuation,
            clearance: &clearance,
            // A section laid out on its own starts at page 1 — the §17.10.6
            // renumbering only exists across a document's section list.
            logical_page_base: 1,
        },
    )
}

/// Lay out a sequence of blocks using header/footer clearances selected for
/// each physical page in the section.
pub(crate) fn layout_section_with_clearance(
    blocks: &[LayoutBlock],
    config: &PageConfig,
    measure_text: super::super::paragraph::MeasureTextFn<'_>,
    separator_indent: Pt,
    default_line_height: Pt,
    start: SectionStart<'_>,
) -> Vec<LayoutedPage> {
    layout_section_with_clearance_result(
        blocks,
        config,
        measure_text,
        separator_indent,
        default_line_height,
        start,
        false,
    )
    .pages
}

/// Document-level section layout that can preserve the last physical page and
/// its real flow cursor for a following continuous section.
pub(crate) fn layout_section_with_clearance_result(
    blocks: &[LayoutBlock],
    config: &PageConfig,
    measure_text: super::super::paragraph::MeasureTextFn<'_>,
    separator_indent: Pt,
    default_line_height: Pt,
    start: SectionStart<'_>,
    preserve_continuation: bool,
) -> SectionLayoutResult {
    let SectionStart {
        continuation,
        clearance,
        logical_page_base,
    } = start;
    let content_width = config.content_width();
    let num_cols = config.num_columns();

    let ctx = LayoutCtx {
        config,
        clearance,
        measure_text,
        separator_indent,
        default_line_height,
    };
    let mut state = PageLayoutState::new(
        config,
        continuation,
        clearance.for_page(0),
        logical_page_base,
    );

    // Column-aware constraints and x-offset for the current column.
    let col_constraints = |col: usize, page_height: Pt| -> BoxConstraints {
        let col_width = config.columns[col].width;
        BoxConstraints::new(Pt::ZERO, col_width, Pt::ZERO, page_height)
    };
    let col_x = |col: usize| -> Pt { config.margins.left + config.columns[col].x_offset };

    // A forward-scanned absolute float can later move to another page. Keep
    // those owners out of the source page's next scan while replaying it.
    let mut relocated_absolute_float_blocks = std::collections::HashSet::new();
    let mut page_replay_state = PageReplayCheckpoint::capture(&state);
    let mut checkpoint_page_index = state.page_index;
    let mut replay_block_idx = 0;
    let mut block_idx = 0;
    // End of a P7 prefix that was already moved as one admission unit.  A
    // bridge table inside it must not run a second preflight and skip another
    // page when its Table arm is reached.
    let mut table_aware_admitted_through = 0usize;

    'blocks: while block_idx < blocks.len() {
        let block = &blocks[block_idx];
        // §17.3.3.1: a deferred inline page break from the previous block
        // forces this block onto a new page.
        if state.pending_page_break {
            state.pending_page_break = false;
            // An explicit page break always advances exactly one page. In
            // particular, a second break at the top of a page must preserve
            // the intervening blank page instead of being collapsed.
            state.push_new_page(block_idx, &ctx, PageBreakCause::DeferredInlineBreak);
            state.prev_space_after = Pt::ZERO;
        }
        refresh_page_replay_checkpoint(
            &state,
            block_idx,
            &mut page_replay_state,
            &mut checkpoint_page_index,
            &mut replay_block_idx,
        );

        match block {
            LayoutBlock::Paragraph {
                fragments,
                style,
                page_break_before,
                footnotes,
                floating_images,
                floating_shapes,
            } => {
                // §17.3.1.23: force a new page before this paragraph.
                if *page_break_before && state.cursor_y > state.page_top {
                    state.push_new_page(block_idx, &ctx, PageBreakCause::PageBreakBefore);
                    state.prev_space_after = Pt::ZERO;
                }

                let mut table_aware_keep_next_handled =
                    table_aware_prefix_already_admitted(block_idx, table_aware_admitted_through);
                if num_cols == 1
                    && !table_aware_keep_next_handled
                    && starts_keep_next_chain(blocks, block_idx)
                    && state.page_floats.is_empty()
                {
                    let constraints = col_constraints(
                        state.current_col,
                        (state.bottom - state.page_top).max(Pt::ZERO),
                    );
                    let full_page_height = ctx.page_bounds(state.page_index + 1).height();
                    let fresh_spacing_overlap = fresh_page_spacing_overlap(
                        &blocks[block_idx],
                        &state.prev_style_id,
                        state.prev_after_auto_spacing,
                        state.prev_list_spacing_context,
                    );
                    let fresh_prefix = measure_table_aware_keep_next_prefix(
                        blocks,
                        block_idx,
                        &constraints,
                        ctx.default_line_height,
                        ctx.measure_text,
                        full_page_height + fresh_spacing_overlap,
                        None,
                        None,
                    );
                    if let Some(fresh_prefix) = fresh_prefix {
                        let current_prefix = measure_table_aware_keep_next_prefix(
                            blocks,
                            block_idx,
                            &constraints,
                            ctx.default_line_height,
                            ctx.measure_text,
                            Pt::INFINITY,
                            state.prev_table_style_id.as_ref(),
                            Some(fresh_prefix.end_exclusive),
                        );
                        if let Some(current_prefix) = current_prefix {
                            table_aware_keep_next_handled = true;
                            let current_group_top = state.cursor_y
                                - style.spacing_overlap_with_previous(
                                    state.prev_space_after,
                                    state.prev_style_id.as_ref(),
                                    state.prev_after_auto_spacing,
                                    state.prev_list_spacing_context,
                                );
                            let current_group_height = current_prefix
                                .group
                                .total_height(!state.page_footnotes.is_empty());
                            let fresh_group_height =
                                fresh_prefix.group.total_height(false) - fresh_spacing_overlap;
                            let should_move = fresh_group_height <= full_page_height
                                && current_group_height > state.bottom - current_group_top;
                            if should_move && !state.at_full_column_top() {
                                log::debug!(
                                    "[layout] table-aware keepNext prefix {}..{} moved to fresh page",
                                    block_idx,
                                    fresh_prefix.end_exclusive,
                                );
                                state.push_new_page(block_idx, &ctx, PageBreakCause::KeepNextChain);
                                state.prev_space_after = Pt::ZERO;
                                table_aware_admitted_through =
                                    table_aware_admitted_through.max(fresh_prefix.end_exclusive);
                            }
                        }
                    }
                }

                if num_cols == 1
                    && !table_aware_keep_next_handled
                    && starts_keep_next_chain(blocks, block_idx)
                    && state.page_floats.is_empty()
                {
                    let constraints = col_constraints(
                        state.current_col,
                        (state.bottom - state.page_top).max(Pt::ZERO),
                    );
                    if let Some(group) = measure_keep_next_group(
                        blocks,
                        block_idx,
                        &constraints,
                        ctx.default_line_height,
                        ctx.measure_text,
                    ) {
                        let current_group_height =
                            group.total_height(!state.page_footnotes.is_empty());
                        let current_group_top = match &blocks[block_idx] {
                            LayoutBlock::Paragraph { style, .. } => {
                                state.cursor_y
                                    - style.spacing_overlap_with_previous(
                                        state.prev_space_after,
                                        state.prev_style_id.as_ref(),
                                        state.prev_after_auto_spacing,
                                        state.prev_list_spacing_context,
                                    )
                            }
                            LayoutBlock::Table { .. } => state.cursor_y,
                        };
                        let full_page_height = ctx.page_bounds(state.page_index + 1).height();
                        let fresh_page_group_height = group.total_height(false)
                            - fresh_page_spacing_overlap(
                                &blocks[block_idx],
                                &state.prev_style_id,
                                state.prev_after_auto_spacing,
                                state.prev_list_spacing_context,
                            );
                        let current_available = state.bottom - current_group_top;
                        let should_move = match keep_next_terminal_table(blocks, block_idx) {
                            Some(LayoutBlock::Table {
                                rows,
                                col_widths,
                                cell_spacing,
                                border_config,
                                ..
                            }) if !rows.is_empty() => {
                                let current_available_after_group =
                                    current_available - current_group_height;
                                let full_page_available =
                                    full_page_height - fresh_page_group_height;
                                let leading_group_height = measure_leading_table_group_height(
                                    rows,
                                    col_widths,
                                    *cell_spacing,
                                    ctx.default_line_height,
                                    border_config.as_ref(),
                                    ctx.measure_text,
                                    false,
                                );
                                leading_group_height.is_some_and(|height| {
                                    height <= full_page_available
                                        && height > current_available_after_group
                                })
                            }
                            _ => {
                                // §17.3.1.15 (11.2): the group doesn't fit here
                                // but fits on a fresh page. Rather than move the
                                // whole group, let a splittable leading
                                // paragraph fill this page and carry its
                                // widow-legal tail onward — the remainder is
                                // strictly smaller than the (fresh-page-fitting)
                                // group, so the keepNext boundary stays intact.
                                // Only for paragraph terminals: a table terminal
                                // is excluded from the group measurement, so its
                                // leading row is not covered by that guarantee
                                // and keeps the whole-move (handled above).
                                fresh_page_group_height <= full_page_height
                                    && current_group_top + current_group_height > state.bottom
                                    && !leading_keep_next_paragraph_splittable(
                                        &blocks[block_idx],
                                        &constraints,
                                        ctx.default_line_height,
                                        ctx.measure_text,
                                    )
                                    && !terminal_keep_next_paragraph_splittable_here(
                                        blocks,
                                        block_idx,
                                        current_available,
                                        &constraints,
                                        ctx.default_line_height,
                                        ctx.measure_text,
                                    )
                            }
                        };
                        if should_move && !state.at_full_column_top() {
                            state.push_new_page(block_idx, &ctx, PageBreakCause::KeepNextChain);
                            state.prev_space_after = Pt::ZERO;
                        }
                    }
                }
                refresh_page_replay_checkpoint(
                    &state,
                    block_idx,
                    &mut page_replay_state,
                    &mut checkpoint_page_index,
                    &mut replay_block_idx,
                );

                let mut effective_style = style.clone_for_layout();

                // Log paragraph info.
                let first_text = fragments
                    .iter()
                    .find_map(|f| {
                        if let super::super::fragment::Fragment::Text { text, .. } = f {
                            Some(&**text)
                        } else {
                            None
                        }
                    })
                    .unwrap_or("");
                log::debug!(
                    "[layout] page={} block[{block_idx}] para style={:?} text={:?} cursor_y={:.1} col={} floats={} fwd_floats={}",
                    state.page_index + 1,
                    effective_style.style_id, &first_text[..first_text.len().min(30)],
                    state.cursor_y.raw(), state.current_col,
                    state.page_floats.len(), state.current_page_abs_floats.len()
                );

                // §17.3.1.33: suppress space_before for the structural first
                // paragraph of a section on its initial page.
                if state.cursor_y <= state.column_top && state.first_on_section_page {
                    effective_style.space_before = Pt::ZERO;
                }
                // §17.3.1.24: paragraph border grouping — consecutive paragraphs
                // with identical borders suppress interior top borders.
                if effective_style.borders.is_some()
                    && effective_style.borders == state.prev_borders
                {
                    if let Some(ref mut b) = effective_style.borders {
                        b.top = None;
                    }
                }
                // §17.3.1.9: spacing collapse (must happen before float registration).
                state.cursor_y -= effective_style.spacing_overlap_with_previous(
                    state.prev_space_after,
                    state.prev_style_id.as_ref(),
                    state.prev_after_auto_spacing,
                    state.prev_list_spacing_context,
                );

                // Register floating images (both relative and absolute).
                // §20.4.2.18: wrapTopAndBottom images are emitted immediately
                // and register a full-width band. Paragraph lines before the
                // band remain above it; the first overlapping line jumps below.
                // §20.4.2.10: paragraph-relative floats use the content area
                // top (after space_before), not the total paragraph box top.
                let float_checkpoint = ParagraphFloatCheckpoint::capture(&state);
                let content_top = state.cursor_y + effective_style.space_before;
                let col_width = config.columns[state.current_col].width;
                let page_x = col_x(state.current_col);
                register_paragraph_floats(
                    &mut state,
                    floating_images,
                    floating_shapes,
                    content_top,
                    page_x,
                    col_width,
                );

                // Prune expired floats.
                float::prune_floats(&mut state.page_floats, state.cursor_y);

                // §20.4.2 / §17.4.56: floats affecting text at the cursor —
                // registered page floats plus the boundary-/relocation-aware
                // forward scan of upcoming blocks — advancing past any
                // full-width blocker.
                let effective_floats = state.effective_floats_at_cursor(
                    blocks,
                    &relocated_absolute_float_blocks,
                    num_cols,
                    effective_style.space_before,
                    page_x,
                    col_width,
                );

                effective_style.page_floats = effective_floats;
                effective_style.page_y = state.cursor_y;
                effective_style.page_x = page_x;
                effective_style.page_content_width = col_width;

                // §17.3.3.1: split paragraph at inline page breaks first,
                // then §17.6.4: split each page-chunk at column breaks.
                let page_chunks = split_at_page_breaks(fragments);
                if state.cursor_y >= state.bottom
                    && !state.at_full_column_top()
                    && break_only_follows_table_spacer_chain(blocks, block_idx)
                {
                    state.push_new_page(
                        block_idx,
                        &ctx,
                        PageBreakCause::BreakParagraphAfterTableOverflow,
                    );
                    state.prev_space_after = Pt::ZERO;
                    effective_style.page_y = state.cursor_y;
                    effective_style.page_x = col_x(state.current_col);
                    effective_style.page_content_width = config.columns[state.current_col].width;
                    effective_style.page_floats = state.page_floats.clone();
                }
                let mut para_start_y = state.cursor_y;
                // §17.4.56 (#86 relocation): true once any of this paragraph's
                // content has been placed, so a later overflow relocates its
                // absolute float instead of double-wrapping earlier text.
                let mut paragraph_content_placed = false;
                // §17.11.12: set once a split segment reserves this paragraph's
                // footnotes per page, so the atomic after-loop reservation is
                // skipped (avoids double-reserving).
                let mut footnotes_reserved = false;

                // §17.3.3.1: track whether an unresolved page break remains
                // after processing all chunks, so it can be deferred to the
                // next block.
                let mut unresolved_page_break = false;

                // Word keeps a plain empty spacer immediately following a
                // body table on the table's page when that spacer is the only
                // block that no longer fits. The invisible paragraph mark may
                // extend into the bottom margin; moving it to the next page
                // creates a spurious blank line above the following content.
                // Do not apply this to decorated paragraphs or paragraphs that
                // own notes/floats, because those have visible page semantics.
                let current_is_plain_empty_spacer = is_plain_empty_spacer(block);
                // Word leaves the final invisible paragraph mark on the
                // source page when the mark starts inside the text area but
                // its line box crosses the bottom boundary.  This is not
                // limited to the structural mark after a table: form-like
                // documents commonly use an ordinary empty paragraph as
                // response space. Moving that paragraph to the next page
                // creates a visible blank band above the next question.
                //
                // The start-position guard matters for consecutive empty
                // paragraphs. Only the mark that straddles the boundary may
                // remain; a later mark whose start is already outside the
                // text area must advance normally, otherwise an arbitrary run
                // of empty paragraphs could disappear into the bottom margin.
                let page_tail_empty_spacer_may_overflow =
                    current_is_plain_empty_spacer && state.cursor_y < state.bottom;
                let trailing_table_spacer_may_overflow = current_is_plain_empty_spacer
                    && block_idx > 0
                    && matches!(blocks[block_idx - 1], LayoutBlock::Table { .. });
                let explicit_break_spacer_may_overflow = current_is_plain_empty_spacer
                    && followed_by_explicit_page_break(blocks, block_idx);
                // A break-only paragraph has no visible line of its own. Keep
                // its bookmark/paragraph mark on the source page at the page
                // tail and let the explicit break advance following content
                // exactly once. Moving the mark first creates an empty page.
                let break_only_paragraph_may_overflow = is_break_only_paragraph(block);

                for (page_chunk_idx, page_chunk) in page_chunks.iter().enumerate() {
                    if page_chunk_idx > 0 {
                        unresolved_page_break = true;
                    }

                    // §17.3.3.1: skip empty chunks — they carry no renderable
                    // content. A page break whose leading or trailing side is
                    // empty simply means "nothing on this side of the break."
                    if page_chunk.is_empty() {
                        continue;
                    }

                    // §17.3.3.1: force a new page for non-empty chunks that
                    // follow a page break.
                    if unresolved_page_break {
                        if mark_absolute_float_relocation(
                            paragraph_content_placed,
                            true,
                            block_idx,
                            replay_block_idx,
                            floating_images,
                            &mut relocated_absolute_float_blocks,
                        ) {
                            page_replay_state.restore(&mut state);
                            block_idx = replay_block_idx;
                            continue 'blocks;
                        }
                        if !paragraph_content_placed {
                            float_checkpoint.restore(&mut state);
                        }
                        state.push_new_page(block_idx, &ctx, PageBreakCause::ParagraphOverflow);
                        state.prev_space_after = Pt::ZERO;
                        para_start_y = state.cursor_y;
                        if !paragraph_content_placed {
                            let col_width = config.columns[state.current_col].width;
                            let content_x = col_x(state.current_col);
                            register_destination_paragraph_floats(
                                &mut state,
                                floating_images,
                                floating_shapes,
                                effective_style.space_before,
                                content_x,
                                col_width,
                            );
                        }
                        effective_style.page_y = state.cursor_y;
                        effective_style.page_x = col_x(state.current_col);
                        effective_style.page_content_width =
                            config.columns[state.current_col].width;
                        effective_style.page_floats = state.page_floats.clone();
                    }

                    let col_chunks = split_at_column_breaks(page_chunk);

                    for (chunk_idx, chunk) in col_chunks.iter().enumerate() {
                        // Advance to the next column for chunks after a column break.
                        if chunk_idx > 0 {
                            let starts_new_page = state.current_col + 1 >= num_cols;
                            if mark_absolute_float_relocation(
                                paragraph_content_placed,
                                starts_new_page,
                                block_idx,
                                replay_block_idx,
                                floating_images,
                                &mut relocated_absolute_float_blocks,
                            ) {
                                page_replay_state.restore(&mut state);
                                block_idx = replay_block_idx;
                                continue 'blocks;
                            }
                            if !paragraph_content_placed && starts_new_page {
                                float_checkpoint.restore(&mut state);
                            }
                            if !starts_new_page {
                                state.current_col += 1;
                            } else {
                                // All columns full — new page, reset to column 0.
                                state.push_new_page(
                                    block_idx,
                                    &ctx,
                                    PageBreakCause::ExplicitColumnBreak,
                                );
                            }
                            state.cursor_y = state.column_top;
                            if !paragraph_content_placed && starts_new_page {
                                let col_width = config.columns[state.current_col].width;
                                let content_x = col_x(state.current_col);
                                register_destination_paragraph_floats(
                                    &mut state,
                                    floating_images,
                                    floating_shapes,
                                    effective_style.space_before,
                                    content_x,
                                    col_width,
                                );
                            }
                            effective_style.page_y = state.cursor_y;
                            effective_style.page_x = col_x(state.current_col);
                            effective_style.page_content_width =
                                config.columns[state.current_col].width;
                            if starts_new_page {
                                effective_style.page_floats = state.page_floats.clone();
                            }
                        }

                        let constraints = col_constraints(
                            state.current_col,
                            (state.bottom - state.page_top).max(Pt::ZERO),
                        );
                        let mut placed = place_paragraph(
                            chunk,
                            &constraints,
                            &effective_style,
                            ctx.default_line_height,
                            ctx.measure_text,
                        );

                        let single_chunk = page_chunks.len() == 1 && col_chunks.len() == 1;
                        if single_chunk
                            && footnotes.is_empty()
                            && effective_style.borders.is_none()
                            && effective_style.shading.is_none()
                            && effective_style.drop_cap.is_none()
                        {
                            if let Some(clearance_start) = wps_trailing_space_clearance_start(
                                state.cursor_y,
                                state.bottom,
                                &effective_style,
                                &placed,
                                chunk,
                                floating_images,
                                floating_shapes,
                            ) {
                                drop(placed);
                                state.cursor_y = clearance_start;
                                para_start_y = clearance_start;
                                effective_style.page_y = clearance_start;
                                float::prune_floats(&mut state.page_floats, clearance_start);
                                effective_style.page_floats = state.effective_floats_at_cursor(
                                    blocks,
                                    &relocated_absolute_float_blocks,
                                    num_cols,
                                    effective_style.space_before,
                                    effective_style.page_x,
                                    effective_style.page_content_width,
                                );
                                placed = place_paragraph(
                                    chunk,
                                    &constraints,
                                    &effective_style,
                                    ctx.default_line_height,
                                    ctx.measure_text,
                                );
                            }
                        }

                        // §17.3.1.14: a paragraph may break across a page
                        // boundary when keepLines is unset. Borders/shading, drop
                        // caps, and lines wrapped around an active float are
                        // handled per segment (the float-wrapped run is kept on
                        // the first segment by the unbreakable-prefix guard, so
                        // the tail reuses its fitted lines). Paragraphs that own
                        // floating objects stay atomic (the object anchors to one
                        // page) and take the #86 relocation path in the `else`
                        // branch below. Footnotes are reserved per segment, but
                        // only for a single, unbroken chunk — with explicit
                        // page/column breaks their reference→segment mapping is
                        // ambiguous, so those keep the atomic reservation.
                        // §17.6.4: splitting re-fits the
                        // remainder against each column's own width
                        // (`emit_split_paragraph`), so unequal-width columns split
                        // correctly — no equal-width gate.
                        let can_split = paragraph_breakable(
                            &effective_style,
                            footnotes,
                            floating_images,
                            floating_shapes,
                            single_chunk,
                        ) && placed.line_count() >= 2;

                        if can_split {
                            if !footnotes.is_empty() {
                                footnotes_reserved = true;
                            }
                            drop(placed);
                            emit_split_paragraph(
                                &mut state,
                                chunk,
                                &effective_style,
                                block_idx,
                                config,
                                &ctx,
                                &mut para_start_y,
                                footnotes,
                                content_width,
                                blocks,
                                &relocated_absolute_float_blocks,
                            );
                        } else {
                            let mut para = placed.emit_full();
                            // Column/page overflow: advance column, then page.
                            if state.cursor_y + para.size.height > state.bottom
                                && !state.at_full_column_top()
                                && !page_tail_empty_spacer_may_overflow
                                && !trailing_table_spacer_may_overflow
                                && !explicit_break_spacer_may_overflow
                                && !break_only_paragraph_may_overflow
                            {
                                // `placed` (which borrows `effective_style`) is no
                                // longer needed; drop it before mutating the style
                                // and re-placing at the destination page, whose
                                // max_height differs and re-clamps the height.
                                drop(placed);
                                // #86: if this atomic paragraph owns an absolute
                                // wrap float and moves to a new page before any of
                                // its own content is placed, earlier source-page
                                // text may already have wrapped around that future
                                // float. Replay the page without it before placing
                                // the owner on its destination page.
                                let moves_to_new_page = state.current_col + 1 >= num_cols;
                                if mark_absolute_float_relocation(
                                    paragraph_content_placed,
                                    moves_to_new_page,
                                    block_idx,
                                    replay_block_idx,
                                    floating_images,
                                    &mut relocated_absolute_float_blocks,
                                ) {
                                    page_replay_state.restore(&mut state);
                                    block_idx = replay_block_idx;
                                    continue 'blocks;
                                }
                                if !paragraph_content_placed {
                                    float_checkpoint.restore(&mut state);
                                }
                                if state.current_col + 1 < num_cols {
                                    state.current_col += 1;
                                    state.cursor_y = state.column_top;
                                } else {
                                    state.push_new_page(
                                        block_idx,
                                        &ctx,
                                        PageBreakCause::ParagraphOverflow,
                                    );
                                }
                                // §17.3.1.33: Word suppresses space-before
                                // when an ordinary paragraph is moved to the
                                // top of a page/column by automatic overflow.
                                // Explicit `pageBreakBefore` is handled before
                                // this path and deliberately preserves it.
                                // Re-place with the destination style so the
                                // suppressed spacing is removed from both the
                                // paragraph box and any paragraph-relative
                                // float origin.
                                effective_style.space_before = Pt::ZERO;
                                let destination_para_start_y = state.cursor_y;
                                // Re-register this paragraph's own floats at the
                                // destination when none of its content has landed
                                // yet (§20.4.2).
                                if !paragraph_content_placed {
                                    let col_width = config.columns[state.current_col].width;
                                    let content_x = col_x(state.current_col);
                                    register_destination_paragraph_floats(
                                        &mut state,
                                        floating_images,
                                        floating_shapes,
                                        effective_style.space_before,
                                        content_x,
                                        col_width,
                                    );
                                }
                                // Update para_start_y after page/column change so
                                // floating images use the correct position.
                                para_start_y = destination_para_start_y;
                                effective_style.page_y = state.cursor_y;
                                effective_style.page_x = col_x(state.current_col);
                                effective_style.page_content_width =
                                    config.columns[state.current_col].width;
                                effective_style.page_floats = state.page_floats.clone();
                                let destination_constraints = col_constraints(
                                    state.current_col,
                                    (state.bottom - state.page_top).max(Pt::ZERO),
                                );
                                para = place_paragraph(
                                    chunk,
                                    &destination_constraints,
                                    &effective_style,
                                    ctx.default_line_height,
                                    ctx.measure_text,
                                )
                                .emit_full();
                            }

                            log::debug!(
                                "[layout]   page_chunk[{page_chunk_idx}] col_chunk[{chunk_idx}] placed at y={:.1} x={:.1} height={:.1}",
                                state.cursor_y.raw(),
                                col_x(state.current_col).raw(),
                                para.size.height.raw()
                            );
                            for mut cmd in para.commands {
                                cmd.shift_y(state.cursor_y);
                                cmd.shift_x(col_x(state.current_col));
                                state.current_page.commands.push(cmd);
                            }
                            state.cursor_y += para.size.height;
                        }
                        // #86: mark that content of this paragraph has landed
                        // (either split segments or the atomic block), so a
                        // later block's absolute-float overflow relocates rather
                        // than double-wrapping this already-placed text.
                        paragraph_content_placed |= !chunk.is_empty();
                    }
                    // The page break has been consumed by this non-empty chunk.
                    unresolved_page_break = false;
                }

                // §17.3.3.1: if the paragraph ended with a page break and
                // no non-empty chunk followed, defer the break to the next block.
                if unresolved_page_break {
                    state.pending_page_break = true;
                }

                state.first_on_section_page = false;
                state.prev_borders = style.borders.clone();
                state.prev_space_after = effective_style.space_after;
                state.prev_style_id = effective_style.style_id.clone();
                state.prev_after_auto_spacing = effective_style.after_auto_spacing;
                state.prev_list_spacing_context = effective_style.list_spacing_context;
                state.prev_table_style_id = None; // paragraph breaks adjacent table chain

                // §20.4.2.3: emit non-wrapTopAndBottom floating images.
                // (wrapTopAndBottom images were emitted immediately above.)
                let parity = state.parity();
                for fi in floating_images {
                    if fi.is_wrap_top_and_bottom() {
                        continue;
                    }
                    let img_y = match fi.y {
                        FloatingImageY::Absolute(y) => y,
                        FloatingImageY::RelativeToParagraph(offset) => {
                            para_start_y + effective_style.space_before + offset
                        }
                    };
                    push_floating_command(
                        &mut state,
                        fi.behind_doc,
                        fi.relative_height,
                        DrawCommand::Image {
                            rect: PtRect::from_xywh(
                                fi.x.resolve(parity),
                                img_y,
                                fi.size.width,
                                fi.size.height,
                            ),
                            image_data: fi.image_data.clone(),
                            src_rect: fi.src_rect,
                        },
                    );
                }

                // §20.4.2: emit floating DrawingML shapes after the
                // paragraph's text so they paint on top (for `behindDoc=0`).
                // `wrapTopAndBottom` shapes were emitted pre-layout along
                // with their cursor advance — skip them here.
                for fs in floating_shapes {
                    if fs.is_wrap_top_and_bottom() {
                        continue;
                    }
                    let shape_y = match fs.y {
                        FloatingImageY::Absolute(y) => y,
                        FloatingImageY::RelativeToParagraph(offset) => {
                            para_start_y + effective_style.space_before + offset
                        }
                    };
                    push_floating_command(
                        &mut state,
                        fs.behind_doc,
                        fs.relative_height,
                        DrawCommand::Path {
                            origin: crate::render::geometry::PtOffset::new(
                                fs.x.resolve(parity),
                                shape_y,
                            ),
                            rotation: fs.rotation,
                            flip_h: fs.flip_h,
                            flip_v: fs.flip_v,
                            extent: fs.size,
                            paths: fs.paths.clone(),
                            fill: fs.fill.clone(),
                            stroke: fs.stroke.clone(),
                            effects: fs.effects.clone(),
                        },
                    );
                    emit_shape_text(&mut state, fs, shape_y);
                }

                // Collect footnotes for this page and reduce the available
                // bottom. A split paragraph already reserved them per segment
                // (on the page each reference landed on), so skip here.
                if !footnotes_reserved {
                    reserve_footnotes(&mut state, &ctx, content_width, footnotes);
                }
            }
            LayoutBlock::Table {
                rows,
                col_widths,
                cell_spacing,
                border_config,
                indent,
                alignment,
                float_info,
                style_id,
            } => {
                // §17.4.58: floating table — render and register as a float so
                // subsequent text wraps around it. Floating tables are absolutely
                // positioned and do not participate in adjacent border collapse.
                if let Some(fi) = float_info {
                    // Run an un-paginated layout once to get the table's
                    // width (for x positioning + alignment overrides) and
                    // total height (for the §17.4.58 page-push heuristic).
                    // The actual emission uses `layout_table_paginated`
                    // below so rows that overflow split across pages.
                    let table = layout_table(
                        rows,
                        col_widths,
                        *cell_spacing,
                        ctx.default_line_height,
                        border_config.as_ref(),
                        ctx.measure_text,
                        false,
                    );

                    // §17.4.28 / §17.4.51: compute table x position.
                    let table_x = table_x_offset(
                        *alignment,
                        *indent,
                        table.size.width,
                        content_width,
                        config.margins.left,
                    );
                    // §17.4.58: apply tblpXSpec horizontal alignment override.
                    let table_x = match fi.x_align {
                        Some(crate::model::TableXAlign::Center) => {
                            config.margins.left + (content_width - table.size.width) * 0.5
                        }
                        Some(crate::model::TableXAlign::Right) => {
                            config.margins.left + content_width - table.size.width
                        }
                        _ => table_x,
                    };

                    // §17.4.58: resolve `tblpY` on the current page, then
                    // §17.4.57 resolve collisions with prior floats. Do not
                    // pre-push from the monolithic table height: the paginator
                    // below is the authority on whether a legal first row
                    // group (or split-row fragment) fits at this anchor. When
                    // none fits it emits an empty first slice, and the normal
                    // continuation path advances exactly one page.
                    //
                    // On `Spillover`, push to next page and re-resolve with the
                    // new (empty) float list.
                    //
                    // This loop terminates because `Spillover` requires a
                    // collision-induced shift and `push_new_page` clears
                    // `page_floats`: the retry has no floats to collide
                    // with, so it cannot spill again. A table too tall for
                    // the body comes back as `OnCurrentPage` and is sliced
                    // by the pagination call below, never re-resolved.
                    let float_y_start = loop {
                        let requested_y = if fi.y_offset > Pt::ZERO {
                            let anchor_y = match fi.vert_anchor {
                                // §17.4.58: a floating table's logical block
                                // position is resolved against the next regular
                                // paragraph. At this point that paragraph starts
                                // at the current flow cursor, not at the start of
                                // the preceding paragraph.
                                crate::model::TableAnchor::Text => state.cursor_y + fi.y_offset,
                                crate::model::TableAnchor::Margin => state.page_top + fi.y_offset,
                                crate::model::TableAnchor::Page => fi.y_offset,
                            };
                            anchor_y.max(state.cursor_y)
                        } else {
                            state.cursor_y
                        };

                        match resolve_floating_anchor(
                            requested_y,
                            table.size.height,
                            fi.overlap,
                            &state.page_floats,
                            state.bottom,
                        ) {
                            FloatingTableAnchor::OnCurrentPage(y) => break y,
                            FloatingTableAnchor::Shifted { from, to } => {
                                log::debug!(
                                    "[layout]   shift float past prior (overlap=Never): {:.1} -> {:.1}",
                                    from.raw(),
                                    to.raw(),
                                );
                                break to;
                            }
                            FloatingTableAnchor::Spillover => {
                                log::debug!(
                                    "[layout]   float spill to next page (overlap=Never): block_idx={block_idx}",
                                );
                                state.push_new_page(
                                    block_idx,
                                    &ctx,
                                    PageBreakCause::FloatingTableCollision,
                                );
                                state.prev_space_after = Pt::ZERO;
                                // Loop: re-resolve on the fresh page (empty
                                // float list, cursor at the selected page top).
                            }
                        }
                    };

                    // Floating table breaks the adjacent table chain.
                    state.prev_table_style_id = None;

                    // §17.4.58: paginate at row boundaries when the table
                    // would overflow. First slice gets the anchor page's
                    // remaining height (`bottom - float_y_start`);
                    // continuation slices get the selected body height for
                    // each subsequent page.
                    let available_first = (state.bottom - float_y_start).max(Pt::ZERO);
                    let section_page_index = state.page_index;
                    let slices = layout_table_paginated_with_page_heights(
                        rows,
                        col_widths,
                        *cell_spacing,
                        ctx.default_line_height,
                        border_config.as_ref(),
                        ctx.measure_text,
                        TablePaginationHeights {
                            available_height: available_first,
                            suppress_first_row_top: false,
                            page_height_for_slice: |slice_index| {
                                ctx.page_bounds(section_page_index + slice_index).height()
                            },
                            footnote_width: Some(content_width),
                            footnote_separator_height: FOOTNOTE_SEPARATOR_GAP,
                            first_page_has_footnotes: !state.page_footnotes.is_empty(),
                        },
                    );

                    // §17.4.58: anchor only the first slice; continuation
                    // slices flow at the top of subsequent pages. Encoded
                    // by the `Anchor` / `Continuation` enum variants in
                    // the placement plan.
                    let plan = plan_floating_table_pages_with_page_tops(
                        slices,
                        float_y_start,
                        |slice_index| ctx.page_bounds(section_page_index + slice_index).top,
                    );

                    let table_width = table.size.width;
                    for (page_idx, placement) in plan.pages.into_iter().enumerate() {
                        if page_idx > 0 {
                            state.push_new_page(
                                block_idx,
                                &ctx,
                                PageBreakCause::FloatingTableContinuation,
                            );
                            state.prev_space_after = Pt::ZERO;
                        }

                        let (y_start, slice, is_anchor) = match placement {
                            FloatingTablePagePlacement::Anchor { y_start, slice } => {
                                (y_start, slice, true)
                            }
                            FloatingTablePagePlacement::Continuation { y_start, slice } => {
                                (y_start, slice, false)
                            }
                        };
                        let TableSlice {
                            commands,
                            size,
                            footnotes,
                        } = slice;
                        let slice_height = size.height;

                        for mut cmd in commands {
                            cmd.shift_y(y_start);
                            cmd.shift_x(table_x);
                            state.current_page.commands.push(cmd);
                        }
                        reserve_owned_footnotes(&mut state, &ctx, content_width, footnotes);

                        // §17.4.56 / §17.4.57: register every slice as a
                        // float on its respective page. The anchor slice
                        // drives text wrapping for body paragraphs that
                        // follow; continuation slices are registered so
                        // subsequent floating tables can see them during
                        // collision resolution (§17.4.57 `tblOverlap`).
                        log::debug!(
                            "[layout]   register table float ({}): x={:.1} y={:.1}-{:.1} w={:.1} block_idx={block_idx}",
                            if is_anchor { "anchor" } else { "continuation" },
                            table_x.raw(),
                            y_start.raw(),
                            (y_start + slice_height).raw(),
                            (table_width + fi.right_gap).raw(),
                        );
                        state.page_floats.push(float::ActiveFloat {
                            page_x: table_x,
                            page_y_start: y_start,
                            page_y_end: y_start + slice_height,
                            width: table_width + fi.right_gap,
                            source: float::FloatSource::Table {
                                owner_block_idx: block_idx,
                            },
                            vertical_exclusion: false,
                            // §17.4.58: floating tables default to
                            // bothSides; no dedicated wrapText
                            // attribute exists for tables.
                            wrap_text: float::WrapTextSide::BothSides,
                        });
                        state.column_reactivation_unsafe = true;
                        // Suppress unused warning when `is_anchor` is no
                        // longer the discriminant for registration.
                        let _ = is_anchor;
                    }
                    block_idx += 1;
                    continue;
                }

                let mut moved_for_table_keep_next = false;
                if num_cols == 1
                    && !table_aware_prefix_already_admitted(block_idx, table_aware_admitted_through)
                    && table_keep_next_sentinel(block)
                    && state.page_floats.is_empty()
                {
                    let constraints = col_constraints(
                        state.current_col,
                        (state.bottom - state.page_top).max(Pt::ZERO),
                    );
                    let full_page_height = ctx.page_bounds(state.page_index + 1).height();
                    let fresh_prefix = measure_table_aware_keep_next_prefix(
                        blocks,
                        block_idx,
                        &constraints,
                        ctx.default_line_height,
                        ctx.measure_text,
                        full_page_height,
                        None,
                        None,
                    );
                    if let Some(fresh_prefix) = fresh_prefix {
                        let current_prefix = measure_table_aware_keep_next_prefix(
                            blocks,
                            block_idx,
                            &constraints,
                            ctx.default_line_height,
                            ctx.measure_text,
                            Pt::INFINITY,
                            state.prev_table_style_id.as_ref(),
                            Some(fresh_prefix.end_exclusive),
                        );
                        if let Some(current_prefix) = current_prefix {
                            let current_group_height = current_prefix
                                .group
                                .total_height(!state.page_footnotes.is_empty());
                            let fresh_group_height = fresh_prefix.group.total_height(false);
                            let should_move = fresh_group_height <= full_page_height
                                && current_group_height > state.bottom - state.cursor_y;
                            if should_move && !state.at_full_column_top() {
                                log::debug!(
                                    "[layout] table bridge keepNext prefix {}..{} moved to fresh page",
                                    block_idx,
                                    fresh_prefix.end_exclusive,
                                );
                                state.push_new_page(block_idx, &ctx, PageBreakCause::KeepNextChain);
                                state.prev_space_after = Pt::ZERO;
                                moved_for_table_keep_next = true;
                                table_aware_admitted_through =
                                    table_aware_admitted_through.max(fresh_prefix.end_exclusive);
                            }
                        }
                    }
                }

                // §17.4.38: consecutive non-floating tables with the same style
                // are treated as one merged table — the second table's top border
                // is suppressed so the shared edge is drawn once.
                // A P7 move breaks adjacency at the physical page boundary;
                // fresh-page measurement and actual layout must both restore
                // the second table's own top edge.
                let suppress_top = !moved_for_table_keep_next
                    && style_id.is_some()
                    && *style_id == state.prev_table_style_id;

                // Non-floating table: paginated row-level splitting.
                // §17.4.49 / §17.4.1: split at row boundaries, repeat headers.
                let available = state.bottom - state.cursor_y;
                let section_page_index = state.page_index;
                let starting_col = state.current_col;
                let current_page_height = ctx.page_bounds(section_page_index).height();
                let next_page_height = ctx.page_bounds(section_page_index + 1).height();
                let following_page_height = ctx.page_bounds(section_page_index + 2).height();
                float::prune_floats(&mut state.page_floats, state.cursor_y);
                // A table can continue through the remaining columns of the
                // same physical page only when page-wide state cannot change
                // underneath an already-emitted earlier column.  Footnotes
                // are rendered in one full-page strip, so any existing or
                // table-owned note keeps the historical page-only path.  The
                // equal-height guard also prevents an unsplittable row from
                // being stranded in a short intermediate column when a later
                // parity page would have more room.
                let continue_across_columns = num_cols > 1
                    && page_heights_match(state.column_top, state.page_top)
                    && state.page_footnotes.is_empty()
                    && state.page_floats.is_empty()
                    && !state.column_reactivation_unsafe
                    // A continuous predecessor may already have rendered and
                    // cleared its page-wide notes while retaining the reduced
                    // body bottom. Do not give later columns the unreduced
                    // height in that case.
                    && page_heights_match(
                        state.bottom,
                        ctx.page_bounds(state.page_index).bottom,
                    )
                    // Returning to a column top can revive old wrapping
                    // regions. Only opt in when every flow-affecting float in
                    // the remaining section is a page-absolute TopAndBottom
                    // exclusion, the one kind retained by the per-page
                    // registry above.
                    && !blocks[state.page_start_block..]
                        .iter()
                        .any(layout_block_has_unsupported_column_float)
                    // A later-column paragraph that first introduces a
                    // page-wide footnote would retroactively shrink every
                    // earlier column. Until the paginator can repack already
                    // emitted slots, only use column flow when the remainder
                    // of this section is recursively footnote-free.
                    && !blocks[block_idx..]
                        .iter()
                        .any(layout_block_has_footnotes)
                    && page_heights_match(current_page_height, next_page_height)
                    && page_heights_match(next_page_height, following_page_height);
                let slices = layout_table_paginated_with_page_heights(
                    rows,
                    col_widths,
                    *cell_spacing,
                    ctx.default_line_height,
                    border_config.as_ref(),
                    ctx.measure_text,
                    TablePaginationHeights {
                        available_height: available,
                        suppress_first_row_top: suppress_top,
                        page_height_for_slice: |slice_index| {
                            if continue_across_columns {
                                let absolute_slot = starting_col + slice_index;
                                let page_delta = absolute_slot / num_cols;
                                ctx.page_bounds(section_page_index + page_delta).height()
                            } else {
                                ctx.page_bounds(section_page_index + slice_index).height()
                            }
                        },
                        footnote_width: Some(content_width),
                        footnote_separator_height: FOOTNOTE_SEPARATOR_GAP,
                        first_page_has_footnotes: !state.page_footnotes.is_empty(),
                    },
                );

                // §17.4.28 / §17.4.51: table width is stable, but the x
                // position is resolved per flow region below because unequal
                // columns have different origins and alignment boxes.
                let table_width: Pt = col_widths.iter().copied().sum();

                for (slice_idx, slice) in slices.into_iter().enumerate() {
                    if slice_idx > 0 {
                        if continue_across_columns && state.current_col + 1 < num_cols {
                            state.current_col += 1;
                            state.cursor_y = state.column_top;
                        } else {
                            state.push_new_page(block_idx, &ctx, PageBreakCause::TableContinuation);
                        }
                    }
                    let column = &config.columns[state.current_col];
                    let table_x = table_x_offset(
                        *alignment,
                        *indent,
                        table_width,
                        column.width,
                        col_x(state.current_col),
                    );
                    let TableSlice {
                        commands,
                        size,
                        footnotes,
                    } = slice;
                    for mut cmd in commands {
                        cmd.shift_y(state.cursor_y);
                        cmd.shift_x(table_x);
                        state.current_page.commands.push(cmd);
                    }
                    state.cursor_y += size.height;
                    reserve_owned_footnotes(&mut state, &ctx, content_width, footnotes);
                }
                state.first_on_section_page = false;
                state.prev_borders = None; // table breaks border grouping
                state.prev_space_after = Pt::ZERO;
                state.prev_style_id = None;
                state.prev_after_auto_spacing = false;
                state.prev_list_spacing_context = None;
                state.prev_table_style_id = style_id.clone();
            }
        }
        block_idx += 1;
    }

    // Flush remaining footnotes and push the last page.
    state.finalize(&ctx, preserve_continuation)
}

#[cfg(test)]
mod keep_next_chain_tests {
    use super::*;
    use crate::render::geometry::{PtEdgeInsets, PtSize};
    use crate::render::layout::fragment::{FontProps, TextMetrics};
    use crate::render::layout::page::ColumnGeometry;
    use crate::render::layout::paragraph::{LineSpacingRule, ParagraphStyle};
    use crate::render::layout::table::{CellVAlign, TableCellInput, TableRowInput};
    use crate::render::resolve::color::RgbColor;
    use std::rc::Rc;

    fn paragraph(keep_next: bool, page_break_before: bool) -> LayoutBlock {
        LayoutBlock::Paragraph {
            fragments: Vec::new(),
            style: ParagraphStyle {
                keep_next,
                ..Default::default()
            },
            page_break_before,
            footnotes: Vec::new(),
            floating_images: Vec::new(),
            floating_shapes: Vec::new(),
        }
    }

    fn line_paragraph(keep_next: bool, line_height: f32) -> LayoutBlock {
        LayoutBlock::Paragraph {
            fragments: vec![Fragment::LineBreak {
                line_height: Pt::new(line_height),
                text_height: Pt::new(line_height),
            }],
            style: ParagraphStyle {
                keep_next,
                ..Default::default()
            },
            page_break_before: false,
            footnotes: Vec::new(),
            floating_images: Vec::new(),
            floating_shapes: Vec::new(),
        }
    }

    fn text_paragraph(text: &str, keep_next: bool, line_height: f32) -> LayoutBlock {
        LayoutBlock::Paragraph {
            fragments: vec![Fragment::Text {
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
                width: Pt::new(20.0),
                trimmed_width: Pt::new(20.0),
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
            }],
            style: ParagraphStyle {
                keep_next,
                line_spacing: LineSpacingRule::Exact(Pt::new(line_height)),
                ..Default::default()
            },
            page_break_before: false,
            footnotes: Vec::new(),
            floating_images: Vec::new(),
            floating_shapes: Vec::new(),
        }
    }

    fn auto_three_line_paragraph(space_after: f32) -> LayoutBlock {
        let mut block = text_paragraph("one ", false, 14.0);
        let LayoutBlock::Paragraph {
            fragments, style, ..
        } = &mut block
        else {
            unreachable!();
        };
        fragments.push(match &fragments[0] {
            Fragment::Text { .. } => {
                let mut fragment = fragments[0].clone();
                let Fragment::Text { text, .. } = &mut fragment else {
                    unreachable!();
                };
                *text = "two ".into();
                fragment
            }
            _ => unreachable!(),
        });
        fragments.push(match &fragments[0] {
            Fragment::Text { .. } => {
                let mut fragment = fragments[0].clone();
                let Fragment::Text { text, .. } = &mut fragment else {
                    unreachable!();
                };
                *text = "three".into();
                fragment
            }
            _ => unreachable!(),
        });
        for fragment in fragments.iter_mut() {
            let Fragment::Text {
                width,
                trimmed_width,
                metrics,
                ..
            } = fragment
            else {
                unreachable!();
            };
            *width = Pt::new(100.0);
            *trimmed_width = Pt::new(100.0);
            metrics.leading = Pt::ZERO;
        }
        style.line_spacing = LineSpacingRule::Auto(1.15);
        style.space_after = Pt::new(space_after);
        style.widow_control = true;
        block
    }

    fn cell(blocks: Vec<LayoutBlock>) -> TableCellInput {
        TableCellInput {
            blocks,
            margins: PtEdgeInsets::ZERO,
            grid_span: 1,
            shading: None,
            cell_borders: None,
            vertical_merge: None,
            vertical_align: CellVAlign::Top,
            text_direction: None,
        }
    }

    fn row(cells: Vec<TableCellInput>) -> TableRowInput {
        TableRowInput {
            cells,
            height_rule: None,
            is_header: None,
            cant_split: None,
            grid_before: 0,
            border_overrides: None,
        }
    }

    fn table(rows: Vec<TableRowInput>) -> LayoutBlock {
        LayoutBlock::Table {
            rows,
            col_widths: vec![Pt::new(100.0)],
            cell_spacing: Pt::ZERO,
            border_config: None,
            indent: Pt::ZERO,
            alignment: None,
            float_info: None,
            style_id: None,
        }
    }

    fn text_bridge_table(text: &str, line_height: f32) -> LayoutBlock {
        table(vec![row(vec![cell(vec![text_paragraph(
            text,
            true,
            line_height,
        )])])])
    }

    fn top_and_bottom_owner(space_after: f32, wrap_mode: WrapMode) -> LayoutBlock {
        let mut block = text_paragraph("owner", false, 10.0);
        let LayoutBlock::Paragraph {
            style,
            floating_shapes,
            ..
        } = &mut block
        else {
            unreachable!();
        };
        style.space_before = Pt::new(5.0);
        style.space_after = Pt::new(space_after);
        floating_shapes.push(FloatingShape {
            x: crate::render::layout::section::FloatingImageX::Absolute(Pt::ZERO),
            y: FloatingImageY::Absolute(Pt::new(30.0)),
            size: PtSize::new(Pt::new(180.0), Pt::new(10.0)),
            rotation: crate::model::dimension::Dimension::new(0),
            flip_h: false,
            flip_v: false,
            wrap_mode,
            dist_top: Pt::ZERO,
            dist_bottom: Pt::ZERO,
            dist_left: Pt::ZERO,
            dist_right: Pt::ZERO,
            behind_doc: true,
            relative_height: 0,
            paths: Vec::new(),
            fill: crate::render::layout::draw_command::ResolvedFill::None,
            stroke: None,
            effects: Vec::new(),
            text_commands: Vec::new(),
        });
        block
    }

    fn text_y(page: &LayoutedPage, expected: &str) -> Pt {
        page.commands
            .iter()
            .find_map(|command| match command {
                DrawCommand::Text { position, text, .. } if text.as_ref() == expected => {
                    Some(position.y)
                }
                _ => None,
            })
            .expect("expected text command")
    }

    fn text_x(page: &LayoutedPage, expected: &str) -> Pt {
        page.commands
            .iter()
            .find_map(|command| match command {
                DrawCommand::Text { position, text, .. } if text.as_ref() == expected => {
                    Some(position.x)
                }
                _ => None,
            })
            .expect("expected text command")
    }

    fn text_location(pages: &[LayoutedPage], expected: &str) -> Option<(usize, Pt)> {
        pages.iter().enumerate().find_map(|(page_idx, page)| {
            page.commands.iter().find_map(|command| match command {
                DrawCommand::Text { position, text, .. } if text.as_ref() == expected => {
                    Some((page_idx, position.x))
                }
                _ => None,
            })
        })
    }

    fn small_page_config() -> PageConfig {
        PageConfig {
            page_size: PtSize::new(Pt::new(200.0), Pt::new(100.0)),
            margins: PtEdgeInsets::new(Pt::new(10.0), Pt::new(10.0), Pt::new(10.0), Pt::new(10.0)),
            header_margin: Pt::new(5.0),
            footer_margin: Pt::new(5.0),
            columns: vec![ColumnGeometry {
                x_offset: Pt::ZERO,
                width: Pt::new(180.0),
            }],
        }
    }

    fn two_column_page_config() -> PageConfig {
        PageConfig {
            page_size: PtSize::new(Pt::new(220.0), Pt::new(100.0)),
            margins: PtEdgeInsets::new(Pt::new(10.0), Pt::new(10.0), Pt::new(10.0), Pt::new(10.0)),
            header_margin: Pt::new(5.0),
            footer_margin: Pt::new(5.0),
            columns: vec![
                ColumnGeometry {
                    x_offset: Pt::ZERO,
                    width: Pt::new(80.0),
                },
                ColumnGeometry {
                    x_offset: Pt::new(110.0),
                    width: Pt::new(80.0),
                },
            ],
        }
    }

    fn page_has_text(page: &LayoutedPage, expected: &str) -> bool {
        page.commands.iter().any(|command| {
            matches!(command, DrawCommand::Text { text, .. } if text.as_ref() == expected)
        })
    }

    #[test]
    fn page_break_before_starts_a_new_keep_next_chain() {
        let blocks = [paragraph(true, false), paragraph(true, true)];

        assert!(starts_keep_next_chain(&blocks, 1));
    }

    #[test]
    fn table_sentinel_is_last_row_first_cell_first_block_only() {
        let positive = table(vec![
            row(vec![cell(vec![paragraph(false, false)])]),
            row(vec![cell(vec![paragraph(true, false)])]),
        ]);
        assert!(table_keep_next_sentinel(&positive));

        let second_cell_only = table(vec![row(vec![
            cell(vec![paragraph(false, false)]),
            cell(vec![paragraph(true, false)]),
        ])]);
        assert!(!table_keep_next_sentinel(&second_cell_only));

        let later_block_only = table(vec![row(vec![cell(vec![
            paragraph(false, false),
            paragraph(true, false),
        ])])]);
        assert!(!table_keep_next_sentinel(&later_block_only));
    }

    #[test]
    fn floating_table_never_exports_a_body_keep_next_sentinel() {
        let mut floating = table(vec![row(vec![cell(vec![paragraph(true, false)])])]);
        let LayoutBlock::Table { float_info, .. } = &mut floating else {
            unreachable!();
        };
        *float_info = Some(crate::render::layout::section::TableFloatInfo {
            right_gap: Pt::ZERO,
            bottom_gap: Pt::ZERO,
            x_align: None,
            y_offset: Pt::ZERO,
            vert_anchor: crate::model::TableAnchor::Text,
            overlap: None,
        });

        assert!(!table_keep_next_sentinel(&floating));
        assert!(layout_block_has_unsupported_column_float(&floating));
    }

    #[test]
    fn complex_table_content_falls_back_from_complete_bridge_measurement() {
        let with_footnote = table(vec![row(vec![cell(vec![LayoutBlock::Paragraph {
            fragments: Vec::new(),
            style: ParagraphStyle {
                keep_next: true,
                ..Default::default()
            },
            page_break_before: false,
            footnotes: vec![LayoutFootnote {
                paragraphs: vec![(Vec::new(), ParagraphStyle::default())],
            }],
            floating_images: Vec::new(),
            floating_shapes: Vec::new(),
        }])])]);
        assert!(!table_is_simple_keep_next_bridge(&with_footnote));

        let nested = table(vec![row(vec![cell(vec![table(Vec::new())])])]);
        assert!(!table_is_simple_keep_next_bridge(&nested));
    }

    #[test]
    fn table_aware_prefix_measures_complete_bridge_and_terminal_paragraph() {
        let blocks = vec![
            line_paragraph(true, 10.0),
            table(vec![row(vec![cell(vec![line_paragraph(true, 20.0)])])]),
            line_paragraph(false, 12.0),
        ];
        let constraints = BoxConstraints::tight_width(Pt::new(100.0), Pt::INFINITY);
        let prefix = measure_table_aware_keep_next_prefix(
            &blocks,
            0,
            &constraints,
            Pt::new(14.0),
            None,
            Pt::new(100.0),
            None,
            None,
        )
        .expect("a paragraph-table-paragraph bridge is measurable");

        assert_eq!(prefix.end_exclusive, blocks.len());
        assert_eq!(prefix.group.body_height, Pt::new(42.0));
    }

    #[test]
    fn over_page_chain_may_end_at_a_complete_bridge_table() {
        let blocks = vec![
            table(vec![row(vec![cell(vec![line_paragraph(true, 14.0)])])]),
            line_paragraph(false, 80.0),
        ];
        let constraints = BoxConstraints::tight_width(Pt::new(100.0), Pt::INFINITY);
        let prefix = measure_table_aware_keep_next_prefix(
            &blocks,
            0,
            &constraints,
            Pt::new(14.0),
            None,
            Pt::new(20.0),
            None,
            None,
        )
        .expect("the complete first bridge is the safe maximal prefix");

        assert_eq!(prefix.end_exclusive, 1);
        assert_eq!(prefix.group.body_height, Pt::new(14.0));
    }

    #[test]
    fn first_bridge_without_successor_or_that_exceeds_a_page_falls_back() {
        let bridge = || table(vec![row(vec![cell(vec![line_paragraph(true, 14.0)])])]);
        let constraints = BoxConstraints::tight_width(Pt::new(100.0), Pt::INFINITY);

        assert!(measure_table_aware_keep_next_prefix(
            &[bridge()],
            0,
            &constraints,
            Pt::new(14.0),
            None,
            Pt::new(100.0),
            None,
            None,
        )
        .is_none());

        assert!(measure_table_aware_keep_next_prefix(
            &[bridge(), line_paragraph(false, 10.0)],
            0,
            &constraints,
            Pt::new(14.0),
            None,
            Pt::new(5.0),
            None,
            None,
        )
        .is_none());

        let mut hard_successor = line_paragraph(false, 10.0);
        let LayoutBlock::Paragraph {
            page_break_before, ..
        } = &mut hard_successor
        else {
            unreachable!();
        };
        *page_break_before = true;
        assert!(measure_table_aware_keep_next_prefix(
            &[bridge(), hard_successor],
            0,
            &constraints,
            Pt::new(14.0),
            None,
            Pt::new(100.0),
            None,
            None,
        )
        .is_none());
    }

    #[test]
    fn admitted_prefix_suppresses_every_contained_block_preflight() {
        // A paragraph immediately after a bridge table looks like a new chain
        // to `starts_keep_next_chain` (the previous block is not a paragraph).
        // The admitted interval, not the block kind, is what prevents that
        // paragraph and later bridge tables from moving the same prefix twice.
        let admitted_through = 9;
        for block_idx in 4..admitted_through {
            assert!(table_aware_prefix_already_admitted(
                block_idx,
                admitted_through
            ));
        }
        assert!(!table_aware_prefix_already_admitted(
            admitted_through,
            admitted_through
        ));
    }

    #[test]
    fn paragraph_bridge_terminal_prefix_moves_together_to_fresh_page() {
        let blocks = vec![
            text_paragraph("filler", false, 50.0),
            text_paragraph("head", true, 14.0),
            text_bridge_table("bridge", 14.0),
            text_paragraph("terminal", false, 14.0),
        ];
        let pages = layout_section(
            &blocks,
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(14.0),
            None,
        );

        assert_eq!(pages.len(), 2);
        for expected in ["head", "bridge", "terminal"] {
            assert!(!page_has_text(&pages[0], expected));
            assert!(page_has_text(&pages[1], expected));
        }
    }

    #[test]
    fn bridge_table_can_start_body_admission() {
        let blocks = vec![
            text_paragraph("filler", false, 60.0),
            text_bridge_table("bridge", 14.0),
            text_paragraph("terminal", false, 14.0),
        ];
        let pages = layout_section(
            &blocks,
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(14.0),
            None,
        );

        assert_eq!(pages.len(), 2);
        for expected in ["bridge", "terminal"] {
            assert!(!page_has_text(&pages[0], expected));
            assert!(page_has_text(&pages[1], expected));
        }
    }

    #[test]
    fn admitted_long_prefix_does_not_move_again_after_its_first_table() {
        let blocks = vec![
            text_paragraph("filler", false, 50.0),
            text_paragraph("head", true, 21.0),
            text_bridge_table("bridge-a", 21.0),
            text_paragraph("mid", true, 14.0),
            text_bridge_table("bridge-b", 14.0),
            text_paragraph("terminal", false, 14.0),
        ];
        let pages = layout_section(
            &blocks,
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(14.0),
            None,
        );

        assert_eq!(pages.len(), 3);
        assert!(page_has_text(&pages[1], "mid"));
        assert!(page_has_text(&pages[1], "bridge-b"));
        assert!(!page_has_text(&pages[2], "mid"));
        assert!(page_has_text(&pages[2], "terminal"));
    }

    #[test]
    fn trailing_space_entering_owner_top_bottom_band_moves_single_line_below_it() {
        let no_collision = layout_section(
            &[top_and_bottom_owner(4.0, WrapMode::TopAndBottom)],
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(10.0),
            None,
        );
        let collision = layout_section(
            &[top_and_bottom_owner(15.0, WrapMode::TopAndBottom)],
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(10.0),
            None,
        );

        assert_eq!(no_collision.len(), 1);
        assert_eq!(collision.len(), 1);
        assert_eq!(
            text_y(&collision[0], "owner") - text_y(&no_collision[0], "owner"),
            Pt::new(30.0),
            "the paragraph box starts at the 40pt band bottom, preserving space_before once"
        );
    }

    #[test]
    fn trailing_space_does_not_clear_past_a_wrap_none_owner() {
        let plain = layout_section(
            &[top_and_bottom_owner(4.0, WrapMode::None)],
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(10.0),
            None,
        );
        let large_after = layout_section(
            &[top_and_bottom_owner(15.0, WrapMode::None)],
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(10.0),
            None,
        );

        assert_eq!(text_y(&plain[0], "owner"), text_y(&large_after[0], "owner"));
    }

    #[test]
    fn final_auto_text_baseline_can_fit_without_charging_space_after() {
        let pages = layout_section(
            &[
                text_paragraph("filler", false, 37.8),
                auto_three_line_paragraph(0.0),
                text_paragraph("next", false, 10.0),
            ],
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(14.0),
            None,
        );

        assert_eq!(pages.len(), 2);
        for expected in ["one ", "two ", "three"] {
            assert!(
                page_has_text(&pages[0], expected),
                "{expected:?} stays on page 1"
            );
        }
        assert!(page_has_text(&pages[1], "next"));
    }

    #[test]
    fn final_auto_text_baseline_that_exceeds_the_page_still_moves_whole() {
        let pages = layout_section(
            &[
                text_paragraph("filler", false, 37.9),
                auto_three_line_paragraph(0.0),
            ],
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(14.0),
            None,
        );

        assert_eq!(pages.len(), 2);
        assert!(!page_has_text(&pages[0], "one "));
        assert!(page_has_text(&pages[1], "one "));
    }

    #[test]
    fn final_auto_text_baseline_does_not_charge_glyph_descent() {
        let mut paragraph = auto_three_line_paragraph(0.0);
        let LayoutBlock::Paragraph { fragments, .. } = &mut paragraph else {
            unreachable!();
        };
        let Fragment::Text { metrics, .. } = &mut fragments[2] else {
            unreachable!();
        };
        metrics.descent = Pt::new(4.2);

        let pages = layout_section(
            &[text_paragraph("filler", false, 37.8), paragraph],
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(14.0),
            None,
        );

        assert_eq!(pages.len(), 1);
        for expected in ["one ", "two ", "three"] {
            assert!(
                page_has_text(&pages[0], expected),
                "{expected:?} stays on page 1 even though its glyph descent crosses the boundary"
            );
        }
    }

    #[test]
    fn space_after_does_not_decide_whether_the_final_line_fits() {
        let layout = |space_after| {
            let mut paragraph = auto_three_line_paragraph(space_after);
            let LayoutBlock::Paragraph { style, .. } = &mut paragraph else {
                unreachable!();
            };
            style.line_spacing = LineSpacingRule::Exact(Pt::new(14.0));
            layout_section(
                &[
                    text_paragraph("filler", false, 38.0),
                    paragraph,
                    text_paragraph("next", false, 10.0),
                ],
                &small_page_config(),
                None,
                Pt::ZERO,
                Pt::new(14.0),
                None,
            )
        };
        let no_after = layout(0.0);
        let large_after = layout(50.0);

        for pages in [&no_after, &large_after] {
            assert_eq!(pages.len(), 2);
            for expected in ["one ", "two ", "three"] {
                assert!(
                    page_has_text(&pages[0], expected),
                    "{expected:?} stays on page 1"
                );
            }
            assert!(page_has_text(&pages[1], "next"));
        }
        assert_eq!(
            text_y(&no_after[1], "next"),
            text_y(&large_after[1], "next"),
            "trailing space is discarded at the physical page boundary"
        );
    }

    #[test]
    fn exact_spacing_keeps_the_full_final_line_box_for_page_fit() {
        let mut paragraph = auto_three_line_paragraph(0.0);
        let LayoutBlock::Paragraph { style, .. } = &mut paragraph else {
            unreachable!();
        };
        style.line_spacing = LineSpacingRule::Exact(Pt::new(16.1));
        let pages = layout_section(
            &[text_paragraph("filler", false, 33.0), paragraph],
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(14.0),
            None,
        );

        assert_eq!(pages.len(), 2);
        assert!(!page_has_text(&pages[0], "one "));
        assert!(page_has_text(&pages[1], "one "));
    }

    #[test]
    fn auto_one_keeps_the_full_final_line_box_for_page_fit() {
        let mut paragraph = auto_three_line_paragraph(0.0);
        let LayoutBlock::Paragraph { style, .. } = &mut paragraph else {
            unreachable!();
        };
        style.line_spacing = LineSpacingRule::Auto(1.0);
        let pages = layout_section(
            &[text_paragraph("filler", false, 40.0), paragraph],
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(14.0),
            None,
        );

        assert_eq!(pages.len(), 2);
        assert!(!page_has_text(&pages[0], "one "));
        assert!(page_has_text(&pages[1], "one "));
    }

    #[test]
    fn natural_only_final_text_keeps_the_full_line_box_for_page_fit() {
        let mut paragraph = auto_three_line_paragraph(0.0);
        let LayoutBlock::Paragraph { fragments, .. } = &mut paragraph else {
            unreachable!();
        };
        let Fragment::Text { font, .. } = &mut fragments[2] else {
            unreachable!();
        };
        Rc::make_mut(font).auto_line_spacing =
            crate::render::layout::fragment::AutoLineSpacingContribution::NaturalOnly;
        let pages = layout_section(
            &[text_paragraph("filler", false, 34.0), paragraph],
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(14.0),
            None,
        );

        assert_eq!(pages.len(), 2);
        assert!(!page_has_text(&pages[0], "one "));
        assert!(page_has_text(&pages[1], "one "));
    }

    #[test]
    fn underlined_final_auto_text_keeps_the_full_line_box_for_page_fit() {
        let mut paragraph = auto_three_line_paragraph(0.0);
        let LayoutBlock::Paragraph { fragments, .. } = &mut paragraph else {
            unreachable!();
        };
        let Fragment::Text { font, .. } = &mut fragments[2] else {
            unreachable!();
        };
        Rc::make_mut(font).underline = true;
        let pages = layout_section(
            &[text_paragraph("filler", false, 33.0), paragraph],
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(14.0),
            None,
        );

        assert_eq!(pages.len(), 2);
        assert!(!page_has_text(&pages[0], "one "));
        assert!(page_has_text(&pages[1], "one "));
    }

    #[test]
    fn a_footnote_on_an_earlier_line_disables_final_line_baseline_admission() {
        let mut paragraph = auto_three_line_paragraph(0.0);
        let LayoutBlock::Paragraph {
            fragments,
            footnotes,
            ..
        } = &mut paragraph
        else {
            unreachable!();
        };
        let Fragment::Text {
            is_footnote_ref, ..
        } = &mut fragments[0]
        else {
            unreachable!();
        };
        *is_footnote_ref = true;
        footnotes.push(LayoutFootnote {
            paragraphs: vec![(Vec::new(), ParagraphStyle::default())],
        });
        let pages = layout_section(
            &[text_paragraph("filler", false, 33.0), paragraph],
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(14.0),
            None,
        );

        assert_eq!(pages.len(), 2);
        assert!(!page_has_text(&pages[0], "one "));
        assert!(page_has_text(&pages[1], "one "));
    }

    #[test]
    fn final_auto_text_baseline_uses_the_lowest_visible_run() {
        let mut paragraph = auto_three_line_paragraph(0.0);
        let LayoutBlock::Paragraph { fragments, .. } = &mut paragraph else {
            unreachable!();
        };
        let mut lower_run = fragments[2].clone();
        let Fragment::Text {
            text,
            width,
            trimmed_width,
            baseline_offset,
            ..
        } = &mut lower_run
        else {
            unreachable!();
        };
        *text = " lower".into();
        *width = Pt::new(20.0);
        *trimmed_width = Pt::new(20.0);
        *baseline_offset = Pt::new(0.1);
        fragments.push(lower_run);

        let pages = layout_section(
            &[text_paragraph("filler", false, 37.8), paragraph],
            &small_page_config(),
            None,
            Pt::ZERO,
            Pt::new(14.0),
            None,
        );

        assert_eq!(pages.len(), 2);
        assert!(!page_has_text(&pages[0], "one "));
        assert!(page_has_text(&pages[1], "one "));
    }

    #[test]
    fn footnote_free_table_continues_in_the_next_column_before_a_new_page() {
        let blocks = vec![
            text_paragraph("filler", false, 50.0),
            table(vec![
                row(vec![cell(vec![text_paragraph("row-a", false, 25.0)])]),
                row(vec![cell(vec![text_paragraph("row-b", false, 25.0)])]),
            ]),
        ];

        let pages = layout_section(
            &blocks,
            &two_column_page_config(),
            None,
            Pt::ZERO,
            Pt::new(10.0),
            None,
        );

        assert_eq!(pages.len(), 1);
        assert!(text_x(&pages[0], "row-a") < Pt::new(100.0));
        assert!(text_x(&pages[0], "row-b") > Pt::new(100.0));
        assert!(text_y(&pages[0], "row-b") < text_y(&pages[0], "row-a"));
    }

    #[test]
    fn table_with_a_footnote_keeps_the_historical_page_only_continuation() {
        let mut noted = text_paragraph("row-a", false, 25.0);
        let LayoutBlock::Paragraph { footnotes, .. } = &mut noted else {
            unreachable!();
        };
        footnotes.push(LayoutFootnote {
            paragraphs: vec![(Vec::new(), ParagraphStyle::default())],
        });
        let pages = layout_section(
            &[
                text_paragraph("filler", false, 50.0),
                table(vec![
                    row(vec![cell(vec![noted])]),
                    row(vec![cell(vec![text_paragraph("row-b", false, 25.0)])]),
                ]),
            ],
            &two_column_page_config(),
            None,
            Pt::ZERO,
            Pt::new(10.0),
            None,
        );

        assert_eq!(pages.len(), 2);
        let row_b = text_location(&pages, "row-b").expect("row-b must be emitted");
        assert_eq!(row_b.0, 1);
        assert!(row_b.1 < Pt::new(100.0));
    }

    #[test]
    fn table_before_a_later_footnote_also_keeps_page_only_continuation() {
        let mut noted = text_paragraph("after-note", false, 10.0);
        let LayoutBlock::Paragraph { footnotes, .. } = &mut noted else {
            unreachable!();
        };
        footnotes.push(LayoutFootnote {
            paragraphs: vec![(Vec::new(), ParagraphStyle::default())],
        });
        let pages = layout_section(
            &[
                text_paragraph("filler", false, 50.0),
                table(vec![
                    row(vec![cell(vec![text_paragraph("row-a", false, 25.0)])]),
                    row(vec![cell(vec![text_paragraph("row-b", false, 25.0)])]),
                ]),
                noted,
            ],
            &two_column_page_config(),
            None,
            Pt::ZERO,
            Pt::new(10.0),
            None,
        );

        let row_b = text_location(&pages, "row-b").expect("row-b must be emitted");
        assert_eq!(row_b.0, 1, "a later page-wide note forbids column slicing");
        assert!(row_b.1 < Pt::new(100.0));
    }

    #[test]
    fn table_continuing_from_the_last_column_uses_next_page_first_column() {
        let mut column_break = paragraph(false, false);
        let LayoutBlock::Paragraph { fragments, .. } = &mut column_break else {
            unreachable!();
        };
        fragments.push(Fragment::ColumnBreak);
        let pages = layout_section(
            &[
                column_break,
                text_paragraph("filler", false, 40.0),
                table(vec![
                    row(vec![cell(vec![text_paragraph("row-a", false, 25.0)])]),
                    row(vec![cell(vec![text_paragraph("row-b", false, 25.0)])]),
                ]),
            ],
            &two_column_page_config(),
            None,
            Pt::ZERO,
            Pt::new(10.0),
            None,
        );

        assert_eq!(pages.len(), 2);
        let row_a = text_location(&pages, "row-a").expect("row-a must be emitted");
        let row_b = text_location(&pages, "row-b").expect("row-b must be emitted");
        assert_eq!((row_a.0, row_b.0), (0, 1), "locations={row_a:?}/{row_b:?}");
        assert!(row_a.1 > Pt::new(100.0));
        assert!(row_b.1 < Pt::new(100.0));
    }

    #[test]
    fn column_table_float_gate_only_accepts_absolute_top_bottom_exclusions() {
        let absolute_top_bottom = top_and_bottom_owner(0.0, WrapMode::TopAndBottom);
        assert!(!layout_block_has_unsupported_column_float(
            &absolute_top_bottom
        ));

        let mut relative_top_bottom = top_and_bottom_owner(0.0, WrapMode::TopAndBottom);
        let LayoutBlock::Paragraph {
            floating_shapes, ..
        } = &mut relative_top_bottom
        else {
            unreachable!();
        };
        floating_shapes[0].y = FloatingImageY::RelativeToParagraph(Pt::ZERO);
        assert!(layout_block_has_unsupported_column_float(
            &relative_top_bottom
        ));

        let absolute_side_wrap =
            top_and_bottom_owner(0.0, WrapMode::Square(crate::model::WrapText::BothSides));
        assert!(layout_block_has_unsupported_column_float(
            &absolute_side_wrap
        ));

        let overlay = top_and_bottom_owner(0.0, WrapMode::None);
        assert!(!layout_block_has_unsupported_column_float(&overlay));
    }

    #[test]
    fn absolute_top_bottom_shape_reactivates_after_same_page_column_reset() {
        let layout = |wrap_mode| {
            let mut owner = top_and_bottom_owner(0.0, wrap_mode);
            let LayoutBlock::Paragraph {
                fragments, style, ..
            } = &mut owner
            else {
                unreachable!();
            };
            fragments.clear();
            style.space_before = Pt::ZERO;
            style.space_after = Pt::ZERO;
            let LayoutBlock::Paragraph {
                floating_shapes, ..
            } = &mut owner
            else {
                unreachable!();
            };
            floating_shapes[0].y = FloatingImageY::Absolute(Pt::new(40.0));

            let mut filler = text_paragraph("filler", false, 10.0);
            let LayoutBlock::Paragraph { style, .. } = &mut filler else {
                unreachable!();
            };
            style.space_after = Pt::new(46.0);
            let mut config = two_column_page_config();
            config.page_size.height = Pt::new(120.0);

            layout_section(
                &[
                    owner,
                    filler,
                    table(vec![
                        row(vec![cell(vec![text_paragraph("row-a", false, 25.0)])]),
                        row(vec![cell(vec![text_paragraph("row-b", false, 25.0)])]),
                    ]),
                    text_paragraph("after", false, 10.0),
                ],
                &config,
                None,
                Pt::ZERO,
                Pt::new(10.0),
                None,
            )
        };

        let without_band = layout(WrapMode::None);
        let with_band = layout(WrapMode::TopAndBottom);

        assert_eq!(without_band.len(), 1);
        assert_eq!(with_band.len(), 1);
        assert!(text_x(&with_band[0], "row-b") > Pt::new(100.0));
        assert!(
            text_y(&with_band[0], "after") > text_y(&without_band[0], "after"),
            "the page-absolute exclusion must remain active after returning to the column top"
        );
    }
}

/// §17.3.1.14 / §17.3.1.15 — `paragraph_breakable`, the predicate shared by the
/// placement gate and the keepNext predictor. Extracted precisely because the
/// two had drifted apart on the footnote condition (E4a#3).
#[cfg(test)]
mod paragraph_breakable_tests {
    use super::*;
    use crate::render::layout::paragraph::ParagraphStyle;

    fn footnote() -> LayoutFootnote {
        LayoutFootnote {
            paragraphs: vec![(Vec::new(), ParagraphStyle::default())],
        }
    }

    fn plain() -> ParagraphStyle {
        ParagraphStyle::default()
    }

    #[test]
    fn a_plain_paragraph_is_breakable() {
        assert!(paragraph_breakable(&plain(), &[], &[], &[], true));
        assert!(paragraph_breakable(&plain(), &[], &[], &[], false));
    }

    #[test]
    fn keep_lines_forbids_breaking() {
        let style = ParagraphStyle {
            keep_lines: true,
            ..Default::default()
        };
        assert!(!paragraph_breakable(&style, &[], &[], &[], true));
    }

    /// **The condition the keepNext predictor was missing.** Footnotes are
    /// reserved per segment, but only for a single unbroken chunk — with
    /// explicit page/column breaks a reference's segment is ambiguous.
    ///
    /// Both directions matter: footnotes on one chunk stay breakable (or every
    /// footnote-bearing paragraph would become atomic), and footnotes across
    /// several chunks do not.
    #[test]
    fn footnotes_are_breakable_only_on_a_single_chunk() {
        let notes = [footnote()];
        assert!(
            paragraph_breakable(&plain(), &notes, &[], &[], true),
            "footnotes on one unbroken chunk may still split"
        );
        assert!(
            !paragraph_breakable(&plain(), &notes, &[], &[], false),
            "footnotes spanning page/column chunks keep the atomic reservation"
        );
    }

    /// `single_chunk` is irrelevant without footnotes — it must not become a
    /// blanket restriction on multi-chunk paragraphs.
    #[test]
    fn single_chunk_only_matters_when_footnotes_are_present() {
        assert!(paragraph_breakable(&plain(), &[], &[], &[], false));
    }

    // ── The predictor's own derivation of `single_chunk` ─────────────────

    use crate::render::geometry::PtSize;
    use crate::render::layout::fragment::{FontProps, TextMetrics};
    use crate::render::resolve::color::RgbColor;
    use std::rc::Rc;

    fn text_frag(text: &str) -> Fragment {
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
            width: Pt::new(30.0),
            trimmed_width: Pt::new(30.0),
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

    fn page_break() -> Fragment {
        Fragment::PageBreak {
            line_height: Pt::new(14.0),
        }
    }

    /// Six wrapping lines, optionally interrupted by `brk`, optionally carrying
    /// a footnote.
    fn keep_next_head(brk: Option<Fragment>, with_footnote: bool) -> LayoutBlock {
        let mut fragments: Vec<Fragment> = (0..3).map(|i| text_frag(&format!("L{i} "))).collect();
        if let Some(b) = brk {
            fragments.push(b);
        }
        fragments.extend((3..6).map(|i| text_frag(&format!("L{i} "))));
        LayoutBlock::Paragraph {
            fragments,
            style: ParagraphStyle {
                keep_next: true,
                ..Default::default()
            },
            page_break_before: false,
            footnotes: if with_footnote {
                vec![footnote()]
            } else {
                Vec::new()
            },
            floating_images: Vec::new(),
            floating_shapes: Vec::new(),
        }
    }

    fn splittable(block: &LayoutBlock) -> bool {
        // Width 40 against 30pt fragments → one line each.
        let constraints = BoxConstraints::loose(PtSize::new(Pt::new(40.0), Pt::new(1000.0)));
        leading_keep_next_paragraph_splittable(block, &constraints, Pt::new(14.0), None)
    }

    /// The predictor must derive `single_chunk` itself, not assume it.
    ///
    /// A keepNext-chain head carrying footnotes **across a page break** is not
    /// splittable: the placement gate will refuse it, so reporting it splittable
    /// skips the whole-group move and then falls back to atomic anyway — an
    /// under-filled page. Hard-coding `single_chunk = true` here is precisely
    /// the bug this closes, and only this test sees it.
    #[test]
    fn footnote_bearing_head_with_an_internal_break_is_not_splittable() {
        // §17.3.3.1 page break and §17.6.3 column break both end a chunk, so
        // both must be consulted — the derivation is an `&&` of two splitters
        // and testing only one leaves half of it unverified.
        for (label, brk) in [
            ("page break", page_break()),
            ("column break", Fragment::ColumnBreak),
        ] {
            assert!(
                !splittable(&keep_next_head(Some(brk), true)),
                "footnotes + an internal {label} → atomic"
            );
        }
    }

    /// The controls, so the test above can't pass for the wrong reason: each
    /// ingredient alone still permits a split.
    #[test]
    fn a_break_or_a_footnote_alone_still_permits_a_split() {
        assert!(
            splittable(&keep_next_head(None, true)),
            "footnotes on a single chunk are fine"
        );
        assert!(
            splittable(&keep_next_head(Some(page_break()), false)),
            "a page break without footnotes is fine"
        );
        assert!(
            splittable(&keep_next_head(Some(Fragment::ColumnBreak), false)),
            "a column break without footnotes is fine"
        );
        assert!(
            splittable(&keep_next_head(None, false)),
            "plain multi-line head is splittable"
        );
    }
}

#[cfg(test)]
mod paragraph_split_tests {
    use super::{decide_paragraph_split, prefix_adjusted_head, ParagraphSplit};

    // ── Whole paragraph fits ──────────────────────────────────────────────

    #[test]
    fn all_lines_fit_is_all() {
        assert_eq!(
            decide_paragraph_split(5, 5, true, false),
            ParagraphSplit::All
        );
        // More space than needed also counts as fitting.
        assert_eq!(
            decide_paragraph_split(9, 5, true, false),
            ParagraphSplit::All
        );
    }

    // ── Widow/orphan control on (§17.3.1.44) ──────────────────────────────

    #[test]
    fn widow_control_keeps_two_lines_on_each_side() {
        // 6 lines, 4 fit: place 4, carry 2 — both sides satisfy the >= 2 rule.
        assert_eq!(
            decide_paragraph_split(4, 6, true, false),
            ParagraphSplit::Break { head: 4 }
        );
    }

    #[test]
    fn widow_control_caps_head_to_leave_a_non_widow_tail() {
        // 4 lines, 3 fit: placing 3 would strand 1 (widow), so cap head to 2.
        assert_eq!(
            decide_paragraph_split(3, 4, true, false),
            ParagraphSplit::Break { head: 2 }
        );
    }

    #[test]
    fn widow_control_rejects_single_orphan_line_and_moves_whole() {
        // Only 1 line fits: an orphan. Not at page top → move the whole para.
        assert_eq!(
            decide_paragraph_split(1, 6, true, false),
            ParagraphSplit::MoveWhole
        );
    }

    #[test]
    fn widow_control_three_line_paragraph_cannot_split() {
        // No head in 1..=2 leaves >= 2 on both sides → move whole.
        assert_eq!(
            decide_paragraph_split(2, 3, true, false),
            ParagraphSplit::MoveWhole
        );
    }

    #[test]
    fn widow_control_paragraph_taller_than_page_splits_two_by_two() {
        // 10 lines, 4 fit per page: 4 / 4 / 2 across three pages.
        assert_eq!(
            decide_paragraph_split(4, 10, true, true),
            ParagraphSplit::Break { head: 4 }
        );
        assert_eq!(
            decide_paragraph_split(4, 6, true, true),
            ParagraphSplit::Break { head: 4 }
        );
        assert_eq!(
            decide_paragraph_split(4, 2, true, true),
            ParagraphSplit::All
        );
    }

    // ── Widow control off ─────────────────────────────────────────────────

    #[test]
    fn without_widow_control_a_single_line_may_split() {
        assert_eq!(
            decide_paragraph_split(1, 6, false, false),
            ParagraphSplit::Break { head: 1 }
        );
        assert_eq!(
            decide_paragraph_split(3, 4, false, false),
            ParagraphSplit::Break { head: 3 }
        );
    }

    #[test]
    fn without_widow_control_nothing_fits_moves_whole() {
        assert_eq!(
            decide_paragraph_split(0, 4, false, false),
            ParagraphSplit::MoveWhole
        );
    }

    // ── Degenerate cases at the page top guarantee progress ────────────────

    #[test]
    fn at_page_top_unsplittable_paragraph_overflows_whole() {
        // 3-line paragraph, only 2 fit even on a full page, widow control on:
        // cannot split legally, already at the top → emit whole (overflow).
        assert_eq!(
            decide_paragraph_split(2, 3, true, true),
            ParagraphSplit::All
        );
    }

    #[test]
    fn at_page_top_single_oversized_line_overflows_whole() {
        // Even one line does not fit a full page → emit it and overflow.
        assert_eq!(
            decide_paragraph_split(0, 1, true, true),
            ParagraphSplit::All
        );
        assert_eq!(
            decide_paragraph_split(0, 3, false, true),
            ParagraphSplit::All
        );
    }

    // ── §17.3.1.11 drop-cap prefix guard ──────────────────────────────────

    #[test]
    fn drop_cap_head_clearing_the_prefix_is_unchanged() {
        // head >= drop_cap_lines: the whole prefix is in the first segment.
        assert_eq!(prefix_adjusted_head(5, 8, 3, true, true), Some(5));
        // No drop cap.
        assert_eq!(prefix_adjusted_head(2, 8, 0, true, false), Some(2));
        // Single-line drop cap can't be split within.
        assert_eq!(prefix_adjusted_head(1, 8, 1, true, false), Some(1));
        // Not the first segment — the prefix is behind us.
        assert_eq!(prefix_adjusted_head(1, 8, 3, false, false), Some(1));
        // head == remaining is the whole paragraph (no break inside the prefix).
        assert_eq!(prefix_adjusted_head(2, 2, 3, true, false), Some(2));
    }

    #[test]
    fn drop_cap_break_inside_prefix_moves_whole_when_not_at_top() {
        // head 2 < drop_cap_lines 3 → tearing the glyph; move the whole para.
        assert_eq!(prefix_adjusted_head(2, 8, 3, true, false), None);
        assert_eq!(prefix_adjusted_head(1, 8, 3, true, false), None);
    }

    #[test]
    fn drop_cap_break_inside_prefix_overflows_whole_at_top() {
        // At the page top the prefix can't move up: emit the whole prefix
        // (drop_cap_lines) and let it overflow, keeping the glyph intact.
        assert_eq!(prefix_adjusted_head(1, 8, 3, true, true), Some(3));
        // A paragraph shorter than the prefix clamps to its line count.
        assert_eq!(prefix_adjusted_head(1, 2, 3, true, true), Some(2));
    }
}
