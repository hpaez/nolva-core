//! Public API of the Nolva core for the native apps, exported with UniFFI.
//!
//! Design rules for this surface:
//! - Pass identifiers and file paths across the boundary, never image bytes.
//! - Identifiers are strings (UUIDs) so they stay simple in Swift and Kotlin.
//! - Calls are synchronous and may block on I/O: native code must invoke them
//!   off the main thread (Dispatchers.IO / a background Task).
//! - Keep this crate thin: it only maps types and errors; logic lives in the
//!   engine crates.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use document_engine::{
    Crop, Document, DocumentError, DocumentId, DocumentService, Page, PageFileStore, PageId,
    PdfExportOptions, SystemClock,
};
use storage_engine::{DATABASE_FILE_NAME, LocalPageFileStore, SqliteDocumentRepository};

mod processor;

use processor::{ImagePageProcessor, PdfDocumentExporter};

uniffi::setup_scaffolding!();

/// Version of the core library, for diagnostics and compatibility checks.
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// A grayscale camera preview frame: the luminance (Y) plane as camera APIs
/// deliver it (Android `ImageProxy.planes[0]`, iOS `CVPixelBuffer` plane 0).
#[derive(Debug, Clone, uniffi::Record)]
pub struct CameraFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes from the start of one row to the next; at least `width`.
    pub row_stride: u32,
    pub luma: Vec<u8>,
    /// Clockwise rotation that makes the frame upright: 0, 90, 180 or 270.
    pub rotation_degrees: u32,
}

/// Finds the sheet in a live camera frame, for drawing a guide over the
/// preview and capturing automatically. Corners are normalized to the
/// upright frame. Takes a few milliseconds; call it from the camera's
/// analysis thread, not the main thread. Needs no `NolvaCore`.
#[uniffi::export]
pub fn detect_corners_in_frame(frame: CameraFrame) -> Result<Option<CornerDetection>, NolvaError> {
    let luma = scanner_engine::LumaFrame {
        width: frame.width,
        height: frame.height,
        row_stride: frame.row_stride,
        pixels: &frame.luma,
        rotation_degrees: frame.rotation_degrees,
    };
    let detection = scanner_engine::detect_document_in_frame(&luma).map_err(|e| {
        NolvaError::InvalidArgument {
            reason: e.to_string(),
        }
    })?;
    Ok(detection.map(|d| {
        let [tl, tr, br, bl] = d.quad.corners().map(|p| NormalizedPoint { x: p.x, y: p.y });
        CornerDetection {
            corners: PageCorners {
                top_left: tl,
                top_right: tr,
                bottom_right: br,
                bottom_left: bl,
            },
            confidence: d.confidence,
        }
    }))
}

/// Configuration supplied by the host app at startup.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CoreConfig {
    /// Absolute path of a private, app-owned directory where the core keeps
    /// its database and files (e.g. `filesDir` on Android, Application
    /// Support on iOS). Created if missing.
    pub data_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DocumentInfo {
    pub id: String,
    pub title: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub page_count: u32,
    /// Absolute path of the first page's image (processed if it has one),
    /// for thumbnails. `None` while the document has no pages.
    pub cover_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PageInfo {
    pub id: String,
    pub document_id: String,
    pub position: u32,
    /// Absolute path of the original image, resolved for the current install.
    /// Never modified.
    pub original_path: String,
    /// Absolute path of the cropped and/or filtered image, if the page has
    /// a crop or a filter. Show this when present, else the original.
    pub processed_path: Option<String>,
    /// Corners used for `processed_path`, to reopen the crop editor where
    /// the user left it.
    pub corners: Option<PageCorners>,
    /// Filter applied on top of the crop.
    pub filter: PageFilter,
    pub created_at_ms: i64,
}

/// A point in normalized image coordinates (`0.0..=1.0`, origin top left)
/// of the original image as displayed (EXIF orientation applied).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct NormalizedPoint {
    pub x: f32,
    pub y: f32,
}

