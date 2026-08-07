//! Font collection — extract all font families referenced in the document.

use std::collections::HashSet;

use crate::model::{
    Block, Document, FontSet, FontSlot, Inline, Lang, ScriptTag, Theme, ThemeFontRef,
    ThemeFontScheme,
};

/// Collect all unique font family names referenced in the document.
/// Sources: theme, style sheet, inline content, numbering levels, paragraph marks.
pub fn collect_font_families(doc: &Document) -> Vec<String> {
    let mut families = HashSet::new();

    // Theme fonts
    if let Some(ref theme) = doc.theme {
        add_nonempty(&mut families, &theme.major_font.latin);
        add_nonempty(&mut families, &theme.minor_font.latin);
        add_nonempty(&mut families, &theme.major_font.east_asian);
        add_nonempty(&mut families, &theme.minor_font.east_asian);
        add_nonempty(&mut families, &theme.major_font.complex_script);
        add_nonempty(&mut families, &theme.minor_font.complex_script);
        for sf in &theme.major_font.script_fonts {
            add_nonempty(&mut families, &sf.typeface);
        }
        for sf in &theme.minor_font.script_fonts {
            add_nonempty(&mut families, &sf.typeface);
        }
    }

    // Style sheet defaults
    collect_from_fontset(&mut families, &doc.styles.doc_defaults_run.fonts);

    // Style definitions
    for style in doc.styles.styles.values() {
        if let Some(ref rp) = style.run_properties {
            collect_from_fontset(&mut families, &rp.fonts);
        }
    }

    // Body content
    collect_from_blocks(&mut families, &doc.body);

    // Headers and footers
    for blocks in doc.headers.values() {
        collect_from_blocks(&mut families, blocks);
    }
    for blocks in doc.footers.values() {
        collect_from_blocks(&mut families, blocks);
    }

    // Footnotes and endnotes
    for blocks in doc.footnotes.values() {
        collect_from_blocks(&mut families, blocks);
    }
    for blocks in doc.endnotes.values() {
        collect_from_blocks(&mut families, blocks);
    }

    // Numbering levels
    for abs in doc.numbering.abstract_nums.values() {
        for level in &abs.levels {
            if let Some(ref rp) = level.run_properties {
                collect_from_fontset(&mut families, &rp.fonts);
            }
        }
    }

    families.into_iter().collect()
}

/// Extract the effective font family from a FontSet.
///
/// Returns the first `explicit` name that is set, in priority order:
/// ascii > high_ansi > east_asian > complex_script (§17.3.2.26).
/// By the time this is called, theme references will have been resolved
/// into the `explicit` field by [`resolve_font_set_themes`].
pub fn effective_font(fonts: &FontSet) -> Option<&str> {
    fonts
        .ascii
        .explicit
        .as_deref()
        .or(fonts.high_ansi.explicit.as_deref())
        .or(fonts.east_asian.explicit.as_deref())
        .or(fonts.complex_script.explicit.as_deref())
}

/// Select the OOXML font slot for actual text instead of applying the ASCII
/// slot to every character in a run. Word can store Latin and Han characters
/// in the same `<w:r>`, with `ascii`/`hAnsi` and `eastAsia` naming different
/// faces (§17.3.2.26).
pub fn effective_font_for_text(
    fonts: &FontSet,
    text: &str,
    lang: Option<&Lang>,
    theme: Option<&Theme>,
) -> Option<String> {
    let east_asian = text.chars().any(is_east_asian_char);
    let slots = if east_asian {
        [
            &fonts.east_asian,
            &fonts.ascii,
            &fonts.high_ansi,
            &fonts.complex_script,
        ]
    } else {
        [
            &fonts.ascii,
            &fonts.high_ansi,
            &fonts.east_asian,
            &fonts.complex_script,
        ]
    };
    let script = east_asian.then(|| infer_east_asian_script(text, lang));

    let selected = slots
        .into_iter()
        .find_map(|slot| resolve_font_slot(slot, theme, script.as_ref()));

    selected.or_else(|| script.as_ref().map(default_east_asian_script_font))
}

/// Strong East Asian characters used both for slot selection and for splitting
/// a mixed-script run. Punctuation outside these ranges inherits its adjacent
/// strong script in the caller.
pub(crate) fn is_east_asian_char(ch: char) -> bool {
    matches!(
        ch as u32,
        0x2E80..=0x2FFF
            | 0x3000..=0x303F
            | 0x3040..=0x30FF
            | 0x3100..=0x312F
            | 0x31A0..=0x31BF
            | 0x31F0..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xAC00..=0xD7AF
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFFEF
            | 0x20000..=0x2FA1F
    )
}

