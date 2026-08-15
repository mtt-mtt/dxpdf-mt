//! Chart-local vector command generation.
//!
//! Commands are flattened into the ordinary `DrawCommand` vocabulary so the
//! existing font subsetting, PDF painting, coordinate shifting, and
//! behind-document ordering continue to apply without a chart-specific
//! painter branch.

use std::f32::consts::PI;
use std::rc::Rc;

use crate::model::{
    Chart, ChartLayout, ChartText, ChartTextAlignment, ChartTextStyle, PathFillMode, PieChart,
    Theme,
};
use crate::render::dimension::Pt;
use crate::render::geometry::{PtLineSegment, PtOffset, PtRect, PtSize};
use crate::render::layout::draw_command::{
    DrawCommand, ResolvedDashPattern, ResolvedFill, ResolvedLineCap, ResolvedLineJoin,
    ResolvedStroke,
};
use crate::render::layout::fragment::FontProps;
use crate::render::resolve::color::{rgb_from_u32, RgbColor};
use crate::render::resolve::drawing_color::{resolve_drawing_color, DrawingColorContext, Rgba};
use crate::render::resolve::shape_geometry::{PathVerb, SubPath};
use crate::render::resolve::shape_visuals::resolve_shape_visuals;

use super::BuildContext;

pub(super) fn build_chart_commands(
    chart: &Chart,
    extent: PtSize,
    ctx: &BuildContext<'_>,
) -> Vec<DrawCommand> {
    match chart {
        Chart::Pie(pie) => build_pie_commands(pie, extent, ctx),
    }
}

