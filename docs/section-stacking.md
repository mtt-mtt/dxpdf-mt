# Section Stacking & Pagination — §17.6

Section layout takes measured blocks — paragraphs with fragments, tables with
cells — and sequences them vertically into pages. It is the largest subsystem in
the engine (`src/render/layout/section/`, ~6.5k lines) and the place where
almost every "why did it paginate like that?" question is answered.

## Two layers

The split between them is the key idea:

| | `stacker.rs` | `layout.rs` |
|---|---|---|
| Entry | `stack_blocks` | `layout_section` |
| Knows about | vertical flow within one fixed-width area | pages, columns, breaks, footnotes |
| Used by | page layout **and** table cells | page layout only |

`stack_blocks(blocks, content_width, default_line_height, measure_text)` is the
shared core. It handles paragraph spacing collapse, table layout, floating
image/shape registration, and text wrapping. It explicitly does **not** handle
page breaks, column breaks, or footnote collection — those are page-level
concerns owned by `layout_section`.

That sharing is what makes a table cell behave like a miniature page: the same
code lays out cell content as body content, so spacing collapse and float
wrapping work identically in both.

### Spacing collapse

At each paragraph, before emitting:

- §17.3.1.9 `contextualSpacing` with the same style id as the previous
  paragraph removes `prev_space_after + space_before` entirely.
- Otherwise the standard collapse applies: `min(prev_space_after,
  space_before)` is subtracted.

See [Paragraph Spacing](paragraph-spacing.md).

### Floats

Floating images and §20.4.2 floating shapes are registered as `ActiveFloat`s so
subsequent lines narrow around them. `TopAndBottom` is different — it emits
immediately and advances `cursor_y` past the drawing rather than registering.
`WrapMode::None` is pure overlay and does not participate.

See [Floating Images](floating-images.md) and [Floating Tables](floating-tables.md).

## `CellLine` — the cut model

`StackResult` carries `lines: Vec<CellLine>` alongside its commands and height.
Each entry records where a fitted line sits and, crucially, **whether it is
legal to cut there**:

| Field | Rule |
|---|---|
| `top_y` | Box top in cell-content coordinates |
| `para` | Owning paragraph index — lines of one paragraph are contiguous |
| `interior_atomic` | §17.3.1.14 `keepLines`, or a bordered / shaded / drop-cap paragraph whose box would be torn — may move whole, never be divided |
| `widow_control` | §17.3.1.44 — an interior cut must leave ≥ 2 lines on each side |
| `keep_next` | §17.3.1.15 — a cut at this paragraph's trailing boundary is illegal |

This exists so §17.4.1 table row splitting chooses cut points from **paragraph
structure**, not from raw draw commands. Without it, a row split could tear a
`keepLines` paragraph or strand a widow inside a cell — behaviours Word never
produces.

`lines` is left **empty** when the content cannot be safely bisected (a nested
table or floating object is present). Such cells move whole rather than split.
See [Table Layout](table-layout.md).

## Page-level concerns (`layout.rs`)

### Paragraph splitting

`decide_paragraph_split(n_fit, total, widow_control, at_page_top)` returns
`All`, `Break { head }`, or `MoveWhole`:

- With widow/orphan control, the head is capped at `total - 2` and must itself
  be ≥ 2 — enforcing both the orphan rule (≥ 2 lines stay) and the widow rule
  (≥ 2 lines follow).
- Without it, any `n_fit ≥ 1` is a legal break.
- When no legal break exists: **at page top**, emit whole and let it overflow
  (the remaining space is already a full page — moving it again would loop);
  otherwise move the whole paragraph to the next page.

Termination is guaranteed because `line_start` advances by ≥ 1 on every emitted
segment, and a `MoveWhole` always lands on a fresh page where the `at_page_top`
branch forces progress.

`emit_split_paragraph` requires the caller to have established that the
paragraph is splittable. The `can_split` gate in `layout.rs` is the authority:
no `keepLines`, no floating images/shapes, ≥ 2 fitted lines, and footnotes only
within a single unbroken chunk.