fn resolve_font_slot(
    slot: &FontSlot,
    theme: Option<&Theme>,
    script: Option<&ScriptTag>,
) -> Option<String> {
    if let Some(name) = slot.explicit.as_ref().filter(|name| !name.is_empty()) {
        return Some(name.clone());
    }
    let theme_ref = slot.theme.as_ref()?;
    if let Some(theme) = theme {
        if let Some(name) = resolve_theme_font_ref(theme_ref, theme) {
            return Some(name);
        }
        if let Some(script) = script {
            let scheme = theme_scheme(theme_ref, theme);
            if let Some(font) = scheme
                .script_fonts
                .iter()
                .find(|font| &font.script == script && !font.typeface.is_empty())
            {
                return Some(font.typeface.clone());
            }
        }
    }
    default_theme_font(theme_ref, script).map(str::to_owned)
}

fn theme_scheme<'a>(theme_ref: &ThemeFontRef, theme: &'a Theme) -> &'a ThemeFontScheme {
    match theme_ref {
        ThemeFontRef::MajorHAnsi | ThemeFontRef::MajorEastAsia | ThemeFontRef::MajorBidi => {
            &theme.major_font
        }
        ThemeFontRef::MinorHAnsi | ThemeFontRef::MinorEastAsia | ThemeFontRef::MinorBidi => {
            &theme.minor_font
        }
    }
}

fn infer_east_asian_script(text: &str, lang: Option<&Lang>) -> ScriptTag {
    if text
        .chars()
        .any(|ch| matches!(ch as u32, 0x3040..=0x30FF | 0x31F0..=0x31FF))
    {
        return ScriptTag::Jpan;
    }
    if text.chars().any(|ch| matches!(ch as u32, 0xAC00..=0xD7AF)) {
        return ScriptTag::Hang;
    }
    if text
        .chars()
        .any(|ch| matches!(ch as u32, 0x3100..=0x312F | 0x31A0..=0x31BF))
    {
        return ScriptTag::Hant;
    }

    let language = lang
        .and_then(|value| value.east_asia.as_deref())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if language.starts_with("ja") {
        ScriptTag::Jpan
    } else if language.starts_with("ko") {
        ScriptTag::Hang
    } else if language.starts_with("zh-tw")
        || language.starts_with("zh-hk")
        || language.starts_with("zh-mo")
        || language.contains("hant")
    {
        ScriptTag::Hant
    } else {
        // Han without a language tag is ambiguous. The Office fallback used by
        // the Chinese business fixtures is Hans/SimSun, and matches Word's
        // materialized theme when those documents are opened and saved.
        ScriptTag::Hans
    }
}

fn default_theme_font(
    theme_ref: &ThemeFontRef,
    script: Option<&ScriptTag>,
) -> Option<&'static str> {
    match theme_ref {
        ThemeFontRef::MajorHAnsi => Some("Cambria"),
        ThemeFontRef::MinorHAnsi => Some("Calibri"),
        ThemeFontRef::MajorBidi | ThemeFontRef::MinorBidi => Some("Arial"),
        ThemeFontRef::MajorEastAsia | ThemeFontRef::MinorEastAsia => match script {
            Some(ScriptTag::Jpan) => Some("Yu Mincho"),
            Some(ScriptTag::Hang) => Some("Malgun Gothic"),
            Some(ScriptTag::Hant) => Some("PMingLiU"),
            Some(_) => Some("SimSun"),
            None => None,
        },
    }
}

fn default_east_asian_script_font(script: &ScriptTag) -> String {
    match script {
        ScriptTag::Jpan => "Yu Mincho",
        ScriptTag::Hang => "Malgun Gothic",
        ScriptTag::Hant => "PMingLiU",
        ScriptTag::Hans => "SimSun",
        _ => "SimSun",
    }
    .to_owned()
}

/// §17.3.2.26: resolve theme font references in a FontSet.
///
/// For each slot that carries a theme reference, look up the concrete font
/// family name from the theme and write it into `slot.explicit`, overwriting
/// any explicit name — theme references take precedence per §17.3.2.26.
pub fn resolve_font_set_themes(fonts: &mut FontSet, theme: &crate::model::Theme) {
    for slot in [
        &mut fonts.ascii,
        &mut fonts.high_ansi,
        &mut fonts.east_asian,
        &mut fonts.complex_script,
    ] {
        if let Some(ref tf) = slot.theme {
            if let Some(name) = resolve_theme_font_ref(tf, theme) {
                slot.explicit = Some(name);
            }
        }
    }
}

