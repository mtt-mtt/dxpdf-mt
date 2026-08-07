use std::rc::Rc;

use crate::model::{
    Block, FieldCharType, Inline, NoteId, RunElement, RunProperties, TextRun, VerticalAlign,
};
use crate::render::dimension::Pt;
use crate::render::emoji::cluster::EmojiCluster;
use crate::render::geometry::PtSize;
use crate::render::resolve::color::RgbColor;

use super::segment::{build_inline_units, InlineUnit, SegmentPiece};
use super::text::{
    emit_emoji_or_fallback, emit_text_fragments, emit_text_words, resolve_highlight_color,
    TextRunStyle,
};
use super::{
    font_props_from_run, to_roman_lower, FontProps, Fragment, FragmentBorder, LinkTarget,
    TextMetrics, SUBSCRIPT_HEIGHT_OFFSET_RATIO, SUPERSCRIPT_ASCENT_OFFSET_RATIO,
    SUPERSCRIPT_FONT_SIZE_RATIO,
};

/// §17.11.12: a footnote reference recorded while walking a paragraph's inlines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordedFootnote {
    /// The referenced note.
    pub id: NoteId,
    /// The sequential display number emitted as this reference's superscript.
    pub display: u32,
}

/// §17.11.12: footnote numbering state threaded through fragment collection.
///
/// The display counter and the record of *which* notes were referenced advance
/// at a single site (the private `record` method), so the footnote body a
/// caller renders can never disagree with the superscript mark this walk
/// emitted.
///
/// This replaced a design where the counter was advanced by this **recursive**
/// walk (which descends into hyperlinks, non-substituted fields, MCE
/// fallbacks, and VML text boxes) while callers re-derived the reference list
/// from a **flat** scan of the paragraph's top-level inlines. The two walkers
/// disagreed for any nested reference: its body was never emitted, and when it
/// preceded a top-level reference the body's number no longer matched the mark.
#[derive(Default, Debug)]
pub struct FootnoteTracker {
    /// Display number of the most recently emitted reference.
    last_display: u32,
    /// References recorded since the last [`FootnoteTracker::take_pending`].
    pending: Vec<RecordedFootnote>,
}

impl FootnoteTracker {
    /// Assign the next display number to `id`, record it, and return the number.
    fn record(&mut self, id: NoteId) -> u32 {
        self.last_display += 1;
        self.pending.push(RecordedFootnote {
            id,
            display: self.last_display,
        });
        self.last_display
    }

    /// Take the references recorded since the last call, in encounter order.
    ///
    /// Every caller of [`collect_fragments`] must drain — including those that
    /// don't render footnote bodies (headers/footers, footnote bodies
    /// themselves). Otherwise their references leak into the next paragraph's
    /// batch and are rendered against the wrong host.
    pub fn take_pending(&mut self) -> Vec<RecordedFootnote> {
        std::mem::take(&mut self.pending)
    }
}

/// §17.3.2.4: convert a run-level [`crate::model::Border`] into a render-side
/// [`FragmentBorder`], filtering out the spec's "no border" styles.
///
/// `<w:bdr w:val="nil"/>` and `<w:bdr w:val="none"/>` (§17.18.2 ST_Border) are
/// kept apart by the model, because table border conflict resolution needs the
/// difference — but a run border has no adjacent edge to conflict with, so here
/// they are genuinely equivalent and `draws_nothing` covers both. The model
/// preserves the explicit `Some` so it can override an inherited border in the
/// §17.7.2 cascade; at the render boundary we drop it, otherwise the painter
/// would draw a hairline box around every word.
pub(super) fn run_border_to_fragment(
    border: Option<&crate::model::Border>,
) -> Option<FragmentBorder> {
    let b = border?;
    if b.style.draws_nothing() {
        return None;
    }
    Some(FragmentBorder {
        width: Pt::from(b.width),
        color: crate::render::resolve::color::resolve_color(
            b.color,
            crate::render::resolve::color::ColorContext::Text,
        ),
        space: Pt::new(b.space.raw() as f32),
    })
}

/// Split one OOXML run when its text switches between the East Asian and
/// Latin/high-ANSI font slots. Neutral punctuation and whitespace stay with
/// the nearest preceding strong script; leading neutral characters follow the
/// first strong script.
fn split_text_for_font_slots(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut current: Option<bool> = None;

    for (index, ch) in text.char_indices() {
        // Unicode blocks also contain CJK punctuation. Treat punctuation as
        // neutral before deciding which OOXML font slot owns the character,
        // otherwise a full-width colon can spuriously start an East Asian run.
        let next = if !ch.is_alphanumeric() {
            None
        } else if crate::render::resolve::fonts::is_east_asian_char(ch) {
            Some(true)
        } else {
            Some(false)
        };
        let Some(next) = next else { continue };
        match current {
            None => current = Some(next),
            Some(previous) if previous != next => {
                if index > start {
                    parts.push(&text[start..index]);
                }
                start = index;
                current = Some(next);
            }
            Some(_) => {}
        }
    }

    if start < text.len() {
        parts.push(&text[start..]);
    }
    if parts.is_empty() && !text.is_empty() {
        parts.push(text);
    }
    parts
}

/// §17.7.2: resolve the effective styling of a single run by walking the
/// cascade (direct → character style → paragraph run defaults), then
/// translating to render-side `FontProps` + `TextRunStyle`.
///
/// This is the single source of truth for run-level styling. Both the
/// per-run path (`Discrete TextRun`) and the per-segment-piece path
/// (cross-run cluster reassembly via `segment.rs`) call it — for cross-
/// run clusters, the *base run*'s styling drives the entire piece per
/// the design in `fragment/segment.rs`.
#[allow(clippy::too_many_arguments)] // the cascade has many independent inputs by spec
fn resolve_run_styling<F>(
    tr: &TextRun,
    default_family: &str,
    default_size: Pt,
    default_color: RgbColor,
    resolved_styles: Option<
        &std::collections::HashMap<
            crate::model::StyleId,
            crate::render::resolve::styles::ResolvedStyle,
        >,
    >,
    paragraph_run_defaults: Option<&RunProperties>,
    theme: Option<&crate::model::Theme>,
    auto_fit: crate::render::layout::ShapeAutoFit,
    text_hint: Option<&str>,
    measure_text: &F,
) -> (FontProps, TextRunStyle)
where
    F: Fn(&str, &FontProps) -> (Pt, TextMetrics),
{
    let mut effective_props = tr.properties.clone();
    // §17.3.2.26: resolve theme font references before merging.
    if let Some(th) = theme {
        crate::render::resolve::fonts::resolve_font_set_themes(&mut effective_props.fonts, th);
    }
    if let (Some(ref style_id), Some(styles)) = (&tr.style_id, resolved_styles) {
        if let Some(resolved_style) = styles.get(style_id) {
            crate::render::resolve::properties::merge_run_properties(
                &mut effective_props,
                &resolved_style.run,
            );
        }
    }
    if let Some(para_run) = paragraph_run_defaults {
        crate::render::resolve::properties::merge_run_properties(&mut effective_props, para_run);
    }

    let selected_family = text_hint.and_then(|text| {
        crate::render::resolve::fonts::effective_font_for_text(
            &effective_props.fonts,
            text,
            effective_props.lang.as_ref(),
            theme,
        )
    });
    let mut font = font_props_from_run(&effective_props, default_family, default_size, auto_fit);
    if let Some(family) = selected_family {
        font.family = Rc::from(family);
    }
    let color = effective_props
        .color
        .map(|c| {
            crate::render::resolve::color::resolve_color(
                c,
                crate::render::resolve::color::ColorContext::Text,
            )
        })
        .unwrap_or(default_color);
    // §17.3.2.32 / §17.3.2.15: shading or highlight as background.
    let shading = effective_props
        .shading
        .as_ref()
        .map(|s| {
            crate::render::resolve::color::resolve_color(
                s.fill,
                crate::render::resolve::color::ColorContext::Background,
            )
        })
        // §17.18.40: HighlightColor::None is the explicit "no highlight"
        // override and yields no fill, so use `and_then` to thread the
        // Option through.
        .or_else(|| effective_props.highlight.and_then(resolve_highlight_color));

    // §17.3.2.42: vertical alignment (super/sub).
    let mut baseline_offset = match effective_props.vertical_align {
        Some(VerticalAlign::Superscript) => {
            let (_, base_m) = measure_text("X", &font);
            font.size = font.size * SUPERSCRIPT_FONT_SIZE_RATIO;
            -(base_m.ascent * SUPERSCRIPT_ASCENT_OFFSET_RATIO)
        }
        Some(VerticalAlign::Subscript) => {
            let (_, base_m) = measure_text("X", &font);
            font.size = font.size * SUPERSCRIPT_FONT_SIZE_RATIO;
            base_m.height() * SUBSCRIPT_HEIGHT_OFFSET_RATIO
        }
        _ => Pt::ZERO,
    };
    // §17.3.2.19: w:position — vertical baseline offset in half-points.
    if let Some(pos) = effective_props.position {
        baseline_offset += Pt::from(pos);
    }

    // §17.3.2.4: run-level border (filtered to drop the no-border styles).
    let border = run_border_to_fragment(effective_props.border.as_ref());

    let text_style = TextRunStyle {
        color,
        shading,
        border,
        baseline_offset,
    };
    (font, text_style)
}

