//! Bounded SVG rasterization for DrawingML preferred picture sources.
//!
//! SVG stays entirely in the paint phase: the authored picture frame remains
//! the sole layout extent.  A closed resource provider permits bounded
//! `data:` assets and never opens a URL or local path.

use std::borrow::Cow;
use std::collections::HashSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, Writer};
use skia_safe::codec::Codec;
use skia_safe::resources::{
    helpers::{self, ResourceKind},
    ImageAsset, ResourceProvider,
};
use skia_safe::{
    AlphaType, Color4f, ColorSpace, ColorType, Data, FontMgr, Image, ImageInfo, Matrix, Rect,
};

use crate::render::geometry::PtRect;

const POINTS_PER_INCH: f64 = 72.0;
const CSS_PIXELS_PER_INCH: f64 = 96.0;

pub(crate) const MAX_SVG_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_SVG_RASTER_PIXELS: usize = 8_000_000;
const MAX_SVG_RASTER_AXIS: usize = 32_768;
const MAX_SVG_ELEMENTS: usize = 100_000;
const MAX_SVG_DEPTH: usize = 256;
const MAX_SVG_ATTRIBUTES: usize = 500_000;
const MAX_SVG_IMAGE_ELEMENTS: usize = 4_096;
const MAX_CSS_BYTES: usize = 256 * 1024;
const MAX_CSS_CLASS_RULES: usize = 64;
const MAX_CSS_DECLARATIONS_PER_RULE: usize = 16;
const MAX_CSS_CLASS_TOKENS: usize = 16;
const MAX_INLINED_SVG_BYTES: usize = 32 * 1024 * 1024;
const MAX_DATA_URI_CHARS: usize = MAX_SVG_BYTES;
const MAX_EMBEDDED_RESOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_EMBEDDED_RESOURCE_BYTES_TOTAL: usize = 16 * 1024 * 1024;
const MAX_EMBEDDED_RESOURCE_PIXELS: usize = 8_000_000;
const MAX_EMBEDDED_RESOURCE_PIXELS_TOTAL: usize = 16_000_000;
const MAX_EMBEDDED_RESOURCE_FRAMES: usize = 256;

#[derive(Default)]
struct ResourceBudget {
    bytes: usize,
    pixels: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EmbeddedRasterFormat {
    Png,
    Jpeg,
    Gif,
    WebP,
}

impl EmbeddedRasterFormat {
    fn from_data_header(header: &str) -> Option<Self> {
        let mime = header
            .strip_prefix("data:")?
            .strip_suffix(";base64")?
            .to_ascii_lowercase();
        match mime.as_str() {
            "image/png" => Some(Self::Png),
            "image/jpeg" | "image/jpg" => Some(Self::Jpeg),
            "image/gif" => Some(Self::Gif),
            "image/webp" => Some(Self::WebP),
            _ => None,
        }
    }

    fn from_magic(data: &[u8]) -> Option<Self> {
        match data {
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => Some(Self::Png),
            [0xff, 0xd8, 0xff, ..] => Some(Self::Jpeg),
            [b'G', b'I', b'F', b'8', ..] => Some(Self::Gif),
            [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some(Self::WebP),
            _ => None,
        }
    }
}

struct BoundedResourceProvider {
    font_mgr: FontMgr,
    budget: Arc<Mutex<ResourceBudget>>,
    failed: Arc<AtomicBool>,
}

impl BoundedResourceProvider {
    fn new(font_mgr: FontMgr, failed: Arc<AtomicBool>) -> Self {
        Self {
            font_mgr,
            budget: Arc::new(Mutex::new(ResourceBudget::default())),
            failed,
        }
    }

    fn mark_failed(&self) {
        self.failed.store(true, Ordering::Relaxed);
    }

    fn decode_data_uri(&self, resource_name: &str) -> Option<Data> {
        if !resource_name.starts_with("data:") || resource_name.len() > MAX_DATA_URI_CHARS {
            return None;
        }
        let (header, payload) = resource_name.split_once(',')?;
        if payload.is_empty() || payload.contains(',') {
            return None;
        }
        let declared = EmbeddedRasterFormat::from_data_header(header)?;
        let ResourceKind::Base64(data) = helpers::identify_resource_kind("", resource_name) else {
            return None;
        };
        if data.size() == 0
            || data.size() > MAX_EMBEDDED_RESOURCE_BYTES
            || EmbeddedRasterFormat::from_magic(data.as_bytes()) != Some(declared)
        {
            return None;
        }
        Some(data)
    }

    fn reserve(&self, bytes: usize, pixels: usize) -> bool {
        let Ok(mut budget) = self.budget.lock() else {
            return false;
        };
        let Some(next_bytes) = budget.bytes.checked_add(bytes) else {
            return false;
        };
        let Some(next_pixels) = budget.pixels.checked_add(pixels) else {
            return false;
        };
        if next_bytes > MAX_EMBEDDED_RESOURCE_BYTES_TOTAL
            || next_pixels > MAX_EMBEDDED_RESOURCE_PIXELS_TOTAL
        {
            return false;
        }
        budget.bytes = next_bytes;
        budget.pixels = next_pixels;
        true
    }

    fn load_image_asset_impl(&self, resource_name: &str) -> Option<ImageAsset> {
        let data = self.decode_data_uri(resource_name)?;
        let mut codec = Codec::from_data(data.clone())?;
        let dimensions = codec.dimensions();
        if dimensions.width <= 0 || dimensions.height <= 0 {
            return None;
        }
        let frame_count = codec.get_frame_count().max(1);
        if frame_count > MAX_EMBEDDED_RESOURCE_FRAMES {
            return None;
        }
        let pixels_per_frame = usize::try_from(dimensions.width)
            .ok()?
            .checked_mul(usize::try_from(dimensions.height).ok()?)?;
        if pixels_per_frame > MAX_EMBEDDED_RESOURCE_PIXELS {
            return None;
        }
        let total_pixels = pixels_per_frame.checked_mul(frame_count)?;
        if !self.reserve(data.size(), total_pixels) {
            return None;
        }
        ImageAsset::from_data(data, None)
    }
}

impl ResourceProvider for BoundedResourceProvider {
    fn load(&self, _resource_path: &str, _resource_name: &str) -> Option<Data> {
        // Generic loads have no pixel/frame contract and could let an image
        // bypass `load_image_asset`. This package supports embedded raster
        // images only through the audited image callback.
        self.mark_failed();
        None
    }

    fn load_image_asset(
        &self,
        _resource_path: &str,
        resource_name: &str,
        _resource_id: &str,
    ) -> Option<ImageAsset> {
        let loaded = catch_unwind(AssertUnwindSafe(|| {
            self.load_image_asset_impl(resource_name)
        }))
        .ok()
        .flatten();
        if loaded.is_none() {
            self.mark_failed();
        }
        loaded
    }

