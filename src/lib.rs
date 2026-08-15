pub mod docx;
pub mod error;
pub mod field;
pub mod model;
#[doc(hidden)]
pub mod path_io;
pub mod render;

pub use docx::zip::PackageLimits;
pub use error::Error;
pub use render::fonts::FontPack;
pub use render::{RenderOptions, DEFAULT_IMAGE_DPI, MIN_IMAGE_DPI};

/// Convert raw DOCX bytes into PDF bytes using default [`RenderOptions`].
pub fn convert(docx_bytes: &[u8]) -> Result<Vec<u8>, Error> {
    convert_with_options(docx_bytes, &RenderOptions::default())
}

/// Convert raw DOCX bytes into PDF bytes with caller-supplied [`RenderOptions`]
/// (e.g. a non-default embedded-image DPI).
pub fn convert_with_options(docx_bytes: &[u8], options: &RenderOptions) -> Result<Vec<u8>, Error> {
    convert_with_options_and_limits(docx_bytes, options, &PackageLimits::default())
}

/// Convert DOCX bytes with explicit rendering and package resource limits.
pub fn convert_with_options_and_limits(
    docx_bytes: &[u8],
    options: &RenderOptions,
    limits: &PackageLimits,
) -> Result<Vec<u8>, Error> {
    use std::time::Instant;

    let t0 = Instant::now();
    let document = crate::docx::parse_with_limits(docx_bytes, limits)?;
    log::debug!("Parse:  {:?}", t0.elapsed());

    let t1 = Instant::now();
    let pdf_bytes = crate::render::render(document, options)?;
    log::debug!("Render: {:?}", t1.elapsed());

    log::debug!("Total:  {:?}", t0.elapsed());
    Ok(pdf_bytes)
}

/// Convert with a reusable controlled font pack.
///
/// Servers should create `font_mgr` and `font_pack` once, then reuse both for
/// every conversion. The pack is process-local and does not install fonts into
/// the operating system.
pub fn convert_with_options_and_font_pack(
    docx_bytes: &[u8],
    options: &RenderOptions,
    limits: &PackageLimits,
    font_mgr: &skia_safe::FontMgr,
    font_pack: &FontPack,
) -> Result<Vec<u8>, Error> {
    use std::time::Instant;

    let t0 = Instant::now();
    let document = crate::docx::parse_with_limits(docx_bytes, limits)?;
    log::debug!("Parse:  {:?}", t0.elapsed());

    let t1 = Instant::now();
    let pdf_bytes = crate::render::render_with_font_mgr_and_font_pack(
        document,
        font_mgr,
        Some(font_pack),
        options,
    )?;
    log::debug!("Render: {:?}", t1.elapsed());
    log::debug!("Total:  {:?}", t0.elapsed());
    Ok(pdf_bytes)
}

/// Convenience entry point that loads a font directory for one conversion.
/// Batch and server callers should prefer [`convert_with_options_and_font_pack`]
/// so the directory is read only once.
pub fn convert_with_options_and_font_dir(
    docx_bytes: &[u8],
    options: &RenderOptions,
    limits: &PackageLimits,
    font_dir: impl AsRef<std::path::Path>,
) -> Result<Vec<u8>, Error> {
    let font_mgr = skia_safe::FontMgr::new();
    let font_pack = FontPack::load_dir(&font_mgr, font_dir)?;
    convert_with_options_and_font_pack(docx_bytes, options, limits, &font_mgr, &font_pack)
}

// --- Python bindings (enabled with `python` feature) ---

#[cfg(feature = "python")]
mod python {
    use pyo3::exceptions::PyRuntimeError;
    use pyo3::prelude::*;

    /// Convert DOCX bytes to PDF bytes.
    ///
    /// `image_dpi` sets the target resolution (pixels per inch) embedded raster
    /// images are downsampled to; defaults to 220.
    #[pyfunction]
    #[pyo3(signature = (docx_bytes, image_dpi = crate::DEFAULT_IMAGE_DPI, font_dir = None))]
    fn convert(docx_bytes: &[u8], image_dpi: f32, font_dir: Option<&str>) -> PyResult<Vec<u8>> {
        let options = crate::RenderOptions::default().with_image_dpi(image_dpi);
        let result = if let Some(font_dir) = font_dir {
            crate::convert_with_options_and_font_dir(
                docx_bytes,
                &options,
                &crate::PackageLimits::default(),
                font_dir,
            )
        } else {
            crate::convert_with_options(docx_bytes, &options)
        };
        result.map_err(|e| PyRuntimeError::new_err(e.to_string()))
    }

    /// Convert a DOCX file to a PDF file.
    ///
    /// `image_dpi` sets the target resolution (pixels per inch) embedded raster
    /// images are downsampled to; defaults to 220.
    #[pyfunction]
    #[pyo3(signature = (input, output, image_dpi = crate::DEFAULT_IMAGE_DPI, font_dir = None))]
    fn convert_file(
        input: &str,
        output: &str,
        image_dpi: f32,
        font_dir: Option<&str>,
    ) -> PyResult<()> {
        let docx_bytes = crate::path_io::read(input)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to read {input}: {e}")))?;
        let options = crate::RenderOptions::default().with_image_dpi(image_dpi);
        let pdf_bytes = if let Some(font_dir) = font_dir {
            crate::convert_with_options_and_font_dir(
                &docx_bytes,
                &options,
                &crate::PackageLimits::default(),
                font_dir,
            )
        } else {
            crate::convert_with_options(&docx_bytes, &options)
        }
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        crate::path_io::write(output, &pdf_bytes)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to write {output}: {e}")))?;
        Ok(())
    }

    /// A fast DOCX-to-PDF converter powered by Skia.
    #[pymodule]
    fn dxpdf(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add_function(wrap_pyfunction!(convert, m)?)?;
        m.add_function(wrap_pyfunction!(convert_file, m)?)?;
        Ok(())
    }
}
