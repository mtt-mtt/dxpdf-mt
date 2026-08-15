use crate::model::{self, Block, Table, TableCell};
use crate::render::dimension::Pt;
use crate::render::geometry;
use crate::render::layout::paragraph::DropCapInfo;
use crate::render::layout::section::LayoutBlock;
use crate::render::layout::table::{
    compute_column_widths, CellBorderConfig, CellBorderOverride, TableCellInput, TableRowInput,
};
use crate::render::resolve::color::{resolve_color, ColorContext};
use crate::render::resolve::conditional::{
    resolve_cell_conditional, CellConditionalFormatting, CellGridPosition,
};
use crate::render::resolve::styles::ResolvedStyle;

use super::block::build_paragraph_block;
use crate::render::layout::fragment::split_oversized_fragments_for_word_wrap;

use super::convert::{
    convert_cell_border_override, convert_table_border_config, merge_table_borders,
};
use super::{BuildContext, BuildState};

/// Result of building a table from the model.
pub(super) struct BuiltTable {
    pub(super) rows: Vec<TableRowInput>,
    /// Grid slots, already shrunk by `cell_spacing`.
    pub(super) col_widths: Vec<Pt>,
    /// §17.4.44 `tblCellSpacing` in points; zero when unset.
    pub(super) cell_spacing: Pt,
    pub(super) border_config: Option<crate::render::layout::table::TableBorderConfig>,
    /// §17.4.51: table indentation from left margin.
    pub(super) indent: Pt,
    /// §17.4.28: table horizontal alignment (left/center/right).
    pub(super) alignment: Option<model::Alignment>,
    pub(super) float_info: Option<super::super::section::TableFloatInfo>,
}

/// §17.4.81: turn a parsed `trHeight` into a layout constraint.
///
/// `exact` pins the height, `atLeast` is a minimum, and `auto` **ignores `val`**
/// — the row sizes to its content and carries no constraint at all, hence the
/// `None`.
///
/// An omitted `hRule` never reaches here as `Auto`: the parse seam defaults it
/// to `AtLeast`, matching Word ([MS-OI29500] §17.4.80(a)) rather than the
/// standard's `auto`. That default is what makes returning `None` for `auto`
/// safe — otherwise every Word row with a `trHeight` would lose its minimum.
fn row_height_rule(
    h: model::TableRowHeight,
) -> Option<crate::render::layout::table::RowHeightRule> {
    use crate::model::HeightRule;
    use crate::render::layout::table::RowHeightRule;
    match h.rule {
        HeightRule::Exact => Some(RowHeightRule::Exact(Pt::from(h.value))),
        HeightRule::AtLeast => Some(RowHeightRule::AtLeast(Pt::from(h.value))),
        HeightRule::Auto => None,
    }
}

/// Resolve §17.4.43 table cell-margin defaults per side.
///
/// Direct `tblCellMar` wins over the associated table style. A structurally
/// absent side inherits, while an explicit `w:w="0"` remains zero. If neither
/// level specifies a side, Word's effective defaults are 0 twips top/bottom
/// and 108 twips start/end (0.075in, displayed as 0.08in in the UI).
fn resolve_table_cell_margins(
    direct: Option<crate::model::geometry::PartialEdgeInsets<crate::model::dimension::Twips>>,
    style: Option<crate::model::geometry::PartialEdgeInsets<crate::model::dimension::Twips>>,
) -> crate::model::geometry::EdgeInsets<crate::model::dimension::Twips> {
    use crate::model::dimension::Dimension;
    use crate::model::geometry::{EdgeInsets, PartialEdgeInsets};

    let format_default = EdgeInsets::new(
        Dimension::new(0),
        Dimension::new(108),
        Dimension::new(0),
        Dimension::new(108),
    );
    let cascaded = match (direct, style) {
        (Some(direct), Some(style)) => direct.inherit_missing_from(style),
        (Some(direct), None) => direct,
        (None, Some(style)) => style,
        (None, None) => PartialEdgeInsets::new(None, None, None, None),
    };
    cascaded.resolve_against(format_default)
}

/// §17.4.44: resolve `tblCellSpacing` to points.
///
/// `CT_TblWidth` allows `pct` and `auto`, and the spec says both **are ignored**
/// for this element — only `dxa` carries a usable value. `nil` and an omitted
/// element are zero, which is every table in the test corpora.
fn resolve_cell_spacing(m: Option<model::TableMeasure>) -> Pt {
    match m {
        Some(model::TableMeasure::Twips(tw)) => Pt::from(tw).max(Pt::ZERO),
        // Auto / Pct / Nil / absent.
        _ => Pt::ZERO,
    }
}

/// Carve one `cell_spacing` out of the grid so the slots plus one spacing add
/// up to the table's own width, scaling the columns proportionally.
///
/// Clamped: a spacing at least as large as the table would leave nothing to
/// scale, so the columns collapse to zero rather than going negative. Word
/// caps the usable spacing well below that, but the file format does not.
fn reserve_cell_spacing(col_widths: Vec<Pt>, cell_spacing: Pt) -> Vec<Pt> {
    if cell_spacing <= Pt::ZERO || col_widths.is_empty() {
        return col_widths;
    }
    let total: Pt = col_widths.iter().copied().sum();
    if total <= cell_spacing {
        return vec![Pt::ZERO; col_widths.len()];
    }
    let scale = (total - cell_spacing).raw() / total.raw();
    col_widths.into_iter().map(|w| w * scale).collect()
}

