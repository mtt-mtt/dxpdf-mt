# Floating Images — §20.4.2

## Anchor Positioning

Floating images use `wp:anchor` inside `w:drawing`:

```xml
<wp:anchor distT="0" distB="0" distL="114300" distR="114300"
           simplePos="0" relativeHeight="251658240"
           behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
  <wp:positionH relativeFrom="margin"><wp:align>left</wp:align></wp:positionH>
  <wp:positionV relativeFrom="margin"><wp:align>top</wp:align></wp:positionV>
  <wp:extent cx="975360" cy="975360"/>
  <wp:wrapSquare wrapText="bothSides"/>
</wp:anchor>
```

### Vertical Position Variants

Images resolve to one of two internal representations:

- `FloatingImageY::Absolute(y)` — page-absolute position. Used for `relativeFrom="margin"`, `"page"`, `"topMargin"`, `"bottomMargin"`.
- `FloatingImageY::RelativeToParagraph(offset)` — offset from the anchor paragraph's content area top. Used for `relativeFrom="paragraph"` and `"line"`.

### Horizontal Reference Regions — §20.4.3.4 `ST_RelFromH`

`relativeFrom` names a **region**, and both `<wp:posOffset>` and `<wp:align>`
are measured against it: an offset counts from the region's left edge, an
alignment places the object inside it. `horizontal_region` in
[`floating.rs`](../src/render/layout/build/floating.rs) is that mapping, and it
is deliberately **total over `AnchorRelativeFrom` with no catch-all** — the
four margin strips previously shared one arm with `margin` and all resolved
against the text area, which a catch-all is exactly how you get.

On a US Letter page with 1in margins:

| `relativeFrom` | Region | Span |
|---|---|---|
| `page` | the whole sheet | 0 … 612 |
| `margin`, `column` | the text area | 72 … 540 |
| `leftMargin` | page edge → left margin edge | 0 … 72 |
| `rightMargin` | right margin edge → page edge | 540 … 612 |
| `insideMargin` | left margin on an odd page, right on an even one | 0 … 72 (odd) |
| `outsideMargin` | the mirror of `insideMargin` | 540 … 612 (odd) |
| `character` | *not a region* — see below | falls back to 72 … 540 |

A margin strip can be narrower than the object in it, and that is not an
error: right-aligning a 100pt image in a 72pt `leftMargin` puts its left edge
at −28, hanging into the sheet's bleed. Word draws it the same way.

**Two references mirror.** `insideMargin`/`outsideMargin` depend on the parity
of the page the object lands on, so `horizontal_region` returns
`HorizontalRegion::Mirrored { odd, even }` and the caller picks per page. See
**Page parity** below.

**`character` is not a region.** It positions relative to the anchor's own spot
in the text run, which build-time float extraction has not laid out. It falls
back to the text area and logs — the same result it had inside the catch-all,
now reached by a named arm that admits to it.

**Frames.** `AnchorFrame::Stack` (table cell, header, footer) puts the origin
at the body's left margin, and the caller adds that margin back on the way into
page coordinates. The regions split cleanly along that seam: `page` and the
four margin strips are pure functions of the sheet, so they survive the round
trip and a header float can reach the page edge; `margin`/`column` name the
*container's* area, whose extent never reaches the resolver in a stack frame,
so they collapse onto the frame origin with zero extent.

### Alignment

Named alignments (`top`, `center`, `bottom` for vertical; `left`, `center`, `right` for horizontal) are resolved to absolute positions during building, based on the reference area.

`inside`/`outside` (§20.4.3.1) are page-parity dependent — see below.

### Page parity — §20.4.3.1/.2

Parity reaches a float's x through **two** channels: the region
(`insideMargin`/`outsideMargin`) and the alignment (`inside`/`outside`). Both
depend on which page the object lands on, and floats are extracted *before*
pagination — so at build time that page does not exist.

The position is therefore carried, not guessed:

```rust
pub enum FloatingImageX {
    Absolute(Pt),
    PageParity { odd: Pt, even: Pt },
}
```

This mirrors `FloatingImageY::RelativeToParagraph`, which exists for the same
reason on the other axis. Rather than thread parity through the anchor
arithmetic, `resolve_anchor_x` evaluates the *whole* position once per parity
and hands both readings to `FloatingImageX::from_pages`, which collapses them
to `Absolute` when they agree. They agree for every anchor that is not
`inside`/`outside`, so a document without a mirrored anchor carries no deferral
at all and every downstream `resolve` is a no-op. It also means the two
channels compose correctly for free: an `inside` alignment within an
`insideMargin` region mirrors once, not twice.

