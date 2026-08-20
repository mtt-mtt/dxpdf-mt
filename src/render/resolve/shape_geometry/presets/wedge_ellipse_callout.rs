//! DrawingML `wedgeEllipseCallout` preset geometry.
//!
//! The ellipse parameter-space arc between the two wedge attachments spans
//! 338 degrees. It is emitted as cubic Beziers rather than `PathVerb::ArcTo`
//! because the global arc painter does not yet derive an OOXML ellipse centre
//! from the current point. The callout tip follows the proposed DR-18-0013
//! correction, which clamps a handle inside the ellipse to its boundary.

use std::f64::consts::{FRAC_PI_2, PI, TAU};

use crate::model::{PathFillMode, PresetGeometryDef};
use crate::render::dimension::Pt;
use crate::render::geometry::{PtOffset, PtRect, PtSize};
use crate::render::resolve::shape_geometry::{PathVerb, ShapePath, SubPath};

const DEFAULT_ADJ1: f64 = -20_833.0;
const DEFAULT_ADJ2: f64 = 62_500.0;
const WEDGE_HALF_ANGLE: f64 = 11.0 * PI / 180.0;

fn adjustment(def: &PresetGeometryDef, name: &str, default: f64) -> f64 {
    def.adjust_values
        .iter()
        .find(|guide| guide.name == name)
        .and_then(|guide| {
            let mut tokens = guide.formula.split_whitespace();
            if tokens.next()? != "val" {
                return None;
            }
            let value = tokens.next()?.parse::<f64>().ok()?;
            if tokens.next().is_some() || !value.is_finite() {
                return None;
            }
            Some(value)
        })
        .unwrap_or(default)
}

fn point(x: f64, y: f64) -> PtOffset {
    PtOffset::new(Pt::new(x as f32), Pt::new(y as f32))
}

fn ellipse_point(hc: f64, vc: f64, rx: f64, ry: f64, angle: f64) -> (f64, f64) {
    (hc + rx * angle.cos(), vc + ry * angle.sin())
}

fn append_ellipse_arc(
    verbs: &mut Vec<PathVerb>,
    hc: f64,
    vc: f64,
    rx: f64,
    ry: f64,
    start: f64,
    end: f64,
) {
    let sweep = end - start;
    let segment_count = (sweep / FRAC_PI_2).ceil().max(1.0) as usize;
    let step = sweep / segment_count as f64;

    for segment in 0..segment_count {
        let a = start + step * segment as f64;
        let b = if segment + 1 == segment_count {
            end
        } else {
            start + step * (segment + 1) as f64
        };
        let alpha = 4.0 / 3.0 * ((b - a) * 0.25).tan();
        let (ax, ay) = ellipse_point(hc, vc, rx, ry, a);
        let (bx, by) = ellipse_point(hc, vc, rx, ry, b);
        let dax = -rx * a.sin();
        let day = ry * a.cos();
        let dbx = -rx * b.sin();
        let dby = ry * b.cos();
        verbs.push(PathVerb::CubicTo(
            point(ax + alpha * dax, ay + alpha * day),
            point(bx - alpha * dbx, by - alpha * dby),
            point(bx, by),
        ));
    }
}

