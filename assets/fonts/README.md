# Bundled fonts

dxpdf loads these fonts directly from application resources. They are not
installed into the host operating system.

| File | Purpose | Upstream revision | SHA-256 |
| --- | --- | --- | --- |
| `NotoSansSymbols2-Regular.ttf` | Monochrome business symbols such as `☐`, `☑`, `☒`, `✓`, and `✔` | `google/fonts@c28e08582e7bd36751febb3391142a5eb18bbb34` | `7D5FB73B7CA67A6798101741F5D280A3D016A56A197AFCD4199DBB57B4B82A21` |
| `NotoColorEmoji-COLRv1.ttf` | Deterministic raster artwork for Unicode emoji presentation; upstream file `fonts/Noto-COLRv1.ttf` | `googlefonts/noto-emoji@8998f5dd683424a73e2314a8c1f1e359c19e8742` (`v2.051`) | `0AE57FE58645638523BA35F388D93739D292539A9ACB84DF5700C81B1E1A28D2` |

Both fonts are distributed under the SIL Open Font License 1.1. The complete
license texts are stored beside the font files as
`OFL-NotoSansSymbols2.txt` and `OFL-NotoColorEmoji.txt`.

Rendering policy:

- Plain checkbox and check-mark characters use Noto Sans Symbols 2.
- U+FE0E preserves monochrome text presentation.
- U+FE0F and default emoji-presentation graphemes use Noto Color Emoji.
- The color font is rasterized by the existing emoji pipeline; the complete
  font is not embedded into each generated PDF.
