//! Adapter from the document domain's `PageProcessor` port to the image
//! crates. Swapping detection or processing implementations (e.g. to
//! OpenCV) only touches this file.

use std::path::Path;

use document_engine::{CornerDetection, Crop, PageProcessor, PortError, ProcessingError};
use image_engine::{ImageError, correct_perspective, load_oriented, save_jpeg};
use scanner_engine::{Point, Quad, detect_document};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ImagePageProcessor;

impl PageProcessor for ImagePageProcessor {
    fn detect_corners(&self, original: &Path) -> Result<Option<CornerDetection>, ProcessingError> {
        let image = load_oriented(original).map_err(processing_error)?;
        detect_document(&image)
            .map(|d| {
                Ok(CornerDetection {
                    crop: quad_to_crop(&d.quad)?,
                    confidence: d.confidence,
                })
            })
            .transpose()
    }

    fn render_crop(
        &self,
        original: &Path,
        crop: &Crop,
        output: &Path,
    ) -> Result<(), ProcessingError> {
        let image = load_oriented(original).map_err(processing_error)?;
        let page = correct_perspective(&image, &crop_to_quad(crop)).map_err(processing_error)?;
        save_jpeg(&page, output).map_err(processing_error)
    }
}

fn crop_to_quad(crop: &Crop) -> Quad {
    let [tl, tr, br, bl] = crop.corners().map(|(x, y)| Point::new(x, y));
    Quad {
        top_left: tl,
        top_right: tr,
        bottom_right: br,
        bottom_left: bl,
    }
}

fn quad_to_crop(quad: &Quad) -> Result<Crop, ProcessingError> {
    Crop::new(quad.corners().map(|p| (p.x, p.y)))
        .map_err(|e| ProcessingError::InvalidCorners(e.to_string()))
}

fn processing_error(error: ImageError) -> ProcessingError {
    match error {
        ImageError::UnsupportedFormat(_) => ProcessingError::UnsupportedFormat,
        ImageError::InvalidCorners(reason) => ProcessingError::InvalidCorners(reason.to_owned()),
        other => ProcessingError::Failed(PortError::new("process image", other)),
    }
}
