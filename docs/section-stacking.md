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
terminals. When an unsplittable prefix (for example a one-line heading) fits
together with a widow-legal head of the terminal paragraph, that prefix and
head stay on the current page and the terminal paragraph continues on the next
page. A chain terminating in a table (whose leading row group falls outside the
measured group) keeps the conservative whole-move.

A non-floating table can also *bridge* a body chain when the first block in the
first cell of its last row is a paragraph whose resolved `keepNext` is on. This
is deliberately a sentinel, not an "any cell" search: it models the terminal
paragraph mark Word uses without promoting unrelated cell content. Admission
measures the complete bridge table with the same row-height pass as normal
layout, then continues into following body blocks. If the authored chain is
longer than a page, only its largest prefix of complete paragraph/table
segments is considered; no paragraph or table is cut by the predictor.

The bridge predictor is body-only and conservative. Floating tables, active
page floats, multiple columns, explicit page/column boundaries, and bridge
tables containing footnotes, nested tables, or anchored objects fall back to
the established paragraph/table paginator. It neither changes row grouping nor
interprets `lastRenderedPageBreak`. A prefix moved to a fresh page records its
exclusive block end so a paragraph after an included table cannot be mistaken
for a new chain and move the same content a second time.

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

The suppression above applies only to the paragraph mark that owns an
**outgoing hard section break**. The final `w:sectPr` is a direct child of
`w:body`, so trailing body paragraphs before it are real document content and
are never removed as a structural section mark.

At the document boundary there is one still narrower case: when the first
outgoing ordinary `nextPage` section becomes empty only because that structural
terminal mark was suppressed, it owns no physical sheet. The following real
section therefore starts on physical page 1 and uses its own page-number and
header/footer settings. This rule applies only to section 0 with a following
section. It does not change the one-page contract for a truly empty document,
does not remove non-leading blank sections, and never folds explicit
`oddPage`, `evenPage`, `continuous`, or `nextColumn` section intent.

When document-level `evenAndOddHeaders` is enabled, an ordinary `nextPage`
section that explicitly restarts `w:pgNumType/@start` also preserves physical
recto/verso parity. If the restarted logical number is odd but the next
physical sheet is even (or vice versa), a genuinely blank separator is inserted
before the section. This applies only after the first section and does not
double-handle explicit `oddPage`/`evenPage`, continuous, or next-column starts.

There is one narrower page-tail case. When a table is followed by one or more
plain empty spacer paragraphs and then by a break-only paragraph, the spacers
may consume the remaining body height. If that break paragraph has therefore
already moved beyond the body bottom, the renderer first commits the full page
and then applies the inline break. This preserves the deliberate blank page
between the table and the following content instead of collapsing two page
advances into one.

### Pagination reason ledger

Every committed page records a debug-only `PageBreakCause` through the
`dxpdf::pagination` log target. Causes distinguish deferred inline breaks,
`pageBreakBefore`, `keepNext` chains, paragraph overflow, the table-spacer case
above, explicit column breaks, floating-table placement/continuation, and
ordinary table continuation. The record also carries section/logical page,
block and column indexes, the cursor and body bounds, and command/footnote/float
counts. This ledger is diagnostic only: enabling it does not change layout.

### Footnotes

`reserve_footnotes` (§17.11.23) measures each footnote on the current page,
subtracts its height — plus the separator gap for the first footnote on that
page — from the available bottom, and queues it for rendering. Shared by both
the atomic-placement and per-segment split paths, so a split paragraph reserves
footnotes per segment.

Table slices use the same page-level reservation and rendering path. Unlike a
body paragraph, their note bodies are owned by the slice because table row
measurement and splitting create new layout objects. The page queue therefore
accepts both borrowed paragraph notes and owned table notes while preserving
document order.

### Columns and clearance

`advance_column_or_page` moves to the next column before starting a new page.
§17.6.4: columns of unequal width share one page height, which is why a
paragraph can split across columns of different widths — the continuation
re-fits against each column's own width.

`layout_section_with_clearance` accepts per-page header/footer clearance, so
pages with differently-sized headers get correct body bounds. See
[Headers and Footers](headers-footers.md).

### Continuous-section terminal state

The document orchestrator uses `layout_section_with_clearance_result` when the
following section is `continuous`. The outgoing stacker preserves the pending
physical page together with its actual `cursor_y`, column index, column top,
effective bottom, active floats, and source page/column geometry. The incoming
section inherits that state exactly when both flow frames match.

When page or column geometry changes, the incoming section starts a shorter
flow region at the outgoing cursor. That region is deliberately not classified
as a full-height fresh column: if no legal line fits, the normal column/page
advance path runs instead of allowing text below the body boundary. Full Word
column balancing for changed geometry is still a separate layout operation.

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
