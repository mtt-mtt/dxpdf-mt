# Table Layout — §17.4

Tables are the hardest layout problem in the engine: column widths depend on
the grid, row heights depend on laid-out cell content, cell content depends on
column widths, and the whole thing has to survive being cut across a page
boundary. Source: `src/render/layout/table/`.

## Three passes

The circular dependency is broken by fixing column widths *first*, from the
declared grid rather than from content.

```
grid.rs      compute_column_widths()  →  Vec<Pt>, one per grid column
measure.rs   measure_table_rows()     →  MeasuredTable (cell layouts + row heights)
emit.rs      emit_table_rows()        →  positioned DrawCommands
```

### Pass 1 — column widths (`grid.rs`)

`compute_column_widths(grid_cols, num_cols, available_width)` scales the
declared `w:tblGrid` values proportionally to the available width. With no
grid, columns are distributed equally.

Widths come from the grid, never from content measurement. Word's autofit
algorithm is not implemented; proportional scaling of the declared grid matches
Word's output for the overwhelming majority of real documents, and it makes the
pass O(columns) instead of requiring a content-measurement pre-pass.

### Pass 2 — measure (`measure.rs`)

With widths fixed, each cell is laid out under a tight width constraint via
`layout_cell`, producing a `CellLayout` and therefore a natural height. Row
height is the max over its cells, then adjusted by:

- **§17.4.81 `RowHeightRule`** — `AtLeast(Pt)` constrains the row's content
  box, then preserves cell padding outside it. Its measured height is
  `max(natural content, atLeast) + max(top cell margin) + max(bottom cell
  margin)`; the two row-wide margin maxima may come from different cells.
  `Exact(Pt)` pins the content budget and lets content clip. Word/WPS reserve
  only the largest effective bottom cell margin after that exact budget, so the
  measured row height is `exact + max(bottom cell margin)` (plus any table cell
  spacing handled separately).
- **§17.4.85 vertical merge** — `expand_rows_for_vmerge` grows the rows of a
  merge group so the `Restart` cell's content fits within the combined spanned
  height. The shortfall is spread **evenly** across every row in the span
  (`overflow / rows`), not concentrated in one of them.

  > **Unverified heuristic, and ECMA-376 cannot settle it.** The spec was
  > searched for a rule and does not contain one. All three places it would
  > have to live were checked:
  >
  > | § | Says | On spanning cells |
  > |---|---|---|
  > | 17.4.85 `vMerge` | which cells merge; a group spanning different grid columns is invalid | nothing — no height language at all |
  > | 17.4.81 `trHeight` | `auto` = "the height required by its contents" | never defines "contents" for a cell that spans rows |
  > | 17.4.21 `hideMark` | "the height of a table row is determined by the height of all glyphs in all cells in that row" | doesn't mention vMerge |
  >
  > §17.4.21 is the only row-height *rule* in the spec, and it is worth reading
  > carefully because it constrains the answer even though it doesn't give it.
  > The model it describes is a **per-row maximum over the cells in that row** —
  > not a budget divided among rows. Even distribution (`overflow / rows`) is
  > not expressible as a per-row maximum of anything, so of the candidates it is
  > the one the spec's own row-height sentence structurally disfavours.
  >
  > It does *not* follow that the first row takes the excess. Applied literally
  > to a merge span, §17.4.21 puts the restart cell's glyphs in the restart row
  > and so sizes that row to the whole content, while each `continue` row still
  > carries its own end-of-cell mark — the merged box would come out *taller*
  > than its content by the sum of the continue rows. No implementation does
  > that, which is the tell that the sentence was written without merged cells
  > in mind. The spec is silent by omission, not by implication.
  >
  > So: **even distribution is disfavoured; last-row and first-row remain
  > open.** Last-row has the better structural argument — a single-pass
  > top-to-bottom row sizer can only enforce a span's total once the span
  > closes, which is at its last row — but no spec text.
  >
  > The choice is *observable*: it changes rendered output on 1 of the 24 real
  > documents in the local `test-cases/` corpus
  > (`2026-03-09_annahme_abgabe_zusatzartikel__200.docx`, 4 restarts / 28
  > continues). That file also shows the overflow case is genuine rather than an
  > artefact — Word wrote **one** `w:trHeight` in the entire document, so the
  > merge rows carry no authored heights for a renderer to defer to.
  >
  > Settling it still needs a Word-exported PDF of a two-row vertical merge
  > whose restart cell overflows both rows. Until then the current behaviour is
  > pinned by `expand_spreads_overflow_across_the_merge_span`, so it cannot
  > change by accident. Tracked as E5a#6.
  >
  > A lone `Restart` (no `Continue` below it) is not a span and is sized by the
  > normal row-height path instead — see `measure_table_rows`.