/// Replace a stale automatic grid only when every row supplies the same,
/// complete direct-width grid.  `tcW` is merely a preferred width, so an
/// isolated cell must not enlarge a column.  The narrow consensus below is the
/// interoperable signal produced by WPS/ONLYOFFICE when every `tblGrid` entry
/// is uniformly stale by one rounding/gutter amount:
///
/// * at least a 2x2 table;
/// * every row covers every grid column with unmerged `dxa` cells;
/// * each column repeats one identical preferred width in every row;
/// * every preferred width grows its grid column; and
/// * the per-column growth differs by at most one twip.
///
/// Validate into a temporary vector first so a malformed row can never leave a
/// partially widened grid behind.
fn apply_consensus_cell_width_grid(col_widths: &mut [Pt], rows: &[model::TableRow]) -> bool {
    if col_widths.len() < 2 || rows.len() < 2 {
        return false;
    }

    let mut preferred_twips = vec![None; col_widths.len()];
    for row in rows {
        if row.properties.grid_before != 0
            || row.properties.grid_after != 0
            || row.cells.len() != col_widths.len()
        {
            return false;
        }
        for (grid_col, cell) in row.cells.iter().enumerate() {
            if cell.properties.grid_span.unwrap_or(1) != 1 {
                return false;
            }
            let Some(model::TableMeasure::Twips(width)) = cell.properties.width else {
                return false;
            };
            if width.raw() <= 0 {
                return false;
            }
            match preferred_twips[grid_col] {
                Some(previous) if previous != width.raw() => return false,
                Some(_) => {}
                None => preferred_twips[grid_col] = Some(width.raw()),
            }
        }
    }

    let preferred: Vec<Pt> = preferred_twips
        .into_iter()
        .map(|width| Pt::new(width.expect("all columns were visited") as f32 / 20.0))
        .collect();
    let mut min_growth = f32::INFINITY;
    let mut max_growth = f32::NEG_INFINITY;
    for (grid, preferred) in col_widths.iter().zip(&preferred) {
        if *preferred <= *grid {
            return false;
        }
        let growth = (*preferred - *grid).raw();
        min_growth = min_growth.min(growth);
        max_growth = max_growth.max(growth);
    }
    if max_growth - min_growth > 0.050_1 {
        return false;
    }

    col_widths.copy_from_slice(&preferred);
    true
}

fn resolve_table_indent(
    explicit_indent: Option<model::TableMeasure>,
    alignment: Option<model::Alignment>,
    width: Option<model::TableMeasure>,
    auto_tcw_widened: bool,
    default_cell_left: crate::model::dimension::Dimension<crate::model::dimension::Twips>,
) -> Pt {
    if let Some(model::TableMeasure::Twips(tw)) = explicit_indent {
        return Pt::from(tw);
    }

    let is_left_aligned = !matches!(
        alignment,
        Some(model::Alignment::Center) | Some(model::Alignment::End)
    );
    let is_full_width = matches!(
        width,
        Some(model::TableMeasure::Pct(pct)) if pct.raw() >= 5000
    );
    if is_left_aligned && (is_full_width || auto_tcw_widened) {
        -Pt::from(default_cell_left)
    } else {
        Pt::ZERO
    }
}

