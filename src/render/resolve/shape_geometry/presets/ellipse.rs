//! §20.1.9.18 `ellipse` preset.

use crate::model::PathFillMode;
use crate::render::dimension::Pt;
use crate::render::geometry::{PtOffset, PtRect, PtSize};
use crate::render::resolve::shape_geometry::{PathVerb, ShapePath, SubPath};

pub fn build(extent: PtSize) -> ShapePath {
    const KAPPA: f32 = 0.552_284_8;
    const INSCRIBED_INSET: f32 = 0.146_446_62;
    let rx = extent.width * 0.5;
    let ry = extent.height * 0.5;
    let kx = rx * KAPPA;
    let ky = ry * KAPPA;
    let cx = rx;
    let cy = ry;
    let verbs = vec![
        PathVerb::MoveTo(PtOffset::new(cx, Pt::ZERO)),
        PathVerb::CubicTo(
            PtOffset::new(cx + kx, Pt::ZERO),
            PtOffset::new(extent.width, cy - ky),
            PtOffset::new(extent.width, cy),
        ),
        PathVerb::CubicTo(
            PtOffset::new(extent.width, cy + ky),
            PtOffset::new(cx + kx, extent.height),
            PtOffset::new(cx, extent.height),
        ),
        PathVerb::CubicTo(
            PtOffset::new(cx - kx, extent.height),
            PtOffset::new(Pt::ZERO, cy + ky),
            PtOffset::new(Pt::ZERO, cy),
        ),
        PathVerb::CubicTo(
            PtOffset::new(Pt::ZERO, cy - ky),
            PtOffset::new(cx - kx, Pt::ZERO),
            PtOffset::new(cx, Pt::ZERO),
        ),
        PathVerb::Close,
    ];
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

    #[test]
    fn ellipse_is_four_cubic_segments_and_a_close() {
        let shape = build(PtSize::new(Pt::new(100.0), Pt::new(50.0)));
        assert_eq!(shape.paths[0].verbs.len(), 6);
        assert_eq!(
            shape.paths[0]
                .verbs
                .iter()
                .filter(|verb| matches!(verb, PathVerb::CubicTo(_, _, _)))
                .count(),
            4
        );
        assert!(matches!(shape.paths[0].verbs[5], PathVerb::Close));
    }

    #[test]
    fn text_rectangle_is_inscribed_in_the_ellipse() {
        let rect = build(PtSize::new(Pt::new(100.0), Pt::new(50.0)))
            .text_rect
            .unwrap();
        assert!(rect.origin.x > Pt::ZERO && rect.origin.y > Pt::ZERO);
        assert!(rect.size.width < Pt::new(100.0) && rect.size.height < Pt::new(50.0));
    }
}
