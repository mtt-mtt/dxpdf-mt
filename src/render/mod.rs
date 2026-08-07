//! PDF renderer for dxpdf — measure, layout, and paint pipeline.
//!
//! Takes a parsed `Document` from `dxpdf-docx` and produces PDF bytes.
//!
//! # Pipeline
//!
//! 1. **Resolve** — flatten style inheritance, split sections, extract images/fonts
//! 2. **Layout** — fit content into pages using constraint-based layout
//! 3. **Subset** *(optional, gated by `subset-fonts`)* — collect glyph usage
//!    and replace each typeface with a subsetted variant before paint
//! 4. **Paint** — emit draw commands to Skia PDF canvas (requires `skia-safe`)

pub mod dimension;
pub(crate) mod emf;
pub mod emoji;
pub mod error;
pub mod fonts;
pub mod geometry;
pub mod layout;
pub mod painter;
pub mod resolve;
pub mod skia_conv;
#[cfg(feature = "subset-fonts")]
pub mod subset;

use crate::model::Document;

/// Default target resolution (pixels per inch) for embedded raster images.
///
/// This is a *ceiling*: images are downsampled toward it but never upsampled
/// (see [`painter::render_to_pdf`]), so it caps only oversized images and
/// otherwise preserves the source resolution. 220 mirrors Microsoft Word's
/// default image-compression resolution, keeping images crisp at 100% zoom on
/// typical (including HiDPI) displays. Front-ends (CLI, Python) override it to
/// trade file size against sharpness — e.g. 300 for print, 96 for small files.
pub const DEFAULT_IMAGE_DPI: f32 = 220.0;

/// Lower bound applied to any requested image DPI. A non-positive request would
/// produce a zero/negative downsample target, so it is clamped up to this floor.
///
/// Public alongside [`DEFAULT_IMAGE_DPI`] because [`RenderOptions::with_image_dpi`]
/// silently clamps to it: a caller passing `0.0` gets this value back, and
/// without the constant there is no way to predict or detect that from outside
/// the crate.
pub const MIN_IMAGE_DPI: f32 = 1.0;

/// Clamp a requested image DPI to a positive, finite value: non-positive and
/// non-finite (`NaN`, `±∞`) requests are floored to [`MIN_IMAGE_DPI`]. The
/// clamp lives here (not at the paint boundary) because `render_to_pdf` takes a
/// [`RenderOptions`], which can only be built through this — so a sanitized DPI
/// is guaranteed by construction.
fn sanitize_image_dpi(image_dpi: f32) -> f32 {
    if image_dpi.is_finite() {
        image_dpi.max(MIN_IMAGE_DPI)
    } else {
        MIN_IMAGE_DPI
    }
}

/// Whether a hard section break must consume a physical separator page before
/// the next section can be laid out. Word's `oddPage`/`evenPage` section
/// starts are based on the physical page sequence, not on the section's
/// logical PAGE numbering. The separator itself is intentionally not owned by
/// either section: Word emits a genuinely blank page without a header/footer.
fn needs_parity_separator(
    section_type: Option<crate::model::SectionType>,
    pages_before: usize,
) -> bool {
    let next_page_is_odd = (pages_before + 1) % 2 == 1;
    match section_type {
        Some(crate::model::SectionType::OddPage) => !next_page_is_odd,
        Some(crate::model::SectionType::EvenPage) => next_page_is_odd,
        _ => false,
    }
}

/// Tunable knobs for the paint phase.
///
/// Constructed via [`RenderOptions::default`] and the `with_*` builder setters,
/// so requested values are sanitized on the way in and additional knobs can be
/// added without breaking call sites.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderOptions {
    /// Target resolution (pixels per inch) images are downsampled to before
    /// embedding. Higher values yield crisper images and larger PDFs.
    image_dpi: f32,
}

impl RenderOptions {
    /// Set the target image resolution in pixels per inch.
    ///
    /// Non-positive or non-finite requests are **silently clamped** up to
    /// [`MIN_IMAGE_DPI`], which is public precisely so a caller can predict the
    /// result: passing `0.0` yields `MIN_IMAGE_DPI`, not an error. The `dxpdf`
    /// CLI takes the opposite line and *rejects* out-of-range `--image-dpi`,
    /// on the reasoning that a computed value should still render while a typed
    /// one is usually a typo.
    pub fn with_image_dpi(mut self, image_dpi: f32) -> Self {
        self.image_dpi = sanitize_image_dpi(image_dpi);
        self
    }

