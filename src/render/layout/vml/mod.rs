//! VML layout support.
//!
//! VML groups use a unitless local coordinate space (`coordorigin` /
//! `coordsize`) and are then fitted into a point-sized viewport. This module
//! owns that conversion so primitive compilers never guess whether a bare
//! number is a point, an EMU, or a group-local coordinate.

use crate::model::{VmlPoint, VmlVector2D};
use crate::render::dimension::Pt;
use crate::render::geometry::{PtOffset, PtSize};
use crate::render::layout::draw_command::{
    DrawCommand, ResolvedDashPattern, ResolvedFill, ResolvedLineCap, ResolvedLineJoin,
    ResolvedStroke,
};
use crate::render::layout::fragment::Fragment;
use crate::render::resolve::drawing_color::Rgba;
use crate::render::resolve::shape_geometry::{PathVerb, SubPath};

/// Axis-aligned affine transform used by nested VML groups.
///
/// The transform maps a child coordinate `(x, y)` to its parent's coordinate
/// system. Parent and child transforms compose without rounding, so rounding
/// happens only when the final point enters the renderer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VmlTransform {
    scale_x: f32,
    scale_y: f32,
    offset_x: f32,
    offset_y: f32,
}

impl VmlTransform {
    /// Build the mapping from a VML coordinate space into `extent` points.
    /// Returns `None` for an empty coordinate space or viewport.
    pub(crate) fn from_coord_space(
        coord_origin: Option<VmlVector2D>,
        coord_size: VmlVector2D,
        extent: PtSize,
    ) -> Option<Self> {
        if coord_size.x <= 0
            || coord_size.y <= 0
            || extent.width.raw() <= 0.0
            || extent.height.raw() <= 0.0
        {
            return None;
        }
        let origin = coord_origin.unwrap_or(VmlVector2D { x: 0, y: 0 });
        let scale_x = extent.width.raw() / coord_size.x as f32;
        let scale_y = extent.height.raw() / coord_size.y as f32;
        Some(Self {
            scale_x,
            scale_y,
            offset_x: -(origin.x as f32) * scale_x,
            offset_y: -(origin.y as f32) * scale_y,
        })
    }

    /// Compose `child` below `self` (`self(child(point))`).
    pub(crate) fn compose(self, child: Self) -> Self {
        Self {
            scale_x: self.scale_x * child.scale_x,
            scale_y: self.scale_y * child.scale_y,
            offset_x: self.offset_x + child.offset_x * self.scale_x,
            offset_y: self.offset_y + child.offset_y * self.scale_y,
        }
    }

    pub(crate) fn map_point(self, point: VmlPoint) -> PtOffset {
        self.map_xy(point.x, point.y)
    }

    pub(crate) fn map_xy(self, x: f32, y: f32) -> PtOffset {
        PtOffset::new(
            crate::render::dimension::Pt::new(x * self.scale_x + self.offset_x),
            crate::render::dimension::Pt::new(y * self.scale_y + self.offset_y),
        )
    }
}

/// Point extent of a top-level VML group. Top-level style measurements use
/// ordinary CSS units; a bare style number is an EMU, matching Word's VML
/// style rules outside a parent group.
pub(crate) fn inline_group_extent(group: &crate::model::VmlGroup) -> Option<PtSize> {
    let width = top_level_style_length(group.common.style.width?)?;
    let height = top_level_style_length(group.common.style.height?)?;
    if width <= Pt::ZERO || height <= Pt::ZERO {
        return None;
    }
    Some(PtSize::new(width, height))
}

/// Compile every pending VML inline group into local draw commands.
pub(crate) fn populate_inline_graphics(
    fragments: &mut [Fragment],
    ctx: &crate::render::layout::build::BuildContext,
    state: &crate::render::layout::build::BuildState,
) {
    for fragment in fragments {
        let Fragment::InlineGraphic {
            size,
            source,
            commands,
        } = fragment
        else {
            continue;
        };
        if commands.is_empty() {
            *commands = compile_group(source, *size, Some((ctx, state)));
        }
    }
}