/// Recursively build a table: resolve styles, conditional formatting, and
/// recurse into each cell's content blocks.
pub(super) fn build_table(
    t: &Table,
    available_width: Pt,
    ctx: &BuildContext,
    state: &mut BuildState,
) -> BuiltTable {
    // §17.4.14: grid column widths.
    let num_cols = if t.grid.is_empty() {
        t.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0)
    } else {
        t.grid.len()
    };
    let grid_cols: Vec<Pt> = t.grid.iter().map(|g| Pt::from(g.width)).collect();

    // §17.7.6: table style for conditional formatting, borders, cell margins.
    // §17.4.63: when `w:tblStyle` is absent or unresolved, no table style is
    // applied. The stylesheet's `w:default="1"` table style is not an implicit
    // substitute for a missing `w:tblStyle`.
    let raw_table_style = t
        .properties
        .style_id
        .as_ref()
        .and_then(|sid| ctx.resolved.styles.get(sid));

    // §17.4.42: default cell margins from table style cascade.
    let style_cell_margins = raw_table_style
        .and_then(|s| s.table.as_ref())
        .and_then(|tp| tp.cell_margins);
    let default_cell_margins =
        resolve_table_cell_margins(t.properties.cell_margins, style_cell_margins);

    // §17.4.63: resolve table width from tblW.
    let is_auto_width = matches!(
        t.properties.width,
        None | Some(model::TableMeasure::Auto) | Some(model::TableMeasure::Nil)
    );
    let cell_margins_h = Pt::from(default_cell_margins.left) + Pt::from(default_cell_margins.right);
    // §17.4.63 / Word heuristic: a full-width left-aligned table extends
    // beyond the body content area by its cell margins so cell content
    // aligns with surrounding paragraph text. Centered/right-aligned tables
    // are positioned as a unit — extending them would just shift them out
    // by half the margins, producing a width discrepancy when consecutive
    // centered tables have different cell margins (cf. the stacked
    // "Anhang: Sauberkeit" tables in the Volvo Annahme-Protokoll).
    let extends_for_alignment = !matches!(
        t.properties.alignment,
        Some(model::Alignment::Center) | Some(model::Alignment::End)
    );
    let target_width = match t.properties.width {
        Some(model::TableMeasure::Pct(pct)) => {
            // §17.4.63: percentage in fiftieths of a percent. 5000 = 100%.
            let ratio = pct.raw() as f32 / 5000.0;
            let base = if pct.raw() >= 5000 && extends_for_alignment {
                available_width + cell_margins_h
            } else {
                available_width
            };
            base * ratio
        }
        Some(model::TableMeasure::Twips(tw)) => Pt::from(tw),
        _ => available_width, // auto/nil: use grid cols or available width
    };
    // §17.4.53: tblLayout controls whether columns may auto-resize to fit
    // content; it does not override the preferred table width from tblW.
    // Word scales grid column widths proportionally to match tblW in both
    // fixed and auto layouts. Only when tblW is auto/nil do we keep the raw
    // grid widths (no preferred width was specified).
    let (col_widths, auto_tcw_widened) = if is_auto_width && !grid_cols.is_empty() {
        let mut widths = grid_cols.clone();
        let widened = apply_consensus_cell_width_grid(&mut widths, &t.rows);
        (widths, widened)
    } else {
        (
            compute_column_widths(&grid_cols, num_cols, target_width),
            false,
        )
    };
    // §17.4.44: cell spacing is carved out of the table's own width rather than
    // added to it — the spec calls it "the minimum amount of space which shall
    // be left between all cells", not extra width. Reserving one spacing here
    // and offsetting each cell by one in `measure_table_rows` yields exactly
    // `cell_spacing` between adjacent cells *and* at both table edges.
    let cell_spacing = resolve_cell_spacing(t.properties.cell_spacing);
    let col_widths = reserve_cell_spacing(col_widths, cell_spacing);
    let style_overrides = raw_table_style
        .map(|s| s.table_style_overrides.as_slice())
        .unwrap_or(&[]);
    let tbl_look = t.properties.look.as_ref();
    let row_band_size = t.properties.style_row_band_size.unwrap_or(1);
    let col_band_size = t.properties.style_col_band_size.unwrap_or(1);
    let num_rows = t.rows.len();

    // §17.4.38: resolve table borders — merge direct properties over table style.
    // Direct tblBorders may specify only a subset of edges (e.g. insideH=none);
    // unspecified edges inherit from the table style. Computed up front so
    // per-row tblPrEx merges (§17.4.61) below have a stable basis.
    let style_borders = raw_table_style
        .and_then(|s| s.table.as_ref())
        .and_then(|tp| tp.borders.as_ref());
    let tbl_borders = match (t.properties.borders.as_ref(), style_borders) {
        (Some(direct), Some(style)) => Some(merge_table_borders(direct, style)),
        (Some(direct), None) => Some(*direct),
        (None, Some(style)) => Some(*style),
        (None, None) => None,
    };
    let border_config = tbl_borders
        .as_ref()
        .map(|b| convert_table_border_config(b, state));

    // Build rows by iterating cells and recursing into their content.
    let rows: Vec<TableRowInput> = t
        .rows
        .iter()
        .enumerate()
        .map(|(row_idx, row)| {
            let num_cells = row.cells.len();
            let cells: Vec<TableCellInput> = row
                .cells
                .iter()
                .enumerate()
                .map(|(col_idx, cell)| {
                    let cond = resolve_cell_conditional(
                        &CellGridPosition {
                            row_idx,
                            col_idx,
                            num_rows,
                            num_cols: num_cells,
                            row_band_size,
                            col_band_size,
                        },
                        tbl_look,
                        style_overrides,
                    );

                    // Compute available width for nested content.
                    // §17.4.17: gridBefore offsets the row's first cell to the
                    // right by that many grid columns; subsequent spans accumulate.
                    let span = cell.properties.grid_span.unwrap_or(1) as usize;
                    let mut grid_start = row.properties.grid_before as usize;
                    for ci in 0..col_idx {
                        grid_start += row.cells[ci].properties.grid_span.unwrap_or(1) as usize;
                    }
                    // A row may address more grid columns than `tblGrid` declares —
                    // a `gridBefore` past the end, or simply more `<w:tc>` than
                    // `<w:gridCol>`. Both occur in real producer output and Word
                    // recovers from them. Clamp *both* ends: clamping only the end
                    // inverts the range and panics on the slice.
                    let grid_start = grid_start.min(col_widths.len());
                    let grid_end = (grid_start + span).min(col_widths.len());
                    let cell_width: Pt = col_widths[grid_start..grid_end].iter().copied().sum();
                    // Per-side cascade against the table default (see
                    // `build_table_cell` for the spec rationale): the horizontal
                    // padding contribution is the resolved left+right insets.
                    let table_default = default_cell_margins;
                    let resolved_h = match cell.properties.margins {
                        Some(partial) => partial.resolve_against(table_default),
                        None => table_default,
                    };
                    let cell_margins_h = Pt::from(resolved_h.left) + Pt::from(resolved_h.right);
                    let inner_width = (cell_width - cell_margins_h).max(Pt::ZERO);

                    build_table_cell(
                        cell,
                        raw_table_style,
                        default_cell_margins,
                        &cond,
                        inner_width,
                        ctx,
                        state,
                    )
                })
                .collect();

            // Word/LibreOffice row-uniform content-area quirk — see
            // `normalize_row_uniform_vertical_insets` for the spec gap and
            // empirical evidence motivating this pass.
            let mut cells = cells;
            normalize_row_uniform_vertical_insets(&mut cells);

            // §17.4.41 / §17.4.42: a row (or its `tblPrEx`) may override the
            // table's `tblCellSpacing`. Layout applies spacing per *table* —
            // the grid slots are shrunk once, up front — so a per-row value
            // cannot be honoured without a per-row grid. Report it rather than
            // dropping it silently; the parsed value stays on the model.
            let row_spacing = row.properties.cell_spacing.or_else(|| {
                row.property_exceptions
                    .as_ref()
                    .and_then(|e| e.cell_spacing)
            });
            if let Some(rs) = row_spacing {
                if resolve_cell_spacing(Some(rs)) != cell_spacing && !state.warned_row_cell_spacing
                {
                    state.warned_row_cell_spacing = true;
                    log::warn!(
                        "§17.4.41/§17.4.42: row-level tblCellSpacing overrides are not applied; \
                         using the table-level value ({cell_spacing:?})"
                    );
                }
            }

            TableRowInput {
                cells,
                // §17.4.81: `exact` pins the height, `atLeast` is a minimum,
                // and `auto` **ignores `val`** — the row sizes to its content,
                // so it carries no constraint at all. An omitted `hRule`
                // arrives as `AtLeast` (Word's default, set at the parse seam),
                // which is why folding `auto` in with it used to be invisible:
                // [MS-OE376] §2.4.77(c) notes Word requires `val = 0` whenever
                // `hRule="auto"`, making `AtLeast(0)` a no-op on Word output.
                // Other producers are not bound by that.
                height_rule: row.properties.height.and_then(row_height_rule),
                is_header: row.properties.is_header,
                cant_split: row.properties.cant_split,
                grid_before: row.properties.grid_before,
                // §17.4.61: row-level tblPrEx.tblBorders — per-side
                // override of the table's effective borders. We merge
                // *at the model layer* (Option<Border> with style=None
                // is preserved), then convert to layout — that keeps
                // the spec's "specified as none" vs "not specified"
                // distinction that converting first would erase.
                border_overrides: row
                    .property_exceptions
                    .as_ref()
                    .and_then(|ex| ex.borders.as_ref())
                    .map(|over| {
                        let merged = match tbl_borders.as_ref() {
                            Some(table) => merge_table_borders(over, table),
                            None => *over,
                        };
                        convert_table_border_config(&merged, state)
                    }),
            }
        })
        .collect();

    // §17.4.58: floating table positioning.
    let float_info = t.properties.positioning.as_ref().map(|pos| {
        super::super::section::TableFloatInfo {
            right_gap: pos.right_from_text.map(Pt::from).unwrap_or(Pt::ZERO),
            bottom_gap: pos.bottom_from_text.map(Pt::from).unwrap_or(Pt::ZERO),
            x_align: pos.x_align,
            // §17.4.58: tblpY — absolute Y offset from the vertical anchor.
            y_offset: pos.y.map(Pt::from).unwrap_or(Pt::ZERO),
            // §17.4.58: default vertical anchor is "text".
            vert_anchor: pos.vert_anchor.unwrap_or(crate::model::TableAnchor::Text),
            // §17.4.57: tblOverlap controls collision behavior with
            // other floats on the same page.
            overlap: t.properties.overlap,
        }
    });

    // §17.4.51: table indentation from left margin. Full-width tables and
    // automatic tables whose stale grid was replaced by a complete repeated
    // cell-width consensus extend left by the default cell margin so their
    // *content* remains aligned with surrounding paragraphs.
    let indent = resolve_table_indent(
        t.properties.indent,
        t.properties.alignment,
        t.properties.width,
        auto_tcw_widened,
        default_cell_margins.left,
    );

    BuiltTable {
        rows,
        col_widths,
        cell_spacing,
        border_config,
        indent,
        alignment: t.properties.alignment,
        float_info,
    }
}