    /// The sanitized target image resolution in pixels per inch.
    pub fn image_dpi(&self) -> f32 {
        self.image_dpi
    }
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            image_dpi: DEFAULT_IMAGE_DPI,
        }
    }
}

use crate::model::Block;
use crate::render::layout::build::{
    build_document_endnotes, build_section_blocks, default_line_height, BuildContext, BuildState,
};
use crate::render::layout::draw_command::LayoutedPage;
use crate::render::layout::header_footer::{
    render_headers_footers, HeaderFooterBlocks, HeaderFooterClearance, PageRange,
};
use crate::render::layout::page::PageConfig;
use crate::render::layout::section::layout_section_with_clearance;
use crate::render::resolve::header_footer::HeaderFooterSet;
use crate::render::resolve::ResolvedDocument;

/// Estimate where the content on `page` ends, so a `Continuous` section
/// (§17.6.22) knows where to resume on the same page.
///
/// The match is **exhaustive on purpose**: a `_ => continue` arm silently gives
/// any unlisted variant zero height, and the following section then paints over
/// it. That is how `EmojiCluster` and `Path` came to be ignored — a paragraph
/// containing only an emoji resumed at `margins.top`, drawing the next
/// section's text inside the emoji's box. Adding a variant should break this
/// build, not the output.
fn estimate_cursor_y(
    page: &layout::draw_command::LayoutedPage,
    config: &layout::page::PageConfig,
) -> dimension::Pt {
    use layout::draw_command::DrawCommand;
    let mut max_y = config.margins.top;
    for cmd in &page.commands {
        let bottom = match cmd {
            // Baseline plus a full font size approximates the descender.
            DrawCommand::Text {
                position,
                font_size,
                ..
            } => position.y + *font_size,
            // A segment may run in either direction; take the lower end.
            DrawCommand::Underline { line, .. } | DrawCommand::Line { line, .. } => {
                line.start.y.max(line.end.y)
            }
            DrawCommand::Image { rect, .. }
            | DrawCommand::EmojiCluster { rect, .. }
            | DrawCommand::Rect { rect, .. } => rect.origin.y + rect.size.height,
            // `extent` is the shape's unrotated bounding box; a rotation can
            // reach slightly past it, which is within this function's remit.
            DrawCommand::Path { origin, extent, .. } => origin.y + extent.height,
            // §17.3.1.19: draws nothing, so it consumes no vertical space.
            DrawCommand::Outline(_) => continue,
            // Annotations mark content that is already accounted for by the
            // command underneath them, and a named destination is a point.
            // Neither adds extent of its own.
            DrawCommand::LinkAnnotation { .. }
            | DrawCommand::InternalLink { .. }
            | DrawCommand::NamedDestination { .. } => continue,
        };
        if bottom > max_y {
            max_y = bottom;
        }
    }
    max_y
}

/// Full pipeline: resolve → preload fonts → layout → paint.
///
/// Consumes the document — see [`resolve::resolve`] for why.
pub fn render(doc: Document, options: &RenderOptions) -> Result<Vec<u8>, error::RenderError> {
    let font_mgr = skia_safe::FontMgr::new();
    render_with_font_mgr(doc, &font_mgr, options)
}

/// Render with a pre-configured FontMgr (for reuse across calls).
///
/// Each stage is timed at `debug` level. `convert` already reports
/// parse-vs-render, but "render" is four very differently-shaped costs —
/// registry construction is a fixed price paid per render regardless of
/// document size, while layout and paint scale with content — and a single
/// number cannot tell them apart. Any claim about where this pipeline spends
/// its time should be checkable with `RUST_LOG=debug`, not inferred.
pub fn render_with_font_mgr(
    doc: Document,
    font_mgr: &skia_safe::FontMgr,
    options: &RenderOptions,
) -> Result<Vec<u8>, error::RenderError> {
    use std::time::Instant;

    let t = Instant::now();
    let resolved = resolve::resolve(doc);
    log::debug!("  resolve:  {:?}", t.elapsed());

    let t = Instant::now();
    #[allow(unused_mut)] // mut required only when subset-fonts is enabled
    let mut registry = fonts::FontRegistry::build(
        font_mgr.clone(),
        &resolved.embedded_fonts,
        &resolved.font_families,
    )?;
    log::debug!("  registry: {:?}", t.elapsed());

    let t = Instant::now();
    let pages = layout_document(&resolved, &registry);
    log::debug!("  layout:   {:?} ({} pages)", t.elapsed(), pages.len());

    #[cfg(feature = "subset-fonts")]
    {
        let t = Instant::now();
        let usage = subset::collect(&pages, &registry);
        let report = subset::apply(usage, &mut registry);
        log::debug!("  subset:   {:?}", t.elapsed());
        log::info!("font subset: {report}");
    }

    let t = Instant::now();
    let pdf = painter::render_to_pdf(&pages, &registry, options);
    log::debug!("  paint:    {:?}", t.elapsed());
    pdf
}

