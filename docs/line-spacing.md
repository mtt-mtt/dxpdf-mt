# Line Spacing — §17.3.1.33

## Spacing Element

```xml
<w:spacing w:line="276" w:lineRule="auto"/>
```

### lineRule Modes

| lineRule | Meaning | Computation |
|----------|---------|-------------|
| `auto` | Proportional to the measured text line box | `max(text_height * (line / 240), natural_height)` |
| `exact` | Fixed height, clips if content is taller | `line` value in twips → Pt |
| `atLeast` | Minimum height, grows for taller content | `max(natural_height, line_in_pt)` |

Default when absent: `auto` with `line=240` (single spacing, 1.0x multiplier).

### Auto Mode Multiplier

The `line` value for `auto` mode is in 240ths of a line:

```
multiplier = Pt::from(line_twips).raw() / 12.0
```

Common values:
- `240` → 1.0x (single)
- `276` → 1.15x (Word default for modern templates)
- `259` → ~1.08x (Aptos/Calibri default in newer documents)
- `360` → 1.5x
- `480` → 2.0x (double)

### Natural and Text Heights

Layout tracks two heights because Auto scales text metrics but must not scale
inline images:

- `natural_height` is the tallest natural fragment on the line.
- `text_height` is the tallest font line box (`ascent + descent + leading`).

Natural fragment heights are:

- Text: `ascent + descent` from font metrics
- Images: image height
- LineBreak: its natural `line_height`

A leading or consecutive text-wrapping `w:br` creates an empty visual line. Its
Auto `text_height` is measured through the same effective `FontProps` and
`TextMeasurer` as the run, so the empty line includes the font's real leading.
The nominal point size is retained separately as the break's natural height;
therefore this correction does not change the `exact` or `atLeast` formulas.
Page and column breaks do not use this manual-break metric path. Synthetic
empty-paragraph breaks already carry their measured default line height as both
values.

## Section Document Grid

An explicit section `w:docGrid` type of `lines`, `linesAndChars`, or
`snapToChars` activates a positive `w:linePitch` as the vertical line grid.
`linesAndChars` and `snapToChars` additionally retain character-grid state for
East Asian compatibility behavior. An omitted `w:type`, an explicit
`w:type="default"`, or an absent `w:docGrid` does not activate either grid.

The effective paragraph properties still control whether the vertical grid is
used. `lineRule="exact"` and `snapToGrid="false"` bypass it. Table-cell text
also bypasses it unless `w:compat/w:adjustLineHeightInTable` is enabled. Auto
spacing applies its multiplier to the pitch, while AtLeast keeps its authored
minimum and rounds the natural glyph/object box to whole pitches.

The vertical line-grid rule is independent from character-pitch layout.
`w:charSpace` is parsed and preserved, but this line-spacing path does not use
it to reposition glyphs.

### Implementation

```rust
fn resolve_line_height(natural: Pt, text_height: Pt, rule: &LineSpacingRule) -> Pt {
    match rule {
        LineSpacingRule::Auto(multiplier) => (text_height * *multiplier).max(natural),
        LineSpacingRule::Exact(h) => *h,
        LineSpacingRule::AtLeast(min) => natural.max(*min),
    }
}
```

## Paragraph Height

Total paragraph height = `space_before + sum(line_heights) + space_after`.

This height is added to `cursor_y` after the paragraph is rendered. The `space_after` portion can collapse with the next paragraph's `space_before` (see [Paragraph Spacing](paragraph-spacing.md)).
