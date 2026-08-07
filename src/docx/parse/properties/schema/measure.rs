//! Table measure (§17.18.87 ST_TblWidth) — a discriminated width value used
//! by `<w:tblW>`, `<w:tcW>`, `<w:tblInd>`, `<w:tblCellSpacing>`, `<w:wAfter>`.
//!
//! `@type` picks the interpretation of `@w`:
//! - `dxa` → twips
//! - `pct` → percentage (stored as thousandths-of-percent)
//! - `auto` / `nil` → no explicit value

use serde::{Deserialize, Deserializer};

use crate::docx::model::dimension::{Dimension, ThousandthPercent};
use crate::docx::model::TableMeasure;
use crate::docx::parse::primitives::integer_measure::{parse_integer_measure, IntegerMeasure};

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum StTblWidthType {
    Auto,
    Dxa,
    Nil,
    Pct,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TableMeasureXml {
    w: Option<IntegerMeasure>,
    ty: StTblWidthType,
}

impl<'de> Deserialize<'de> for TableMeasureXml {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(rename = "@w", default)]
            w: Option<String>,
            #[serde(rename = "@type", default = "default_type")]
            ty: StTblWidthType,
        }

        let raw = Raw::deserialize(deserializer)?;
        let w = raw
            .w
            .as_deref()
            .map(|value| parse_table_measure_value(value, raw.ty))
            .transpose()
            .map_err(serde::de::Error::custom)?;
        Ok(Self { w, ty: raw.ty })
    }
}

fn parse_table_measure_value(
    raw: &str,
    ty: StTblWidthType,
) -> Result<IntegerMeasure, &'static str> {
    let Some(percent) = raw.strip_suffix('%') else {
        return parse_integer_measure(raw);
    };
    if !matches!(ty, StTblWidthType::Pct) {
        return Err("percentage table width requires type=pct");
    }
    let percent = percent
        .parse::<f64>()
        .map_err(|_| "invalid percentage table width")?;
    let fiftieths = percent * 50.0;
    if !fiftieths.is_finite() || fiftieths < i64::MIN as f64 || fiftieths > i64::MAX as f64 {
        return Err("percentage table width is outside the supported range");
    }
    parse_integer_measure(&format!("{:.0}", fiftieths.round()))
}

fn default_type() -> StTblWidthType {
    StTblWidthType::Auto
}

impl From<TableMeasureXml> for TableMeasure {
    fn from(x: TableMeasureXml) -> Self {
        let value = x.w.map(IntegerMeasure::value).unwrap_or(0);
        match x.ty {
            StTblWidthType::Auto => Self::Auto,
            StTblWidthType::Nil => Self::Nil,
            StTblWidthType::Dxa => Self::Twips(Dimension::new(value)),
            StTblWidthType::Pct => Self::Pct(Dimension::<ThousandthPercent>::new(value)),
        }
    }
}

pub(crate) fn deserialize_optional_nonnegative_table_measure<'de, D>(
    deserializer: D,
) -> Result<Option<TableMeasureXml>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let measure = Option::<TableMeasureXml>::deserialize(deserializer)?;
    if measure
        .as_ref()
        .and_then(|value| value.w.as_ref())
        .is_some_and(IntegerMeasure::is_negative)
    {
        return Err(serde::de::Error::custom(
            "negative value is not valid for this OOXML table measurement",
        ));
    }
    Ok(measure)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct NonnegativeTableMeasure {
        #[serde(
            rename = "tblW",
            default,
            deserialize_with = "deserialize_optional_nonnegative_table_measure"
        )]
        value: Option<TableMeasureXml>,
    }

    fn parse(xml: &str) -> TableMeasure {
        let x: TableMeasureXml = quick_xml::de::from_str(xml).unwrap();
        x.into()
    }

    #[test]
    fn dxa_twips() {
        match parse(r#"<tblW w="5000" type="dxa"/>"#) {
            TableMeasure::Twips(d) => assert_eq!(d.raw(), 5000),
            other => panic!("expected Twips, got {other:?}"),
        }
    }

    #[test]
    fn pct_thousandth_percent() {
        match parse(r#"<tblW w="2500" type="pct"/>"#) {
            TableMeasure::Pct(d) => assert_eq!(d.raw(), 2500),
            other => panic!("expected Pct, got {other:?}"),
        }
    }

    #[test]
    fn pct_literal_percent_is_converted_to_ooxml_fiftieths() {
        match parse(r#"<tblW w="100%" type="pct"/>"#) {
            TableMeasure::Pct(d) => assert_eq!(d.raw(), 5000),
            other => panic!("expected Pct, got {other:?}"),
        }
        match parse(r#"<tblW w="33.3%" type="pct"/>"#) {
            TableMeasure::Pct(d) => assert_eq!(d.raw(), 1665),
            other => panic!("expected Pct, got {other:?}"),
        }
    }

    #[test]
    fn auto_and_nil() {
        assert!(matches!(
            parse(r#"<tblW w="0" type="auto"/>"#),
            TableMeasure::Auto
        ));
        assert!(matches!(
            parse(r#"<tblW w="0" type="nil"/>"#),
            TableMeasure::Nil
        ));
    }

    #[test]
    fn missing_type_defaults_to_auto() {
        assert!(matches!(parse(r#"<tblW/>"#), TableMeasure::Auto));
    }

    #[test]
    fn decimal_widths_round_to_nearest_integer() {
        match parse(r#"<tblW w="2500.4" type="dxa"/>"#) {
            TableMeasure::Twips(d) => assert_eq!(d.raw(), 2500),
            other => panic!("expected Twips, got {other:?}"),
        }
        match parse(r#"<tblW w="2500.5" type="dxa"/>"#) {
            TableMeasure::Twips(d) => assert_eq!(d.raw(), 2501),
            other => panic!("expected Twips, got {other:?}"),
        }
    }

    #[test]
    fn negative_fractions_are_rejected_for_nonnegative_table_measurements() {
        for raw in ["-0.1", "-0.49"] {
            let result: Result<NonnegativeTableMeasure, _> =
                quick_xml::de::from_str(&format!(r#"<x><tblW w="{raw}" type="dxa"/></x>"#));
            if let Ok(value) = result {
                panic!(
                    "{raw:?} must be rejected, got {:?}",
                    value.value.map(TableMeasure::from)
                );
            }
        }
    }
}
