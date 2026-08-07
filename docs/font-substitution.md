# Font Resolution & Substitution — §17.8

A DOCX names fonts it does not carry. `Calibri` and `Cambria` are Microsoft
fonts that are absent on most Linux hosts and on many Macs. Resolution turns a
requested `(family, weight, slant)` into a concrete Skia `Typeface`, preferring
fidelity and degrading predictably.

Source: `src/render/fonts.rs`.

## `FontRegistry` — one owner per render

`FontRegistry` is the single source of truth for typeface data within one
render. It owns:

- the document's embedded-font bytes (deobfuscated upstream by the parser per
  §17.8.3.3), and
- a cache of resolved Skia `Typeface`s keyed by `TypefaceKey`
  (lowercased family + weight + slant).

A registry is constructed per render (`FontRegistry::build`) and passed by
reference to layout and paint.

### Why not a thread-local cache

An earlier implementation cached typefaces in a `thread_local!` map. That is
now a **correctness bug**, not just a design preference: the
[font subsetting](font-subsetting.md) pass mutates typefaces in place after
layout. A process-lifetime cache would leak a subsetted typeface — one whose
glyph coverage is limited to *the previous document* — into the next render,
producing missing glyphs that depend on conversion order.

Per-render ownership makes that leakage impossible by construction. Do not
reintroduce a global typeface cache.

## The resolution chain

`FontRegistry::resolve(family, style)` is cached after first call. On a miss,
`resolve_uncached` tries six steps in order:

1. **Embedded fonts** — anything in `word/fonts/*.odttf` wins outright. This is
   the highest-fidelity option: it is the exact font the author used.
2. **Exact system match** — `match_family_style`, accepted only if the returned
   family name actually matches what was asked for (`is_exact_family`). Whether
   the matcher substitutes is **platform-dependent**: fontconfig routinely
   returns a fallback, so an unchecked match would swallow every miss and make
   steps 3–6 unreachable; CoreText declines instead — measured on macOS, every
   unknown name tried returns `None`, including the CSS generics `sans-serif`,
   `serif` and `monospace`. So the guard is inert on macOS and load-bearing on
   Linux, which is also why it is tested on the predicate rather than through
   `match_exact`: on a non-substituting host the rejection path cannot be
   reached from there, and the test would pass without the guard existing.
3. **Face-alias index** — a lazily-built map from PostScript names and style
   names to `(family, weight)`. This catches documents that name a *face*
   (`Arial-BoldMT`, `HelveticaNeue-Light`) where Skia indexes the *family*.
   Ambiguous names are recorded as `Ambiguous` and skipped rather than guessed.
4. **Localized East Asian family aliases** — narrowly maps names that Windows
   font APIs expose under a different canonical family. This includes common
   localized system names and the frequently referenced, non-embedded
   `方正小标宋简体`, which falls back to the Song-family `SimSun` instead of a
   CJK-incapable generic UI font.
5. **Metric-compatible substitution** — `FONT_SUBSTITUTIONS`, tried in order.
   The table is keyed by *family*, so a face-qualified name falls back to its
   base family first: `"Segoe UI Light"` → strip the trailing weight word →
   `"Segoe UI"` → that family's substitutes. Without it a document naming a face
   walked straight past this step, and when step 3 also declined the name as
   `Ambiguous` (as it does for `"Segoe UI Light"` on macOS) there was no path at
   all from a face name to its family's substitutes. The longest weight suffix
   wins, and the suffix must be a separate trailing word — `"Highlight"` is not
   face-qualified.
6. **System default** — `legacy_make_typeface(None, style)`.

Every step logs at `debug`, so `RUST_LOG=debug` shows exactly which arm fired
for each family — the fastest way to diagnose a font-fidelity complaint.

### OOXML font slots and East Asian theme faces

`w:rFonts` does not name one font for a whole run. It has separate `ascii`,
`hAnsi`, `eastAsia`, and `cs` slots, and one `<w:r>` may contain characters
from more than one slot. The fragment collector therefore splits mixed Latin
and East Asian text at strong-script boundaries before measuring it. Neutral
punctuation remains with the preceding strong script.

For `majorEastAsia` and `minorEastAsia`, a DrawingML theme may leave the
generic East Asian typeface empty and provide only script-specific faces such
as `Hans`, `Hant`, `Jpan`, and `Hang`. Resolution checks those script faces
using `w:lang/@w:eastAsia` plus the text itself. Documents that reference the
standard Office theme but omit the theme part use the corresponding Office
fallback family (`SimSun`, `PMingLiU`, `Yu Mincho`, or `Malgun Gothic`).

