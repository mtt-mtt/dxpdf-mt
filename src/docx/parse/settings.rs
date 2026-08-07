//! Parser for `word/settings.xml`.

use serde::Deserialize;

use crate::docx::dimension::{Dimension, Twips};
use crate::docx::error::Result;
use crate::docx::model::{DocumentSettings, RevisionSaveId};
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
    #[serde(default)]
    compat: Option<CompatXml>,
    #[serde(default)]
    rsids: Option<RsidsXml>,
}

#[derive(Deserialize, Default)]
struct CompatXml {
    #[serde(rename = "adjustLineHeightInTable", default)]
    adjust_line_height_in_table: Option<OnOff>,
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
        if let Some(OnOff(on)) = x.compat.and_then(|c| c.adjust_line_height_in_table) {
            s.adjust_line_height_in_table = on;
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
}
