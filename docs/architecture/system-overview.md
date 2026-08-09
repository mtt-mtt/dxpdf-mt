# System Architecture

dxpdf-mt is a standalone DOCX-to-PDF engine. The near-term product target is a
controlled business-document converter; arbitrary Word/WPS pixel fidelity is a
separate, longer-term compatibility target.

## Conversion pipeline

```text
DOCX bytes
  -> bounded ZIP extraction and OOXML parsing
  -> immutable Document model
  -> style, theme, font, relationship and section resolution
  -> Skia-backed measurement and page layout
  -> per-document font subsetting
  -> DrawCommand painting
  -> PDF bytes
```

The implementation boundaries are:

| Layer | Path | Responsibility |
|---|---|---|
| Public API | `src/lib.rs`, `src/main.rs` | Rust, CLI and Python entry points |
| Package parsing | `src/docx/` | ZIP limits, relationships and OOXML schemas |
| Domain model | `src/model/` | Parser-independent ADTs and typed dimensions |
| Resolution | `src/render/resolve/` | Cascades, fonts, colors, images and sections |
| Layout | `src/render/layout/` | Paragraphs, tables, floats, headers and pagination |
| Font subsetting | `src/render/subset/` | Per-render codepoint collection and font reduction |
| PDF painting | `src/render/painter.rs` | The Skia/f32/PDF output boundary |

## Server deployment boundary

An application server should not load the native renderer directly in an HTTP
worker. The recommended production topology is:

```text
application -> bounded queue -> conversion supervisor -> limited dxpdf worker
```

The supervisor owns wall-clock timeouts, memory and concurrency limits, process
recycling, temporary-file cleanup and structured error classification. A
process-local `FontMgr` and controlled `FontPack` may be reused, but every
document keeps an independent `FontRegistry` so subsetted fonts cannot leak
between conversions.

## Architecture invariants

1. Do not use global scaling, universal line-height factors or filename-specific
   branches to improve aggregate page counts.
2. A new Word/OOXML behavior needs a minimal fixture and explicit evidence.
3. Locate the first layout divergence before changing pagination behavior.
4. Report PDF text extraction, DOCX source coverage, page structure and visual
   differences separately.
5. Treat missing text, clipping, overlap and changed business meaning as more
   severe than a page-count mismatch.
6. Generated results identify the source revision, release binary, font pack and
   execution parameters used to create them.
7. Do not commit private corpora, customer documents, generated PDFs, toolchains
   or build caches to this repository.

Behavior-specific design documentation lives directly under `docs/` and must be
updated with the code it describes.
