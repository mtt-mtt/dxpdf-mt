//! Preset shape generators (§20.1.9.18 ST_ShapeType).
//!
//! Each generator is a pure function `PtSize → ShapePath`. Dispatch by
//! variant lives in [`build_preset`]. Unimplemented presets return `None`
//! and log once; callers should fall back to the shape's bounding box or
//! skip the shape.
//!
//! Tier 0 supports only `line` and `rect` — the minimum to validate the
//! pipeline end-to-end. Tier 1 adds the common ~20 shapes; Tier 2 adds the
//! remaining ~60; Tier 3 completes the spec's ~200.

mod accent_callout;
mod donut;
mod ellipse;
mod line;
mod rect;
mod wedge_ellipse_callout;
mod wedge_rect_callout;

use crate::model::{PresetGeometryDef, PresetShapeType};
use crate::render::geometry::PtSize;

use super::ShapePath;

/// Dispatch a preset to its generator. Returns `None` for presets not yet
/// implemented; the call site is expected to log.
pub fn build_preset(def: &PresetGeometryDef, extent: PtSize) -> Option<ShapePath> {
    match def.preset {
        PresetShapeType::Line => Some(line::build(extent)),
        PresetShapeType::Rect => Some(rect::build(extent)),
        PresetShapeType::Ellipse => Some(ellipse::build(extent)),
        PresetShapeType::Donut => Some(donut::build(def, extent)),
        PresetShapeType::AccentCallout1 => Some(accent_callout::build(def, extent, 2)),
        PresetShapeType::AccentCallout2 => Some(accent_callout::build(def, extent, 3)),
        PresetShapeType::AccentCallout3 => Some(accent_callout::build(def, extent, 4)),
        PresetShapeType::WedgeEllipseCallout => Some(wedge_ellipse_callout::build(def, extent)),
        PresetShapeType::WedgeRectCallout => Some(wedge_rect_callout::build(def, extent)),
        _ => {
            log::warn!(
                "shape_geometry: preset {:?} not yet implemented",
                def.preset
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::dimension::Pt;

    fn def(preset: PresetShapeType) -> PresetGeometryDef {
        PresetGeometryDef {
            preset,
            adjust_values: vec![],
        }
    }

    #[test]
    fn line_dispatches() {
        let p = build_preset(
            &def(PresetShapeType::Line),
            PtSize::new(Pt::new(10.0), Pt::new(20.0)),
        );
        assert!(p.is_some());
    }

    #[test]
    fn rect_dispatches() {
        let p = build_preset(
            &def(PresetShapeType::Rect),
            PtSize::new(Pt::new(10.0), Pt::new(20.0)),
        );
        assert!(p.is_some());
    }

    #[test]
    fn ellipse_dispatches() {
        let p = build_preset(
            &def(PresetShapeType::Ellipse),
            PtSize::new(Pt::new(10.0), Pt::new(20.0)),
        );
        assert!(p.is_some());
    }

    #[test]
    fn donut_dispatches() {
        let p = build_preset(
            &def(PresetShapeType::Donut),
            PtSize::new(Pt::new(20.0), Pt::new(20.0)),
        );
        assert!(p.is_some());
    }

    #[test]
    fn wedge_ellipse_callout_dispatches() {
        let p = build_preset(
            &def(PresetShapeType::WedgeEllipseCallout),
            PtSize::new(Pt::new(20.0), Pt::new(20.0)),
        );
        assert!(p.is_some());
    }

    #[test]
    fn unknown_preset_returns_none() {
        let p = build_preset(
            &def(PresetShapeType::Star12),
            PtSize::new(Pt::new(10.0), Pt::new(20.0)),
        );
        assert!(p.is_none());
    }
}