**Parity is the logical page number** — `w:pgNumType/@start` applied — not the
physical sheet index. That is the same key §17.10.6 `evenAndOddHeaders` uses to
pick a header, so a page that gets the "even" header mirrors the same way.

**Where it resolves.** Everywhere a float is assigned to a page:
`layout_section` (via `PageLayoutState::parity`, from `logical_page_base +
page_index`) and `render_header`/`render_footer` (which already computed the
logical page number to select the slot). Resolution must happen *before* line
fitting, because `ActiveFloat.page_x` drives text wrapping.

**Two paths still cannot resolve it**, for the same structural reason build
could not:

- **Table cells.** A cell is measured before its table is paginated — the row
  split is decided *from* those measurements — so the page is not known. An
  `inside` float in a table takes the odd-page reading (`layout_cell`).
- **Shape text boxes.** Laid out at build time, before the shape is placed
  (`build_shape_text_commands`).

Both are named at their call site rather than hidden in a default.

**Not applied vertically.** §20.4.3.2 `inside`/`outside` and §20.4.3.5
`insideMargin`/`outsideMargin` exist on the vertical axis too, but a two-sided
document mirrors left and right, not top and bottom — there is no reading of a
vertical "inside" that source alone can derive. Both align to the region's top.

Settling it needs a Word render of a two-sided document with a float anchored
`inside`/`outside` vertically, on both an odd and an even page. All four values
resolve through named arms in `resolve_anchor_y`, and `FloatingImageX` with its
`PageParity` variant is already in place should the answer be "it mirrors".

## `mc:AlternateContent` — MCE §M.1.2

Word writes most anchored drawings twice: modern DrawingML in an
`<mc:Choice Requires="wps">`, and a VML equivalent in `<mc:Fallback>` for
clients that predate it. **Exactly one branch is live.** Drawing both puts two
copies of the same object on the page; drawing neither loses it.

`live_mc_branch` (in `layout/mod.rs`) is the **single** selection point. Every
walker that meets the element consults it, so the answer is a property of the
element rather than of who asked:

| Walker | On `Choice` | On `Fallback` |
|---|---|---|
| `find_anchor_images` (DrawingML pictures) | recurse into the live Choice | recurse into the Fallback |
| `find_anchor_shapes` (`wps:wsp` shapes) | ” | ” |
| `extract_vml_floating_images` (VML images) | ” | ” |
| `extract_vml_primitive_shapes` (VML rects) | ” | ” |
| `fragment::collect` (inline content) | collect inline pictures; anchors naturally skip | collect |
| `find_vml_absolute_position` | `None` | probe the Fallback |

The test is **content-based, not a `Requires` namespace check** — a Choice can
declare a namespace we nominally support and still hold nothing that becomes
geometry, and the honest question is whether we will actually draw it. The
first Choice carrying either an anchored drawing or an inline DrawingML picture
wins; otherwise the Fallback is live; if there is no Fallback, nothing is.
Inline `wps:wsp` shapes and `wpg` groups do not yet have a fragment renderer and
therefore continue to yield to their VML fallback. Returning one Choice rather
than the whole Choice list also enforces MCE's exactly-one-branch rule when a
producer supplies multiple usable alternatives.

### Why one predicate, and why the `Fallback` arm has no owner

Both of this element's historical bugs came from computing the answer more than
once. First a **double** render: the shape walker read the Choice while the
image walker read only the Fallback, so a Choice's shape and a Fallback's
picture both reached the page. Then a **missing** render: the two VML walkers
answered `AlternateContent(_) => {}` — a third answer, given by not answering —
so a Choice this renderer could not draw plus a VML Fallback yielded no float at
all, while the fallback's text still arrived inline. A fourth, narrower
predicate (`choices_render_wps_shape`, "does a Choice hold a `wps:wsp`?") drove
the two suppression sites, which let a *picture* Choice render as a float while
its Fallback's text was collected inline — two branches of one element on one
page.

`McBranch::Fallback` deliberately does **not** name an owner, and splitting it
into "float" and "inline" variants — the tidier-looking design — would be wrong.
One VML fallback is routinely both, by design: `extract_vml_primitive` draws a
`<v:rect>`'s geometry and leaves its `text_commands` empty,
`extract_vml_primitive_image` skips any shape that also hosts text, and
`fragment::collect` picks the text box up at the host paragraph. That division
is by *graphic role*, not by branch, and it is exactly how a bare `<w:pict>`
already renders. A live Fallback therefore goes to all of them and each takes
the part it owns; naming one owner would drop the text of every VML rect that
has one.

