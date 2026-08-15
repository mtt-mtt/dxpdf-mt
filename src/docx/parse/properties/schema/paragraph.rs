//! `<w:pPr>` schema (§17.3.1 paragraph properties).
//!
//! Entry point: `PPrXml::split()` returns `ParsedParagraphProperties` —
//! direct formatting, style id, mark-run properties, and an optional
//! nested `<w:sectPr>` (§17.6.18 "last-paragraph-of-section" marker).

use serde::Deserialize;

use crate::docx::model::dimension::{Dimension, HundredthChars, Twips};
use crate::docx::model::{
    Alignment, CnfStyle, DropCap, FirstLineIndent, FirstLineIndentChars, FrameKind, FrameWrap,
    HeightRule, Indentation, LineSpacing, NumberingReference, OutlineLevel, ParagraphBorders,
    ParagraphProperties, ParagraphSpacing, RunProperties, Shading, StyleId, TabStop, TextAlignment,
    TextBoxPositioning,
};
use crate::docx::parse::primitives::st_enums::{
    StAnchor, StFrameWrap, StHeightRule, StJc, StLineSpacingRule, StTextAlignment, StXAlign,
    StYAlign,
};
use crate::docx::parse::primitives::units::deserialize_optional_nonnegative_dimension;
use crate::docx::parse::primitives::{last_toggle, OnOff};

use super::border::ParagraphBordersXml;
use super::cnf_style::CnfStyleXml;
use super::run::RPrXml;
use super::section::SectPrXml;
use super::shading::ShdXml;
use super::tabs::TabsXml;