pub(super) fn build(def: &PresetGeometryDef, extent: PtSize) -> ShapePath {
    let w = extent.width.raw() as f64;
    let h = extent.height.raw() as f64;
    let hc = w * 0.5;
    let vc = h * 0.5;
    let rx = hc;
    let ry = vc;

    let adj1 = adjustment(def, "adj1", DEFAULT_ADJ1);
    let adj2 = adjustment(def, "adj2", DEFAULT_ADJ2);
    let dx_pos = w * adj1 / 100_000.0;
    let dy_pos = h * adj2 / 100_000.0;
    let x_pos = hc + dx_pos;
    let y_pos = vc + dy_pos;

    // ECMA `at2 sdx sdy` is atan2(sdy, sdx). The aspect correction converts
    // the handle ray into the ellipse's parameter angle.
    let pang = (dy_pos * w).atan2(dx_pos * h);
    let start = pang + WEDGE_HALF_ANGLE;
    let end = pang - WEDGE_HALF_ANGLE + TAU;
    let (x1, y1) = ellipse_point(hc, vc, rx, ry, start);

    // DR-18-0013: an interior handle starts on the ellipse boundary instead
    // of cutting through the filled silhouette.
    let dx_edge = rx * pang.cos();
    let dy_edge = ry * pang.sin();
    let handle_is_inside = dx_edge.hypot(dy_edge) - dx_pos.hypot(dy_pos) > 0.0;
    let (x_tip, y_tip) = if handle_is_inside {
        (hc + dx_edge, vc + dy_edge)
    } else {
        (x_pos, y_pos)
    };

    let mut verbs = vec![
        PathVerb::MoveTo(point(x_tip, y_tip)),
        PathVerb::LineTo(point(x1, y1)),
    ];
    append_ellipse_arc(&mut verbs, hc, vc, rx, ry, start, end);
    verbs.push(PathVerb::Close);

    let inset_fraction = ((1.0 - 0.5_f64.sqrt()) * 0.5) as f32;
    let inset_x = extent.width * inset_fraction;
    let inset_y = extent.height * inset_fraction;
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

    const EPSILON: f32 = 1e-4;

    fn def(adj1: Option<&str>, adj2: Option<&str>) -> PresetGeometryDef {
        let mut adjust_values = Vec::new();
        if let Some(formula) = adj1 {
            adjust_values.push(GeomGuide {
                name: "adj1".into(),
                formula: formula.into(),
            });
        }
        if let Some(formula) = adj2 {
            adjust_values.push(GeomGuide {
                name: "adj2".into(),
                formula: formula.into(),
            });
        }
        PresetGeometryDef {
            preset: PresetShapeType::WedgeEllipseCallout,
            adjust_values,
        }
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() <= EPSILON,
            "expected {expected}, got {actual}"
        );
    }

    fn tip(shape: &ShapePath) -> PtOffset {
        let PathVerb::MoveTo(tip) = shape.paths[0].verbs[0] else {
            panic!("expected the callout tip to start the path");
        };
        tip
    }

    #[test]
    fn target_adjustments_match_onlyoffice_geometry() {
        let extent = PtSize::new(
            Pt::new(1_490_490.0 / 12_700.0),
            Pt::new(1_289_972.0 / 12_700.0),
        );
        let shape = build(&def(Some("val -52865"), Some("val 36681")), extent);

        assert_eq!(shape.paths.len(), 1);
        let subpath = &shape.paths[0];
        assert_eq!(subpath.fill_mode, PathFillMode::Norm);
        assert!(subpath.stroked);
        assert!(matches!(subpath.verbs.last(), Some(PathVerb::Close)));
        assert_eq!(
            subpath
                .verbs
                .iter()
                .filter(|verb| matches!(verb, PathVerb::CubicTo(_, _, _)))
                .count(),
            4
        );
        assert!(!subpath
            .verbs
            .iter()
            .any(|verb| matches!(verb, PathVerb::ArcTo { .. })));

        let actual_tip = tip(&shape);
        assert_close(actual_tip.x.raw(), -3.362_404_6);
        assert_close(actual_tip.y.raw(), 88.044_14);

        let PathVerb::LineTo(first_attachment) = subpath.verbs[1] else {
            panic!("expected the first wedge attachment");
        };
        assert_close(first_attachment.x.raw(), 4.971_769);
        assert_close(first_attachment.y.raw(), 71.244_61);

        let PathVerb::CubicTo(_, _, second_attachment) = subpath.verbs[5] else {
            panic!("expected the final ellipse segment");
        };
        assert_close(second_attachment.x.raw(), 17.737_759);
        assert_close(second_attachment.y.raw(), 87.167_9);

        // The builder emits unflipped geometry. The existing painter mirrors
        // it around the shape centre when the target's `flipH=1` is applied.
        let flipped_tip_x = extent.width.raw() - actual_tip.x.raw();
        assert_close(flipped_tip_x / extent.width.raw(), 1.028_65);
        assert_close(actual_tip.y.raw() / extent.height.raw(), 0.866_81);
    }

    #[test]
    fn inside_handle_is_clamped_to_ellipse_edge() {
        let extent = PtSize::new(Pt::new(100.0), Pt::new(60.0));
        let shape = build(&def(Some("val 0"), Some("val 0")), extent);
        let actual_tip = tip(&shape);
        assert_close(actual_tip.x.raw(), 100.0);
        assert_close(actual_tip.y.raw(), 30.0);

        let ellipse_value = ((actual_tip.x.raw() - 50.0) / 50.0).powi(2)
            + ((actual_tip.y.raw() - 30.0) / 30.0).powi(2);
        assert_close(ellipse_value, 1.0);
    }

    #[test]
    fn defaults_are_used_for_missing_malformed_and_non_finite_adjustments() {
        let extent = PtSize::new(Pt::new(100.0), Pt::new(60.0));
        let expected = tip(&build(&def(None, None), extent));
        let malformed = [
            def(Some("val not-a-number"), None),
            def(None, Some("val not-a-number")),
            def(Some("*/ 1 2 -20833"), None),
            def(Some("val -20833 trailing"), None),
            def(Some("val NaN"), None),
            def(None, Some("val inf")),
        ];

        for definition in malformed {
            assert_eq!(tip(&build(&definition, extent)), expected);
        }
    }

    #[test]
    fn text_rect_is_the_inscribed_ellipse_rectangle() {
        let extent = PtSize::new(Pt::new(100.0), Pt::new(80.0));
        let rect = build(&def(None, None), extent).text_rect.unwrap();
        let left = rect.origin.x.raw() / extent.width.raw();
        let top = rect.origin.y.raw() / extent.height.raw();
        let right = (rect.origin.x + rect.size.width).raw() / extent.width.raw();
        let bottom = (rect.origin.y + rect.size.height).raw() / extent.height.raw();
        assert_close(left, 0.146_446_62);
        assert_close(top, 0.146_446_62);
        assert_close(right, 0.853_553_4);
        assert_close(bottom, 0.853_553_4);
    }
}