fn resolve_theme_font_ref(
    tf: &crate::model::ThemeFontRef,
    theme: &crate::model::Theme,
) -> Option<String> {
    use crate::model::ThemeFontRef;
    let name = match tf {
        ThemeFontRef::MajorHAnsi => &theme.major_font.latin,
        ThemeFontRef::MajorEastAsia => &theme.major_font.east_asian,
        ThemeFontRef::MajorBidi => &theme.major_font.complex_script,
        ThemeFontRef::MinorHAnsi => &theme.minor_font.latin,
        ThemeFontRef::MinorEastAsia => &theme.minor_font.east_asian,
        ThemeFontRef::MinorBidi => &theme.minor_font.complex_script,
    };
    if name.is_empty() {
        None
    } else {
        Some(name.clone())
    }
}

fn add_nonempty(set: &mut HashSet<String>, s: &str) {
    if !s.is_empty() {
        set.insert(s.to_string());
    }
}

fn collect_from_fontset(set: &mut HashSet<String>, fonts: &FontSet) {
    for slot in [
        &fonts.ascii,
        &fonts.high_ansi,
        &fonts.east_asian,
        &fonts.complex_script,
    ] {
        if let Some(ref f) = slot.explicit {
            add_nonempty(set, f);
        }
    }
}

fn collect_from_blocks(set: &mut HashSet<String>, blocks: &[Block]) {
    for block in blocks {
        match block {
            Block::Paragraph(p) => {
                if let Some(ref mrp) = p.mark_run_properties {
                    collect_from_fontset(set, &mrp.fonts);
                }
                collect_from_inlines(set, &p.content);
            }
            Block::Table(t) => {
                for row in &t.rows {
                    for cell in &row.cells {
                        collect_from_blocks(set, &cell.content);
                    }
                }
            }
            Block::SectionBreak(_) => {}
        }
    }
}