§M.1.2's content model for a branch is `drawing | pict`, so a nested
`<mc:AlternateContent>` cannot come from a document — `live_mc_branch` still
resolves it innermost-first, because that is the only reading under which the
outer answer stays consistent with the inner one.

Coverage is `tests/mce_branch_selection.rs` (page-level, against inline
fixtures) plus the `live_mc_branch` and fragment-collector unit tests. The
inline-picture case is pinned separately from inline `wps:wsp` and `wpg`
groups so widening Choice support cannot strand their still-required fallback.

## Shape Text Bodies — §20.1.2.1.1 / §20.1.10.60

A `wps:wsp` shape's `txbx` content is laid out into shape-local commands by
`build_shape_text_commands` and emitted over the shape's fill. Three properties
of `a:bodyPr` shape it:

- **Insets** (`lIns`/`tIns`/`rIns`/`bIns`, defaulting to 91440/45720 EMU)
  deflate the shape's extent into the **box** the body occupies.
- **`anchor`** (§20.1.10.60) places the body within that box: `t` under the top
  inset, `ctr` centred, `b` on the bottom inset.
- **`a:normAutofit`** (§20.1.2.1.18) shrinks the body to fit — see below.

`bIns` matters only through the anchor — it is what closes off the bottom of
the box — so the two are implemented together. `just` and `dist` stretch
*inter-line* spacing to fill the box, which this sub-layout has no line-level
control over; both degrade to `t` and log, which is the closest honest reading
(a justified body also begins at the top, it simply is not stretched).

**Overflow is drawn, not clipped — unless the body says otherwise.** When the
body is taller than its box the slack is floored at zero, so it anchors to the
top and overflows downward whatever `anchor` says. `vertOverflow` defaults to
`overflow`: Word draws overflowing shape text rather than clipping it, and it
is what the entire corpus asks for (4 explicit `overflow`, 10 `bodyPr` with the
attribute absent, zero `clip` or `ellipsis`). Centring a body that does not fit
would put its first lines *above* the shape, over whatever sits there. So the
anchor only ever decides where spare room goes, and the behaviour of a body
that overflows is what it was before anchoring existed.

### Clipping — `a:bodyPr/@vertOverflow`

`TextVertOverflow` is `Overflow` (the `#[default]`) / `Clip` / `Ellipsis`, and
`overflow_keeps` is total over it with no catch-all, so a new value has to
state its own behaviour. The clip box is the **inset** box —
`tIns … extent.height - bIns`, the same box `anchor` places the body in — not
the shape's extent.

`Ellipsis` degrades to `Clip` with the indicator dropped. Choosing the last
visible line and refitting it to leave room for the glyph is a decision this
sub-layout does not make; keeping the clipping is much closer to Word than not
clipping at all, since `ellipsis` does clip.

**The clip is line-granular, which is an approximation.** Word clips at the
pixel, so a line straddling the box edge shows its top sliver; here the whole
command is dropped and it disappears. Real clipping needs a canvas clip that
survives into paint, and draw commands are flattened into one flat per-page
list with no scoping — the three `text_commands` emitters shift each command
and push it straight onto the page — so expressing it would mean a new
`DrawCommand` wrapper variant plus an arm in every consumer. Dropping is the
safe direction, because `clip`'s contract is that nothing paints outside the
box — and it is unexercised: the corpus holds 4 explicit `vertOverflow="overflow"`,
10 `bodyPr` with the attribute absent, and zero `clip` or `ellipsis` (checked,
not assumed). Worth revisiting only once a real document needs the sliver.

The extent test itself, `DrawCommand::vertical_span`, is exhaustive over the
variants. A `Text` command carries a baseline and a font size but not the
ascent/descent it was measured with, so its band is `baseline ± font_size` —
deliberately conservative for clipping. Continuous-section placement no longer
infers flow position from draw-command extents; the section stacker returns its
actual terminal cursor and column state. The three annotation variants report
no band at all: they paint nothing of their own, so a clip must not drop a link
whose rect hangs below the box while the text it annotates stays.

### Auto-fit — §20.1.2.1.18 `a:normAutofit`

`normAutofit` is **not a hint.** When Word lays out a shape whose text does not
fit, it shrinks the text and writes the result back into the file as
`@fontScale` and `@lnSpcReduction` (both in thousandths of a percent). It does
not re-derive them on open. Parsing the element and dropping its attributes —
which is what this renderer did — therefore draws every shrunk body at full
size — and, since `vertOverflow` defaults to `overflow`, then spills out of the
box rather than being clipped back. The two halves of the shrink compound.