fn compile_group(
    group: &crate::model::VmlGroup,
    extent: PtSize,
    text_context: Option<(
        &crate::render::layout::build::BuildContext<'_>,
        &crate::render::layout::build::BuildState,
    )>,
) -> Vec<DrawCommand> {
    let Some(coord_size) = group.coord_size else {
        return Vec::new();
    };
    let Some(transform) = VmlTransform::from_coord_space(group.coord_origin, coord_size, extent)
    else {
        return Vec::new();
    };
    let mut commands = Vec::new();
    compile_children(
        &group.children,
        &group.shape_types,
        transform,
        text_context,
        &mut commands,
    );
    commands
}

fn compile_children(
    primitives: &[crate::model::VmlPrimitive],
    shape_types: &[crate::model::VmlShapeType],
    transform: VmlTransform,
    text_context: Option<(
        &crate::render::layout::build::BuildContext<'_>,
        &crate::render::layout::build::BuildState,
    )>,
    commands: &mut Vec<DrawCommand>,
) {
    use crate::model::VmlPrimitive;
    for primitive in primitives {
        match primitive {
            VmlPrimitive::Shape(shape) => {
                let corner_ratio = shape
                    .shape_type_ref
                    .as_ref()
                    .and_then(|id| {
                        shape_types
                            .iter()
                            .find(|shape_type| shape_type.id.as_ref() == Some(id))
                    })
                    .and_then(rounded_corner_ratio);
                compile_box(
                    &shape.common,
                    corner_ratio,
                    transform,
                    text_context,
                    commands,
                );
            }
            VmlPrimitive::Rect(rect) => {
                compile_box(&rect.common, None, transform, text_context, commands)
            }
            VmlPrimitive::RoundRect(rect) => compile_box(
                &rect.common,
                Some(rect.arcsize.unwrap_or(0.2) * 0.5),
                transform,
                text_context,
                commands,
            ),
            VmlPrimitive::Line(line) => {
                if let (Some(from), Some(to)) = (line.from, line.to) {
                    let Some(stroke) = resolve_vml_stroke(&line.common) else {
                        continue;
                    };
                    commands.push(DrawCommand::Line {
                        line: crate::render::geometry::PtLineSegment::new(
                            transform.map_point(from),
                            transform.map_point(to),
                        ),
                        color: crate::render::resolve::color::RgbColor {
                            r: (stroke.color.r * 255.0).round() as u8,
                            g: (stroke.color.g * 255.0).round() as u8,
                            b: (stroke.color.b * 255.0).round() as u8,
                        },
                        width: stroke.width,
                    });
                }
            }
            VmlPrimitive::Group(group) => {
                if let Some(child_transform) = nested_group_transform(group) {
                    compile_children(
                        &group.children,
                        &group.shape_types,
                        transform.compose(child_transform),
                        text_context,
                        commands,
                    );
                }
            }
            VmlPrimitive::Oval(oval) => {
                compile_oval(&oval.common, transform, text_context, commands)
            }
            VmlPrimitive::PolyLine(_)
            | VmlPrimitive::Arc(_)
            | VmlPrimitive::Curve(_)
            | VmlPrimitive::Image(_) => {}
        }
    }
}

fn compile_box(
    common: &crate::model::VmlCommonAttrs,
    corner_ratio: Option<f32>,
    transform: VmlTransform,
    text_context: Option<(
        &crate::render::layout::build::BuildContext<'_>,
        &crate::render::layout::build::BuildState,
    )>,
    commands: &mut Vec<DrawCommand>,
) {
    let Some((left, top, width, height)) = local_style_box(&common.style) else {
        return;
    };
    let origin = transform.map_xy(left, top);
    let far_corner = transform.map_xy(left + width, top + height);
    let extent = PtSize::new(far_corner.x - origin.x, far_corner.y - origin.y);
    if extent.width <= Pt::ZERO || extent.height <= Pt::ZERO {
        return;
    }
    let fill = resolve_vml_solid_fill(common);
    let stroke = resolve_vml_stroke(common);
    commands.push(DrawCommand::Path {
        origin,
        rotation: crate::model::dimension::Dimension::new(0),
        flip_h: false,
        flip_v: false,
        extent,
        paths: vec![SubPath {
            verbs: box_path_verbs(extent, corner_ratio),
            fill_mode: crate::model::PathFillMode::Norm,
            stroked: stroke.is_some(),
        }],
        fill,
        stroke,
        effects: Vec::new(),
    });
    if let (Some(text_box), Some((ctx, state))) = (common.text_box.as_ref(), text_context) {
        let mut text_commands = crate::render::layout::build::floating::build_vml_text_commands(
            text_box, extent, ctx, state,
        );
        for command in &mut text_commands {
            command.shift(origin.x, origin.y);
        }
        commands.extend(text_commands);
    }
}

