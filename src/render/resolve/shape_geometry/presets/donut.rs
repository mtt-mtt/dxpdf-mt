//! DrawingML `donut` preset geometry.
//!
//! `adj` is the ring thickness in 1/100000 of the shortest side, clamped to
//! 0..50000 by the preset definition. The inner ellipse is emitted with the
//! opposite winding direction so Skia's winding fill leaves a transparent
//! hole while both contours retain the shape outline.

use crate::model::{PathFillMode, PresetGeometryDef};
use crate::render::dimension::Pt;
use crate::render::geometry::{PtOffset, PtRect, PtSize};
use crate::render::resolve::shape_geometry::{PathVerb, ShapePath, SubPath};

const KAPPA: f32 = 0.552_284_8;
const DEFAULT_ADJ: f32 = 25_000.0;
const INSCRIBED_INSET: f32 = 0.146_446_62;

fn adjustment(def: &PresetGeometryDef) -> f32 {
    def.adjust_values
        .iter()
        .find(|guide| guide.name == "adj")
        .and_then(|guide| guide.formula.split_whitespace().last())
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(DEFAULT_ADJ)
        .clamp(0.0, 50_000.0)
}

fn point(x: f32, y: f32) -> PtOffset {
    PtOffset::new(Pt::new(x), Pt::new(y))
}

pub(super) fn build(def: &PresetGeometryDef, extent: PtSize) -> ShapePath {
    let w = extent.width.raw();
    let h = extent.height.raw();
    let cx = w * 0.5;
    let cy = h * 0.5;
    let outer_rx = cx;
    let outer_ry = cy;
    let thickness = w.min(h) * adjustment(def) / 100_000.0;
    let inner_rx = (outer_rx - thickness).max(0.0);
    let inner_ry = (outer_ry - thickness).max(0.0);

    // Outer contour: clockwise in the renderer's y-down coordinate system.
    let mut verbs = vec![
        PathVerb::MoveTo(point(cx, 0.0)),
        PathVerb::CubicTo(
            point(cx + outer_rx * KAPPA, 0.0),
            point(w, cy - outer_ry * KAPPA),
            point(w, cy),
        ),
        PathVerb::CubicTo(
            point(w, cy + outer_ry * KAPPA),
            point(cx + outer_rx * KAPPA, h),
            point(cx, h),
        ),
        PathVerb::CubicTo(
            point(cx - outer_rx * KAPPA, h),
            point(0.0, cy + outer_ry * KAPPA),
            point(0.0, cy),
        ),
        PathVerb::CubicTo(
            point(0.0, cy - outer_ry * KAPPA),
            point(cx - outer_rx * KAPPA, 0.0),
            point(cx, 0.0),
        ),
        PathVerb::Close,
    ];

    // At maximum thickness the inner ellipse degenerates. Omitting it is the
    // stable equivalent of the preset's zero-radius arc sequence.
    if inner_rx > f32::EPSILON && inner_ry > f32::EPSILON {
        let left = cx - inner_rx;
        let right = cx + inner_rx;
        let top = cy - inner_ry;
        let bottom = cy + inner_ry;
        let kx = inner_rx * KAPPA;
        let ky = inner_ry * KAPPA;

        // Inner contour: counter-clockwise, cancelling the winding interior.
        verbs.extend([
            PathVerb::MoveTo(point(cx, top)),
            PathVerb::CubicTo(point(cx - kx, top), point(left, cy - ky), point(left, cy)),
            PathVerb::CubicTo(
                point(left, cy + ky),
                point(cx - kx, bottom),
                point(cx, bottom),
            ),
            PathVerb::CubicTo(
                point(cx + kx, bottom),
                point(right, cy + ky),
                point(right, cy),
            ),
            PathVerb::CubicTo(point(right, cy - ky), point(cx + kx, top), point(cx, top)),
            PathVerb::Close,
        ]);
    }

    let inset_x = extent.width * INSCRIBED_INSET;
    let inset_y = extent.height * INSCRIBED_INSET;
    ShapePath {
        paths: vec![SubPath {
            verbs,
            fill_mode: PathFillMode::Norm,
            stroked: true,
        }],
        text_rect: Some(PtRect::from_xywh(
            inset_x,
            inset_y,
            extent.width - inset_x * 2.0,
            extent.height - inset_y * 2.0,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GeomGuide, PresetShapeType};

    fn def(adj: Option<i32>) -> PresetGeometryDef {
        PresetGeometryDef {
            preset: PresetShapeType::Donut,
            adjust_values: adj
                .map(|value| {
                    vec![GeomGuide {
                        name: "adj".into(),
                        formula: format!("val {value}"),
                    }]
                })
                .unwrap_or_default(),
        }
    }

    #[test]
    fn default_adjustment_emits_opposite_inner_contour() {
        let shape = build(&def(None), PtSize::new(Pt::new(100.0), Pt::new(80.0)));
        let verbs = &shape.paths[0].verbs;
        assert_eq!(verbs.len(), 12);
        assert!(matches!(verbs[0], PathVerb::MoveTo(p) if p == point(50.0, 0.0)));
        // dr=20, so the inner radii are 30x20 and its top is y=20.
        assert!(matches!(verbs[6], PathVerb::MoveTo(p) if p == point(50.0, 20.0)));
        assert!(matches!(verbs[7], PathVerb::CubicTo(_, _, p) if p == point(20.0, 40.0)));
        assert!(matches!(verbs[9], PathVerb::CubicTo(_, _, p) if p == point(80.0, 40.0)));
    }

    #[test]
    fn adjustment_is_clamped_and_a_closed_hole_becomes_solid() {
        let shape = build(
            &def(Some(80_000)),
            PtSize::new(Pt::new(100.0), Pt::new(80.0)),
        );
        assert_eq!(shape.paths[0].verbs.len(), 6);
    }

    #[test]
    fn malformed_adjustment_uses_the_default() {
        let mut definition = def(None);
        definition.adjust_values.push(GeomGuide {
            name: "adj".into(),
            formula: "val not-a-number".into(),
        });
        let shape = build(&definition, PtSize::new(Pt::new(100.0), Pt::new(80.0)));
        assert_eq!(shape.paths[0].verbs.len(), 12);
    }
}
