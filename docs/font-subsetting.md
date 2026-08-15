# Font Subsetting — §17.8 / ISO 32000-1 §9.6

Between layout and paint, every typeface the document actually paints is
replaced by a subset containing only the glyphs it needs. A DOCX that uses 60
characters of a 400 KB font should not embed 400 KB in the PDF.

Gated by the default-on `subset-fonts` Cargo feature. Source:
`src/render/subset/`. Driven from `render_with_font_mgr` (`src/render/mod.rs:155`):

```rust
let usage = subset::collect(&pages, &registry);
let report = subset::apply(usage, &mut registry);
log::info!("font subset: {report}");
```

The pass runs *after* layout because it needs the final `Vec<LayoutedPage>` —
you cannot know which glyphs are used until pagination, headers/footers, and
field evaluation are done.

## Why codepoints, not glyph ids

`collect.rs` records **Unicode codepoints** per typeface, not glyph ids. Two
reasons (`src/render/subset/collect.rs:9-15`):

1. `fontcull` is codepoint-driven. It walks the font's own `cmap` to derive the
   glyph closure, then keeps `GSUB` substitutions reachable from those glyphs —
   ligatures and contextual alternates survive. Feeding it glyph ids observed
   during layout would drop any glyph the runtime *might* shape into later.
2. Codepoints are the source of truth in DOCX text, independent of whichever
   font happens to render them.

`Codepoint(u32)` is a newtype specifically so it cannot be confused with a glyph
id (`u16`).

## Pass 1 — collect

`collect(pages, registry) -> CodepointUsage` walks every `DrawCommand::Text`
and accumulates `BTreeMap<TypefaceId, BTreeSet<Codepoint>>`.

