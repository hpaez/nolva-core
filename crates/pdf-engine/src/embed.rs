use std::path::Path;

use image_engine::{ImageError, UprightJpeg, upright_jpeg};

use crate::PdfError;

/// JPEG bytes that display correctly in a PDF (viewers ignore EXIF), ready
/// to embed with `DCTDecode`.
pub(crate) fn prepare(path: &Path) -> Result<UprightJpeg, PdfError> {
    upright_jpeg(path).map_err(|source| match source {
        ImageError::UnsupportedFormat(p) => PdfError::UnsupportedFormat(p),
        ImageError::NotFound(_) => PdfError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("page image not found: {}", path.display()),
        )),
        source => PdfError::Image {
            path: path.to_path_buf(),
            source,
        },
    })
}