/// The four corners of a page outline.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct PageCorners {
    pub top_left: NormalizedPoint,
    pub top_right: NormalizedPoint,
    pub bottom_right: NormalizedPoint,
    pub bottom_left: NormalizedPoint,
}

/// An automatically detected page outline.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct CornerDetection {
    pub corners: PageCorners,
    /// `0.0..=1.0`. Below ~0.6, ask the user to check the corners.
    pub confidence: f32,
}

/// Look applied to a page on top of its crop. Always rendered from the
/// original, so switching filters never degrades the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PageFilter {
    /// Colors as photographed.
    Original,
    Grayscale,
    /// Pure black ink on white paper: smallest files, best for text.
    BlackAndWhite,
    /// Evens out lighting and shadows, whitens the paper, keeps colors.
    Enhanced,
}

impl From<PageFilter> for document_engine::PageFilter {
    fn from(filter: PageFilter) -> Self {
        match filter {
            PageFilter::Original => Self::Original,
            PageFilter::Grayscale => Self::Grayscale,
            PageFilter::BlackAndWhite => Self::BlackAndWhite,
            PageFilter::Enhanced => Self::Enhanced,
        }
    }
}

impl From<document_engine::PageFilter> for PageFilter {
    fn from(filter: document_engine::PageFilter) -> Self {
        match filter {
            document_engine::PageFilter::Original => Self::Original,
            document_engine::PageFilter::Grayscale => Self::Grayscale,
            document_engine::PageFilter::BlackAndWhite => Self::BlackAndWhite,
            document_engine::PageFilter::Enhanced => Self::Enhanced,
        }
    }
}