/// Resolve and lay out a document without painting to PDF.
/// Uses a real FontMgr for text measurement.
pub fn resolve_and_layout(doc: Document) -> (ResolvedDocument, Vec<LayoutedPage>) {
    let font_mgr = skia_safe::FontMgr::new();
    let resolved = resolve::resolve(doc);
    // A debug/test helper: it always supplies the real system `FontMgr`, so
    // the font-less case `build` guards against cannot arise here.
    let registry =
        fonts::FontRegistry::build(font_mgr, &resolved.embedded_fonts, &resolved.font_families)
            .expect("the system FontMgr exposes at least one typeface");
    let pages = layout_document(&resolved, &registry);
    (resolved, pages)
}

/// Lay out a resolved document using Skia font metrics resolved through
/// the supplied [`fonts::FontRegistry`].
pub fn layout_document(
    resolved: &ResolvedDocument,
    registry: &fonts::FontRegistry,
) -> Vec<LayoutedPage> {
    let measurer = layout::measurer::TextMeasurer::new(registry);
    let ctx = BuildContext {
        measurer: &measurer,
        resolved,
    };
    let mut state = BuildState::default();
    let dlh = default_line_height(&ctx);
    let mut all_pages = Vec::new();
    let mut last_config = PageConfig::default();
    // Per-section metadata for deferred header/footer rendering.
    // Carries the section's resolved slot sets, `<w:titlePg/>` flag,
    // and logical page number of the section's first page (§17.6.12);
    // the global `<w:evenAndOddHeaders/>` setting is read once below.
    struct SectionHfInfo<'a> {
        page_range: std::ops::Range<usize>,
        config: PageConfig,
        headers: &'a crate::render::resolve::header_footer::HeaderFooterSet<Vec<Block>>,
        footers: &'a crate::render::resolve::header_footer::HeaderFooterSet<Vec<Block>>,
        title_pg: bool,
        logical_page_base: usize,
    }
    let mut section_hf: Vec<SectionHfInfo> = Vec::new();
    // §17.6.12: logical PAGE numbering accumulates across sections,
    // resetting wherever a section sets `pgNumType.start`. Document
    // starts at logical 1 unless the first section overrides it.
    let mut next_logical: usize = 1;

    // §17.11.23: footnote separator indent from default paragraph style.
    let separator_indent = resolved
        .default_paragraph_style_id
        .as_ref()
        .and_then(|id| resolved.styles.get(id))
        .and_then(|s| s.paragraph.indentation)
        .and_then(|ind| ind.first_line)
        .map(|fl| match fl {
            crate::model::FirstLineIndent::FirstLine(v) => dimension::Pt::from(v),
            _ => dimension::Pt::ZERO,
        })
        .unwrap_or(dimension::Pt::ZERO);

    // §17.6.22: track continuation state for `Continuous` section breaks.
    let mut pending_continuation: Option<layout::section::ContinuationState> = None;
    let even_and_odd = resolved.even_and_odd_headers;

    // Phase 1: layout all sections to determine total page count.
    for (section_idx, section) in resolved.sections.iter().enumerate() {
        let config = PageConfig::from_section(&section.properties);
        state.page_config = config.clone();

        // §17.6.22/§17.6.23: an odd/even section break may require one
        // physical blank page before the new section. Do this before taking
        // the section page range so the blank page receives no section
        // header/footer. It still advances the document's logical sequence,
        // just as it does in Word when PAGE numbering is continuous.
        if needs_parity_separator(section.properties.section_type, all_pages.len()) {
            all_pages.push(LayoutedPage::new(config.page_size));
            next_logical += 1;
        }
        let logical_page_base = layout::header_footer::next_logical_page_base(
            next_logical,
            section.properties.page_number_type.as_ref(),
        );
        let clearance = measure_header_footer_clearance(
            &config,
            section,
            &ctx,
            &mut state,
            dlh,
            even_and_odd,
            logical_page_base,
        );

        let mut built = build_section_blocks(section, &config, &ctx, &mut state);
        let has_next_section = section_idx + 1 < resolved.sections.len();
        let ends_with_hard_section_break = has_next_section
            && !matches!(
                section.properties.section_type,
                Some(crate::model::SectionType::Continuous | crate::model::SectionType::NextColumn)
            );
        // The final plain paragraph mark is structural as well. Keeping it as
        // a font-sized empty line can push an otherwise full last page onto a
        // contentless trailing page. As with a hard section mark, decorated
        // or object-owning terminal paragraphs remain intact.
        if ends_with_hard_section_break || !has_next_section {
            layout::section::suppress_plain_terminal_section_mark(&mut built.blocks);
        }
        let measure_fn = |text: &str,
                          font: &layout::fragment::FontProps|
         -> (dimension::Pt, layout::fragment::TextMetrics) {
            measurer.measure(text, font)
        };

        // §17.6.22: continuous sections continue on the current page.
        let continuation =
            if section.properties.section_type == Some(crate::model::SectionType::Continuous) {
                pending_continuation.take()
            } else {
                pending_continuation = None;
                None
            };

        let mut pages = layout_section_with_clearance(
            &built.blocks,
            &config,
            Some(&measure_fn),
            separator_indent,
            dlh,
            layout::section::SectionStart {
                continuation,
                clearance: &clearance,
                logical_page_base,
            },
        );

        last_config = config.clone();

        // Check if the NEXT section is continuous — if so, save the last page
        // as continuation state instead of appending it.
        // (Peek ahead by checking the section index.)
        let next_is_continuous = resolved.sections.get(section_idx + 1).is_some_and(|next| {
            next.properties.section_type == Some(crate::model::SectionType::Continuous)
        });

        if next_is_continuous && !pages.is_empty() {
            let last_page = pages.pop().unwrap();
            let cursor_y = estimate_cursor_y(&last_page, &last_config);
            pending_continuation = Some(layout::section::ContinuationState {
                page: last_page,
                cursor_y,
            });
        }

        let page_start = all_pages.len();
        all_pages.append(&mut pages);
        let pages_in_section = all_pages.len() - page_start;
        next_logical = logical_page_base + pages_in_section;
        section_hf.push(SectionHfInfo {
            page_range: page_start..all_pages.len(),
            config,
            headers: &section.headers,
            footers: &section.footers,
            title_pg: section.properties.title_page.unwrap_or(false),
            logical_page_base,
        });
    }

    // §17.11.2: endnotes are document-scoped — built once, after every section,
    // so a multi-section document doesn't repeat them per section.
    let all_endnotes = build_document_endnotes(&ctx, &mut state);

    // Phase 2: render headers/footers with correct NUMPAGES (total page count).
    let total_pages = all_pages.len();
    for info in &section_hf {
        state.page_config = info.config.clone();
        render_headers_footers(
            &mut all_pages[info.page_range.clone()],
            &info.config,
            &HeaderFooterBlocks {
                headers: info.headers,
                footers: info.footers,
                title_pg: info.title_pg,
                even_and_odd,
            },
            &ctx,
            &mut state,
            dlh,
            &PageRange {
                page_base: info.page_range.start,
                logical_page_base: info.logical_page_base,
                total_pages,
            },
        );
    }

    // Render endnotes on a new page at the end of the document.
    if !all_endnotes.is_empty() {
        let measure_fn = |text: &str,
                          font: &layout::fragment::FontProps|
         -> (dimension::Pt, layout::fragment::TextMetrics) {
            measurer.measure(text, font)
        };
        let mut endnote_page = LayoutedPage::new(last_config.page_size);
        let content_width = last_config.content_width();
        let constraints =
            layout::BoxConstraints::tight_width(content_width, dimension::Pt::INFINITY);
        let mut cursor_y = last_config.margins.top;

        // Separator line.
        let sep_width = content_width * 0.33;
        let sep_x = last_config.margins.left + separator_indent;
        endnote_page
            .commands
            .push(layout::draw_command::DrawCommand::Line {
                line: crate::render::geometry::PtLineSegment::new(
                    crate::render::geometry::PtOffset::new(sep_x, cursor_y),
                    crate::render::geometry::PtOffset::new(sep_x + sep_width, cursor_y),
                ),
                color: crate::render::resolve::color::RgbColor::BLACK,
                width: dimension::Pt::new(0.5),
            });
        cursor_y += dimension::Pt::new(4.0);

        for (_, frags, style) in &all_endnotes {
            let para = layout::paragraph::layout_paragraph(
                frags,
                &constraints,
                style,
                dlh,
                Some(&measure_fn),
            );
            for mut cmd in para.commands {
                cmd.shift_y(cursor_y);
                cmd.shift_x(last_config.margins.left);
                endnote_page.commands.push(cmd);
            }
            cursor_y += para.size.height;
        }
        all_pages.push(endnote_page);
    }

    if all_pages.is_empty() {
        all_pages.push(LayoutedPage::new(PageConfig::default().page_size));
    }

    all_pages
}

