//! DrawingML chart parsing.
//!
//! This tier intentionally consumes only cached, single-series 2-D
//! pie/doughnut data.  Embedded workbook packages are neither opened nor
//! executed; a chart without usable cached values remains unsupported.

use std::collections::HashMap;

use serde::Deserialize;

use crate::docx::error::Result;
use crate::docx::model::{
    Chart, ChartLayout, ChartLegend, ChartLegendEntry, ChartText, ChartTextAlignment,
    ChartTextStyle, PieChart, PiePoint, ShapeProperties,
};
use crate::docx::parse::drawing::schema::fill::{AttrBool, SolidFillXml};
use crate::docx::parse::drawing::schema::shape::SpPrXml;
use crate::docx::parse::serde_xml::from_xml;

// A chart with at most 1,024 useful pie points does not need a multi-megabyte
// XML part.  Reject oversized optional parts before serde allocates their
// nested caches; this is intentionally much tighter than the package limit.
const MAX_CHART_XML_BYTES: usize = 8 * 1024 * 1024;

pub fn parse_chart(data: &[u8]) -> Result<Option<Chart>> {
    if data.len() > MAX_CHART_XML_BYTES {
        log::warn!(
            "chart part exceeds the {} byte safety limit",
            MAX_CHART_XML_BYTES
        );
        return Ok(None);
    }
    let chart_space: ChartSpaceXml = from_xml(data)?;
    let Some(chart) = chart_space.chart else {
        return Ok(None);
    };
    let Some(plot_area) = chart.plot_area else {
        return Ok(None);
    };

    let plot_layout = plot_area.layout.and_then(LayoutXml::into_model);
    let pie = if let Some(pie) = plot_area.pie_chart {
        build_pie(
            pie.series,
            pie.data_labels,
            0.0,
            pie.first_slice_angle.map_or(0.0, |v| v.val),
            plot_layout,
            chart.title,
            chart.legend,
        )
    } else if let Some(doughnut) = plot_area.doughnut_chart {
        build_pie(
            doughnut.series,
            doughnut.data_labels,
            doughnut.hole_size.map_or(50.0, |v| v.val),
            doughnut.first_slice_angle.map_or(0.0, |v| v.val),
            plot_layout,
            chart.title,
            chart.legend,
        )
    } else {
        None
    };

    Ok(pie.map(Chart::Pie))
}

