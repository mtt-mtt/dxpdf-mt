# Floating Tables — §17.4.58

## Table Positioning Properties

Floating tables use `w:tblpPr` on `w:tblPr`:

```xml
<w:tblpPr w:rightFromText="187" w:bottomFromText="72"
          w:vertAnchor="text" w:tblpY="1"/>
```

Everything below is an **attribute of `tblpPr`**, so all of it lives in §17.4.58 — the section is cited once here rather than repeated per attribute.

### Horizontal Positioning

- `tblpXSpec`: named alignment — `left`, `center`, `right`
- `tblpX`: absolute X offset from `horzAnchor`
- `horzAnchor`: reference frame — `text` (content area), `margin`, `page`
- `leftFromText` / `rightFromText`: gap between table edge and surrounding text

### Vertical Positioning

- `tblpY`: absolute Y offset from `vertAnchor`
- `vertAnchor`: reference frame:
  - `text` — top of the nearest preceding paragraph (default)
  - `margin` — top margin edge (`margins.top`)
  - `page` — top of the page (y=0)

### Y Position Computation

```rust
let anchor_y = match vert_anchor {
    Text   => cursor_y + y_offset,
    Margin => margins.top + y_offset,
    Page   => y_offset,
};
// Table must not start before the current cursor
// (preceding content already occupies space above cursor_y).
let float_y_start = anchor_y.max(cursor_y);
```

The `max(cursor_y)` floor prevents margin- or page-relative placement from
overlapping already-rendered paragraph content above it.

### Anchor-page admission

The table's monolithic total height does not decide whether its anchor moves to
the next page. A floating table may span pages, so rejecting the anchor merely
because the *whole* table does not fit wastes usable space and shifts every
continuation page.

After resolving `tblpY`, the normal table paginator receives the exact remaining
height (`page_bottom - anchor_y`). It keeps the table on that page when the first
legal row group or split-row fragment fits. If nothing can legally fit, the
paginator returns an empty anchor slice and the first non-empty slice begins on
the next page. This keeps admission consistent with `cantSplit`, vertical-merge
row groups, repeating headers, row splitting, footnote reservation, and
page-specific body heights.

### Text-relative anchor

§17.4.58 says a floating table's logical block location is resolved relative
to the next regular (non-table, non-frame) paragraph. At the point the table is
laid out, `cursor_y` is that following paragraph's flow position. Therefore
`vertAnchor="text"` adds `tblpY` to `cursor_y`; using the preceding paragraph's
start would consume the offset inside that paragraph's own line box and can
place the table over a large heading.

## Float Registration

Floating tables are registered as `ActiveFloat` with `FloatSource::Table { owner_block_idx }`. The `owner_block_idx` identifies which paragraph should wrap around the table.

## `tblOverlap`

§17.4.57: `w:tblOverlap val="never"` prevents overlap with **other floating tables**, not with paragraph text. Tables can still visually overlap paragraph content.

> **Which edition the § numbers refer to.** All `§17.4.x` citations in this repo follow **ISO/IEC 29500-1 1st Edition** (© ISO/IEC 29500:2008), where `tblOverlap` is §17.4.57 and `tblpPr` is §17.4.58. Microsoft's [MS-OI29500] annotates a later edition whose §17.4 numbering runs exactly **one lower** (`tblOverlap` §17.4.56, `tblpPr` §17.4.57), so a cross-check against that document will look off by one. It isn't.
>
> This page previously said §17.4.47 and the code said §17.4.39; both were wrong. §17.4.39 is `tblBorders` — an unrelated element whose number had been borrowed.

### Collision resolution and spillover

`resolve_floating_anchor` (`section/floating_table.rs`) resolves the requested `tblpY` against the floats already registered on the page:

- **`Overlap` (the default), or absent** — the anchor is returned unchanged. Overlap is permitted, so there is nothing to resolve.
- **`Never`** — the anchor is pushed below any float whose y-range intersects `[anchor, anchor + height]`, iterating to a fixed point (one pass is not enough, because shifting down can reveal a new collision). It terminates because the anchor strictly increases through the finite set of float `page_y_end`s.

The outcome is one of three variants, and the distinction between the last two is what keeps layout bounded:

| Outcome | When | Caller's response |
|---|---|---|
| `OnCurrentPage(y)` | No collision shifted the anchor | Place here, paginating if it overflows |
| `Shifted { from, to }` | A collision moved the anchor, and it still fits | Place at `to` |
| `Spillover` | A collision moved the anchor **past the page bottom** | Push a new page and re-resolve |

**Overflow alone is not spillover.** A table that is simply taller than the body area is returned as `OnCurrentPage` and sliced at row boundaries by `layout_table_paginated_with_page_heights` — the same path the permitted-overlap case takes. Only a *collision-induced* shift can spill.

This is load-bearing, not a stylistic distinction. The caller answers `Spillover` by pushing a fresh page and calling back in, and `push_new_page` clears `page_floats` — so the retry sees no floats, cannot shift, and cannot spill again. **At most one page push per floating table.** Conditioning `Spillover` on overflow instead of on the shift made `page_top + height > page_bottom` invariant for a tall table, which looped forever allocating one `LayoutedPage` per iteration until the process was OOM-killed. Both halves of the argument — the shift precondition and the cleared float list — must hold; changing either one alone reopens the bug.

**Only tables collide.** `page_floats` holds image and shape floats alongside table ones, but the scan skips everything that isn't `FloatSource::Table` — `tblOverlap` is defined strictly between floating tables, and a table may sit over a floating image. The predicate is named `FloatSource::participates_in_table_overlap` so the rule reads as spec intent rather than a pattern match. Before this was filtered, a floating image across the anchor could push a `never` table down a page.

## Data Flow

```
build_table() → TableFloatInfo { right_gap, bottom_gap, x_align, y_offset, vert_anchor }
    ↓
LayoutBlock::Table { float_info: Option<TableFloatInfo> }
    ↓
layout_section() → compute float_y_start, register ActiveFloat, emit draw commands
```