/// Measure each populated header/footer slot independently so pagination can
/// reserve the slot selected for each physical page.
fn measure_header_footer_clearance(
    config: &PageConfig,
    section: &crate::render::resolve::sections::ResolvedSection,
    ctx: &layout::build::BuildContext,
    state: &mut BuildState,
    default_line_height: dimension::Pt,
    even_and_odd: bool,
    logical_page_base: usize,
) -> HeaderFooterClearance {
    let headers =
        HeaderFooterSet {
            default: section.headers.default.as_deref().map(|blocks| {
                measure_header_bottom(blocks, config, ctx, state, default_line_height)
            }),
            first: section.headers.first.as_deref().map(|blocks| {
                measure_header_bottom(blocks, config, ctx, state, default_line_height)
            }),
            even: section.headers.even.as_deref().map(|blocks| {
                measure_header_bottom(blocks, config, ctx, state, default_line_height)
            }),
        };
    let footers =
        HeaderFooterSet {
            default: section.footers.default.as_deref().map(|blocks| {
                measure_footer_extent(blocks, config, ctx, state, default_line_height)
            }),
            first: section.footers.first.as_deref().map(|blocks| {
                measure_footer_extent(blocks, config, ctx, state, default_line_height)
            }),
            even: section.footers.even.as_deref().map(|blocks| {
                measure_footer_extent(blocks, config, ctx, state, default_line_height)
            }),
        };

    HeaderFooterClearance::new(
        config,
        headers,
        footers,
        section.properties.title_page.unwrap_or(false),
        even_and_odd,
        logical_page_base,
    )
}