fn build_pie_commands(
    chart: &PieChart,
    extent: PtSize,
    ctx: &BuildContext<'_>,
) -> Vec<DrawCommand> {
    if extent.width <= Pt::ZERO
        || extent.height <= Pt::ZERO
        || !chart.hole_size_percent.is_finite()
        || !chart.first_slice_angle_degrees.is_finite()
    {
        return Vec::new();
    }
    let Some(legend_command_reserve) = fixed_command_budget(chart) else {
        log::warn!(
            "pie/doughnut chart exceeds the {} point / {} command safety budget",
            PieChart::MAX_POINTS,
            PieChart::MAX_DRAW_COMMANDS
        );
        return Vec::new();
    };
    let total = chart.points.iter().map(|point| point.value).sum::<f64>();
    if total <= 0.0 || !total.is_finite() {
        return Vec::new();
    }

    let has_legend = chart.legend.is_some();
    let plot_default = if has_legend {
        NormalizedRect::new(0.05, 0.22, 0.52, 0.70)
    } else {
        NormalizedRect::new(0.15, 0.22, 0.70, 0.70)
    };
    let plot = normalized_rect(chart.plot_layout.as_ref(), extent, plot_default);
    let diameter = plot.size.width.min(plot.size.height);
    if diameter <= Pt::ZERO {
        return Vec::new();
    }
    let center = PtOffset::new(
        plot.origin.x + plot.size.width * 0.5,
        plot.origin.y + plot.size.height * 0.5,
    );
    let radius = diameter * 0.5;
    let inner_radius = radius * (chart.hole_size_percent.clamp(0.0, 90.0) / 100.0);

    let mut commands = Vec::new();
    let mut point_colors = Vec::with_capacity(chart.points.len());
    let mut start_degrees = -90.0 + chart.first_slice_angle_degrees;
    for (ordinal, point) in chart.points.iter().enumerate() {
        let sweep_degrees = (point.value / total * 360.0) as f32;
        let visuals = resolve_shape_visuals(
            point.shape_properties.as_ref(),
            None,
            None,
            None,
            ctx.resolved.theme.as_ref(),
        );
        let color = match visuals.fill {
            ResolvedFill::Solid(color) => color,
            _ => fallback_point_color(ordinal, ctx.resolved.theme.as_ref()),
        };
        let stroke = visuals.stroke.or_else(|| Some(default_slice_stroke()));
        point_colors.push(color);

        if sweep_degrees > f32::EPSILON {
            commands.push(DrawCommand::Path {
                origin: PtOffset::new(Pt::ZERO, Pt::ZERO),
                rotation: crate::model::dimension::Dimension::new(0),
                flip_h: false,
                flip_v: false,
                extent,
                paths: vec![pie_slice_path(
                    center,
                    radius,
                    inner_radius,
                    start_degrees,
                    sweep_degrees,
                )],
                fill: ResolvedFill::Solid(color),
                stroke,
                effects: Vec::new(),
            });
        }
        start_degrees += sweep_degrees;
    }

    // Data labels are authored after the slice geometry and therefore paint
    // above the white separators. Explicit rich-text labels already won over
    // computed values in the parser.
    let mut start_degrees = -90.0 + chart.first_slice_angle_degrees;
    for point in &chart.points {
        let sweep_degrees = (point.value / total * 360.0) as f32;
        if let Some(label) = point.label.as_ref() {
            let angle = (start_degrees + sweep_degrees * 0.5).to_radians();
            let label_radius = inner_radius + (radius - inner_radius) * 0.60;
            let label_center = PtOffset::new(
                center.x + label_radius * angle.cos(),
                center.y + label_radius * angle.sin(),
            );
            push_centered_text(
                &mut commands,
                label,
                label_center,
                Pt::new(10.0),
                RgbColor::WHITE,
                ctx,
            );
        }
        start_degrees += sweep_degrees;
    }

    if let Some(title) = chart.title.as_ref() {
        let layout = chart.title_layout.as_ref();
        let x = layout.and_then(|layout| layout.x).unwrap_or(0.05);
        let y = layout.and_then(|layout| layout.y).unwrap_or(0.03);
        let fallback = NormalizedRect::new(x, y, (0.98 - x).max(0.0), 0.18);
        let rect = normalized_rect(layout, extent, fallback);
        let title_line_budget = PieChart::MAX_DRAW_COMMANDS
            .saturating_sub(commands.len())
            .saturating_sub(legend_command_reserve);
        push_wrapped_text(
            &mut commands,
            title,
            rect,
            Pt::new(10.0),
            RgbColor::BLACK,
            ctx,
            title_line_budget,
        );
    }

    if let Some(legend) = chart.legend.as_ref() {
        let rect = normalized_rect(
            legend.layout.as_ref(),
            extent,
            NormalizedRect::new(0.60, 0.35, 0.36, 0.45),
        );
        let visible: Vec<_> = chart
            .points
            .iter()
            .enumerate()
            .filter(|(_, point)| {
                !legend
                    .entries
                    .iter()
                    .any(|entry| entry.index == point.index && entry.deleted)
                    && point
                        .category
                        .as_deref()
                        .is_some_and(|text| !text.is_empty())
            })
            .collect();
        if !visible.is_empty() && rect.size.height > Pt::ZERO {
            let row_height = rect.size.height / visible.len() as f32;
            let marker = Pt::new(5.0).min(row_height * 0.55);
            let marker_x = rect.origin.x + Pt::new(6.0);
            for (row, (ordinal, point)) in visible.into_iter().enumerate() {
                let row_center_y = rect.origin.y + row_height * (row as f32 + 0.5);
                let marker_rect =
                    PtRect::from_xywh(marker_x, row_center_y - marker * 0.5, marker, marker);
                let color = point_colors.get(ordinal).copied().unwrap_or(Rgba::BLACK);
                commands.push(DrawCommand::Rect {
                    rect: marker_rect,
                    color: rgb_from_u32(color.to_rgb24()),
                });
                push_rect_outline(&mut commands, marker_rect, RgbColor::WHITE, Pt::new(0.75));

                let mut style = legend.style.clone();
                if let Some(entry) = legend
                    .entries
                    .iter()
                    .find(|entry| entry.index == point.index)
                {
                    style.merge_from(entry.style.clone());
                }
                let text = ChartText {
                    text: point.category.clone().unwrap_or_default(),
                    style,
                    alignment: ChartTextAlignment::Left,
                };
                let text_rect = PtRect::from_xywh(
                    marker_x + marker + Pt::new(3.0),
                    rect.origin.y + row_height * row as f32,
                    (rect.origin.x + rect.size.width) - (marker_x + marker + Pt::new(3.0)),
                    row_height,
                );
                push_single_line_text(
                    &mut commands,
                    &text,
                    text_rect,
                    Pt::new(8.0),
                    RgbColor::BLACK,
                    ctx,
                );
            }
        }
    }

    debug_assert!(commands.len() <= PieChart::MAX_DRAW_COMMANDS);
    commands
}

/// Validate the model-level point count and reserve the worst-case legend
/// expansion (marker rect, four outline lines, and text per point).  Parser
/// limits are repeated here because callers can construct the public model.
fn fixed_command_budget(chart: &PieChart) -> Option<usize> {
    if chart.points.len() > PieChart::MAX_POINTS
        || chart
            .legend
            .as_ref()
            .is_some_and(|legend| legend.entries.len() > PieChart::MAX_POINTS)
    {
        return None;
    }
    let slices_and_labels = chart.points.len().checked_add(
        chart
            .points
            .iter()
            .filter(|point| point.label.is_some())
            .count(),
    )?;
    let legend_commands = if chart.legend.is_some() {
        chart.points.len().checked_mul(6)?
    } else {
        0
    };
    slices_and_labels
        .checked_add(legend_commands)
        .filter(|count| *count <= PieChart::MAX_DRAW_COMMANDS)?;
    Some(legend_commands)
}

