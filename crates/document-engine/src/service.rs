use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use crate::clock::Clock;
use crate::error::{DocumentError, ProcessingError};
use crate::model::{
    Crop, Document, DocumentId, Folder, FolderId, ImageFormat, NewPage, Page, PageFilter, PageId,
    Rotation,
};
use crate::ports::{
    CleanupSummary, CornerDetection, DocumentExporter, DocumentRepository, DocxExportOptions,
    ExportPage, ExportSummary, MAX_PDF_PASSWORD_BYTES, PageFileStore, PageProcessor,
    PdfExportOptions,
};
use crate::text::{PageText, TextLine, TextMatch, normalize_lines};

/// Maximum length of a document title, in characters.
pub const MAX_TITLE_CHARS: usize = 200;

/// Documents stay in the trash this long before they are deleted for good.
pub const TRASH_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Most search results returned.
pub const MAX_SEARCH_RESULTS: u32 = 50;

/// Files younger than this survive [`DocumentService::remove_orphan_files`]:
/// an import or render may have written them and not yet saved the row
/// that points to them. Far longer than any such operation takes.
pub const ORPHAN_GRACE: Duration = Duration::from_secs(60 * 60);

/// Document use cases, independent of storage technology and platform.
#[derive(Debug)]
pub struct DocumentService<R, F, P, E, C> {
    repository: R,
    files: F,
    processor: P,
    exporter: E,
    clock: C,
}