fn compile_oval(
    common: &crate::model::VmlCommonAttrs,
    transform: VmlTransform,
    text_context: Option<(
        &crate::render::layout::build::BuildContext<'_>,
        &crate::render::layout::build::BuildState,
    )>,
    commands: &mut Vec<DrawCommand>,
) {
    let Some((left, top, width, height)) = local_style_box(&common.style) else {
        return;
    };
    let origin = transform.map_xy(left, top);
    let far_corner = transform.map_xy(left + width, top + height);
    let extent = PtSize::new(far_corner.x - origin.x, far_corner.y - origin.y);
    if extent.width <= Pt::ZERO || extent.height <= Pt::ZERO {
        return;
    }
    let fill = resolve_vml_solid_fill(common);
    let stroke = resolve_vml_stroke(common);
    commands.push(DrawCommand::Path {
        origin,
        rotation: crate::model::dimension::Dimension::new(0),
        flip_h: false,
        flip_v: false,
        extent,
        paths: vec![SubPath {
            verbs: oval_path_verbs(extent),
            fill_mode: crate::model::PathFillMode::Norm,
            stroked: stroke.is_some(),
        }],
        fill,
        stroke,
        effects: Vec::new(),
    });
    if let (Some(text_box), Some((ctx, state))) = (common.text_box.as_ref(), text_context) {
        let mut text_commands = crate::render::layout::build::floating::build_vml_text_commands(
            text_box, extent, ctx, state,
        );
        for command in &mut text_commands {
            command.shift(origin.x, origin.y);
        }
        commands.extend(text_commands);
    }
}

fn rounded_corner_ratio(shape_type: &crate::model::VmlShapeType) -> Option<f32> {
    if shape_type.spt != Some(176.0) {
        return None;
    }
    let coord = shape_type.coord_size?;
    let denominator = coord.x.min(coord.y) as f32;
    let adjustment = shape_type.adj.first().copied().unwrap_or(2_700) as f32;
    (denominator > 0.0).then(|| (adjustment / denominator).clamp(0.0, 0.5))
}

fn box_path_verbs(extent: PtSize, corner_ratio: Option<f32>) -> Vec<PathVerb> {
    let radius = extent.width.min(extent.height) * corner_ratio.unwrap_or(0.0).clamp(0.0, 0.5);
    if radius <= Pt::ZERO {
        return vec![
            PathVerb::MoveTo(PtOffset::new(Pt::ZERO, Pt::ZERO)),
            PathVerb::LineTo(PtOffset::new(extent.width, Pt::ZERO)),
            PathVerb::LineTo(PtOffset::new(extent.width, extent.height)),
            PathVerb::LineTo(PtOffset::new(Pt::ZERO, extent.height)),
            PathVerb::Close,
        ];
    }
    let right = extent.width;
    let bottom = extent.height;
    vec![
        PathVerb::MoveTo(PtOffset::new(radius, Pt::ZERO)),
        PathVerb::LineTo(PtOffset::new(right - radius, Pt::ZERO)),
        PathVerb::QuadTo(PtOffset::new(right, Pt::ZERO), PtOffset::new(right, radius)),
        PathVerb::LineTo(PtOffset::new(right, bottom - radius)),
        PathVerb::QuadTo(
            PtOffset::new(right, bottom),
            PtOffset::new(right - radius, bottom),
        ),
        PathVerb::LineTo(PtOffset::new(radius, bottom)),
        PathVerb::QuadTo(
            PtOffset::new(Pt::ZERO, bottom),
            PtOffset::new(Pt::ZERO, bottom - radius),
        ),
        PathVerb::LineTo(PtOffset::new(Pt::ZERO, radius)),
        PathVerb::QuadTo(
            PtOffset::new(Pt::ZERO, Pt::ZERO),
            PtOffset::new(radius, Pt::ZERO),
        ),
        PathVerb::Close,
    ]
}