    fn load_typeface(&self, name: &str, url: &str) -> Option<skia_safe::Typeface> {
        let loaded = catch_unwind(AssertUnwindSafe(|| {
            helpers::load_typeface(self, &self.font_mgr, name, url)
        }))
        .ok()
        .flatten();
        if loaded.is_none() {
            self.mark_failed();
        }
        loaded
    }

    fn font_mgr(&self) -> FontMgr {
        self.font_mgr.clone()
    }
}

/// Rasterize one SVG into the already-resolved visible destination size.
///
/// `frame_rect` is the original authored `DrawCommand::Image.rect`; it defines
/// the DPI-independent CSS viewport for percentage-sized roots. `display_rect`
/// is the visible destination after negative `srcRect` padding is resolved.
/// The returned bitmap has crop baked in and maps whole-image to
/// `display_rect`; neither rectangle is modified or returned to layout.
pub(crate) fn rasterize_svg(
    data: &[u8],
    frame_rect: PtRect,
    display_rect: PtRect,
    crop: Option<&PtRect>,
    image_dpi: f32,
    font_mgr: FontMgr,
) -> Option<Image> {
    if !validate_svg_input(data) {
        return None;
    }
    let markup = inline_simple_class_styles(data)?;
    let (target_w, target_h) = bounded_target_pixels(display_rect, image_dpi)?;
    let crop = normalized_crop(crop)?;

    let resource_failed = Arc::new(AtomicBool::new(false));
    let provider = BoundedResourceProvider::new(font_mgr, Arc::clone(&resource_failed));
    let mut dom = catch_unwind(AssertUnwindSafe(|| {
        skia_safe::svg::Dom::from_bytes(markup.as_ref(), provider)
    }))
    .ok()?
    .ok()?;
    if resource_failed.load(Ordering::Relaxed) {
        return None;
    }
    let frame_w = frame_rect.size.width.raw() as f64 * CSS_PIXELS_PER_INCH / POINTS_PER_INCH;
    let frame_h = frame_rect.size.height.raw() as f64 * CSS_PIXELS_PER_INCH / POINTS_PER_INCH;
    if !positive_finite_f64(frame_w) || !positive_finite_f64(frame_h) {
        return None;
    }
    let root = dom.root();
    let intrinsic = dom.root().intrinsic_size();
    let viewport_w = resolve_root_axis(*root.width(), intrinsic.width, frame_w)?;
    let viewport_h = resolve_root_axis(*root.height(), intrinsic.height, frame_h)?;
    // Set an explicit logical viewport in both branches. This keeps relative
    // root lengths independent of image_dpi while making fixed-size roots
    // render into their own intrinsic viewport before the OOXML stretch.
    dom.set_container_size((viewport_w as f32, viewport_h as f32));

    let visible_w = viewport_w * crop.2;
    let visible_h = viewport_h * crop.3;
    if !positive_finite_f64(visible_w) || !positive_finite_f64(visible_h) {
        return None;
    }
    let sx = target_w as f64 / visible_w;
    let sy = target_h as f64 / visible_h;
    let tx = -viewport_w * crop.0 * sx;
    let ty = -viewport_h * crop.1 * sy;
    if ![sx, sy, tx, ty].into_iter().all(f64::is_finite)
        || [sx, sy, tx, ty]
            .into_iter()
            .any(|value| value < f32::MIN as f64 || value > f32::MAX as f64)
    {
        return None;
    }

    let info = ImageInfo::new(
        (target_w, target_h),
        ColorType::RGBA8888,
        AlphaType::Premul,
        ColorSpace::new_srgb(),
    );
    let mut surface = skia_safe::surfaces::raster(&info, None, None)?;
    let canvas = surface.canvas();
    canvas.clear(Color4f::new(0.0, 0.0, 0.0, 0.0));
    canvas.clip_rect(Rect::from_iwh(target_w, target_h), None, false);
    canvas.concat(&Matrix::new_all(
        sx as f32, 0.0, tx as f32, 0.0, sy as f32, ty as f32, 0.0, 0.0, 1.0,
    ));
    if catch_unwind(AssertUnwindSafe(|| dom.render(canvas))).is_err() {
        return None;
    }
    if resource_failed.load(Ordering::Relaxed) {
        return None;
    }
    Some(surface.image_snapshot())
}

fn resolve_root_axis(
    length: skia_safe::svg::Length,
    intrinsic: f32,
    frame_css_px: f64,
) -> Option<f64> {
    use skia_safe::svg::LengthUnit;

    let value = length.value as f64;
    let resolved = match length.unit {
        LengthUnit::Number | LengthUnit::PX => value,
        LengthUnit::Percentage => frame_css_px * value / 100.0,
        LengthUnit::IN => value * CSS_PIXELS_PER_INCH,
        LengthUnit::CM => value * CSS_PIXELS_PER_INCH / 2.54,
        LengthUnit::MM => value * CSS_PIXELS_PER_INCH / 25.4,
        LengthUnit::PT => value * CSS_PIXELS_PER_INCH / POINTS_PER_INCH,
        LengthUnit::PC => value * CSS_PIXELS_PER_INCH / 6.0,
        // Root em/ex depend on text state we deliberately do not expose as a
        // layout input. Use a concrete intrinsic result when Skia has one;
        // otherwise fail to the bitmap instead of guessing.
        LengthUnit::EMS | LengthUnit::EXS | LengthUnit::Unknown => intrinsic as f64,
    };
    positive_finite_f64(resolved).then_some(resolved)
}

#[derive(Clone, Debug)]
struct CssClassRule {
    class: String,
    declarations: Vec<(String, String)>,
}

/// Skia SVG 0.93 does not apply the simple class stylesheet emitted by
/// CorelDRAW/ONLYOFFICE, which otherwise turns a colored illustration into a
/// black silhouette. Materialize a fail-closed CSS subset as SVG presentation
/// attributes, then remove the class/style sheet. Non-class inline styles
/// (notably `clip-path:url(#id0)`) remain byte-for-byte intact.
fn inline_simple_class_styles(data: &[u8]) -> Option<Cow<'_, [u8]>> {
    let rules = collect_simple_class_rules(data)?;
    if rules.is_empty() {
        return (!svg_has_class_attribute(data)?).then_some(Cow::Borrowed(data));
    }