`ShapeAutoFit` carries both factors from `bodyPr` down through the body's
`BuildState`. It reaches:

- **every resolved font size**, in `font_props_from_run` — the run's own or the
  one it inherits, because the scale belongs to the *body*, not to any run in
  it. Also the field-substitution fallback and the body's default line height,
  so a blank line in a shrunk body shrinks with it;
- **every resolved line height**, inside `resolve_line_height`.

Two decisions are worth keeping:

**The reduction is applied after §17.3.1.33 resolution, not folded into the
spacing rule.** Turning `lnSpcReduction="20000"` into `Auto(0.8)` looks
equivalent and is not: `resolve_line_height` floors `Auto` at the line's
natural height, so any multiplier below 1 is swallowed whole. That floor is
there to stop an authored multiplier from colliding glyph boxes — but crossing
it is exactly what `lnSpcReduction` is for, since Word has already laid the
body out and decided the tightened text fits. It is a post-resolution factor.

**`resolve_line_height` owns the reduction rather than its callers.** Four
places resolve a line height — fitting, emission, the border box, and the
empty-paragraph path — and they must agree to the point. Putting the factor
inside the shared function is what makes forgetting it impossible; an earlier
pass that applied it at one caller silently moved nothing, because the
baseline advance comes from a different one.

`spAutoFit` (§20.1.2.1.20) resizes the *shape* to its text, which this
sub-layout cannot do — it fills an extent the host already fixed — so it
degrades to no shrink, drawing text at its authored size rather than inventing
a fit. `noAutofit` (§20.1.2.1.16) is the explicit "do not".

## Text Wrapping

Wrapping mode determines how text flows around the image:

- `wrapSquare` / `wrapTight` — text wraps on both sides (registered as `ActiveFloat`)
- `wrapTopAndBottom` (§20.4.2.18) — image registers a full-width vertical
  exclusion band; lines above the band stay in place and the first overlapping
  line resumes below it
- `wrapNone` — no text wrapping, image overlays text (behind or in front based on `behindDoc`)

## Painting Layers

`wp:anchor/@behindDoc` controls painting independently of wrapping. Top-level
section-body floats and directly composed header/footer floats with
`behindDoc="1"` are accumulated in a physical page background layer and painted
before the ordinary page stream. This is a page-level bucket rather than a
local paragraph reorder: a background object anchored by a later paragraph
must not cover text that was already laid out on the same page. Floats nested
inside table cells and paragraph-stacked header/footer content still use the
ordinary stream and remain a separate compatibility item.

Within that layer, `wp:anchor/@relativeHeight` is ascending z-order. Commands
with the same height keep stable renderer emission order, and a shape's
geometry stays before its own text-box commands. Images and shapes are still
extracted into separate collections, so full OOXML cross-type source order is
tracked separately from this `behindDoc` invariant. Foreground objects likewise
retain the normal body stream order.

## Forward-Scan for Absolute Floats

Word uses multi-pass layout where all floats on a page affect all text. Our single-pass renderer approximates this with forward-scanning:

1. When a new page starts (`abs_floats_dirty = true`), scan `blocks[page_start_block..]` for paragraphs with absolute-positioned floating images
2. Register these as `current_page_abs_floats`
3. When building `effective_floats` for each paragraph, merge `current_page_abs_floats` with `page_floats`
4. Dedup by coordinate proximity (< 0.1pt)

### Merge Cutoff

Forward-scanned floats are only included if their `page_y_start <= cursor_y + space_before`. This prevents floats positioned far below the current paragraph from constraining it.

## Float Constraint Zone

Each `ActiveFloat` defines a rectangular constraint zone:

```
page_x = image_x - dist_left
width  = image_width + dist_left + dist_right
page_y_start = image_y - dist_top
page_y_end   = image_y + image_height + dist_bottom
```

The wrap element overrides the matching `wp:anchor` distance per edge;
structurally missing edges fall back to the anchor value, while an explicit
zero remains an override. `wrapTight`/`wrapThrough` can override L/R and
`wrapTopAndBottom` can override T/B. The image/shape drawing rectangle itself
is never expanded or shifted by these values; only the text-clearance zone is.

The `float_adjustments` function computes left/right indentation for each text line based on y-overlap with active floats.

## Pruning

Floats are pruned when `cursor_y >= float.page_y_end` — the cursor has passed below the float's bottom edge.