pub(crate) fn oval_path_verbs(extent: PtSize) -> Vec<PathVerb> {
    // Four cubic Beziers are the conventional stable approximation of an
    // ellipse. Keeping it in shape-local points also preserves nested-group
    // scaling without introducing an additional transform path.
    const KAPPA: f32 = 0.552_284_8;
    let rx = extent.width * 0.5;
    let ry = extent.height * 0.5;
    let kx = rx * KAPPA;
    let ky = ry * KAPPA;
    let cx = rx;
    let cy = ry;
    vec![
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
    ]
}

fn nested_group_transform(group: &crate::model::VmlGroup) -> Option<VmlTransform> {
    let (left, top, width, height) = local_style_box(&group.common.style)?;
    let coord_size = group.coord_size?;
    let viewport = VmlTransform {
        scale_x: width / coord_size.x as f32,
        scale_y: height / coord_size.y as f32,
        offset_x: left
            - group.coord_origin.unwrap_or(VmlVector2D { x: 0, y: 0 }).x as f32 * width
                / coord_size.x as f32,
        offset_y: top
            - group.coord_origin.unwrap_or(VmlVector2D { x: 0, y: 0 }).y as f32 * height
                / coord_size.y as f32,
    };
    Some(viewport)
}

fn local_style_box(style: &crate::model::VmlStyle) -> Option<(f32, f32, f32, f32)> {
    use crate::model::VmlLengthUnit;
    let local = |length: Option<crate::model::VmlLength>| {
        length.and_then(|value| (value.unit == VmlLengthUnit::None).then_some(value.value as f32))
    };
    let left = local(style.left).unwrap_or(0.0);
    let top = local(style.top).unwrap_or(0.0);
    let width = local(style.width)?;
    let height = local(style.height)?;
    (width > 0.0 && height > 0.0).then_some((left, top, width, height))
}

fn top_level_style_length(length: crate::model::VmlLength) -> Option<Pt> {
    use crate::model::VmlLengthUnit;
    if let Some(points) = length.to_absolute_points() {
        return Some(Pt::new(points));
    }
    (length.unit == VmlLengthUnit::None).then(|| Pt::new(length.value as f32 / 914_400.0 * 72.0))
}

pub(crate) fn resolve_vml_solid_fill(common: &crate::model::VmlCommonAttrs) -> ResolvedFill {
    use crate::model::VmlFillType;
    if common.filled == Some(false) {
        return ResolvedFill::None;
    }
    if let Some(fill) = common.fill.as_ref() {
        match fill.fill_type {
            VmlFillType::Solid => {
                if let Some(mut color) = fill.color.as_ref().and_then(vml_color_to_rgba) {
                    color.a = fill.opacity.unwrap_or(1.0).clamp(0.0, 1.0);
                    return ResolvedFill::Solid(color);
                }
            }
            VmlFillType::Gradient
            | VmlFillType::GradientRadial
            | VmlFillType::Tile
            | VmlFillType::Frame
            | VmlFillType::Pattern => return ResolvedFill::None,
        }
    }
    common
        .fill_color
        .as_ref()
        .and_then(vml_color_to_rgba)
        .map(ResolvedFill::Solid)
        .unwrap_or(ResolvedFill::None)
}

pub(crate) fn resolve_vml_stroke(common: &crate::model::VmlCommonAttrs) -> Option<ResolvedStroke> {
    use crate::model::{VmlDashStyle as D, VmlJoinStyle as J};
    if common.stroked == Some(false) {
        return None;
    }
    let child = common.stroke.as_ref();
    let width = child
        .and_then(|stroke| stroke.weight)
        .or(common.stroke_weight)
        .and_then(top_level_style_length)
        .unwrap_or(Pt::new(0.75));
    if width <= Pt::ZERO {
        return None;
    }
    let color = child
        .and_then(|stroke| stroke.color.as_ref())
        .or(common.stroke_color.as_ref())
        .and_then(vml_color_to_rgba)
        .unwrap_or(Rgba::BLACK);
    let dash = match child.and_then(|stroke| stroke.dash_style) {
        None | Some(D::Solid) => ResolvedDashPattern::Solid,
        Some(D::ShortDot | D::Dot) => ResolvedDashPattern::Dashes(vec![width, width * 2.0]),
        Some(D::ShortDash) => ResolvedDashPattern::Dashes(vec![width * 3.0, width * 2.0]),
        Some(D::Dash | D::LongDash) => ResolvedDashPattern::Dashes(vec![width * 6.0, width * 3.0]),
        Some(_) => ResolvedDashPattern::Dashes(vec![width * 6.0, width * 2.0, width, width * 2.0]),
    };
    let join = match child.and_then(|stroke| stroke.join_style) {
        Some(J::Round) => ResolvedLineJoin::Round,
        Some(J::Bevel) => ResolvedLineJoin::Bevel,
        Some(J::Miter) | None => ResolvedLineJoin::Miter,
    };
    Some(ResolvedStroke {
        width,
        color,
        dash,
        cap: ResolvedLineCap::Butt,
        join,
    })
}

