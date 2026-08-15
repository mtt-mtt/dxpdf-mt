//! Parser for `word/settings.xml`.

use serde::Deserialize;

use crate::docx::dimension::{Dimension, Twips};
use crate::docx::error::Result;
use crate::docx::model::{CharacterSpacingControl, DocumentSettings, RevisionSaveId};
use crate::docx::parse::primitives::units::deserialize_nonnegative_dimension;
use crate::docx::parse::primitives::OnOff;
use crate::docx::parse::serde_xml::from_xml;

/// Parse `word/settings.xml`. Entry point: deserializes into an intermediate
/// schema, then maps to the model type.
pub fn parse_settings(data: &[u8]) -> Result<DocumentSettings> {
    from_xml::<SettingsXml>(data).map(Into::into)
}

#[derive(Deserialize, Default)]
struct SettingsXml {
    #[serde(rename = "defaultTabStop", default)]
    default_tab_stop: Option<DimensionVal<Twips>>,
    #[serde(rename = "evenAndOddHeaders", default)]
    even_and_odd_headers: Option<OnOff>,
    #[serde(rename = "characterSpacingControl", default)]
    character_spacing_control: Option<CharacterSpacingControlXml>,
    #[serde(default)]
    compat: Option<CompatXml>,
    #[serde(default)]
    rsids: Option<RsidsXml>,
}

#[derive(Deserialize)]
struct CharacterSpacingControlXml {
    #[serde(rename = "@val")]
    val: StCharacterSpacing,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
enum StCharacterSpacing {
    DoNotCompress,
    CompressPunctuation,
    CompressPunctuationAndJapaneseKana,
}

impl From<StCharacterSpacing> for CharacterSpacingControl {
    fn from(value: StCharacterSpacing) -> Self {
        match value {
            StCharacterSpacing::DoNotCompress => Self::DoNotCompress,
            StCharacterSpacing::CompressPunctuation => Self::CompressPunctuation,
            StCharacterSpacing::CompressPunctuationAndJapaneseKana => {
                Self::CompressPunctuationAndJapaneseKana
            }
        }
    }
}

#[derive(Deserialize, Default)]
struct CompatXml {
    #[serde(rename = "adjustLineHeightInTable", default)]
    adjust_line_height_in_table: Option<OnOff>,
    #[serde(rename = "doNotWrapTextWithPunct", default)]
    do_not_wrap_text_with_punct: Option<OnOff>,
}

#[derive(Deserialize, Default)]
struct RsidsXml {
    #[serde(rename = "rsidRoot", default)]
    rsid_root: Option<StringVal>,
    #[serde(rename = "rsid", default)]
    rsids: Vec<StringVal>,
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "U: crate::docx::dimension::Unit"))]
struct DimensionVal<U: crate::docx::dimension::Unit> {
    #[serde(
        rename = "@val",
        deserialize_with = "deserialize_nonnegative_dimension"
    )]
    val: Dimension<U>,
}

#[derive(Deserialize)]
struct StringVal {
    #[serde(rename = "@val")]
    val: String,
}

impl From<SettingsXml> for DocumentSettings {
    fn from(x: SettingsXml) -> Self {
        let mut s = DocumentSettings::default();
        if let Some(t) = x.default_tab_stop {
            s.default_tab_stop = t.val;
        }
        if let Some(OnOff(on)) = x.even_and_odd_headers {
            s.even_and_odd_headers = on;
        }
        if let Some(control) = x.character_spacing_control {
            s.character_spacing_control = control.val.into();
        }
        if let Some(compat) = x.compat {
            if let Some(OnOff(on)) = compat.adjust_line_height_in_table {
                s.adjust_line_height_in_table = on;
            }
            if let Some(OnOff(on)) = compat.do_not_wrap_text_with_punct {
                s.do_not_wrap_text_with_punct = on;
            }
        }
        if let Some(r) = x.rsids {
            if let Some(root) = r.rsid_root {
                s.rsid_root = RevisionSaveId::from_hex(&root.val);
            }
            s.rsids = r
                .rsids
                .into_iter()
                .filter_map(|v| RevisionSaveId::from_hex(&v.val))
                .collect();
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_adjust_line_height_in_table_on_off_semantics() {
        let enabled =
            parse_settings(br#"<settings><compat><adjustLineHeightInTable/></compat></settings>"#)
                .unwrap();
        assert!(enabled.adjust_line_height_in_table);

        let disabled = parse_settings(
            br#"<settings><compat><adjustLineHeightInTable val="0"/></compat></settings>"#,
        )
        .unwrap();
        assert!(!disabled.adjust_line_height_in_table);

        let omitted = parse_settings(br#"<settings><compat/></settings>"#).unwrap();
        assert!(!omitted.adjust_line_height_in_table);
    }

    #[test]
    fn parses_do_not_wrap_text_with_punctuation_on_off_and_omitted() {
        let both = parse_settings(
            br#"<settings><compat><adjustLineHeightInTable/><doNotWrapTextWithPunct/></compat></settings>"#,
        )
        .unwrap();
        assert!(both.adjust_line_height_in_table);
        assert!(both.do_not_wrap_text_with_punct);

        let disabled = parse_settings(
            br#"<settings><compat><doNotWrapTextWithPunct val="false"/></compat></settings>"#,
        )
        .unwrap();
        assert!(!disabled.do_not_wrap_text_with_punct);

        let omitted = parse_settings(br#"<settings><compat/></settings>"#).unwrap();
        assert!(!omitted.do_not_wrap_text_with_punct);
    }

    #[test]
    fn parses_character_spacing_control_and_uses_spec_default() {
        let punctuation = parse_settings(
            br#"<settings><characterSpacingControl val="compressPunctuation"/></settings>"#,
        )
        .unwrap();
        assert_eq!(
            punctuation.character_spacing_control,
            CharacterSpacingControl::CompressPunctuation
        );

        let kana = parse_settings(
            br#"<settings><characterSpacingControl val="compressPunctuationAndJapaneseKana"/></settings>"#,
        )
        .unwrap();
        assert_eq!(
            kana.character_spacing_control,
            CharacterSpacingControl::CompressPunctuationAndJapaneseKana
        );

        let omitted = parse_settings(br#"<settings/>"#).unwrap();
        assert_eq!(
            omitted.character_spacing_control,
            CharacterSpacingControl::DoNotCompress
        );
    }
}