/// Paper size for exported PDFs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PaperSize {
    /// Each page takes the proportions of its image (good for receipts).
    FitImage,
    A4,
    Letter,
    /// 8.5 × 14 in, the usual "oficio".
    Legal,
}

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct PdfOptions {
    pub paper_size: PaperSize,
    /// White margin around each image on fixed-size pages, in millimetres.
    pub margin_mm: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct PdfExportInfo {
    pub page_count: u32,
    pub size_bytes: u64,
}

/// Errors surfaced to Swift (`throws`) and Kotlin (exceptions).
///
/// `reason` is for logs and diagnostics, not for end users: map the variant
/// to localized UI text on each platform. (The field must not be called
/// `message`: it would clash with `Throwable.message` in the Kotlin bindings.)
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum NolvaError {
    #[error("invalid argument: {reason}")]
    InvalidArgument { reason: String },
    #[error("not found: {reason}")]
    NotFound { reason: String },
    #[error("unsupported: {reason}")]
    Unsupported { reason: String },
    #[error("storage error: {reason}")]
    Storage { reason: String },
}

impl From<DocumentError> for NolvaError {
    fn from(err: DocumentError) -> Self {
        let reason = err.to_string();
        match err {
            DocumentError::InvalidTitle { .. }
            | DocumentError::InvalidId { .. }
            | DocumentError::InvalidCorners { .. }
            | DocumentError::EmptyDocument(_) => Self::InvalidArgument { reason },
            DocumentError::DocumentNotFound(_)
            | DocumentError::PageNotFound(_)
            | DocumentError::SourceImageNotFound(_) => Self::NotFound { reason },
            DocumentError::UnsupportedImageFormat { .. } => Self::Unsupported { reason },
            DocumentError::Storage(_) => Self::Storage { reason },
        }
    }
}

type Service = DocumentService<
    SqliteDocumentRepository,
    LocalPageFileStore,
    ImagePageProcessor,
    PdfDocumentExporter,
    SystemClock,
>;

/// Entry point of the core. Create one per process and share it.
#[derive(Debug, uniffi::Object)]
pub struct NolvaCore {
    service: Service,
}

#[uniffi::export]
impl NolvaCore {
    #[uniffi::constructor]
    pub fn new(config: CoreConfig) -> Result<Arc<Self>, NolvaError> {
        let data_dir = PathBuf::from(&config.data_dir);
        if !data_dir.is_absolute() {
            return Err(NolvaError::InvalidArgument {
                reason: "data_dir must be an absolute path".into(),
            });
        }
        let files = LocalPageFileStore::new(&data_dir).map_err(storage_error)?;
        let repository = SqliteDocumentRepository::open(&data_dir.join(DATABASE_FILE_NAME))
            .map_err(storage_error)?;
        Ok(Arc::new(Self {
            service: DocumentService::new(
                repository,
                files,
                ImagePageProcessor,
                PdfDocumentExporter,
                SystemClock,
            ),
        }))
    }

    pub fn create_document(&self, title: String) -> Result<DocumentInfo, NolvaError> {
        Ok(self.document_info(self.service.create_document(&title)?))
    }

    pub fn list_documents(&self) -> Result<Vec<DocumentInfo>, NolvaError> {
        Ok(self
            .service
            .list_documents()?
            .into_iter()
            .map(|d| self.document_info(d))
            .collect())
    }

    pub fn rename_document(
        &self,
        document_id: String,
        title: String,
    ) -> Result<DocumentInfo, NolvaError> {
        let id: DocumentId = document_id.parse()?;
        Ok(self.document_info(self.service.rename_document(id, &title)?))
    }

    /// Permanently deletes a document, its pages and all their images.
    pub fn delete_document(&self, document_id: String) -> Result<(), NolvaError> {
        let id: DocumentId = document_id.parse()?;
        Ok(self.service.delete_document(id)?)
    }

    /// Permanently deletes a page and its images; later pages move up.
    pub fn delete_page(&self, page_id: String) -> Result<(), NolvaError> {
        let id: PageId = page_id.parse()?;
        Ok(self.service.delete_page(id)?)
    }

    /// Moves a page to `to_position` (0-based; past the end means last) and
    /// returns the document's pages in their new order.
    pub fn move_page(
        &self,
        page_id: String,
        to_position: u32,
    ) -> Result<Vec<PageInfo>, NolvaError> {
        let id: PageId = page_id.parse()?;
        Ok(self
            .service
            .move_page(id, to_position)?
            .into_iter()
            .map(|p| self.page_info(p))
            .collect())
    }

    /// Copies the image at `image_path` into Nolva's storage as the last page
    /// of the document. The source file is left untouched.
    pub fn add_page(
        &self,
        document_id: String,
        image_path: String,
    ) -> Result<PageInfo, NolvaError> {
        let id: DocumentId = document_id.parse()?;
        let page = self.service.add_page(id, Path::new(&image_path))?;
        Ok(self.page_info(page))
    }

    pub fn list_pages(&self, document_id: String) -> Result<Vec<PageInfo>, NolvaError> {
        let id: DocumentId = document_id.parse()?;
        Ok(self
            .service
            .list_pages(id)?
            .into_iter()
            .map(|p| self.page_info(p))
            .collect())
    }

    /// Writes the document as a PDF to `output_path` (absolute; its folder
    /// must exist), one page per page, preferring cropped images. The file
    /// appears only once complete. Call off the main thread.
    pub fn export_pdf(
        &self,
        document_id: String,
        output_path: String,
        options: PdfOptions,
    ) -> Result<PdfExportInfo, NolvaError> {
        let id: DocumentId = document_id.parse()?;
        let output = PathBuf::from(output_path);
        if !output.is_absolute() {
            return Err(NolvaError::InvalidArgument {
                reason: "output_path must be absolute".into(),
            });
        }
        let options = PdfExportOptions {
            paper_size: match options.paper_size {
                PaperSize::FitImage => document_engine::PaperSize::FitImage,
                PaperSize::A4 => document_engine::PaperSize::A4,
                PaperSize::Letter => document_engine::PaperSize::Letter,
                PaperSize::Legal => document_engine::PaperSize::Legal,
            },
            margin_mm: options.margin_mm,
        };
        let summary = self.service.export_pdf(id, &output, &options)?;
        Ok(PdfExportInfo {
            page_count: summary.page_count,
            size_bytes: summary.size_bytes,
        })
    }

    pub fn get_page(&self, page_id: String) -> Result<PageInfo, NolvaError> {
        let id: PageId = page_id.parse()?;
        Ok(self.page_info(self.service.get_page(id)?))
    }

    /// Finds the page outline in the page's original photo. Returns `None`
    /// when nothing document-like is found (start the editor at the full
    /// image). CPU-heavy: call off the main thread.
    pub fn detect_page_corners(
        &self,
        page_id: String,
    ) -> Result<Option<CornerDetection>, NolvaError> {
        let id: PageId = page_id.parse()?;
        Ok(self.service.detect_corners(id)?.map(|d| CornerDetection {
            corners: d.crop.into(),
            confidence: d.confidence,
        }))
    }

    /// Crops and straightens the page to `corners`, storing the result as
    /// the page's processed image. Can be called again to re-crop; the
    /// original is never modified. CPU-heavy: call off the main thread.
    pub fn crop_page(&self, page_id: String, corners: PageCorners) -> Result<PageInfo, NolvaError> {
        let id: PageId = page_id.parse()?;
        let crop = Crop::try_from(corners)?;
        Ok(self.page_info(self.service.crop_page(id, crop)?))
    }

    /// Applies `filter` to the page, on top of its crop if it has one.
    /// CPU-heavy (up to about a second for a 12 MP photo): call it off the
    /// main thread.
    pub fn set_page_filter(
        &self,
        page_id: String,
        filter: PageFilter,
    ) -> Result<PageInfo, NolvaError> {
        let id: PageId = page_id.parse()?;
        Ok(self.page_info(self.service.set_page_filter(id, filter.into())?))
    }
}

impl NolvaCore {
    fn document_info(&self, d: Document) -> DocumentInfo {
        let files = self.service.files();
        DocumentInfo {
            id: d.id.to_string(),
            title: d.title,
            created_at_ms: d.created_at_ms,
            updated_at_ms: d.updated_at_ms,
            page_count: d.page_count,
            cover_path: d
                .cover_path
                .map(|p| files.resolve(&p).to_string_lossy().into_owned()),
        }
    }

    fn page_info(&self, page: Page) -> PageInfo {
        let files = self.service.files();
        let absolute = |p: &std::path::Path| files.resolve(p).to_string_lossy().into_owned();
        PageInfo {
            id: page.id.to_string(),
            document_id: page.document_id.to_string(),
            position: page.position,
            original_path: absolute(&page.original_path),
            processed_path: page.processed_path.as_deref().map(absolute),
            corners: page.crop.map(PageCorners::from),
            filter: page.filter.into(),
            created_at_ms: page.created_at_ms,
        }
    }
}

impl From<Crop> for PageCorners {
    fn from(crop: Crop) -> Self {
        let [tl, tr, br, bl] = crop.corners().map(|(x, y)| NormalizedPoint { x, y });
        Self {
            top_left: tl,
            top_right: tr,
            bottom_right: br,
            bottom_left: bl,
        }
    }
}

impl TryFrom<PageCorners> for Crop {
    type Error = DocumentError;

    fn try_from(c: PageCorners) -> Result<Self, Self::Error> {
        Crop::new([c.top_left, c.top_right, c.bottom_right, c.bottom_left].map(|p| (p.x, p.y)))
    }
}

fn storage_error(err: document_engine::PortError) -> NolvaError {
    NolvaError::Storage {
        reason: err.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn core(dir: &tempfile::TempDir) -> Arc<NolvaCore> {
        NolvaCore::new(CoreConfig {
            data_dir: dir.path().to_string_lossy().into_owned(),
        })
        .unwrap()
    }

    #[test]
    fn end_to_end_create_add_list() {
        let data = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let img = src_dir.path().join("shot.png");
        std::fs::write(&img, b"png").unwrap();

        let core = core(&data);
        let doc = core.create_document("Contrato".into()).unwrap();
        let page = core
            .add_page(doc.id.clone(), img.to_string_lossy().into_owned())
            .unwrap();

        assert!(Path::new(&page.original_path).is_absolute());
        assert!(Path::new(&page.original_path).exists());
        assert_eq!(core.list_pages(doc.id.clone()).unwrap(), vec![page]);

        // Data survives a restart of the core.
        drop(core);
        let reopened = super::tests::core(&data);
        let docs = reopened.list_documents().unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].page_count, 1);
    }

    #[test]
    fn errors_map_to_ffi_variants() {
        let data = tempfile::tempdir().unwrap();
        let core = core(&data);

        assert!(matches!(
            core.create_document("  ".into()),
            Err(NolvaError::InvalidArgument { .. })
        ));
        assert!(matches!(
            core.list_pages("bad-id".into()),
            Err(NolvaError::InvalidArgument { .. })
        ));
        let doc = core.create_document("x".into()).unwrap();
        assert!(matches!(
            core.add_page(doc.id, "/missing/file.jpg".into()),
            Err(NolvaError::NotFound { .. })
        ));
    }

    /// Writes a PNG "photo": a light, tilted sheet on a dark desk.
    fn photo_of_tilted_sheet(dir: &std::path::Path) -> std::path::PathBuf {
        use image::{GrayImage, Luma};
        use imageproc::drawing::draw_polygon_mut;
        use imageproc::point::Point;

        let mut photo = GrayImage::from_pixel(900, 1200, Luma([50]));
        let corners = [(0.20, 0.12), (0.84, 0.17), (0.88, 0.86), (0.10, 0.82)]
            .map(|(x, y)| Point::new((x * 900.0) as i32, (y * 1200.0) as i32));
        draw_polygon_mut(&mut photo, &corners, Luma([225]));
        let path = dir.join("photo.png");
        photo.save(&path).unwrap();
        path
    }

    #[test]
    fn detect_and_crop_end_to_end() {
        let data = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let core = core(&data);
        let doc = core.create_document("Contrato".into()).unwrap();
        let page = core
            .add_page(
                doc.id.clone(),
                photo_of_tilted_sheet(src.path()).to_string_lossy().into(),
            )
            .unwrap();
        assert_eq!(page.processed_path, None);

        let detection = core
            .detect_page_corners(page.id.clone())
            .unwrap()
            .expect("sheet not found");
        assert!(detection.confidence > 0.8);
        assert!((detection.corners.top_left.x - 0.20).abs() < 0.02);

        let cropped = core.crop_page(page.id.clone(), detection.corners).unwrap();
        let first = cropped.processed_path.clone().expect("no processed image");
        assert!(Path::new(&first).is_file());
        assert_eq!(cropped.corners, Some(detection.corners));
        assert!(
            Path::new(&cropped.original_path).is_file(),
            "original must be kept"
        );

        // Re-cropping replaces the processed image and removes the old one.
        let recropped = core.crop_page(page.id.clone(), detection.corners).unwrap();
        let second = recropped.processed_path.clone().unwrap();
        assert_ne!(first, second);
        assert!(!Path::new(&first).exists());
        assert!(Path::new(&second).is_file());

        // The crop survives a restart.
        drop(core);
        let reopened = super::tests::core(&data);
        assert_eq!(reopened.get_page(page.id).unwrap(), recropped);
    }

    #[test]
    fn filters_combine_with_crops_end_to_end() {
        let data = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let core = core(&data);
        let doc = core.create_document("Recibo".into()).unwrap();
        let page = core
            .add_page(
                doc.id.clone(),
                photo_of_tilted_sheet(src.path()).to_string_lossy().into(),
            )
            .unwrap();
        assert_eq!(page.filter, PageFilter::Original);

        let bw = core
            .set_page_filter(page.id.clone(), PageFilter::BlackAndWhite)
            .unwrap();
        let bw_path = bw
            .processed_path
            .clone()
            .expect("filter must render an image");
        let bw_image = image::open(&bw_path).unwrap();
        assert_eq!(
            bw_image.color(),
            image::ColorType::L8,
            "black and white is stored as grayscale"
        );
        assert_eq!(bw_image.width(), 900, "no crop yet: the whole photo");

        let corners = core
            .detect_page_corners(page.id.clone())
            .unwrap()
            .unwrap()
            .corners;
        let cropped = core.crop_page(page.id.clone(), corners).unwrap();
        assert_eq!(
            cropped.filter,
            PageFilter::BlackAndWhite,
            "cropping keeps the filter"
        );
        assert!(
            !Path::new(&bw_path).exists(),
            "previous rendering is removed"
        );
        assert!(
            image::open(cropped.processed_path.as_ref().unwrap())
                .unwrap()
                .width()
                < 900
        );

        let enhanced = core
            .set_page_filter(page.id.clone(), PageFilter::Enhanced)
            .unwrap();
        assert!(enhanced.corners.is_some(), "filters keep the crop");
        assert_eq!(core.get_page(page.id.clone()).unwrap(), enhanced);

        // A filtered page exports like any other.
        let pdf = src.path().join("out.pdf");
        let options = PdfOptions {
            paper_size: PaperSize::A4,
            margin_mm: 0.0,
        };
        assert_eq!(
            core.export_pdf(doc.id, pdf.to_string_lossy().into(), options)
                .unwrap()
                .page_count,
            1
        );
    }

    #[test]
    fn crop_rejects_invalid_corners() {
        let data = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let core = core(&data);
        let doc = core.create_document("x".into()).unwrap();
        let page = core
            .add_page(
                doc.id,
                photo_of_tilted_sheet(src.path()).to_string_lossy().into(),
            )
            .unwrap();
        let p = |x, y| NormalizedPoint { x, y };

        let outside = PageCorners {
            top_left: p(-0.5, 0.0),
            top_right: p(1.0, 0.0),
            bottom_right: p(1.0, 1.0),
            bottom_left: p(0.0, 1.0),
        };
        let crossed = PageCorners {
            top_left: p(0.0, 0.0),
            top_right: p(1.0, 1.0),
            bottom_right: p(1.0, 0.0),
            bottom_left: p(0.0, 1.0),
        };
        for corners in [outside, crossed] {
            assert!(matches!(
                core.crop_page(page.id.clone(), corners),
                Err(NolvaError::InvalidArgument { .. })
            ));
        }
        assert_eq!(core.get_page(page.id).unwrap().processed_path, None);
    }

    #[test]
    fn exports_a_pdf_with_cropped_and_plain_pages() {
        let data = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let core = core(&data);
        let doc = core.create_document("Contrato".into()).unwrap();
        let photo = photo_of_tilted_sheet(src.path())
            .to_string_lossy()
            .into_owned();
        let first = core.add_page(doc.id.clone(), photo.clone()).unwrap();
        core.add_page(doc.id.clone(), photo).unwrap();
        let corners = core
            .detect_page_corners(first.id.clone())
            .unwrap()
            .unwrap()
            .corners;
        core.crop_page(first.id, corners).unwrap();
        let output = src.path().join("contrato.pdf");

        let info = core
            .export_pdf(
                doc.id,
                output.to_string_lossy().into(),
                PdfOptions {
                    paper_size: PaperSize::A4,
                    margin_mm: 5.0,
                },
            )
            .unwrap();

        assert_eq!(info.page_count, 2);
        let pdf = lopdf::Document::load(&output).unwrap();
        assert_eq!(pdf.get_pages().len(), 2);
    }

    #[test]
    fn exporting_an_empty_document_is_an_invalid_argument() {
        let data = tempfile::tempdir().unwrap();
        let core = core(&data);
        let doc = core.create_document("Vacío".into()).unwrap();
        let output = data.path().join("x.pdf").to_string_lossy().into_owned();
        let options = PdfOptions {
            paper_size: PaperSize::FitImage,
            margin_mm: 0.0,
        };

        assert!(matches!(
            core.export_pdf(doc.id.clone(), output, options),
            Err(NolvaError::InvalidArgument { .. })
        ));
        assert!(matches!(
            core.export_pdf(doc.id, "relative.pdf".into(), options),
            Err(NolvaError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn manage_documents_and_pages_end_to_end() {
        let data = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let core = core(&data);
        let doc = core.create_document("Borrador".into()).unwrap();
        let photo = photo_of_tilted_sheet(src.path())
            .to_string_lossy()
            .into_owned();
        let pages: Vec<_> = (0..3)
            .map(|_| core.add_page(doc.id.clone(), photo.clone()).unwrap())
            .collect();

        assert_eq!(doc.cover_path, None);
        let renamed = core
            .rename_document(doc.id.clone(), "Contrato".into())
            .unwrap();
        assert_eq!(renamed.title, "Contrato");
        assert_eq!(renamed.cover_path.as_ref(), Some(&pages[0].original_path));
        assert!(Path::new(renamed.cover_path.as_ref().unwrap()).is_absolute());

        let order = core.move_page(pages[2].id.clone(), 0).unwrap();
        assert_eq!(
            order.iter().map(|p| &p.id).collect::<Vec<_>>(),
            vec![&pages[2].id, &pages[0].id, &pages[1].id]
        );

        core.delete_page(pages[0].id.clone()).unwrap();
        assert!(
            !Path::new(&pages[0].original_path).exists(),
            "page image must be deleted"
        );
        let remaining = core.list_pages(doc.id.clone()).unwrap();
        assert_eq!(
            remaining.iter().map(|p| p.position).collect::<Vec<_>>(),
            vec![0, 1]
        );

        core.delete_document(doc.id.clone()).unwrap();
        assert!(core.list_documents().unwrap().is_empty());
        assert!(
            remaining
                .iter()
                .all(|p| !Path::new(&p.original_path).exists())
        );
        assert!(matches!(
            core.delete_document(doc.id),
            Err(NolvaError::NotFound { .. })
        ));
    }

    #[test]
    fn detects_sheets_in_camera_frames() {
        use image::{GrayImage, Luma};
        use imageproc::drawing::draw_polygon_mut;
        use imageproc::point::Point;

        let mut img = GrayImage::from_pixel(480, 640, Luma([40]));
        let corners = [(60, 50), (420, 60), (430, 590), (50, 600)].map(|(x, y)| Point::new(x, y));
        draw_polygon_mut(&mut img, &corners, Luma([220]));
        let frame = CameraFrame {
            width: 480,
            height: 640,
            row_stride: 480,
            luma: img.into_raw(),
            rotation_degrees: 0,
        };

        let detection = detect_corners_in_frame(frame.clone())
            .unwrap()
            .expect("sheet not found");
        assert!(detection.confidence > 0.8);
        assert!((detection.corners.top_left.x - 60.0 / 480.0).abs() < 0.03);

        let bad = CameraFrame {
            rotation_degrees: 45,
            ..frame
        };
        assert!(matches!(
            detect_corners_in_frame(bad),
            Err(NolvaError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn relative_data_dir_is_rejected() {
        let result = NolvaCore::new(CoreConfig {
            data_dir: "relative/dir".into(),
        });
        assert!(matches!(result, Err(NolvaError::InvalidArgument { .. })));
    }
}