/// §17.16.4.1: context for evaluating dynamic fields (PAGE, NUMPAGES).
#[derive(Clone, Copy, Default)]
pub struct FieldContext {
    /// Current page number (1-based).
    pub page_number: Option<usize>,
    /// Total page count in the document.
    pub num_pages: Option<usize>,
}

/// §17.16.4.1: evaluate a parsed field instruction against the current context.
/// Returns the substituted text for PAGE/NUMPAGES, or None for other fields
/// or when no context is available.
fn evaluate_field_instruction(
    instruction: &crate::field::FieldInstruction,
    ctx: FieldContext,
) -> Option<String> {
    match instruction {
        crate::field::FieldInstruction::Page { .. } => ctx.page_number.map(|n| n.to_string()),
        crate::field::FieldInstruction::NumPages { .. } => ctx.num_pages.map(|n| n.to_string()),
        _ => None,
    }
}

/// Return the visible display text carried by a legacy `MACROBUTTON` field.
///
/// Word uses these fields as editable placeholders in many Chinese thesis
/// templates.  The placeholder is stored in the field *instruction* rather
/// than in a result run, and these fields commonly omit `separate`, so the
/// normal dynamic-field path would discard it.  A macro cannot be executed by
/// a PDF renderer; showing its display argument is the safe and useful Word
/// compatible fallback.  Other unknown fields continue to use their existing
/// result content (if any).
fn fallback_field_instruction_text(instruction: &crate::field::FieldInstruction) -> Option<String> {
    let crate::field::FieldInstruction::Unknown { field_type, raw } = instruction else {
        return None;
    };
    if !field_type.eq_ignore_ascii_case("MACROBUTTON") {
        return None;
    }

    // `MACROBUTTON <macro-name> <display-text>`; preserve whitespace inside
    // the display text, while tolerating the extra spacing emitted by Word.
    let mut parts = raw.trim().splitn(2, char::is_whitespace);
    let _keyword = parts.next()?;
    let rest = parts.next()?.trim_start();
    let mut macro_and_display = rest.splitn(2, char::is_whitespace);
    let _macro_name = macro_and_display.next()?;
    let display = macro_and_display.next()?.trim();
    if display.is_empty() {
        return None;
    }

    // Some producers quote the display argument.  Word's bracketed Chinese
    // placeholders are not quotes and must remain visible.
    let display = display
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .unwrap_or(display);
    (!display.is_empty()).then(|| display.to_owned())
}

/// Pick a safe fallback face for field text when the field has no result run
/// from which to inherit `w:rPr`. Legacy Chinese templates put their visible
/// placeholder in `MACROBUTTON` instruction text, so the paragraph's Latin
/// default is not sufficient for Han characters in that display string.
fn field_fallback_family<'a>(text: &str, default_family: &'a str) -> &'a str {
    if text
        .chars()
        .any(crate::render::resolve::fonts::is_east_asian_char)
    {
        "SimSun"
    } else {
        default_family
    }
}