fn build_pie(
    mut series: Vec<SeriesXml>,
    chart_data_labels: Option<DataLabelsXml>,
    hole_size_percent: f32,
    first_slice_angle_degrees: f32,
    plot_layout: Option<ChartLayout>,
    title: Option<TitleXml>,
    legend: Option<LegendXml>,
) -> Option<PieChart> {
    // Never let non-finite producer values reach trigonometry in the vector
    // builder.  Treat the chart as unsupported instead of guessing geometry.
    if !hole_size_percent.is_finite() || !first_slice_angle_degrees.is_finite() {
        return None;
    }

    // This tier deliberately accepts exactly one series. Silently selecting
    // one ring from a multi-series doughnut would misrepresent the data.
    if series.len() != 1 {
        return None;
    }
    let series = series.pop()?;
    if series.data_points.len() > PieChart::MAX_POINTS
        || series
            .data_labels
            .as_ref()
            .is_some_and(|labels| labels.labels.len() > PieChart::MAX_POINTS)
        || chart_data_labels
            .as_ref()
            .is_some_and(|labels| labels.labels.len() > PieChart::MAX_POINTS)
        || legend
            .as_ref()
            .is_some_and(|legend| legend.entries.len() > PieChart::MAX_POINTS)
    {
        return None;
    }
    let value_points = series.values?.into_points()?;
    if value_points.len() > PieChart::MAX_POINTS {
        return None;
    }
    let mut values = Vec::new();
    for point in value_points {
        let value = point.value?.trim().parse::<f64>().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        values.push((point.index, value));
    }
    values.sort_unstable_by_key(|(index, _)| *index);
    if values.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return None;
    }
    if values.is_empty() || values.iter().map(|(_, value)| *value).sum::<f64>() <= 0.0 {
        return None;
    }

    let category_points = series
        .categories
        .map(CategoriesXml::into_points)
        .unwrap_or_default();
    if category_points.len() > PieChart::MAX_POINTS {
        return None;
    }
    let categories: HashMap<u32, String> = category_points
        .into_iter()
        .filter_map(|point| point.value.map(|value| (point.index, value)))
        .collect();
    let point_shapes: HashMap<u32, ShapeProperties> = series
        .data_points
        .into_iter()
        .filter_map(|point| {
            let index = point.index?.val;
            point.shape_properties.map(|sp| (index, sp.into()))
        })
        .collect();

    let total = values.iter().map(|(_, value)| *value).sum::<f64>();
    let chart_labels = chart_data_labels.unwrap_or_default();
    let series_labels = series.data_labels.unwrap_or_default();
    let mut global_label_style = chart_labels
        .text_properties
        .map(RichTextXml::into_default_style)
        .unwrap_or_default();
    if let Some(properties) = series_labels.text_properties {
        global_label_style.merge_from(properties.into_default_style());
    }
    let global_show_percent = bool_override(
        series_labels.show_percent.as_ref(),
        bool_value(chart_labels.show_percent.as_ref()),
    );
    let global_show_value = bool_override(
        series_labels.show_value.as_ref(),
        bool_value(chart_labels.show_value.as_ref()),
    );
    let global_show_category = bool_override(
        series_labels.show_category_name.as_ref(),
        bool_value(chart_labels.show_category_name.as_ref()),
    );
    let mut labels_by_index = HashMap::new();
    for label in chart_labels.labels {
        if let Some(index) = label.index.as_ref().map(|index| index.val) {
            labels_by_index.insert(index, label);
        }
    }
    for label in series_labels.labels {
        if let Some(index) = label.index.as_ref().map(|index| index.val) {
            let label = match labels_by_index.remove(&index) {
                Some(base) => base.overlay(label),
                None => label,
            };
            labels_by_index.insert(index, label);
        }
    }

    let points = values
        .into_iter()
        .map(|(index, value)| {
            let category = categories.get(&index).cloned();
            let label = make_data_label(
                labels_by_index.remove(&index),
                &global_label_style,
                global_show_percent,
                global_show_value,
                global_show_category,
                value,
                total,
                category.as_deref(),
            );
            PiePoint {
                index,
                value,
                category,
                shape_properties: point_shapes.get(&index).cloned(),
                label,
            }
        })
        .collect();

    let (title_layout, title) = title.map_or((None, None), |title| {
        (
            title.layout.and_then(LayoutXml::into_model),
            title
                .text
                .and_then(|text| text.rich)
                .and_then(|rich| rich.into_text(ChartTextStyle::default())),
        )
    });
    let legend = legend.map(LegendXml::into_model);

    Some(PieChart {
        // Several major producers emit zero even though the normative range
        // starts above zero. It unambiguously means an ordinary pie chart.
        hole_size_percent: hole_size_percent.clamp(0.0, 90.0),
        first_slice_angle_degrees: first_slice_angle_degrees.rem_euclid(360.0),
        points,
        plot_layout,
        title,
        title_layout,
        legend,
    })
}

#[allow(clippy::too_many_arguments)]
fn make_data_label(
    label: Option<DataLabelXml>,
    global_style: &ChartTextStyle,
    global_show_percent: bool,
    global_show_value: bool,
    global_show_category: bool,
    value: f64,
    total: f64,
    category: Option<&str>,
) -> Option<ChartText> {
    let (text, text_properties, show_percent, show_value, show_category_name) =
        label.map_or((None, None, None, None, None), |label| {
            (
                label.text,
                label.text_properties,
                label.show_percent,
                label.show_value,
                label.show_category_name,
            )
        });
    let mut base_style = global_style.clone();
    if let Some(properties) = text_properties {
        base_style.merge_from(properties.into_default_style());
    }
    if let Some(rich) = text.and_then(|text| text.rich) {
        if let Some(text) = rich.into_text(base_style.clone()) {
            if !text.text.is_empty() {
                return Some(text);
            }
        }
    }

    // A point-level flag is an override, not an additive toggle. In
    // particular, `<c:showPercent val="0"/>` must be able to turn off a
    // globally enabled percentage label for one slice.
    let show_percent = bool_override(show_percent.as_ref(), global_show_percent);
    let show_value = bool_override(show_value.as_ref(), global_show_value);
    let show_category = bool_override(show_category_name.as_ref(), global_show_category);
    let text = if show_percent {
        format!("{:.0}%", value / total * 100.0)
    } else if show_value {
        format_number(value)
    } else if show_category {
        category?.to_owned()
    } else {
        return None;
    };
    Some(ChartText {
        text,
        style: base_style,
        alignment: ChartTextAlignment::Center,
    })
}

