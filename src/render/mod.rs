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

/// Whether Word must preserve recto/verso parity when an ordinary hard
/// section restarts logical page numbering.
///
/// With document-level `evenAndOddHeaders` enabled, Word uses
/// `w:pgNumType/@start` to decide whether the section's first page is odd or
/// even.  For a `nextPage` section after the document has already started, a
/// conflicting physical sheet gets a blank separator so (for example) a
/// restarted logical page 1 remains a right-hand/odd page. Explicit
/// `oddPage`/`evenPage` starts remain the responsibility of
/// `needs_parity_separator`; continuous and next-column sections cannot insert
/// a physical separator here.
fn needs_numbering_restart_parity_separator(
    even_and_odd: bool,
    section_type: Option<crate::model::SectionType>,
    pages_before: usize,
    page_number_type: Option<&crate::model::PageNumberType>,
) -> bool {
    if !even_and_odd
        || !matches!(
            section_type,
            None | Some(crate::model::SectionType::NextPage)
        )
    {
        return false;
    }
    let Some(start) = page_number_type.and_then(|numbering| numbering.start) else {
        return false;
    };
    let next_physical_is_odd = (pages_before + 1) % 2 == 1;
    let restarted_logical_is_odd = start % 2 == 1;
    next_physical_is_odd != restarted_logical_is_odd
}

/// Whether the outgoing section owns a structural paragraph mark that should
/// not be laid out as ordinary empty content.
///
/// Only a hard break *before another section* has such an outgoing mark.  The
/// final `w:sectPr` is a direct child of `w:body`; any trailing `w:p` elements
/// before it are real document paragraphs and may legitimately flow onto a
/// blank final page.
fn has_structural_terminal_section_mark(
    section_type: Option<crate::model::SectionType>,
    has_next_section: bool,
) -> bool {
    has_next_section
        && !matches!(
            section_type,
            Some(crate::model::SectionType::Continuous | crate::model::SectionType::NextColumn)
        )
}

/// Whether a leading section consists only of an ordinary structural section
/// mark and therefore owns no physical page.
///
/// This is deliberately narrower than
/// [`has_structural_terminal_section_mark`]. An explicit odd/even break carries
/// physical parity intent, while continuous and next-column breaks can share a
/// flow region. Non-leading empty sections can also be intentional blank pages.
/// `had_blocks_before_suppression` distinguishes a section whose structural
/// paragraph mark was removed from a section that was empty for some unrelated
/// reason in the model/build pipeline.
fn leading_structural_section_owns_no_page(
    section_index: usize,
    section_type: Option<crate::model::SectionType>,
    has_next_section: bool,
    had_blocks_before_suppression: bool,
    blocks_are_empty_after_suppression: bool,
) -> bool {
    section_index == 0
        && has_next_section
        && had_blocks_before_suppression
        && blocks_are_empty_after_suppression
        && matches!(
            section_type,
            None | Some(crate::model::SectionType::NextPage)
        )
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
    build_document_endnotes, build_section_blocks, default_line_height, set_section_document_grid,
    BuildContext, BuildState,
};
use crate::render::layout::draw_command::LayoutedPage;
use crate::render::layout::header_footer::{
    render_headers_footers, HeaderFooterBlocks, HeaderFooterClearance, PageRange,
};
use crate::render::layout::page::PageConfig;
use crate::render::layout::section::layout_section_with_clearance_result;
use crate::render::resolve::header_footer::HeaderFooterSet;
use crate::render::resolve::ResolvedDocument;

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
    render_with_font_mgr_and_font_pack(doc, font_mgr, None, options)
}