impl<R, F, P, E, C> DocumentService<R, F, P, E, C>
where
    R: DocumentRepository,
    F: PageFileStore,
    P: PageProcessor,
    E: DocumentExporter,
    C: Clock,
{
    pub fn new(repository: R, files: F, processor: P, exporter: E, clock: C) -> Self {
        Self {
            repository,
            files,
            processor,
            exporter,
            clock,
        }
    }

    pub fn files(&self) -> &F {
        &self.files
    }

    pub fn create_document(&self, title: &str) -> Result<Document, DocumentError> {
        self.create_document_in(title, None)
    }

    /// Creates an empty document in `folder` (`None`: the top level).
    pub fn create_document_in(
        &self,
        title: &str,
        folder: Option<FolderId>,
    ) -> Result<Document, DocumentError> {
        let title = normalize_title(title)?;
        if let Some(folder) = folder {
            self.get_folder(folder)?;
        }
        let now = self.clock.now_ms();
        let document = Document {
            id: DocumentId::new(),
            title,
            created_at_ms: now,
            updated_at_ms: now,
            page_count: 0,
            cover_path: None,
            trashed_at_ms: None,
            folder_id: folder,
            size_bytes: 0,
        };
        self.repository.insert_document(&document)?;
        Ok(document)
    }

    /// Documents not in the trash, most recently updated first, with their sizes.
    pub fn list_documents(&self) -> Result<Vec<Document>, DocumentError> {
        self.with_sizes(self.repository.list_documents()?)
    }

    /// Fills in [`Document::size_bytes`] from the files on disk. Sizes are
    /// informational, so unreadable files just count as 0.
    fn with_sizes(&self, mut documents: Vec<Document>) -> Result<Vec<Document>, DocumentError> {
        if documents.is_empty() {
            return Ok(documents);
        }
        let mut sizes: HashMap<DocumentId, u64> = HashMap::new();
        for (document, path) in self.repository.files_by_document()? {
            if let Ok(Some(size)) = self.files.file_size(&path) {
                *sizes.entry(document).or_default() += size;
            }
        }
        for document in &mut documents {
            document.size_bytes = sizes.get(&document.id).copied().unwrap_or(0);
        }
        Ok(documents)
    }

    fn with_size(&self, document: Document) -> Result<Document, DocumentError> {
        let id = document.id;
        self.with_sizes(vec![document])?
            .pop()
            .ok_or(DocumentError::DocumentNotFound(id))
    }

    /// Creates a folder. Names follow the same rules as titles.
    pub fn create_folder(&self, name: &str) -> Result<Folder, DocumentError> {
        let name = normalize_title(name)?;
        let now = self.clock.now_ms();
        let folder = Folder {
            id: FolderId::new(),
            name,
            created_at_ms: now,
            updated_at_ms: now,
            document_count: 0,
        };
        self.repository.insert_folder(&folder)?;
        Ok(folder)
    }

    /// All folders, by name.
    pub fn list_folders(&self) -> Result<Vec<Folder>, DocumentError> {
        Ok(self.repository.list_folders()?)
    }

    pub fn get_folder(&self, id: FolderId) -> Result<Folder, DocumentError> {
        self.repository
            .get_folder(id)?
            .ok_or(DocumentError::FolderNotFound(id))
    }

    pub fn rename_folder(&self, id: FolderId, name: &str) -> Result<Folder, DocumentError> {
        let name = normalize_title(name)?;
        self.repository
            .rename_folder(id, &name, self.clock.now_ms())?
            .ok_or(DocumentError::FolderNotFound(id))
    }

    /// Deletes a folder. Its documents are kept and move to the top level.
    pub fn delete_folder(&self, id: FolderId) -> Result<(), DocumentError> {
        if !self.repository.delete_folder(id)? {
            return Err(DocumentError::FolderNotFound(id));
        }
        Ok(())
    }

    /// Moves documents into `folder` (`None`: the top level). Returns how
    /// many moved; ids that no longer exist are skipped.
    pub fn move_documents(
        &self,
        documents: &[DocumentId],
        folder: Option<FolderId>,
    ) -> Result<u32, DocumentError> {
        if let Some(folder) = folder {
            self.get_folder(folder)?;
        }
        Ok(self.repository.move_documents(documents, folder)?)
    }

    pub fn rename_document(&self, id: DocumentId, title: &str) -> Result<Document, DocumentError> {
        let title = normalize_title(title)?;
        self.repository
            .rename_document(id, &title, self.clock.now_ms())?
            .ok_or(DocumentError::DocumentNotFound(id))
    }

    /// Deletes a document, its pages and all their images. Permanent.
    ///
    /// The database goes first: if file cleanup then fails, the leftovers
    /// are unreachable orphans (wasted space), never pages pointing at
    /// missing images.
    pub fn delete_document(&self, id: DocumentId) -> Result<(), DocumentError> {
        if !self.repository.delete_document(id)? {
            return Err(DocumentError::DocumentNotFound(id));
        }
        let _ = self.files.remove_document_files(id);
        Ok(())
    }

    /// Up to `limit` pages whose text still has to be recognized: new pages,
    /// and pages whose image changed (re-cropped or filtered) since. Newest
    /// first, so what the user just scanned becomes searchable first.
    pub fn pages_needing_text(&self, limit: u32) -> Result<Vec<Page>, DocumentError> {
        Ok(self.repository.pages_without_text(limit)?)
    }

    /// Stores the text recognized in `image`, which must be the page's
    /// displayed image as handed out (absolute path). If the page shows a
    /// different image by now, fails with [`DocumentError::PageImageChanged`]
    /// and the page stays pending.
    pub fn save_page_text(
        &self,
        page_id: PageId,
        image: &Path,
        lines: Vec<TextLine>,
    ) -> Result<PageText, DocumentError> {
        let page = self.get_page(page_id)?;
        let current = displayed_image(&page);
        if self.files.resolve(&current) != image {
            return Err(DocumentError::PageImageChanged(page_id));
        }
        let text = PageText {
            page_id,
            source_path: current,
            lines: normalize_lines(lines)?,
            recognized_at_ms: self.clock.now_ms(),
        };
        self.repository.save_page_text(&text, &text.plain_text())?;
        Ok(text)
    }

    /// The page's text, if it was recognized in the image the page shows
    /// now. `None` while recognition is pending.
    pub fn page_text(&self, page_id: PageId) -> Result<Option<PageText>, DocumentError> {
        let page = self.get_page(page_id)?;
        let current = displayed_image(&page);
        Ok(self
            .repository
            .get_page_text(page_id)?
            .filter(|text| text.source_path == current))
    }

    /// Pages whose text contains every word of `query`, best match first.
    pub fn search_text(&self, query: &str) -> Result<Vec<TextMatch>, DocumentError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.repository.search_text(query, MAX_SEARCH_RESULTS)?)
    }

    /// Deletes stored images that no page points to anymore: leftovers of
    /// deletions or re-renders whose file cleanup failed, or of imports that
    /// crashed halfway. Safe to run at any time, e.g. once per app start in
    /// the background; recent files are kept (see [`ORPHAN_GRACE`]).
    pub fn remove_orphan_files(&self) -> Result<CleanupSummary, DocumentError> {
        // Read the references first: a file written after this snapshot is
        // newer than the grace period, so it can't be removed by mistake.
        let referenced: HashSet<_> = self.repository.referenced_files()?.into_iter().collect();
        Ok(self.files.remove_unreferenced(&referenced, ORPHAN_GRACE)?)
    }

    /// Moves a document to the trash: it leaves the list, search and text
    /// recognition, and is deleted for good after [`TRASH_RETENTION_MS`]
    /// unless restored.
    pub fn move_to_trash(&self, id: DocumentId) -> Result<(), DocumentError> {
        if !self
            .repository
            .set_document_trashed(id, Some(self.clock.now_ms()))?
        {
            return Err(DocumentError::DocumentNotFound(id));
        }
        Ok(())
    }

    /// Takes a document out of the trash, as it was.
    pub fn restore_from_trash(&self, id: DocumentId) -> Result<Document, DocumentError> {
        if !self.repository.set_document_trashed(id, None)? {
            return Err(DocumentError::DocumentNotFound(id));
        }
        let document = self
            .repository
            .get_document(id)?
            .ok_or(DocumentError::DocumentNotFound(id))?;
        self.with_size(document)
    }

    /// Documents in the trash, most recently trashed first.
    pub fn list_trash(&self) -> Result<Vec<Document>, DocumentError> {
        self.with_sizes(self.repository.list_trashed_documents()?)
    }

    /// Deletes every trashed document for good. Returns how many.
    pub fn empty_trash(&self) -> Result<u32, DocumentError> {
        let ids: Vec<_> = self.list_trash()?.into_iter().map(|d| d.id).collect();
        self.delete_all(&ids)
    }

    /// Deletes documents trashed more than [`TRASH_RETENTION_MS`] ago.
    /// Returns how many. Meant for periodic maintenance.
    pub fn purge_expired_trash(&self) -> Result<u32, DocumentError> {
        let cutoff = self.clock.now_ms() - TRASH_RETENTION_MS;
        let ids = self.repository.trashed_before(cutoff)?;
        self.delete_all(&ids)
    }

    fn delete_all(&self, ids: &[DocumentId]) -> Result<u32, DocumentError> {
        let mut deleted = 0;
        for &id in ids {
            match self.delete_document(id) {
                Ok(()) => deleted += 1,
                // Deleted meanwhile: nothing left to do.
                Err(DocumentError::DocumentNotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(deleted)
    }

    /// Copies a document under a new `title`: its pages with their images,
    /// crops, filters and recognized text. All or nothing: if a copy fails,
    /// the half-made document is removed.
    pub fn duplicate_document(
        &self,
        id: DocumentId,
        title: &str,
    ) -> Result<Document, DocumentError> {
        let title = normalize_title(title)?;
        let source = self
            .repository
            .get_document(id)?
            .ok_or(DocumentError::DocumentNotFound(id))?;
        let pages = self.repository.list_pages(id)?;
        let now = self.clock.now_ms();
        let copy = Document {
            id: DocumentId::new(),
            title,
            created_at_ms: now,
            updated_at_ms: now,
            page_count: 0,
            cover_path: None,
            trashed_at_ms: None,
            // The copy sits next to the original.
            folder_id: source.folder_id,
            size_bytes: 0,
        };
        self.repository.insert_document(&copy)?;

        let copied = pages
            .iter()
            .try_for_each(|page| self.copy_page(copy.id, page));
        if let Err(error) = copied {
            let _ = self.repository.delete_document(copy.id);
            let _ = self.files.remove_document_files(copy.id);
            return Err(error);
        }
        let copy = self
            .repository
            .get_document(copy.id)?
            .ok_or(DocumentError::DocumentNotFound(copy.id))?;
        self.with_size(copy)
    }

    /// Appends a copy of `page` (files, rendering and current text) to `document_id`.
    fn copy_page(&self, document_id: DocumentId, page: &Page) -> Result<(), DocumentError> {
        let page_id = PageId::new();
        let format = ImageFormat::from_path(&page.original_path)?;
        let original = self.files.import_original(
            document_id,
            page_id,
            format,
            &self.files.resolve(&page.original_path),
        )?;
        let now = self.clock.now_ms();
        let new_page = self.repository.append_page(&NewPage {
            id: page_id,
            document_id,
            original_path: original,
            created_at_ms: now,
        })?;

        let processed = match &page.processed_path {
            Some(source) => {
                let copy = self.files.new_processed_path(document_id, page_id);
                self.files.copy_file(source, &copy)?;
                Some(copy)
            }
            None => None,
        };
        if page.crop.is_some()
            || page.filter != PageFilter::Original
            || page.rotation != Rotation::None
            || processed.is_some()
        {
            self.repository.set_page_rendering(
                page_id,
                page.crop.as_ref(),
                page.filter,
                page.rotation,
                processed.as_deref(),
                now,
            )?;
        }

        if let Some(text) = self.page_text(page.id)? {
            let copy = PageText {
                page_id,
                source_path: processed.unwrap_or(new_page.original_path),
                lines: text.lines,
                recognized_at_ms: text.recognized_at_ms,
            };
            self.repository.save_page_text(&copy, &copy.plain_text())?;
        }
        Ok(())
    }

    /// Deletes a page and its images; later pages move up one position.
    pub fn delete_page(&self, page_id: PageId) -> Result<(), DocumentError> {
        let page = self.get_page(page_id)?;
        if !self.repository.delete_page(page_id, self.clock.now_ms())? {
            return Err(DocumentError::PageNotFound(page_id));
        }
        let _ = self.files.remove_page_files(page.document_id, page_id);
        Ok(())
    }

    /// Moves a page to `to_position` (0-based, clamped to the document) and
    /// returns the document's pages in their new order.
    pub fn move_page(&self, page_id: PageId, to_position: u32) -> Result<Vec<Page>, DocumentError> {
        self.get_page(page_id)?;
        Ok(self
            .repository
            .move_page(page_id, to_position, self.clock.now_ms())?)
    }

    /// Imports an image file as the last page of a document.
    ///
    /// The source file is copied, never moved: the caller keeps ownership of
    /// it (it may be a camera temp file or a photo-library export).
    pub fn add_page(&self, document_id: DocumentId, source: &Path) -> Result<Page, DocumentError> {
        if self.repository.get_document(document_id)?.is_none() {
            return Err(DocumentError::DocumentNotFound(document_id));
        }

        let format = ImageFormat::from_path(source)?;
        if !source.is_file() {
            return Err(DocumentError::SourceImageNotFound(source.to_path_buf()));
        }

        let page_id = PageId::new();
        let original_path = self
            .files
            .import_original(document_id, page_id, format, source)?;

        let new_page = NewPage {
            id: page_id,
            document_id,
            original_path,
            created_at_ms: self.clock.now_ms(),
        };

        self.repository.append_page(&new_page).map_err(|err| {
            // Compensate so we don't leave orphaned files behind. If cleanup
            // also fails, a future orphan sweep catches it; the original
            // error is the one worth reporting.
            let _ = self.files.remove_page_files(document_id, page_id);
            err.into()
        })
    }

    pub fn list_pages(&self, document_id: DocumentId) -> Result<Vec<Page>, DocumentError> {
        if self.repository.get_document(document_id)?.is_none() {
            return Err(DocumentError::DocumentNotFound(document_id));
        }
        Ok(self.repository.list_pages(document_id)?)
    }

    pub fn get_page(&self, page_id: PageId) -> Result<Page, DocumentError> {
        self.repository
            .get_page(page_id)?
            .ok_or(DocumentError::PageNotFound(page_id))
    }

    /// Looks for the page outline in the page's original image. `Ok(None)`
    /// means nothing document-like was found; the user picks the corners.
    pub fn detect_corners(
        &self,
        page_id: PageId,
    ) -> Result<Option<CornerDetection>, DocumentError> {
        let page = self.get_page(page_id)?;
        let original = self.files.resolve(&page.original_path);
        self.processor
            .detect_corners(&original)
            .map_err(|e| processing_error(e, &page))
    }

    /// Crops the page to `crop` and stores the perspective-corrected image,
    /// replacing any previous crop and keeping the page's filter. The
    /// original image is never modified, so a page can be re-cropped any
    /// number of times.
    pub fn crop_page(&self, page_id: PageId, crop: Crop) -> Result<Page, DocumentError> {
        let page = self.get_page(page_id)?;
        let (filter, rotation) = (page.filter, page.rotation);
        self.render(page, Some(crop), filter, rotation)
    }

    /// Applies `filter` to the page, on top of its crop if it has one.
    /// Filters always start from the original, so they can be switched
    /// freely; [`PageFilter::Original`] on an uncropped page simply shows the
    /// original again.
    pub fn set_page_filter(
        &self,
        page_id: PageId,
        filter: PageFilter,
    ) -> Result<Page, DocumentError> {
        let page = self.get_page(page_id)?;
        let (crop, rotation) = (page.crop, page.rotation);
        self.render(page, crop, filter, rotation)
    }

    /// Turns the page a quarter, clockwise or counterclockwise, keeping its
    /// crop and filter.
    pub fn rotate_page(&self, page_id: PageId, clockwise: bool) -> Result<Page, DocumentError> {
        let page = self.get_page(page_id)?;
        let (crop, filter, rotation) = (page.crop, page.filter, page.rotation.turned(clockwise));
        self.render(page, crop, filter, rotation)
    }

    /// Sets the page's whole rendering at once (crop, filter and rotation),
    /// e.g. to undo or redo an edit. Rendering only happens if something
    /// differs from what the page has now.
    pub fn set_page_rendering(
        &self,
        page_id: PageId,
        crop: Option<Crop>,
        filter: PageFilter,
        rotation: Rotation,
    ) -> Result<Page, DocumentError> {
        let page = self.get_page(page_id)?;
        if page.crop == crop && page.filter == filter && page.rotation == rotation {
            return Ok(page);
        }
        self.render(page, crop, filter, rotation)
    }

    /// Re-renders `page` with `crop`, `filter` and `rotation`.
    ///
    /// The new image goes to a fresh path and the database is updated only
    /// after it is fully written; the previous processed image is deleted
    /// last. A failure at any step leaves the page exactly as it was.
    fn render(
        &self,
        page: Page,
        crop: Option<Crop>,
        filter: PageFilter,
        rotation: Rotation,
    ) -> Result<Page, DocumentError> {
        let processed =
            if crop.is_none() && filter == PageFilter::Original && rotation == Rotation::None {
                None
            } else {
                let path = self.files.new_processed_path(page.document_id, page.id);
                let original = self.files.resolve(&page.original_path);
                self.processor
                    .render_page(
                        &original,
                        crop.as_ref(),
                        filter,
                        rotation,
                        &self.files.resolve(&path),
                    )
                    .map_err(|e| processing_error(e, &page))?;
                Some(path)
            };

        let updated = self
            .repository
            .set_page_rendering(
                page.id,
                crop.as_ref(),
                filter,
                rotation,
                processed.as_deref(),
                self.clock.now_ms(),
            )
            .map_err(|err| {
                if let Some(path) = &processed {
                    let _ = self.files.remove_file(path);
                }
                DocumentError::from(err)
            })?;

        if let Some(previous) = page.processed_path {
            // Best effort: an orphan here only wastes space until cleanup.
            let _ = self.files.remove_file(&previous);
        }
        Ok(updated)
    }

    /// Exports the document as a PDF at `output`, one page per page, using
    /// each page's processed image when it has one and the original otherwise.
    /// Pages whose text is already recognized get it as a selectable layer;
    /// the others are exported as plain images.
    pub fn export_pdf(
        &self,
        document_id: DocumentId,
        output: &Path,
        options: &PdfExportOptions,
    ) -> Result<ExportSummary, DocumentError> {
        if options
            .password
            .as_ref()
            .is_some_and(|p| p.is_empty() || p.len() > MAX_PDF_PASSWORD_BYTES)
        {
            return Err(DocumentError::InvalidPassword);
        }
        let (title, pages, _) = self.export_pages(document_id)?;
        self.exporter
            .export_pdf(&title, &pages, options, output)
            .map_err(export_error)
    }

    /// Exports the document as an editable Word file at `output`, rebuilt
    /// from the pages' recognized text. Pages still waiting for recognition
    /// come out empty: [`DocxExport::pages_pending_text`] says how many, so
    /// callers can recognize them first.
    pub fn export_docx(
        &self,
        document_id: DocumentId,
        output: &Path,
        options: &DocxExportOptions,
    ) -> Result<DocxExport, DocumentError> {
        let (title, pages, pages_pending_text) = self.export_pages(document_id)?;
        let summary = self
            .exporter
            .export_docx(&title, &pages, options, output)
            .map_err(export_error)?;
        Ok(DocxExport {
            summary,
            pages_pending_text,
        })
    }

    /// The document's title, its pages ready to export (displayed image and
    /// current text), and how many pages have no current text yet.
    fn export_pages(
        &self,
        document_id: DocumentId,
    ) -> Result<(String, Vec<ExportPage>, u32), DocumentError> {
        let document = self
            .repository
            .get_document(document_id)?
            .ok_or(DocumentError::DocumentNotFound(document_id))?;
        let pages = self.repository.list_pages(document_id)?;
        if pages.is_empty() {
            return Err(DocumentError::EmptyDocument(document_id));
        }
        let mut pending = 0;
        let pages = pages
            .into_iter()
            .map(|page| {
                let image = displayed_image(&page);
                // Only text recognized in this very image lines up with it.
                let text = self
                    .repository
                    .get_page_text(page.id)?
                    .filter(|t| t.source_path == image);
                if text.is_none() {
                    pending += 1;
                }
                Ok(ExportPage {
                    image: self.files.resolve(&image),
                    text: text.map(|t| t.lines).unwrap_or_default(),
                })
            })
            .collect::<Result<Vec<_>, DocumentError>>()?;
        Ok((document.title, pages, pending))
    }
}

fn processing_error(error: ProcessingError, page: &Page) -> DocumentError {
    match error {
        ProcessingError::UnsupportedFormat => DocumentError::UnsupportedImageFormat {
            extension: page
                .original_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_owned(),
        },
        ProcessingError::InvalidCorners(reason) => DocumentError::InvalidCorners { reason },
        ProcessingError::Failed(port) => DocumentError::Storage(port),
    }
}

fn normalize_title(title: &str) -> Result<String, DocumentError> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err(DocumentError::InvalidTitle {
            reason: "title is empty",
        });
    }
    if trimmed.chars().count() > MAX_TITLE_CHARS {
        return Err(DocumentError::InvalidTitle {
            reason: "title is too long",
        });
    }
    if trimmed.chars().any(char::is_control) {
        return Err(DocumentError::InvalidTitle {
            reason: "title contains control characters",
        });
    }
    Ok(trimmed.to_owned())
}