fn vml_color_to_rgba(color: &crate::model::VmlColor) -> Option<Rgba> {
    use crate::model::{VmlColor, VmlNamedColor as N};
    let (r, g, b) = match color {
        VmlColor::Rgb(r, g, b) => (*r, *g, *b),
        VmlColor::Named(name) => match name {
            N::Black | N::WindowText | N::ButtonText | N::CaptionText | N::MenuText => (0, 0, 0),
            N::White | N::Window | N::ButtonHighlight | N::HighlightText => (255, 255, 255),
            N::Silver => (192, 192, 192),
            N::Gray | N::GrayText => (128, 128, 128),
            N::Red => (255, 0, 0),
            N::Blue => (0, 0, 255),
            N::Green => (0, 128, 0),
            N::Lime => (0, 255, 0),
            N::Yellow => (255, 255, 0),
            N::Aqua | N::Cyan => (0, 255, 255),
            N::Fuchsia | N::Magenta => (255, 0, 255),
            N::Maroon => (128, 0, 0),
            N::Purple => (128, 0, 128),
            N::Olive => (128, 128, 0),
            N::Navy => (0, 0, 128),
            N::Teal => (0, 128, 128),
            N::Orange => (255, 165, 0),
            _ => return None,
        },
    };
    Some(Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::dimension::Pt;

    fn assert_near(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 0.001, "{actual} != {expected}");
    }

    #[test]
    fn maps_the_representative_group_coord_space_to_its_point_extent() {
        let transform = VmlTransform::from_coord_space(
            None,
            VmlVector2D {
                x: 13_860,
                y: 7_488,
            },
            PtSize::new(Pt::new(645.0), Pt::new(365.4)),
        )
        .unwrap();
        let origin = transform.map_xy(0.0, 0.0);
        let far_corner = transform.map_xy(13_860.0, 7_488.0);
        assert_near(origin.x.raw(), 0.0);
        assert_near(origin.y.raw(), 0.0);
        assert_near(far_corner.x.raw(), 645.0);
        assert_near(far_corner.y.raw(), 365.4);
    }

    #[test]
    fn honours_non_zero_coord_origin() {
        let transform = VmlTransform::from_coord_space(
            Some(VmlVector2D { x: 100, y: 200 }),
            VmlVector2D { x: 1_000, y: 500 },
            PtSize::new(Pt::new(200.0), Pt::new(100.0)),
        )
        .unwrap();
        let origin = transform.map_point(VmlPoint { x: 100.0, y: 200.0 });
        let far_corner = transform.map_point(VmlPoint {
            x: 1_100.0,
            y: 700.0,
        });
        assert_near(origin.x.raw(), 0.0);
        assert_near(origin.y.raw(), 0.0);
        assert_near(far_corner.x.raw(), 200.0);
        assert_near(far_corner.y.raw(), 100.0);
    }

    #[test]
    fn nested_transforms_compose_without_losing_the_child_offset() {
        let parent = VmlTransform {
            scale_x: 2.0,
            scale_y: 3.0,
            offset_x: 10.0,
            offset_y: 20.0,
        };
        let child = VmlTransform {
            scale_x: 0.5,
            scale_y: 0.25,
            offset_x: 4.0,
            offset_y: 8.0,
        };
        let result = parent.compose(child).map_xy(6.0, 12.0);
        assert_near(result.x.raw(), 24.0);
        assert_near(result.y.raw(), 53.0);
    }

    #[test]
    fn compiles_group_boxes_and_connectors_in_local_points() {
        use crate::model::{
            VmlCommonAttrs, VmlGroup, VmlLength, VmlLengthUnit, VmlLine, VmlPrimitive, VmlRect,
            VmlStyle,
        };
        let local = |value| VmlLength {
            value,
            unit: VmlLengthUnit::None,
        };
        let group = VmlGroup {
            common: VmlCommonAttrs::default(),
            coord_size: Some(VmlVector2D { x: 1_000, y: 500 }),
            coord_origin: None,
            shape_types: vec![],
            children: vec![
                VmlPrimitive::Rect(VmlRect {
                    common: VmlCommonAttrs {
                        style: VmlStyle {
                            left: Some(local(100.0)),
                            top: Some(local(50.0)),
                            width: Some(local(300.0)),
                            height: Some(local(100.0)),
                            ..VmlStyle::default()
                        },
                        ..VmlCommonAttrs::default()
                    },
                }),
                VmlPrimitive::Line(VmlLine {
                    common: VmlCommonAttrs::default(),
                    from: Some(VmlPoint { x: 0.0, y: 0.0 }),
                    to: Some(VmlPoint {
                        x: 1_000.0,
                        y: 500.0,
                    }),
                }),
            ],
        };
        let commands = compile_group(&group, PtSize::new(Pt::new(200.0), Pt::new(100.0)), None);
        assert_eq!(commands.len(), 2);
        let DrawCommand::Path { origin, extent, .. } = &commands[0] else {
            panic!("expected box path");
        };
        assert_near(origin.x.raw(), 20.0);
        assert_near(origin.y.raw(), 10.0);
        assert_near(extent.width.raw(), 60.0);
        assert_near(extent.height.raw(), 20.0);
        let DrawCommand::Line { line, .. } = &commands[1] else {
            panic!("expected connector line");
        };
        assert_near(line.end.x.raw(), 200.0);
        assert_near(line.end.y.raw(), 100.0);
    }

    #[test]
    fn office_spt_176_uses_its_adjustment_as_a_rounded_corner() {
        let verbs = box_path_verbs(
            PtSize::new(Pt::new(200.0), Pt::new(40.0)),
            Some(2_700.0 / 21_600.0),
        );
        assert!(matches!(verbs[0], PathVerb::MoveTo(point) if point.x == Pt::new(5.0)));
        assert!(
            verbs
                .iter()
                .filter(|verb| matches!(verb, PathVerb::QuadTo(_, _)))
                .count()
                == 4
        );
    }

    #[test]
    fn compiles_oval_with_explicit_fill_and_stroke_visuals() {
        use crate::model::{
            VmlColor, VmlCommonAttrs, VmlGroup, VmlLength, VmlLengthUnit, VmlOval, VmlPrimitive,
            VmlStyle,
        };
        let local = |value| VmlLength {
            value,
            unit: VmlLengthUnit::None,
        };
        let group = VmlGroup {
            common: VmlCommonAttrs::default(),
            coord_size: Some(VmlVector2D { x: 100, y: 100 }),
            coord_origin: None,
            shape_types: vec![],
            children: vec![VmlPrimitive::Oval(VmlOval {
                common: VmlCommonAttrs {
                    style: VmlStyle {
                        left: Some(local(10.0)),
                        top: Some(local(20.0)),
                        width: Some(local(60.0)),
                        height: Some(local(40.0)),
                        ..VmlStyle::default()
                    },
                    fill_color: Some(VmlColor::Rgb(255, 0, 0)),
                    stroke_color: Some(VmlColor::Rgb(0, 0, 255)),
                    stroke_weight: Some(VmlLength {
                        value: 2.0,
                        unit: VmlLengthUnit::Pt,
                    }),
                    ..VmlCommonAttrs::default()
                },
            })],
        };
        let commands = compile_group(&group, PtSize::new(Pt::new(100.0), Pt::new(100.0)), None);
        let DrawCommand::Path {
            paths,
            fill,
            stroke,
            ..
        } = &commands[0]
        else {
            panic!("expected oval path");
        };
        assert_eq!(
            paths[0]
                .verbs
                .iter()
                .filter(|verb| matches!(verb, PathVerb::CubicTo(_, _, _)))
                .count(),
            4
        );
        assert!(matches!(fill, ResolvedFill::Solid(color) if color.r == 1.0 && color.g == 0.0));
        let stroke = stroke.as_ref().unwrap();
        assert_eq!(stroke.width, Pt::new(2.0));
        assert_eq!(stroke.color.b, 1.0);
    }
}