/// All the artifacts produced by deserializing a `<w:pPr>`. The split
/// mirrors the legacy `ParsedParagraphProperties` so it plugs into the
/// existing resolve pipeline unchanged.
pub(crate) struct ParsedPPr {
    pub properties: ParagraphProperties,
    pub style_id: Option<StyleId>,
    pub run_properties: Option<RunProperties>,
    pub section_properties: Option<crate::docx::model::SectionProperties>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct PPrXml {
    #[serde(rename = "pStyle", default)]
    p_style: Option<ValString>,
    #[serde(default)]
    ind: Option<IndXml>,
    #[serde(default)]
    spacing: Option<SpacingXml>,
    #[serde(default)]
    jc: Option<ValAttr<StJc>>,
    #[serde(default)]
    shd: Option<ShdXml>,
    #[serde(rename = "outlineLvl", default)]
    outline_lvl: Option<ValAttr<u8>>,
    #[serde(rename = "numPr", default)]
    num_pr: Option<NumPrXml>,
    #[serde(default)]
    tabs: Option<TabsXml>,
    #[serde(rename = "pBdr", default)]
    p_bdr: Option<ParagraphBordersXml>,
    #[serde(rename = "rPr", default)]
    r_pr: Option<RPrXml>,
    #[serde(rename = "sectPr", default)]
    sect_pr: Option<SectPrXml>,
    #[serde(rename = "textAlignment", default)]
    text_alignment: Option<ValAttr<StTextAlignment>>,
    #[serde(rename = "cnfStyle", default)]
    cnf_style: Option<CnfStyleXml>,
    #[serde(rename = "framePr", default)]
    frame_pr: Option<FramePrXml>,

    // OnOff toggles. Typed as `Vec<OnOff>` (not `Option<OnOff>`) so a duplicated
    // toggle — which LibreOffice/AOO emit, e.g. `<w:keepNext/><w:keepNext/>` —
    // doesn't trip serde's "duplicate field" error and fail the whole parse.
    // §17.7.2 last-wins is applied via `last_toggle` in `split`. Same rationale
    // as `RPrXml`'s run toggles.
    #[serde(rename = "keepNext", default)]
    keep_next: Vec<OnOff>,
    #[serde(rename = "keepLines", default)]
    keep_lines: Vec<OnOff>,
    #[serde(rename = "widowControl", default)]
    widow_control: Vec<OnOff>,
    #[serde(rename = "pageBreakBefore", default)]
    page_break_before: Vec<OnOff>,
    #[serde(rename = "suppressAutoHyphens", default)]
    suppress_auto_hyphens: Vec<OnOff>,
    #[serde(rename = "contextualSpacing", default)]
    contextual_spacing: Vec<OnOff>,
    #[serde(default)]
    bidi: Vec<OnOff>,
    #[serde(rename = "wordWrap", default)]
    word_wrap: Vec<OnOff>,
    #[serde(rename = "overflowPunct", default)]
    overflow_punct: Vec<OnOff>,
    #[serde(rename = "snapToGrid", default)]
    snap_to_grid: Vec<OnOff>,
    #[serde(rename = "autoSpaceDE", default)]
    auto_space_de: Vec<OnOff>,
    #[serde(rename = "autoSpaceDN", default)]
    auto_space_dn: Vec<OnOff>,
}

#[derive(Clone, Debug, Deserialize)]
struct ValString {
    #[serde(rename = "@val")]
    val: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(bound(deserialize = "T: serde::Deserialize<'de>"))]
struct ValAttr<T> {
    #[serde(rename = "@val")]
    val: T,
}

/// `<w:ind>` — indentation. Legacy `@left`/`@right` alias `@start`/`@end`.
/// `@firstLine` and `@hanging` are mutually exclusive; when both present,
/// hanging wins per renderer convention (legacy parser matched this).
#[derive(Clone, Copy, Debug, Deserialize)]
struct IndXml {
    #[serde(rename = "@start", alias = "@left", default)]
    start: Option<Dimension<Twips>>,
    #[serde(rename = "@startChars", alias = "@leftChars", default)]
    start_chars: Option<Dimension<HundredthChars>>,
    #[serde(rename = "@end", alias = "@right", default)]
    end: Option<Dimension<Twips>>,
    #[serde(rename = "@endChars", alias = "@rightChars", default)]
    end_chars: Option<Dimension<HundredthChars>>,
    #[serde(rename = "@firstLine", default)]
    first_line: Option<Dimension<Twips>>,
    #[serde(rename = "@firstLineChars", default)]
    first_line_chars: Option<Dimension<HundredthChars>>,
    #[serde(rename = "@hanging", default)]
    hanging: Option<Dimension<Twips>>,
    #[serde(rename = "@hangingChars", default)]
    hanging_chars: Option<Dimension<HundredthChars>>,
    #[serde(rename = "@mirrorIndents", default)]
    mirror: Option<AttrBool>,
}

impl From<IndXml> for Indentation {
    fn from(x: IndXml) -> Self {
        let first_line = match (x.first_line, x.hanging) {
            (_, Some(h)) => Some(FirstLineIndent::Hanging(h)),
            (Some(f), None) => Some(FirstLineIndent::FirstLine(f)),
            (None, None) => None,
        };
        let first_line_chars = match (x.first_line_chars, x.hanging_chars) {
            (_, Some(h)) => Some(FirstLineIndentChars::Hanging(h)),
            (Some(f), None) => Some(FirstLineIndentChars::FirstLine(f)),
            (None, None) => None,
        };
        Self {
            start: x.start,
            start_chars: x.start_chars,
            end: x.end,
            end_chars: x.end_chars,
            first_line,
            first_line_chars,
            mirror: x.mirror.map(|b| b.0),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
struct SpacingXml {
    #[serde(
        rename = "@before",
        default,
        deserialize_with = "deserialize_optional_nonnegative_dimension"
    )]
    before: Option<Dimension<Twips>>,
    #[serde(
        rename = "@after",
        default,
        deserialize_with = "deserialize_optional_nonnegative_dimension"
    )]
    after: Option<Dimension<Twips>>,
    #[serde(rename = "@line", default)]
    line: Option<Dimension<Twips>>,
    #[serde(rename = "@lineRule", default)]
    line_rule: Option<StLineSpacingRule>,
    #[serde(rename = "@beforeAutospacing", default)]
    before_auto: Option<AttrBool>,
    #[serde(rename = "@afterAutospacing", default)]
    after_auto: Option<AttrBool>,
}