/// Word/LibreOffice row layout quirk: top/bottom cell margins are normalized
/// to the row-wide maximum across all cells in the row, while left/right stay
/// per-cell.
///
/// # Spec relationship
///
/// ECMA-376 §17.4.42 (`tcMar`, "Single Table Cell Margins") defines the cell
/// margin as a per-side exception over §17.4.44 (`tblCellMar`, the table-level
/// default). Each side is a `CT_TblWidth` (§17.18.87), where `@type="dxa"
/// @w="N"` is an explicit `N`-twip value. The spec is silent on how
/// *neighbouring* cells in the same row interact when their per-cell margins
/// disagree — there is no row-level "content area" concept defined in
/// §17.4.78 (`tr`) or §17.4.79 (`trHeight`).
///
/// The de-facto behaviour of every mainstream renderer (Word and LibreOffice
/// Writer in particular) is to compute a row-uniform content inset:
///
/// ```text
/// row.uniform_top    = max(cell.tcMar.top    for cell in row)
/// row.uniform_bottom = max(cell.tcMar.bottom for cell in row)
/// ```
///
/// and to position every cell's content within that uniform inset regardless
/// of the cell's own per-cell override. Without this pass, a cell with an
/// explicit `tcMar.top=0` in a row whose siblings inherit a larger value sits
/// flush against the row's top border while its neighbours sit padded — a
/// positioning no mainstream editor produces.
///
/// # Empirical basis
///
/// Verified against the `Wohnungsübergabeprotokoll` sample (LibreOffice
/// origin) by editing the DOCX directly and observing Word's render:
///
/// 1. Rewriting all `<w:tcMar><w:top w:w="0"/></w:tcMar>` to
///    `<w:top w:w="1"/>` produced **byte-equivalent visual output** — Word
///    is invariant to the literal value at this magnitude, ruling out
///    "Word treats `w=0` as no-override" as the explanation.
/// 2. Rewriting just one cell's `<w:tcMar>` to `<w:top w:w="500"/>` (≈ 25pt)
///    pushed **every** cell in that row down by ~25pt, confirming the
///    row-wide max(...) discipline.
///
/// # Scope
///
/// Only `top` and `bottom` are normalized. `left` and `right` stay per-cell
/// because each column's content width is independent — there is no shared
/// row-wide horizontal area in the de-facto layout, and editors do honour
/// per-cell horizontal overrides.
///
/// Applied at the build layer (post per-side `<w:tcMar>`/`<w:tblCellMar>`
/// cascade resolution) so downstream measure/emit/split code sees uniform
/// vertical insets and needs no further changes.
fn normalize_row_uniform_vertical_insets(cells: &mut [TableCellInput]) {
    let max_top = cells.iter().fold(Pt::ZERO, |acc, c| acc.max(c.margins.top));
    let max_bottom = cells
        .iter()
        .fold(Pt::ZERO, |acc, c| acc.max(c.margins.bottom));
    for cell in cells {
        cell.margins.top = max_top;
        cell.margins.bottom = max_bottom;
    }
}

fn cell_anchor_origin_enabled(text_direction: Option<model::TextDirection>) -> bool {
    // Explicit normal-flow lrTb uses the same physical-left origin as the
    // absent/default direction. All vertical/rotated directions remain on
    // the legacy path: those cells lay out against logical margins while an
    // anchor dx is expressed from a physical edge.
    matches!(
        text_direction,
        None | Some(model::TextDirection::LeftToRightTopToBottom)
    )
}

