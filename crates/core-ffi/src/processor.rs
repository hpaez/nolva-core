//! Adapters from the document domain's ports to the image and PDF crates.
//! Swapping implementations (e.g. OpenCV for detection) only touches this file.

use std::path::Path;

use std::path::PathBuf;

use document_engine::{
    CornerDetection, Crop, DocumentExporter, ExportSummary, PageFilter, PageProcessor, PaperSize,
    PdfExportOptions, PortError, ProcessingError,
};
use image_engine::{
    Filter, ImageError, apply_filter, correct_perspective, load_oriented, save_jpeg,
};
use pdf_engine::{PageSize, PdfError, PdfOptions, write_pdf};
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

    fn render_page(
        &self,
        original: &Path,
        crop: Option<&Crop>,
        filter: PageFilter,
        output: &Path,
    ) -> Result<(), ProcessingError> {
        let image = load_oriented(original).map_err(processing_error)?;
        let page = match crop {
            Some(crop) => {
                correct_perspective(&image, &crop_to_quad(crop)).map_err(processing_error)?
            }
            None => image,
        };
        let page = match filter {
            PageFilter::Original => page,
            PageFilter::Grayscale => apply_filter(&page, Filter::Grayscale),
            PageFilter::BlackAndWhite => apply_filter(&page, Filter::BlackAndWhite),
            PageFilter::Enhanced => apply_filter(&page, Filter::Enhanced),
        };
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

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PdfDocumentExporter;

impl DocumentExporter for PdfDocumentExporter {
    fn export_pdf(
        &self,
        title: &str,
        images: &[PathBuf],
        options: &PdfExportOptions,
        output: &Path,
    ) -> Result<ExportSummary, ProcessingError> {
        let options = PdfOptions {
            page_size: match options.paper_size {
                PaperSize::FitImage => PageSize::FitImage,
                PaperSize::A4 => PageSize::A4,
                PaperSize::Letter => PageSize::Letter,
                PaperSize::Legal => PageSize::Legal,
            },
            margin_mm: options.margin_mm,
        };
        let summary = write_pdf(title, images, &options, output).map_err(|e| match e {
            PdfError::UnsupportedFormat(_) => ProcessingError::UnsupportedFormat,
            other => ProcessingError::Failed(PortError::new("export pdf", other)),
        })?;
        Ok(ExportSummary {
            page_count: summary.page_count,
            size_bytes: summary.size_bytes,
        })
    }
}