Keyed by `TypefaceId` (Skia's `Typeface::unique_id`), **not** by requested
family name. This matters: a document requesting both `Calibri` (substituted to
Carlito) and `Carlito` directly resolves to one underlying typeface, and their
usage must merge — otherwise the second subset would drop the first's glyphs.

Resolution is memoized in a 4-slot array indexed `bold | italic << 1`, with the
inner map probed by `&str` so a cache hit allocates nothing. `registry.resolve`
lowercases, hashes, and clones a `TypefaceEntry` on every call, and the same
(family, weight, slant) recurs across a document's 100k+ text commands.

## Pass 2 — apply

`apply(usage, &mut registry) -> SubsetReport` processes each used typeface:

1. **Extract** (`extract.rs`) — get subsettable SFNT bytes. `Embedded` origin
   reads from the registry (already deobfuscated per §17.8.1.4); `System` calls
   `Typeface::to_font_data`; `Packaged` reads the original controlled-pack
   file bytes retained by the registry. WOFF2 is decompressed via `fontcull`; **WOFF1 and
   TTC are documented capability boundaries** — ECMA-376 forbids WOFF in DOCX,
   and Skia's `openStream` strips TTCs to a single face in practice.
2. **Subset** — `fontcull::subset_font_data_unicode(bytes, unicodes, &[])`.
3. **Splice the `name` table** (`name_splice.rs`) — see below.
4. **Reject non-shrinking output** — if `bytes_after >= bytes_before`, keep the
   original (`UnchangedNoSavings`).
5. **Rebuild** as a Skia `Typeface` via `FontMgr::new_from_data`.
6. **Post-validate shapeability** — see below.
7. **Swap into the registry** via `replace_typeface_by_id`.

A typeface reachable from multiple cache keys is subsetted once; the replace
step covers every key pointing at it.

### The `name` table splice

`fontcull` (via `klippa`) drops every record from the `name` table — its public
API hard-codes an empty `name_ids` set. The subset then has no family, full, or
PostScript name, so Skia falls back to a synthetic `font<hex>` identifier and
the PDF embeds *that*.

`splice_original_name` copies the original `name` table back in. This is safe
because the `name` table stores font metadata, not per-glyph data — it is
independent of glyph order and cannot affect shaping. Skia still adds the
standard `ABCDEF+` subset prefix at PDF write time (ISO 32000-1 §9.6.4).

The splice is best-effort: on failure the unnamed subset is used rather than
aborting.

### Shapeability post-validation

A structurally valid SFNT can still ship a broken `cmap`. Observed with macOS
Apple-vendored fonts (Helvetica Neue, Arial Unicode MS), where klippa's cmap
reconstruction silently drops mappings — Skia accepts the bytes, but every
glyph shapes to `.notdef` and the PDF renders blank text.

`check_shapeability` therefore compares the rebuilt typeface **against the
original**, per codepoint via `Typeface::unichar_to_glyph`, and rejects the
subset only where coverage was *lost*: the codepoint shaped before and is
`.notdef` after.

**The baseline is the whole point.** Asking only "does this codepoint shape in
the subset?" cannot distinguish a subsetter that destroyed the cmap from a font
that never had the glyph — and the second is ordinary. Measured before the
baseline existed: a single U+25AA bullet falsely rejected
`sample-docx-files-sample1`, and one SOFT HYPHEN took a document from 13 KB to
272 KB because the full 606 KB face was embedded instead of its subset. Both now
subset correctly, while the two genuine cmap failures in the corpus
(`sample2`, `sample-emoji`, `0/N → N/N`) are still rejected.

There is deliberately **no whitespace or control exemption**. The old code
skipped those because their `.notdef`-ness is font-dependent — which is true of
every codepoint, and is exactly what the baseline handles. The exemption also
hid real regressions: a subset that dropped a space the original had went
uncaught.

**This validation is the single most important invariant in the pass.** Without
it, subsetting silently destroys text on affected hosts. The regression tests are
`apply_never_installs_unshapeable_subset` and
`losing_coverage_the_original_had_is_a_regression`.

## Failure handling

Every terminal state is an explicit `SubsetOutcome` variant — nothing is
silently swallowed:

| Variant | Meaning |
|---|---|
| `Subsetted` | Strictly smaller bytes; new typeface installed |
| `UnchangedNoSavings` | Nothing to drop — every glyph is referenced |
| `UnsupportedFormat` | WOFF1 or TTC — capability boundary |
| `NoBytesAvailable` | `to_font_data` returned `None` for a system font |
| `DynamicFallbackKept` | A system font-link fallback is kept whole |
| `SubsetterError` | `fontcull` rejected the input |
| `SkiaRebuildFailed` | Skia would not build a typeface from the output |
| `UnshapeableSubset` | Rebuilt, but coverage the original had was lost |

The pass is **best-effort per typeface**: a failure leaves that entry's original
in place (PDF gets bigger, text still renders) while other typefaces keep their
savings. Add a state → add a variant.

System font-link faces selected for a missing run-level glyph are pinned under
a render-local alias and deliberately bypass subsetting. On Windows these faces
can expose a valid cmap entry while their extracted/subset bytes no longer carry
the linked glyph outline; retaining the original face prevents silent symbol
loss.

`SubsetReport::Display` emits the one-line `log::info!` summary.

## Interaction with the font registry

The subset pass **mutates `FontRegistry` in place** after layout. This is why
the typeface cache is per-render and owned by the registry rather than
`thread_local!` — a cross-render cache would leak subsetted typefaces into
later renders, where their glyph coverage is wrong. See
[Font Substitution](font-substitution.md).

Ordering is load-bearing: layout must measure against the **full** typeface,
because subsetting is derived from what layout produced. Subsetting before
layout would be circular.

## Spec references

- **ECMA-376 §17.8** — DOCX font embedding.
- **ECMA-376 §17.8.1.4** — embedded-font obfuscation; deobfuscation happens
  upstream in the parser, so this pass sees plain SFNT bytes.
- **ISO 32000-1 §9.6.4** — PDF subset name prefixes (`ABCDEF+`), emitted by
  Skia's PDF backend, not by us.
- **OpenType** — offset-table signatures used by `format.rs` for detection.