/// Build a single table cell: resolve content blocks, margins, shading, borders.
fn build_table_cell(
    cell: &TableCell,
    table_style: Option<&ResolvedStyle>,
    table_default_margins: crate::model::geometry::EdgeInsets<crate::model::dimension::Twips>,
    cond: &CellConditionalFormatting,
    inner_width: Pt,
    ctx: &BuildContext,
    state: &mut BuildState,
) -> TableCellInput {
    // §17.4.42: cell margins cascade *per side* against the pre-merged
    // table default. A cell-level `<w:tcMar>` that specifies only some sides
    // (e.g. `top`/`bottom` only — common in LibreOffice output) must inherit
    // the remaining sides from `<w:tblCellMar>` rather than zeroing them out;
    // collapsing missing sides to 0 produces text that hugs the cell borders
    // instead of carrying the table's intended padding.
    let table_default = table_default_margins;
    let resolved_margins = match cell.properties.margins {
        Some(partial) => partial.resolve_against(table_default),
        None => table_default,
    };
    let cell_margins = geometry::PtEdgeInsets::new(
        Pt::from(resolved_margins.top),
        Pt::from(resolved_margins.right),
        Pt::from(resolved_margins.bottom),
        Pt::from(resolved_margins.left),
    );

    // §17.7.6: resolve cell shading.  Priority: direct → conditional → none.
    let shading = cell
        .properties
        .shading
        .map(|s| resolve_color(s.fill, ColorContext::Background))
        .or_else(|| {
            cond.cell_properties
                .as_ref()
                .and_then(|tcp| tcp.shading.as_ref())
                .map(|s| resolve_color(s.fill, ColorContext::Background))
        });

    // §17.4.66: cell borders cascade — direct cell borders (highest priority)
    // → conditional formatting → table-level borders (resolved in layout).
    let cond_borders = cond
        .cell_properties
        .as_ref()
        .and_then(|tcp| tcp.borders.as_ref());
    let direct_borders = cell.properties.borders.as_ref();

    let cell_borders = match (direct_borders, cond_borders) {
        (Some(db), _) => {
            // Direct cell borders: highest priority.  Fall through to
            // conditional for edges not specified directly.
            Some(CellBorderConfig {
                top: convert_cell_border_override(&db.top, state).or_else(|| {
                    cond_borders.and_then(|cb| convert_cell_border_override(&cb.top, state))
                }),
                bottom: convert_cell_border_override(&db.bottom, state).or_else(|| {
                    cond_borders.and_then(|cb| convert_cell_border_override(&cb.bottom, state))
                }),
                left: convert_cell_border_override(&db.left, state).or_else(|| {
                    cond_borders.and_then(|cb| convert_cell_border_override(&cb.left, state))
                }),
                right: convert_cell_border_override(&db.right, state).or_else(|| {
                    cond_borders.and_then(|cb| convert_cell_border_override(&cb.right, state))
                }),
            })
        }
        (None, Some(cb)) => Some(CellBorderConfig {
            top: convert_cell_border_override(&cb.top, state),
            bottom: convert_cell_border_override(&cb.bottom, state),
            left: convert_cell_border_override(&cb.left, state),
            right: convert_cell_border_override(&cb.right, state),
        }),
        (None, None) => None,
    };

    // §17.4.84: vertical alignment — direct cell, conditional, or default top.
    let valign = cell
        .properties
        .vertical_align
        .or_else(|| {
            cond.cell_properties
                .as_ref()
                .and_then(|tcp| tcp.vertical_align)
        })
        .map(|va| match va {
            model::CellVerticalAlign::Bottom => crate::render::layout::table::CellVAlign::Bottom,
            model::CellVerticalAlign::Center => crate::render::layout::table::CellVAlign::Center,
            _ => crate::render::layout::table::CellVAlign::Top,
        })
        .unwrap_or(crate::render::layout::table::CellVAlign::Top);

    // §17.4.70: direct cell text direction wins, followed by conditional
    // table-style formatting. Omission keeps the normal section flow.
    let text_direction = cell.properties.text_direction.or_else(|| {
        cond.cell_properties
            .as_ref()
            .and_then(|tcp| tcp.text_direction)
    });

    // Estimate border insets to compute effective content width for
    // character-level splitting of oversized fragments.
    let border_w = |ovr: &Option<CellBorderOverride>| -> Pt {
        match ovr {
            Some(CellBorderOverride::Border(b)) => b.width,
            _ => Pt::ZERO,
        }
    };
    let border_inset_h = cell_borders
        .as_ref()
        .map(|cb| {
            let bl = (border_w(&cb.left) - cell_margins.left).max(Pt::ZERO);
            let br = (border_w(&cb.right) - cell_margins.right).max(Pt::ZERO);
            bl + br
        })
        .unwrap_or(Pt::ZERO);
    let content_width = (inner_width - border_inset_h).max(Pt::ZERO);

    // Recurse into cell content blocks.
    let cell_blocks = build_cell_blocks(
        &cell.content,
        table_style,
        cond,
        content_width,
        cell_anchor_origin_enabled(text_direction),
        ctx,
        state,
    );

    TableCellInput {
        blocks: cell_blocks,
        margins: cell_margins,
        grid_span: cell.properties.grid_span.unwrap_or(1),
        shading,
        cell_borders,
        vertical_merge: cell.properties.vertical_merge.map(|vm| match vm {
            model::VerticalMerge::Restart => {
                crate::render::layout::table::VerticalMergeState::Restart
            }
            model::VerticalMerge::Continue => {
                crate::render::layout::table::VerticalMergeState::Continue
            }
        }),
        vertical_align: valign,
        text_direction,
    }
}

/// Recursively build cell content blocks.
///
/// Paragraphs are resolved with table style + conditional overrides.
/// Nested tables recurse via `build_table()` → `layout_table()`.
fn build_cell_blocks(
    content: &[Block],
    table_style: Option<&ResolvedStyle>,
    cond: &CellConditionalFormatting,
    inner_width: Pt,
    cell_anchor_origin_enabled: bool,
    ctx: &BuildContext,
    state: &mut BuildState,
) -> Vec<LayoutBlock> {
    let mut blocks = Vec::new();
    let mut pending_dropcap: Option<DropCapInfo> = None;

    for (i, block) in content.iter().enumerate() {
        match block {
            Block::Paragraph(p) => {
                // §17.4.66: every cell must end with a paragraph. When the
                // last block is an empty paragraph following a table, it is
                // structural — Word renders it with zero height.
                if p.content.is_empty()
                    && i > 0
                    && matches!(content[i - 1], Block::Table(_))
                    && i == content.len() - 1
                {
                    continue;
                }
                if let Some(lb) = build_paragraph_block(
                    p,
                    ctx,
                    state,
                    &mut pending_dropcap,
                    table_style,
                    Some(cond),
                    cell_anchor_origin_enabled,
                ) {
                    // §17.3.1.45: character-level breaking is opt-in for
                    // space-delimited words. East Asian text is already split
                    // at legal boundaries during fragment construction.
                    let lb = if let LayoutBlock::Paragraph {
                        fragments,
                        style,
                        page_break_before,
                        footnotes,
                        floating_images,
                        floating_shapes,
                    } = lb
                    {
                        // Keep the owned vector when no split is needed.
                        let measure = |t: &str, f: &crate::render::layout::fragment::FontProps| {
                            ctx.measurer.measure(t, f)
                        };
                        let fragments = split_oversized_fragments_for_word_wrap(
                            &fragments,
                            inner_width,
                            Some(&measure),
                            style.word_wrap,
                        )
                        .unwrap_or(fragments);
                        LayoutBlock::Paragraph {
                            fragments,
                            style,
                            page_break_before,
                            footnotes,
                            floating_images,
                            floating_shapes,
                        }
                    } else {
                        lb
                    };
                    blocks.push(lb);
                }
            }
            Block::Table(nested_t) => {
                let built = build_table(nested_t, inner_width, ctx, state);
                blocks.push(LayoutBlock::Table {
                    rows: built.rows,
                    col_widths: built.col_widths,
                    cell_spacing: built.cell_spacing,
                    border_config: built.border_config,
                    indent: built.indent,
                    alignment: built.alignment,
                    float_info: built.float_info,
                    style_id: nested_t.properties.style_id.clone(),
                });
            }
            _ => {}
        }
    }

    suppress_auto_spacing_at_cell_edges(&mut blocks);
    blocks
}

