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
    Crop, Document, DocumentError, DocumentId, DocumentService, FolderId, Page, PageFileStore,
    PageId, PageText, PdfExportOptions, Rotation, SystemClock, TextBounds, TextLine,
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
    /// When it was moved to the trash; `None` while it isn't.
    pub trashed_at_ms: Option<i64>,
    /// Folder it's in; `None` for the top level.
    pub folder_id: Option<String>,
    /// Bytes its page images take. Filled in by listings
    /// (`list_documents`, `list_trashed_documents`) and by restore and
    /// duplicate; 0 elsewhere.
    pub size_bytes: u64,
}

/// A folder of documents. Folders don't nest.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FolderInfo {
    pub id: String,
    pub name: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// Documents in it, not counting trashed ones.
    pub document_count: u32,
}

impl From<document_engine::Folder> for FolderInfo {
    fn from(f: document_engine::Folder) -> Self {
        Self {
            id: f.id.to_string(),
            name: f.name,
            created_at_ms: f.created_at_ms,
            updated_at_ms: f.updated_at_ms,
            document_count: f.document_count,
        }
    }
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
    /// Clockwise quarter turns applied after the crop: 0, 90, 180 or 270.
    pub rotation_degrees: u32,
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
    /// Print "n / total" at the bottom centre of each page. Off unless asked.
    #[uniffi(default = false)]
    pub page_numbers: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct PdfExportInfo {
    pub page_count: u32,
    pub size_bytes: u64,
}

/// Rectangle in normalized coordinates (`0.0..=1.0`) of the page image the
/// text was recognized in, origin at the top left.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct TextRect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// One line of recognized text, as the platform recognizer reports it.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct RecognizedLine {
    /// Block (paragraph or column) index in reading order; lines of the same
    /// block are consecutive.
    pub block: u32,
    pub text: String,
    pub bounds: TextRect,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PageTextInfo {
    pub page_id: String,
    /// All the text: lines separated by newlines, blocks by a blank line.
    pub text: String,
    pub lines: Vec<RecognizedLine>,
    pub recognized_at_ms: i64,
}

/// A page whose text matches a search.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TextSearchHit {
    pub document_id: String,
    pub page_id: String,
    pub page_position: u32,
    /// Short excerpt around the match.
    pub snippet: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct DocxOptions {
    /// Append each page's image after the text.
    pub include_images: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct DocxExportInfo {
    pub page_count: u32,
    pub size_bytes: u64,
    /// Pages exported without text because it wasn't recognized yet.
    pub pages_pending_text: u32,
}

/// What [`NolvaCore::remove_orphan_files`] cleaned up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct CleanupInfo {
    pub files_removed: u32,
    pub bytes_freed: u64,
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
    /// The data changed while the operation ran; it no longer applies.
    #[error("conflict: {reason}")]
    Conflict { reason: String },
}

impl From<DocumentError> for NolvaError {
    fn from(err: DocumentError) -> Self {
        let reason = err.to_string();
        match err {
            DocumentError::InvalidTitle { .. }
            | DocumentError::InvalidId { .. }
            | DocumentError::InvalidCorners { .. }
            | DocumentError::InvalidRotation { .. }
            | DocumentError::EmptyDocument(_)
            | DocumentError::InvalidText { .. } => Self::InvalidArgument { reason },
            DocumentError::PageImageChanged(_) => Self::Conflict { reason },
            DocumentError::DocumentNotFound(_)
            | DocumentError::PageNotFound(_)
            | DocumentError::FolderNotFound(_)
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

    /// Creates an empty document in a folder (`None`: the top level).
    pub fn create_document_in_folder(
        &self,
        title: String,
        folder_id: Option<String>,
    ) -> Result<DocumentInfo, NolvaError> {
        let folder = folder_id.map(|f| f.parse::<FolderId>()).transpose()?;
        Ok(self.document_info(self.service.create_document_in(&title, folder)?))
    }

    /// Creates a folder. Names follow the same rules as document titles.
    pub fn create_folder(&self, name: String) -> Result<FolderInfo, NolvaError> {
        Ok(self.service.create_folder(&name)?.into())
    }

    /// All folders, by name (ignoring case), with their document counts.
    pub fn list_folders(&self) -> Result<Vec<FolderInfo>, NolvaError> {
        Ok(self
            .service
            .list_folders()?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub fn rename_folder(&self, folder_id: String, name: String) -> Result<FolderInfo, NolvaError> {
        let id: FolderId = folder_id.parse()?;
        Ok(self.service.rename_folder(id, &name)?.into())
    }

    /// Deletes a folder. Its documents are kept and move to the top level.
    pub fn delete_folder(&self, folder_id: String) -> Result<(), NolvaError> {
        let id: FolderId = folder_id.parse()?;
        Ok(self.service.delete_folder(id)?)
    }

    /// Moves documents into a folder (`None`: the top level), atomically.
    /// Returns how many moved; ids that no longer exist are skipped.
    pub fn move_documents(
        &self,
        document_ids: Vec<String>,
        folder_id: Option<String>,
    ) -> Result<u32, NolvaError> {
        let ids = document_ids
            .iter()
            .map(|id| id.parse::<DocumentId>())
            .collect::<Result<Vec<_>, _>>()?;
        let folder = folder_id.map(|f| f.parse::<FolderId>()).transpose()?;
        Ok(self.service.move_documents(&ids, folder)?)
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

    /// Moves a document to the trash. It leaves `list_documents`, search
    /// and text recognition, and is deleted for good after 30 days unless
    /// restored.
    pub fn move_document_to_trash(&self, document_id: String) -> Result<(), NolvaError> {
        let id: DocumentId = document_id.parse()?;
        Ok(self.service.move_to_trash(id)?)
    }

    /// Takes a document out of the trash, as it was.
    pub fn restore_document(&self, document_id: String) -> Result<DocumentInfo, NolvaError> {
        let id: DocumentId = document_id.parse()?;
        Ok(self.document_info(self.service.restore_from_trash(id)?))
    }

    /// Documents in the trash, most recently trashed first.
    pub fn list_trashed_documents(&self) -> Result<Vec<DocumentInfo>, NolvaError> {
        Ok(self
            .service
            .list_trash()?
            .into_iter()
            .map(|d| self.document_info(d))
            .collect())
    }

    /// Permanently deletes every trashed document. Returns how many.
    pub fn empty_trash(&self) -> Result<u32, NolvaError> {
        Ok(self.service.empty_trash()?)
    }

    /// Permanently deletes documents trashed more than 30 days ago. Returns
    /// how many. Run it with the other maintenance, e.g. once per app start.
    pub fn purge_expired_trash(&self) -> Result<u32, NolvaError> {
        Ok(self.service.purge_expired_trash()?)
    }

    /// Copies a document, with its pages, crops, filters and recognized
    /// text, under `title` (the app supplies it, e.g. "Contrato (copia)").
    pub fn duplicate_document(
        &self,
        document_id: String,
        title: String,
    ) -> Result<DocumentInfo, NolvaError> {
        let id: DocumentId = document_id.parse()?;
        Ok(self.document_info(self.service.duplicate_document(id, &title)?))
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
            page_numbers: options.page_numbers,
        };
        let summary = self.service.export_pdf(id, &output, &options)?;
        Ok(PdfExportInfo {
            page_count: summary.page_count,
            size_bytes: summary.size_bytes,
        })
    }

    /// Up to `limit` pages whose text still has to be recognized, newest
    /// first: new pages and pages whose image changed since. Recognize each
    /// one's displayed image (`processed_path`, else `original_path`) and
    /// pass the result to [`Self::save_page_text`].
    pub fn pages_needing_text(&self, limit: u32) -> Result<Vec<PageInfo>, NolvaError> {
        Ok(self
            .service
            .pages_needing_text(limit)?
            .into_iter()
            .map(|p| self.page_info(p))
            .collect())
    }

    /// Stores the text recognized in `image_path`, the page's displayed
    /// image as [`PageInfo`] gave it. Fails with `Conflict` if the page
    /// shows another image by now (it was re-cropped meanwhile): drop the
    /// result, the page is pending again. Lines in reading order; blank
    /// lines are dropped and boxes are clamped into the image.
    pub fn save_page_text(
        &self,
        page_id: String,
        image_path: String,
        lines: Vec<RecognizedLine>,
    ) -> Result<PageTextInfo, NolvaError> {
        let id: PageId = page_id.parse()?;
        let lines = lines
            .into_iter()
            .map(|l| {
                let b = l.bounds;
                Ok(TextLine {
                    block: l.block,
                    text: l.text,
                    bounds: TextBounds::new(b.left, b.top, b.right, b.bottom)?,
                })
            })
            .collect::<Result<Vec<_>, DocumentError>>()?;
        let text = self
            .service
            .save_page_text(id, Path::new(&image_path), lines)?;
        Ok(page_text_info(text))
    }

    /// The page's recognized text, or `None` while recognition is pending.
    pub fn get_page_text(&self, page_id: String) -> Result<Option<PageTextInfo>, NolvaError> {
        let id: PageId = page_id.parse()?;
        Ok(self.service.page_text(id)?.map(page_text_info))
    }

    /// Pages whose text contains every word of `query` (as typed by the
    /// user; prefixes match, case and accents are ignored), best first, at
    /// most 50.
    pub fn search_text(&self, query: String) -> Result<Vec<TextSearchHit>, NolvaError> {
        Ok(self
            .service
            .search_text(&query)?
            .into_iter()
            .map(|m| TextSearchHit {
                document_id: m.document_id.to_string(),
                page_id: m.page_id.to_string(),
                page_position: m.page_position,
                snippet: m.snippet,
            })
            .collect())
    }

    /// Deletes stored images that no page points to anymore (leftovers of
    /// failed deletions or interrupted imports). Files from the last hour are
    /// kept, so it is safe while the app is in use. Run it in the background,
    /// e.g. once per app start; it walks every stored file.
    pub fn remove_orphan_files(&self) -> Result<CleanupInfo, NolvaError> {
        let summary = self.service.remove_orphan_files()?;
        Ok(CleanupInfo {
            files_removed: summary.files_removed,
            bytes_freed: summary.bytes_freed,
        })
    }

    /// Writes the document as an editable Word file to `output_path`
    /// (absolute; its folder must exist): the pages' recognized text as
    /// paragraphs, one Word page per page. Recognize pending pages first
    /// (see [`Self::pages_needing_text`]); any left are reported in
    /// `pages_pending_text` and come out empty. Call off the main thread.
    pub fn export_docx(
        &self,
        document_id: String,
        output_path: String,
        options: DocxOptions,
    ) -> Result<DocxExportInfo, NolvaError> {
        let id: DocumentId = document_id.parse()?;
        let output = PathBuf::from(output_path);
        if !output.is_absolute() {
            return Err(NolvaError::InvalidArgument {
                reason: "output_path must be absolute".into(),
            });
        }
        let options = document_engine::DocxExportOptions {
            include_images: options.include_images,
        };
        let export = self.service.export_docx(id, &output, &options)?;
        Ok(DocxExportInfo {
            page_count: export.summary.page_count,
            size_bytes: export.summary.size_bytes,
            pages_pending_text: export.pages_pending_text,
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

    /// Turns the page a quarter, keeping its crop and filter. The original
    /// is never modified.
    pub fn rotate_page(&self, page_id: String, clockwise: bool) -> Result<PageInfo, NolvaError> {
        let id: PageId = page_id.parse()?;
        Ok(self.page_info(self.service.rotate_page(id, clockwise)?))
    }

    /// Sets crop, filter and rotation at once, e.g. to undo or redo an edit.
    /// Nothing is rendered if the page already looks like that.
    pub fn set_page_rendering(
        &self,
        page_id: String,
        corners: Option<PageCorners>,
        filter: PageFilter,
        rotation_degrees: u32,
    ) -> Result<PageInfo, NolvaError> {
        let id: PageId = page_id.parse()?;
        let crop = corners.map(Crop::try_from).transpose()?;
        let rotation = Rotation::from_degrees(rotation_degrees)?;
        Ok(self.page_info(
            self.service
                .set_page_rendering(id, crop, filter.into(), rotation)?,
        ))
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
            trashed_at_ms: d.trashed_at_ms,
            folder_id: d.folder_id.map(|f| f.to_string()),
            size_bytes: d.size_bytes,
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
            rotation_degrees: page.rotation.degrees(),
            created_at_ms: page.created_at_ms,
        }
    }
}

fn page_text_info(text: PageText) -> PageTextInfo {
    PageTextInfo {
        page_id: text.page_id.to_string(),
        text: text.plain_text(),
        lines: text
            .lines
            .into_iter()
            .map(|l| RecognizedLine {
                block: l.block,
                text: l.text,
                bounds: TextRect {
                    left: l.bounds.left,
                    top: l.bounds.top,
                    right: l.bounds.right,
                    bottom: l.bounds.bottom,
                },
            })
            .collect(),
        recognized_at_ms: text.recognized_at_ms,
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

    fn line(block: u32, text: &str) -> RecognizedLine {
        RecognizedLine {
            block,
            text: text.into(),
            bounds: TextRect {
                left: 0.1,
                top: 0.1,
                right: 0.9,
                bottom: 0.15,
            },
        }
    }

    #[test]
    fn recognized_text_end_to_end() {
        let data = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let img = src_dir.path().join("shot.png");
        std::fs::write(&img, b"png").unwrap();
        let core = core(&data);
        let doc = core.create_document("Arriendo".into()).unwrap();
        let page = core
            .add_page(doc.id.clone(), img.to_string_lossy().into_owned())
            .unwrap();

        assert_eq!(core.pages_needing_text(5).unwrap(), vec![page.clone()]);
        assert_eq!(core.get_page_text(page.id.clone()).unwrap(), None);

        let saved = core
            .save_page_text(
                page.id.clone(),
                page.original_path.clone(),
                vec![line(0, "Contrato de arriendo"), line(1, "Valparaíso")],
            )
            .unwrap();
        assert_eq!(saved.text, "Contrato de arriendo\n\nValparaíso");
        assert_eq!(core.get_page_text(page.id.clone()).unwrap(), Some(saved));
        assert!(core.pages_needing_text(5).unwrap().is_empty());

        let hits = core.search_text("valparaiso arrien".into()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            (hits[0].document_id.as_str(), hits[0].page_id.as_str()),
            (doc.id.as_str(), page.id.as_str())
        );
        assert!(core.search_text("  ".into()).unwrap().is_empty());
    }

    #[test]
    fn folders_and_sizes() {
        let data = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let img = src_dir.path().join("shot.png");
        std::fs::write(&img, b"png-bytes").unwrap();
        let core = core(&data);

        let folder = core.create_folder("Cuentas".into()).unwrap();
        let inside = core
            .create_document_in_folder("Luz".into(), Some(folder.id.clone()))
            .unwrap();
        core.add_page(inside.id.clone(), img.to_string_lossy().into_owned())
            .unwrap();
        let loose = core.create_document("Agua".into()).unwrap();
        assert_eq!(
            core.move_documents(vec![loose.id.clone()], Some(folder.id.clone()))
                .unwrap(),
            1
        );

        let folders = core.list_folders().unwrap();
        assert_eq!(
            (folders[0].name.as_str(), folders[0].document_count),
            ("Cuentas", 2)
        );
        let listed = core.list_documents().unwrap();
        let luz = listed.iter().find(|d| d.id == inside.id).unwrap();
        assert_eq!(
            (luz.folder_id.as_deref(), luz.size_bytes),
            (Some(folder.id.as_str()), 9)
        );

        assert_eq!(
            core.rename_folder(folder.id.clone(), "Hogar".into())
                .unwrap()
                .name,
            "Hogar"
        );
        core.delete_folder(folder.id.clone()).unwrap();
        assert!(
            core.list_documents()
                .unwrap()
                .iter()
                .all(|d| d.folder_id.is_none())
        );
        assert!(matches!(
            core.delete_folder(folder.id),
            Err(NolvaError::NotFound { .. })
        ));
        assert!(matches!(
            core.move_documents(vec!["not-a-uuid".into()], None),
            Err(NolvaError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn trash_and_duplicate_round_trip() {
        let data = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let img = src_dir.path().join("shot.png");
        std::fs::write(&img, b"png").unwrap();
        let core = core(&data);
        let doc = core.create_document("Arriendo".into()).unwrap();
        core.add_page(doc.id.clone(), img.to_string_lossy().into_owned())
            .unwrap();

        let copy = core
            .duplicate_document(doc.id.clone(), "Arriendo (copia)".into())
            .unwrap();
        assert_eq!(
            (copy.title.as_str(), copy.page_count),
            ("Arriendo (copia)", 1)
        );
        assert_ne!(copy.cover_path, doc.cover_path);

        core.move_document_to_trash(doc.id.clone()).unwrap();
        let listed: Vec<_> = core
            .list_documents()
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(listed, vec![copy.id.clone()]);
        let trash = core.list_trashed_documents().unwrap();
        assert_eq!(trash[0].id, doc.id);
        assert!(trash[0].trashed_at_ms.is_some());
        assert_eq!(
            core.purge_expired_trash().unwrap(),
            0,
            "not 30 days old yet"
        );

        let restored = core.restore_document(doc.id.clone()).unwrap();
        assert_eq!(restored.trashed_at_ms, None);
        core.move_document_to_trash(doc.id).unwrap();
        assert_eq!(core.empty_trash().unwrap(), 1);
        assert!(core.list_trashed_documents().unwrap().is_empty());
    }

    #[test]
    fn exports_word_documents() {
        let data = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let img = src_dir.path().join("shot.png");
        image::RgbImage::from_pixel(60, 80, image::Rgb([250, 250, 250]))
            .save(&img)
            .unwrap();
        let core = core(&data);
        let doc = core.create_document("Carta".into()).unwrap();
        let first = core
            .add_page(doc.id.clone(), img.to_string_lossy().into_owned())
            .unwrap();
        core.add_page(doc.id.clone(), img.to_string_lossy().into_owned())
            .unwrap();
        core.save_page_text(
            first.id,
            first.original_path,
            vec![line(0, "Estimado Hugo:")],
        )
        .unwrap();

        let output = src_dir.path().join("Carta.docx");
        let info = core
            .export_docx(
                doc.id,
                output.to_string_lossy().into_owned(),
                DocxOptions {
                    include_images: true,
                },
            )
            .unwrap();

        assert_eq!((info.page_count, info.pages_pending_text), (2, 1));
        assert_eq!(info.size_bytes, std::fs::metadata(&output).unwrap().len());
    }

    #[test]
    fn text_for_another_image_is_a_conflict() {
        let data = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let img = src_dir.path().join("shot.png");
        std::fs::write(&img, b"png").unwrap();
        let core = core(&data);
        let doc = core.create_document("Doc".into()).unwrap();
        let page = core
            .add_page(doc.id, img.to_string_lossy().into_owned())
            .unwrap();

        assert!(matches!(
            core.save_page_text(
                page.id.clone(),
                "/elsewhere/x.jpg".into(),
                vec![line(0, "x")]
            ),
            Err(NolvaError::Conflict { .. })
        ));
        let mut inverted = line(0, "x");
        inverted.bounds.left = 0.95;
        assert!(matches!(
            core.save_page_text(page.id, page.original_path, vec![inverted]),
            Err(NolvaError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn orphan_cleanup_keeps_live_pages() {
        let data = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let img = src_dir.path().join("shot.png");
        std::fs::write(&img, b"png").unwrap();
        let core = core(&data);
        let doc = core.create_document("Boleta".into()).unwrap();
        let page = core
            .add_page(doc.id, img.to_string_lossy().into_owned())
            .unwrap();

        // Everything is recent, so nothing qualifies yet.
        let info = core.remove_orphan_files().unwrap();
        assert_eq!(
            info,
            CleanupInfo {
                files_removed: 0,
                bytes_freed: 0
            }
        );
        assert!(Path::new(&page.original_path).exists());
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
    fn rotation_turns_the_image_and_can_be_undone() {
        let data = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let core = core(&data);
        let doc = core.create_document("Giro".into()).unwrap();
        let page = core
            .add_page(
                doc.id.clone(),
                photo_of_tilted_sheet(src.path()).to_string_lossy().into(),
            )
            .unwrap();
        let (w, h) = image::image_dimensions(&page.original_path).unwrap();

        let turned = core.rotate_page(page.id.clone(), true).unwrap();
        assert_eq!(turned.rotation_degrees, 90);
        let path = turned.processed_path.clone().expect("rotation renders");
        assert_eq!(image::image_dimensions(&path).unwrap(), (h, w));

        let half = core.rotate_page(page.id.clone(), true).unwrap();
        assert_eq!(half.rotation_degrees, 180);
        assert!(!Path::new(&path).exists(), "old render removed");

        // Undo back to how it was imported.
        let undone = core
            .set_page_rendering(page.id.clone(), None, PageFilter::Original, 0)
            .unwrap();
        assert_eq!(undone.rotation_degrees, 0);
        assert_eq!(undone.processed_path, None);
        assert!(matches!(
            core.set_page_rendering(page.id, None, PageFilter::Original, 45),
            Err(NolvaError::InvalidArgument { .. })
        ));
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
            page_numbers: false,
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
                    page_numbers: false,
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
            page_numbers: false,
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