fn pie_slice_path(
    center: PtOffset,
    outer_radius: Pt,
    inner_radius: Pt,
    start_degrees: f32,
    sweep_degrees: f32,
) -> SubPath {
    let mut verbs = Vec::new();
    let outer_start = ellipse_point(center, outer_radius, start_degrees);
    if inner_radius <= Pt::new(0.01) {
        verbs.push(PathVerb::MoveTo(center));
        verbs.push(PathVerb::LineTo(outer_start));
        append_circular_arc(
            &mut verbs,
            center,
            outer_radius,
            start_degrees,
            sweep_degrees,
        );
    } else {
        verbs.push(PathVerb::MoveTo(outer_start));
        append_circular_arc(
            &mut verbs,
            center,
            outer_radius,
            start_degrees,
            sweep_degrees,
        );
        let end_degrees = start_degrees + sweep_degrees;
        verbs.push(PathVerb::LineTo(ellipse_point(
            center,
            inner_radius,
            end_degrees,
        )));
        append_circular_arc(
            &mut verbs,
            center,
            inner_radius,
            end_degrees,
            -sweep_degrees,
        );
    }
    verbs.push(PathVerb::Close);
    SubPath {
        verbs,
        fill_mode: PathFillMode::Norm,
        stroked: true,
    }
}

fn append_circular_arc(
    verbs: &mut Vec<PathVerb>,
    center: PtOffset,
    radius: Pt,
    start_degrees: f32,
    sweep_degrees: f32,
) {
    let segments = (sweep_degrees.abs() / 90.0).ceil().max(1.0) as usize;
    let delta = sweep_degrees / segments as f32;
    let radius = radius.raw();
    let cx = center.x.raw();
    let cy = center.y.raw();
    for segment in 0..segments {
        let a0 = (start_degrees + delta * segment as f32) * PI / 180.0;
        let a1 = (start_degrees + delta * (segment + 1) as f32) * PI / 180.0;
        let k = 4.0 / 3.0 * ((a1 - a0) * 0.25).tan();
        let p0 = (cx + radius * a0.cos(), cy + radius * a0.sin());
        let p1 = (cx + radius * a1.cos(), cy + radius * a1.sin());
        let c1 = (p0.0 - k * radius * a0.sin(), p0.1 + k * radius * a0.cos());
        let c2 = (p1.0 + k * radius * a1.sin(), p1.1 - k * radius * a1.cos());
        verbs.push(PathVerb::CubicTo(
            pt_offset(c1.0, c1.1),
            pt_offset(c2.0, c2.1),
            pt_offset(p1.0, p1.1),
        ));
    }
}

fn ellipse_point(center: PtOffset, radius: Pt, degrees: f32) -> PtOffset {
    let angle = degrees.to_radians();
    PtOffset::new(
        center.x + radius * angle.cos(),
        center.y + radius * angle.sin(),
    )
}

fn pt_offset(x: f32, y: f32) -> PtOffset {
    PtOffset::new(Pt::new(x), Pt::new(y))
}

fn fallback_point_color(index: usize, theme: Option<&Theme>) -> Rgba {
    let rgb = theme.map_or_else(
        || [0x5B9BD5, 0xED7D31, 0xA5A5A5, 0xFFC000, 0x4472C4, 0x70AD47][index % 6],
        |theme| {
            let scheme = &theme.color_scheme;
            [
                scheme.accent1,
                scheme.accent2,
                scheme.accent3,
                scheme.accent4,
                scheme.accent5,
                scheme.accent6,
            ][index % 6]
        },
    );
    Rgba::from_rgb24(rgb)
}

fn default_slice_stroke() -> ResolvedStroke {
    ResolvedStroke {
        width: Pt::new(0.75),
        color: Rgba::WHITE,
        dash: ResolvedDashPattern::Solid,
        cap: ResolvedLineCap::Butt,
        join: ResolvedLineJoin::Round,
    }
}

