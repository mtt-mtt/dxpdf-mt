//! Document-level settings.

use crate::model::dimension::{Dimension, Twips};

use super::identifiers::RevisionSaveId;

#[derive(Clone, Debug)]
pub struct DocumentSettings {
    /// Default tab stop interval (OOXML default: 720 twips = 0.5 inch).
    pub default_tab_stop: Dimension<Twips>,
    /// Whether even/odd headers/footers are enabled.
    pub even_and_odd_headers: bool,
    /// §17.15.1.20: display the document background in print-layout view.
    /// Omitted settings resolve to `false`.
    pub display_background_shape: bool,
    /// §17.15.3.1: apply the active section's document-grid line pitch to
    /// paragraphs inside table cells as well as body paragraphs.
    pub adjust_line_height_in_table: bool,
    /// §17.15.3.29: when a section defines a character grid, forbid the
    /// extra grid cell that hanging punctuation would otherwise occupy.
    pub do_not_wrap_text_with_punct: bool,
    /// §17.15.1.18: document-wide compression of whitespace carried by
    /// full-width punctuation (and, for the third mode, Japanese kana).
    pub character_spacing_control: CharacterSpacingControl,
    /// The rsid of the original editing session that created this document.
    pub rsid_root: Option<RevisionSaveId>,
    /// All revision save IDs recorded in this document's history.
    pub rsids: Vec<RevisionSaveId>,
}

impl Default for DocumentSettings {
    fn default() -> Self {
        Self {
            // §17.15.1.25: when `w:defaultTabStop` is omitted the default tab
            // stop interval is 720 twips (0.5"). A derived `Default` would give
            // 0 here — wrong per spec, and it would silently collapse default
            // tabs for any consumer that reads this field.
            default_tab_stop: Dimension::new(720),
            even_and_odd_headers: false,
            display_background_shape: false,
            adjust_line_height_in_table: false,
            do_not_wrap_text_with_punct: false,
            character_spacing_control: CharacterSpacingControl::DoNotCompress,
            rsid_root: None,
            rsids: Vec::new(),
        }
    }
}

/// §17.18.10: character-level whitespace compression policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CharacterSpacingControl {
    /// Spec default when `w:characterSpacingControl` is omitted.
    #[default]
    DoNotCompress,
    CompressPunctuation,
    CompressPunctuationAndJapaneseKana,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tab_stop_is_720_twips_per_spec() {
        // §17.15.1.25: an omitted `w:defaultTabStop` is 720 twips (0.5"), not
        // the 0 a derived `Default` would produce. Guards against a "simplify
        // to #[derive(Default)]" regression.
        assert_eq!(DocumentSettings::default().default_tab_stop.raw(), 720);
    }

    #[test]
    fn remaining_defaults() {
        let s = DocumentSettings::default();
        assert!(!s.even_and_odd_headers);
        assert!(!s.display_background_shape);
        assert!(!s.adjust_line_height_in_table);
        assert!(!s.do_not_wrap_text_with_punct);
        assert_eq!(
            s.character_spacing_control,
            CharacterSpacingControl::DoNotCompress
        );
        assert!(s.rsid_root.is_none());
        assert!(s.rsids.is_empty());
    }
}