/// Render with a reusable controlled font pack installed ahead of host fonts.
pub fn render_with_font_mgr_and_font_pack(
    doc: Document,
    font_mgr: &skia_safe::FontMgr,
    font_pack: Option<&fonts::FontPack>,
    options: &RenderOptions,
) -> Result<Vec<u8>, error::RenderError> {
    use std::time::Instant;

    let t = Instant::now();
    let resolved = resolve::resolve(doc);
    log::debug!("  resolve:  {:?}", t.elapsed());

    let t = Instant::now();
    #[allow(unused_mut)] // mut required only when subset-fonts is enabled
    let mut registry = fonts::FontRegistry::build_with_font_pack(
        font_mgr.clone(),
        &resolved.embedded_fonts,
        &resolved.font_families,
        font_pack,
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
    let pdf = painter::render_to_pdf_with_character_spacing_control(
        &pages,
        &registry,
        options,
        resolved.character_spacing_control,
    );
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
    let measurer = layout::measurer::TextMeasurer::with_character_spacing_control(
        registry,
        resolved.character_spacing_control,
    );
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
        doc_grid_line_pitch: Option<dimension::Pt>,
        character_grid_active: bool,
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
        set_section_document_grid(&mut state, &section.properties);

        // §17.6.22/§17.6.23: an odd/even section break may require one
        // physical blank page before the new section. Do this before taking
        // the section page range so the blank page receives no section
        // header/footer. It still advances the document's logical sequence,
        // just as it does in Word when PAGE numbering is continuous.
        if section_idx > 0
            && section.properties.section_type != Some(crate::model::SectionType::Continuous)
        {
            log::debug!(
                target: "dxpdf::pagination",
                "section-boundary section={} type={:?} pages_before={} logical_next={}",
                section_idx,
                section.properties.section_type,
                all_pages.len(),
                next_logical,
            );
        }
        let explicit_parity_separator =
            needs_parity_separator(section.properties.section_type, all_pages.len());
        let numbering_restart_separator = section_idx > 0
            && needs_numbering_restart_parity_separator(
                even_and_odd,
                section.properties.section_type,
                all_pages.len(),
                section.properties.page_number_type.as_ref(),
            );
        if explicit_parity_separator || numbering_restart_separator {
            let cause = if explicit_parity_separator {
                "ParitySectionSeparator"
            } else {
                "NumberingRestartParitySeparator"
            };
            log::debug!(
                target: "dxpdf::pagination",
                "page-break cause={} section={} type={:?} physical_page={} logical_page={} body_commands=0 total_commands=0 footnotes=0 floats=0",
                cause,
                section_idx,
                section.properties.section_type,
                all_pages.len() + 1,
                next_logical,
            );
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
        let ends_with_hard_section_break =
            has_structural_terminal_section_mark(section.properties.section_type, has_next_section);
        let had_blocks_before_suppression = !built.blocks.is_empty();
        if ends_with_hard_section_break {
            layout::section::suppress_plain_terminal_section_mark(&mut built.blocks);
        }
        if leading_structural_section_owns_no_page(
            section_idx,
            section.properties.section_type,
            has_next_section,
            had_blocks_before_suppression,
            built.blocks.is_empty(),
        ) {
            log::debug!(
                target: "dxpdf::pagination",
                "section-boundary cause=LeadingStructuralSectionSuppressed section={} type={:?} pages_before={} logical_next={}",
                section_idx,
                section.properties.section_type,
                all_pages.len(),
                next_logical,
            );
            continue;
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
                // Consume any stale state defensively; a non-continuous
                // section cannot share the outgoing physical page.
                let _ = pending_continuation.take();
                None
            };

        let next_is_continuous = resolved.sections.get(section_idx + 1).is_some_and(|next| {
            next.properties.section_type == Some(crate::model::SectionType::Continuous)
        });

        let layout_result = layout_section_with_clearance_result(
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
            next_is_continuous,
        );
        let mut pages = layout_result.pages;
        pending_continuation = layout_result.continuation;
        last_config = config.clone();

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
            doc_grid_line_pitch: state.doc_grid_line_pitch,
            character_grid_active: state.character_grid_active,
        });
    }

    // §17.11.2: endnotes are document-scoped — built once, after every section,
    // so a multi-section document doesn't repeat them per section.
    let all_endnotes = build_document_endnotes(&ctx, &mut state);

    // Phase 2: render headers/footers with correct NUMPAGES (total page count).
    let total_pages = all_pages.len();
    for info in &section_hf {
        state.page_config = info.config.clone();
        state.doc_grid_line_pitch = info.doc_grid_line_pitch;
        state.character_grid_active = info.character_grid_active;
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
    fn numbering_restart_preserves_odd_even_physical_parity() {
        let restart = PageNumberType {
            format: None,
            start: Some(1),
            chap_style: None,
            chap_sep: None,
        };
        assert!(needs_numbering_restart_parity_separator(
            true,
            Some(SectionType::NextPage),
            1,
            Some(&restart),
        ));
        assert!(!needs_numbering_restart_parity_separator(
            true,
            Some(SectionType::NextPage),
            2,
            Some(&restart),
        ));
        assert!(!needs_numbering_restart_parity_separator(
            false,
            Some(SectionType::NextPage),
            1,
            Some(&restart),
        ));
        assert!(!needs_numbering_restart_parity_separator(
            true,
            Some(SectionType::Continuous),
            1,
            Some(&restart),
        ));
        assert!(!needs_numbering_restart_parity_separator(
            true,
            Some(SectionType::OddPage),
            1,
            Some(&restart),
        ));
        assert!(!needs_numbering_restart_parity_separator(
            true,
            Some(SectionType::NextPage),
            1,
            None,
        ));
    }

    #[test]
    fn only_an_outgoing_hard_section_has_a_structural_terminal_mark() {
        assert!(has_structural_terminal_section_mark(
            Some(SectionType::NextPage),
            true
        ));
        assert!(has_structural_terminal_section_mark(None, true));
        assert!(!has_structural_terminal_section_mark(
            Some(SectionType::Continuous),
            true
        ));
        assert!(!has_structural_terminal_section_mark(
            Some(SectionType::NextColumn),
            true
        ));
        assert!(!has_structural_terminal_section_mark(
            Some(SectionType::NextPage),
            false
        ));
        assert!(!has_structural_terminal_section_mark(None, false));
    }

    #[test]
    fn only_a_leading_ordinary_structural_section_owns_no_page() {
        for section_type in [None, Some(SectionType::NextPage)] {
            assert!(leading_structural_section_owns_no_page(
                0,
                section_type,
                true,
                true,
                true,
            ));
        }

        for section_type in [
            Some(SectionType::OddPage),
            Some(SectionType::EvenPage),
            Some(SectionType::Continuous),
            Some(SectionType::NextColumn),
        ] {
            assert!(
                !leading_structural_section_owns_no_page(0, section_type, true, true, true,),
                "{section_type:?} carries layout intent and must not be folded"
            );
        }

        assert!(!leading_structural_section_owns_no_page(
            1, None, true, true, true,
        ));
        assert!(!leading_structural_section_owns_no_page(
            0, None, false, true, true,
        ));
        assert!(!leading_structural_section_owns_no_page(
            0, None, true, false, true,
        ));
        assert!(!leading_structural_section_owns_no_page(
            0, None, true, true, false,
        ));
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
    fn leading_structural_section_uses_no_physical_page() {
        let mut doc = empty_doc();
        let outgoing_footer = RelId::new("outgoing-footer");
        let body_footer = RelId::new("body-footer");
        doc.footers
            .insert(outgoing_footer.clone(), vec![para("OUTGOING FOOTER")]);
        doc.footers
            .insert(body_footer.clone(), vec![para("BODY FOOTER")]);

        let mut leading_section = SectionProperties {
            section_type: Some(SectionType::NextPage),
            ..Default::default()
        };
        leading_section.footer_refs.default = Some(outgoing_footer);
        doc.final_section.footer_refs.default = Some(body_footer);
        doc.body = vec![
            para(""),
            Block::SectionBreak(Box::new(leading_section)),
            para("BODY"),
        ];

        let (resolved, pages) = resolve_and_layout(doc);

        assert_eq!(resolved.sections.len(), 2);
        assert_eq!(
            pages.len(),
            1,
            "the leading structural section owns no sheet"
        );
        let text = pages[0]
            .commands
            .iter()
            .filter_map(|command| match command {
                layout::draw_command::DrawCommand::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(text.matches("BODY").count(), 2, "rendered text: {text:?}");
        assert!(text.contains("FOOTER"), "rendered text: {text:?}");
        assert!(!text.contains("OUTGOING"), "rendered text: {text:?}");
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
