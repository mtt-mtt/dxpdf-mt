//! Font resolution and caching with per-render ownership (`FontRegistry`).
//!
//! `FontRegistry` is the single source of truth for typeface data within
//! one render. It owns:
//!
//! - the document's embedded-font bytes (deobfuscated upstream by the
//!   parser per ECMA-376 §17.8.3.3),
//! - a cache of resolved Skia [`Typeface`]s keyed by (family, weight,
//!   slant), with embedded fonts taking priority over system resolution.
//!
//! A `FontRegistry` is constructed per render and passed by reference to
//! layout and paint. The previous `thread_local!` typeface cache leaked
//! typefaces across renders — once font subsetting mutates them, that
//! becomes a real correctness bug. With per-render ownership, no such
//! leakage is possible.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;

use skia_safe::font_style::{Slant, Weight, Width};
use skia_safe::{Data, Font, FontMgr, FontStyle, Typeface};

use crate::model::{EmbeddedFont, EmbeddedFontVariant};
use crate::render::dimension::Pt;

const NOTO_SANS_SYMBOLS_2_BYTES: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/fonts/NotoSansSymbols2-Regular.ttf"
));
const NOTO_COLOR_EMOJI_BYTES: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/fonts/NotoColorEmoji-COLRv1.ttf"
));
const BUNDLED_SYMBOL_FAMILY: &str = "__dxpdf_noto_sans_symbols_2";

// ─── Public types ───────────────────────────────────────────────────────────

/// Stable id for an embedded font registered in the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EmbeddedFontId(u32);

impl EmbeddedFontId {
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// Identity for a Skia [`Typeface`], wrapping `Typeface::unique_id`.
/// Used as the join key with [`crate::render::subset::CodepointUsage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypefaceId(pub u32);

impl From<&Typeface> for TypefaceId {
    fn from(tf: &Typeface) -> Self {
        Self(tf.unique_id())
    }
}

/// Font assets shipped with dxpdf to make symbols and emoji deterministic
/// across hosts. The enum is also the key used to recover original bytes for
/// PDF font subsetting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BundledFont {
    NotoSansSymbols2,
    NotoColorEmoji,
}

pub fn bundled_font_bytes(font: BundledFont) -> &'static [u8] {
    match font {
        BundledFont::NotoSansSymbols2 => NOTO_SANS_SYMBOLS_2_BYTES,
        BundledFont::NotoColorEmoji => NOTO_COLOR_EMOJI_BYTES,
    }
}

/// Single source of truth for "where did this typeface come from?" — drives
/// byte extraction during subsetting (Embedded → registry's bytes, System →
/// `Typeface::to_font_data`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypefaceOrigin {
    /// Resolved from a font embedded in the DOCX (`word/fonts/*.odttf`).
    Embedded { id: EmbeddedFontId },
    /// Resolved through Skia's `FontMgr` — exact match, substitution, or
    /// system default fallback. The id is the original Skia typeface id
    /// at resolution time.
    System { typeface_id: TypefaceId },
    /// A system face selected for a missing run-level glyph and pinned under
    /// an internal alias. These composite/linked fallback faces are kept
    /// whole because rebuilding them from subset bytes can retain the cmap
    /// entry while losing the glyph outline.
    SystemFallback { typeface_id: TypefaceId },
    /// Loaded from a font asset distributed with dxpdf rather than from the
    /// DOCX or host font collection.
    Bundled { font: BundledFont },
}

#[derive(Clone, Debug)]
pub struct TypefaceEntry {
    pub typeface: Typeface,
    pub origin: TypefaceOrigin,
}

/// Cache key for resolved typefaces — case-insensitive family + weight + slant.
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct TypefaceKey {
    pub family_lc: String,
    pub weight: i32,
    pub slant: skia_safe::font_style::Slant,
}