fn measure_header_bottom(
    blocks: &[crate::model::Block],
    config: &PageConfig,
    ctx: &layout::build::BuildContext,
    state: &mut BuildState,
    default_line_height: dimension::Pt,
) -> dimension::Pt {
    let hf = layout::build::build_header_footer_content(blocks, ctx, state);
    // Height only — no float x is read here, so the parity is immaterial.
    let result = layout::section::stack_blocks(
        &hf.blocks,
        config.content_width(),
        default_line_height,
        None,
        layout::section::PageParity::Odd,
    );
    let blocks_bottom = config.header_margin + result.height;
    let floats_bottom = hf
        .floating_images
        .iter()
        .filter(|fi| fi.is_wrap_top_and_bottom())
        .map(|fi| {
            let y = match fi.y {
                layout::section::FloatingImageY::Absolute(y) => y,
                layout::section::FloatingImageY::RelativeToParagraph(off) => {
                    config.header_margin + off
                }
            };
            y + fi.size.height
        })
        .fold(dimension::Pt::ZERO, |a, b| a.max(b));
    blocks_bottom.max(floats_bottom)
}

fn measure_footer_extent(
    blocks: &[crate::model::Block],
    config: &PageConfig,
    ctx: &layout::build::BuildContext,
    state: &mut BuildState,
    default_line_height: dimension::Pt,
) -> dimension::Pt {
    let hf = layout::build::build_header_footer_content(blocks, ctx, state);
    // Height only — no float x is read here, so the parity is immaterial.
    let result = layout::section::stack_blocks(
        &hf.blocks,
        config.content_width(),
        default_line_height,
        None,
        layout::section::PageParity::Odd,
    );
    let blocks_extent = config.footer_margin + result.height;
    let floats_extent = hf
        .floating_images
        .iter()
        .filter(|fi| fi.is_wrap_top_and_bottom())
        .map(|fi| match fi.y {
            layout::section::FloatingImageY::Absolute(y) => config.page_size.height - y,
            layout::section::FloatingImageY::RelativeToParagraph(off) => {
                config.footer_margin + off + fi.size.height
            }
        })
        .fold(dimension::Pt::ZERO, |a, b| a.max(b));
    blocks_extent.max(floats_extent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use std::collections::HashMap;

    fn empty_doc() -> Document {
        Document {
            settings: DocumentSettings::default(),
            theme: None,
            styles: StyleSheet::default(),
            numbering: NumberingDefinitions::default(),
            body: vec![],
            final_section: SectionProperties::default(),
            headers: HashMap::new(),
            footers: HashMap::new(),
            footnotes: HashMap::new(),
            endnotes: HashMap::new(),
            media: HashMap::new(),
            embedded_fonts: vec![],
        }
    }

    fn para(text: &str) -> Block {
        Block::Paragraph(Box::new(Paragraph {
            style_id: None,
            properties: ParagraphProperties::default(),
            mark_run_properties: None,
            content: vec![Inline::TextRun(Box::new(TextRun {
                style_id: None,
                properties: RunProperties::default(),
                content: vec![RunElement::Text(text.to_string())],
                rsids: RevisionIds::default(),
            }))],
            rsids: ParagraphRevisionIds::default(),
        }))
    }

    #[test]
    fn render_options_default_matches_word_resolution() {
        // 220 ppi mirrors Word's default image-compression resolution.
        assert_eq!(DEFAULT_IMAGE_DPI, 220.0);
        assert_eq!(RenderOptions::default().image_dpi(), 220.0);
    }

    #[test]
    fn parity_section_break_inserts_only_the_required_physical_separator() {
        assert!(needs_parity_separator(Some(SectionType::OddPage), 1,));
        assert!(!needs_parity_separator(Some(SectionType::OddPage), 2,));
        assert!(!needs_parity_separator(Some(SectionType::EvenPage), 1,));
        assert!(needs_parity_separator(Some(SectionType::EvenPage), 2,));
        assert!(!needs_parity_separator(Some(SectionType::NextPage), 1));
    }

    #[test]
    fn render_options_with_image_dpi_overrides() {
        assert_eq!(
            RenderOptions::default().with_image_dpi(300.0).image_dpi(),
            300.0
        );
    }

    #[test]
    fn render_options_clamps_non_positive_and_non_finite_dpi() {
        // Zero, negative, and non-finite requests clamp up to the floor so the
        // downsample target is always a meaningful positive resolution.
        assert_eq!(
            RenderOptions::default().with_image_dpi(0.0).image_dpi(),
            1.0
        );
        assert_eq!(
            RenderOptions::default().with_image_dpi(-50.0).image_dpi(),
            1.0
        );
        assert_eq!(
            RenderOptions::default()
                .with_image_dpi(f32::NAN)
                .image_dpi(),
            1.0
        );
        assert_eq!(
            RenderOptions::default()
                .with_image_dpi(f32::INFINITY)
                .image_dpi(),
            1.0
        );
    }

    #[test]
    fn resolve_and_layout_empty_doc() {
        let doc = empty_doc();
        let (resolved, pages) = resolve_and_layout(doc);

        assert_eq!(resolved.sections.len(), 1);
        assert_eq!(pages.len(), 1);
        assert!(pages[0].commands.is_empty());
    }

    #[test]
    fn resolve_and_layout_with_paragraphs() {
        let mut doc = empty_doc();
        doc.body = vec![para("hello"), para("world")];

        let (_, pages) = resolve_and_layout(doc);

        assert_eq!(pages.len(), 1);
        let text_count = pages[0]
            .commands
            .iter()
            .filter(|c| matches!(c, layout::draw_command::DrawCommand::Text { .. }))
            .count();
        assert_eq!(text_count, 2);
    }

    #[test]
    fn body_layout_uses_the_header_and_footer_selected_for_each_page() {
        use crate::model::dimension::{Dimension, Twips};

        let mut doc = empty_doc();
        let default_header = RelId::new("default-header");
        let first_header = RelId::new("first-header");
        let default_footer = RelId::new("default-footer");
        let first_footer = RelId::new("first-footer");
        doc.headers
            .insert(default_header.clone(), vec![para("default header")]);
        doc.headers.insert(
            first_header.clone(),
            vec![para("first header 1"), para("first header 2")],
        );
        doc.footers
            .insert(default_footer.clone(), vec![para("default footer")]);
        doc.footers.insert(
            first_footer.clone(),
            vec![para("first footer 1"), para("first footer 2")],
        );
        doc.body = (0..6).map(|index| para(&format!("body {index}"))).collect();
        doc.final_section = SectionProperties {
            page_size: Some(PageSize {
                width: Some(Dimension::<Twips>::new(4000)),
                height: Some(Dimension::<Twips>::new(2000)),
                orientation: None,
            }),
            page_margins: Some(PageMargins {
                top: Some(Dimension::<Twips>::new(200)),
                right: Some(Dimension::<Twips>::new(200)),
                bottom: Some(Dimension::<Twips>::new(200)),
                left: Some(Dimension::<Twips>::new(200)),
                header: Some(Dimension::<Twips>::new(100)),
                footer: Some(Dimension::<Twips>::new(100)),
                gutter: None,
            }),
            header_refs: SectionHeaderFooterRefs {
                default: Some(default_header),
                first: Some(first_header),
                even: None,
            },
            footer_refs: SectionHeaderFooterRefs {
                default: Some(default_footer),
                first: Some(first_footer),
                even: None,
            },
            title_page: Some(true),
            ..Default::default()
        };

        let (_, pages) = resolve_and_layout(doc);

        assert_eq!(
            pages.len(),
            2,
            "shorter default slots must expand page 2 body"
        );
        let body_positions = pages
            .iter()
            .map(|page| {
                page.commands
                    .iter()
                    .filter_map(|command| match command {
                        layout::draw_command::DrawCommand::Text { text, position, .. }
                            if text.starts_with("body") =>
                        {
                            Some(position.y)
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(body_positions[0].len(), 3);
        assert_eq!(body_positions[1].len(), 3);
        assert!(
            body_positions[1][0] < body_positions[0][0],
            "page 2 body must start below the shorter default header",
        );
    }

    #[test]
    fn resolve_and_layout_with_table() {
        let mut doc = empty_doc();
        doc.body = vec![Block::Table(Box::new(Table {
            properties: TableProperties::default(),
            grid: vec![
                GridColumn {
                    width: crate::model::dimension::Dimension::new(4680),
                },
                GridColumn {
                    width: crate::model::dimension::Dimension::new(4680),
                },
            ],
            rows: vec![TableRow {
                properties: TableRowProperties::default(),
                cells: vec![
                    TableCell {
                        properties: TableCellProperties::default(),
                        content: vec![para("A")],
                    },
                    TableCell {
                        properties: TableCellProperties::default(),
                        content: vec![para("B")],
                    },
                ],
                rsids: TableRowRevisionIds::default(),
                property_exceptions: None,
            }],
        }))];

        let (_, pages) = resolve_and_layout(doc);
        assert_eq!(pages.len(), 1);

        let text_count = pages[0]
            .commands
            .iter()
            .filter(|c| matches!(c, layout::draw_command::DrawCommand::Text { .. }))
            .count();
        assert_eq!(text_count, 2, "two cells = two text commands");
    }

    #[test]
    fn layout_respects_page_size() {
        let mut doc = empty_doc();
        doc.final_section = SectionProperties {
            page_size: Some(PageSize {
                width: Some(crate::model::dimension::Dimension::new(12240)),
                height: Some(crate::model::dimension::Dimension::new(15840)),
                orientation: None,
            }),
            ..Default::default()
        };

        let (_, pages) = resolve_and_layout(doc);
        assert_eq!(pages[0].page_size.width.raw(), 612.0);
        assert_eq!(pages[0].page_size.height.raw(), 792.0);
    }

    // ─── estimate_cursor_y (H3#1) ─────────────────────────────────────────

    mod cursor_y {
        use super::*;
        use crate::render::dimension::Pt;
        use crate::render::geometry::{PtLineSegment, PtOffset, PtRect, PtSize};
        use crate::render::layout::draw_command::{DrawCommand, LayoutedPage};
        use crate::render::layout::page::PageConfig;

        fn page_with(cmd: DrawCommand) -> (LayoutedPage, PageConfig) {
            let config = PageConfig::default();
            let mut page = LayoutedPage::new(config.page_size);
            page.commands.push(cmd);
            (page, config)
        }

        fn rect_at(y: f32, h: f32) -> PtRect {
            PtRect::from_xywh(Pt::ZERO, Pt::new(y), Pt::new(10.0), Pt::new(h))
        }

        /// An empty page resumes at the top margin — the floor every case
        /// below has to beat.
        #[test]
        fn empty_page_resumes_at_the_top_margin() {
            let config = PageConfig::default();
            let page = LayoutedPage::new(config.page_size);
            assert_eq!(estimate_cursor_y(&page, &config), config.margins.top);
        }

        /// Every variant that occupies vertical space must move the cursor.
        /// `EmojiCluster` and `Path` are H3#1: both were swallowed by a
        /// `_ => continue`, so a section following them resumed at the top
        /// margin and painted over them.
        #[test]
        fn every_extent_bearing_variant_advances_the_cursor() {
            let below = Pt::new(400.0);
            let cases: Vec<(&str, DrawCommand)> = vec![
                (
                    "Image",
                    DrawCommand::Image {
                        rect: rect_at(400.0, 50.0),
                        image_data: crate::render::resolve::images::MediaEntry {
                            data: std::sync::Arc::from(Vec::new().into_boxed_slice()),
                            format: crate::model::ImageFormat::Png,
                        },
                        src_rect: None,
                    },
                ),
                (
                    "Rect",
                    DrawCommand::Rect {
                        rect: rect_at(400.0, 50.0),
                        color: crate::render::resolve::color::RgbColor::BLACK,
                    },
                ),
                (
                    "Line",
                    DrawCommand::Line {
                        line: PtLineSegment::new(
                            PtOffset::new(Pt::ZERO, Pt::new(450.0)),
                            PtOffset::new(Pt::new(10.0), Pt::new(400.0)),
                        ),
                        color: crate::render::resolve::color::RgbColor::BLACK,
                        width: Pt::new(1.0),
                    },
                ),
                (
                    "Path",
                    DrawCommand::Path {
                        origin: PtOffset::new(Pt::ZERO, Pt::new(400.0)),
                        rotation: crate::model::dimension::Dimension::new(0),
                        flip_h: false,
                        flip_v: false,
                        extent: PtSize::new(Pt::new(10.0), Pt::new(50.0)),
                        paths: Vec::new(),
                        fill: crate::render::layout::draw_command::ResolvedFill::None,
                        stroke: None,
                        effects: Vec::new(),
                    },
                ),
            ];

            for (label, cmd) in cases {
                let (page, config) = page_with(cmd);
                let y = estimate_cursor_y(&page, &config);
                assert!(
                    y > below,
                    "{label} occupies y=400..450 but the cursor resumed at {y:?} — \
                     the next section would paint over it"
                );
            }
        }

        /// The emoji case, kept separate because it is the one reproduced
        /// end-to-end: a paragraph containing only an emoji, followed by a
        /// continuous section break, resumed at exactly `margins.top`.
        #[test]
        fn emoji_cluster_advances_the_cursor() {
            let (page, config) = page_with(DrawCommand::EmojiCluster {
                rect: rect_at(36.0, 47.25),
                text: "\u{1F4DE}".into(),
                typeface: emoji_test_typeface(),
                size: Pt::new(36.0),
                presentation: crate::render::emoji::cluster::EmojiPresentation::Emoji,
                structure: crate::render::emoji::cluster::EmojiStructure::Single,
            });
            assert_eq!(
                estimate_cursor_y(&page, &config),
                Pt::new(83.25),
                "the cursor must clear the emoji's box, not sit inside it"
            );
        }

        fn emoji_test_typeface() -> crate::render::fonts::TypefaceEntry {
            use skia_safe::{FontMgr, FontStyle};
            let tf = FontMgr::new()
                .legacy_make_typeface(None::<&str>, FontStyle::normal())
                .expect("system default typeface");
            let id = crate::render::fonts::TypefaceId::from(&tf);
            crate::render::fonts::TypefaceEntry {
                typeface: tf,
                origin: crate::render::fonts::TypefaceOrigin::System { typeface_id: id },
            }
        }

        /// Annotations sit on top of content that already contributed its own
        /// extent, and a destination is a point. Counting them would push the
        /// cursor past content that is not there.
        #[test]
        fn annotations_and_destinations_contribute_nothing() {
            for cmd in [
                DrawCommand::LinkAnnotation {
                    rect: rect_at(400.0, 50.0),
                    url: "https://example.invalid".into(),
                },
                DrawCommand::InternalLink {
                    rect: rect_at(400.0, 50.0),
                    destination: "anchor".into(),
                },
                DrawCommand::NamedDestination {
                    position: PtOffset::new(Pt::ZERO, Pt::new(400.0)),
                    name: "anchor".into(),
                },
            ] {
                let (page, config) = page_with(cmd);
                assert_eq!(estimate_cursor_y(&page, &config), config.margins.top);
            }
        }
    }

    // ─── Error surface (H3#4) ─────────────────────────────────────────────

    /// The only condition the pipeline cannot render its way out of. Emptiness
    /// is deliberately not one — see `empty_document_still_renders_a_page`.
    #[test]
    fn a_font_less_host_is_an_error_not_a_panic() {
        let doc = empty_doc();
        let err =
            render_with_font_mgr(doc, &skia_safe::FontMgr::empty(), &RenderOptions::default())
                .expect_err("a FontMgr with no typefaces cannot render");
        assert!(matches!(err, error::RenderError::NoFontsAvailable));
        assert!(
            err.to_string().contains("no fonts available"),
            "the message must say what went wrong, got {err}"
        );
    }

    /// The behaviour that made `RenderError::EmptyDocument` unreachable: an
    /// empty document is a blank page, as in Word — not an error.
    #[test]
    fn empty_document_still_renders_a_page() {
        let pdf = render(empty_doc(), &RenderOptions::default()).expect("empty doc renders");
        assert!(pdf.starts_with(b"%PDF"));
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("/Count 1"), "exactly one blank page");
    }
}