/// Result of [`DocumentService::export_docx`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocxExport {
    pub summary: ExportSummary,
    /// Pages exported without text because it wasn't recognized yet.
    pub pages_pending_text: u32,
}

fn export_error(error: ProcessingError) -> DocumentError {
    match error {
        ProcessingError::UnsupportedFormat => DocumentError::UnsupportedImageFormat {
            extension: "page image".into(),
        },
        ProcessingError::InvalidCorners(reason) => DocumentError::InvalidCorners { reason },
        ProcessingError::Failed(port) => DocumentError::Storage(port),
    }
}

/// The image a page shows: its processed image, or the original.
fn displayed_image(page: &Page) -> std::path::PathBuf {
    page.processed_path
        .clone()
        .unwrap_or_else(|| page.original_path.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::error::PortError;

    struct FixedClock(i64);
    impl Clock for FixedClock {
        fn now_ms(&self) -> i64 {
            self.0
        }
    }

    #[derive(Default)]
    struct InMemoryRepo {
        docs: Mutex<HashMap<DocumentId, Document>>,
        pages: Mutex<Vec<Page>>,
        fail_append: bool,
        fail_set_crop: bool,
        texts: Mutex<HashMap<PageId, (PageText, String)>>,
        searched: Mutex<Option<String>>,
        folders: Mutex<HashMap<FolderId, Folder>>,
    }

    impl DocumentRepository for InMemoryRepo {
        fn insert_document(&self, d: &Document) -> Result<(), PortError> {
            self.docs.lock().unwrap().insert(d.id, d.clone());
            Ok(())
        }
        fn get_document(&self, id: DocumentId) -> Result<Option<Document>, PortError> {
            Ok(self.docs.lock().unwrap().get(&id).cloned())
        }
        fn list_documents(&self) -> Result<Vec<Document>, PortError> {
            Ok(self
                .docs
                .lock()
                .unwrap()
                .values()
                .filter(|d| d.trashed_at_ms.is_none())
                .cloned()
                .collect())
        }
        fn list_trashed_documents(&self) -> Result<Vec<Document>, PortError> {
            Ok(self
                .docs
                .lock()
                .unwrap()
                .values()
                .filter(|d| d.trashed_at_ms.is_some())
                .cloned()
                .collect())
        }
        fn set_document_trashed(
            &self,
            id: DocumentId,
            trashed_at_ms: Option<i64>,
        ) -> Result<bool, PortError> {
            Ok(self
                .docs
                .lock()
                .unwrap()
                .get_mut(&id)
                .map(|d| d.trashed_at_ms = trashed_at_ms)
                .is_some())
        }
        fn files_by_document(&self) -> Result<Vec<(DocumentId, PathBuf)>, PortError> {
            Ok(self
                .pages
                .lock()
                .unwrap()
                .iter()
                .flat_map(|p| {
                    std::iter::once((p.document_id, p.original_path.clone()))
                        .chain(p.processed_path.clone().map(|path| (p.document_id, path)))
                })
                .collect())
        }
        fn insert_folder(&self, folder: &Folder) -> Result<(), PortError> {
            self.folders
                .lock()
                .unwrap()
                .insert(folder.id, folder.clone());
            Ok(())
        }
        fn get_folder(&self, id: FolderId) -> Result<Option<Folder>, PortError> {
            let docs = self.docs.lock().unwrap();
            Ok(self.folders.lock().unwrap().get(&id).map(|f| Folder {
                document_count: docs
                    .values()
                    .filter(|d| d.folder_id == Some(id) && d.trashed_at_ms.is_none())
                    .count() as u32,
                ..f.clone()
            }))
        }
        fn list_folders(&self) -> Result<Vec<Folder>, PortError> {
            let ids: Vec<_> = self.folders.lock().unwrap().keys().copied().collect();
            let mut folders: Vec<_> = ids
                .into_iter()
                .filter_map(|id| self.get_folder(id).unwrap())
                .collect();
            folders.sort_by_key(|f| f.name.to_lowercase());
            Ok(folders)
        }
        fn rename_folder(
            &self,
            id: FolderId,
            name: &str,
            updated_at_ms: i64,
        ) -> Result<Option<Folder>, PortError> {
            if let Some(f) = self.folders.lock().unwrap().get_mut(&id) {
                f.name = name.to_owned();
                f.updated_at_ms = updated_at_ms;
            }
            self.get_folder(id)
        }
        fn delete_folder(&self, id: FolderId) -> Result<bool, PortError> {
            for d in self.docs.lock().unwrap().values_mut() {
                if d.folder_id == Some(id) {
                    d.folder_id = None;
                }
            }
            Ok(self.folders.lock().unwrap().remove(&id).is_some())
        }
        fn move_documents(
            &self,
            documents: &[DocumentId],
            folder: Option<FolderId>,
        ) -> Result<u32, PortError> {
            let mut docs = self.docs.lock().unwrap();
            let mut moved = 0;
            for id in documents {
                if let Some(d) = docs.get_mut(id) {
                    d.folder_id = folder;
                    moved += 1;
                }
            }
            Ok(moved)
        }
        fn trashed_before(&self, cutoff_ms: i64) -> Result<Vec<DocumentId>, PortError> {
            Ok(self
                .docs
                .lock()
                .unwrap()
                .values()
                .filter(|d| d.trashed_at_ms.is_some_and(|t| t < cutoff_ms))
                .map(|d| d.id)
                .collect())
        }
        fn append_page(&self, p: &NewPage) -> Result<Page, PortError> {
            if self.fail_append {
                return Err(PortError::new("append_page", "simulated failure"));
            }
            let mut docs = self.docs.lock().unwrap();
            let d = docs.get_mut(&p.document_id).unwrap();
            let page = Page {
                id: p.id,
                document_id: p.document_id,
                position: d.page_count,
                original_path: p.original_path.clone(),
                crop: None,
                filter: PageFilter::Original,
                rotation: Rotation::None,
                processed_path: None,
                created_at_ms: p.created_at_ms,
            };
            d.page_count += 1;
            d.updated_at_ms = p.created_at_ms;
            self.pages.lock().unwrap().push(page.clone());
            Ok(page)
        }
        fn list_pages(&self, id: DocumentId) -> Result<Vec<Page>, PortError> {
            Ok(self
                .pages
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.document_id == id)
                .cloned()
                .collect())
        }
        fn get_page(&self, id: PageId) -> Result<Option<Page>, PortError> {
            Ok(self
                .pages
                .lock()
                .unwrap()
                .iter()
                .find(|p| p.id == id)
                .cloned())
        }
        fn rename_document(
            &self,
            id: DocumentId,
            title: &str,
            updated_at_ms: i64,
        ) -> Result<Option<Document>, PortError> {
            let mut docs = self.docs.lock().unwrap();
            Ok(docs.get_mut(&id).map(|d| {
                d.title = title.to_owned();
                d.updated_at_ms = updated_at_ms;
                d.clone()
            }))
        }
        fn delete_document(&self, id: DocumentId) -> Result<bool, PortError> {
            self.pages.lock().unwrap().retain(|p| p.document_id != id);
            Ok(self.docs.lock().unwrap().remove(&id).is_some())
        }
        fn delete_page(&self, id: PageId, _updated_at_ms: i64) -> Result<bool, PortError> {
            let mut pages = self.pages.lock().unwrap();
            let Some(index) = pages.iter().position(|p| p.id == id) else {
                return Ok(false);
            };
            let removed = pages.remove(index);
            renumber(&mut pages, removed.document_id);
            self.docs
                .lock()
                .unwrap()
                .get_mut(&removed.document_id)
                .unwrap()
                .page_count -= 1;
            Ok(true)
        }
        fn move_page(
            &self,
            id: PageId,
            to: u32,
            _updated_at_ms: i64,
        ) -> Result<Vec<Page>, PortError> {
            let mut pages = self.pages.lock().unwrap();
            let page = pages.iter().find(|p| p.id == id).cloned().unwrap();
            let mut order: Vec<Page> = pages
                .iter()
                .filter(|p| p.document_id == page.document_id)
                .cloned()
                .collect();
            order.sort_by_key(|p| p.position);
            let from = order.iter().position(|p| p.id == id).unwrap();
            let moved = order.remove(from);
            let to = (to as usize).min(order.len());
            order.insert(to, moved);
            for (position, p) in order.iter().enumerate() {
                pages.iter_mut().find(|x| x.id == p.id).unwrap().position = position as u32;
            }
            let mut result: Vec<Page> = pages
                .iter()
                .filter(|p| p.document_id == page.document_id)
                .cloned()
                .collect();
            result.sort_by_key(|p| p.position);
            Ok(result)
        }
        fn set_page_rendering(
            &self,
            id: PageId,
            crop: Option<&Crop>,
            filter: PageFilter,
            rotation: Rotation,
            processed_path: Option<&Path>,
            _updated_at_ms: i64,
        ) -> Result<Page, PortError> {
            if self.fail_set_crop {
                return Err(PortError::new("set_page_rendering", "simulated failure"));
            }
            let mut pages = self.pages.lock().unwrap();
            let page = pages.iter_mut().find(|p| p.id == id).unwrap();
            page.crop = crop.copied();
            page.filter = filter;
            page.rotation = rotation;
            page.processed_path = processed_path.map(Path::to_path_buf);
            Ok(page.clone())
        }
        fn save_page_text(&self, text: &PageText, plain: &str) -> Result<(), PortError> {
            self.texts
                .lock()
                .unwrap()
                .insert(text.page_id, (text.clone(), plain.to_owned()));
            Ok(())
        }
        fn get_page_text(&self, page_id: PageId) -> Result<Option<PageText>, PortError> {
            Ok(self
                .texts
                .lock()
                .unwrap()
                .get(&page_id)
                .map(|(t, _)| t.clone()))
        }
        fn pages_without_text(&self, limit: u32) -> Result<Vec<Page>, PortError> {
            let texts = self.texts.lock().unwrap();
            Ok(self
                .pages
                .lock()
                .unwrap()
                .iter()
                .filter(|p| {
                    texts
                        .get(&p.id)
                        .is_none_or(|(t, _)| t.source_path != displayed_image(p))
                })
                .take(limit as usize)
                .cloned()
                .collect())
        }
        fn search_text(&self, query: &str, _limit: u32) -> Result<Vec<TextMatch>, PortError> {
            *self.searched.lock().unwrap() = Some(query.to_owned());
            Ok(Vec::new())
        }
        fn referenced_files(&self) -> Result<Vec<PathBuf>, PortError> {
            let pages = self.pages.lock().unwrap();
            Ok(pages
                .iter()
                .flat_map(|p| {
                    std::iter::once(p.original_path.clone()).chain(p.processed_path.clone())
                })
                .collect())
        }
    }

    fn renumber(pages: &mut [Page], document_id: DocumentId) {
        let mut ids: Vec<(u32, PageId)> = pages
            .iter()
            .filter(|p| p.document_id == document_id)
            .map(|p| (p.position, p.id))
            .collect();
        ids.sort();
        for (position, (_, id)) in ids.into_iter().enumerate() {
            pages.iter_mut().find(|p| p.id == id).unwrap().position = position as u32;
        }
    }

    /// Tracks which processed files "exist", to check cleanup.
    #[derive(Default)]
    struct FakeFiles {
        removed_documents: Mutex<Vec<DocumentId>>,
        removed_pages: Mutex<Vec<PageId>>,
        processed: Mutex<HashSet<PathBuf>>,
        counter: Mutex<u32>,
        fail_copy: Mutex<bool>,
        /// What the last cleanup was asked to keep, and with which grace.
        swept: Mutex<Option<(HashSet<PathBuf>, Duration)>>,
    }

    impl PageFileStore for FakeFiles {
        fn import_original(
            &self,
            d: DocumentId,
            p: PageId,
            f: ImageFormat,
            _source: &Path,
        ) -> Result<PathBuf, PortError> {
            Ok(PathBuf::from(format!("{d}/{p}.{}", f.extension())))
        }
        fn new_processed_path(&self, d: DocumentId, p: PageId) -> PathBuf {
            let mut counter = self.counter.lock().unwrap();
            *counter += 1;
            PathBuf::from(format!("{d}/{p}/processed-{counter}.jpg"))
        }
        fn file_size(&self, _relative: &Path) -> Result<Option<u64>, PortError> {
            // Every stored file "weighs" 100 bytes.
            Ok(Some(100))
        }
        fn copy_file(&self, _from: &Path, to: &Path) -> Result<(), PortError> {
            if *self.fail_copy.lock().unwrap() {
                return Err(PortError::new("copy_file", "simulated failure"));
            }
            self.processed.lock().unwrap().insert(to.to_path_buf());
            Ok(())
        }
        fn remove_file(&self, relative: &Path) -> Result<(), PortError> {
            self.processed.lock().unwrap().remove(relative);
            Ok(())
        }
        fn remove_document_files(&self, d: DocumentId) -> Result<(), PortError> {
            self.removed_documents.lock().unwrap().push(d);
            Ok(())
        }
        fn remove_page_files(&self, _d: DocumentId, p: PageId) -> Result<(), PortError> {
            self.removed_pages.lock().unwrap().push(p);
            Ok(())
        }
        fn resolve(&self, relative: &Path) -> PathBuf {
            relative.to_path_buf()
        }
        fn remove_unreferenced(
            &self,
            referenced: &HashSet<PathBuf>,
            grace: Duration,
        ) -> Result<CleanupSummary, PortError> {
            *self.swept.lock().unwrap() = Some((referenced.clone(), grace));
            Ok(CleanupSummary {
                files_removed: 2,
                bytes_freed: 10,
            })
        }
    }

    /// "Renders" by recording the output path in the shared file set.
    #[derive(Default)]
    struct FakeProcessor {
        files: std::sync::Arc<Mutex<HashSet<PathBuf>>>,
        detection: Option<CornerDetection>,
        fail_render: Option<fn() -> ProcessingError>,
    }

    impl PageProcessor for FakeProcessor {
        fn detect_corners(
            &self,
            _original: &Path,
        ) -> Result<Option<CornerDetection>, ProcessingError> {
            Ok(self.detection)
        }
        fn render_page(
            &self,
            _original: &Path,
            _crop: Option<&Crop>,
            _filter: PageFilter,
            _rotation: Rotation,
            output: &Path,
        ) -> Result<(), ProcessingError> {
            if let Some(fail) = self.fail_render {
                return Err(fail());
            }
            self.files.lock().unwrap().insert(output.to_path_buf());
            Ok(())
        }
    }

    /// Records what it was asked to export.
    #[derive(Default)]
    struct FakeExporter {
        exported: Mutex<Vec<(String, Vec<ExportPage>)>>,
    }

    impl DocumentExporter for FakeExporter {
        fn export_pdf(
            &self,
            title: &str,
            pages: &[ExportPage],
            _options: &PdfExportOptions,
            _output: &Path,
        ) -> Result<ExportSummary, ProcessingError> {
            self.exported
                .lock()
                .unwrap()
                .push((title.to_owned(), pages.to_vec()));
            Ok(ExportSummary {
                page_count: pages.len() as u32,
                size_bytes: 1,
            })
        }

        fn export_docx(
            &self,
            title: &str,
            pages: &[ExportPage],
            _options: &DocxExportOptions,
            _output: &Path,
        ) -> Result<ExportSummary, ProcessingError> {
            self.exported
                .lock()
                .unwrap()
                .push((format!("docx:{title}"), pages.to_vec()));
            Ok(ExportSummary {
                page_count: pages.len() as u32,
                size_bytes: 2,
            })
        }
    }

    type TestService =
        DocumentService<InMemoryRepo, FakeFiles, FakeProcessor, FakeExporter, FixedClock>;

    fn service(repo: InMemoryRepo) -> TestService {
        service_with(repo, FakeProcessor::default())
    }

    fn service_with(repo: InMemoryRepo, processor: FakeProcessor) -> TestService {
        DocumentService::new(
            repo,
            FakeFiles::default(),
            processor,
            FakeExporter::default(),
            FixedClock(1_000),
        )
    }

    fn temp_image(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, b"fake").unwrap();
        (dir, path)
    }

    fn crop() -> Crop {
        Crop::new([(0.1, 0.1), (0.9, 0.1), (0.9, 0.9), (0.1, 0.9)]).unwrap()
    }

    #[test]
    fn create_document_trims_title_and_sets_timestamps() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("  Factura luz  ").unwrap();
        assert_eq!(doc.title, "Factura luz");
        assert_eq!(doc.created_at_ms, 1_000);
        assert_eq!(doc.page_count, 0);
    }

    #[test]
    fn create_document_rejects_bad_titles() {
        let svc = service(InMemoryRepo::default());
        assert!(svc.create_document("   ").is_err());
        assert!(svc.create_document("a\u{0007}b").is_err());
        assert!(
            svc.create_document(&"x".repeat(MAX_TITLE_CHARS + 1))
                .is_err()
        );
        assert!(svc.create_document(&"á".repeat(MAX_TITLE_CHARS)).is_ok());
    }

    #[test]
    fn add_page_appends_in_order() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.JPG");

        let p0 = svc.add_page(doc.id, &img).unwrap();
        let p1 = svc.add_page(doc.id, &img).unwrap();

        assert_eq!((p0.position, p1.position), (0, 1));
        assert!(p0.original_path.to_string_lossy().ends_with(".jpg"));
        assert_eq!(p0.processed_path, None);
        assert_eq!(svc.list_pages(doc.id).unwrap().len(), 2);
    }

    #[test]
    fn add_page_validates_inputs() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.png");

        assert!(matches!(
            svc.add_page(DocumentId::new(), &img),
            Err(DocumentError::DocumentNotFound(_))
        ));
        assert!(matches!(
            svc.add_page(doc.id, Path::new("/nope/missing.png")),
            Err(DocumentError::SourceImageNotFound(_))
        ));
        let (_d2, pdf) = temp_image("file.pdf");
        assert!(matches!(
            svc.add_page(doc.id, &pdf),
            Err(DocumentError::UnsupportedImageFormat { .. })
        ));
    }

    #[test]
    fn add_page_cleans_up_files_when_persistence_fails() {
        let repo = InMemoryRepo {
            fail_append: true,
            ..Default::default()
        };
        let svc = service(repo);
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.png");

        assert!(svc.add_page(doc.id, &img).is_err());
        assert_eq!(svc.files().removed_pages.lock().unwrap().len(), 1);
    }

    #[test]
    fn crop_rejects_coordinates_outside_the_image() {
        assert!(Crop::new([(0.0, 0.0), (1.1, 0.0), (1.0, 1.0), (0.0, 1.0)]).is_err());
        assert!(Crop::new([(f32::NAN, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]).is_err());
    }

    #[test]
    fn detect_corners_reports_missing_pages() {
        let svc = service(InMemoryRepo::default());
        assert!(matches!(
            svc.detect_corners(PageId::new()),
            Err(DocumentError::PageNotFound(_))
        ));
    }

    #[test]
    fn crop_page_records_the_crop_and_replaces_the_previous_image() {
        let processor = FakeProcessor::default();
        let rendered = processor.files.clone();
        let svc = service_with(InMemoryRepo::default(), processor);
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let page = svc.add_page(doc.id, &img).unwrap();

        let first = svc.crop_page(page.id, crop()).unwrap();
        let first_path = first.processed_path.clone().unwrap();
        assert_eq!(first.crop, Some(crop()));

        // The fake file store and processor share no state, so mirror the
        // rendered file into the store's view to observe its removal.
        svc.files()
            .processed
            .lock()
            .unwrap()
            .insert(first_path.clone());
        let second = svc.crop_page(page.id, crop()).unwrap();

        assert_ne!(second.processed_path, Some(first_path.clone()));
        assert!(!svc.files().processed.lock().unwrap().contains(&first_path));
        assert_eq!(rendered.lock().unwrap().len(), 2);
        assert_eq!(svc.get_page(page.id).unwrap(), second);
    }

    #[test]
    fn filters_keep_the_crop_and_crops_keep_the_filter() {
        let processor = FakeProcessor::default();
        let rendered = processor.files.clone();
        let svc = service_with(InMemoryRepo::default(), processor);
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let page = svc.add_page(doc.id, &img).unwrap();

        // A filter alone renders the whole photo.
        let filtered = svc
            .set_page_filter(page.id, PageFilter::BlackAndWhite)
            .unwrap();
        assert_eq!(
            (filtered.crop, filtered.filter),
            (None, PageFilter::BlackAndWhite)
        );
        assert!(filtered.processed_path.is_some());

        let cropped = svc.crop_page(page.id, crop()).unwrap();
        assert_eq!(
            (cropped.crop, cropped.filter),
            (Some(crop()), PageFilter::BlackAndWhite)
        );

        let refiltered = svc.set_page_filter(page.id, PageFilter::Enhanced).unwrap();
        assert_eq!(
            (refiltered.crop, refiltered.filter),
            (Some(crop()), PageFilter::Enhanced)
        );
        assert_eq!(rendered.lock().unwrap().len(), 3);
    }

    #[test]
    fn original_filter_without_crop_shows_the_original_again() {
        let svc = service_with(InMemoryRepo::default(), FakeProcessor::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let page = svc.add_page(doc.id, &img).unwrap();

        let filtered = svc.set_page_filter(page.id, PageFilter::Grayscale).unwrap();
        let previous = filtered.processed_path.clone().unwrap();
        svc.files()
            .processed
            .lock()
            .unwrap()
            .insert(previous.clone());

        let restored = svc.set_page_filter(page.id, PageFilter::Original).unwrap();
        assert_eq!(restored.processed_path, None);
        assert_eq!(restored.filter, PageFilter::Original);
        assert!(!svc.files().processed.lock().unwrap().contains(&previous));
        assert!(matches!(
            svc.set_page_filter(PageId::new(), PageFilter::Grayscale),
            Err(DocumentError::PageNotFound(_))
        ));
    }

    #[test]
    fn crop_page_keeps_the_page_unchanged_when_saving_fails() {
        let repo = InMemoryRepo {
            fail_set_crop: true,
            ..Default::default()
        };
        let svc = service(repo);
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let page = svc.add_page(doc.id, &img).unwrap();

        assert!(svc.crop_page(page.id, crop()).is_err());
        assert_eq!(svc.get_page(page.id).unwrap().processed_path, None);
    }

    #[test]
    fn crop_page_maps_processor_errors() {
        let processor = FakeProcessor {
            fail_render: Some(|| ProcessingError::UnsupportedFormat),
            ..Default::default()
        };
        let svc = service_with(InMemoryRepo::default(), processor);
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("photo.heic");
        let page = svc.add_page(doc.id, &img).unwrap();

        let err = svc.crop_page(page.id, crop()).unwrap_err();
        assert!(
            matches!(err, DocumentError::UnsupportedImageFormat { extension } if extension == "heic")
        );
    }

    const PDF: PdfExportOptions = PdfExportOptions {
        paper_size: crate::ports::PaperSize::A4,
        margin_mm: 0.0,
        page_numbers: false,
        password: None,
    };

    #[test]
    fn export_uses_processed_images_when_available() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Contrato").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let cropped = svc.add_page(doc.id, &img).unwrap();
        let plain = svc.add_page(doc.id, &img).unwrap();
        let cropped = svc.crop_page(cropped.id, crop()).unwrap();

        let summary = svc.export_pdf(doc.id, Path::new("/out.pdf"), &PDF).unwrap();

        assert_eq!(summary.page_count, 2);
        let exported = svc.exporter.exported.lock().unwrap();
        let (title, pages) = &exported[0];
        assert_eq!(title, "Contrato");
        let images: Vec<_> = pages.iter().map(|p| p.image.clone()).collect();
        assert_eq!(
            images,
            vec![cropped.processed_path.unwrap(), plain.original_path]
        );
    }

    #[test]
    fn export_includes_only_current_text() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Boletas").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let read = svc.add_page(doc.id, &img).unwrap();
        let outdated = svc.add_page(doc.id, &img).unwrap();
        let pending = svc.add_page(doc.id, &img).unwrap();
        for page in [&read, &outdated] {
            let image = svc.files().resolve(&page.original_path);
            svc.save_page_text(page.id, &image, vec![text_line(0, "Total")])
                .unwrap();
        }
        svc.crop_page(outdated.id, crop()).unwrap();
        let _ = pending;

        svc.export_pdf(doc.id, Path::new("/out.pdf"), &PDF).unwrap();

        let exported = svc.exporter.exported.lock().unwrap();
        let texts: Vec<Vec<&str>> = exported[0]
            .1
            .iter()
            .map(|p| p.text.iter().map(|l| l.text.as_str()).collect())
            .collect();
        assert_eq!(texts, vec![vec!["Total"], vec![], vec![]]);
    }

    #[test]
    fn docx_export_reports_pages_still_pending() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Carta").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let read = svc.add_page(doc.id, &img).unwrap();
        svc.add_page(doc.id, &img).unwrap();
        let image = svc.files().resolve(&read.original_path);
        svc.save_page_text(read.id, &image, vec![text_line(0, "Estimado")])
            .unwrap();

        let export = svc
            .export_docx(
                doc.id,
                Path::new("/out.docx"),
                &DocxExportOptions::default(),
            )
            .unwrap();

        assert_eq!(export.pages_pending_text, 1);
        assert_eq!(export.summary.page_count, 2);
        let exported = svc.exporter.exported.lock().unwrap();
        assert_eq!(exported[0].0, "docx:Carta");
        drop(exported);
        assert!(matches!(
            svc.export_docx(
                DocumentId::new(),
                Path::new("/x.docx"),
                &DocxExportOptions::default()
            ),
            Err(DocumentError::DocumentNotFound(_))
        ));
    }

    #[test]
    fn export_rejects_empty_or_missing_documents() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Vacío").unwrap();

        assert!(matches!(
            svc.export_pdf(doc.id, Path::new("/out.pdf"), &PDF),
            Err(DocumentError::EmptyDocument(_))
        ));
        assert!(matches!(
            svc.export_pdf(DocumentId::new(), Path::new("/out.pdf"), &PDF),
            Err(DocumentError::DocumentNotFound(_))
        ));
    }

    #[test]
    fn rename_validates_and_updates_the_title() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Borrador").unwrap();

        let renamed = svc.rename_document(doc.id, "  Contrato final ").unwrap();

        assert_eq!(renamed.title, "Contrato final");
        assert!(svc.rename_document(doc.id, "   ").is_err());
        assert!(matches!(
            svc.rename_document(DocumentId::new(), "x"),
            Err(DocumentError::DocumentNotFound(_))
        ));
    }

    #[test]
    fn delete_document_removes_pages_and_files() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        svc.add_page(doc.id, &img).unwrap();

        svc.delete_document(doc.id).unwrap();

        assert!(svc.list_documents().unwrap().is_empty());
        assert_eq!(*svc.files().removed_documents.lock().unwrap(), vec![doc.id]);
        assert!(matches!(
            svc.delete_document(doc.id),
            Err(DocumentError::DocumentNotFound(_))
        ));
    }

    #[test]
    fn delete_page_closes_the_gap() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let pages: Vec<_> = (0..3)
            .map(|_| svc.add_page(doc.id, &img).unwrap())
            .collect();

        svc.delete_page(pages[1].id).unwrap();

        let remaining = svc.list_pages(doc.id).unwrap();
        assert_eq!(
            remaining
                .iter()
                .map(|p| (p.id, p.position))
                .collect::<Vec<_>>(),
            vec![(pages[0].id, 0), (pages[2].id, 1)]
        );
        assert_eq!(
            *svc.files().removed_pages.lock().unwrap(),
            vec![pages[1].id]
        );
        assert!(matches!(
            svc.delete_page(pages[1].id),
            Err(DocumentError::PageNotFound(_))
        ));
    }

    #[test]
    fn move_page_reorders_and_clamps() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let ids: Vec<_> = (0..4)
            .map(|_| svc.add_page(doc.id, &img).unwrap().id)
            .collect();

        let order = svc.move_page(ids[3], 0).unwrap();
        assert_eq!(
            order.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![ids[3], ids[0], ids[1], ids[2]]
        );

        let order = svc.move_page(ids[3], 99).unwrap();
        assert_eq!(order.iter().map(|p| p.id).collect::<Vec<_>>(), ids);
        assert!(
            order
                .iter()
                .enumerate()
                .all(|(i, p)| p.position == i as u32)
        );
    }

    #[test]
    fn orphan_cleanup_keeps_every_referenced_file() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let plain = svc.add_page(doc.id, &img).unwrap();
        let cropped = svc.add_page(doc.id, &img).unwrap();
        let cropped = svc.crop_page(cropped.id, crop()).unwrap();

        let summary = svc.remove_orphan_files().unwrap();

        assert_eq!(summary.files_removed, 2);
        let (kept, grace) = svc.files().swept.lock().unwrap().clone().unwrap();
        let expected: HashSet<PathBuf> = [
            plain.original_path,
            cropped.original_path,
            cropped.processed_path.unwrap(),
        ]
        .into();
        assert_eq!(kept, expected);
        assert_eq!(grace, ORPHAN_GRACE);
    }

    fn text_line(block: u32, text: &str) -> TextLine {
        TextLine {
            block,
            text: text.into(),
            bounds: crate::TextBounds::new(0.1, 0.1, 0.9, 0.2).unwrap(),
        }
    }

    #[test]
    fn recognized_text_follows_the_displayed_image() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let page = svc.add_page(doc.id, &img).unwrap();
        assert_eq!(svc.pages_needing_text(10).unwrap(), vec![page.clone()]);
        assert_eq!(svc.page_text(page.id).unwrap(), None);

        let image = svc.files().resolve(&page.original_path);
        let saved = svc
            .save_page_text(
                page.id,
                &image,
                vec![text_line(0, " Boleta "), text_line(1, "Total")],
            )
            .unwrap();
        assert_eq!(saved.plain_text(), "Boleta\n\nTotal");
        assert_eq!(saved.recognized_at_ms, 1_000);
        assert_eq!(svc.page_text(page.id).unwrap(), Some(saved));
        assert!(svc.pages_needing_text(10).unwrap().is_empty());

        // Cropping changes the image: the old text no longer applies.
        let cropped = svc.crop_page(page.id, crop()).unwrap();
        assert_eq!(svc.page_text(page.id).unwrap(), None);
        assert_eq!(svc.pages_needing_text(10).unwrap(), vec![cropped]);
    }

    #[test]
    fn text_from_an_outdated_image_is_rejected() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let page = svc.add_page(doc.id, &img).unwrap();
        let before = svc.files().resolve(&page.original_path);
        svc.crop_page(page.id, crop()).unwrap();

        assert!(matches!(
            svc.save_page_text(page.id, &before, vec![text_line(0, "x")]),
            Err(DocumentError::PageImageChanged(id)) if id == page.id
        ));
        assert!(matches!(
            svc.save_page_text(PageId::new(), &before, vec![]),
            Err(DocumentError::PageNotFound(_))
        ));
    }

    #[test]
    fn blank_searches_skip_the_index() {
        let svc = service(InMemoryRepo::default());
        assert!(svc.search_text("   ").unwrap().is_empty());
        svc.search_text("boleta luz").unwrap();
        assert_eq!(
            svc.repository.searched.lock().unwrap().as_deref(),
            Some("boleta luz")
        );
    }

    #[test]
    fn trashed_documents_leave_the_list_until_restored() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Boleta").unwrap();
        let other = svc.create_document("Otro").unwrap();

        svc.move_to_trash(doc.id).unwrap();

        assert_eq!(ids_of(svc.list_documents().unwrap()), vec![other.id]);
        let trash = svc.list_trash().unwrap();
        assert_eq!(ids_of(trash.clone()), vec![doc.id]);
        assert_eq!(trash[0].trashed_at_ms, Some(1_000));

        let restored = svc.restore_from_trash(doc.id).unwrap();
        assert_eq!(restored.trashed_at_ms, None);
        assert!(svc.list_trash().unwrap().is_empty());
        assert!(matches!(
            svc.move_to_trash(DocumentId::new()),
            Err(DocumentError::DocumentNotFound(_))
        ));
    }

    #[test]
    fn emptying_and_expiry_delete_for_good() {
        let svc = service(InMemoryRepo::default());
        let old = svc.create_document("Viejo").unwrap();
        let recent = svc.create_document("Reciente").unwrap();
        let kept = svc.create_document("Activo").unwrap();
        svc.repository
            .set_document_trashed(old.id, Some(1_000 - TRASH_RETENTION_MS - 1))
            .unwrap();
        svc.move_to_trash(recent.id).unwrap();

        assert_eq!(svc.purge_expired_trash().unwrap(), 1);
        assert_eq!(ids_of(svc.list_trash().unwrap()), vec![recent.id]);
        assert_eq!(*svc.files().removed_documents.lock().unwrap(), vec![old.id]);

        assert_eq!(svc.empty_trash().unwrap(), 1);
        assert!(svc.list_trash().unwrap().is_empty());
        assert_eq!(ids_of(svc.list_documents().unwrap()), vec![kept.id]);
    }

    #[test]
    fn duplicates_copy_pages_rendering_and_text() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Contrato").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let plain = svc.add_page(doc.id, &img).unwrap();
        let cropped = svc.add_page(doc.id, &img).unwrap();
        let cropped = svc.crop_page(cropped.id, crop()).unwrap();
        let image = svc
            .files()
            .resolve(cropped.processed_path.as_ref().unwrap());
        svc.save_page_text(cropped.id, &image, vec![text_line(0, "Firma")])
            .unwrap();

        let copy = svc
            .duplicate_document(doc.id, " Contrato (copia) ")
            .unwrap();

        assert_eq!(copy.title, "Contrato (copia)");
        assert_ne!(copy.id, doc.id);
        let pages = svc.list_pages(copy.id).unwrap();
        assert_eq!(pages.len(), 2);
        assert!(pages.iter().all(|p| p.id != plain.id && p.id != cropped.id));
        assert_eq!(pages[0].processed_path, None);
        assert_eq!(pages[1].crop, cropped.crop);
        let copied = pages[1].processed_path.clone().unwrap();
        assert_ne!(Some(copied.clone()), cropped.processed_path, "own file");
        assert!(svc.files().processed.lock().unwrap().contains(&copied));
        let text = svc.page_text(pages[1].id).unwrap().unwrap();
        assert_eq!(text.plain_text(), "Firma");
        assert_eq!(svc.page_text(pages[0].id).unwrap(), None);
        // The original is untouched.
        assert_eq!(svc.list_pages(doc.id).unwrap().len(), 2);
    }

    #[test]
    fn failed_duplicates_leave_nothing_behind() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Contrato").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let page = svc.add_page(doc.id, &img).unwrap();
        svc.crop_page(page.id, crop()).unwrap();
        *svc.files().fail_copy.lock().unwrap() = true;

        assert!(svc.duplicate_document(doc.id, "Copia").is_err());

        assert_eq!(ids_of(svc.list_documents().unwrap()), vec![doc.id]);
        assert!(matches!(
            svc.duplicate_document(doc.id, "   "),
            Err(DocumentError::InvalidTitle { .. })
        ));
    }

    fn ids_of(documents: Vec<Document>) -> Vec<DocumentId> {
        documents.into_iter().map(|d| d.id).collect()
    }

    #[test]
    fn folders_hold_documents_until_deleted() {
        let svc = service(InMemoryRepo::default());
        let bills = svc.create_folder(" Cuentas ").unwrap();
        let work = svc.create_folder("archivo").unwrap();
        assert_eq!(bills.name, "Cuentas");
        let names: Vec<_> = svc
            .list_folders()
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["archivo", "Cuentas"], "by name, ignoring case");

        let inside = svc.create_document_in("Luz", Some(bills.id)).unwrap();
        let loose = svc.create_document("Suelto").unwrap();
        assert_eq!(inside.folder_id, Some(bills.id));
        assert_eq!(
            svc.move_documents(&[loose.id, DocumentId::new()], Some(bills.id))
                .unwrap(),
            1
        );
        assert_eq!(svc.get_folder(bills.id).unwrap().document_count, 2);

        svc.move_to_trash(loose.id).unwrap();
        assert_eq!(
            svc.get_folder(bills.id).unwrap().document_count,
            1,
            "trash doesn't count"
        );

        assert_eq!(
            svc.rename_folder(work.id, "Trabajo").unwrap().name,
            "Trabajo"
        );
        svc.delete_folder(bills.id).unwrap();
        let docs = svc.list_documents().unwrap();
        assert!(
            docs.iter().all(|d| d.folder_id.is_none()),
            "documents survive"
        );
        assert_eq!(docs.len(), 1);
    }

    #[test]
    fn folder_operations_check_the_folder_exists() {
        let svc = service(InMemoryRepo::default());
        let missing = FolderId::new();
        let doc = svc.create_document("Doc").unwrap();
        for result in [
            svc.create_document_in("x", Some(missing)).map(|_| ()),
            svc.move_documents(&[doc.id], Some(missing)).map(|_| ()),
            svc.rename_folder(missing, "x").map(|_| ()),
            svc.delete_folder(missing),
        ] {
            assert!(matches!(result, Err(DocumentError::FolderNotFound(id)) if id == missing));
        }
        assert!(matches!(
            svc.create_folder("  "),
            Err(DocumentError::InvalidTitle { .. })
        ));
        assert_eq!(
            svc.move_documents(&[doc.id], None).unwrap(),
            1,
            "back to the top level"
        );
    }

    #[test]
    fn listings_include_sizes_and_copies_stay_in_their_folder() {
        let svc = service(InMemoryRepo::default());
        let folder = svc.create_folder("Cuentas").unwrap();
        let doc = svc.create_document_in("Luz", Some(folder.id)).unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        svc.add_page(doc.id, &img).unwrap();
        let page = svc.add_page(doc.id, &img).unwrap();
        svc.crop_page(page.id, crop()).unwrap();
        svc.create_document("Vacío").unwrap();

        let sizes: HashMap<_, _> = svc
            .list_documents()
            .unwrap()
            .into_iter()
            .map(|d| (d.title, d.size_bytes))
            .collect();
        assert_eq!(sizes["Luz"], 300, "two originals and one processed image");
        assert_eq!(sizes["Vacío"], 0);

        let copy = svc.duplicate_document(doc.id, "Luz (copia)").unwrap();
        assert_eq!((copy.folder_id, copy.size_bytes), (Some(folder.id), 300));
    }

    #[test]
    fn rotation_turns_both_ways() {
        assert_eq!(Rotation::None.turned(true), Rotation::Clockwise90);
        assert_eq!(Rotation::Clockwise270.turned(true), Rotation::None);
        assert_eq!(Rotation::None.turned(false), Rotation::Clockwise270);
        assert_eq!(Rotation::from_degrees(180).unwrap().degrees(), 180);
        assert!(matches!(
            Rotation::from_degrees(45),
            Err(DocumentError::InvalidRotation { degrees: 45 })
        ));
    }

    #[test]
    fn rotating_renders_and_keeps_crop_and_filter() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let page = svc.add_page(doc.id, &img).unwrap();
        let page = svc.set_page_filter(page.id, PageFilter::Grayscale).unwrap();

        let turned = svc.rotate_page(page.id, true).unwrap();
        assert_eq!(
            (turned.rotation, turned.filter),
            (Rotation::Clockwise90, PageFilter::Grayscale)
        );
        assert_ne!(turned.processed_path, page.processed_path, "a new image");

        // Four turns are a full circle: back to no rotation, still filtered.
        let mut page = turned;
        for _ in 0..3 {
            page = svc.rotate_page(page.id, true).unwrap();
        }
        assert_eq!(page.rotation, Rotation::None);
        assert!(
            page.processed_path.is_some(),
            "the filter still needs an image"
        );

        // A rotation alone also needs a processed image.
        let plain = svc.add_page(doc.id, &img).unwrap();
        let plain = svc.rotate_page(plain.id, false).unwrap();
        assert_eq!(plain.rotation, Rotation::Clockwise270);
        assert!(plain.processed_path.is_some());
    }

    #[test]
    fn whole_renderings_can_be_restored() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let original = svc.add_page(doc.id, &img).unwrap();
        let edited = svc.crop_page(original.id, crop()).unwrap();
        let edited = svc.rotate_page(edited.id, true).unwrap();

        // Restoring what it already has doesn't render again.
        let same = svc
            .set_page_rendering(edited.id, edited.crop, edited.filter, edited.rotation)
            .unwrap();
        assert_eq!(same.processed_path, edited.processed_path);

        // Undo everything: back to the plain original, no processed image.
        let undone = svc
            .set_page_rendering(edited.id, None, PageFilter::Original, Rotation::None)
            .unwrap();
        assert_eq!(
            (undone.crop, undone.rotation, undone.processed_path),
            (None, Rotation::None, None)
        );
    }

    #[test]
    fn duplicates_keep_the_rotation() {
        let svc = service(InMemoryRepo::default());
        let doc = svc.create_document("Doc").unwrap();
        let (_dir, img) = temp_image("scan.jpg");
        let page = svc.add_page(doc.id, &img).unwrap();
        svc.rotate_page(page.id, true).unwrap();

        let copy = svc.duplicate_document(doc.id, "Copia").unwrap();

        assert_eq!(
            svc.list_pages(copy.id).unwrap()[0].rotation,
            Rotation::Clockwise90
        );
    }
}