/// §17.3.1.33: automatic paragraph spacing in a table cell applies only at a
/// boundary with a neighbouring paragraph. A cell edge is not such a boundary,
/// so Word suppresses automatic spacing before the first paragraph and after
/// the last paragraph in the cell.
///
/// Only the actual first/last layout block qualifies. In particular, do not
/// search past a nested table for a paragraph: that would turn spacing on the
/// other side of the nested table into cell-edge spacing. Keep the auto flags
/// themselves so internal paragraph boundaries retain the ordinary automatic
/// value and its contextual/list handling.
fn suppress_auto_spacing_at_cell_edges(blocks: &mut [LayoutBlock]) {
    if let Some(LayoutBlock::Paragraph { style, .. }) = blocks.first_mut() {
        if style.before_auto_spacing {
            style.space_before = Pt::ZERO;
        }
    }

    if let Some(LayoutBlock::Paragraph { style, .. }) = blocks.last_mut() {
        if style.after_auto_spacing {
            style.space_after = Pt::ZERO;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::dimension::Dimension;
    use crate::model::geometry::PartialEdgeInsets;
    use crate::render::layout::paragraph::ParagraphStyle;
    use crate::render::layout::table::{CellVAlign, TableCellInput};

    #[test]
    fn only_default_and_explicit_normal_flow_use_the_cell_anchor_origin_path() {
        assert!(cell_anchor_origin_enabled(None));
        assert!(cell_anchor_origin_enabled(Some(
            model::TextDirection::LeftToRightTopToBottom
        )));
        for direction in [
            model::TextDirection::TopToBottomRightToLeft,
            model::TextDirection::BottomToTopLeftToRight,
            model::TextDirection::LeftToRightTopToBottomRotated,
            model::TextDirection::TopToBottomRightToLeftRotated,
            model::TextDirection::TopToBottomLeftToRightRotated,
        ] {
            assert!(!cell_anchor_origin_enabled(Some(direction)));
        }
    }

    fn paragraph_with_spacing(
        before: f32,
        after: f32,
        before_auto: bool,
        after_auto: bool,
    ) -> LayoutBlock {
        LayoutBlock::Paragraph {
            fragments: vec![],
            style: ParagraphStyle {
                space_before: Pt::new(before),
                space_after: Pt::new(after),
                before_auto_spacing: before_auto,
                after_auto_spacing: after_auto,
                ..ParagraphStyle::default()
            },
            page_break_before: false,
            footnotes: vec![],
            floating_images: vec![],
            floating_shapes: vec![],
        }
    }

    fn nested_table_block() -> LayoutBlock {
        LayoutBlock::Table {
            rows: vec![],
            col_widths: vec![],
            cell_spacing: Pt::ZERO,
            border_config: None,
            indent: Pt::ZERO,
            alignment: None,
            float_info: None,
            style_id: None,
        }
    }

    fn width_cell(width_twips: i64, span: u32) -> model::TableCell {
        model::TableCell {
            properties: model::TableCellProperties {
                width: Some(model::TableMeasure::Twips(Dimension::new(width_twips))),
                grid_span: Some(span),
                ..Default::default()
            },
            content: Vec::new(),
        }
    }

    fn width_row(grid_before: u32, cells: Vec<model::TableCell>) -> model::TableRow {
        model::TableRow {
            properties: model::TableRowProperties {
                grid_before,
                ..Default::default()
            },
            cells,
            rsids: Default::default(),
            property_exceptions: None,
        }
    }

    #[test]
    fn auto_grid_uses_a_complete_repeated_consensus_cell_grid() {
        let rows = vec![
            width_row(0, vec![width_cell(2_438, 1), width_cell(2_438, 1)]),
            width_row(0, vec![width_cell(2_438, 1), width_cell(2_438, 1)]),
        ];
        let mut widths = vec![Pt::new(102.25), Pt::new(102.30)];

        assert!(apply_consensus_cell_width_grid(&mut widths, &rows));

        assert_eq!(widths, vec![Pt::new(121.9), Pt::new(121.9)]);
    }

    #[test]
    fn incomplete_or_spanned_cell_widths_do_not_change_the_grid() {
        let rows = vec![
            width_row(0, vec![width_cell(6_000, 2)]),
            width_row(0, vec![width_cell(6_000, 2)]),
        ];
        let original = vec![Pt::new(100.0), Pt::new(60.0), Pt::new(40.0)];
        let mut widths = original.clone();

        assert!(!apply_consensus_cell_width_grid(&mut widths, &rows));

        assert_eq!(widths, original);
    }

    #[test]
    fn partial_or_nonuniform_cell_preferences_leave_the_grid_atomic() {
        let cases = [
            // One preferred column is smaller than its grid (en051 pattern).
            vec![
                width_row(0, vec![width_cell(2_677, 1), width_cell(2_057, 1)]),
                width_row(0, vec![width_cell(2_677, 1), width_cell(2_057, 1)]),
            ],
            // Both grow, but by materially different amounts (en029 pattern).
            vec![
                width_row(0, vec![width_cell(1_596, 1), width_cell(1_596, 1)]),
                width_row(0, vec![width_cell(1_596, 1), width_cell(1_596, 1)]),
            ],
        ];
        let grids = [
            vec![Pt::new(127.1), Pt::new(119.1)],
            vec![Pt::new(78.45), Pt::new(77.95)],
        ];

        for (rows, original) in cases.into_iter().zip(grids) {
            let mut widths = original.clone();
            assert!(!apply_consensus_cell_width_grid(&mut widths, &rows));
            assert_eq!(widths, original, "failure must not partially widen columns");
        }
    }

    #[test]
    fn a_single_row_or_single_column_is_not_a_consensus_grid() {
        let mut two_columns = vec![Pt::new(100.0), Pt::new(100.0)];
        assert!(!apply_consensus_cell_width_grid(
            &mut two_columns,
            &[width_row(
                0,
                vec![width_cell(2_100, 1), width_cell(2_100, 1)]
            )]
        ));

        let mut one_column = vec![Pt::new(100.0)];
        assert!(!apply_consensus_cell_width_grid(
            &mut one_column,
            &[
                width_row(0, vec![width_cell(2_100, 1)]),
                width_row(0, vec![width_cell(2_100, 1)])
            ]
        ));
    }

    #[test]
    fn widened_auto_left_table_outdents_by_the_default_cell_margin() {
        assert_eq!(
            resolve_table_indent(
                None,
                None,
                Some(model::TableMeasure::Auto),
                true,
                Dimension::new(108),
            ),
            Pt::new(-5.4)
        );
        assert_eq!(
            resolve_table_indent(
                None,
                Some(model::Alignment::Center),
                Some(model::TableMeasure::Auto),
                true,
                Dimension::new(108),
            ),
            Pt::ZERO,
            "centered tables remain positioned as a unit"
        );
        assert_eq!(
            resolve_table_indent(
                None,
                None,
                Some(model::TableMeasure::Auto),
                false,
                Dimension::new(108),
            ),
            Pt::ZERO,
            "an unchanged automatic grid keeps its historical origin"
        );
    }

    #[test]
    fn explicit_table_indent_wins_over_auto_grid_outdent() {
        assert_eq!(
            resolve_table_indent(
                Some(model::TableMeasure::Twips(Dimension::new(240))),
                None,
                Some(model::TableMeasure::Auto),
                true,
                Dimension::new(108),
            ),
            Pt::new(12.0)
        );
    }

    fn paragraph_style(block: &LayoutBlock) -> &ParagraphStyle {
        let LayoutBlock::Paragraph { style, .. } = block else {
            panic!("expected paragraph block")
        };
        style
    }

    #[test]
    fn cell_auto_spacing_suppresses_only_the_outer_sides() {
        let mut blocks = vec![
            paragraph_with_spacing(14.0, 14.0, true, true),
            paragraph_with_spacing(14.0, 14.0, true, true),
            paragraph_with_spacing(14.0, 14.0, true, true),
        ];

        suppress_auto_spacing_at_cell_edges(&mut blocks);

        let first = paragraph_style(&blocks[0]);
        let middle = paragraph_style(&blocks[1]);
        let last = paragraph_style(&blocks[2]);
        assert_eq!(
            (first.space_before, first.space_after),
            (Pt::ZERO, Pt::new(14.0))
        );
        assert_eq!(
            (middle.space_before, middle.space_after),
            (Pt::new(14.0), Pt::new(14.0)),
            "internal automatic spacing remains intact"
        );
        assert_eq!(
            (last.space_before, last.space_after),
            (Pt::new(14.0), Pt::ZERO)
        );
        assert!(
            [first, middle, last]
                .into_iter()
                .all(|style| style.before_auto_spacing && style.after_auto_spacing),
            "edge suppression must preserve the auto flags"
        );
    }

    #[test]
    fn single_paragraph_cell_suppresses_both_auto_edges() {
        let mut blocks = vec![paragraph_with_spacing(14.0, 14.0, true, true)];

        suppress_auto_spacing_at_cell_edges(&mut blocks);

        let style = paragraph_style(&blocks[0]);
        assert_eq!(
            (style.space_before, style.space_after),
            (Pt::ZERO, Pt::ZERO)
        );
        assert!(style.before_auto_spacing && style.after_auto_spacing);
    }

    #[test]
    fn explicit_cell_edge_spacing_is_not_suppressed() {
        let mut blocks = vec![paragraph_with_spacing(6.0, 8.0, false, false)];

        suppress_auto_spacing_at_cell_edges(&mut blocks);

        let style = paragraph_style(&blocks[0]);
        assert_eq!(
            (style.space_before, style.space_after),
            (Pt::new(6.0), Pt::new(8.0))
        );
    }

    #[test]
    fn nested_tables_stop_cell_edge_paragraph_search() {
        let mut blocks = vec![
            nested_table_block(),
            paragraph_with_spacing(14.0, 14.0, true, true),
            nested_table_block(),
        ];

        suppress_auto_spacing_at_cell_edges(&mut blocks);

        let style = paragraph_style(&blocks[1]);
        assert_eq!(
            (style.space_before, style.space_after),
            (Pt::new(14.0), Pt::new(14.0)),
            "a paragraph across a nested table is not at the cell edge"
        );
    }

    #[test]
    fn table_cell_margin_word_defaults_are_horizontal_108_twips() {
        let margins = resolve_table_cell_margins(None, None);
        assert_eq!(margins.top.raw(), 0);
        assert_eq!(margins.right.raw(), 108);
        assert_eq!(margins.bottom.raw(), 0);
        assert_eq!(margins.left.raw(), 108);
    }

    #[test]
    fn direct_table_cell_margin_zero_overrides_style_per_side() {
        let direct =
            PartialEdgeInsets::new(None, Some(Dimension::new(0)), None, Some(Dimension::new(0)));
        let style = PartialEdgeInsets::new(
            Some(Dimension::new(40)),
            Some(Dimension::new(115)),
            Some(Dimension::new(60)),
            Some(Dimension::new(115)),
        );
        let margins = resolve_table_cell_margins(Some(direct), Some(style));
        assert_eq!(margins.top.raw(), 40, "missing direct side inherits");
        assert_eq!(margins.right.raw(), 0, "explicit zero wins");
        assert_eq!(margins.bottom.raw(), 60, "missing direct side inherits");
        assert_eq!(margins.left.raw(), 0, "explicit zero wins");
    }

    fn cell_with_margins(top: f32, right: f32, bottom: f32, left: f32) -> TableCellInput {
        TableCellInput {
            blocks: vec![],
            margins: geometry::PtEdgeInsets::new(
                Pt::new(top),
                Pt::new(right),
                Pt::new(bottom),
                Pt::new(left),
            ),
            grid_span: 1,
            shading: None,
            cell_borders: None,
            vertical_merge: None,
            vertical_align: CellVAlign::Top,
            text_direction: None,
        }
    }

    /// Word/Writer row-uniform content-area normalization: when cells in a row
    /// disagree on `tcMar.top` (or `bottom`), the row-wide *maximum* wins for
    /// every cell. Verified empirically against MS Word — see
    /// [`normalize_row_uniform_vertical_insets`] for the experimental
    /// reproducer and spec references.
    #[test]
    fn row_uniform_picks_max_top_and_bottom_across_row() {
        // Mirrors the Wohnungsübergabe "Keller" row: one cell with
        // explicit zero top/bottom (the Keller cell after per-side tcMar
        // cascade) and another with the inherited 57-twip table default
        // (≈ 2.85 pt) — siblings without a tcMar override.
        let mut cells = vec![
            cell_with_margins(0.0, 5.4, 0.0, 5.15),   // Keller-like
            cell_with_margins(2.85, 5.4, 2.85, 5.15), // sibling with table default
            cell_with_margins(0.0, 5.4, 0.0, 5.15),   // another Keller-like
        ];
        normalize_row_uniform_vertical_insets(&mut cells);

        for (i, cell) in cells.iter().enumerate() {
            assert_eq!(
                cell.margins.top.raw(),
                2.85,
                "cell #{i}: every cell's top inset must equal the row-wide max"
            );
            assert_eq!(
                cell.margins.bottom.raw(),
                2.85,
                "cell #{i}: every cell's bottom inset must equal the row-wide max"
            );
            // Horizontal stays per-cell.
            assert_eq!(cell.margins.left.raw(), 5.15, "left is per-cell");
            assert_eq!(cell.margins.right.raw(), 5.4, "right is per-cell");
        }
    }

    /// Mirrors the `_kellertop500` experiment: a single cell with a `tcMar.top`
    /// far larger than its siblings becomes the row-wide max, so every cell —
    /// including the ones with no override — picks up that large top inset.
    /// In Word, this is observable as the entire row's content shifting down.
    #[test]
    fn row_uniform_one_large_cell_top_pushes_whole_row_down() {
        let mut cells = vec![
            cell_with_margins(25.0, 5.4, 0.0, 5.15), // Keller with top=25pt
            cell_with_margins(2.85, 5.4, 2.85, 5.15), // small sibling
        ];
        normalize_row_uniform_vertical_insets(&mut cells);

        assert_eq!(
            cells[1].margins.top.raw(),
            25.0,
            "sibling cell inherits the large top from the dominating cell"
        );
        // Bottom max is the smaller cell's 2.85 (Keller bottom stayed 0).
        assert_eq!(cells[0].margins.bottom.raw(), 2.85);
    }

    #[test]
    fn row_uniform_single_cell_row_is_noop() {
        let mut cells = vec![cell_with_margins(2.85, 5.4, 2.85, 5.15)];
        let before = cells[0].margins;
        normalize_row_uniform_vertical_insets(&mut cells);
        assert_eq!(cells[0].margins, before);
    }

    #[test]
    fn row_uniform_handles_empty_row() {
        let mut cells: Vec<TableCellInput> = vec![];
        normalize_row_uniform_vertical_insets(&mut cells);
        // No panic, no work done.
        assert!(cells.is_empty());
    }

    /// §17.4.44: `CT_TblWidth` permits `pct` and `auto`, and the spec says both
    /// are **ignored** for this element — only `dxa` carries a usable value.
    /// Honouring a percentage here would scale the gap with the table width,
    /// which is exactly what the spec rules out.
    #[test]
    fn cell_spacing_only_honours_dxa() {
        use crate::model::dimension::Dimension;
        assert_eq!(
            resolve_cell_spacing(Some(model::TableMeasure::Twips(Dimension::new(240)))),
            Pt::new(12.0),
            "240 twips = 12pt"
        );
        for ignored in [
            Some(model::TableMeasure::Pct(Dimension::new(2500))),
            Some(model::TableMeasure::Auto),
            Some(model::TableMeasure::Nil),
            None,
        ] {
            assert_eq!(resolve_cell_spacing(ignored), Pt::ZERO, "{ignored:?}");
        }
    }

    /// The spacing is carved *out of* the table, so the slots shrink by exactly
    /// one spacing in total and keep their proportions.
    #[test]
    fn reserving_spacing_shrinks_the_grid_by_exactly_one_spacing() {
        let widths = vec![Pt::new(60.0), Pt::new(40.0)];
        let out = reserve_cell_spacing(widths, Pt::new(10.0));
        let total: Pt = out.iter().copied().sum();
        assert_eq!(total, Pt::new(90.0), "one spacing removed in total");
        // Proportions preserved: 60:40 becomes 54:36.
        assert_eq!(out[0], Pt::new(54.0));
        assert_eq!(out[1], Pt::new(36.0));
    }

    /// Zero spacing must be a byte-for-byte no-op — every table that does not
    /// set `tblCellSpacing` goes through this path.
    #[test]
    fn reserving_zero_spacing_leaves_the_grid_untouched() {
        let widths = vec![Pt::new(60.0), Pt::new(40.0)];
        assert_eq!(reserve_cell_spacing(widths.clone(), Pt::ZERO), widths);
    }

    /// A spacing at least as wide as the table leaves nothing to distribute.
    /// Word caps the value long before this, but the file format does not, and
    /// scaling by a negative factor would put cells at negative widths.
    #[test]
    fn spacing_wider_than_the_table_collapses_columns_rather_than_going_negative() {
        let widths = vec![Pt::new(30.0), Pt::new(30.0)];
        let out = reserve_cell_spacing(widths, Pt::new(60.0));
        assert_eq!(out, vec![Pt::ZERO, Pt::ZERO]);
        assert!(out.iter().all(|w| *w >= Pt::ZERO));
    }

    /// §17.4.81: `hRule="auto"` ignores `val`, so the row carries **no** height
    /// constraint — it is not a zero-height minimum, and not a minimum of the
    /// stated value either.
    ///
    /// Word writes `val="0"` alongside `hRule="auto"` ([MS-OE376] §2.4.77(c)),
    /// which is why treating `auto` as `AtLeast(val)` was invisible on Word
    /// output: `AtLeast(0)` constrains nothing. Other producers are not bound by
    /// that, and a non-zero `val` under `auto` would have become a real minimum.
    #[test]
    fn auto_height_rule_carries_no_constraint() {
        use crate::model::{HeightRule, TableRowHeight};
        use crate::render::layout::table::RowHeightRule;
        let to_layout = |rule, twips: i64| -> Option<RowHeightRule> {
            row_height_rule(TableRowHeight {
                value: crate::model::dimension::Dimension::new(twips),
                rule,
            })
        };
        assert!(
            to_layout(HeightRule::Auto, 1000).is_none(),
            "auto ignores val entirely"
        );
        assert!(matches!(
            to_layout(HeightRule::AtLeast, 1000),
            Some(RowHeightRule::AtLeast(_))
        ));
        assert!(matches!(
            to_layout(HeightRule::Exact, 1000),
            Some(RowHeightRule::Exact(_))
        ));
    }
}