Border resolution also happens here (below), because a resolved border width
affects the space available to content.

### Pass 3 — emit (`emit.rs`)

Positions cells and emits commands into **three layered buffers** —
`shading`, `content`, `borders` — concatenated in that order. Painting borders
last is what keeps a neighbouring cell's background from covering a shared
edge.

## Border resolution (`borders.rs`)

`resolve_cell_effective_borders` (§17.4.38 / §17.7.6) computes each cell's four
edges from per-cell borders (including conditional formatting), table-level
borders, and `insideH`/`insideV` mapped onto interior edges.

Edge detection uses the cell's **absolute grid column**, not its index within
the row. §17.4.17 `gridBefore` and §17.4.16 `gridAfter` mean a row's first cell
is not necessarily at the table's left edge.

Only `gridBefore` is carried into layout. **`gridAfter` is derived**: a row's
right edge is `gridBefore` plus the sum of its cells' `gridSpan`s, and every
consumer already tracks that running grid column — including the check that
decides whether the last cell gets the outer `right` border or `insideV`. The
parsed value stays on `model::TableRowProperties` (the model mirrors the
document); `TableRowInput` used to carry it too, where nothing read it.

### Shared-edge conflict resolution

Where two cells share an edge, `resolve_border_conflict` reconciles them. The
algorithm is **not in ISO/IEC 29500-1** — the standard only says a method exists
— but [MS-OI29500] §17.4.66 (`tcBorders`, note a) states it in full:

1. **An edge with no border yields to one that has it.** `nil` and `none` are
   not synonyms, but the difference is about *inheritance*, not this step: an
   edge omitted or written `val="none"` falls back to the table style, then
   `tblPrEx`, then `tblBorders`, while `val="nil"` declines that fallback and
   stays empty. Either way an empty edge yields here — see below.
2. Otherwise weight = width in eighths of a point × the style's border number
   (single = 1, double = 3). Heavier wins.
3. Equal weight: the style **earlier** in the spec's precedence list wins, which
   means **`single` beats `double`** — the intuition runs the other way, since
   double has the greater style number and therefore the greater weight at equal
   width. Equal weight means the single is three times wider.
4. Equal style: the darker colour wins (`R+B+2G`, then `B+2G`, then `G`).