impl TypefaceKey {
    pub fn new(family: &str, style: FontStyle) -> Self {
        Self {
            family_lc: family.to_lowercase(),
            weight: *style.weight(),
            slant: style.slant(),
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum RegisterError {
    #[error("invalid embedded font data for '{family}' ({variant:?})")]
    InvalidFontData {
        family: String,
        variant: EmbeddedFontVariant,
    },
}

/// Open-source metric-compatible substitutes for proprietary fonts. Tried
/// in order when `match_family_style` for the requested family fails.
const FONT_SUBSTITUTIONS: &[(&str, &[&str])] = &[
    // Word on Windows maps the legacy Century Schoolbook family to Segoe
    // Print when the requested face is absent.  This is not a cosmetic
    // choice: Segoe Print's wider advances and taller line box materially
    // change form pagination.  Keep conventional serif fallbacks behind the
    // Word-compatible face for hosts that do not ship Segoe Print.
    (
        "Century Schoolbook",
        &["Segoe Print", "Century", "Liberation Serif", "Noto Serif"],
    ),
    ("Calibri", &["Carlito", "Liberation Sans", "Noto Sans"]),
    ("Cambria", &["Caladea", "Liberation Serif", "Noto Serif"]),
    ("Arial", &["Liberation Sans", "Noto Sans", "Helvetica"]),
    (
        "Times New Roman",
        &["Liberation Serif", "Noto Serif", "Times"],
    ),
    (
        "Courier New",
        &["Liberation Mono", "Noto Sans Mono", "Courier"],
    ),
    ("Verdana", &["DejaVu Sans", "Noto Sans"]),
    ("Georgia", &["DejaVu Serif", "Noto Serif"]),
    ("Trebuchet MS", &["Ubuntu", "Noto Sans"]),
    (
        "Consolas",
        &["Inconsolata", "Liberation Mono", "Noto Sans Mono"],
    ),
    ("Segoe UI", &["Noto Sans", "Liberation Sans"]),
];

/// Canonical Windows family names for localized East Asian names commonly
/// written into OOXML. Skia exposes the canonical English names on Windows,
/// so an exact lookup for the localized spelling otherwise falls through to
/// an unrelated default font and loses CJK glyphs.
const LOCALIZED_FAMILY_ALIASES: &[(&str, &str)] = &[
    ("宋体", "SimSun"),
    ("微软雅黑", "Microsoft YaHei"),
    ("等线", "DengXian"),
    ("仿宋", "FangSong"),
    ("仿宋_GB2312", "FangSong"),
    ("方正仿宋_GBK", "FangSong"),
    // 方正小标宋 is a proprietary Chinese title face frequently named in
    // government and university forms but not embedded. SimSun is the narrow
    // Windows Song-family fallback; unlike the generic Segoe UI fallback it
    // retains CJK glyph coverage and a serif title character.
    ("方正小标宋简体", "SimSun"),
    ("方正小标宋_GBK", "SimSun"),
    ("黑体", "SimHei"),
    ("方正黑体_GBK", "SimHei"),
    ("楷体", "KaiTi"),
    ("楷体_GB2312", "KaiTi"),
    ("方正楷体_GBK", "KaiTi"),
    ("新細明體", "PMingLiU"),
];

fn localized_family_alias(family: &str) -> Option<&'static str> {
    LOCALIZED_FAMILY_ALIASES
        .iter()
        .find(|(localized, _)| localized.eq_ignore_ascii_case(family.trim()))
        .map(|(_, canonical)| *canonical)
}

#[derive(Debug, Clone)]
struct EmbeddedRecord {
    family: String,
    variant: EmbeddedFontVariant,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FaceAlias {
    family: String,
    weight: i32,
}

#[derive(Clone, Debug)]
enum AliasEntry {
    Unique(FaceAlias),
    Ambiguous,
}

#[derive(Default)]
struct FaceAliasIndex {
    aliases: HashMap<String, AliasEntry>,
}

impl FaceAliasIndex {
    fn build(font_mgr: &FontMgr) -> Self {
        let mut index = Self::default();

        for family in font_mgr.family_names() {
            let mut style_set = font_mgr.match_family(&family);
            let count = style_set.count();
            for face_index in 0..count {
                let (style, style_name) = style_set.style(face_index);
                let post_script_name = style_set
                    .new_typeface(face_index)
                    .and_then(|typeface| typeface.post_script_name());
                index.insert_face(
                    &family,
                    style,
                    style_name.as_deref(),
                    post_script_name.as_deref(),
                );
            }
        }

        index
    }

    fn insert_face(
        &mut self,
        family: &str,
        style: FontStyle,
        style_name: Option<&str>,
        post_script_name: Option<&str>,
    ) {
        // This compatibility layer resolves weight-bearing face names. Width
        // and slant aliases require a fuller OpenType identity model.
        if style.width() != Width::NORMAL || style.slant() != Slant::Upright {
            return;
        }

        let alias = FaceAlias {
            family: family.to_owned(),
            weight: *style.weight(),
        };

        if let Some(style_name) = style_name.filter(|name| !name.trim().is_empty()) {
            self.insert_alias(&format!("{family} {style_name}"), alias.clone());
        }
        if let Some(post_script_name) = post_script_name.filter(|name| !name.trim().is_empty()) {
            self.insert_alias(post_script_name, alias.clone());
        }
        for weight_name in canonical_weight_names(alias.weight) {
            self.insert_alias(&format!("{family} {weight_name}"), alias.clone());
        }
    }

    fn insert_alias(&mut self, name: &str, alias: FaceAlias) {
        use std::collections::hash_map::Entry;

        match self.aliases.entry(face_name_key(name)) {
            Entry::Vacant(entry) => {
                entry.insert(AliasEntry::Unique(alias));
            }
            Entry::Occupied(mut entry) => match entry.get() {
                AliasEntry::Unique(existing) if existing == &alias => {}
                AliasEntry::Unique(_) => {
                    entry.insert(AliasEntry::Ambiguous);
                }
                AliasEntry::Ambiguous => {}
            },
        }
    }

    fn resolve(&self, name: &str) -> Option<&FaceAlias> {
        match self.aliases.get(&face_name_key(name))? {
            AliasEntry::Unique(alias) => Some(alias),
            AliasEntry::Ambiguous => None,
        }
    }
}

fn face_name_key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Strip a trailing weight word from a face-qualified family name, yielding the
/// base family: `"Segoe UI Light"` → `"Segoe UI"`. `None` when the name does not
/// end in a recognised weight word.
///
/// [`FONT_SUBSTITUTIONS`] is keyed by family, so a document naming a *face*
/// otherwise walks straight past step 4 to the system default — `"Segoe UI"` is
/// in the table but `"Segoe UI Light"` was not reachable from it. When step 3
/// also declines the name as `Ambiguous` (as it does for `"Segoe UI Light"` on
/// macOS) there was no path at all from a face name to its family's
/// metric-compatible substitutes.
///
/// The longest matching suffix wins, so `"Foo Extra Light"` yields `"Foo"`
/// rather than `"Foo Extra"`. A weight word must be a separate trailing word:
/// `"Highlight"` ends in `"light"` but is not face-qualified.
fn strip_weight_suffix(name: &str) -> Option<&str> {
    let trimmed = name.trim_end();
    let mut best: Option<&str> = None;
    for weight in (100..=900).step_by(100) {
        for suffix in canonical_weight_names(weight) {
            let Some(cut) = trimmed.len().checked_sub(suffix.len()) else {
                continue;
            };
            if cut == 0 || !trimmed.is_char_boundary(cut) {
                continue;
            }
            if !trimmed[cut..].eq_ignore_ascii_case(suffix) {
                continue;
            }
            let base = trimmed[..cut].trim_end();
            // The suffix has to be its own word, and something must precede it.
            if base.len() == cut || base.is_empty() {
                continue;
            }
            if best.is_none_or(|b| base.len() < b.len()) {
                best = Some(base);
            }
        }
    }
    best
}

/// Metric-compatible substitutes for `family`, falling back to its base family
/// when the name is face-qualified. Returns the matched key alongside the list
/// so the caller can log which one fired.
fn substitutes_for(family: &str) -> Option<(&'static str, &'static [&'static str])> {
    let lookup = |name: &str| {
        FONT_SUBSTITUTIONS
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(key, subs)| (*key, *subs))
    };
    lookup(family).or_else(|| lookup(strip_weight_suffix(family)?))
}

fn canonical_weight_names(weight: i32) -> &'static [&'static str] {
    match weight {
        100 => &["Thin", "Hairline"],
        200 => &["ExtraLight", "Extra Light", "UltraLight", "Ultra Light"],
        300 => &["Light"],
        400 => &["Regular", "Normal"],
        500 => &["Medium"],
        600 => &["Semibold", "SemiBold", "Semi Bold", "DemiBold", "Demi Bold"],
        700 => &["Bold"],
        800 => &["ExtraBold", "Extra Bold", "UltraBold", "Ultra Bold"],
        900 => &["Black", "Heavy"],
        _ => &[],
    }
}

/// Combine the weight a face *name* carries with the weight the run requested.
///
/// The requested weight is not really a weight. `FontCache::get_indexed` builds
/// it from a `bold: bool`, so it is only ever `NORMAL` or `BOLD`: `BOLD` means
/// "this run is bold", and `NORMAL` means only "this run is not bold" — it
/// carries no information about what weight the face should be.
///
/// So `NORMAL` must not participate. Taking `max` unconditionally let that
/// content-free default overrule an explicit face name, promoting every face
/// lighter than Regular: `"Calibri Light"` resolves through the alias index to
/// `(Calibri, 342)` and came back out as plain Calibri at 400. Step 3 exists to
/// honour documents that name a *face* rather than a family, and that is
/// exactly the information it was discarding.
///
/// A bold request still raises a light face — `"Calibri Light"` with `<w:b/>`
/// wants something bolder than Light, and `BOLD` is the closest this layer can
/// express.
fn merged_alias_weight(alias_weight: i32, requested_weight: i32) -> i32 {
    if requested_weight > *Weight::NORMAL {
        alias_weight.max(requested_weight)
    } else {
        alias_weight
    }
}

fn style_for_face_alias(alias: &FaceAlias, requested: FontStyle) -> FontStyle {
    FontStyle::new(
        Weight::from(merged_alias_weight(alias.weight, *requested.weight())),
        Width::NORMAL,
        requested.slant(),
    )
}

// ─── FontRegistry ────────────────────────────────────────────────────────────

pub struct FontRegistry {
    font_mgr: FontMgr,
    embedded: Vec<EmbeddedRecord>,
    embedded_index: HashMap<(String, EmbeddedFontVariant), EmbeddedFontId>,
    bundled_symbols: OnceCell<Option<TypefaceEntry>>,
    bundled_color_emoji: OnceCell<Option<TypefaceEntry>>,
    system_face_aliases: OnceCell<FaceAliasIndex>,
    typefaces: RefCell<HashMap<TypefaceKey, TypefaceEntry>>,
}

impl FontRegistry {
    /// Empty registry without any embedded fonts.
    pub fn new(font_mgr: FontMgr) -> Self {
        Self {
            font_mgr,
            embedded: Vec::new(),
            embedded_index: HashMap::new(),
            bundled_symbols: OnceCell::new(),
            bundled_color_emoji: OnceCell::new(),
            system_face_aliases: OnceCell::new(),
            typefaces: RefCell::new(HashMap::new()),
        }
    }

    /// Build a registry, registering all embedded fonts and preloading the
    /// requested family/style combinations.
    ///
    /// Fails with [`crate::render::error::RenderError::NoFontsAvailable`] when the host exposes no
    /// typeface at all. Checking here rather than at the point of use is what
    /// lets [`Self::resolve`] return a `TypefaceEntry` rather than an
    /// `Option`: the last-resort arm of `resolve_uncached` is unreachable for
    /// any registry this constructor returns.
    pub fn build(
        font_mgr: FontMgr,
        embedded: &[EmbeddedFont],
        families: &[String],
    ) -> Result<Self, crate::render::error::RenderError> {
        if font_mgr
            .legacy_make_typeface(None::<&str>, FontStyle::normal())
            .is_none()
        {
            return Err(crate::render::error::RenderError::NoFontsAvailable);
        }
        let mut reg = Self::new(font_mgr);
        for ef in embedded {
            if let Err(err) = reg.register_embedded(&ef.family, ef.variant, ef.data.clone()) {
                log::warn!("{err}");
            }
        }
        reg.preload(families);
        Ok(reg)
    }

    pub fn font_mgr(&self) -> &FontMgr {
        &self.font_mgr
    }

    pub fn embedded_font_count(&self) -> usize {
        self.embedded.len()
    }

    pub fn cached_typeface_count(&self) -> usize {
        self.typefaces.borrow().len()
    }

    /// Register an embedded font. Subsequent `resolve` calls for the same
    /// family + variant will return this typeface in preference to system
    /// resolution.
    pub fn register_embedded(
        &mut self,
        family: &str,
        variant: EmbeddedFontVariant,
        bytes: Vec<u8>,
    ) -> Result<EmbeddedFontId, RegisterError> {
        let data = Data::new_copy(&bytes);
        let typeface = self.font_mgr.new_from_data(&data, 0).ok_or_else(|| {
            RegisterError::InvalidFontData {
                family: family.to_string(),
                variant,
            }
        })?;
        let id = EmbeddedFontId(self.embedded.len() as u32);
        self.embedded.push(EmbeddedRecord {
            family: family.to_string(),
            variant,
            bytes,
        });
        self.embedded_index
            .insert((family.to_lowercase(), variant), id);
        let style = font_style_for_variant(variant);
        let key = TypefaceKey::new(family, style);
        self.typefaces.borrow_mut().insert(
            key,
            TypefaceEntry {
                typeface,
                origin: TypefaceOrigin::Embedded { id },
            },
        );
        log::debug!("registered embedded font '{}' {:?}", family, variant);
        Ok(id)
    }

    /// Bytes for a registered embedded font.
    pub fn embedded_bytes(&self, id: EmbeddedFontId) -> &[u8] {
        &self.embedded[id.0 as usize].bytes
    }

    /// Family + variant for a registered embedded font.
    pub fn embedded_meta(&self, id: EmbeddedFontId) -> (&str, EmbeddedFontVariant) {
        let r = &self.embedded[id.0 as usize];
        (&r.family, r.variant)
    }

    /// Resolve a typeface by family + style. Embedded fonts win over system.
    /// Cached after the first resolution; later calls are O(1).
    pub fn resolve(&self, family: &str, style: FontStyle) -> TypefaceEntry {
        let key = TypefaceKey::new(family, style);

        if let Some(entry) = self.typefaces.borrow().get(&key) {
            return entry.clone();
        }

        let entry = self.resolve_uncached(family, style);
        self.typefaces.borrow_mut().insert(key, entry.clone());
        entry
    }

    fn resolve_uncached(&self, family: &str, style: FontStyle) -> TypefaceEntry {
        let variant = variant_for_style(style);
        if let Some(id) = self
            .embedded_index
            .get(&(family.to_lowercase(), variant))
            .copied()
        {
            let bytes = &self.embedded[id.0 as usize].bytes;
            let data = Data::new_copy(bytes);
            if let Some(tf) = self.font_mgr.new_from_data(&data, 0) {
                log::debug!("[font] '{}' {:?} → embedded #{}", family, style, id.0);
                return TypefaceEntry {
                    typeface: tf,
                    origin: TypefaceOrigin::Embedded { id },
                };
            }
        }

        if let Some(tf) = match_exact(&self.font_mgr, family, style) {
            log::debug!("[font] '{}' {:?} → exact match", family, style);
            return system_entry(tf);
        }

        if let Some(alias) = self
            .system_face_aliases
            .get_or_init(|| FaceAliasIndex::build(&self.font_mgr))
            .resolve(family)
        {
            let alias_style = style_for_face_alias(alias, style);
            if let Some(tf) = match_exact(&self.font_mgr, &alias.family, alias_style) {
                log::debug!(
                    "[font] '{}' {:?} → face alias '{}' {:?}",
                    family,
                    style,
                    alias.family,
                    alias_style
                );
                return system_entry(tf);
            }
        }

        if let Some(canonical) = localized_family_alias(family) {
            if let Some(tf) = match_exact(&self.font_mgr, canonical, style) {
                log::debug!(
                    "[font] '{}' {:?} -> localized family alias '{}'",
                    family,
                    style,
                    canonical
                );
                return system_entry(tf);
            }
        }

        if let Some((matched, subs)) = substitutes_for(family) {
            for sub in subs {
                if let Some(tf) = match_exact(&self.font_mgr, sub, style) {
                    log::debug!(
                        "[font] '{}' {:?} → substitute '{}' (via '{}')",
                        family,
                        style,
                        sub,
                        matched
                    );
                    return system_entry(tf);
                }
            }
        }

        // Unreachable for a registry from `FontRegistry::build`, which rejects
        // a font-less `FontMgr` up front so this arm always has something to
        // return. A registry made with `FontRegistry::new` carries no such
        // guarantee — that constructor is for tests, which supply a real
        // `FontMgr`.
        let tf = self
            .font_mgr
            .legacy_make_typeface(None::<&str>, style)
            .expect("FontRegistry::build guarantees a last-resort typeface");
        log::debug!(
            "[font] '{}' {:?} → system default '{}'",
            family,
            style,
            tf.family_name()
        );
        system_entry(tf)
    }

    /// Resolve a typeface by exact family + style match, or `None` if the
    /// family is neither registered as an embedded font nor present in the
    /// host's font system.
    ///
    /// Unlike [`Self::resolve`], this does not fall back to substitutes or to the
    /// system default — necessary for the emoji pipeline, where substituting
    /// a non-emoji typeface for a missing color emoji font is never correct.
    pub fn resolve_exact(&self, family: &str, style: FontStyle) -> Option<TypefaceEntry> {
        let variant = variant_for_style(style);
        if let Some(id) = self
            .embedded_index
            .get(&(family.to_lowercase(), variant))
            .copied()
        {
            let bytes = &self.embedded[id.0 as usize].bytes;
            let data = Data::new_copy(bytes);
            if let Some(tf) = self.font_mgr.new_from_data(&data, 0) {
                return Some(TypefaceEntry {
                    typeface: tf,
                    origin: TypefaceOrigin::Embedded { id },
                });
            }
        }
        match_exact(&self.font_mgr, family, style).map(system_entry)
    }

    /// Resolve a typeface from the host font system only — bypasses the
    /// embedded-font index. Used by the color emoji pipeline: Word's font
    /// subsetter strips color glyph tables (sbix/CBDT/COLR/SVG) when
    /// embedding emoji fonts, so a docx-embedded "Segoe UI Emoji" carries
    /// the right family name but no color glyphs and must not satisfy
    /// emoji resolution.
    pub fn resolve_system_only(&self, family: &str, style: FontStyle) -> Option<TypefaceEntry> {
        match_exact(&self.font_mgr, family, style).map(system_entry)
    }

    /// The controlled color emoji face distributed with dxpdf. Keeping this
    /// outside the host `FontMgr` lookup makes Windows and Linux choose the
    /// same artwork without installing a system font.
    pub fn bundled_color_emoji(&self) -> Option<TypefaceEntry> {
        self.bundled_color_emoji
            .get_or_init(|| bundled_entry(&self.font_mgr, BundledFont::NotoColorEmoji))
            .clone()
    }

    /// Pin business-form symbols to the bundled monochrome Noto face.
    ///
    /// U+FE0F explicitly requests emoji presentation and therefore bypasses
    /// this path. Plain or U+FE0E text-presentation checkbox/check-mark
    /// characters remain monochrome even when DirectWrite would otherwise
    /// choose the colored Segoe UI Emoji face.
    pub fn bundled_symbol_family(&self, style: FontStyle, text: &str) -> Option<String> {
        let base = monochrome_business_symbol_base(text)?;
        let entry = self
            .bundled_symbols
            .get_or_init(|| bundled_entry(&self.font_mgr, BundledFont::NotoSansSymbols2))
            .as_ref()?;
        if entry.typeface.unichar_to_glyph(base as u32 as i32) == 0 {
            return None;
        }

        self.typefaces.borrow_mut().insert(
            TypefaceKey::new(BUNDLED_SYMBOL_FAMILY, style),
            entry.clone(),
        );
        Some(BUNDLED_SYMBOL_FAMILY.to_string())
    }

    /// Resolve a system typeface for `text` only when the requested typeface
    /// does not cover every Unicode scalar in that text.
    ///
    /// Word performs font fallback below the run level: a run tagged Arial
    /// can still draw symbols such as U+2610 BALLOT BOX through Segoe UI
    /// Symbol. Skia's `draw_str` does not do that fallback for a pre-resolved
    /// [`Typeface`]; missing codepoints map to glyph 0 and silently disappear
    /// from the PDF. Returning the actual fallback face lets layout split the
    /// affected grapheme into its own measured fragment, keeping measurement
    /// and paint on the same typeface.
    ///
    /// `None` means either the requested face already covers `text`, or the
    /// host has no single fallback face that covers the complete grapheme.
    pub fn resolve_text_fallback(
        &self,
        family: &str,
        style: FontStyle,
        text: &str,
    ) -> Option<TypefaceEntry> {
        let requested = self.resolve(family, style);
        let codepoints: Vec<_> = text
            .chars()
            .filter(|ch| !ch.is_control())
            .map(|ch| ch as u32 as i32)
            .collect();
        if codepoints.is_empty()
            || codepoints
                .iter()
                .all(|&ch| requested.typeface.unichar_to_glyph(ch) != 0)
        {
            return None;
        }

        let missing = codepoints
            .iter()
            .copied()
            .find(|&ch| requested.typeface.unichar_to_glyph(ch) == 0)?;
        let fallback = self
            .font_mgr
            .match_family_style_character(family, style, &["zh-CN", "en-US"], missing)
            .or_else(|| {
                self.font_mgr
                    .match_family_style_character("", style, &["zh-CN", "en-US"], missing)
            })?;

        if TypefaceId::from(&fallback) == TypefaceId::from(&requested.typeface)
            || !codepoints
                .iter()
                .all(|&ch| fallback.unichar_to_glyph(ch) != 0)
        {
            return None;
        }

        Some(system_entry(fallback))
    }

    /// Resolve and pin a text fallback face under a render-local family key.
    ///
    /// DirectWrite can return a fallback face whose reported `family_name`
    /// equals the requested family even though it is a distinct typeface.
    /// Re-resolving that name during paint then selects the original missing-
    /// glyph face again. The synthetic key keeps the exact selected typeface
    /// stable through layout, subsetting, and paint without exposing the key
    /// outside this render.
    pub fn text_fallback_family(
        &self,
        family: &str,
        style: FontStyle,
        text: &str,
    ) -> Option<String> {
        if let Some(symbol_family) = self.bundled_symbol_family(style, text) {
            return Some(symbol_family);
        }
        let mut entry = self.resolve_text_fallback(family, style, text)?;
        let id = TypefaceId::from(&entry.typeface);
        entry.origin = TypefaceOrigin::SystemFallback { typeface_id: id };
        let alias = format!("__dxpdf_fallback_{}", id.0);
        log::debug!(
            "[font] '{}' {:?} missing {:?} -> fallback '{}' as '{}' (id={})",
            family,
            style,
            text,
            entry.typeface.family_name(),
            alias,
            id.0
        );
        self.typefaces
            .borrow_mut()
            .insert(TypefaceKey::new(&alias, style), entry);
        Some(alias)
    }

    /// Pre-resolve all four style variants for each family.
    pub fn preload(&self, families: &[String]) {
        let styles = [
            FontStyle::normal(),
            FontStyle::bold(),
            FontStyle::italic(),
            FontStyle::bold_italic(),
        ];
        for family in families {
            for &style in &styles {
                self.resolve(family, style);
            }
        }
    }

    /// Replace the typeface for every cached entry whose current id matches
    /// `old_id`. Returns the number of entries updated. Used by the font-
    /// subsetting pass to swap in subsetted bytes; multiple cache keys can
    /// share one underlying typeface (e.g. Calibri → Carlito substitution
    /// causes both keys to point at the same Skia typeface), so we update
    /// them all at once.
    pub fn replace_typeface_by_id(
        &mut self,
        old_id: TypefaceId,
        new_typeface: Typeface,
        new_origin: TypefaceOrigin,
    ) -> usize {
        let mut count = 0;
        for entry in self.typefaces.get_mut().values_mut() {
            if TypefaceId::from(&entry.typeface) == old_id {
                entry.typeface = new_typeface.clone();
                entry.origin = new_origin.clone();
                count += 1;
            }
        }
        count
    }

    /// Snapshot of all cached entries.
    pub fn cached_entries(&self) -> Vec<(TypefaceKey, TypefaceEntry)> {
        self.typefaces
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn font_style_for_variant(v: EmbeddedFontVariant) -> FontStyle {
    match v {
        EmbeddedFontVariant::Regular => FontStyle::normal(),
        EmbeddedFontVariant::Bold => FontStyle::bold(),
        EmbeddedFontVariant::Italic => FontStyle::italic(),
        EmbeddedFontVariant::BoldItalic => FontStyle::bold_italic(),
    }
}

fn variant_for_style(style: FontStyle) -> EmbeddedFontVariant {
    use skia_safe::font_style::{Slant, Weight};
    let bold = *style.weight() >= *Weight::SEMI_BOLD;
    let italic = matches!(style.slant(), Slant::Italic | Slant::Oblique);
    match (bold, italic) {
        (true, true) => EmbeddedFontVariant::BoldItalic,
        (true, false) => EmbeddedFontVariant::Bold,
        (false, true) => EmbeddedFontVariant::Italic,
        (false, false) => EmbeddedFontVariant::Regular,
    }
}

/// Does `tf` actually carry the family that was asked for?
///
/// `FontMgr::match_family_style` is permitted to answer with a *substitute*
/// rather than declining, and whether it does is platform-dependent:
/// fontconfig routinely falls back, while CoreText returns `None` for a family
/// it does not have (measured — every unknown name tried on macOS, including
/// the CSS generics `sans-serif`/`serif`/`monospace`, yields `None`). So this
/// guard is inert on macOS and load-bearing on Linux, where without it every
/// miss would be swallowed as a hit and steps 3–5 would be unreachable.
///
/// Split out from [`match_exact`] so it can be tested directly: on a host whose
/// matcher never substitutes there is no way to reach the rejection path
/// through `match_exact` itself.
fn is_exact_family(tf: &Typeface, requested: &str) -> bool {
    tf.family_name().eq_ignore_ascii_case(requested)
}

fn match_exact(font_mgr: &FontMgr, family: &str, style: FontStyle) -> Option<Typeface> {
    font_mgr
        .match_family_style(family, style)
        .filter(|tf| is_exact_family(tf, family))
}

fn system_entry(tf: Typeface) -> TypefaceEntry {
    let id = TypefaceId::from(&tf);
    TypefaceEntry {
        typeface: tf,
        origin: TypefaceOrigin::System { typeface_id: id },
    }
}

fn bundled_entry(font_mgr: &FontMgr, font: BundledFont) -> Option<TypefaceEntry> {
    // SAFETY: `include_bytes!` gives both assets static storage, so the slice
    // outlives Skia's reference-counted `Data` and every typeface created
    // from it. Avoiding a copy saves ~12 MB of allocation per conversion.
    let data = unsafe { Data::new_bytes(bundled_font_bytes(font)) };
    let typeface = font_mgr.new_from_data(&data, 0);
    if typeface.is_none() {
        log::warn!("could not load bundled font asset {font:?}");
    }
    typeface.map(|typeface| TypefaceEntry {
        typeface,
        origin: TypefaceOrigin::Bundled { font },
    })
}

fn monochrome_business_symbol_base(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let base = chars.next()?;
    if !matches!(
        base,
        '\u{2610}' | '\u{2611}' | '\u{2612}' | '\u{2713}' | '\u{2714}'
    ) {
        return None;
    }
    chars.all(|ch| ch == '\u{FE0E}').then_some(base)
}

// ─── FontCache (per-component, not per-render) ──────────────────────────────

#[derive(Hash, Eq, PartialEq)]
struct FontKey {
    family: String,
    /// Font size stored as bits for exact f32 hashing.
    size_bits: u32,
    weight: i32,
    slant: skia_safe::font_style::Slant,
}

fn needs_synthetic_bold(requested_bold: bool, resolved_weight: i32) -> bool {
    requested_bold && resolved_weight < *skia_safe::font_style::Weight::SEMI_BOLD
}

/// Raw (un-folded) inputs of the most recent [`FontCache::get`] call plus the
/// slot its result lives in. Words within a run share one `FontProps`, so
/// consecutive calls usually match this exactly and skip the `to_lowercase`
/// allocation and the hash lookup entirely.
struct LastCall {
    family: String,
    size_bits: u32,
    weight: i32,
    slant: skia_safe::font_style::Slant,
    idx: usize,
}

/// Per-component cache of fully-configured `Font` objects, avoiding repeated
/// `FontRegistry::resolve` lookups and `Font::from_typeface` construction.
///
/// `fonts` owns the resolved `Font`s (append-only, so slot indices stay
/// stable); `index` maps a case-folded `FontKey` to a slot; `last` is a
/// one-entry fast path keyed on the raw inputs of the previous call, so the
/// common per-word case costs no allocation and no hashing.
///
/// Must be discarded if the underlying `FontRegistry` is mutated (e.g. by
/// `replace_typeface_by_id`). The render pipeline already creates a fresh
/// `FontCache` for layout and another for paint, with the subset pass in
/// between; no stale Font objects can survive.
#[derive(Default)]
pub struct FontCache {
    fonts: Vec<Font>,
    index: HashMap<FontKey, usize>,
    last: Option<LastCall>,
}

impl FontCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get or create a `Font` for the given properties.
    pub fn get(
        &mut self,
        registry: &FontRegistry,
        font_family: &str,
        font_size: Pt,
        bold: bool,
        italic: bool,
    ) -> &Font {
        self.get_indexed(registry, font_family, font_size, bold, italic)
            .1
    }

    /// Like [`get`](Self::get), but also returns the resolved `Font`'s stable
    /// slot index. The index is a cheap integer identity for the (family, size,
    /// weight, slant) tuple — higher-level caches (e.g. the measurer's width
    /// memo) key on it to avoid re-hashing the family string per call.
    pub fn get_indexed(
        &mut self,
        registry: &FontRegistry,
        font_family: &str,
        font_size: Pt,
        bold: bool,
        italic: bool,
    ) -> (usize, &Font) {
        let style = match (bold, italic) {
            (true, true) => FontStyle::bold_italic(),
            (true, false) => FontStyle::bold(),
            (false, true) => FontStyle::italic(),
            (false, false) => FontStyle::normal(),
        };
        let size_bits = f32::from(font_size).to_bits();
        let weight = *style.weight();
        let slant = style.slant();

        // Fast path: identical inputs to the previous call. Exact string
        // comparison keeps this behaviour-identical to the case-folded slow
        // path (a hit resolves to the same `Font`), while avoiding the
        // per-call `to_lowercase` allocation and hash probe. `idx` is copied
        // out so the `last` borrow ends before `fonts` is indexed.
        let hit = self.last.as_ref().and_then(|last| {
            (last.size_bits == size_bits
                && last.weight == weight
                && last.slant == slant
                && last.family == font_family)
                .then_some(last.idx)
        });
        if let Some(idx) = hit {
            return (idx, &self.fonts[idx]);
        }

        // Slow path: case-folded hash lookup. The owned `FontKey` (with its
        // lowercased `String`) is built only here — at most once per distinct
        // (family, size, style), not once per call.
        let key = FontKey {
            family: font_family.to_lowercase(),
            size_bits,
            weight,
            slant,
        };
        let idx = match self.index.get(&key) {
            Some(&i) => i,
            None => {
                let resolve_style = FontStyle::new(
                    skia_safe::font_style::Weight::from(weight),
                    skia_safe::font_style::Width::NORMAL,
                    slant,
                );
                let entry = registry.resolve(font_family, resolve_style);
                let resolved_weight = *entry.typeface.font_style().weight();
                let mut font = Font::from_typeface(entry.typeface, f32::from(font_size));
                // Families such as SimSun commonly ship only a regular face.
                // FontMgr then returns that regular outline even for a bold
                // request. Preserve the OOXML distinction by asking Skia to
                // synthesize weight only when no semibold-or-heavier face was
                // actually resolved; real bold fonts remain untouched.
                font.set_embolden(needs_synthetic_bold(bold, resolved_weight));
                font.set_subpixel(true);
                font.set_linear_metrics(true);
                font.set_hinting(skia_safe::FontHinting::None);
                let i = self.fonts.len();
                self.fonts.push(font);
                self.index.insert(key, i);
                i
            }
        };
        self.last = Some(LastCall {
            family: font_family.to_owned(),
            size_bits,
            weight,
            slant,
            idx,
        });
        (idx, &self.fonts[idx])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skia_safe::font_style::{Slant, Weight, Width};

    #[test]
    fn synthetic_bold_is_used_only_when_a_bold_face_is_missing() {
        assert!(needs_synthetic_bold(true, *Weight::NORMAL));
        assert!(needs_synthetic_bold(true, *Weight::MEDIUM));
        assert!(!needs_synthetic_bold(true, *Weight::SEMI_BOLD));
        assert!(!needs_synthetic_bold(true, *Weight::BOLD));
        assert!(!needs_synthetic_bold(false, *Weight::NORMAL));
    }

    fn fmgr() -> FontMgr {
        FontMgr::new()
    }

    #[test]
    fn covered_text_does_not_request_a_fallback_face() {
        let manager = fmgr();
        let family = manager
            .legacy_make_typeface(None::<&str>, FontStyle::normal())
            .expect("test host must expose a default typeface")
            .family_name();
        let registry = FontRegistry::new(manager);
        assert!(registry
            .resolve_text_fallback(&family, FontStyle::normal(), "A")
            .is_none());
    }

    #[test]
    fn business_checkboxes_use_bundled_monochrome_symbols() {
        let registry = FontRegistry::new(fmgr());
        for symbol in ['\u{2610}', '\u{2611}', '\u{2612}', '\u{2713}', '\u{2714}'] {
            let family = registry
                .text_fallback_family("Arial", FontStyle::normal(), &symbol.to_string())
                .expect("bundled symbol face must cover business check marks");
            let entry = registry.resolve(&family, FontStyle::normal());
            assert!(matches!(
                entry.origin,
                TypefaceOrigin::Bundled {
                    font: BundledFont::NotoSansSymbols2
                }
            ));
            assert_ne!(
                entry.typeface.unichar_to_glyph(symbol as u32 as i32),
                0,
                "bundled symbol face must contain {symbol:?}"
            );
        }
    }

    #[test]
    fn emoji_presentation_selector_bypasses_monochrome_symbol_face() {
        let registry = FontRegistry::new(fmgr());
        assert!(registry
            .bundled_symbol_family(FontStyle::normal(), "\u{2611}\u{FE0F}")
            .is_none());
        assert!(registry
            .bundled_symbol_family(FontStyle::normal(), "\u{2611}\u{FE0E}")
            .is_some());
    }

    #[test]
    fn bundled_color_emoji_font_loads_and_covers_common_emoji() {
        let registry = FontRegistry::new(fmgr());
        let entry = registry
            .bundled_color_emoji()
            .expect("bundled Noto Color Emoji must load through Skia");
        assert!(matches!(
            entry.origin,
            TypefaceOrigin::Bundled {
                font: BundledFont::NotoColorEmoji
            }
        ));
        assert_ne!(
            entry.typeface.unichar_to_glyph('\u{1F4DE}' as u32 as i32),
            0
        );
        let flag = crate::render::emoji::shape::ClusterShaper::new()
            .expect("Skia HarfBuzz shaper must be available")
            .shape(&entry.typeface, "\u{1F1E8}\u{1F1F3}", 32.0)
            .expect("bundled full COLRv1 font must shape a flag sequence");
        assert_eq!(flag.glyphs.len(), 1, "flag sequence must form one glyph");
        assert_ne!(flag.glyphs[0].id, 0, "flag glyph must not be .notdef");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn ballot_boxes_fall_back_from_arial_to_a_covering_face() {
        let registry = FontRegistry::new(fmgr());
        let requested = registry.resolve("Arial", FontStyle::normal());
        for symbol in ['\u{2610}', '\u{2611}'] {
            assert_eq!(
                requested.typeface.unichar_to_glyph(symbol as u32 as i32),
                0,
                "the regression requires Arial to lack {symbol:?}"
            );
            let fallback = registry
                .resolve_text_fallback("Arial", FontStyle::normal(), &symbol.to_string())
                .expect("Windows must provide a symbol fallback face");
            assert_ne!(
                fallback.typeface.unichar_to_glyph(symbol as u32 as i32),
                0,
                "the selected fallback must contain {symbol:?}"
            );
            let alias = registry
                .text_fallback_family("Arial", FontStyle::normal(), &symbol.to_string())
                .expect("fallback face must be pinned under a render-local alias");
            assert_ne!(
                registry
                    .resolve(&alias, FontStyle::normal())
                    .typeface
                    .unichar_to_glyph(symbol as u32 as i32),
                0,
                "the render-local alias must resolve to the selected fallback face"
            );
        }
    }

    fn test_alias(family: &str, weight: i32) -> FaceAlias {
        FaceAlias {
            family: family.to_owned(),
            weight,
        }
    }

    #[test]
    fn localized_cjk_family_names_map_to_windows_canonical_names() {
        assert_eq!(localized_family_alias("宋体"), Some("SimSun"));
        assert_eq!(
            localized_family_alias(" 微软雅黑 "),
            Some("Microsoft YaHei")
        );
        assert_eq!(localized_family_alias("等线"), Some("DengXian"));
        assert_eq!(localized_family_alias("仿宋_GB2312"), Some("FangSong"));
        assert_eq!(localized_family_alias("方正仿宋_GBK"), Some("FangSong"));
        assert_eq!(localized_family_alias("方正小标宋简体"), Some("SimSun"));
        assert_eq!(localized_family_alias("方正小标宋_GBK"), Some("SimSun"));
        assert_eq!(localized_family_alias("方正黑体_GBK"), Some("SimHei"));
        assert_eq!(localized_family_alias("方正楷体_GBK"), Some("KaiTi"));
        assert_eq!(localized_family_alias("Calibri"), None);
    }

    #[test]
    fn face_aliases_support_semibold_spellings() {
        let mut index = FaceAliasIndex::default();
        index.insert_face(
            "Proxima Nova",
            FontStyle::new(Weight::SEMI_BOLD, Width::NORMAL, Slant::Upright),
            Some("Semibold"),
            Some("ProximaNova-Semibold"),
        );

        for name in [
            "Proxima Nova Semibold",
            "Proxima Nova SemiBold",
            "Proxima Nova Semi Bold",
            "ProximaNova-Semibold",
        ] {
            let alias = index.resolve(name).expect("alias should resolve");
            assert_eq!(alias.family, "Proxima Nova");
            assert_eq!(alias.weight, *Weight::SEMI_BOLD);
        }
    }

    #[test]
    fn face_aliases_support_extra_bold_spellings() {
        let mut index = FaceAliasIndex::default();
        index.insert_face(
            "Inter",
            FontStyle::new(Weight::EXTRA_BOLD, Width::NORMAL, Slant::Upright),
            Some("ExtraBold"),
            Some("Inter-ExtraBold"),
        );

        for name in ["Inter ExtraBold", "Inter Extra Bold", "Inter-ExtraBold"] {
            assert_eq!(
                index.resolve(name).expect("alias should resolve").weight,
                *Weight::EXTRA_BOLD
            );
        }
    }

    #[test]
    fn face_alias_keys_ignore_case_and_repeated_whitespace() {
        assert_eq!(
            face_name_key("  Proxima   Nova SEMIBOLD "),
            face_name_key("proxima nova semibold")
        );
    }

    #[test]
    fn face_alias_keys_preserve_punctuation() {
        assert_ne!(face_name_key("A-B"), face_name_key("AB"));
    }

    #[test]
    fn conflicting_face_aliases_are_ambiguous() {
        let mut index = FaceAliasIndex::default();
        index.insert_alias("Shared Alias", test_alias("Family A", 600));
        index.insert_alias("Shared Alias", test_alias("Family B", 600));

        assert!(index.resolve("Shared Alias").is_none());
    }

    /// A bold run raises a lighter face, but never *lowers* an already-heavier
    /// one — `<w:b/>` on an ExtraBold face must not flatten it to Bold.
    #[test]
    fn bold_request_raises_a_lighter_face_but_never_lowers_a_heavier_one() {
        assert_eq!(
            merged_alias_weight(*Weight::SEMI_BOLD, *Weight::BOLD),
            *Weight::BOLD
        );
        assert_eq!(
            merged_alias_weight(*Weight::EXTRA_BOLD, *Weight::BOLD),
            *Weight::EXTRA_BOLD
        );
        assert_eq!(
            merged_alias_weight(*Weight::LIGHT, *Weight::BOLD),
            *Weight::BOLD
        );
    }

    /// H2#1 regression. The two cases above are the *only* ones the suite used
    /// to cover, and `max` is correct for both — every alias weight tested was
    /// at or above Regular. Below it the rule inverted: a `NORMAL` request
    /// means only "not bold", yet it overruled the face's own weight, so
    /// `"Calibri Light"` (alias weight 342 on macOS) came back as plain
    /// Calibri at 400.
    #[test]
    fn normal_request_leaves_a_light_face_at_its_own_weight() {
        assert_eq!(merged_alias_weight(342, *Weight::NORMAL), 342);
        assert_eq!(
            merged_alias_weight(*Weight::LIGHT, *Weight::NORMAL),
            *Weight::LIGHT
        );
        assert_eq!(
            merged_alias_weight(*Weight::THIN, *Weight::NORMAL),
            *Weight::THIN
        );
        // Unchanged where it already worked.
        assert_eq!(
            merged_alias_weight(*Weight::BOLD, *Weight::NORMAL),
            *Weight::BOLD
        );
        assert_eq!(
            merged_alias_weight(*Weight::NORMAL, *Weight::NORMAL),
            *Weight::NORMAL
        );
    }

    /// The same guarantee at the seam `resolve_uncached` actually calls, so a
    /// future change that reintroduces the promotion one level up is caught.
    #[test]
    fn face_alias_style_keeps_a_light_face_light() {
        let alias = test_alias("Calibri", 342);
        let resolved = style_for_face_alias(&alias, FontStyle::normal());
        assert_eq!(
            *resolved.weight(),
            342,
            "a non-bold run must not thicken the face"
        );

        let bolded = style_for_face_alias(&alias, FontStyle::bold());
        assert_eq!(
            *bolded.weight(),
            *Weight::BOLD,
            "an explicit bold run still applies"
        );
    }

    #[test]
    fn face_alias_style_preserves_requested_slant() {
        let alias = test_alias("Proxima Nova", *Weight::SEMI_BOLD);
        let requested = FontStyle::new(Weight::NORMAL, Width::NORMAL, Slant::Italic);

        let resolved = style_for_face_alias(&alias, requested);

        assert_eq!(*resolved.weight(), *Weight::SEMI_BOLD);
        assert_eq!(resolved.width(), Width::NORMAL);
        assert_eq!(resolved.slant(), Slant::Italic);
    }

    /// Pull bytes from a guaranteed-available system typeface so tests don't
    /// need bundled font fixtures for the registry-level invariants.
    fn arbitrary_system_font_bytes() -> Vec<u8> {
        let mgr = fmgr();
        let tf = mgr
            .legacy_make_typeface(None::<&str>, FontStyle::normal())
            .expect("system has no default typeface — cannot run test");
        let (bytes, _ttc_index) = tf
            .to_font_data()
            .expect("legacy default typeface lacks raw font bytes — cannot run test");
        bytes
    }

    #[test]
    fn registry_empty_after_construction() {
        let r = FontRegistry::new(fmgr());
        assert_eq!(r.embedded_font_count(), 0);
        assert_eq!(r.cached_typeface_count(), 0);
    }

    #[test]
    fn registry_resolves_system_font_idempotently() {
        let r = FontRegistry::new(fmgr());
        let a = r.resolve("DefinitelyNotInstalledXYZ", FontStyle::normal());
        let b = r.resolve("DefinitelyNotInstalledXYZ", FontStyle::normal());
        assert_eq!(
            TypefaceId::from(&a.typeface),
            TypefaceId::from(&b.typeface),
            "second resolution must hit the cache and yield the same typeface"
        );
        assert!(matches!(a.origin, TypefaceOrigin::System { .. }));
    }

    #[test]
    fn registry_embedded_takes_precedence_over_system() {
        let bytes = arbitrary_system_font_bytes();

        let mut without = FontRegistry::new(fmgr());
        let baseline = without.resolve("NonexistentFamilyABC", FontStyle::normal());
        assert!(
            matches!(baseline.origin, TypefaceOrigin::System { .. }),
            "without embedding, an unknown family must fall back to the system path"
        );
        // Quiet the unused-mut warning while making the contrast explicit.
        let _ = &mut without;

        let mut with = FontRegistry::new(fmgr());
        let id = with
            .register_embedded("NonexistentFamilyABC", EmbeddedFontVariant::Regular, bytes)
            .expect("register_embedded should accept a valid system font's bytes");
        let resolved = with.resolve("NonexistentFamilyABC", FontStyle::normal());
        assert_eq!(
            resolved.origin,
            TypefaceOrigin::Embedded { id },
            "after registration, resolution must return the embedded origin"
        );
    }

    #[test]
    fn registry_stores_embedded_bytes_byte_identical() {
        // ECMA-376 §17.8.3.3 deobfuscation is enforced upstream by the parser
        // (see src/docx/parse/fonts.rs::deobfuscate_round_trip). The registry-
        // level invariant is that the bytes it stores must be byte-identical
        // to the bytes handed in — no re-encoding, no normalization.
        let bytes = arbitrary_system_font_bytes();
        let mut r = FontRegistry::new(fmgr());
        let id = r
            .register_embedded(
                "ByteIdentityProbe",
                EmbeddedFontVariant::Regular,
                bytes.clone(),
            )
            .expect("registration should succeed for valid font bytes");
        assert_eq!(
            r.embedded_bytes(id),
            bytes.as_slice(),
            "stored bytes must match the originally passed-in bytes byte-for-byte"
        );
    }

    #[test]
    fn registry_drop_clears_all_typefaces() {
        // Two registries on the same thread must not share state — this is
        // the structural fix for the cross-render poisoning that the previous
        // thread_local!-backed cache caused.
        let r1 = FontRegistry::new(fmgr());
        let _ = r1.resolve("FamilyOne", FontStyle::normal());
        assert_eq!(r1.cached_typeface_count(), 1);
        drop(r1);

        let r2 = FontRegistry::new(fmgr());
        assert_eq!(
            r2.cached_typeface_count(),
            0,
            "a fresh registry must not see typefaces cached by an earlier one"
        );
    }

    #[test]
    fn registry_resolution_records_origin() {
        // Every resolved entry must report a non-default origin variant.
        let bytes = arbitrary_system_font_bytes();
        let mut r = FontRegistry::new(fmgr());
        r.register_embedded("OriginEmbedded", EmbeddedFontVariant::Regular, bytes)
            .unwrap();
        let _ = r.resolve("OriginEmbedded", FontStyle::normal());
        let _ = r.resolve("OriginSystemFallbackXYZ", FontStyle::normal());

        for (_, entry) in r.cached_entries() {
            match entry.origin {
                TypefaceOrigin::Embedded { .. }
                | TypefaceOrigin::System { .. }
                | TypefaceOrigin::SystemFallback { .. }
                | TypefaceOrigin::Bundled { .. } => {}
            }
        }
    }

    #[test]
    fn replace_typeface_by_id_updates_all_keys_pointing_at_it() {
        // Two distinct cache keys can share one underlying typeface (e.g. via
        // FONT_SUBSTITUTIONS). The subsetting pass must update them all so
        // paint never sees a stale typeface.
        let mut r = FontRegistry::new(fmgr());

        // Two unknown families → both fall back to the system default → same
        // underlying Skia typeface.
        let a = r.resolve("UnknownFamilyA", FontStyle::normal());
        let b = r.resolve("UnknownFamilyB", FontStyle::normal());
        assert_eq!(
            TypefaceId::from(&a.typeface),
            TypefaceId::from(&b.typeface),
            "test setup precondition — both unknowns must resolve to the same default"
        );
        let shared_id = TypefaceId::from(&a.typeface);

        // Build a *different* typeface to swap in.
        let bytes = arbitrary_system_font_bytes();
        let data = Data::new_copy(&bytes);
        let replacement = r
            .font_mgr()
            .new_from_data(&data, 0)
            .expect("replacement typeface should construct from valid bytes");
        let replacement_origin = TypefaceOrigin::System {
            typeface_id: TypefaceId::from(&replacement),
        };

        let updated = r.replace_typeface_by_id(shared_id, replacement, replacement_origin);
        assert_eq!(
            updated, 2,
            "both shared-typeface entries must be updated in lockstep"
        );

        // Subsequent resolution returns the new typeface, not the old one.
        let after = r.resolve("UnknownFamilyA", FontStyle::normal());
        assert_ne!(
            TypefaceId::from(&after.typeface),
            shared_id,
            "post-replace resolution must yield the new typeface"
        );
    }

    #[test]
    fn font_cache_uses_registry_after_replacement() {
        // The cross-cutting invariant: if the registry is mutated and a
        // *new* FontCache is created afterwards, that cache must produce
        // Fonts backed by the new typeface. (Stale FontCaches must be
        // discarded — that contract is satisfied by the pipeline creating
        // fresh caches around the subset pass.)
        let mut r = FontRegistry::new(fmgr());
        let original = r.resolve("CacheReplaceProbe", FontStyle::normal());
        let original_id = TypefaceId::from(&original.typeface);

        let bytes = arbitrary_system_font_bytes();
        let data = Data::new_copy(&bytes);
        let replacement = r
            .font_mgr()
            .new_from_data(&data, 0)
            .expect("replacement typeface should construct");
        let new_id = TypefaceId::from(&replacement);
        assert_ne!(
            new_id, original_id,
            "test precondition — replacement must be a different typeface"
        );
        let replacement_origin = TypefaceOrigin::System {
            typeface_id: new_id,
        };
        r.replace_typeface_by_id(original_id, replacement, replacement_origin);

        let mut fresh_cache = FontCache::new();
        let font = fresh_cache.get(&r, "CacheReplaceProbe", Pt::new(12.0), false, false);
        assert_eq!(
            TypefaceId::from(&font.typeface()),
            new_id,
            "FontCache must observe the post-replacement typeface"
        );
    }

    #[test]
    fn resolve_exact_returns_none_for_unknown_family() {
        let r = FontRegistry::new(fmgr());
        assert!(
            r.resolve_exact("DefinitelyNotInstalledXYZ", FontStyle::normal())
                .is_none(),
            "unlike resolve(), resolve_exact must not invent a fallback"
        );
    }

    #[test]
    fn resolve_exact_returns_some_for_embedded_family() {
        let bytes = arbitrary_system_font_bytes();
        let mut r = FontRegistry::new(fmgr());
        r.register_embedded("ExactProbe", EmbeddedFontVariant::Regular, bytes)
            .expect("register_embedded should accept valid font bytes");
        let entry = r
            .resolve_exact("ExactProbe", FontStyle::normal())
            .expect("embedded font must be resolvable via exact match");
        assert!(matches!(entry.origin, TypefaceOrigin::Embedded { .. }));
    }

    /// Word's font subsetter strips color glyph tables (sbix/CBDT/COLR/SVG)
    /// when embedding emoji fonts. A docx that embeds e.g. "Segoe UI Emoji"
    /// therefore registers a typeface with the right family name but only
    /// monochrome outlines — rasterizing it produces black blobs instead of
    /// the colored glyph. The emoji pipeline must bypass embedded fonts so
    /// resolution falls through to the host's real color emoji typeface.
    #[test]
    fn resolve_system_only_skips_embedded_fonts() {
        let bytes = arbitrary_system_font_bytes();
        let mut r = FontRegistry::new(fmgr());
        r.register_embedded("SystemOnlyProbe", EmbeddedFontVariant::Regular, bytes)
            .expect("register_embedded should accept valid font bytes");

        // resolve_exact returns the embedded font:
        let exact = r
            .resolve_exact("SystemOnlyProbe", FontStyle::normal())
            .expect("embedded font must be reachable via resolve_exact");
        assert!(matches!(exact.origin, TypefaceOrigin::Embedded { .. }));

        // resolve_system_only must NOT return it (no system font has this
        // synthetic family name):
        assert!(
            r.resolve_system_only("SystemOnlyProbe", FontStyle::normal())
                .is_none(),
            "resolve_system_only must skip embedded fonts so emoji resolution \
             can fall through to the host's color emoji typeface"
        );
    }

    // ─── Face-qualified substitution (H2#4) ───────────────────────────────

    #[test]
    fn strips_a_trailing_weight_word_to_the_base_family() {
        assert_eq!(strip_weight_suffix("Segoe UI Light"), Some("Segoe UI"));
        assert_eq!(strip_weight_suffix("Calibri Light"), Some("Calibri"));
        assert_eq!(strip_weight_suffix("Segoe UI Semibold"), Some("Segoe UI"));
        assert_eq!(strip_weight_suffix("Arial Black"), Some("Arial"));
        // Case-insensitive, and tolerant of the spacing variants the alias
        // index already accepts.
        assert_eq!(strip_weight_suffix("Calibri LIGHT"), Some("Calibri"));
        assert_eq!(strip_weight_suffix("Foo Extra Bold"), Some("Foo"));
    }

    /// The longest suffix wins, so a two-word weight name is not left half
    /// attached.
    #[test]
    fn longest_weight_suffix_wins() {
        assert_eq!(strip_weight_suffix("Foo Extra Light"), Some("Foo"));
        assert_eq!(strip_weight_suffix("Foo Ultra Bold"), Some("Foo"));
    }

    /// A weight word must be a separate trailing word — otherwise every family
    /// ending in those letters would be silently truncated.
    #[test]
    fn weight_word_must_be_its_own_word() {
        assert_eq!(strip_weight_suffix("Highlight"), None);
        assert_eq!(strip_weight_suffix("Blackadder"), None);
        assert_eq!(strip_weight_suffix("Light"), None, "nothing precedes it");
        assert_eq!(strip_weight_suffix("Times New Roman"), None);
    }

    /// H2#4 regression: `"Segoe UI"` is in the table, so a face-qualified name
    /// built on it must reach the same substitutes. Before this, the lookup
    /// was an exact whole-name match and face names fell straight through to
    /// the system default.
    #[test]
    fn substitutes_reach_face_qualified_names_through_the_base_family() {
        let (matched, subs) = substitutes_for("Segoe UI").expect("base family is in the table");
        assert_eq!(matched, "Segoe UI");

        let (via, face_subs) =
            substitutes_for("Segoe UI Light").expect("face-qualified name must reach the family");
        assert_eq!(via, "Segoe UI", "resolved through the base family");
        assert_eq!(face_subs, subs, "and gets the same substitutes");

        assert_eq!(
            substitutes_for("Calibri Light").map(|(k, _)| k),
            Some("Calibri")
        );
        // A family that is not in the table stays absent, stripped or not.
        assert!(substitutes_for("Wingdings").is_none());
        assert!(substitutes_for("Wingdings Light").is_none());
    }

    #[test]
    fn century_schoolbook_uses_the_word_compatible_windows_fallback_first() {
        let (matched, substitutes) =
            substitutes_for("Century Schoolbook").expect("known legacy family");
        assert_eq!(matched, "Century Schoolbook");
        assert_eq!(substitutes.first(), Some(&"Segoe Print"));
    }

    // ─── Chain guards (H2#6) ──────────────────────────────────────────────

    /// `match_exact` is the guard that makes steps 3–5 reachable at all.
    /// Skia's matcher returns *something* for any request, so without the
    /// family-name check every miss would be swallowed as a hit and the
    /// substitution chain would be dead code.
    /// The guard that makes steps 3–5 reachable: a matcher that substitutes
    /// (fontconfig does; CoreText does not) would otherwise report every miss
    /// as a hit. Tested on the predicate rather than through `match_exact`,
    /// because on a non-substituting host the rejection path is unreachable
    /// from there — the test would pass without the guard existing.
    #[test]
    fn exact_family_guard_rejects_a_substituted_face() {
        let mgr = fmgr();
        let tf = mgr
            .legacy_make_typeface(None::<&str>, FontStyle::normal())
            .expect("system default typeface");
        let name = tf.family_name();

        assert!(is_exact_family(&tf, &name), "its own family must match");
        assert!(
            is_exact_family(&tf, &name.to_uppercase()),
            "the comparison is case-insensitive"
        );
        assert!(
            !is_exact_family(&tf, "No Such Family 8f3a1c"),
            "a face carrying a different family must be refused — this is the \
             whole reason the substitution chain is reachable"
        );
        // And the wiring: a real family still resolves through `match_exact`.
        assert!(match_exact(&mgr, &name, FontStyle::normal()).is_some());
    }

    /// The embedded-variant bucket is chosen by weight threshold, not by an
    /// exact 400/700 match — a run at Medium or Semibold must still find the
    /// embedded Bold face rather than falling through to the system.
    #[test]
    fn variant_for_style_buckets_at_semi_bold() {
        use EmbeddedFontVariant::*;
        let at = |w: Weight, s: Slant| variant_for_style(FontStyle::new(w, Width::NORMAL, s));
        assert_eq!(at(Weight::NORMAL, Slant::Upright), Regular);
        assert_eq!(
            at(Weight::MEDIUM, Slant::Upright),
            Regular,
            "500 is not bold"
        );
        assert_eq!(
            at(Weight::SEMI_BOLD, Slant::Upright),
            Bold,
            "600 is the cutoff"
        );
        assert_eq!(at(Weight::BOLD, Slant::Italic), BoldItalic);
        assert_eq!(
            at(Weight::LIGHT, Slant::Oblique),
            Italic,
            "oblique counts as italic"
        );
    }
}