fn format_number(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn bool_value(value: Option<&BoolValXml>) -> bool {
    value.is_some_and(BoolValXml::resolved)
}

fn bool_override(value: Option<&BoolValXml>, inherited: bool) -> bool {
    value.map_or(inherited, BoolValXml::resolved)
}

#[derive(Deserialize, Default)]
struct ChartSpaceXml {
    #[serde(rename = "chart", default)]
    chart: Option<ChartXml>,
}

#[derive(Deserialize, Default)]
struct ChartXml {
    #[serde(rename = "title", default)]
    title: Option<TitleXml>,
    #[serde(rename = "plotArea", default)]
    plot_area: Option<PlotAreaXml>,
    #[serde(rename = "legend", default)]
    legend: Option<LegendXml>,
}

#[derive(Deserialize, Default)]
struct PlotAreaXml {
    #[serde(rename = "layout", default)]
    layout: Option<LayoutXml>,
    #[serde(rename = "pieChart", default)]
    pie_chart: Option<PieChartXml>,
    #[serde(rename = "doughnutChart", default)]
    doughnut_chart: Option<DoughnutChartXml>,
}

#[derive(Deserialize, Default)]
struct PieChartXml {
    #[serde(rename = "ser", default)]
    series: Vec<SeriesXml>,
    #[serde(rename = "dLbls", default)]
    data_labels: Option<DataLabelsXml>,
    #[serde(rename = "firstSliceAng", default)]
    first_slice_angle: Option<FloatValXml>,
}

#[derive(Deserialize, Default)]
struct DoughnutChartXml {
    #[serde(rename = "ser", default)]
    series: Vec<SeriesXml>,
    #[serde(rename = "dLbls", default)]
    data_labels: Option<DataLabelsXml>,
    #[serde(rename = "firstSliceAng", default)]
    first_slice_angle: Option<FloatValXml>,
    #[serde(rename = "holeSize", default)]
    hole_size: Option<FloatValXml>,
}

#[derive(Deserialize, Default)]
struct SeriesXml {
    #[serde(rename = "dPt", default)]
    data_points: Vec<DataPointXml>,
    #[serde(rename = "dLbls", default)]
    data_labels: Option<DataLabelsXml>,
    #[serde(rename = "cat", default)]
    categories: Option<CategoriesXml>,
    #[serde(rename = "val", default)]
    values: Option<ValuesXml>,
}

#[derive(Deserialize, Default)]
struct DataPointXml {
    #[serde(rename = "idx", default)]
    index: Option<UIntValXml>,
    #[serde(rename = "spPr", default)]
    shape_properties: Option<SpPrXml>,
}

#[derive(Deserialize, Default)]
struct CategoriesXml {
    #[serde(rename = "strRef", default)]
    string_ref: Option<StringReferenceXml>,
    #[serde(rename = "strLit", default)]
    string_literal: Option<PointsCacheXml>,
    #[serde(rename = "numRef", default)]
    number_ref: Option<NumberReferenceXml>,
    #[serde(rename = "numLit", default)]
    number_literal: Option<PointsCacheXml>,
}

impl CategoriesXml {
    fn into_points(self) -> Vec<CachePointXml> {
        self.string_ref
            .and_then(|reference| reference.string_cache)
            .or(self.string_literal)
            .or_else(|| self.number_ref.and_then(|reference| reference.num_cache))
            .or(self.number_literal)
            .map(|cache| cache.points)
            .unwrap_or_default()
    }
}

#[derive(Deserialize, Default)]
struct StringReferenceXml {
    #[serde(rename = "strCache", default)]
    string_cache: Option<PointsCacheXml>,
}

#[derive(Deserialize, Default)]
struct ValuesXml {
    #[serde(rename = "numRef", default)]
    num_ref: Option<NumberReferenceXml>,
    #[serde(rename = "numLit", default)]
    num_literal: Option<PointsCacheXml>,
}

impl ValuesXml {
    fn into_points(self) -> Option<Vec<CachePointXml>> {
        self.num_ref
            .and_then(|reference| reference.num_cache)
            .or(self.num_literal)
            .map(|cache| cache.points)
    }
}

#[derive(Deserialize, Default)]
struct NumberReferenceXml {
    #[serde(rename = "numCache", default)]
    num_cache: Option<PointsCacheXml>,
}

#[derive(Deserialize, Default)]
struct PointsCacheXml {
    #[serde(rename = "pt", default)]
    points: Vec<CachePointXml>,
}

#[derive(Deserialize)]
struct CachePointXml {
    #[serde(rename = "@idx")]
    index: u32,
    #[serde(rename = "v", default)]
    value: Option<String>,
}

#[derive(Deserialize, Default)]
struct DataLabelsXml {
    #[serde(rename = "dLbl", default)]
    labels: Vec<DataLabelXml>,
    #[serde(rename = "showPercent", default)]
    show_percent: Option<BoolValXml>,
    #[serde(rename = "showVal", default)]
    show_value: Option<BoolValXml>,
    #[serde(rename = "showCatName", default)]
    show_category_name: Option<BoolValXml>,
    #[serde(rename = "txPr", default)]
    text_properties: Option<RichTextXml>,
}

#[derive(Deserialize, Default)]
struct DataLabelXml {
    #[serde(rename = "idx", default)]
    index: Option<UIntValXml>,
    #[serde(rename = "tx", default)]
    text: Option<TextXml>,
    #[serde(rename = "txPr", default)]
    text_properties: Option<RichTextXml>,
    #[serde(rename = "showPercent", default)]
    show_percent: Option<BoolValXml>,
    #[serde(rename = "showVal", default)]
    show_value: Option<BoolValXml>,
    #[serde(rename = "showCatName", default)]
    show_category_name: Option<BoolValXml>,
}

impl DataLabelXml {
    /// Apply a more specific label declaration over an inherited one.
    fn overlay(self, higher: Self) -> Self {
        Self {
            index: higher.index.or(self.index),
            text: higher.text.or(self.text),
            text_properties: higher.text_properties.or(self.text_properties),
            show_percent: higher.show_percent.or(self.show_percent),
            show_value: higher.show_value.or(self.show_value),
            show_category_name: higher.show_category_name.or(self.show_category_name),
        }
    }
}

#[derive(Deserialize, Default)]
struct TitleXml {
    #[serde(rename = "tx", default)]
    text: Option<TextXml>,
    #[serde(rename = "layout", default)]
    layout: Option<LayoutXml>,
}

#[derive(Deserialize, Default)]
struct TextXml {
    #[serde(rename = "rich", default)]
    rich: Option<RichTextXml>,
}

#[derive(Deserialize, Default)]
struct RichTextXml {
    #[serde(rename = "p", default)]
    paragraphs: Vec<RichParagraphXml>,
}

impl RichTextXml {
    fn into_default_style(self) -> ChartTextStyle {
        self.paragraphs
            .into_iter()
            .next()
            .and_then(|paragraph| paragraph.properties)
            .and_then(|properties| properties.default_run_properties)
            .map(RunPropertiesXml::into_style)
            .unwrap_or_default()
    }

    fn into_text(self, mut style: ChartTextStyle) -> Option<ChartText> {
        let mut text = String::new();
        let mut alignment = ChartTextAlignment::Center;
        let mut captured_run_style = false;
        for (paragraph_index, paragraph) in self.paragraphs.into_iter().enumerate() {
            if paragraph_index > 0 {
                text.push('\n');
            }
            if let Some(properties) = paragraph.properties {
                alignment = properties
                    .alignment
                    .as_deref()
                    .map(parse_alignment)
                    .unwrap_or(alignment);
                if let Some(defaults) = properties.default_run_properties {
                    style.merge_from(defaults.into_style());
                }
            }
            for run in paragraph.runs {
                if !captured_run_style {
                    if let Some(properties) = run.properties {
                        style.merge_from(properties.into_style());
                    }
                    captured_run_style = true;
                }
                if let Some(run_text) = run.text {
                    text.push_str(&run_text);
                }
            }
        }
        (!text.is_empty()).then_some(ChartText {
            text,
            style,
            alignment,
        })
    }
}

#[derive(Deserialize, Default)]
struct RichParagraphXml {
    #[serde(rename = "pPr", default)]
    properties: Option<ParagraphPropertiesXml>,
    #[serde(rename = "r", default)]
    runs: Vec<RichRunXml>,
}

#[derive(Deserialize, Default)]
struct ParagraphPropertiesXml {
    #[serde(rename = "@algn", default)]
    alignment: Option<String>,
    #[serde(rename = "defRPr", default)]
    default_run_properties: Option<RunPropertiesXml>,
}

#[derive(Deserialize, Default)]
struct RichRunXml {
    #[serde(rename = "rPr", default)]
    properties: Option<RunPropertiesXml>,
    #[serde(rename = "t", default)]
    text: Option<String>,
}

#[derive(Deserialize, Default)]
struct RunPropertiesXml {
    #[serde(rename = "@sz", default)]
    size: Option<u32>,
    #[serde(rename = "@b", default)]
    bold: Option<AttrBool>,
    #[serde(rename = "@i", default)]
    italic: Option<AttrBool>,
    #[serde(rename = "solidFill", default)]
    solid_fill: Option<SolidFillXml>,
    #[serde(rename = "latin", default)]
    latin: Option<TypefaceXml>,
}

impl RunPropertiesXml {
    fn into_style(self) -> ChartTextStyle {
        ChartTextStyle {
            font_family: self.latin.map(|latin| latin.typeface),
            font_size_points: self.size.map(|size| size as f32 / 100.0),
            bold: self.bold.map(|value| value.0),
            italic: self.italic.map(|value| value.0),
            color: self.solid_fill.and_then(|fill| fill.color).map(Into::into),
        }
    }
}

#[derive(Deserialize)]
struct TypefaceXml {
    #[serde(rename = "@typeface")]
    typeface: String,
}

fn parse_alignment(value: &str) -> ChartTextAlignment {
    match value {
        "l" => ChartTextAlignment::Left,
        "r" => ChartTextAlignment::Right,
        _ => ChartTextAlignment::Center,
    }
}

#[derive(Deserialize, Default)]
struct LegendXml {
    #[serde(rename = "layout", default)]
    layout: Option<LayoutXml>,
    #[serde(rename = "txPr", default)]
    text_properties: Option<RichTextXml>,
    #[serde(rename = "legendEntry", default)]
    entries: Vec<LegendEntryXml>,
}

impl LegendXml {
    fn into_model(self) -> ChartLegend {
        let LegendXml {
            layout,
            text_properties,
            entries,
        } = self;
        let style = text_properties
            .map(RichTextXml::into_default_style)
            .unwrap_or_default();
        let entries = entries
            .into_iter()
            .filter_map(|entry| {
                let LegendEntryXml {
                    index,
                    deleted,
                    text_properties,
                } = entry;
                let index = index?.val;
                let mut entry_style = style.clone();
                if let Some(properties) = text_properties {
                    entry_style.merge_from(properties.into_default_style());
                }
                Some(ChartLegendEntry {
                    index,
                    deleted: bool_value(deleted.as_ref()),
                    style: entry_style,
                })
            })
            .collect();
        ChartLegend {
            layout: layout.and_then(LayoutXml::into_model),
            style,
            entries,
        }
    }
}

#[derive(Deserialize, Default)]
struct LegendEntryXml {
    #[serde(rename = "idx", default)]
    index: Option<UIntValXml>,
    #[serde(rename = "delete", default)]
    deleted: Option<BoolValXml>,
    #[serde(rename = "txPr", default)]
    text_properties: Option<RichTextXml>,
}

#[derive(Deserialize, Default)]
struct LayoutXml {
    #[serde(rename = "manualLayout", default)]
    manual: Option<ManualLayoutXml>,
}

impl LayoutXml {
    fn into_model(self) -> Option<ChartLayout> {
        self.manual.map(ManualLayoutXml::into_model)
    }
}

#[derive(Deserialize, Default)]
struct ManualLayoutXml {
    #[serde(rename = "x", default)]
    x: Option<FloatValXml>,
    #[serde(rename = "y", default)]
    y: Option<FloatValXml>,
    #[serde(rename = "w", default)]
    width: Option<FloatValXml>,
    #[serde(rename = "h", default)]
    height: Option<FloatValXml>,
}

impl ManualLayoutXml {
    fn into_model(self) -> ChartLayout {
        ChartLayout {
            x: self.x.map(|value| value.val),
            y: self.y.map(|value| value.val),
            width: self.width.map(|value| value.val),
            height: self.height.map(|value| value.val),
        }
    }
}

#[derive(Deserialize)]
struct UIntValXml {
    #[serde(rename = "@val")]
    val: u32,
}

#[derive(Deserialize)]
struct FloatValXml {
    #[serde(rename = "@val")]
    val: f32,
}

#[derive(Deserialize)]
struct BoolValXml {
    #[serde(rename = "@val", default)]
    val: Option<AttrBool>,
}

impl BoolValXml {
    fn resolved(&self) -> bool {
        // DrawingML CT_Boolean treats a present element with omitted `val` as
        // enabled, matching the toggle convention used by Office producers.
        self.val.is_none_or(|value| value.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColorTransform, DrawingColor, SchemeColorVal};

    const NS: &str = r#"xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"
        xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main""#;

    #[test]
    fn parses_cached_single_series_doughnut_and_sparse_points() {
        let xml = format!(
            r#"<c:chartSpace {NS}><c:chart>
              <c:title><c:tx><c:rich><a:p><a:pPr algn="r"/><a:r>
                <a:rPr sz="900" b="1" i="1"><a:solidFill><a:schemeClr val="bg1"/></a:solidFill><a:latin typeface="Arial"/></a:rPr>
                <a:t>Cached title</a:t></a:r></a:p></c:rich></c:tx>
                <c:layout><c:manualLayout><c:x val="0.2"/><c:y val="0.05"/></c:manualLayout></c:layout>
              </c:title>
              <c:plotArea><c:layout><c:manualLayout><c:x val="0.05"/><c:y val="0.25"/><c:w val="0.4"/><c:h val="0.65"/></c:manualLayout></c:layout>
                <c:doughnutChart><c:ser>
                  <c:dPt><c:idx val="0"/><c:spPr><a:solidFill><a:schemeClr val="accent1"/></a:solidFill></c:spPr></c:dPt>
                  <c:dPt><c:idx val="2"/><c:spPr><a:solidFill><a:schemeClr val="accent5"><a:lumMod val="74901"/></a:schemeClr></a:solidFill></c:spPr></c:dPt>
                  <c:dLbls><c:dLbl><c:idx val="0"/><c:tx><c:rich><a:p><a:r><a:t>28%</a:t></a:r></a:p></c:rich></c:tx></c:dLbl><c:showVal val="1"/></c:dLbls>
                  <c:cat><c:strRef><c:strCache><c:ptCount val="6"/><c:pt idx="0"><c:v>A</c:v></c:pt><c:pt idx="2"><c:v>C</c:v></c:pt></c:strCache></c:strRef></c:cat>
                  <c:val><c:numRef><c:numCache><c:ptCount val="6"/><c:pt idx="0"><c:v>70</c:v></c:pt><c:pt idx="2"><c:v>95</c:v></c:pt></c:numCache></c:numRef></c:val>
                </c:ser><c:firstSliceAng val="15"/><c:holeSize val="0"/></c:doughnutChart>
              </c:plotArea>
              <c:legend><c:layout><c:manualLayout><c:x val="0.5"/><c:y val="0.4"/><c:w val="0.45"/><c:h val="0.4"/></c:manualLayout></c:layout></c:legend>
            </c:chart></c:chartSpace>"#
        );
        let chart = parse_chart(xml.as_bytes()).unwrap().unwrap();
        let Chart::Pie(chart) = chart;
        assert_eq!(chart.hole_size_percent, 0.0);
        assert_eq!(chart.first_slice_angle_degrees, 15.0);
        assert_eq!(
            chart.points.len(),
            2,
            "ptCount must not create phantom points"
        );
        assert_eq!(chart.points[0].index, 0);
        assert_eq!(chart.points[1].index, 2);
        assert_eq!(chart.points[0].label.as_ref().unwrap().text, "28%");
        assert_eq!(chart.title.as_ref().unwrap().text, "Cached title");
        assert_eq!(
            chart.title.as_ref().unwrap().alignment,
            ChartTextAlignment::Right
        );
        assert_eq!(chart.plot_layout.unwrap().width, Some(0.4));
        let fill = chart.points[1]
            .shape_properties
            .as_ref()
            .and_then(|properties| properties.fill.as_ref())
            .unwrap();
        let crate::model::DrawingFill::Solid(DrawingColor::Scheme { name, transforms }) = fill
        else {
            panic!("expected scheme fill")
        };
        assert_eq!(*name, SchemeColorVal::Accent5);
        assert!(matches!(transforms.as_slice(), [ColorTransform::LumMod(_)]));
    }

    #[test]
    fn explicit_label_wins_over_recomputed_percentage() {
        let xml = format!(
            r#"<c:chartSpace {NS}><c:chart><c:plotArea><c:pieChart><c:ser>
              <c:dLbls><c:dLbl><c:idx val="0"/><c:showPercent val="1"/><c:tx><c:rich><a:p><a:r><a:t>23%</a:t></a:r></a:p></c:rich></c:tx></c:dLbl></c:dLbls>
              <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>A</c:v></c:pt><c:pt idx="1"><c:v>B</c:v></c:pt></c:strCache></c:strRef></c:cat>
              <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>60</c:v></c:pt><c:pt idx="1"><c:v>195</c:v></c:pt></c:numCache></c:numRef></c:val>
            </c:ser></c:pieChart></c:plotArea></c:chart></c:chartSpace>"#
        );
        let Some(Chart::Pie(chart)) = parse_chart(xml.as_bytes()).unwrap() else {
            panic!("expected pie")
        };
        assert_eq!(chart.points[0].label.as_ref().unwrap().text, "23%");
    }

    #[test]
    fn global_labels_apply_to_every_point_and_local_false_overrides() {
        let xml = format!(
            r#"<c:chartSpace {NS}><c:chart><c:plotArea><c:pieChart><c:ser>
              <c:dLbls><c:dLbl><c:idx val="0"/><c:showPercent val="0"/></c:dLbl></c:dLbls>
              <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>A</c:v></c:pt><c:pt idx="1"><c:v>B</c:v></c:pt></c:strCache></c:strRef></c:cat>
              <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>1</c:v></c:pt><c:pt idx="1"><c:v>3</c:v></c:pt></c:numCache></c:numRef></c:val>
            </c:ser><c:dLbls><c:showPercent/></c:dLbls></c:pieChart></c:plotArea></c:chart></c:chartSpace>"#
        );
        let Some(Chart::Pie(chart)) = parse_chart(xml.as_bytes()).unwrap() else {
            panic!("expected pie")
        };
        assert!(chart.points[0].label.is_none());
        assert_eq!(chart.points[1].label.as_ref().unwrap().text, "75%");
    }

    #[test]
    fn parses_literal_caches_and_orders_points_by_index() {
        let xml = format!(
            r#"<c:chartSpace {NS}><c:chart><c:plotArea><c:pieChart><c:ser>
              <c:cat><c:strLit><c:pt idx="1"><c:v>B</c:v></c:pt><c:pt idx="0"><c:v>A</c:v></c:pt></c:strLit></c:cat>
              <c:val><c:numLit><c:pt idx="1"><c:v>3</c:v></c:pt><c:pt idx="0"><c:v>1</c:v></c:pt></c:numLit></c:val>
            </c:ser></c:pieChart></c:plotArea></c:chart></c:chartSpace>"#
        );
        let Some(Chart::Pie(chart)) = parse_chart(xml.as_bytes()).unwrap() else {
            panic!("expected pie")
        };
        assert_eq!(chart.points[0].index, 0);
        assert_eq!(chart.points[0].category.as_deref(), Some("A"));
        assert_eq!(chart.points[1].index, 1);
        assert_eq!(chart.points[1].category.as_deref(), Some("B"));
    }

    #[test]
    fn rejects_multi_series_and_missing_numeric_cache() {
        let multi = format!(
            r#"<c:chartSpace {NS}><c:chart><c:plotArea><c:pieChart><c:ser/><c:ser/></c:pieChart></c:plotArea></c:chart></c:chartSpace>"#
        );
        assert!(parse_chart(multi.as_bytes()).unwrap().is_none());
        let missing = format!(
            r#"<c:chartSpace {NS}><c:chart><c:plotArea><c:pieChart><c:ser><c:val><c:numRef/></c:val></c:ser></c:pieChart></c:plotArea></c:chart></c:chartSpace>"#
        );
        assert!(parse_chart(missing.as_bytes()).unwrap().is_none());
        let duplicate = format!(
            r#"<c:chartSpace {NS}><c:chart><c:plotArea><c:pieChart><c:ser><c:val><c:numLit><c:pt idx="0"><c:v>1</c:v></c:pt><c:pt idx="0"><c:v>2</c:v></c:pt></c:numLit></c:val></c:ser></c:pieChart></c:plotArea></c:chart></c:chartSpace>"#
        );
        assert!(parse_chart(duplicate.as_bytes()).unwrap().is_none());
    }

    #[test]
    fn rejects_non_finite_pie_geometry() {
        let cached_series = || {
            let xml = format!(
                r#"<c:chartSpace {NS}><c:chart><c:plotArea><c:pieChart><c:ser>
                  <c:val><c:numLit><c:pt idx="0"><c:v>1</c:v></c:pt></c:numLit></c:val>
                </c:ser></c:pieChart></c:plotArea></c:chart></c:chartSpace>"#
            );
            let chart_space: ChartSpaceXml = from_xml(xml.as_bytes()).unwrap();
            chart_space
                .chart
                .unwrap()
                .plot_area
                .unwrap()
                .pie_chart
                .unwrap()
                .series
        };
        assert!(build_pie(cached_series(), None, 0.0, f32::NAN, None, None, None).is_none());
        assert!(build_pie(cached_series(), None, f32::INFINITY, 0.0, None, None, None).is_none());
    }

    #[test]
    fn rejects_chart_with_excessive_point_count() {
        let points = (0..=PieChart::MAX_POINTS)
            .map(|index| format!(r#"<c:pt idx="{index}"><c:v>1</c:v></c:pt>"#))
            .collect::<String>();
        let xml = format!(
            r#"<c:chartSpace {NS}><c:chart><c:plotArea><c:pieChart><c:ser>
              <c:val><c:numLit>{points}</c:numLit></c:val>
            </c:ser></c:pieChart></c:plotArea></c:chart></c:chartSpace>"#
        );
        assert!(parse_chart(xml.as_bytes()).unwrap().is_none());
    }

    #[test]
    fn rejects_oversized_chart_part_before_xml_deserialization() {
        let oversized = vec![b' '; MAX_CHART_XML_BYTES + 1];
        assert!(parse_chart(&oversized).unwrap().is_none());
    }
}