impl From<SpacingXml> for ParagraphSpacing {
    fn from(x: SpacingXml) -> Self {
        let line = x
            .line
            .map(|v| match x.line_rule.unwrap_or(StLineSpacingRule::Auto) {
                StLineSpacingRule::Auto => LineSpacing::Auto(v),
                StLineSpacingRule::Exact => LineSpacing::Exact(v),
                StLineSpacingRule::AtLeast => LineSpacing::AtLeast(v),
            });
        Self {
            before: x.before,
            after: x.after,
            line,
            before_auto_spacing: x.before_auto.map(|b| b.0),
            after_auto_spacing: x.after_auto.map(|b| b.0),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
struct NumPrXml {
    #[serde(default)]
    ilvl: Option<ValAttr<i16>>,
    #[serde(rename = "numId", default)]
    num_id: Option<ValAttr<i64>>,
}

/// `<w:framePr>` — legacy frame positioning. Splits by `@dropCap`:
/// `drop`/`margin` → `FrameKind::DropCap`; absent or `none` → `TextBox`.
#[derive(Clone, Copy, Debug, Deserialize)]
struct FramePrXml {
    #[serde(rename = "@dropCap", default)]
    drop_cap: Option<StDropCap>,
    #[serde(rename = "@lines", default)]
    lines: Option<u32>,
    #[serde(
        rename = "@hSpace",
        default,
        deserialize_with = "deserialize_optional_nonnegative_dimension"
    )]
    h_space: Option<Dimension<Twips>>,
    #[serde(
        rename = "@vSpace",
        default,
        deserialize_with = "deserialize_optional_nonnegative_dimension"
    )]
    v_space: Option<Dimension<Twips>>,
    #[serde(
        rename = "@w",
        default,
        deserialize_with = "deserialize_optional_nonnegative_dimension"
    )]
    w: Option<Dimension<Twips>>,
    #[serde(
        rename = "@h",
        default,
        deserialize_with = "deserialize_optional_nonnegative_dimension"
    )]
    h: Option<Dimension<Twips>>,
    #[serde(rename = "@hRule", default)]
    h_rule: Option<StHeightRule>,
    #[serde(rename = "@wrap", default)]
    wrap: Option<StFrameWrap>,
    #[serde(rename = "@hAnchor", default)]
    h_anchor: Option<StAnchor>,
    #[serde(rename = "@vAnchor", default)]
    v_anchor: Option<StAnchor>,
    #[serde(rename = "@x", default)]
    x: Option<Dimension<Twips>>,
    #[serde(rename = "@y", default)]
    y: Option<Dimension<Twips>>,
    #[serde(rename = "@xAlign", default)]
    x_align: Option<StXAlign>,
    #[serde(rename = "@yAlign", default)]
    y_align: Option<StYAlign>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum StDropCap {
    None,
    Drop,
    Margin,
}

impl From<FramePrXml> for FrameKind {
    fn from(x: FramePrXml) -> Self {
        match x.drop_cap {
            Some(StDropCap::Drop) => Self::DropCap {
                style: DropCap::Drop,
                lines: x.lines.unwrap_or(3),
                h_space: x.h_space,
            },
            Some(StDropCap::Margin) => Self::DropCap {
                style: DropCap::Margin,
                lines: x.lines.unwrap_or(3),
                h_space: x.h_space,
            },
            Some(StDropCap::None) | None => Self::TextBox(TextBoxPositioning {
                width: x.w,
                height: x.h,
                height_rule: x.h_rule.map(HeightRule::from),
                h_space: x.h_space,
                v_space: x.v_space,
                wrap: x.wrap.map(FrameWrap::from),
                h_anchor: x.h_anchor.map(Into::into),
                v_anchor: x.v_anchor.map(Into::into),
                x: x.x,
                y: x.y,
                x_align: x.x_align.map(Into::into),
                y_align: x.y_align.map(Into::into),
            }),
        }
    }
}

use crate::docx::parse::primitives::AttrBool;

impl PPrXml {
    pub(crate) fn text_effect_fill(&self) -> Option<&crate::docx::model::DrawingFill> {
        self.r_pr.as_ref()?.text_effect_fill()
    }

