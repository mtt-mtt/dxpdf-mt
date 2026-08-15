//! Minimal DrawingML chart model.
//!
//! The first supported family is a cached, single-series 2-D pie/doughnut
//! chart.  Keeping the cache-derived values in the document model means the
//! renderer never needs to execute or even open an embedded workbook.

use super::{DrawingColor, ShapeProperties};

/// A parsed chart part that the renderer understands.
#[derive(Clone, Debug)]
pub enum Chart {
    Pie(PieChart),
}

/// One cached, single-series 2-D pie/doughnut chart.
#[derive(Clone, Debug)]
pub struct PieChart {
    /// `0` is accepted as a producer-compatible spelling of a pie chart.
    pub hole_size_percent: f32,
    /// Clockwise degrees from twelve o'clock.
    pub first_slice_angle_degrees: f32,
    pub points: Vec<PiePoint>,
    pub plot_layout: Option<ChartLayout>,
    pub title: Option<ChartText>,
    pub title_layout: Option<ChartLayout>,
    pub legend: Option<ChartLegend>,
}

impl PieChart {
    /// A pie with more slices than this is neither useful on a page nor safe
    /// to expand into an unbounded number of vector commands.
    pub(crate) const MAX_POINTS: usize = 1_024;

    /// Defense in depth for models constructed outside the DOCX parser.  A
    /// slice, label, and legend row can expand to several primitive commands.
    pub(crate) const MAX_DRAW_COMMANDS: usize = 8_192;
}

/// One point/slice from the cached series.
#[derive(Clone, Debug)]
pub struct PiePoint {
    pub index: u32,
    pub value: f64,
    pub category: Option<String>,
    /// Direct `c:dPt/c:spPr`; its fill/line resolve through the normal
    /// DrawingML theme-color pipeline.
    pub shape_properties: Option<ShapeProperties>,
    pub label: Option<ChartText>,
}

/// Normalized chart-local layout values (`0.0 .. 1.0`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChartLayout {
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub width: Option<f32>,
    pub height: Option<f32>,
}

/// Plain text plus the first effective DrawingML run style.
///
/// Pie-chart titles and data labels in real Word documents are frequently
/// split into several identically styled `<a:r>` nodes.  Concatenating those
/// nodes preserves the authored text while keeping this first chart tier
/// deliberately smaller than the full DrawingML rich-text model.
#[derive(Clone, Debug)]
pub struct ChartText {
    pub text: String,
    pub style: ChartTextStyle,
    pub alignment: ChartTextAlignment,
}

#[derive(Clone, Debug, Default)]
pub struct ChartTextStyle {
    pub font_family: Option<String>,
    pub font_size_points: Option<f32>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub color: Option<DrawingColor>,
}

impl ChartTextStyle {
    /// Apply the explicitly present fields in `other` over this style.
    pub fn merge_from(&mut self, other: Self) {
        if other.font_family.is_some() {
            self.font_family = other.font_family;
        }
        if other.font_size_points.is_some() {
            self.font_size_points = other.font_size_points;
        }
        if other.bold.is_some() {
            self.bold = other.bold;
        }
        if other.italic.is_some() {
            self.italic = other.italic;
        }
        if other.color.is_some() {
            self.color = other.color;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChartTextAlignment {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Clone, Debug)]
pub struct ChartLegend {
    pub layout: Option<ChartLayout>,
    pub style: ChartTextStyle,
    pub entries: Vec<ChartLegendEntry>,
}

#[derive(Clone, Debug)]
pub struct ChartLegendEntry {
    pub index: u32,
    pub deleted: bool,
    pub style: ChartTextStyle,
}
