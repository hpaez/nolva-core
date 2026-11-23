use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use crate::clock::Clock;
use crate::error::{DocumentError, ProcessingError};
use crate::model::{Crop, Document, DocumentId, ImageFormat, NewPage, Page, PageFilter, PageId};
use crate::ports::{
    CleanupSummary, CornerDetection, DocumentExporter, DocumentRepository, ExportPage,
    ExportSummary, PageFileStore, PageProcessor, PdfExportOptions,
};
use crate::text::{PageText, TextLine, TextMatch, normalize_lines};

/// Maximum length of a document title, in characters.
pub const MAX_TITLE_CHARS: usize = 200;

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
        let title = normalize_title(title)?;
        let now = self.clock.now_ms();
        let document = Document {
            id: DocumentId::new(),
            title,
            created_at_ms: now,
            updated_at_ms: now,
            page_count: 0,
            cover_path: None,
        };
        self.repository.insert_document(&document)?;
        Ok(document)
    }

    pub fn list_documents(&self) -> Result<Vec<Document>, DocumentError> {
        Ok(self.repository.list_documents()?)
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
        let filter = page.filter;
        self.render(page, Some(crop), filter)
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
        let crop = page.crop;
        self.render(page, crop, filter)
    }

    /// Re-renders `page` with `crop` and `filter`.
    ///
    /// The new image goes to a fresh path and the database is updated only
    /// after it is fully written; the previous processed image is deleted
    /// last. A failure at any step leaves the page exactly as it was.
    fn render(
        &self,
        page: Page,
        crop: Option<Crop>,
        filter: PageFilter,
    ) -> Result<Page, DocumentError> {
        let processed = if crop.is_none() && filter == PageFilter::Original {
            None
        } else {
            let path = self.files.new_processed_path(page.document_id, page.id);
            let original = self.files.resolve(&page.original_path);
            self.processor
                .render_page(&original, crop.as_ref(), filter, &self.files.resolve(&path))
                .map_err(|e| processing_error(e, &page))?;
            Some(path)
        };

        let updated = self
            .repository
            .set_page_rendering(
                page.id,
                crop.as_ref(),
                filter,
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
        let document = self
            .repository
            .get_document(document_id)?
            .ok_or(DocumentError::DocumentNotFound(document_id))?;
        let pages = self.repository.list_pages(document_id)?;
        if pages.is_empty() {
            return Err(DocumentError::EmptyDocument(document_id));
        }
        let pages = pages
            .into_iter()
            .map(|page| {
                let image = displayed_image(&page);
                // Only text recognized in this very image lines up with it.
                let text = self
                    .repository
                    .get_page_text(page.id)?
                    .filter(|t| t.source_path == image)
                    .map(|t| t.lines)
                    .unwrap_or_default();
                Ok(ExportPage {
                    image: self.files.resolve(&image),
                    text,
                })
            })
            .collect::<Result<Vec<_>, DocumentError>>()?;

        self.exporter
            .export_pdf(&document.title, &pages, options, output)
            .map_err(|error| match error {
                ProcessingError::UnsupportedFormat => DocumentError::UnsupportedImageFormat {
                    extension: "page image".into(),
                },
                ProcessingError::InvalidCorners(reason) => DocumentError::InvalidCorners { reason },
                ProcessingError::Failed(port) => DocumentError::Storage(port),
            })
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
            Ok(self.docs.lock().unwrap().values().cloned().collect())
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
}
