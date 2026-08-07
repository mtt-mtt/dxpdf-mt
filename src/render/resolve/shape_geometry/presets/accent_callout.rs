//! DrawingML `accentCallout1/2/3` preset geometries.
//!
//! These callouts are deliberately represented as three subpaths, matching
//! LibreOffice's `presetShapeDefinitions.xml`: a filled body rectangle, the
//! vertical accent bar, and the leader polyline.  Keeping the subpaths
//! separate is important because the accent variants have no outline around
//! the body path; only the bar and leader are stroked.

use crate::model::{PathFillMode, PresetGeometryDef};
use crate::render::dimension::Pt;
use crate::render::geometry::{PtOffset, PtRect, PtSize};
use crate::render::resolve::shape_geometry::{PathVerb, ShapePath, SubPath};

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

/// Build one of the three accent callout variants from the normative guide
/// defaults. `leader_points` is 2, 3, or 4 for AccentCallout1/2/3.
pub(super) fn build(def: &PresetGeometryDef, extent: PtSize, leader_points: usize) -> ShapePath {
    let (w, h) = (extent.width.raw(), extent.height.raw());
    let defaults = match leader_points {
        2 => [18_750.0, -8_333.0, 112_500.0, -38_333.0, 0.0, 0.0, 0.0, 0.0],
        3 => [
            18_750.0, -8_333.0, 18_750.0, -16_667.0, 112_500.0, -46_667.0, 0.0, 0.0,
        ],
        _ => [
            18_750.0, -8_333.0, 18_750.0, -16_667.0, 100_000.0, -16_667.0, 112_963.0, -8_333.0,
        ],
    };

    let x1 = w * adjustment(def, "adj2", defaults[1]) / 100_000.0;
    let y1 = h * adjustment(def, "adj1", defaults[0]) / 100_000.0;
    let x2 = w * adjustment(def, "adj4", defaults[3]) / 100_000.0;
    let y2 = h * adjustment(def, "adj3", defaults[2]) / 100_000.0;
    let x3 = if leader_points >= 3 {
        w * adjustment(def, "adj6", defaults[5]) / 100_000.0
    } else {
        0.0
    };
    let y3 = if leader_points >= 3 {
        h * adjustment(def, "adj5", defaults[4]) / 100_000.0
    } else {
        0.0
    };
    let x4 = if leader_points >= 4 {
        w * adjustment(def, "adj8", defaults[7]) / 100_000.0
    } else {
        0.0
    };
    let y4 = if leader_points >= 4 {
        h * adjustment(def, "adj7", defaults[6]) / 100_000.0
    } else {
        0.0
    };

    let body = vec![
        PathVerb::MoveTo(point(0.0, 0.0)),
        PathVerb::LineTo(point(w, 0.0)),
        PathVerb::LineTo(point(w, h)),
        PathVerb::LineTo(point(0.0, h)),
        PathVerb::Close,
    ];
    let accent = vec![
        PathVerb::MoveTo(point(x1, 0.0)),
        PathVerb::LineTo(point(x1, h)),
    ];
    let mut leader = vec![
        PathVerb::MoveTo(point(x1, y1)),
        PathVerb::LineTo(point(x2, y2)),
    ];
    if leader_points >= 3 {
        leader.push(PathVerb::LineTo(point(x3, y3)));
    }
    if leader_points >= 4 {
        leader.push(PathVerb::LineTo(point(x4, y4)));
    }

    ShapePath {
        paths: vec![
            SubPath {
                verbs: body,
                fill_mode: PathFillMode::Norm,
                stroked: false,
            },
            SubPath {
                verbs: accent,
                fill_mode: PathFillMode::None,
                stroked: true,
            },
            SubPath {
                verbs: leader,
                fill_mode: PathFillMode::None,
                stroked: true,
            },
        ],
        text_rect: Some(PtRect::from_xywh(
            Pt::ZERO,
            Pt::ZERO,
            extent.width,
            extent.height,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PresetShapeType;

    fn def(preset: PresetShapeType) -> PresetGeometryDef {
        PresetGeometryDef {
            preset,
            adjust_values: vec![],
        }
    }

    #[test]
    fn accent_callout_two_has_body_bar_and_three_point_leader() {
        let path = build(
            &def(PresetShapeType::AccentCallout2),
            PtSize::new(Pt::new(72.0), Pt::new(48.0)),
            3,
        );
        assert_eq!(path.paths.len(), 3);
        assert_eq!(path.paths[0].verbs.len(), 5);
        assert_eq!(path.paths[1].verbs.len(), 2);
        assert_eq!(path.paths[2].verbs.len(), 3);
        assert!(matches!(path.paths[0].fill_mode, PathFillMode::Norm));
        assert!(matches!(path.paths[1].fill_mode, PathFillMode::None));
    }
}