#[derive(Clone, Copy)]
struct NormalizedRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl NormalizedRect {
    const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

fn normalized_rect(
    layout: Option<&ChartLayout>,
    extent: PtSize,
    fallback: NormalizedRect,
) -> PtRect {
    let x = finite_or(layout.and_then(|layout| layout.x), fallback.x).clamp(0.0, 1.0);
    let y = finite_or(layout.and_then(|layout| layout.y), fallback.y).clamp(0.0, 1.0);
    let width = finite_or(layout.and_then(|layout| layout.width), fallback.width)
        .max(0.0)
        .min(1.0 - x);
    let height = finite_or(layout.and_then(|layout| layout.height), fallback.height)
        .max(0.0)
        .min(1.0 - y);
    PtRect::from_xywh(
        extent.width * x,
        extent.height * y,
        extent.width * width,
        extent.height * height,
    )
}

fn finite_or(value: Option<f32>, fallback: f32) -> f32 {
    value.filter(|value| value.is_finite()).unwrap_or(fallback)
}

#[derive(Clone)]
struct EffectiveTextStyle {
    family: Rc<str>,
    size: Pt,
    bold: bool,
    italic: bool,
    color: RgbColor,
}

fn effective_text_style(
    style: &ChartTextStyle,
    default_size: Pt,
    default_color: RgbColor,
    theme: Option<&Theme>,
) -> EffectiveTextStyle {
    let color = style.color.as_ref().map_or(default_color, |color| {
        let rgba = resolve_drawing_color(color, &DrawingColorContext::new(theme));
        rgb_from_u32(rgba.to_rgb24())
    });
    EffectiveTextStyle {
        family: Rc::from(style.font_family.as_deref().unwrap_or("Arial")),
        size: Pt::new(
            style
                .font_size_points
                .filter(|size| size.is_finite() && *size > 0.0)
                .unwrap_or(default_size.raw()),
        ),
        bold: style.bold.unwrap_or(false),
        italic: style.italic.unwrap_or(false),
        color,
    }
}

fn font_props(style: &EffectiveTextStyle) -> FontProps {
    FontProps {
        family: style.family.clone(),
        size: style.size,
        bold: style.bold,
        italic: style.italic,
        underline: false,
        char_spacing: Pt::ZERO,
        text_scale: 1.0,
        east_asian_language: None,
        underline_position: Pt::ZERO,
        underline_thickness: Pt::ZERO,
    }
}

fn push_centered_text(
    commands: &mut Vec<DrawCommand>,
    text: &ChartText,
    center: PtOffset,
    default_size: Pt,
    default_color: RgbColor,
    ctx: &BuildContext<'_>,
) {
    let style = effective_text_style(
        &text.style,
        default_size,
        default_color,
        ctx.resolved.theme.as_ref(),
    );
    let props = font_props(&style);
    let (width, metrics) = ctx.measurer.measure(&text.text, &props);
    push_text_command(
        commands,
        &text.text,
        PtOffset::new(
            center.x - width * 0.5,
            center.y + (metrics.ascent - metrics.descent) * 0.5,
        ),
        &style,
    );
}

fn push_single_line_text(
    commands: &mut Vec<DrawCommand>,
    text: &ChartText,
    rect: PtRect,
    default_size: Pt,
    default_color: RgbColor,
    ctx: &BuildContext<'_>,
) {
    let style = effective_text_style(
        &text.style,
        default_size,
        default_color,
        ctx.resolved.theme.as_ref(),
    );
    let props = font_props(&style);
    let (width, metrics) = ctx.measurer.measure(&text.text, &props);
    let x = aligned_x(text.alignment, rect, width);
    let baseline =
        rect.origin.y + rect.size.height * 0.5 + (metrics.ascent - metrics.descent) * 0.5;
    push_text_command(commands, &text.text, PtOffset::new(x, baseline), &style);
}

fn push_wrapped_text(
    commands: &mut Vec<DrawCommand>,
    text: &ChartText,
    rect: PtRect,
    default_size: Pt,
    default_color: RgbColor,
    ctx: &BuildContext<'_>,
    max_lines: usize,
) {
    let style = effective_text_style(
        &text.style,
        default_size,
        default_color,
        ctx.resolved.theme.as_ref(),
    );
    let props = font_props(&style);
    let lines = wrap_text(&text.text, rect.size.width, &props, ctx, max_lines);
    let (_, metrics) = ctx.measurer.measure("Mg", &props);
    let line_height = metrics.ascent + metrics.descent + metrics.leading;
    let mut baseline = rect.origin.y + metrics.ascent;
    for line in lines {
        if baseline + metrics.descent > rect.origin.y + rect.size.height {
            break;
        }
        let (width, _) = ctx.measurer.measure(&line, &props);
        let x = aligned_x(text.alignment, rect, width);
        push_text_command(commands, &line, PtOffset::new(x, baseline), &style);
        baseline += line_height;
    }
}

fn wrap_text(
    text: &str,
    max_width: Pt,
    props: &FontProps,
    ctx: &BuildContext<'_>,
    max_lines: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    for authored_line in text.split('\n') {
        let mut current = String::new();
        for word in authored_line.split_whitespace() {
            let candidate = if current.is_empty() {
                word.to_owned()
            } else {
                format!("{current} {word}")
            };
            if current.is_empty() || ctx.measurer.measure(&candidate, props).0 <= max_width {
                current = candidate;
            } else {
                if lines.len() == max_lines {
                    return lines;
                }
                lines.push(current);
                current = word.to_owned();
            }
        }
        if !current.is_empty() {
            if lines.len() == max_lines {
                return lines;
            }
            lines.push(current);
        }
    }
    lines
}

fn aligned_x(alignment: ChartTextAlignment, rect: PtRect, width: Pt) -> Pt {
    match alignment {
        ChartTextAlignment::Left => rect.origin.x,
        ChartTextAlignment::Center => rect.origin.x + (rect.size.width - width) * 0.5,
        ChartTextAlignment::Right => rect.origin.x + rect.size.width - width,
    }
}

fn push_text_command(
    commands: &mut Vec<DrawCommand>,
    text: &str,
    position: PtOffset,
    style: &EffectiveTextStyle,
) {
    commands.push(DrawCommand::Text {
        position,
        text: Rc::from(text),
        font_family: style.family.clone(),
        char_spacing: Pt::ZERO,
        font_size: style.size,
        bold: style.bold,
        italic: style.italic,
        color: style.color,
        text_scale: 1.0,
        rotation_degrees: 0.0,
    });
}

fn push_rect_outline(commands: &mut Vec<DrawCommand>, rect: PtRect, color: RgbColor, width: Pt) {
    let left = rect.origin.x;
    let top = rect.origin.y;
    let right = left + rect.size.width;
    let bottom = top + rect.size.height;
    for (start, end) in [
        (PtOffset::new(left, top), PtOffset::new(right, top)),
        (PtOffset::new(right, top), PtOffset::new(right, bottom)),
        (PtOffset::new(right, bottom), PtOffset::new(left, bottom)),
        (PtOffset::new(left, bottom), PtOffset::new(left, top)),
    ] {
        commands.push(DrawCommand::Line {
            line: PtLineSegment::new(start, end),
            color,
            width,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pie_slice_starts_at_center_and_closes() {
        let path = pie_slice_path(
            PtOffset::new(Pt::new(50.0), Pt::new(50.0)),
            Pt::new(40.0),
            Pt::ZERO,
            -90.0,
            90.0,
        );
        assert!(matches!(path.verbs[0], PathVerb::MoveTo(_)));
        assert!(matches!(path.verbs[1], PathVerb::LineTo(_)));
        assert!(matches!(path.verbs[2], PathVerb::CubicTo(_, _, _)));
        assert!(matches!(path.verbs.last(), Some(PathVerb::Close)));
    }

    #[test]
    fn doughnut_slice_traces_outer_and_inner_arcs() {
        let path = pie_slice_path(
            PtOffset::new(Pt::new(50.0), Pt::new(50.0)),
            Pt::new(40.0),
            Pt::new(20.0),
            -90.0,
            180.0,
        );
        assert_eq!(
            path.verbs
                .iter()
                .filter(|verb| matches!(verb, PathVerb::CubicTo(_, _, _)))
                .count(),
            4
        );
        assert!(matches!(path.verbs.last(), Some(PathVerb::Close)));
    }

    #[test]
    fn normalized_layout_clamps_to_chart_bounds() {
        let layout = ChartLayout {
            x: Some(0.8),
            y: Some(0.9),
            width: Some(0.5),
            height: Some(0.5),
        };
        let rect = normalized_rect(
            Some(&layout),
            PtSize::new(Pt::new(100.0), Pt::new(200.0)),
            NormalizedRect::new(0.0, 0.0, 1.0, 1.0),
        );
        assert!((rect.size.width.raw() - 20.0).abs() < 0.01);
        assert!((rect.size.height.raw() - 20.0).abs() < 0.01);
    }

    #[test]
    fn oversized_public_chart_model_exceeds_command_budget() {
        let chart = PieChart {
            hole_size_percent: 0.0,
            first_slice_angle_degrees: 0.0,
            points: (0..=PieChart::MAX_POINTS)
                .map(|index| crate::model::PiePoint {
                    index: index as u32,
                    value: 1.0,
                    category: None,
                    shape_properties: None,
                    label: None,
                })
                .collect(),
            plot_layout: None,
            title: None,
            title_layout: None,
            legend: None,
        };
        assert!(fixed_command_budget(&chart).is_none());
    }
}
