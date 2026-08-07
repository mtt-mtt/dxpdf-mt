//! Emoji rendering pipeline.
//!
//! Three stages: cluster classification (UAX #29 + UTS #51) in [`cluster`],
//! controlled/host color emoji typeface resolution in [`resolve`], and Skia
//! raster-backend rasterization with a per-render cache in [`raster`];
//! [`shape`] drives Skia's HarfBuzz so multi-codepoint sequences ligate.
//!
//! Skia's PDF backend cannot emit colored glyph tables (COLR/CPAL, CBDT/CBLC,
//! sbix, SVG-in-OT) even when the resolved typeface carries them, but its
//! raster backend honours all four — hence rasterize-then-embed rather than
//! a text run.
//!
//! The rest of the renderer interacts with this module through typed ADTs;
//! no string-name allowlists, with one license-tracked controlled Noto asset.

pub mod cluster;
pub mod raster;
pub mod resolve;
pub mod shape;
