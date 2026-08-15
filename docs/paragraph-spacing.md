# Paragraph Spacing — §17.3.1.33

## Space Before / After

- `w:spacing w:before="N"` — N twips above the paragraph (1 twip = 1/20 pt)
- `w:spacing w:after="N"` — N twips below the paragraph
- `w:spacing w:beforeAutospacing="1"` / `afterAutospacing="1"` — the
  explicit value is ignored and the consumer determines an HTML-like automatic
  value (§17.3.1.33). At ordinary boundaries dxpdf retains its 14pt fallback.

The fixed 5pt-before / 10pt-after interpretation belongs specifically to the
`w:compat/w:doNotUseHTMLParagraphAutoSpacing` compatibility switch. It is not a
general replacement for automatic spacing when that switch is absent, and is
therefore deliberately not used as a global constant here.

### Automatic Spacing Between List Items

Word suppresses the boundary gap between adjacent peer items when the previous
paragraph has `afterAutospacing="1"`, the next has
`beforeAutospacing="1"`, and their effective `(numId, ilvl)` values match.
dxpdf preserves both auto flags and the effective numbering identity through
the build phase, then removes the complete `previous.after + next.before`
boundary in both page and table-cell stacking.

The exact identity and facing-flag requirements are intentional. Different
numbering instances, nested-level transitions, ordinary paragraphs, and an
explicit `false` on either facing auto property retain the ordinary
`min(after, before)` collapse. This makes the rule contextual instead of a
document-wide spacing reduction.

### Automatic Spacing at Table-Cell Edges

Word applies automatic paragraph spacing only at a boundary with a neighbouring
paragraph. Consequently, a table cell suppresses automatic spacing before its
first paragraph and after its last paragraph. A one-paragraph cell suppresses
both sides, while automatic spacing between multiple paragraphs in the same
cell retains the ordinary 14pt value and collapse rules above.

This is an edge rule, not a blanket table-cell override: explicit spacing is
unchanged, and the automatic-spacing flags remain available for internal
boundaries. Only an actual first or last paragraph layout block qualifies. The
builder does not search past a nested table to find a paragraph on the other
side and treat it as a cell-edge paragraph.

### Page-Top Suppression

**§17.3.1.33**: space_before is suppressed for "the first paragraph in a body/text story that begins on a page." This means only the **structural first paragraph** of a section on its initial page:

- First paragraph of the document body
- First paragraph after a section break

Space_before is **NOT** suppressed for:

- Paragraphs that start a new page via `pageBreakBefore` (e.g., Heading1 with `spacing before="480"` retains its 24pt offset)
- Paragraphs pushed to a new page by overflow
- Paragraphs re-laid after `keepNext` forces a page break

Implementation: `cursor_y <= column_top && first_on_section_page` in `layout_section`. The `first_on_section_page` flag is true only until the first paragraph or table is processed.

### Spacing Collapse

When two consecutive paragraphs meet, their spacing collapses (§17.3.1.33 note):

```
effective_gap = max(prev.space_after, current.space_before) - min(prev.space_after, current.space_before)
```

Simplified: `collapse = min(prev.space_after, current.space_before)`, then `cursor_y -= collapse`.

### Contextual Spacing

§17.3.1.9 `contextualSpacing`: when adjacent paragraphs share the same `styleId`, both `space_after` and `space_before` between them are eliminated entirely.

## Empty Paragraphs in the Body

§17.3.1.29: every paragraph ends with a paragraph mark (¶). A paragraph with no runs still occupies one line — the mark's line height comes from `w:pPr/w:rPr` (captured as `mark_run_properties`), falling back to the paragraph style and doc defaults.

Implementation: `build_paragraph_block` injects a `Fragment::LineBreak` whose natural and text heights both use the measured default line height when the collected fragments are empty. This is required because `section::layout_section` splits paragraph fragments at page breaks and drops empty chunks (§17.3.3.1) — without the injected break, an empty-from-the-start paragraph would split into one empty chunk and be skipped.

Table cells cover the §17.4.66 trailing-empty-after-table case *before* calling `build_paragraph_block` (`build_cell_blocks`), so genuinely structural cell terminators still produce zero height.

## Empty Paragraphs in Headers/Footers

§17.10.1: empty non-last paragraphs in headers/footers still occupy a line height derived from the paragraph mark's font size (`w:rPr` on `w:pPr`, or `mark_run_properties`). A measured `Fragment::LineBreak` is inserted to produce this vertical offset.

The last empty paragraph produces no height (it's a structural terminator).