/// §17.16.19 MERGEFORMAT — source of formatting for a complex field's
/// substituted dynamic value. Resolved when the `Separate` fldChar is
/// reached so the lookup honors the OOXML "first result run wins" rule
/// regardless of how the inline-units pre-pass packaged the result zone:
/// an empty `<w:t></w:t>` placeholder run carries `<w:rPr>` but does not
/// surface as its own unit (segment joining drops it as 0 chars), yet
/// is still the spec's first-result-run for formatting purposes.
#[derive(Clone, Copy)]
pub(super) enum FieldFormatSource<'a> {
    /// First TextRun encountered between `Separate` and the matching
    /// `End` at the outer field's nesting level. Its `<w:rPr>` provides
    /// font family, size, bold, italic, color per §17.16.19.
    FirstResultRun(&'a TextRun),
    /// Run containing the first visible character of a legacy MACROBUTTON's
    /// display argument. Such fields commonly have no result zone, so the
    /// instruction run is the only faithful source of character formatting.
    InstructionDisplayRun(&'a TextRun),
    /// No result TextRun is present at the outer level. The
    /// substitution falls back to paragraph default font properties at
    /// emission time.
    ParagraphDefaults,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ComplexFieldPhase {
    Instruction,
    Result,
}

/// Mutable state for one level of a complex field.
///
/// Complex fields may be nested inside another field's result zone. Keeping a
/// frame per level prevents the inner `Begin`/`Separate`/`End` sequence from
/// overwriting the outer instruction, substitution, and formatting source.
struct ComplexFieldFrame<'a> {
    phase: ComplexFieldPhase,
    instruction: String,
    instruction_format_spans: Vec<(std::ops::Range<usize>, &'a TextRun)>,
    substitution_pending: Option<String>,
    substitution_emitted: bool,
    had_separate: bool,
    format_source: Option<FieldFormatSource<'a>>,
}

impl ComplexFieldFrame<'_> {
    fn new() -> Self {
        Self {
            phase: ComplexFieldPhase::Instruction,
            instruction: String::new(),
            instruction_format_spans: Vec::new(),
            substitution_pending: None,
            substitution_emitted: false,
            had_separate: false,
            format_source: None,
        }
    }
}

/// Whether an enclosing field suppresses the current field's visible result.
///
/// A dynamic outer field replaces its complete result zone, including any
/// nested field. An outer instruction zone is never visible either. The last
/// frame is intentionally excluded because its pending substitution is the
/// value the caller is about to emit.
fn enclosing_field_suppresses(frames: &[ComplexFieldFrame<'_>]) -> bool {
    frames
        .get(..frames.len().saturating_sub(1))
        .unwrap_or_default()
        .iter()
        .any(|frame| {
            frame.phase == ComplexFieldPhase::Instruction
                || frame.substitution_pending.is_some()
                || frame.substitution_emitted
        })
}

fn instruction_display_format_source<'a>(
    instruction: &str,
    display: &str,
    spans: &[(std::ops::Range<usize>, &'a TextRun)],
) -> Option<FieldFormatSource<'a>> {
    let display_start = instruction.find(display)?;
    spans
        .iter()
        .find(|(range, _)| range.contains(&display_start))
        .map(|(_, run)| FieldFormatSource::InstructionDisplayRun(*run))
}

/// Locate the formatting source for the complex field whose `Separate`
/// fldChar sits at `inlines[separate_idx]`. Walks raw inlines (not
/// unit-packaged), tracking nesting via `Begin` / `End` counts, and
/// returns at the first top-level `TextRun` or at the matching `End`,
/// whichever comes first.
///
/// "Top level" = the depth of the field whose `Separate` triggered the
/// lookup. Text runs that sit inside a nested field's own result zone
/// belong to that nested field's substitution and are skipped — they
/// are not the outer field's first result run.
///
/// Malformed input (no matching `End`) returns `ParagraphDefaults`
/// rather than panicking.
pub(super) fn resolve_field_format_source(
    inlines: &[Inline],
    separate_idx: usize,
) -> FieldFormatSource<'_> {
    let mut depth: i32 = 0;
    for inline in &inlines[separate_idx + 1..] {
        match inline {
            Inline::FieldChar(fc) => match fc.field_char_type {
                FieldCharType::Begin => depth += 1,
                FieldCharType::End => {
                    if depth == 0 {
                        return FieldFormatSource::ParagraphDefaults;
                    }
                    depth -= 1;
                }
                FieldCharType::Separate => {
                    // Belongs to a nested field; the outer scan ignores it.
                }
            },
            Inline::TextRun(tr) if depth == 0 => {
                return FieldFormatSource::FirstResultRun(tr.as_ref());
            }
            _ => {}
        }
    }
    FieldFormatSource::ParagraphDefaults
}

/// Emit the substituted text of a complex field using the formatting
/// resolved at `Separate` (§17.16.19). When the source is
/// [`FieldFormatSource::FirstResultRun`] the substitution inherits font
/// family, size, bold, italic, color, etc. from that run's `<w:rPr>` —
/// matching what Word renders when it updates a dynamic field in place.
/// When no result run was present in the field zone the substitution
/// falls back to paragraph defaults.
#[allow(clippy::too_many_arguments)]
fn emit_field_substitution<F>(
    text: &str,
    source: Option<&FieldFormatSource<'_>>,
    default_family: &str,
    default_size: Pt,
    default_color: RgbColor,
    resolved_styles: Option<
        &std::collections::HashMap<
            crate::model::StyleId,
            crate::render::resolve::styles::ResolvedStyle,
        >,
    >,
    paragraph_run_defaults: Option<&RunProperties>,
    theme: Option<&crate::model::Theme>,
    auto_fit: crate::render::layout::ShapeAutoFit,
    hyperlink_url: Option<&LinkTarget>,
    measure_text: &F,
    measurer: Option<&crate::render::layout::measurer::TextMeasurer<'_>>,
    fragments: &mut Vec<Fragment>,
) where
    F: Fn(&str, &FontProps) -> (Pt, TextMetrics),
{
    let source_run = match source {
        Some(
            FieldFormatSource::FirstResultRun(tr) | FieldFormatSource::InstructionDisplayRun(tr),
        ) => Some(*tr),
        _ => None,
    };
    // A legacy MACROBUTTON often has no result run at all. Keep the old
    // paragraph-default fallback for model callers that do not preserve the
    // instruction run, but prefer the actual source run whenever available.
    let synthetic = paragraph_run_defaults.map(|props| TextRun {
        style_id: None,
        properties: props.clone(),
        content: Vec::new(),
        rsids: crate::model::RevisionIds::default(),
    });

    if let Some(run) = source_run.or(synthetic.as_ref()) {
        // Character formatting comes from one source run, while the font
        // family still follows OOXML's per-script slots. This is why a single
        // bold placeholder can use SimSun for Han text and Times New Roman for
        // brackets/digits, matching normal run rendering.
        for text_part in split_text_for_font_slots(text) {
            let (font, text_style) = resolve_run_styling(
                run,
                default_family,
                default_size,
                default_color,
                resolved_styles,
                paragraph_run_defaults,
                theme,
                auto_fit,
                Some(text_part),
                measure_text,
            );
            emit_text_fragments(
                text_part,
                &font,
                &text_style,
                hyperlink_url,
                measure_text,
                measurer,
                fragments,
            );
        }
        return;
    }

    let font = FontProps {
        family: Rc::from(field_fallback_family(text, default_family)),
        size: auto_fit.scale_font(default_size),
        bold: false,
        italic: false,
        underline: false,
        char_spacing: Pt::ZERO,
        text_scale: 1.0,
        underline_position: Pt::ZERO,
        underline_thickness: Pt::ZERO,
    };
    let text_style = TextRunStyle {
        color: default_color,
        shading: None,
        border: None,
        baseline_offset: Pt::ZERO,
    };
    emit_text_fragments(
        text,
        &font,
        &text_style,
        hyperlink_url,
        measure_text,
        measurer,
        fragments,
    );
}

/// Build a text fragment for a substituted field value, using the paragraph's
/// default font properties.
fn make_field_text_fragment<F>(
    text: Rc<str>,
    default_family: &str,
    default_size: Pt,
    default_color: crate::render::resolve::color::RgbColor,
    measure_text: &F,
) -> Fragment
where
    F: Fn(&str, &FontProps) -> (Pt, TextMetrics),
{
    let font = FontProps {
        family: Rc::from(field_fallback_family(&text, default_family)),
        size: default_size,
        bold: false,
        italic: false,
        underline: false,
        char_spacing: Pt::ZERO,
        text_scale: 1.0,
        underline_position: Pt::ZERO,
        underline_thickness: Pt::ZERO,
    };
    let (w, m) = measure_text(&text, &font);
    Fragment::Text {
        text,
        font: Rc::new(font),
        color: default_color,
        shading: None,
        border: None,
        width: w,
        trimmed_width: w,
        metrics: m,
        hyperlink_url: None,
        baseline_offset: Pt::ZERO,
        text_offset: Pt::ZERO,
        is_footnote_ref: false,
    }
}

/// Invariant context threaded through all recursive `collect_fragments` calls.
pub struct FragmentCtx<'a> {
    pub default_family: &'a str,
    pub default_size: Pt,
    pub default_color: RgbColor,
    pub resolved_styles: Option<
        &'a std::collections::HashMap<
            crate::model::StyleId,
            crate::render::resolve::styles::ResolvedStyle,
        >,
    >,
    pub paragraph_run_defaults: Option<&'a RunProperties>,
    pub theme: Option<&'a crate::model::Theme>,
    /// Measurer used by the emoji pipeline for typeface resolution and
    /// raster-backend metrics. `None` disables the emoji path entirely —
    /// callers without a font registry (most unit tests) pass `None` and
    /// emoji codepoints flow through the existing text path unchanged.
    pub measurer: Option<&'a crate::render::layout::measurer::TextMeasurer<'a>>,
    /// §20.1.2.1.18: the `a:normAutofit` shrink of the enclosing shape text
    /// body. [`ShapeAutoFit::NONE`] everywhere else.
    ///
    /// [`ShapeAutoFit::NONE`]: crate::render::layout::ShapeAutoFit::NONE
    pub auto_fit: crate::render::layout::ShapeAutoFit,
}