Borders, shading, drop caps and multiple columns are **not** disqualifying,
though earlier revisions of this doc said they were. Bordered and shaded
paragraphs are drawn per segment (`emit_segment_borders_and_shading`), and
§17.6.4 unequal-width columns split correctly because each segment re-fits
against its own column's width.

### `keepNext` chains (§17.3.1.15)

`starts_keep_next_chain` identifies the head of a run of keep-together
paragraphs. `keep_next_terminal_table` walks the chain forward to find a
non-floating table that terminates it, returning `None` if the chain hits a
`pageBreakBefore`, a non-`keepNext` paragraph, or a *floating* table (which is
positioned independently and cannot anchor a chain).

`measure_keep_next_group` pre-measures the chain so the fit decision is made
once. Rather than moving an entire oversized chain — which would leave a
half-empty page — a splittable leading paragraph is peeled onto the current
page. Oversized groups fall through to in-line placement, so progress is always
made and the pagination loop cannot hang.

The peel applies to a splittable *leading* paragraph and to paragraph
terminals. A chain whose leading paragraph is unsplittable but a later one is
long, and a chain terminating in a table (whose leading row group falls outside
the measured group), both keep the conservative whole-move — correct in every
case, only less page-filling.

### Which segment owns what (§17.3.1.24, §17.3.1.33)

When a paragraph splits, `SegmentEdges` (`paragraph/borders.rs`) decides per
segment: side borders and shading span **every** segment; the top border and
`space_before` belong to the **first**; the bottom border and `space_after` to
the **last**. The §17.3.1.11 drop cap and any float-narrowed prefix are held on
the first segment by `prefix_adjusted_head` so a split never tears the glyph or
strands a wrapped line, and the §17.9 list label is first-segment-only by
construction (it lives on line 0).

### Inline page breaks at hard section boundaries

A section break is stored on the final paragraph mark of the outgoing section.
For a following next/odd/even-page section, Word does not let a plain,
otherwise empty section-mark paragraph create a page of its own. Before layout,
the renderer suppresses only that terminal mark. A preceding inline page break
therefore coalesces naturally with the hard section boundary. Decorated
paragraphs, paragraphs with notes or floating objects, continuous sections, and
next-column sections are not suppressed.

Likewise, a run of plain empty paragraphs immediately before either a paragraph
with `pageBreakBefore` or a paragraph whose first meaningful fragment is an
inline page break may fill the tail of the current page but does not create a
separate blank page. The explicit break remains authoritative. This rule does
not collapse two real inline page breaks; authors can still create a deliberate
blank page with consecutive break markers.

A break-only paragraph may also keep its invisible bookmark or paragraph mark
at the page tail. Its inline break terminates `keepNext` prediction and moves
the following content forward once; the structural mark is not first moved to
an otherwise empty page.

### Footnotes

`reserve_footnotes` (§17.11.23) measures each footnote on the current page,
subtracts its height — plus the separator gap for the first footnote on that
page — from the available bottom, and queues it for rendering. Shared by both
the atomic-placement and per-segment split paths, so a split paragraph reserves
footnotes per segment.

### Columns and clearance

`advance_column_or_page` moves to the next column before starting a new page.
§17.6.4: columns of unequal width share one page height, which is why a
paragraph can split across columns of different widths — the continuation
re-fits against each column's own width.

`layout_section_with_clearance` accepts per-page header/footer clearance, so
pages with differently-sized headers get correct body bounds. See
[Headers and Footers](headers-footers.md).

### Floating tables

`floating_table.rs` (§17.4.58) assigns the slices produced by
`layout_table_paginated` to page slots. `tblpY` positions only the **first**
slice; continuations start at the top of their page's content area. Text
wrapping is anchored to the first page only, since continuation pages contain
nothing but the table.

The spec defines `tblpY` for the anchor but not overflow behaviour — the
continuation-at-top rule mirrors Word's observable behaviour.

## Absolute-float forward scan

Word runs multi-pass layout where every float on a page affects all its text.
This single-pass renderer approximates that by forward-scanning upcoming blocks
for absolute-positioned floats when a page starts, then merging them into each
paragraph's effective float set. Details in
[Floating Images](floating-images.md).