The comparison is a **total order**: the caller passes (upper row's bottom,
lower row's top) and (left cell's right, right cell's left), so a rule that
stopped short would let the winner depend on which *side of the edge* a border
was declared on. `resolve_border_conflict(a, b) == resolve_border_conflict(b, a)`
is a property test over a 20-border matrix.

Nonzero `tblCellSpacing` (§17.4.44) disables step 1–4 entirely: with a gap
between them adjacent cells share no edge, so every cell keeps all four borders.

#### What `nil` does — and what it does not

The note adds *"If the conflicting table cell border is nil, then no border
shall be displayed"*, which reads as `nil` beating everything on the far side of
the edge. **It does not.** `nil` acts on its own cell only: it is how a cell
declines the inheritance in step 1, which is the whole of its difference from
`none`. The facing cell's border is untouched, so a `nil` edge yields in
resolution exactly like an absent one.

Three independent facts in `IP 05 Trenches` fix that reading:

- A cell declaring `<w:bottom w:val="single"/>` above one declaring
  `<w:top w:val="nil"/>` — Word draws the line, as does macOS's own DOCX
  renderer on the same markup.
- The `Date/Time:` cell *inherits* its bottom from `insideH` and is faced by a
  `gridSpan=2` spacer cell whose `nil` was aimed at the neighbouring column.
  Word draws that line too, and could not do otherwise: a cell paints one border
  across its whole width, so a wide cell's `nil` cannot punch a hole in the cell
  above it.
- Down that document's spacer columns the generator writes `nil` on **both**
  sides of every shared edge. Writing both is only necessary because one alone
  does not suppress.

Word's built-in `Medium List 2` family depends on the same reading: its heavy
header rule lives on `firstRow`'s `bottom` with `nil` on the `band1Horz` `top`
directly below, so a `nil` that reached across would erase the rule that defines
the style.

`nil` is still not a no-op. With nothing facing it — a table's outer edge, or a
facing cell that is also `nil` — declining inheritance is exactly what removes
the border, and it is the only way to remove one in a `Table Grid` document.
That is why `CellEdge` keeps `Suppressed` distinct from `Absent` even though
both paint nothing: the page-split top-border restore in `emit.rs` may revive an
`Absent` top and must not revive an emptied one.

`tests/table_border_conflict.rs` pins the full matrix end-to-end.

Because a cell paints one border across its whole width, `measure.rs` decides
per inter-row edge which row *owns* it, then draws the whole edge from that
side — splitting one line between the two rows would offset segments by a border
width. Uniformity is compared with `CellEdge::paints_same`, not `==`: the
`Absent`/`Suppressed` distinction is invisible to the painter, and letting it
into the comparison splits a run that paints one continuous line.

### Border styles — Tier 0

§17.4.38 defines 26 `ST_Border` styles; the painter draws **two**. `double`
renders as a double line, `single` as a single line, and the remaining 24 —
`dotted`, `dashed`, `thick`, `triple`, `wave`, `dashDotStroked`, the whole
`thinThick*` / `thickThin*` family, `threeDEmboss`, `inset`/`outset` — are
approximated by a solid line.

The approximation is bounded: position, width and colour all come through
unchanged, and only the stroke pattern is lost. `convert_model_border` reports
each distinct unhandled style once per render under `RUST_LOG=warn` (deduped via
`BuildState::warned_border_styles`, since warning per occurrence would emit one
line per cell edge). A shape text box reports independently of the body.

`val="none"` / `val="nil"` are filtered *before* this conversion, so they
correctly draw nothing rather than collapsing to a visible single line.

Borders are drawn **inward** from the cell edge as filled rectangles, not
strokes. Horizontal borders own the corner squares and span the full cell
width; vertical borders fill only the gap between them. This partitioning
eliminates the anti-aliasing seams that a stroke-based approach produced at
corners.

`suppress_first_row_top` (§17.4.38) supports adjacent-table collapse:
consecutive tables sharing a style render as one merged table, so the second
table's top border would duplicate the first's bottom border.

## Pagination

Entry points in `mod.rs`:

| Function | Use |
|---|---|
| `layout_table` | Monolithic — table fits on one page |
| `layout_table_paginated` | Splits across pages |
| `layout_table_paginated_with_page_heights` | As above, with per-page heights (differing header/footer clearance) |
| `measure_leading_table_group_height` | Lookahead for the section stacker's fit decisions |

### Row groups

`build_row_groups` partitions rows into **atomic units** for page-break
decisions. A group is *not* splittable if:

- any row sets §17.4.1 `cantSplit`,
- the group spans a vMerge run (`Restart` through consecutive `Continue`), or
- any cell contains a nested table.

`row_group_end` walks forward while the next row has any `vMerge=Continue`
cell, which is what makes a merge span indivisible.

§17.4.49 header rows (`is_header`) repeat at the top of each continuation page.

### Splitting a row (`split.rs`)

When a splittable row doesn't fit, `find_row_cut` derives cut points from each
cell's already-laid-out content and partitions the commands into a first slice
(current page) and a second (next page). Both halves keep the cell's top and
bottom margins so text never collides with the cut-edge border.

Two rules make this safe:

- A cell that **cannot** be cut text-wise — too few baselines, or image/shape-
  only content — returns `CellCut::keep_all()`, and its full visible height is
  added to the required first-half height. You cannot cut through an image.
- If honouring those non-splittable cells pushes the first half past the
  available space, the cut is abandoned (`None`) and the caller spills the
  whole row to the next page.

Cut-point *legality* within a cell's text comes from `CellLine` records
produced by the section stacker, not from raw draw commands. §17.3.1.14
`keepLines` (together with bordered, shaded, and drop-cap paragraphs) still
makes an interior paragraph cut atomic, and §17.3.1.15 `keepNext` still blocks
a cut at that paragraph boundary.

Word gives §17.4.1 row splitting a narrower widow/orphan rule than body
pagination: once an otherwise splittable row crosses a page, an interior cut
may leave one line of a cell paragraph on either side. Thus a two-line cell can
split 1/1, a three-line cell can split 1/2 or 2/1, and a longer cell can end in
a one-line continuation even when its resolved §17.3.1.44 `widowControl` is
enabled. This exception exists only in `table::split::largest_legal_cut`;
ordinary body paragraphs continue to use the 2/2 widow/orphan gate described
in [Section Stacking](section-stacking.md). The row-group rules above still
exclude `cantSplit`, vMerge spans, nested-table rows, and repeating headers
before this interior-cut logic can run.

`emit_table_rows` takes a `top_border_override` so a continuation slice still
gets a visible top edge, even though the measured top borders were suppressed
or resolved away.

### Footnotes in cells

Cell layout records every footnote together with the line that contains its
reference. Row splitting partitions those records with the same cut used for
text commands, so a note follows the split half that owns its reference.

The paginated table path measures note bodies at page content width and charges
their height, plus one separator gap per page, while packing rows. Each
`TableSlice` returns the complete notes referenced by that slice to the section
stacker. Repeated header rows do not repeat their original footnotes on later
pages.

## Related

- [Floating Tables](floating-tables.md) — §17.4.58 `tblpPr` positioning.
- [Style Cascade](style-cascade.md) — §17.7.6 conditional formatting feeding
  cell borders and shading.