> **Read that log with care: most of its lines describe fonts nothing draws.**
> `FontRegistry::build` calls `preload`, which resolves all four style variants
> of every family the document mentions, whether or not any run uses them. So a
> document that embeds `Aptos` Regular and Bold still logs `'Aptos' … Italic →
> system default 'Helvetica'` — which reads like a fidelity bug and is not one,
> because no italic Aptos run exists. Confirm against the `(family, bold,
> italic)` combinations actually emitted as draw commands before chasing a
> decision line. This cost one investigation during the H2 review.

### Cost

Step 3 builds its index over the **entire host font system** — 213 families /
701 faces on the review machine, instantiating a typeface per face to read its
PostScript name. Measured: **434 ms cold, ~40 ms warm**, of which ~31 ms is the
enumeration alone. It is `OnceCell`-guarded, so once per render — but every
corpus document reaches it, because every one names at least one family the host
does not have (2 to 44 such families per document, counting the normal style
alone). Dropping the eager `preload` would therefore *not* avoid it.

Sharing the index across renders would remove the cost, and unlike the typeface
cache it holds no font bytes — only family names and weights — so it would not
reintroduce the leak that made `FontRegistry` per-render. It is not shared
because `render_with_font_mgr` accepts a caller-supplied `FontMgr`, and an index
built from one manager is wrong for another (`FontMgr::empty()` being the case
that would break loudest). Fixing it properly means keying the index to its
manager, which is why it is recorded here rather than done.

### `FONT_SUBSTITUTIONS`

Metric-compatible means *same advance widths*, so line breaks and page counts
match Word even though glyph shapes differ. This is why Carlito/Caladea are
used rather than a generic sans/serif.

| Requested | Substitutes, in order |
|---|---|
| Century Schoolbook | Segoe Print, Century, Liberation Serif, Noto Serif |
| Calibri | Carlito, Liberation Sans, Noto Sans |
| Cambria | Caladea, Liberation Serif, Noto Serif |
| Arial | Liberation Sans, Noto Sans, Helvetica |
| Times New Roman | Liberation Serif, Noto Serif, Times |
| Courier New | Liberation Mono, Noto Sans Mono, Courier |
| Verdana | DejaVu Sans, Noto Sans |
| Georgia | DejaVu Serif, Noto Serif |
| Trebuchet MS | Ubuntu, Noto Sans |
| Consolas | Inconsolata, Liberation Mono, Noto Sans Mono |
| Segoe UI | Noto Sans, Liberation Sans |

Substitution is a *host-dependent* step: the same DOCX can paginate differently
on macOS and Linux depending on what is installed. When cross-platform output
differs, check this chain first.

## The two narrow resolvers

`resolve` always returns something. Two callers must not accept a fallback:

- **`resolve_exact`** — embedded or exact system match only. Used by the emoji
  pipeline, where substituting a non-emoji typeface for a missing color-emoji
  font is never correct; better to fail over to the monochrome path.
- **`resolve_system_only`** — bypasses the embedded index entirely. Word's font
  subsetter strips color glyph tables (`sbix`/`CBDT`/`COLR`/`SVG`) when it
  embeds an emoji font, so a DOCX-embedded "Segoe UI Emoji" carries the right
  family name but **no color glyphs**. It must not satisfy emoji resolution.

  This is a deliberate portability boundary, not an oversight: a document that
  ships its own emoji font does not get it back, and the same document renders
  with different emoji artwork on macOS, Windows and Linux. It applies only to
  the color-emoji path — ordinary embedded text fonts (§17.8) are honoured
  normally. See `src/render/emoji/resolve.rs`.

## Typeface identity and the subsetting contract

`TypefaceId` wraps Skia's `Typeface::unique_id` and is the join key with the
subsetting pass's `CodepointUsage`.

`TypefaceOrigin` records where a typeface came from — `Embedded { id }` or
`System { typeface_id }` — which is what tells byte extraction whether to read
the registry's own bytes or call `to_font_data`.

`replace_typeface_by_id` exists because **one typeface can be reachable from
several cache keys**. A document using both `Calibri` and `Carlito` has two
`TypefaceKey`s pointing at one Skia typeface; subsetting must update all of
them, or the un-updated key would keep painting with the pre-subset typeface.
The method returns the number of entries updated.

`preload(families)` warms all four style variants per family up front, so
layout's measurement loop never takes the slow path.

## Spec references

- **ECMA-376 §17.8** — font embedding and the `w:fonts` part.
- **ECMA-376 §17.8.3.3** — embedded-font obfuscation; deobfuscated by the
  parser before the registry ever sees bytes.