    let mut reader = Reader::from_reader(data);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Vec::with_capacity(data.len().min(MAX_INLINED_SVG_BYTES)));
    let mut skipped_style_depth = 0usize;
    loop {
        let event = reader.read_event().ok()?;
        let event = match event {
            Event::Start(_) if skipped_style_depth > 0 => {
                skipped_style_depth = skipped_style_depth.checked_add(1)?;
                continue;
            }
            Event::Start(start) if start.local_name().as_ref().eq_ignore_ascii_case(b"style") => {
                skipped_style_depth = 1;
                continue;
            }
            Event::Empty(_) if skipped_style_depth > 0 => continue,
            Event::Empty(start) if start.local_name().as_ref().eq_ignore_ascii_case(b"style") => {
                continue;
            }
            Event::End(_) if skipped_style_depth > 0 => {
                skipped_style_depth -= 1;
                continue;
            }
            Event::Start(start) => {
                Event::Start(materialize_class_attributes(start, &reader, &rules)?)
            }
            Event::Empty(start) => {
                Event::Empty(materialize_class_attributes(start, &reader, &rules)?)
            }
            Event::Eof => break,
            other => other.into_owned(),
        };
        writer.write_event(event).ok()?;
        if writer.get_ref().len() > MAX_INLINED_SVG_BYTES {
            return None;
        }
    }
    (skipped_style_depth == 0).then(|| Cow::Owned(writer.into_inner()))
}

fn svg_has_class_attribute(data: &[u8]) -> Option<bool> {
    let mut reader = Reader::from_reader(data);
    loop {
        match reader.read_event().ok()? {
            Event::Start(start) | Event::Empty(start) => {
                for attribute in start.attributes() {
                    let attribute = attribute.ok()?;
                    if attribute.key.local_name().as_ref() == b"class" {
                        return Some(true);
                    }
                }
            }
            Event::Eof => return Some(false),
            _ => {}
        }
    }
}

fn collect_simple_class_rules(data: &[u8]) -> Option<Vec<CssClassRule>> {
    let mut reader = Reader::from_reader(data);
    reader.config_mut().trim_text(false);
    let mut depth = 0usize;
    let mut style_depth = None;
    let mut css = String::new();
    loop {
        match reader.read_event().ok()? {
            Event::Start(start) => {
                depth = depth.checked_add(1)?;
                if start.local_name().as_ref().eq_ignore_ascii_case(b"style") {
                    if style_depth.is_some() {
                        return None;
                    }
                    style_depth = Some(depth);
                }
            }
            Event::Text(text) if style_depth.is_some() => css.push_str(&text.decode().ok()?),
            Event::CData(text) if style_depth.is_some() => css.push_str(&text.decode().ok()?),
            Event::End(_) => {
                if style_depth == Some(depth) {
                    style_depth = None;
                    css.push('\n');
                }
                depth = depth.checked_sub(1)?;
            }
            Event::Eof => break,
            _ => {}
        }
        if css.len() > MAX_CSS_BYTES {
            return None;
        }
    }
    (style_depth.is_none() && depth == 0)
        .then_some(())
        .and_then(|_| parse_simple_class_css(&css))
}

fn parse_simple_class_css(css: &str) -> Option<Vec<CssClassRule>> {
    if css.len() > MAX_CSS_BYTES
        || css.contains("/*")
        || css.contains("*/")
        || css.contains('@')
        || css.contains("!important")
    {
        return None;
    }
    let mut rest = css.trim();
    let mut rules = Vec::new();
    while !rest.is_empty() {
        if rules.len() >= MAX_CSS_CLASS_RULES {
            return None;
        }
        let open = rest.find('{')?;
        let close = rest[open + 1..].find('}')? + open + 1;
        let selector = rest[..open].trim().strip_prefix('.')?;
        if !is_css_ident(selector) {
            return None;
        }
        let declarations = parse_css_declarations(rest[open + 1..close].trim())?;
        rules.push(CssClassRule {
            class: selector.to_owned(),
            declarations,
        });
        rest = rest[close + 1..].trim();
    }
    Some(rules)
}

fn parse_css_declarations(input: &str) -> Option<Vec<(String, String)>> {
    let mut declarations = Vec::new();
    for raw in input.split(';') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        if declarations.len() >= MAX_CSS_DECLARATIONS_PER_RULE {
            return None;
        }
        let (property, value) = raw.split_once(':')?;
        let property = property.trim();
        let value = value.trim();
        if value.contains(':') {
            return None;
        }
        let value = validate_css_value(property, value)?;
        declarations.push((property.to_owned(), value));
    }
    (!declarations.is_empty()).then_some(declarations)
}

fn validate_css_value(property: &str, value: &str) -> Option<String> {
    match property {
        "fill" => normalize_css_color(value, true),
        "stroke" => normalize_css_color(value, false),
        "fill-opacity" => {
            let number = parse_plain_css_number(value)?;
            (number <= 1.0).then(|| value.to_owned())
        }
        "stroke-width" | "stroke-miterlimit" => {
            let number = parse_plain_css_number(value)?;
            (number > 0.0 && number <= 10_000.0).then(|| value.to_owned())
        }
        "stroke-linecap" | "stroke-linejoin" if value == "round" => Some(value.to_owned()),
        _ => None,
    }
}

fn normalize_css_color(value: &str, allow_none: bool) -> Option<String> {
    match value {
        "none" if allow_none => Some("none".to_owned()),
        "white" => Some("#FFFFFF".to_owned()),
        "gray" => Some("#808080".to_owned()),
        _ if value.len() == 7
            && value.starts_with('#')
            && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            Some(value.to_ascii_uppercase())
        }
        _ => None,
    }
}

fn parse_plain_css_number(value: &str) -> Option<f32> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        || value.bytes().filter(|byte| *byte == b'.').count() > 1
    {
        return None;
    }
    value
        .parse::<f32>()
        .ok()
        .filter(|number| number.is_finite() && *number >= 0.0)
}

fn is_css_ident(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn materialize_class_attributes(
    start: BytesStart<'_>,
    reader: &Reader<&[u8]>,
    rules: &[CssClassRule],
) -> Option<BytesStart<'static>> {
    let mut class_value = None;
    let mut has_inline_style = false;
    for attribute in start.attributes() {
        let attribute = attribute.ok()?;
        let local = attribute.key.local_name();
        if local.as_ref().eq_ignore_ascii_case(b"class") {
            class_value = Some(
                attribute
                    .decode_and_unescape_value(reader.decoder())
                    .ok()?
                    .into_owned(),
            );
        } else if local.as_ref().eq_ignore_ascii_case(b"style") {
            has_inline_style = true;
        }
    }
    let Some(classes) = class_value else {
        return Some(start.into_owned());
    };
    if has_inline_style {
        return None;
    }
    let classes = classes.split_ascii_whitespace().collect::<Vec<_>>();
    if classes.is_empty()
        || classes.len() > MAX_CSS_CLASS_TOKENS
        || classes.iter().any(|class| !is_css_ident(class))
        || classes
            .iter()
            .any(|class| !rules.iter().any(|rule| rule.class == *class))
    {
        return None;
    }

    let mut resolved = Vec::<(String, String)>::new();
    for rule in rules {
        if !classes.iter().any(|class| *class == rule.class) {
            continue;
        }
        for (property, value) in &rule.declarations {
            if let Some(existing) = resolved.iter_mut().find(|(name, _)| name == property) {
                existing.1.clone_from(value);
            } else {
                resolved.push((property.clone(), value.clone()));
            }
        }
    }
    if resolved.is_empty() {
        return None;
    }

    let original = start.into_owned();
    for attribute in original.attributes() {
        let attribute = attribute.ok()?;
        if resolved.iter().any(|(property, _)| {
            attribute
                .key
                .local_name()
                .as_ref()
                .eq_ignore_ascii_case(property.as_bytes())
        }) {
            return None;
        }
    }
    let mut rewritten = original.clone();
    rewritten.clear_attributes();
    for attribute in original.attributes() {
        let attribute = attribute.ok()?;
        if !attribute
            .key
            .local_name()
            .as_ref()
            .eq_ignore_ascii_case(b"class")
        {
            rewritten.push_attribute(attribute);
        }
    }
    for (property, value) in &resolved {
        rewritten.push_attribute((property.as_str(), value.as_str()));
    }
    Some(rewritten)
}