/// Walk inline content and collect fragments.
/// `measure_text` is a callback that measures text width/height/ascent for a given font.
/// `resolved_styles` is used to look up character styles (w:rStyle) on text runs.
///
/// Returns fragments suitable for the line-fitting algorithm.
pub fn collect_fragments<F>(
    inlines: &[Inline],
    ctx: &FragmentCtx<'_>,
    hyperlink_url: Option<&LinkTarget>,
    measure_text: &F,
    footnotes: &mut FootnoteTracker,
    endnote_counter: &mut u32,
    field_ctx: FieldContext,
) -> Vec<Fragment>
where
    F: Fn(&str, &FontProps) -> (Pt, TextMetrics), // (width, metrics)
{
    let default_family = ctx.default_family;
    let default_size = ctx.default_size;
    let default_color = ctx.default_color;
    let resolved_styles = ctx.resolved_styles;
    let paragraph_run_defaults = ctx.paragraph_run_defaults;
    let theme = ctx.theme;
    let auto_fit = ctx.auto_fit;
    let mut fragments = Vec::new();
    let mut field_stack: Vec<ComplexFieldFrame<'_>> = Vec::new();

    // §17.16.19 MERGEFORMAT — pre-resolve formatting for each complex
    // field's substitution against raw inlines, so empty placeholder
    // result runs (`<w:t></w:t>` — swallowed by `build_inline_units`
    // because they contribute 0 chars) still surface their `<w:rPr>`.
    // One entry per `Separate` fldChar, consumed in order.
    let field_format_sources: Vec<FieldFormatSource<'_>> = inlines
        .iter()
        .enumerate()
        .filter_map(|(idx, inl)| match inl {
            Inline::FieldChar(fc) if matches!(fc.field_char_type, FieldCharType::Separate) => {
                Some(resolve_field_format_source(inlines, idx))
            }
            _ => None,
        })
        .collect();
    let mut field_format_idx: usize = 0;
    // Pre-pass: join consecutive text-only TextRuns into segments so
    // UAX #29 grapheme clusters reassemble across `<w:rFonts>`-induced
    // run splits (keycap `1️⃣`, ZWJ family, modifier sequence, …).
    let units = build_inline_units(inlines);
    for unit in units {
        match unit {
            InlineUnit::TextSegment(seg) => {
                // Field state (mirrors the per-run logic below). Field chars
                // appear as Discrete Inlines and break segment joining, so
                // a TextSegment is always entirely inside one field zone.
                if let Some(frame) = field_stack.last() {
                    if enclosing_field_suppresses(&field_stack)
                        || frame.phase == ComplexFieldPhase::Instruction
                        || frame.substitution_emitted
                    {
                        continue;
                    }
                }

                // §17.16.19: a dynamic substitution replaces the field's
                // complete result zone. Prefer the pre-resolved first result
                // run (which may be an empty run dropped by segment joining),
                // then fall back to this segment's first run.
                if field_stack
                    .last()
                    .is_some_and(|frame| frame.substitution_pending.is_some())
                {
                    let base_run = seg.char_runs()[0];
                    let source = match field_stack.last().and_then(|frame| frame.format_source) {
                        Some(
                            source @ (FieldFormatSource::FirstResultRun(_)
                            | FieldFormatSource::InstructionDisplayRun(_)),
                        ) => source,
                        Some(FieldFormatSource::ParagraphDefaults) | None => {
                            FieldFormatSource::FirstResultRun(base_run)
                        }
                    };
                    let sub = field_stack
                        .last_mut()
                        .and_then(|frame| frame.substitution_pending.take())
                        .expect("pending substitution checked above");
                    emit_field_substitution(
                        &sub,
                        Some(&source),
                        default_family,
                        default_size,
                        default_color,
                        resolved_styles,
                        paragraph_run_defaults,
                        theme,
                        auto_fit,
                        hyperlink_url,
                        measure_text,
                        ctx.measurer,
                        &mut fragments,
                    );
                    if let Some(frame) = field_stack.last_mut() {
                        frame.substitution_emitted = true;
                    }
                    continue;
                }

                // Normal segment: classify and emit each piece using its
                // own (or for emoji, base) run's resolved styling.
                for piece in seg.classify() {
                    match piece {
                        SegmentPiece::Text { run, text } => {
                            for text_part in split_text_for_font_slots(&text) {
                                let (font, text_style) = resolve_run_styling(
                                    run,
                                    default_family,
                                    default_size,
                                    default_color,
                                    resolved_styles,
                                    paragraph_run_defaults,
                                    theme,
                                    auto_fit,
                                    Some(text_part),
                                    measure_text,
                                );
                                // Pre-classified text: bypass cluster::classify
                                // by going straight to the word-split path.
                                emit_text_words(
                                    text_part,
                                    &font,
                                    &text_style,
                                    hyperlink_url,
                                    measure_text,
                                    &mut fragments,
                                );
                            }
                        }
                        SegmentPiece::Emoji {
                            base_run,
                            text,
                            presentation,
                            structure,
                        } => {
                            let (font, text_style) = resolve_run_styling(
                                base_run,
                                default_family,
                                default_size,
                                default_color,
                                resolved_styles,
                                paragraph_run_defaults,
                                theme,
                                auto_fit,
                                Some(&text),
                                measure_text,
                            );
                            if let Some(measurer) = ctx.measurer {
                                let cluster = EmojiCluster {
                                    text: &text,
                                    presentation,
                                    structure,
                                };
                                emit_emoji_or_fallback(
                                    &cluster,
                                    &font,
                                    &text_style,
                                    hyperlink_url,
                                    measure_text,
                                    measurer,
                                    &mut fragments,
                                );
                            } else {
                                // No measurer (test path): fall through to
                                // text — the cluster's codepoints survive
                                // in the PDF text stream verbatim.
                                emit_text_words(
                                    &text,
                                    &font,
                                    &text_style,
                                    hyperlink_url,
                                    measure_text,
                                    &mut fragments,
                                );
                            }
                        }
                    }
                }
            }
            InlineUnit::Discrete(inline) => match inline {
                Inline::TextRun(tr) => {
                    // A text-only TextRun would have been a TextSegment; this
                    // branch handles runs whose content includes Tab,
                    // LineBreak, PageBreak, ColumnBreak, or
                    // LastRenderedPageBreak.
                    if let Some(frame) = field_stack.last() {
                        if enclosing_field_suppresses(&field_stack)
                            || frame.phase == ComplexFieldPhase::Instruction
                            || frame.substitution_emitted
                        {
                            continue;
                        }
                    }

                    if field_stack
                        .last()
                        .is_some_and(|frame| frame.substitution_pending.is_some())
                    {
                        let source = match field_stack.last().and_then(|frame| frame.format_source)
                        {
                            Some(
                                source @ (FieldFormatSource::FirstResultRun(_)
                                | FieldFormatSource::InstructionDisplayRun(_)),
                            ) => source,
                            Some(FieldFormatSource::ParagraphDefaults) | None => {
                                FieldFormatSource::FirstResultRun(tr)
                            }
                        };
                        if let Some(sub) = field_stack
                            .last_mut()
                            .and_then(|frame| frame.substitution_pending.take())
                        {
                            emit_field_substitution(
                                &sub,
                                Some(&source),
                                default_family,
                                default_size,
                                default_color,
                                resolved_styles,
                                paragraph_run_defaults,
                                theme,
                                auto_fit,
                                hyperlink_url,
                                measure_text,
                                ctx.measurer,
                                &mut fragments,
                            );
                            if let Some(frame) = field_stack.last_mut() {
                                frame.substitution_emitted = true;
                            }
                        }
                    } else {
                        let (font, text_style) = resolve_run_styling(
                            tr,
                            default_family,
                            default_size,
                            default_color,
                            resolved_styles,
                            paragraph_run_defaults,
                            theme,
                            auto_fit,
                            None,
                            measure_text,
                        );
                        for element in &tr.content {
                            match element {
                                RunElement::Text(text) => {
                                    emit_text_fragments(
                                        text,
                                        &font,
                                        &text_style,
                                        hyperlink_url,
                                        measure_text,
                                        ctx.measurer,
                                        &mut fragments,
                                    );
                                }
                                RunElement::Tab => {
                                    fragments.push(Fragment::Tab {
                                        line_height: font.size,
                                        // §17.3.1.38: a leader on this tab is
                                        // drawn in the tab run's own formatting.
                                        font: Rc::new(font.clone()),
                                        color: text_style.color,
                                        fitting_width: None,
                                    });
                                }
                                RunElement::PositionTab(ptab) => {
                                    fragments.push(Fragment::PTab {
                                        align: ptab.alignment,
                                        relative_to: ptab.relative_to,
                                        leader: ptab.leader.into(),
                                        line_height: font.size,
                                        font: Rc::new(font.clone()),
                                        color: text_style.color,
                                    });
                                }
                                RunElement::LineBreak(_) => {
                                    fragments.push(Fragment::LineBreak {
                                        line_height: font.size,
                                    });
                                }
                                RunElement::PageBreak => {
                                    fragments.push(Fragment::PageBreak {
                                        line_height: font.size,
                                    });
                                }
                                RunElement::ColumnBreak => {
                                    fragments.push(Fragment::ColumnBreak);
                                }
                                RunElement::LastRenderedPageBreak => {}
                            }
                        }
                    }
                }
                Inline::Image(img) => {
                    // Only render INLINE images as fragments.
                    // Anchor (floating) images are handled separately in build.rs.
                    if matches!(img.placement, crate::model::ImagePlacement::Inline { .. }) {
                        if let Some(rel_id) =
                            crate::render::resolve::images::extract_image_rel_id(img)
                        {
                            let w = Pt::from(img.extent.width);
                            let h = Pt::from(img.extent.height);
                            fragments.push(Fragment::Image {
                                size: PtSize::new(w, h),
                                rel_id: rel_id.as_str().to_string(),
                                image_data: None,
                                src_rect: crate::render::resolve::images::extract_src_rect(img),
                            });
                        }
                    }
                }
                Inline::Hyperlink(link) => {
                    // Preserve the external/internal kind as a closed ADT so
                    // the emitter routes external→URI and internal→GoTo without
                    // guessing from the string (§17.16.22).
                    // One allocation per `w:hyperlink`, shared by every word
                    // fragment inside it and every command they emit.
                    let target: Option<LinkTarget> = match &link.target {
                        crate::model::HyperlinkTarget::ExternalUrl(url) => {
                            Some(LinkTarget::External(Rc::from(url.as_str())))
                        }
                        crate::model::HyperlinkTarget::Internal { anchor } => {
                            Some(LinkTarget::Internal(Rc::from(anchor.as_str())))
                        }
                        // An unresolved rId (no matching relationship) has no link.
                        crate::model::HyperlinkTarget::ExternalRel(_) => None,
                    };
                    let mut sub = collect_fragments(
                        &link.content,
                        ctx,
                        target.as_ref(),
                        measure_text,
                        footnotes,
                        endnote_counter,
                        field_ctx,
                    );
                    fragments.append(&mut sub);
                }
                Inline::Field(field) => {
                    // §17.16.18: simple field — check for dynamic substitution.
                    let substituted = evaluate_field_instruction(&field.instruction, field_ctx);
                    if let Some(text) = substituted {
                        fragments.push(make_field_text_fragment(
                            Rc::from(text.as_str()),
                            default_family,
                            default_size,
                            default_color,
                            measure_text,
                        ));
                    } else {
                        let mut sub = collect_fragments(
                            &field.content,
                            ctx,
                            hyperlink_url,
                            measure_text,
                            footnotes,
                            endnote_counter,
                            field_ctx,
                        );
                        fragments.append(&mut sub);
                    }
                }
                Inline::FieldChar(fc) => {
                    // §17.16.18: complex field state machine:
                    // Begin → InstrText... → Separate → result runs → End
                    match fc.field_char_type {
                        FieldCharType::Begin => {
                            field_stack.push(ComplexFieldFrame::new());
                        }
                        FieldCharType::Separate => {
                            let format_source = field_format_sources.get(field_format_idx).copied();
                            field_format_idx += 1;
                            // §17.16.4.1: parse accumulated instruction, evaluate
                            // PAGE/NUMPAGES if field context is available.
                            if let Some(frame) = field_stack.last_mut() {
                                if let Ok(parsed) = crate::field::parse(&frame.instruction) {
                                    frame.substitution_pending =
                                        evaluate_field_instruction(&parsed, field_ctx)
                                            .or_else(|| fallback_field_instruction_text(&parsed));
                                }
                                frame.had_separate = true;
                                frame.format_source = format_source;
                                frame.phase = ComplexFieldPhase::Result;
                            }
                        }
                        FieldCharType::End => {
                            let Some(mut frame) = field_stack.pop() else {
                                continue;
                            };
                            // Legacy MACROBUTTON placeholders often have no
                            // `separate` marker at all.  Their visible text is
                            // the instruction's display argument, so recover
                            // it before closing the field instead of dropping
                            // the whole placeholder.
                            if !frame.had_separate && frame.substitution_pending.is_none() {
                                if let Ok(parsed) = crate::field::parse(&frame.instruction) {
                                    if let Some(text) = fallback_field_instruction_text(&parsed) {
                                        frame.format_source = instruction_display_format_source(
                                            &frame.instruction,
                                            &text,
                                            &frame.instruction_format_spans,
                                        );
                                        frame.substitution_pending = Some(text);
                                    }
                                }
                            }
                            // Substitution still pending at End: the unit
                            // stream never carried a result run (either the
                            // placeholder was empty and got swallowed by
                            // segment joining, or the field has no result
                            // content at all). Use the pre-resolved format
                            // source — §17.16.19 first-result-run when
                            // present, paragraph defaults otherwise.
                            let suppressed_by_enclosing = field_stack.iter().any(|outer| {
                                outer.phase == ComplexFieldPhase::Instruction
                                    || outer.substitution_pending.is_some()
                                    || outer.substitution_emitted
                            });
                            if !suppressed_by_enclosing {
                                if let Some(text) = frame.substitution_pending.take() {
                                    emit_field_substitution(
                                        &text,
                                        frame.format_source.as_ref(),
                                        default_family,
                                        default_size,
                                        default_color,
                                        resolved_styles,
                                        paragraph_run_defaults,
                                        theme,
                                        auto_fit,
                                        hyperlink_url,
                                        measure_text,
                                        ctx.measurer,
                                        &mut fragments,
                                    );
                                }
                            }
                        }
                    }
                }
                Inline::InstrText(text) => {
                    // Accumulate instruction text for complex field parsing.
                    if let Some(frame) = field_stack
                        .last_mut()
                        .filter(|frame| frame.phase == ComplexFieldPhase::Instruction)
                    {
                        frame.instruction.push_str(text);
                    }
                }
                Inline::InstrTextRun(run) => {
                    // Preserve byte ranges back to the source runs so a
                    // MACROBUTTON display value can use the format of its
                    // first visible character, even when the instruction is
                    // split across several differently formatted runs.
                    if let Some(frame) = field_stack
                        .last_mut()
                        .filter(|frame| frame.phase == ComplexFieldPhase::Instruction)
                    {
                        let start = frame.instruction.len();
                        for element in &run.content {
                            if let RunElement::Text(text) = element {
                                frame.instruction.push_str(text);
                            }
                        }
                        let end = frame.instruction.len();
                        if end > start {
                            frame
                                .instruction_format_spans
                                .push((start..end, run.as_ref()));
                        }
                    }
                }
                Inline::AlternateContent(ac) => {
                    use crate::render::layout::{live_mc_branch, McBranch};
                    // §M.1.2 / §17.17.1: only the live branch contributes, and
                    // `live_mc_branch` is the one place that decides which.
                    //
                    // A drawable Choice is drawn as float geometry by the
                    // floating extractor — and for a wps shape, its `txbx`
                    // contents are laid out into shape-local commands emitted
                    // on top of the shape's path. Walking the Fallback here as
                    // well would duplicate that text into the host paragraph at
                    // the wrong y.
                    //
                    // A live Fallback does come through here, and must: its VML
                    // geometry goes to the float walkers, its text box to this
                    // collector, landing at the host paragraph y as the Tier 0
                    // placeholder it has always been.
                    match live_mc_branch(ac) {
                        McBranch::Fallback(fallback) => {
                            let mut sub = collect_fragments(
                                fallback,
                                ctx,
                                hyperlink_url,
                                measure_text,
                                footnotes,
                                endnote_counter,
                                field_ctx,
                            );
                            fragments.append(&mut sub);
                        }
                        McBranch::Choices(_) | McBranch::Neither => {}
                    }
                }
                Inline::Symbol(sym) => {
                    let font = FontProps {
                        family: Rc::from(sym.font.as_str()),
                        size: default_size,
                        bold: false,
                        italic: false,
                        underline: false,
                        char_spacing: Pt::ZERO,
                        text_scale: 1.0,
                        underline_position: Pt::ZERO,
                        underline_thickness: Pt::ZERO,
                    };
                    let ch = char::from_u32(sym.char_code as u32).unwrap_or('\u{FFFD}');
                    let text = ch.to_string();
                    let (w, m) = measure_text(&text, &font);
                    fragments.push(Fragment::Text {
                        text: Rc::from(text.as_str()),
                        font: Rc::new(font),
                        color: RgbColor::BLACK,
                        shading: None,
                        border: None,
                        width: w,
                        trimmed_width: w,
                        metrics: m,
                        hyperlink_url: hyperlink_url.cloned(),
                        baseline_offset: Pt::ZERO,
                        text_offset: Pt::ZERO,
                        is_footnote_ref: false,
                    });
                }
                // Bookmark target — emit as zero-width named destination.
                Inline::BookmarkStart { name, .. } => {
                    fragments.push(Fragment::Bookmark { name: name.clone() });
                }
                // Non-visual inlines — skip
                Inline::BookmarkEnd(_)
                | Inline::Separator
                | Inline::ContinuationSeparator
                | Inline::FootnoteRefMark
                | Inline::EndnoteRefMark => {}
                // §17.11.12: footnote reference — render as superscript number.
                Inline::FootnoteRef(note_id) => {
                    let num_text = format!("{}", footnotes.record(*note_id));
                    // §17.11.12: footnote reference uses superscript sizing.
                    let ref_size = default_size * super::SUPERSCRIPT_FONT_SIZE_RATIO;
                    let ref_font = FontProps {
                        family: std::rc::Rc::from(default_family),
                        size: ref_size,
                        bold: false,
                        italic: false,
                        underline: false,
                        char_spacing: Pt::ZERO,
                        text_scale: 1.0,
                        underline_position: Pt::ZERO,
                        underline_thickness: Pt::ZERO,
                    };
                    let (w, m) = measure_text(&num_text, &ref_font);
                    // Raise the mark clear of the baseline (see the constant).
                    let baseline_offset = -(default_size * super::NOTE_REF_BASELINE_OFFSET_RATIO);
                    fragments.push(Fragment::Text {
                        text: Rc::from(num_text.as_str()),
                        font: Rc::new(ref_font),
                        color: default_color,
                        shading: None,
                        border: None,
                        width: w,
                        trimmed_width: w,
                        metrics: m,
                        hyperlink_url: None,
                        baseline_offset,
                        text_offset: Pt::ZERO,
                        // §17.11.12: tag so across-page splitting reserves this
                        // footnote on the page its reference mark lands on.
                        is_footnote_ref: true,
                    });
                }
                // §17.11.2: endnote reference — render as superscript Roman numeral.
                Inline::EndnoteRef(_note_id) => {
                    *endnote_counter += 1;
                    let num_text = to_roman_lower(*endnote_counter);
                    let ref_size = default_size * super::SUPERSCRIPT_FONT_SIZE_RATIO;
                    let ref_font = FontProps {
                        family: std::rc::Rc::from(default_family),
                        size: ref_size,
                        bold: false,
                        italic: false,
                        underline: false,
                        char_spacing: Pt::ZERO,
                        text_scale: 1.0,
                        underline_position: Pt::ZERO,
                        underline_thickness: Pt::ZERO,
                    };
                    let (w, m) = measure_text(&num_text, &ref_font);
                    let baseline_offset = -(default_size * super::NOTE_REF_BASELINE_OFFSET_RATIO);
                    fragments.push(Fragment::Text {
                        text: Rc::from(num_text.as_str()),
                        font: Rc::new(ref_font),
                        color: default_color,
                        shading: None,
                        border: None,
                        width: w,
                        trimmed_width: w,
                        metrics: m,
                        hyperlink_url: None,
                        baseline_offset,
                        text_offset: Pt::ZERO,
                        is_footnote_ref: false,
                    });
                }
                Inline::Pict(pict) => {
                    let mut emitted_group = false;
                    for primitive in &pict.primitives {
                        if let crate::model::VmlPrimitive::Group(group) = primitive {
                            if let Some(size) =
                                crate::render::layout::vml::inline_group_extent(group)
                            {
                                fragments.push(Fragment::InlineGraphic {
                                    size,
                                    source: Rc::new((**group).clone()),
                                    commands: Vec::new(),
                                });
                                emitted_group = true;
                            }
                        }
                    }

                    // Render text content from VML text-box-bearing
                    // primitives inline. Every primitive variant
                    // (`<v:shape>`, `<v:rect>`, `<v:roundrect>`,
                    // `<v:oval>`, …) admits a `<v:textbox>` child via
                    // `VmlCommonAttrs.text_box`; the previous code
                    // only walked the `Shape` variant and silently
                    // dropped text from rect / roundrect / oval text
                    // boxes (the case footer3.xml of the vorlage doc
                    // exercised — the gray bar is a `<v:rect>`).
                    //
                    // Does not handle absolute positioning — text
                    // appears inline with the surrounding paragraph.
                    if !emitted_group {
                        for primitive in &pict.primitives {
                            let common = primitive.common();
                            if common.style.position == Some(crate::model::CssPosition::Absolute) {
                                continue;
                            }
                            if let Some(ref text_box) = common.text_box {
                                for block in &text_box.content {
                                    if let Block::Paragraph(p) = block {
                                        let pict_ctx = FragmentCtx {
                                            default_family,
                                            default_size,
                                            default_color,
                                            resolved_styles,
                                            paragraph_run_defaults: p.mark_run_properties.as_ref(),
                                            theme,
                                            measurer: ctx.measurer,
                                            auto_fit: ctx.auto_fit,
                                        };
                                        let mut sub = collect_fragments(
                                            &p.content,
                                            &pict_ctx,
                                            hyperlink_url,
                                            measure_text,
                                            footnotes,
                                            endnote_counter,
                                            field_ctx,
                                        );
                                        fragments.append(&mut sub);
                                    }
                                }
                            }
                        }
                    }
                }
            },
        }
    }

    fragments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::dimension::{Dimension, HalfPoints};
    use crate::model::*;

    #[test]
    fn mixed_latin_and_chinese_text_splits_by_font_slot() {
        assert_eq!(
            split_text_for_font_slots("ABC 123：中文，XYZ"),
            vec!["ABC 123：", "中文，", "XYZ"]
        );
        assert_eq!(split_text_for_font_slots("（中文）"), vec!["（中文）"]);
    }

    /// Dummy measurer: width = text.len() * 6.0, ascent = 10.0, descent = 2.0
    fn dummy_measure(text: &str, _font: &FontProps) -> (Pt, TextMetrics) {
        (
            Pt::new(text.len() as f32 * 6.0),
            TextMetrics {
                ascent: Pt::new(10.0),
                descent: Pt::new(2.0),
                leading: Pt::ZERO,
            },
        )
    }

    fn default_ctx(size: f32) -> FragmentCtx<'static> {
        FragmentCtx {
            default_family: "Default",
            default_size: Pt::new(size),
            default_color: RgbColor::BLACK,
            resolved_styles: None,
            paragraph_run_defaults: None,
            theme: None,
            measurer: None,
            auto_fit: crate::render::layout::ShapeAutoFit::NONE,
        }
    }

    // ── §17.3.2.4 / §17.18.2 run-level border tri-state ─────────────────
    //
    // The cascade may carry a child run whose `<w:bdr w:val="nil"/>`
    // (or "none") explicitly turns off an inherited border. The model
    // preserves this as `Some(Border { style: nil-or-none, .. })`
    // so the §17.7.2 merge can distinguish "explicit no border" from
    // "field absent → inherit". At the render boundary we must drop the
    // sentinel; otherwise the painter draws a hairline box around every
    // word in any Word-saved doc (Word emits `<w:bdr w:val="nil"/>` in
    // the default rPrDefault for the entire document).

    fn border_with_style(style: BorderStyle) -> crate::model::Border {
        crate::model::Border {
            style,
            width: Dimension::new(0),
            space: Dimension::new(0),
            color: crate::model::Color::Auto,
        }
    }

    #[test]
    fn run_border_absent_yields_no_fragment_border() {
        assert!(run_border_to_fragment(None).is_none());
    }

    #[test]
    fn run_border_explicit_none_yields_no_fragment_border() {
        let b = border_with_style(BorderStyle::None);
        assert!(
            run_border_to_fragment(Some(&b)).is_none(),
            "<w:bdr w:val=\"nil\"/> / \"none\" must NOT produce a render-side border"
        );
    }

    #[test]
    fn run_border_actual_style_yields_fragment_border() {
        let b = border_with_style(BorderStyle::Single);
        assert!(
            run_border_to_fragment(Some(&b)).is_some(),
            "explicit Single border must reach the painter"
        );
    }

    fn text_run(text: &str) -> Inline {
        Inline::TextRun(Box::new(TextRun {
            style_id: None,
            properties: RunProperties::default(),
            content: vec![RunElement::Text(text.into())],
            rsids: RevisionIds::default(),
        }))
    }

    fn instr_text_run(text: &str, bold: bool) -> Inline {
        Inline::InstrTextRun(Box::new(TextRun {
            style_id: None,
            properties: RunProperties {
                bold: Some(bold),
                ..Default::default()
            },
            content: vec![RunElement::Text(text.into())],
            rsids: RevisionIds::default(),
        }))
    }

    fn text_run_with_font(text: &str, font: &str, size: i64) -> Inline {
        Inline::TextRun(Box::new(TextRun {
            style_id: None,
            properties: RunProperties {
                fonts: FontSet {
                    ascii: FontSlot::from_name(font),
                    ..Default::default()
                },
                font_size: Some(Dimension::<HalfPoints>::new(size)),
                ..Default::default()
            },
            content: vec![RunElement::Text(text.into())],
            rsids: RevisionIds::default(),
        }))
    }

    #[test]
    fn single_text_run() {
        let inlines = vec![text_run("hello")];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].width().raw(), 30.0); // 5 * 6
        assert_eq!(frags[0].height().raw(), 12.0);
    }

    #[test]
    fn text_run_uses_run_font() {
        let inlines = vec![text_run_with_font("hi", "Arial", 24)];
        let ctx = default_ctx(10.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        if let Fragment::Text { font, .. } = &frags[0] {
            assert_eq!(&*font.family, "Arial");
            assert_eq!(font.size.raw(), 12.0); // 24 half-points = 12pt
        } else {
            panic!("expected Text fragment");
        }
    }

    #[test]
    fn tab_produces_tab_fragment() {
        let inlines = vec![Inline::TextRun(Box::new(TextRun {
            style_id: None,
            properties: RunProperties::default(),
            content: vec![RunElement::Tab],
            rsids: RevisionIds::default(),
        }))];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        assert_eq!(frags.len(), 1);
        assert!(matches!(frags[0], Fragment::Tab { .. }));
    }

    #[test]
    fn position_tab_produces_ptab_fragment() {
        use crate::model::{PTabAlignment, PTabLeader, PTabRelativeTo, PositionTab};
        let inlines = vec![Inline::TextRun(Box::new(TextRun {
            style_id: None,
            properties: RunProperties::default(),
            content: vec![RunElement::PositionTab(PositionTab {
                alignment: PTabAlignment::Right,
                relative_to: PTabRelativeTo::Margin,
                leader: PTabLeader::Dot,
            })],
            rsids: RevisionIds::default(),
        }))];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        assert_eq!(frags.len(), 1);
        assert!(matches!(
            frags[0],
            Fragment::PTab {
                align: PTabAlignment::Right,
                relative_to: PTabRelativeTo::Margin,
                leader: crate::model::TabLeader::Dot,
                ..
            }
        ));
    }

    #[test]
    fn line_break_produces_break_fragment() {
        let inlines = vec![Inline::TextRun(Box::new(TextRun {
            style_id: None,
            properties: RunProperties::default(),
            content: vec![RunElement::LineBreak(BreakKind::TextWrapping)],
            rsids: RevisionIds::default(),
        }))];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        assert_eq!(frags.len(), 1);
        assert!(frags[0].is_line_break());
    }

    #[test]
    fn hyperlink_recurses_into_content() {
        let inlines = vec![Inline::Hyperlink(Hyperlink {
            target: HyperlinkTarget::ExternalUrl("https://example.com".into()),
            content: vec![text_run("click me")],
        })];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        assert_eq!(frags.len(), 2, "split into 'click ' and 'me'");
        if let Fragment::Text {
            hyperlink_url,
            text,
            ..
        } = &frags[0]
        {
            assert_eq!(&**text, "click ");
            assert_eq!(
                hyperlink_url,
                &Some(LinkTarget::External("https://example.com".into()))
            );
        } else {
            panic!("expected Text fragment");
        }
    }

    #[test]
    fn complex_field_skips_instructions_collects_result() {
        // FieldChar::Begin -> InstrText("PAGE") -> FieldChar::Separate -> TextRun("3") -> FieldChar::End
        let inlines = vec![
            Inline::FieldChar(FieldChar {
                field_char_type: FieldCharType::Begin,
                dirty: None,
                fld_lock: None,
            }),
            Inline::InstrText("PAGE".into()),
            Inline::FieldChar(FieldChar {
                field_char_type: FieldCharType::Separate,
                dirty: None,
                fld_lock: None,
            }),
            text_run("3"),
            Inline::FieldChar(FieldChar {
                field_char_type: FieldCharType::End,
                dirty: None,
                fld_lock: None,
            }),
        ];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        // Should only have the "3" result, not "PAGE"
        assert_eq!(frags.len(), 1);
        if let Fragment::Text { text, .. } = &frags[0] {
            assert_eq!(&**text, "3");
        }
    }

    #[test]
    fn outer_dynamic_field_suppresses_a_nested_result_field() {
        let inlines = vec![
            fld_char(FieldCharType::Begin),
            Inline::InstrText("PAGE".into()),
            fld_char(FieldCharType::Separate),
            fld_char(FieldCharType::Begin),
            Inline::InstrText("NUMPAGES".into()),
            fld_char(FieldCharType::Separate),
            text_run("old inner result"),
            fld_char(FieldCharType::End),
            text_run("old outer result"),
            fld_char(FieldCharType::End),
        ];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext {
                page_number: Some(7),
                num_pages: Some(99),
            },
        );
        let rendered: String = frags
            .iter()
            .filter_map(|fragment| match fragment {
                Fragment::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(rendered, "7");
    }

    #[test]
    fn unknown_outer_field_preserves_a_nested_dynamic_field() {
        let inlines = vec![
            fld_char(FieldCharType::Begin),
            Inline::InstrText("UNKNOWN".into()),
            fld_char(FieldCharType::Separate),
            text_run("before "),
            fld_char(FieldCharType::Begin),
            Inline::InstrText("PAGE".into()),
            fld_char(FieldCharType::Separate),
            text_run("old page"),
            fld_char(FieldCharType::End),
            text_run(" after"),
            fld_char(FieldCharType::End),
        ];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext {
                page_number: Some(7),
                num_pages: None,
            },
        );
        let rendered: String = frags
            .iter()
            .filter_map(|fragment| match fragment {
                Fragment::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(rendered, "before 7 after");
    }

    #[test]
    fn legacy_field_without_separate_does_not_hide_following_text() {
        let inlines = vec![
            fld_char(FieldCharType::Begin),
            Inline::InstrText(" FORMCHECKBOX ".into()),
            fld_char(FieldCharType::End),
            text_run("visible after control"),
        ];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        let rendered: String = frags
            .iter()
            .filter_map(|fragment| match fragment {
                Fragment::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(rendered, "visible after control");
    }

    #[test]
    fn macrobutton_without_separate_renders_its_display_argument() {
        let inlines = vec![
            fld_char(FieldCharType::Begin),
            Inline::InstrText(" MACROBUTTON  AcceptAllChangesShown ".into()),
            Inline::InstrText("［点此输入中文论文题目］".into()),
            fld_char(FieldCharType::End),
        ];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );
        let rendered: String = frags
            .iter()
            .filter_map(|fragment| match fragment {
                Fragment::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(rendered, "［点此输入中文论文题目］");
    }

    #[test]
    fn macrobutton_uses_the_display_arguments_first_character_format() {
        let inlines = vec![
            fld_char(FieldCharType::Begin),
            instr_text_run(" MACROBUTTON AcceptAllChangesShown ", false),
            instr_text_run("[", true),
            instr_text_run("placeholder]", false),
            fld_char(FieldCharType::End),
        ];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        assert!(frags.iter().all(|fragment| match fragment {
            Fragment::Text { font, .. } => font.bold,
            _ => true,
        }));
    }

    #[test]
    fn macrobutton_with_separate_and_empty_result_uses_display_argument() {
        let inlines = vec![
            fld_char(FieldCharType::Begin),
            Inline::InstrText("MACROBUTTON AcceptAllChangesShown [placeholder]".into()),
            fld_char(FieldCharType::Separate),
            text_run(""),
            fld_char(FieldCharType::End),
        ];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );
        let rendered: String = frags
            .iter()
            .filter_map(|fragment| match fragment {
                Fragment::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(rendered, "[placeholder]");
    }

    #[test]
    fn bookmarks_and_separators_skipped() {
        let inlines = vec![
            Inline::BookmarkStart {
                id: BookmarkId::new(1),
                name: "bm1".into(),
            },
            text_run("visible"),
            Inline::BookmarkEnd(BookmarkId::new(1)),
            Inline::Separator,
            Inline::ContinuationSeparator,
            Inline::FootnoteRefMark,
            Inline::EndnoteRefMark,
            // LastRenderedPageBreak is now inside RunElement, not Inline
        ];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        // BookmarkStart produces a Bookmark fragment, text run produces a Text fragment.
        assert_eq!(
            frags.len(),
            2,
            "bookmark + text run should produce fragments"
        );
        assert!(matches!(frags[0], Fragment::Bookmark { .. }));
        assert!(matches!(frags[1], Fragment::Text { .. }));
    }

    #[test]
    fn alternate_content_uses_fallback() {
        let inlines = vec![Inline::AlternateContent(AlternateContent {
            choices: vec![McChoice {
                requires: vec![McRequires::Wps],
                content: vec![text_run("choice")],
            }],
            fallback: Some(vec![text_run("fallback")]),
        })];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        assert_eq!(frags.len(), 1);
        if let Fragment::Text { text, .. } = &frags[0] {
            assert_eq!(&**text, "fallback");
        }
    }

    #[test]
    fn empty_text_run_produces_no_fragment() {
        let inlines = vec![Inline::TextRun(Box::new(TextRun {
            style_id: None,
            properties: RunProperties::default(),
            content: vec![RunElement::Text(String::new())],
            rsids: RevisionIds::default(),
        }))];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );
        assert!(frags.is_empty());
    }

    #[test]
    fn symbol_produces_text_fragment() {
        let inlines = vec![Inline::Symbol(Symbol {
            font: "Wingdings".into(),
            char_code: 0x46, // 'F'
        })];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        assert_eq!(frags.len(), 1);
        if let Fragment::Text { font, text, .. } = &frags[0] {
            assert_eq!(&*font.family, "Wingdings");
            assert_eq!(&**text, "F");
        }
    }

    #[test]
    fn simple_field_collects_content() {
        let inlines = vec![Inline::Field(Field {
            instruction: crate::field::FieldInstruction::Page {
                switches: Default::default(),
            },
            content: vec![text_run("5")],
        })];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        assert_eq!(frags.len(), 1);
        if let Fragment::Text { text, .. } = &frags[0] {
            assert_eq!(&**text, "5");
        }
    }

    #[test]
    fn multi_word_text_run_splits_into_fragments() {
        let inlines = vec![text_run("hello world foo")];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext::default(),
        );

        assert_eq!(frags.len(), 3);
        if let Fragment::Text { text, .. } = &frags[0] {
            assert_eq!(&**text, "hello ");
        }
        if let Fragment::Text { text, .. } = &frags[1] {
            assert_eq!(&**text, "world ");
        }
        if let Fragment::Text { text, .. } = &frags[2] {
            assert_eq!(&**text, "foo");
        }
    }

    // ── §17.16.19 MERGEFORMAT — field result format source ───────────────
    //
    // `resolve_field_format_source` walks raw inlines forward from a
    // `Separate` fldChar to locate the formatting that should be applied
    // to a substituted dynamic value (PAGE/NUMPAGES/...). Decoupling
    // this from `build_inline_units` is the key correctness property:
    // an empty `<w:t></w:t>` placeholder result run carries `<w:rPr>`
    // but is swallowed by segment joining (it contributes 0 chars), so
    // we cannot rely on units to surface it.

    fn fld_char(kind: FieldCharType) -> Inline {
        Inline::FieldChar(FieldChar {
            field_char_type: kind,
            dirty: None,
            fld_lock: None,
        })
    }

    fn bold_text_run(text: &str) -> Inline {
        Inline::TextRun(Box::new(TextRun {
            style_id: None,
            properties: RunProperties {
                bold: Some(true),
                ..Default::default()
            },
            content: vec![RunElement::Text(text.into())],
            rsids: RevisionIds::default(),
        }))
    }

    /// Helper: pull `&TextRun` out of `FieldFormatSource::FirstResultRun`,
    /// or panic with a descriptive message.
    fn expect_first_run<'a>(src: FieldFormatSource<'a>) -> &'a TextRun {
        match src {
            FieldFormatSource::FirstResultRun(tr) => tr,
            FieldFormatSource::InstructionDisplayRun(_) => {
                panic!("expected FirstResultRun, got InstructionDisplayRun")
            }
            FieldFormatSource::ParagraphDefaults => {
                panic!("expected FirstResultRun, got ParagraphDefaults")
            }
        }
    }

    /// Extract the concatenated text from a TextRun's content. Lets the
    /// assertions read like prose without depending on `PartialEq` for
    /// the `RunElement` ADT.
    fn run_text(tr: &TextRun) -> String {
        tr.content
            .iter()
            .filter_map(|e| match e {
                RunElement::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Canonical complex field shape with a non-empty result run.
    /// Inline layout: `[Begin, InstrText, Separate, TextRun("3"), End]`.
    #[test]
    fn format_source_finds_text_run_after_separate() {
        let inlines = vec![
            fld_char(FieldCharType::Begin),
            Inline::InstrText("PAGE".into()),
            fld_char(FieldCharType::Separate),
            bold_text_run("3"),
            fld_char(FieldCharType::End),
        ];
        let src = resolve_field_format_source(&inlines, 2);
        let tr = expect_first_run(src);
        assert_eq!(run_text(tr), "3");
        assert_eq!(tr.properties.bold, Some(true));
    }

    /// The original bug: an empty `<w:t></w:t>` placeholder result run
    /// must still be discoverable as the format source, because its
    /// `<w:rPr>` is the only place the substitution can find its
    /// formatting.
    #[test]
    fn format_source_finds_empty_placeholder_result_run() {
        let inlines = vec![
            fld_char(FieldCharType::Begin),
            Inline::InstrText("PAGE".into()),
            fld_char(FieldCharType::Separate),
            bold_text_run(""), // empty placeholder, bold rPr
            fld_char(FieldCharType::End),
        ];
        let src = resolve_field_format_source(&inlines, 2);
        let tr = expect_first_run(src);
        assert!(run_text(tr).is_empty(), "expected empty text content");
        assert_eq!(
            tr.properties.bold,
            Some(true),
            "empty placeholder's bold rPr must be reachable"
        );
    }

    /// §17.16.19 — the FIRST result run wins when multiple are present.
    #[test]
    fn format_source_uses_first_when_multiple_result_runs() {
        let inlines = vec![
            fld_char(FieldCharType::Begin),
            Inline::InstrText("PAGE".into()),
            fld_char(FieldCharType::Separate),
            bold_text_run("first"),
            text_run("second"), // not bold
            fld_char(FieldCharType::End),
        ];
        let src = resolve_field_format_source(&inlines, 2);
        let tr = expect_first_run(src);
        assert_eq!(run_text(tr), "first");
        assert_eq!(tr.properties.bold, Some(true));
    }

    /// `Separate` immediately followed by `End` — no result run exists,
    /// so the resolver returns `ParagraphDefaults` and the substitution
    /// will fall back to paragraph defaults at emission time.
    #[test]
    fn format_source_returns_defaults_for_empty_result_zone() {
        let inlines = vec![
            fld_char(FieldCharType::Begin),
            Inline::InstrText("PAGE".into()),
            fld_char(FieldCharType::Separate),
            fld_char(FieldCharType::End),
        ];
        let src = resolve_field_format_source(&inlines, 2);
        assert!(matches!(src, FieldFormatSource::ParagraphDefaults));
    }

    /// Text runs inside a NESTED field's own result zone belong to that
    /// nested field's substitution, not the outer one's. The outer
    /// resolver must skip them and look for runs at its own nesting
    /// level — here, `outer_run` after the nested End.
    #[test]
    fn format_source_skips_runs_inside_nested_field() {
        let inlines = vec![
            fld_char(FieldCharType::Begin), // outer
            Inline::InstrText("OUTER".into()),
            fld_char(FieldCharType::Separate), // index 2
            // ── nested field at depth 1 ──
            fld_char(FieldCharType::Begin),
            Inline::InstrText("INNER".into()),
            fld_char(FieldCharType::Separate),
            bold_text_run("inner-result"), // inside nested zone — skip
            fld_char(FieldCharType::End),
            // ── back at outer's level ──
            bold_text_run("outer-result"), // this is the one we want
            fld_char(FieldCharType::End),
        ];
        let src = resolve_field_format_source(&inlines, 2);
        let tr = expect_first_run(src);
        assert_eq!(
            run_text(tr),
            "outer-result",
            "must skip runs inside nested field"
        );
    }

    /// Content past the matching `End` belongs to a later field (or to
    /// the surrounding paragraph). The resolver stops at `End` and does
    /// not leak formatting from outside.
    #[test]
    fn format_source_stops_at_matching_end() {
        let inlines = vec![
            fld_char(FieldCharType::Begin),
            Inline::InstrText("PAGE".into()),
            fld_char(FieldCharType::Separate), // index 2
            fld_char(FieldCharType::End),
            bold_text_run("trailing"), // outside the field — must not be picked up
        ];
        let src = resolve_field_format_source(&inlines, 2);
        assert!(
            matches!(src, FieldFormatSource::ParagraphDefaults),
            "trailing run after End must not become the source"
        );
    }

    /// Malformed inlines: no matching `End` after `Separate`. Treat as
    /// "no result" rather than panicking or returning a partial result.
    #[test]
    fn format_source_handles_missing_end_gracefully() {
        let inlines = vec![
            fld_char(FieldCharType::Begin),
            Inline::InstrText("PAGE".into()),
            fld_char(FieldCharType::Separate), // index 2
                                               // (no End — malformed)
        ];
        let src = resolve_field_format_source(&inlines, 2);
        // No TextRun present and no End either — defaults is correct.
        assert!(matches!(src, FieldFormatSource::ParagraphDefaults));
    }

    // ── End-to-end: empty-placeholder result run formatting ──────────────
    //
    // Mirrors the Fotodokumentation Test header structure:
    //   `Seite ` [Begin → InstrText("PAGE") → Separate → <w:t></w:t> bold → End]
    // The substituted page number must inherit bold from the empty
    // placeholder result run's `<w:rPr>`.

    #[test]
    fn page_field_with_empty_placeholder_inherits_bold() {
        let inlines = vec![
            bold_text_run("Seite "),
            fld_char(FieldCharType::Begin),
            Inline::InstrText("PAGE".into()),
            fld_char(FieldCharType::Separate),
            bold_text_run(""), // placeholder result with bold rPr
            fld_char(FieldCharType::End),
        ];
        let ctx = default_ctx(12.0);
        let frags = collect_fragments(
            &inlines,
            &ctx,
            None,
            &dummy_measure,
            &mut FootnoteTracker::default(),
            &mut 0,
            FieldContext {
                page_number: Some(7),
                num_pages: None,
            },
        );
        // Two visible text fragments: "Seite " and the substituted "7".
        let texts: Vec<(&str, bool)> = frags
            .iter()
            .filter_map(|f| match f {
                Fragment::Text { text, font, .. } => Some((text.as_ref(), font.bold)),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec![("Seite ", true), ("7", true)],
            "PAGE substitution must inherit bold from empty placeholder run"
        );
    }
}
