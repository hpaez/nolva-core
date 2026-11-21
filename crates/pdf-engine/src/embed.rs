use std::fs;
use std::io::Cursor;
use std::path::Path;

use image::codecs::jpeg::{JpegDecoder, JpegEncoder};
use image::metadata::Orientation;
use image::{ColorType, ImageDecoder};
use image_engine::{JPEG_QUALITY, load_oriented};

use crate::PdfError;

/// A JPEG ready to embed with `DCTDecode`.
pub(crate) struct Embedded {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub grayscale: bool,
}

/// Returns JPEG bytes that display correctly in a PDF.
///
/// Fast path: an upright RGB or grayscale JPEG is used as-is (no quality
/// loss). Anything else is decoded with its orientation applied and
/// re-encoded.
pub(crate) fn prepare(path: &Path) -> Result<Embedded, PdfError> {
    let bytes = fs::read(path)?;
    if let Some(embedded) = as_is(&bytes) {
        return Ok(embedded);
    }

    let image = load_oriented(path).map_err(|source| match source {
        image_engine::ImageError::UnsupportedFormat(p) => PdfError::UnsupportedFormat(p),
        source => PdfError::Image {
            path: path.to_path_buf(),
            source,
        },
    })?;
    let grayscale = matches!(
        image.color(),
        ColorType::L8 | ColorType::L16 | ColorType::La8 | ColorType::La16
    );
    let mut jpeg = Vec::new();
    let encoder = JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), JPEG_QUALITY);
    let result = if grayscale {
        image.to_luma8().write_with_encoder(encoder)
    } else {
        image.to_rgb8().write_with_encoder(encoder)
    };
    result.map_err(|e| PdfError::Image {
        path: path.to_path_buf(),
        source: image_engine::ImageError::Encode(e),
    })?;
    Ok(Embedded {
        jpeg,
        width: image.width(),
        height: image.height(),
        grayscale,
    })
}

/// `Some` when the file is a JPEG a PDF viewer will display correctly.
fn as_is(bytes: &[u8]) -> Option<Embedded> {
    let mut decoder = JpegDecoder::new(Cursor::new(bytes)).ok()?;
    if decoder.orientation().ok()? != Orientation::NoTransforms {
        return None;
    }
    let grayscale = match decoder.color_type() {
        ColorType::L8 => true,
        ColorType::Rgb8 => false,
        // CMYK and friends need extra PDF parameters; re-encode instead.
        _ => return None,
    };
    let (width, height) = decoder.dimensions();
    Some(Embedded {
        jpeg: bytes.to_vec(),
        width,
        height,
        grayscale,
    })
}