fn positive_finite_f64(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

fn normalized_crop(crop: Option<&PtRect>) -> Option<(f64, f64, f64, f64)> {
    let Some(crop) = crop else {
        return Some((0.0, 0.0, 1.0, 1.0));
    };
    let values = [
        crop.origin.x.raw() as f64,
        crop.origin.y.raw() as f64,
        crop.size.width.raw() as f64,
        crop.size.height.raw() as f64,
    ];
    if !values.into_iter().all(f64::is_finite)
        || values[0] < 0.0
        || values[1] < 0.0
        || values[2] <= 0.0
        || values[3] <= 0.0
        || values[0] + values[2] > 1.0 + 1e-6
        || values[1] + values[3] > 1.0 + 1e-6
    {
        return None;
    }
    Some((values[0], values[1], values[2], values[3]))
}

fn bounded_target_pixels(display_rect: PtRect, image_dpi: f32) -> Option<(i32, i32)> {
    let width_pt = display_rect.size.width.raw() as f64;
    let height_pt = display_rect.size.height.raw() as f64;
    let dpi = image_dpi as f64;
    if !positive_finite_f64(width_pt)
        || !positive_finite_f64(height_pt)
        || !positive_finite_f64(dpi)
    {
        return None;
    }
    let desired_w = (width_pt * dpi / POINTS_PER_INCH).ceil();
    let desired_h = (height_pt * dpi / POINTS_PER_INCH).ceil();
    if !positive_finite_f64(desired_w) || !positive_finite_f64(desired_h) {
        return None;
    }
    let area = desired_w * desired_h;
    let mut scale = 1.0f64;
    if area > MAX_SVG_RASTER_PIXELS as f64 {
        scale = scale.min((MAX_SVG_RASTER_PIXELS as f64 / area).sqrt());
    }
    scale = scale.min(MAX_SVG_RASTER_AXIS as f64 / desired_w);
    scale = scale.min(MAX_SVG_RASTER_AXIS as f64 / desired_h);
    if !positive_finite_f64(scale) {
        return None;
    }
    let width = (desired_w * scale).floor().max(1.0) as usize;
    let height = (desired_h * scale).floor().max(1.0) as usize;
    if width > MAX_SVG_RASTER_AXIS
        || height > MAX_SVG_RASTER_AXIS
        || width.checked_mul(height)? > MAX_SVG_RASTER_PIXELS
    {
        return None;
    }
    Some((i32::try_from(width).ok()?, i32::try_from(height).ok()?))
}

fn validate_svg_input(data: &[u8]) -> bool {
    if data.is_empty()
        || data.len() > MAX_SVG_BYTES
        || data.starts_with(&[0x1f, 0x8b])
        || ascii_contains_case_insensitive(data, b"<!entity")
    {
        return false;
    }

    let mut reader = Reader::from_reader(data);
    reader.config_mut().trim_text(false);
    let mut state = SvgValidationState::default();

    loop {
        let Ok(event) = reader.read_event() else {
            return false;
        };
        match event {
            Event::Start(start) => {
                state.depth = match state.depth.checked_add(1) {
                    Some(depth) if depth <= MAX_SVG_DEPTH => depth,
                    _ => return false,
                };
                if !inspect_element(&start, &reader, &mut state, false) {
                    return false;
                }
                if start.local_name().as_ref().eq_ignore_ascii_case(b"style")
                    && state.style_depth.replace(state.depth).is_some()
                {
                    return false;
                }
                state
                    .element_stack
                    .push(start.local_name().as_ref().to_vec());
            }
            Event::Empty(start) => {
                state.depth = match state.depth.checked_add(1) {
                    Some(depth) if depth <= MAX_SVG_DEPTH => depth,
                    _ => return false,
                };
                if !inspect_element(&start, &reader, &mut state, true) {
                    return false;
                }
                state.depth -= 1;
            }
            Event::Text(text)
                if state.style_depth.is_some() && has_disallowed_css_resource(text.as_ref()) =>
            {
                return false;
            }
            Event::CData(text)
                if state.style_depth.is_some() && has_disallowed_css_resource(text.as_ref()) =>
            {
                return false;
            }
            Event::End(end) => {
                let Some(open) = state.element_stack.pop() else {
                    return false;
                };
                if open.as_slice() != end.local_name().as_ref() {
                    return false;
                }
                if state.style_depth == Some(state.depth) {
                    state.style_depth = None;
                }
                let Some(next_depth) = state.depth.checked_sub(1) else {
                    return false;
                };
                state.depth = next_depth;
            }
            Event::DocType(doc_type)
                if doc_type.as_ref().contains(&b'[')
                    || ascii_contains_case_insensitive(doc_type.as_ref(), b"entity") =>
            {
                return false;
            }
            Event::Eof => {
                return state.depth == 0
                    && state.element_stack.is_empty()
                    && state.elements > 0
                    && state.root_seen
                    && state.root_svg_namespace
                    && state
                        .clip_references
                        .iter()
                        .all(|id| state.clip_path_ids.contains(id));
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct SvgValidationState {
    depth: usize,
    elements: usize,
    attributes: usize,
    image_elements: usize,
    data_uri_chars: usize,
    style_depth: Option<usize>,
    element_stack: Vec<Vec<u8>>,
    ids: HashSet<String>,
    clip_path_ids: HashSet<String>,
    clip_references: Vec<String>,
    root_seen: bool,
    root_svg_namespace: bool,
}

fn inspect_element(
    start: &BytesStart<'_>,
    reader: &Reader<&[u8]>,
    state: &mut SvgValidationState,
    empty: bool,
) -> bool {
    state.elements += 1;
    if state.elements > MAX_SVG_ELEMENTS {
        return false;
    }
    let raw_name = start.name();
    if raw_name.as_ref().contains(&b':') {
        return false;
    }
    let local = start.local_name();
    let name = local.as_ref();
    if !is_supported_svg_element(name) {
        return false;
    }
    if state.depth == 1 {
        if name != b"svg" || state.root_seen {
            return false;
        }
        state.root_seen = true;
    } else if name == b"svg" {
        // Nested documents enlarge the viewport/reference surface. The first
        // preferred-source package keeps one audited root only.
        return false;
    }
    let inside_defs = state
        .element_stack
        .iter()
        .any(|element| element.as_slice() == b"defs");
    let inside_clip = state
        .element_stack
        .iter()
        .any(|element| element.as_slice() == b"clipPath");
    if name == b"style" && state.element_stack.last().map(Vec::as_slice) != Some(b"defs".as_slice())
    {
        return false;
    }
    if name == b"clipPath" && (inside_clip || !inside_defs) {
        return false;
    }
    if name == b"image" {
        if inside_defs || inside_clip {
            return false;
        }
        state.image_elements += 1;
        if state.image_elements > MAX_SVG_IMAGE_ELEMENTS {
            return false;
        }
    }

    let mut id = None;
    let mut image_href = None;
    let mut image_width = None;
    let mut image_height = None;
    let mut root_namespace = false;
    for attribute in start.attributes() {
        let Ok(attribute) = attribute else {
            return false;
        };
        state.attributes += 1;
        if state.attributes > MAX_SVG_ATTRIBUTES {
            return false;
        }
        let raw_attr = attribute.key.as_ref();
        if !is_supported_svg_attribute(name, raw_attr) {
            return false;
        }
        let Ok(value) = attribute.decode_and_unescape_value(reader.decoder()) else {
            return false;
        };
        let value = value.trim();
        let attr_local = attribute.key.local_name();
        let attr_name = attr_local.as_ref();
        if raw_attr == b"xmlns" && name == b"svg" {
            root_namespace = value == "http://www.w3.org/2000/svg";
        }
        if attr_name == b"id" {
            // Unreferenced authoring IDs are opaque XML strings and may be
            // non-ASCII (CorelDRAW emits localized layer names). Keep the
            // stricter grammar only for IDs which enter url(#...).
            if value.is_empty() || !state.ids.insert(value.to_owned()) {
                return false;
            }
            id = Some(value.to_owned());
        }
        if attr_name == b"href"
            && (name != b"image" || image_href.replace(value.to_owned()).is_some())
        {
            return false;
        }
        if name == b"image" && attr_name == b"width" {
            image_width = parse_positive_svg_length(value);
        }
        if name == b"image" && attr_name == b"height" {
            image_height = parse_positive_svg_length(value);
        }
        if attr_name == b"clip-path" {
            if inside_clip || name == b"clipPath" {
                return false;
            }
            let Some(reference) = parse_local_url(value) else {
                return false;
            };
            state.clip_references.push(reference.to_owned());
        }
        if attr_name == b"style"
            && (inside_clip || name == b"clipPath" || !validate_inline_style(value, state))
        {
            return false;
        }
        if attr_name == b"overflow" && (name != b"svg" || value != "hidden") {
            return false;
        }
        if attr_name != b"style" && attr_name != b"clip-path" && value.contains("url(") {
            return false;
        }
        if has_disallowed_css_resource(value.as_bytes()) {
            return false;
        }
    }
    if state.depth == 1 {
        state.root_svg_namespace = root_namespace;
    }
    if name == b"clipPath" {
        let Some(id) = id else {
            return false;
        };
        if !is_local_iri_ident(&id) || !state.clip_path_ids.insert(id) {
            return false;
        }
    }
    if name == b"image" {
        let Some(href) = image_href else {
            return false;
        };
        if image_width.is_none() || image_height.is_none() || !validate_data_uri(&href, state) {
            return false;
        }
    }
    if name == b"style" && empty {
        return false;
    }
    true
}

fn is_supported_svg_element(name: &[u8]) -> bool {
    matches!(
        name,
        b"svg"
            | b"g"
            | b"defs"
            | b"metadata"
            | b"style"
            | b"clipPath"
            | b"path"
            | b"polygon"
            | b"polyline"
            | b"rect"
            | b"circle"
            | b"ellipse"
            | b"line"
            | b"image"
    )
}

fn is_supported_svg_attribute(element: &[u8], raw: &[u8]) -> bool {
    if matches!(
        raw,
        b"id"
            | b"class"
            | b"style"
            | b"transform"
            | b"fill"
            | b"fill-rule"
            | b"fill-opacity"
            | b"stroke"
            | b"stroke-width"
            | b"stroke-linecap"
            | b"stroke-linejoin"
            | b"stroke-miterlimit"
            | b"stroke-opacity"
            | b"opacity"
            | b"shape-rendering"
            | b"clip-path"
    ) {
        return true;
    }
    match element {
        b"svg" => matches!(
            raw,
            b"xmlns"
                | b"xmlns:xlink"
                | b"xmlns:xodm"
                | b"width"
                | b"height"
                | b"viewBox"
                | b"version"
                | b"xml:space"
                | b"overflow"
                | b"preserveAspectRatio"
        ),
        b"style" => raw == b"type",
        b"clipPath" => matches!(raw, b"clipPathUnits"),
        b"path" => raw == b"d",
        b"polygon" | b"polyline" => raw == b"points",
        b"rect" => matches!(raw, b"x" | b"y" | b"width" | b"height" | b"rx" | b"ry"),
        b"circle" => matches!(raw, b"cx" | b"cy" | b"r"),
        b"ellipse" => matches!(raw, b"cx" | b"cy" | b"rx" | b"ry"),
        b"line" => matches!(raw, b"x1" | b"y1" | b"x2" | b"y2"),
        b"image" => matches!(
            raw,
            b"x" | b"y" | b"width" | b"height" | b"href" | b"xlink:href" | b"preserveAspectRatio"
        ),
        b"g" | b"defs" | b"metadata" => false,
        _ => false,
    }
}

fn parse_positive_svg_length(value: &str) -> Option<f64> {
    let number = value.strip_suffix("px").unwrap_or(value);
    number
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
}

fn is_local_iri_ident(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn parse_local_url(value: &str) -> Option<&str> {
    let value = value.trim();
    let token = value.strip_prefix("url(")?.strip_suffix(')')?.trim();
    let token = if let Some(token) = token.strip_prefix('\'') {
        token.strip_suffix('\'')?
    } else if let Some(token) = token.strip_prefix('"') {
        token.strip_suffix('"')?
    } else {
        token
    };
    let id = token.strip_prefix('#')?;
    is_local_iri_ident(id).then_some(id)
}

fn validate_inline_style(value: &str, state: &mut SvgValidationState) -> bool {
    let mut declarations = 0usize;
    for declaration in value.split(';') {
        let declaration = declaration.trim();
        if declaration.is_empty() {
            continue;
        }
        declarations += 1;
        if declarations > MAX_CSS_DECLARATIONS_PER_RULE {
            return false;
        }
        let Some((property, value)) = declaration.split_once(':') else {
            return false;
        };
        let property = property.trim();
        let value = value.trim();
        match property {
            "clip-path" => {
                let Some(reference) = parse_local_url(value) else {
                    return false;
                };
                state.clip_references.push(reference.to_owned());
            }
            "shape-rendering" => {
                if !matches!(
                    value,
                    "auto" | "optimizeSpeed" | "crispEdges" | "geometricPrecision"
                ) {
                    return false;
                }
            }
            "text-rendering" => {
                if !matches!(
                    value,
                    "auto" | "optimizeSpeed" | "optimizeLegibility" | "geometricPrecision"
                ) {
                    return false;
                }
            }
            "image-rendering" => {
                if !matches!(
                    value,
                    "auto" | "optimizeSpeed" | "optimizeQuality" | "crisp-edges" | "pixelated"
                ) {
                    return false;
                }
            }
            "fill-rule" | "clip-rule" => {
                if !matches!(value, "nonzero" | "evenodd") {
                    return false;
                }
            }
            _ => return false,
        }
    }
    declarations > 0
}

fn validate_data_uri(value: &str, state: &mut SvgValidationState) -> bool {
    if value.len() > MAX_DATA_URI_CHARS {
        return false;
    }
    let Some((header, payload)) = value.split_once(',') else {
        return false;
    };
    if EmbeddedRasterFormat::from_data_header(header).is_none()
        || payload.is_empty()
        || payload.contains(',')
    {
        return false;
    }
    let Some(total) = state.data_uri_chars.checked_add(value.len()) else {
        return false;
    };
    if total > MAX_DATA_URI_CHARS {
        return false;
    }
    state.data_uri_chars = total;
    true
}

fn has_disallowed_css_resource(value: &[u8]) -> bool {
    let lower = value.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>();
    if lower.windows(b"@import".len()).any(|w| w == b"@import") {
        return true;
    }
    let mut rest = lower.as_slice();
    while let Some(index) = rest.windows(4).position(|window| window == b"url(") {
        let after = &rest[index + 4..];
        let Some(close) = after.iter().position(|byte| *byte == b')') else {
            return true;
        };
        let token = trim_css_url_token(&after[..close]);
        if !token.starts_with(b"#") {
            return true;
        }
        rest = &after[close + 1..];
    }
    false
}

fn trim_css_url_token(mut token: &[u8]) -> &[u8] {
    while token
        .first()
        .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'\'' | b'"'))
    {
        token = &token[1..];
    }
    while token
        .last()
        .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'\'' | b'"'))
    {
        token = &token[..token.len() - 1];
    }
    token
}

fn ascii_contains_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && haystack.windows(needle.len()).any(|window| {
            window
                .iter()
                .zip(needle)
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::dimension::Pt;
    use skia_safe::{image::CachingHint, IPoint};

    fn rect(width: f32, height: f32) -> PtRect {
        PtRect::from_xywh(Pt::ZERO, Pt::ZERO, Pt::new(width), Pt::new(height))
    }

    fn raster(svg: &[u8], frame: PtRect, crop: Option<&PtRect>, dpi: f32) -> Option<Image> {
        rasterize_svg(svg, frame, frame, crop, dpi, FontMgr::new())
    }

    fn alpha_column_bounds(image: &Image) -> Option<(usize, usize)> {
        let width = usize::try_from(image.width()).ok()?;
        let height = usize::try_from(image.height()).ok()?;
        let info = ImageInfo::new(
            (image.width(), image.height()),
            ColorType::RGBA8888,
            AlphaType::Premul,
            None,
        );
        let mut pixels = vec![0u8; width.checked_mul(height)?.checked_mul(4)?];
        if !image.read_pixels(
            &info,
            &mut pixels,
            width * 4,
            IPoint::new(0, 0),
            CachingHint::Disallow,
        ) {
            return None;
        }
        let mut first = None;
        let mut last = None;
        for x in 0..width {
            if (0..height).any(|y| pixels[(y * width + x) * 4 + 3] != 0) {
                first.get_or_insert(x);
                last = Some(x);
            }
        }
        Some((first?, last?))
    }

    fn alpha_row_bounds(image: &Image) -> Option<(usize, usize)> {
        let width = usize::try_from(image.width()).ok()?;
        let height = usize::try_from(image.height()).ok()?;
        let info = ImageInfo::new(
            (image.width(), image.height()),
            ColorType::RGBA8888,
            AlphaType::Premul,
            None,
        );
        let mut pixels = vec![0u8; width.checked_mul(height)?.checked_mul(4)?];
        if !image.read_pixels(
            &info,
            &mut pixels,
            width * 4,
            IPoint::new(0, 0),
            CachingHint::Disallow,
        ) {
            return None;
        }
        let mut first = None;
        let mut last = None;
        for y in 0..height {
            if (0..width).any(|x| pixels[(y * width + x) * 4 + 3] != 0) {
                first.get_or_insert(y);
                last = Some(y);
            }
        }
        Some((first?, last?))
    }

    fn rgba_pixels(image: &Image) -> Vec<u8> {
        let width = usize::try_from(image.width()).expect("positive width");
        let height = usize::try_from(image.height()).expect("positive height");
        let info = ImageInfo::new(
            (image.width(), image.height()),
            ColorType::RGBA8888,
            AlphaType::Premul,
            ColorSpace::new_srgb(),
        );
        let mut pixels = vec![0u8; width * height * 4];
        assert!(image.read_pixels(
            &info,
            &mut pixels,
            width * 4,
            IPoint::new(0, 0),
            CachingHint::Disallow,
        ));
        pixels
    }

    fn base64_encode(data: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let packed = (u32::from(chunk[0]) << 16)
                | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
                | u32::from(chunk.get(2).copied().unwrap_or(0));
            encoded.push(TABLE[((packed >> 18) & 0x3f) as usize] as char);
            encoded.push(TABLE[((packed >> 12) & 0x3f) as usize] as char);
            encoded.push(if chunk.len() > 1 {
                TABLE[((packed >> 6) & 0x3f) as usize] as char
            } else {
                '='
            });
            encoded.push(if chunk.len() > 2 {
                TABLE[(packed & 0x3f) as usize] as char
            } else {
                '='
            });
        }
        encoded
    }

    fn transparent_png(width: i32, height: i32) -> Vec<u8> {
        let info = ImageInfo::new(
            (width, height),
            ColorType::RGBA8888,
            AlphaType::Premul,
            None,
        );
        skia_safe::surfaces::raster(&info, None, None)
            .expect("PNG test surface")
            .image_snapshot()
            .encode(None, skia_safe::EncodedImageFormat::PNG, None)
            .expect("encode PNG fixture")
            .as_bytes()
            .to_vec()
    }

    #[test]
    fn raster_size_follows_display_points_and_dpi() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50">
            <rect width="100" height="50" fill="red"/></svg>"#;
        for (dpi, expected) in [(72.0, (72, 36)), (220.0, (220, 110)), (300.0, (300, 150))] {
            let image = raster(svg, rect(72.0, 36.0), None, dpi).expect("SVG raster");
            assert_eq!((image.width(), image.height()), expected);
        }
    }

    #[test]
    fn relative_root_geometry_is_dpi_independent_and_crop_uses_full_frame_viewport() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" height="100%">
            <rect x="30" y="0" width="10" height="100%" fill="red"/></svg>"#;
        let crop = PtRect::from_xywh(Pt::new(0.25), Pt::ZERO, Pt::new(0.5), Pt::new(1.0));
        let mut normalized = Vec::new();
        for dpi in [72.0, 220.0, 300.0] {
            let image = raster(svg, rect(72.0, 72.0), Some(&crop), dpi).expect("SVG raster");
            let (left, right) = alpha_column_bounds(&image).expect("visible rect");
            normalized.push((
                left as f32 / image.width() as f32,
                (right + 1 - left) as f32 / image.width() as f32,
            ));
        }
        for (left, width) in normalized {
            assert!((left - 0.125).abs() < 0.015, "left={left}");
            assert!((width - (10.0 / 48.0)).abs() < 0.015, "width={width}");
        }
    }

    #[test]
    fn resolved_display_rect_does_not_replace_authored_logical_viewport() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" height="100%">
            <rect x="30" y="0" width="10" height="100%" fill="red"/></svg>"#;
        let image = rasterize_svg(
            svg,
            rect(72.0, 72.0),
            rect(36.0, 72.0),
            None,
            72.0,
            FontMgr::new(),
        )
        .expect("SVG raster");
        let (left_px, right_px) = alpha_column_bounds(&image).expect("visible rect");
        let left = left_px as f32 / image.width() as f32;
        let width = (right_px + 1 - left_px) as f32 / image.width() as f32;
        assert!((left - (30.0 / 96.0)).abs() < 0.04, "left={left}");
        assert!((width - (10.0 / 96.0)).abs() < 0.04, "width={width}");
    }

    #[test]
    fn mixed_relative_and_absolute_root_axes_resolve_independently() {
        let relative_width = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" height="50">
            <rect x="0" y="20" width="100%" height="10" fill="red"/></svg>"#;
        let image = raster(relative_width, rect(72.0, 72.0), None, 96.0).expect("mixed root");
        let (top, bottom) = alpha_row_bounds(&image).expect("visible rect");
        assert!((top as f32 / image.height() as f32 - 0.4).abs() < 0.03);
        assert!(((bottom + 1 - top) as f32 / image.height() as f32 - 0.2).abs() < 0.03);

        let relative_height = br#"<svg xmlns="http://www.w3.org/2000/svg" width="50" height="100%">
            <rect x="20" y="0" width="10" height="100%" fill="red"/></svg>"#;
        let image = raster(relative_height, rect(72.0, 72.0), None, 96.0).expect("mixed root");
        let (left, right) = alpha_column_bounds(&image).expect("visible rect");
        assert!((left as f32 / image.width() as f32 - 0.4).abs() < 0.03);
        assert!(((right + 1 - left) as f32 / image.width() as f32 - 0.2).abs() < 0.03);

        assert!(raster(
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="100%"/>"#,
            rect(72.0, 72.0),
            None,
            96.0,
        )
        .is_none());
    }

    #[test]
    fn ooxml_stretch_fills_frame_even_when_svg_aspect_differs() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100"
            viewBox="0 0 200 100"><rect width="200" height="100" fill="red"/></svg>"#;
        let image = raster(svg, rect(72.0, 72.0), None, 72.0).expect("SVG raster");
        assert_eq!(alpha_column_bounds(&image), Some((0, 71)));
        assert_eq!(alpha_row_bounds(&image), Some((0, 71)));
    }

    #[test]
    fn simple_class_styles_match_equivalent_presentation_attributes() {
        let class_svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10">
            <defs><style type="text/css"><![CDATA[
                .outline {stroke:#222A35;stroke-width:1.75;stroke-linecap:round;stroke-linejoin:round;stroke-miterlimit:22.9256}
                .skin {fill:#FEAE93}
            ]]></style></defs>
            <rect class="skin outline" x="2" y="2" width="16" height="6"/>
        </svg>"##;
        let inline_svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10">
            <rect x="2" y="2" width="16" height="6" fill="#FEAE93"
                stroke="#222A35" stroke-width="1.75" stroke-linecap="round"
                stroke-linejoin="round" stroke-miterlimit="22.9256"/>
        </svg>"##;
        let frame = rect(40.0, 20.0);
        let from_class = raster(class_svg, frame, None, 144.0).expect("class SVG");
        let inline = raster(inline_svg, frame, None, 144.0).expect("inline SVG");
        assert_eq!(rgba_pixels(&from_class), rgba_pixels(&inline));
    }

    #[test]
    fn css_source_order_not_class_token_order_controls_overrides() {
        let first = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
            <defs><style type="text/css"><![CDATA[
                .early {fill:#FF0000}.late {fill:#0000FF}
            ]]></style></defs><rect class="late early" width="10" height="10"/></svg>"##;
        let second = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
            <defs><style type="text/css"><![CDATA[
                .early {fill:#FF0000}.late {fill:#0000FF}
            ]]></style></defs><rect class="early late" width="10" height="10"/></svg>"##;
        let frame = rect(10.0, 10.0);
        let first = raster(first, frame, None, 72.0).expect("first class order");
        let second = raster(second, frame, None, 72.0).expect("second class order");
        assert_eq!(rgba_pixels(&first), rgba_pixels(&second));
        let pixel = &rgba_pixels(&first)[..4];
        assert!(pixel[2] > pixel[0], "later blue rule must win: {pixel:?}");
    }

    #[test]
    fn class_css_is_fail_closed_and_preserves_unrelated_clip_style() {
        let rejected = [
            br##"<svg><defs><style>.a,.b {fill:#FFFFFF}</style></defs><rect class="a"/></svg>"##.as_slice(),
            br##"<svg><defs><style>@import 'x';.a {fill:#FFFFFF}</style></defs><rect class="a"/></svg>"##.as_slice(),
            br##"<svg><defs><style>.a {fill:#FFFFFF!important}</style></defs><rect class="a"/></svg>"##.as_slice(),
            br##"<svg><defs><style>.a {filter:none}</style></defs><rect class="a"/></svg>"##.as_slice(),
            br##"<svg><defs><style>.a {fill:#FFFFFF}</style></defs><rect class="missing"/></svg>"##.as_slice(),
            br##"<svg><defs><style>.a {fill:#FFFFFF}</style></defs><rect class="a" style="clip-path:url(#id0)"/></svg>"##.as_slice(),
            br##"<svg><defs><style>.a {fill:#FFFFFF}</style></defs><rect class="a" fill="#000000"/></svg>"##.as_slice(),
        ];
        for svg in rejected {
            assert!(inline_simple_class_styles(svg).is_none());
        }

        // A class and inline style on the same element is deliberately rejected,
        // but the independent clip-path element from the target stays intact.
        let accepted = br##"<svg><defs><style>.a {fill:#FFFFFF}</style><clipPath id="id0"><path d="M0 0h1v1z"/></clipPath></defs><g style="clip-path:url(#id0)"><rect class="a"/></g></svg>"##;
        let rewritten = inline_simple_class_styles(accepted).expect("simple class rewrite");
        let rewritten = std::str::from_utf8(rewritten.as_ref()).expect("UTF-8 SVG");
        assert!(!rewritten.contains("<style"));
        assert!(!rewritten.contains("class="));
        assert!(rewritten.contains("style=\"clip-path:url(#id0)\""));
        assert!(rewritten.contains("fill=\"#FFFFFF\""));

        assert!(inline_simple_class_styles(br#"<svg><rect class="orphan"/></svg>"#).is_none());
        assert!(validate_svg_input(
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"
                style="shape-rendering:geometricPrecision; text-rendering:geometricPrecision;
                image-rendering:optimizeQuality; fill-rule:evenodd; clip-rule:evenodd">
                <rect width="10" height="10"/></svg>"#
        ));
        assert!(validate_svg_input(
            r#"<svg xmlns="http://www.w3.org/2000/svg"><g id="图层 1"><rect width="1" height="1"/></g></svg>"#.as_bytes()
        ));
        assert!(!validate_svg_input(
            r##"<svg xmlns="http://www.w3.org/2000/svg"><defs><clipPath id="图层 1"><path d="M0 0h1v1z"/></clipPath></defs><g clip-path="url(#图层 1)"/></svg>"##.as_bytes()
        ));
        assert!(validate_svg_input(
            br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1 1" overflow="hidden"><path d="M0 0h1v1z"/></svg>"#
        ));
        assert!(!validate_svg_input(
            br#"<svg xmlns="http://www.w3.org/2000/svg" overflow="scroll"/>"#
        ));
    }

    #[test]
    fn unsupported_nodes_and_ambiguous_local_references_fall_back() {
        let rejected = [
            br##"<svg xmlns="http://www.w3.org/2000/svg"><script/></svg>"##.as_slice(),
            br##"<svg xmlns="http://www.w3.org/2000/svg"><pattern id="p"/></svg>"##.as_slice(),
            br##"<svg xmlns="http://www.w3.org/2000/svg"><image width="1" height="1" href="#p"/></svg>"##.as_slice(),
            br##"<svg xmlns="http://www.w3.org/2000/svg"><g clip-path="url(#missing)"/></svg>"##.as_slice(),
            br##"<svg xmlns="http://www.w3.org/2000/svg"><defs><clipPath id="p"><g clip-path="url(#p)"/></clipPath></defs></svg>"##.as_slice(),
            br##"<svg xmlns="http://www.w3.org/2000/svg"><g id="same"/><g id="same"/></svg>"##.as_slice(),
            br##"<svg xmlns="http://www.w3.org/2000/svg"><evil:path d="M0 0"/></svg>"##.as_slice(),
        ];
        for svg in rejected {
            assert!(
                !validate_svg_input(svg),
                "unexpectedly accepted: {}",
                String::from_utf8_lossy(svg)
            );
        }
    }

    #[test]
    fn gzip_external_resources_and_internal_entities_are_rejected() {
        assert!(!validate_svg_input(&[0x1f, 0x8b, 0x08]));
        assert!(!validate_svg_input(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><image href="https://example.test/a.png"/></svg>"#
        ));
        assert!(!validate_svg_input(
            br#"<!DOCTYPE svg [<!ENTITY x "boom">]><svg xmlns="http://www.w3.org/2000/svg"/>"#
        ));
        assert!(validate_svg_input(
            br#"<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd"><svg xmlns="http://www.w3.org/2000/svg"/>"#
        ));
        assert!(!validate_svg_input(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><text>fallback</text></svg>"#
        ));
        assert!(!validate_svg_input(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><image href="data:image/png,not-base64"/></svg>"#
        ));
    }

    #[test]
    fn structural_limits_reject_deep_use_and_filter_documents() {
        let mut deep = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg">"#);
        for _ in 0..MAX_SVG_DEPTH {
            deep.push_str("<g>");
        }
        for _ in 0..MAX_SVG_DEPTH {
            deep.push_str("</g>");
        }
        deep.push_str("</svg>");
        assert!(!validate_svg_input(deep.as_bytes()));

        assert!(!validate_svg_input(
            br##"<svg xmlns="http://www.w3.org/2000/svg"><use href="#a"/></svg>"##
        ));
        assert!(!validate_svg_input(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><filter/></svg>"#
        ));
    }

    #[test]
    fn embedded_raster_budget_and_closed_resource_policy_fail_whole_svg() {
        let oversized = transparent_png(4_000, 2_001);
        let mut codec = Codec::from_data(Data::new_copy(&oversized)).expect("PNG header codec");
        assert_eq!(
            (codec.dimensions().width, codec.dimensions().height),
            (4_000, 2_001)
        );
        assert_eq!(codec.get_frame_count().max(1), 1);

        let uri = format!("data:image/png;base64,{}", base64_encode(&oversized));
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg"
                    xmlns:xlink="http://www.w3.org/1999/xlink" width="10" height="10">
                <image width="10" height="10" xlink:href="{uri}"/></svg>"#
        );
        assert!(raster(svg.as_bytes(), rect(10.0, 10.0), None, 72.0).is_none());

        let external = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
            <image width="10" height="10" href="file:///tmp/image.png"/></svg>"#;
        assert!(raster(external, rect(10.0, 10.0), None, 72.0).is_none());
    }

    #[test]
    fn oversized_output_is_bounded_without_changing_point_geometry() {
        let (width, height) =
            bounded_target_pixels(rect(100_000.0, 100_000.0), 300.0).expect("bounded target");
        assert!(usize::try_from(width).unwrap() <= MAX_SVG_RASTER_AXIS);
        assert!(usize::try_from(height).unwrap() <= MAX_SVG_RASTER_AXIS);
        assert!(
            usize::try_from(width).unwrap() * usize::try_from(height).unwrap()
                <= MAX_SVG_RASTER_PIXELS
        );
    }
}