    pub(crate) fn split(self) -> ParsedPPr {
        let style_id = self.p_style.map(|v| StyleId::new(v.val));

        let (run_properties, _run_style_id) = match self.r_pr {
            Some(r) => {
                let (rp, sid) = r.split();
                (Some(rp), sid)
            }
            None => (None, None),
        };
        // rStyle inside pPr/rPr applies to the paragraph mark only; the
        // legacy parser discards this style id too.

        let section_properties = self.sect_pr.map(Into::into);

        let properties = ParagraphProperties {
            alignment: self.jc.map(|j| Alignment::from(j.val)),
            indentation: self.ind.map(Into::into),
            spacing: self.spacing.map(Into::into),
            numbering: self.num_pr.and_then(numbering_ref),
            tabs: self.tabs.map(<Vec<TabStop>>::from).unwrap_or_default(),
            borders: self.p_bdr.map(ParagraphBorders::from),
            shading: self.shd.map(Shading::from),
            keep_next: last_toggle(self.keep_next),
            keep_lines: last_toggle(self.keep_lines),
            widow_control: last_toggle(self.widow_control),
            page_break_before: last_toggle(self.page_break_before),
            suppress_auto_hyphens: last_toggle(self.suppress_auto_hyphens),
            contextual_spacing: last_toggle(self.contextual_spacing),
            bidi: last_toggle(self.bidi),
            word_wrap: last_toggle(self.word_wrap),
            overflow_punct: last_toggle(self.overflow_punct),
            snap_to_grid: last_toggle(self.snap_to_grid),
            outline_level: self
                .outline_lvl
                .and_then(|v| OutlineLevel::from_ooxml(v.val)),
            text_alignment: self.text_alignment.map(|v| TextAlignment::from(v.val)),
            cnf_style: self.cnf_style.map(CnfStyle::from),
            frame_properties: self.frame_pr.map(FrameKind::from),
            auto_space_de: last_toggle(self.auto_space_de),
            auto_space_dn: last_toggle(self.auto_space_dn),
        };

        ParsedPPr {
            properties,
            style_id,
            run_properties,
            section_properties,
        }
    }
}