fn collect_from_inlines(set: &mut HashSet<String>, inlines: &[Inline]) {
    for inline in inlines {
        match inline {
            Inline::TextRun(tr) => {
                collect_from_fontset(set, &tr.properties.fonts);
            }
            Inline::Hyperlink(h) => {
                collect_from_inlines(set, &h.content);
            }
            Inline::Field(f) => {
                collect_from_inlines(set, &f.content);
            }
            Inline::AlternateContent(ac) => {
                for choice in &ac.choices {
                    collect_from_inlines(set, &choice.content);
                }
                if let Some(ref fb) = ac.fallback {
                    collect_from_inlines(set, fb);
                }
            }
            _ => {}
        }
    }
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

    fn text_run(font: &str, text: &str) -> Inline {
        Inline::TextRun(Box::new(TextRun {
            style_id: None,
            properties: RunProperties {
                fonts: FontSet {
                    ascii: FontSlot::from_name(font),
                    ..Default::default()
                },
                ..Default::default()
            },
            content: vec![RunElement::Text(text.into())],
            rsids: RevisionIds::default(),
        }))
    }

    fn para_with_run(font: &str) -> Block {
        Block::Paragraph(Box::new(Paragraph {
            style_id: None,
            properties: ParagraphProperties::default(),
            mark_run_properties: None,
            content: vec![text_run(font, "hello")],
            rsids: ParagraphRevisionIds::default(),
        }))
    }

    // ── effective_font ───────────────────────────────────────────────────

    #[test]
    fn effective_font_prefers_ascii() {
        let fs = FontSet {
            ascii: FontSlot::from_name("Arial"),
            high_ansi: FontSlot::from_name("Times"),
            ..Default::default()
        };
        assert_eq!(effective_font(&fs), Some("Arial"));
    }

    #[test]
    fn effective_font_falls_back_to_high_ansi() {
        let fs = FontSet {
            high_ansi: FontSlot::from_name("Times"),
            ..Default::default()
        };
        assert_eq!(effective_font(&fs), Some("Times"));
    }

    #[test]
    fn effective_font_falls_back_to_east_asian() {
        let fs = FontSet {
            east_asian: FontSlot::from_name("SimSun"),
            ..Default::default()
        };
        assert_eq!(effective_font(&fs), Some("SimSun"));
    }

    #[test]
    fn effective_font_empty_returns_none() {
        let fs = FontSet::default();
        assert_eq!(effective_font(&fs), None);
    }

    #[test]
    fn effective_font_uses_hans_script_font_for_chinese_text() {
        let fonts = FontSet {
            ascii: FontSlot {
                explicit: None,
                theme: Some(ThemeFontRef::MinorHAnsi),
            },
            east_asian: FontSlot {
                explicit: None,
                theme: Some(ThemeFontRef::MinorEastAsia),
            },
            ..Default::default()
        };
        let theme = Theme {
            minor_font: ThemeFontScheme {
                latin: "Calibri".into(),
                east_asian: String::new(),
                script_fonts: vec![ThemeScriptFont {
                    script: ScriptTag::Hans,
                    typeface: "SimSun".into(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            effective_font_for_text(&fonts, "中文", None, Some(&theme)).as_deref(),
            Some("SimSun")
        );
        assert_eq!(
            effective_font_for_text(&fonts, "Latin", None, Some(&theme)).as_deref(),
            Some("Calibri")
        );
    }

    #[test]
    fn missing_theme_uses_office_hans_fallback() {
        let fonts = FontSet {
            east_asian: FontSlot {
                explicit: None,
                theme: Some(ThemeFontRef::MinorEastAsia),
            },
            ..Default::default()
        };
        assert_eq!(
            effective_font_for_text(&fonts, "中文", None, None).as_deref(),
            Some("SimSun")
        );
    }

    #[test]
    fn empty_font_set_uses_script_default_only_for_east_asian_text() {
        let fonts = FontSet::default();
        assert_eq!(
            effective_font_for_text(&fonts, "中文", None, None).as_deref(),
            Some("SimSun")
        );
        assert_eq!(effective_font_for_text(&fonts, "Latin", None, None), None);
    }

    // ── collect_font_families ────────────────────────────────────────────

    #[test]
    fn collects_from_body_text_runs() {
        let mut doc = empty_doc();
        doc.body = vec![para_with_run("Calibri"), para_with_run("Arial")];

        let families = collect_font_families(&doc);
        assert!(families.contains(&"Calibri".to_string()));
        assert!(families.contains(&"Arial".to_string()));
    }

    #[test]
    fn collects_from_style_sheet_defaults() {
        let mut doc = empty_doc();
        doc.styles.doc_defaults_run = RunProperties {
            fonts: FontSet {
                ascii: FontSlot::from_name("Cambria"),
                ..Default::default()
            },
            ..Default::default()
        };

        let families = collect_font_families(&doc);
        assert!(families.contains(&"Cambria".to_string()));
    }

    #[test]
    fn collects_from_theme_fonts() {
        let mut doc = empty_doc();
        doc.theme = Some(Theme {
            major_font: ThemeFontScheme {
                latin: "Calibri Light".into(),
                ..Default::default()
            },
            minor_font: ThemeFontScheme {
                latin: "Calibri".into(),
                ..Default::default()
            },
            ..Default::default()
        });

        let families = collect_font_families(&doc);
        assert!(families.contains(&"Calibri Light".to_string()));
        assert!(families.contains(&"Calibri".to_string()));
    }

    #[test]
    fn collects_from_headers() {
        let mut doc = empty_doc();
        let header_id = RelId::new("rId1");
        doc.headers
            .insert(header_id, vec![para_with_run("Georgia")]);

        let families = collect_font_families(&doc);
        assert!(families.contains(&"Georgia".to_string()));
    }

    #[test]
    fn deduplicates() {
        let mut doc = empty_doc();
        doc.body = vec![para_with_run("Arial"), para_with_run("Arial")];

        let families = collect_font_families(&doc);
        let count = families.iter().filter(|f| *f == "Arial").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn empty_doc_returns_empty() {
        let doc = empty_doc();
        let families = collect_font_families(&doc);
        assert!(families.is_empty());
    }

    #[test]
    fn collects_from_style_definitions() {
        let mut doc = empty_doc();
        doc.styles.styles.insert(
            StyleId::new("Heading1"),
            Style {
                name: None,
                style_type: StyleType::Paragraph,
                based_on: None,
                is_default: false,
                paragraph_properties: None,
                run_properties: Some(RunProperties {
                    fonts: FontSet {
                        ascii: FontSlot::from_name("Verdana"),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                table_properties: None,
                table_style_overrides: vec![],
            },
        );

        let families = collect_font_families(&doc);
        assert!(families.contains(&"Verdana".to_string()));
    }
}
