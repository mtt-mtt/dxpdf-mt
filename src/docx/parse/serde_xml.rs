//! Shared helpers for serde-driven OOXML parsers.

use serde::de::DeserializeOwned;

use crate::docx::error::Result;

/// quick-xml intentionally matches elements by local name. Word 2010 run
/// effects such as `<w14:shadow>` can therefore collide with the standard
/// `<w:shadow>` toggle when both occur in one `<w:rPr>`. The renderer does not
/// implement the w14 text-effect object, so remove that extension element
/// before serde sees it while retaining the standard toggle.
fn without_unsupported_w14_shadow(data: &[u8]) -> Option<Vec<u8>> {
    if !data
        .windows(b"w14:shadow".len())
        .any(|window| window == b"w14:shadow")
    {
        return None;
    }

    use quick_xml::events::Event;
    use quick_xml::{Reader, Writer};

    let mut reader = Reader::from_reader(data);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Vec::with_capacity(data.len()));
    let mut skipped_depth = 0usize;
    loop {
        let event = reader.read_event().ok()?;
        match event {
            Event::Start(_) if skipped_depth > 0 => skipped_depth += 1,
            Event::Start(start) if start.name().as_ref() == b"w14:shadow" => {
                skipped_depth = 1;
            }
            Event::Empty(_) if skipped_depth > 0 => {}
            Event::Empty(empty) if empty.name().as_ref() == b"w14:shadow" => {}
            Event::End(_) if skipped_depth > 0 => skipped_depth -= 1,
            Event::Eof => break,
            other if skipped_depth == 0 => writer.write_event(other.into_owned()).ok()?,
            _ => {}
        }
    }
    Some(writer.into_inner())
}

/// Deserialize an OOXML part into a schema type, mapping quick-xml's error
/// into the crate's `ParseError`.
pub fn from_xml<T: DeserializeOwned>(data: &[u8]) -> Result<T> {
    let sanitized = without_unsupported_w14_shadow(data);
    Ok(quick_xml::de::from_reader(
        sanitized.as_deref().unwrap_or(data),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct RunProperties {
        #[serde(rename = "shadow", default)]
        shadow: Vec<Toggle>,
    }

    #[derive(Debug, Deserialize)]
    struct Toggle {
        #[serde(rename = "@val", default)]
        val: Option<String>,
    }

    #[test]
    fn word_2010_shadow_does_not_collide_with_standard_shadow_toggle() {
        let xml = br#"<w:rPr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
                    xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml">
                    <w:shadow w:val="0"/><w:emboss w:val="0"/>
                    <w14:shadow w14:blurRad="0"><w14:srgbClr w14:val="000000"/></w14:shadow>
                    </w:rPr>"#;
        let parsed: RunProperties = from_xml(xml).unwrap();
        assert_eq!(parsed.shadow.len(), 1);
        assert_eq!(parsed.shadow[0].val.as_deref(), Some("0"));
    }
}