fn numbering_ref(x: NumPrXml) -> Option<NumberingReference> {
    let num_id = x.num_id?;
    Some(NumberingReference {
        num_id: num_id.val,
        // Some Word-generated documents use -1 together with numId=0 as a
        // sentinel for "not numbered". Keep parsing tolerant and fall back to
        // level zero for values outside the model's u8 range.
        level: x.ilvl.and_then(|v| u8::try_from(v.val).ok()).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docx::model::{Alignment, BorderStyle, DropCap, ShadingPattern, TextAlignment};

    fn parse(xml: &str) -> ParsedPPr {
        let x: PPrXml = quick_xml::de::from_str(xml).unwrap();
        x.split()
    }

    #[test]
    fn empty_pprx_produces_defaults() {
        let r = parse(r#"<pPr/>"#);
        assert_eq!(r.properties.alignment, None);
        assert!(r.style_id.is_none());
        assert!(r.run_properties.is_none());
        assert!(r.section_properties.is_none());
    }

    #[test]
    fn p_style_routed_separately() {
        let r = parse(r#"<pPr><pStyle val="Heading1"/></pPr>"#);
        assert_eq!(
            r.style_id.map(|s| s.as_str().to_string()),
            Some("Heading1".into())
        );
        assert_eq!(r.properties.alignment, None);
    }

    #[test]
    fn direct_formatting_batch() {
        let r = parse(
            r#"<pPr>
                <jc val="both"/>
                <ind start="720" firstLine="360"/>
                <spacing before="120" after="240" line="360" lineRule="auto"/>
                <keepNext/>
                <keepLines val="false"/>
                <outlineLvl val="0"/>
                <textAlignment val="center"/>
            </pPr>"#,
        );
        let p = r.properties;
        assert_eq!(p.alignment, Some(Alignment::Both));
        assert_eq!(p.indentation.unwrap().start.unwrap().raw(), 720);
        match p.indentation.unwrap().first_line {
            Some(FirstLineIndent::FirstLine(d)) => assert_eq!(d.raw(), 360),
            other => panic!("expected FirstLine, got {other:?}"),
        }
        match p.spacing.unwrap().line {
            Some(LineSpacing::Auto(d)) => assert_eq!(d.raw(), 360),
            other => panic!("expected Auto, got {other:?}"),
        }
        assert_eq!(p.keep_next, Some(true));
        assert_eq!(p.keep_lines, Some(false));
        assert_eq!(p.outline_level.map(|o| o.value()), Some(1));
        assert_eq!(p.text_alignment, Some(TextAlignment::Center));
    }

    #[test]
    fn indentation_legacy_left_right_aliases() {
        let r = parse(r#"<pPr><ind left="720" right="360"/></pPr>"#);
        let ind = r.properties.indentation.unwrap();
        assert_eq!(ind.start.unwrap().raw(), 720);
        assert_eq!(ind.end.unwrap().raw(), 360);
    }

    #[test]
    fn negative_decimal_indentation_remains_valid() {
        let r = parse(r#"<pPr><ind start="-1.5"/></pPr>"#);
        assert_eq!(r.properties.indentation.unwrap().start.unwrap().raw(), -2);
    }

    #[test]
    fn character_unit_indentation_and_legacy_aliases_are_preserved() {
        let r =
            parse(r#"<pPr><ind leftChars="2500" rightChars="-53" firstLineChars="200"/></pPr>"#);
        let ind = r.properties.indentation.unwrap();
        assert_eq!(ind.start_chars.unwrap().raw(), 2500);
        assert_eq!(ind.end_chars.unwrap().raw(), -53);
        assert_eq!(
            ind.first_line_chars,
            Some(FirstLineIndentChars::FirstLine(Dimension::new(200)))
        );
    }

    #[test]
    fn character_unit_zero_and_absolute_fallback_are_both_preserved() {
        let r = parse(
            r#"<pPr><ind startChars="0" start="720" hangingChars="0" firstLine="360"/></pPr>"#,
        );
        let ind = r.properties.indentation.unwrap();
        assert_eq!(ind.start_chars.unwrap().raw(), 0);
        assert_eq!(ind.start.unwrap().raw(), 720);
        assert_eq!(
            ind.first_line_chars,
            Some(FirstLineIndentChars::Hanging(Dimension::new(0)))
        );
        assert_eq!(
            ind.first_line,
            Some(FirstLineIndent::FirstLine(Dimension::new(360)))
        );
    }

    #[test]
    fn num_pr_both_ilvl_and_num_id() {
        let r = parse(r#"<pPr><numPr><ilvl val="2"/><numId val="5"/></numPr></pPr>"#);
        let n = r.properties.numbering.unwrap();
        assert_eq!(n.level, 2);
        assert_eq!(n.num_id, 5);
    }

    #[test]
    fn num_pr_without_num_id_is_none() {
        let r = parse(r#"<pPr><numPr><ilvl val="1"/></numPr></pPr>"#);
        assert!(r.properties.numbering.is_none());
    }

    #[test]
    fn negative_numbering_level_sentinel_does_not_reject_paragraph() {
        let r = parse(r#"<pPr><numPr><ilvl val="-1"/><numId val="0"/></numPr></pPr>"#);
        let n = r.properties.numbering.unwrap();
        assert_eq!(n.level, 0);
        assert_eq!(n.num_id, 0);
    }

    #[test]
    fn snap_to_grid_on_off_is_preserved() {
        let enabled = parse(r#"<pPr><snapToGrid/></pPr>"#);
        assert_eq!(enabled.properties.snap_to_grid, Some(true));

        let disabled = parse(r#"<pPr><snapToGrid val="0"/></pPr>"#);
        assert_eq!(disabled.properties.snap_to_grid, Some(false));
    }

    #[test]
    fn overflow_punctuation_preserves_absent_on_off_and_last_wins() {
        assert_eq!(parse(r#"<pPr/>"#).properties.overflow_punct, None);
        assert_eq!(
            parse(r#"<pPr><overflowPunct/></pPr>"#)
                .properties
                .overflow_punct,
            Some(true)
        );
        assert_eq!(
            parse(r#"<pPr><overflowPunct val="0"/></pPr>"#)
                .properties
                .overflow_punct,
            Some(false)
        );
        assert_eq!(
            parse(r#"<pPr><overflowPunct val="1"/><overflowPunct val="off"/></pPr>"#)
                .properties
                .overflow_punct,
            Some(false)
        );
    }

    #[test]
    fn borders_shading_and_tabs() {
        let r = parse(
            r#"<pPr>
                <pBdr><top val="single"/></pBdr>
                <shd val="solid" fill="FFFF00"/>
                <tabs><tab pos="1440" val="center"/></tabs>
            </pPr>"#,
        );
        let p = r.properties;
        assert_eq!(p.borders.unwrap().top.unwrap().style, BorderStyle::Single);
        assert_eq!(p.shading.unwrap().pattern, ShadingPattern::Solid);
        assert_eq!(p.tabs.len(), 1);
        assert_eq!(p.tabs[0].position.raw(), 1440);
    }

    #[test]
    fn mark_run_properties_split_out() {
        let r = parse(r#"<pPr><rPr><b/><color val="FF0000"/></rPr></pPr>"#);
        let rp = r.run_properties.unwrap();
        assert_eq!(rp.bold, Some(true));
    }

    #[test]
    fn nested_sect_pr_routed_separately() {
        let r = parse(r#"<pPr><sectPr><pgSz w="12240" h="15840"/></sectPr></pPr>"#);
        let sp = r.section_properties.unwrap();
        assert_eq!(sp.page_size.unwrap().width.unwrap().raw(), 12240);
    }

    #[test]
    fn frame_pr_drop_cap() {
        let r = parse(r#"<pPr><framePr dropCap="drop" lines="2"/></pPr>"#);
        match r.properties.frame_properties {
            Some(FrameKind::DropCap { style, lines, .. }) => {
                assert_eq!(style, DropCap::Drop);
                assert_eq!(lines, 2);
            }
            other => panic!("expected DropCap, got {other:?}"),
        }
    }

    #[test]
    fn frame_pr_text_box_default() {
        let r = parse(r#"<pPr><framePr w="5000" h="3000" hAnchor="margin"/></pPr>"#);
        match r.properties.frame_properties {
            Some(FrameKind::TextBox(tb)) => {
                assert_eq!(tb.width.unwrap().raw(), 5000);
                assert_eq!(tb.height.unwrap().raw(), 3000);
            }
            other => panic!("expected TextBox, got {other:?}"),
        }
    }

    #[test]
    fn cnf_style_binary_val() {
        let r = parse(r#"<pPr><cnfStyle val="100000000000"/></pPr>"#);
        assert_eq!(r.properties.cnf_style, Some(CnfStyle::FIRST_ROW));
    }

    #[test]
    fn all_paragraph_toggles() {
        let r = parse(
            r#"<pPr>
                <keepNext/><keepLines/><widowControl/><pageBreakBefore/>
                <suppressAutoHyphens/><contextualSpacing/><bidi/><wordWrap/><overflowPunct/>
                <autoSpaceDE/><autoSpaceDN/>
            </pPr>"#,
        );
        let p = r.properties;
        assert_eq!(p.keep_next, Some(true));
        assert_eq!(p.keep_lines, Some(true));
        assert_eq!(p.widow_control, Some(true));
        assert_eq!(p.page_break_before, Some(true));
        assert_eq!(p.suppress_auto_hyphens, Some(true));
        assert_eq!(p.contextual_spacing, Some(true));
        assert_eq!(p.bidi, Some(true));
        assert_eq!(p.word_wrap, Some(true));
        assert_eq!(p.overflow_punct, Some(true));
        assert_eq!(p.auto_space_de, Some(true));
        assert_eq!(p.auto_space_dn, Some(true));
    }

    #[test]
    fn unknown_jc_is_strict() {
        let r: Result<PPrXml, _> = quick_xml::de::from_str(r#"<pPr><jc val="bogus"/></pPr>"#);
        assert!(r.is_err());
    }

    #[test]
    fn duplicate_toggles_are_tolerated_last_wins() {
        // LibreOffice/AOO emit redundant duplicate toggles. With `Option<OnOff>`
        // serde would fail with "duplicate field" and take down the whole parse;
        // `Vec<OnOff>` + last_toggle accepts them (§17.7.2 last wins).
        let r = parse(r#"<pPr><keepNext/><keepNext/></pPr>"#);
        assert_eq!(r.properties.keep_next, Some(true));

        // When duplicates disagree, the last one wins.
        let r = parse(r#"<pPr><widowControl val="1"/><widowControl val="0"/></pPr>"#);
        assert_eq!(r.properties.widow_control, Some(false));
    }
}
