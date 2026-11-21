use std::fs::{self, File};
use std::io::BufWriter;
use std::path::Path;

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageDecoder, ImageReader};

use crate::ImageError;

/// JPEG quality for processed pages: visually lossless for documents while
/// keeping files a fraction of the PNG size.
pub const JPEG_QUALITY: u8 = 90;

/// Decodes the image at `path` and applies its EXIF orientation.
///
/// Phone cameras usually store the sensor's raw rotation and record the
/// intended one in EXIF; ignoring it would put detected corners and crops
/// 90° off from what the user sees on screen.
pub fn load_oriented(path: &Path) -> Result<DynamicImage, ImageError> {
    if !path.is_file() {
        return Err(ImageError::NotFound(path.to_path_buf()));
    }
    let reader = ImageReader::open(path)?.with_guessed_format()?;
    if reader.format().is_none() {
        return Err(ImageError::UnsupportedFormat(path.to_path_buf()));
    }
    let mut decoder = reader.into_decoder().map_err(|e| match e {
        image::ImageError::Unsupported(_) => ImageError::UnsupportedFormat(path.to_path_buf()),
        other => ImageError::Decode(other),
    })?;
    let orientation = decoder.orientation().map_err(ImageError::Decode)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(ImageError::Decode)?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// Encodes `image` as JPEG at `path`, atomically: it is written to a sibling
/// temp file, flushed to disk and then renamed over the destination.
pub fn save_jpeg(image: &DynamicImage, path: &Path) -> Result<(), ImageError> {
    let partial = path.with_extension("jpg.partial");
    let result = (|| {
        let file = File::create(&partial)?;
        let mut writer = BufWriter::new(file);
        // JPEG has no alpha channel; documents never need one. Grayscale
        // pages stay single-channel, about a third of the size.
        let mut encoder = JpegEncoder::new_with_quality(&mut writer, JPEG_QUALITY);
        match image {
            DynamicImage::ImageLuma8(gray) => encoder.encode_image(gray),
            other => encoder.encode_image(&other.to_rgb8()),
        }
        .map_err(ImageError::Encode)?;
        let file = writer.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        fs::rename(&partial, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}
