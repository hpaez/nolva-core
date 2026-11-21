use std::path::Path;

use crate::clock::Clock;
use crate::error::{DocumentError, ProcessingError};
use crate::model::{Crop, Document, DocumentId, ImageFormat, NewPage, Page, PageId};
use crate::ports::{CornerDetection, DocumentRepository, PageFileStore, PageProcessor};

/// Maximum length of a document title, in characters.
pub const MAX_TITLE_CHARS: usize = 200;

/// Document use cases, independent of storage technology and platform.
#[derive(Debug)]
pub struct DocumentService<R, F, P, C> {
    repository: R,
    files: F,
    processor: P,
    clock: C,
}

impl<R, F, P, C> DocumentService<R, F, P, C>
where
    R: DocumentRepository,
    F: PageFileStore,
    P: PageProcessor,
    C: Clock,
{
    pub fn new(repository: R, files: F, processor: P, clock: C) -> Self {
        Self {
            repository,
            files,
            processor,
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
        };
        self.repository.insert_document(&document)?;
        Ok(document)
    }

    pub fn list_documents(&self) -> Result<Vec<Document>, DocumentError> {
        Ok(self.repository.list_documents()?)
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
    /// replacing any previous crop. The original image is never modified,
    /// so a page can be re-cropped any number of times.
    ///
    /// The new image goes to a fresh path and the database is updated only
    /// after it is fully written; the previous processed image is deleted
    /// last. A failure at any step leaves the page exactly as it was.
    pub fn crop_page(&self, page_id: PageId, crop: Crop) -> Result<Page, DocumentError> {
        let page = self.get_page(page_id)?;
        let original = self.files.resolve(&page.original_path);
        let processed = self.files.new_processed_path(page.document_id, page.id);

        self.processor
            .render_crop(&original, &crop, &self.files.resolve(&processed))
            .map_err(|e| processing_error(e, &page))?;

        let updated = self
            .repository
            .set_page_crop(page.id, &crop, &processed, self.clock.now_ms())
            .map_err(|err| {
                let _ = self.files.remove_file(&processed);
                DocumentError::from(err)
            })?;

        if let Some(previous) = page.processed_path {
            // Best effort: an orphan here only wastes space until cleanup.
            let _ = self.files.remove_file(&previous);
        }
        Ok(updated)
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
        fn set_page_crop(
            &self,
            id: PageId,
            crop: &Crop,
            processed_path: &Path,
            _updated_at_ms: i64,
        ) -> Result<Page, PortError> {
            if self.fail_set_crop {
                return Err(PortError::new("set_page_crop", "simulated failure"));
            }
            let mut pages = self.pages.lock().unwrap();
            let page = pages.iter_mut().find(|p| p.id == id).unwrap();
            page.crop = Some(*crop);
            page.processed_path = Some(processed_path.to_path_buf());
            Ok(page.clone())
        }
    }

    /// Tracks which processed files "exist", to check cleanup.
    #[derive(Default)]
    struct FakeFiles {
        removed_pages: Mutex<Vec<PageId>>,
        processed: Mutex<HashSet<PathBuf>>,
        counter: Mutex<u32>,
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
        fn remove_page_files(&self, _d: DocumentId, p: PageId) -> Result<(), PortError> {
            self.removed_pages.lock().unwrap().push(p);
            Ok(())
        }
        fn resolve(&self, relative: &Path) -> PathBuf {
            relative.to_path_buf()
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
        fn render_crop(
            &self,
            _original: &Path,
            _crop: &Crop,
            output: &Path,
        ) -> Result<(), ProcessingError> {
            if let Some(fail) = self.fail_render {
                return Err(fail());
            }
            self.files.lock().unwrap().insert(output.to_path_buf());
            Ok(())
        }
    }

    type TestService = DocumentService<InMemoryRepo, FakeFiles, FakeProcessor, FixedClock>;

    fn service(repo: InMemoryRepo) -> TestService {
        service_with(repo, FakeProcessor::default())
    }

    fn service_with(repo: InMemoryRepo, processor: FakeProcessor) -> TestService {
        DocumentService::new(repo, FakeFiles::default(), processor, FixedClock(1_000))
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
}
