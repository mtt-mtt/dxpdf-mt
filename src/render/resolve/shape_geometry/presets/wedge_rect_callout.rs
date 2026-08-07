//! DrawingML `wedgeRectCallout` preset geometry.
//!
//! The preset is defined by the OOXML `presetShapeDefinitions.xml` guide
//! formulas.  `adj1` and `adj2` are offsets from the shape centre in units of
//! 1/100000 of the width/height; their signs select the side of the callout
//! and their magnitudes select the attachment point on that side.

use crate::model::PresetGeometryDef;
use crate::render::dimension::Pt;
use crate::render::geometry::{PtOffset, PtSize};

use super::super::{PathVerb, ShapePath, SubPath};

const DEFAULT_ADJ1: f32 = -20_833.0;
const DEFAULT_ADJ2: f32 = 62_500.0;

fn adjustment(def: &PresetGeometryDef, name: &str, default: f32) -> f32 {
    def.adjust_values
        .iter()
        .find(|guide| guide.name == name)
        .and_then(|guide| guide.formula.split_whitespace().last())
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(default)
}

fn point(x: f32, y: f32) -> PtOffset {
    PtOffset::new(Pt::new(x), Pt::new(y))
}

/// Build the complete rectangle-plus-wedge outline from the preset guide
/// formulas.  This follows the same point order as LibreOffice's
/// `presetShapeDefinitions.xml`, including the four possible pointer sides.
pub(super) fn build(def: &PresetGeometryDef, extent: PtSize) -> ShapePath {
    let w = extent.width.raw();
    let h = extent.height.raw();
    let hc = w * 0.5;
    let vc = h * 0.5;

    let adj1 = adjustment(def, "adj1", DEFAULT_ADJ1);
    let adj2 = adjustment(def, "adj2", DEFAULT_ADJ2);
    let dx_pos = w * adj1 / 100_000.0;
    let dy_pos = h * adj2 / 100_000.0;
    let x_pos = hc + dx_pos;
    let y_pos = vc + dy_pos;
    let dq = dx_pos * h / w.max(f32::EPSILON);
    let dz = dy_pos.abs() - dq.abs();

    let xg1 = if dx_pos > 0.0 { 7.0 } else { 2.0 };
    let xg2 = if dx_pos > 0.0 { 10.0 } else { 5.0 };
    let yg1 = if dy_pos > 0.0 { 7.0 } else { 2.0 };
    let yg2 = if dy_pos > 0.0 { 10.0 } else { 5.0 };
    let x1 = w * xg1 / 12.0;
    let x2 = w * xg2 / 12.0;
    let y1 = h * yg1 / 12.0;
    let y2 = h * yg2 / 12.0;

    // DrawingML `?:` selects the first branch only for a positive value;
    // negative adjustments place the pointer on the opposite side.
    let t1 = if dx_pos > 0.0 { 0.0 } else { x_pos };
    let xl = if dz > 0.0 { 0.0 } else { t1 };
    let t2 = if dy_pos > 0.0 { x1 } else { x_pos };
    let xt = if dz > 0.0 { t2 } else { x1 };
    let t3 = if dx_pos > 0.0 { x_pos } else { w };
    let xr = if dz > 0.0 { w } else { t3 };
    let t4 = if dy_pos > 0.0 { x_pos } else { x1 };
    let xb = if dz > 0.0 { t4 } else { x1 };
    let t5 = if dx_pos > 0.0 { y1 } else { y_pos };
    let yl = if dz > 0.0 { y1 } else { t5 };
    let t6 = if dy_pos > 0.0 { 0.0 } else { y_pos };
    let yt = if dz > 0.0 { t6 } else { 0.0 };
    let t7 = if dx_pos > 0.0 { y_pos } else { y1 };
    let yr = if dz > 0.0 { y1 } else { t7 };
    let t8 = if dy_pos > 0.0 { y_pos } else { h };
    let yb = if dz > 0.0 { t8 } else { h };

    let verbs = vec![
        PathVerb::MoveTo(point(0.0, 0.0)),
        PathVerb::LineTo(point(x1, 0.0)),
        PathVerb::LineTo(point(xt, yt)),
        PathVerb::LineTo(point(x2, 0.0)),
        PathVerb::LineTo(point(w, 0.0)),
        PathVerb::LineTo(point(w, y1)),
        PathVerb::LineTo(point(xr, yr)),
        PathVerb::LineTo(point(w, y2)),
        PathVerb::LineTo(point(w, h)),
        PathVerb::LineTo(point(x2, h)),
        PathVerb::LineTo(point(xb, yb)),
        PathVerb::LineTo(point(x1, h)),
        PathVerb::LineTo(point(0.0, h)),
        PathVerb::LineTo(point(0.0, y2)),
        PathVerb::LineTo(point(xl, yl)),
        PathVerb::LineTo(point(0.0, y1)),
        PathVerb::Close,
    ];

    ShapePath {
        paths: vec![SubPath {
            verbs,
            fill_mode: crate::model::PathFillMode::Norm,
            stroked: true,
        }],
        text_rect: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GeomGuide, PresetShapeType};

    fn def(a1: i32, a2: i32) -> PresetGeometryDef {
        PresetGeometryDef {
            preset: PresetShapeType::WedgeRectCallout,
            adjust_values: vec![
                GeomGuide {
                    name: "adj1".into(),
                    formula: format!("val {a1}"),
                },
                GeomGuide {
                    name: "adj2".into(),
                    formula: format!("val {a2}"),
                },
            ],
        }
    }

    #[test]
    fn emits_complete_wedge_path() {
        let path = build(
            &def(-70_102, 33_264),
            PtSize::new(Pt::new(100.0), Pt::new(60.0)),
        );
        assert_eq!(path.paths[0].verbs.len(), 17);
        assert!(matches!(path.paths[0].verbs.last(), Some(PathVerb::Close)));
    }

    #[test]
    fn negative_horizontal_adjustment_puts_pointer_on_left_edge() {
        let path = build(
            &def(-70_102, 33_264),
            PtSize::new(Pt::new(100.0), Pt::new(60.0)),
        );
        let PathVerb::LineTo(pointer) = path.paths[0].verbs[14] else {
            panic!("expected the left-edge pointer vertex");
        };
        assert!(pointer.x.raw() < 0.0);
        assert!(pointer.y.raw() > 0.0);
    }

    #[test]
    fn negative_vertical_adjustment_does_not_cut_through_bottom_edge() {
        let height = 60.0;
        let path = build(
            &def(53_551, -75_672),
            PtSize::new(Pt::new(100.0), Pt::new(height)),
        );
        let PathVerb::LineTo(bottom_wedge) = path.paths[0].verbs[10] else {
            panic!("expected the bottom-edge wedge vertex");
        };
        assert_eq!(bottom_wedge.y.raw(), height);
    }
}
