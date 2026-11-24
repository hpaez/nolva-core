use std::fs;
use std::io::Cursor;
use std::path::Path;

use image::codecs::jpeg::{JpegDecoder, JpegEncoder};
use image::metadata::Orientation;
use image::{ColorType, ImageDecoder};

use crate::ImageError;
use crate::io::{JPEG_QUALITY, load_oriented};

/// A JPEG that displays correctly in documents that ignore EXIF orientation
/// (PDF, Word): its pixels are already upright.
#[derive(Debug, Clone)]
pub struct UprightJpeg {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub grayscale: bool,
}

/// Returns JPEG bytes of the image at `path`, upright.
///
/// Fast path: an upright RGB or grayscale JPEG is used as-is (no quality
/// loss). Anything else (EXIF-rotated photos, PNG, WebP, CMYK) is decoded
/// with its orientation applied and re-encoded.
pub fn upright_jpeg(path: &Path) -> Result<UprightJpeg, ImageError> {
    let bytes = fs::read(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ImageError::NotFound(path.to_path_buf()),
        _ => ImageError::Io(e),
    })?;
    if let Some(upright) = as_is(&bytes) {
        return Ok(upright);
    }

    let image = load_oriented(path)?;
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
    result.map_err(ImageError::Encode)?;
    Ok(UprightJpeg {
        bytes: jpeg,
        width: image.width(),
        height: image.height(),
        grayscale,
    })
}

/// `Some` when the bytes are a JPEG that already displays correctly.
fn as_is(bytes: &[u8]) -> Option<UprightJpeg> {
    let mut decoder = JpegDecoder::new(Cursor::new(bytes)).ok()?;
    if decoder.orientation().ok()? != Orientation::NoTransforms {
        return None;
    }
    let grayscale = match decoder.color_type() {
        ColorType::L8 => true,
        ColorType::Rgb8 => false,
        // CMYK and friends need extra parameters in PDF; re-encode instead.
        _ => return None,
    };
    let (width, height) = decoder.dimensions();
    Some(UprightJpeg {
        bytes: bytes.to_vec(),
        width,
        height,
        grayscale,
    })
}
