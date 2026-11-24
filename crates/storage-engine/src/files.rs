use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use document_engine::{CleanupSummary, DocumentId, ImageFormat, PageFileStore, PageId, PortError};

/// [`PageFileStore`] on the local file system, rooted at the app's data dir.
///
/// Every path it builds comes from UUIDs and a fixed extension allowlist, so
/// caller input can never steer writes outside the root (no path traversal).
#[derive(Debug, Clone)]
pub struct LocalPageFileStore {
    root: PathBuf,
}

impl LocalPageFileStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, PortError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| PortError::new("create storage root", e))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Removes old unreferenced files under `rel_dir` (relative to the
    /// root), then subfolders left empty. Returns whether `rel_dir` ended up
    /// empty. Symlinks and other special entries are never followed or
    /// removed. Failing to remove one entry doesn't stop the sweep.
    fn sweep(
        &self,
        rel_dir: &Path,
        referenced: &HashSet<PathBuf>,
        cutoff: SystemTime,
        summary: &mut CleanupSummary,
    ) -> Result<bool, PortError> {
        let entries = match fs::read_dir(self.root.join(rel_dir)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(PortError::new("list stored files", e)),
        };
        let is_old = |meta: &fs::Metadata| meta.modified().is_ok_and(|t| t < cutoff);

        let mut empty = true;
        for entry in entries {
            let entry = entry.map_err(|e| PortError::new("list stored files", e))?;
            let rel = rel_dir.join(entry.file_name());
            // symlink_metadata: never follow links out of the storage root.
            let Ok(meta) = entry.path().symlink_metadata() else {
                empty = false;
                continue;
            };
            let removed = if meta.is_dir() {
                // Judge the folder's age before sweeping it: removing files
                // inside it bumps its modification time.
                self.sweep(&rel, referenced, cutoff, summary)?
                    && is_old(&meta)
                    && fs::remove_dir(entry.path()).is_ok()
            } else if meta.is_file() && !referenced.contains(&rel) && is_old(&meta) {
                let ok = fs::remove_file(entry.path()).is_ok();
                if ok {
                    summary.files_removed += 1;
                    summary.bytes_freed += meta.len();
                }
                ok
            } else {
                false
            };
            empty &= removed;
        }
        Ok(empty)
    }

    fn page_dir_relative(document_id: DocumentId, page_id: PageId) -> PathBuf {
        PathBuf::from("documents")
            .join(document_id.to_string())
            .join("pages")
            .join(page_id.to_string())
    }
}

impl PageFileStore for LocalPageFileStore {
    fn import_original(
        &self,
        document_id: DocumentId,
        page_id: PageId,
        format: ImageFormat,
        source: &Path,
    ) -> Result<PathBuf, PortError> {
        let dir_rel = Self::page_dir_relative(document_id, page_id);
        let dir_abs = self.root.join(&dir_rel);
        fs::create_dir_all(&dir_abs).map_err(|e| PortError::new("create page directory", e))?;

        let file_name = format!("original.{}", format.extension());
        let final_abs = dir_abs.join(&file_name);
        let partial_abs = dir_abs.join(format!("{file_name}.partial"));

        // Copy to a temporary name then rename, so a crash mid-copy never
        // leaves a truncated file under the final name.
        let result = fs::copy(source, &partial_abs)
            .and_then(|_| fs::File::open(&partial_abs)?.sync_all())
            .and_then(|()| fs::rename(&partial_abs, &final_abs));
        if let Err(e) = result {
            let _ = fs::remove_file(&partial_abs);
            return Err(PortError::new("import page image", e));
        }

        Ok(dir_rel.join(file_name))
    }

    fn new_processed_path(&self, document_id: DocumentId, page_id: PageId) -> PathBuf {
        Self::page_dir_relative(document_id, page_id)
            .join(format!("processed-{}.jpg", uuid::Uuid::new_v4().simple()))
    }

    fn copy_file(&self, from: &Path, to: &Path) -> Result<(), PortError> {
        let source = self.root.join(from);
        let target = self.root.join(to);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| PortError::new("create folder", e))?;
        }
        let partial = target.with_extension("partial");
        let result = fs::copy(&source, &partial)
            .and_then(|_| fs::File::open(&partial)?.sync_all())
            .and_then(|()| fs::rename(&partial, &target));
        if let Err(e) = result {
            let _ = fs::remove_file(&partial);
            return Err(PortError::new("copy file", e));
        }
        Ok(())
    }

    fn remove_file(&self, relative: &Path) -> Result<(), PortError> {
        match fs::remove_file(self.root.join(relative)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(PortError::new("remove file", e)),
        }
    }

    fn remove_document_files(&self, document_id: DocumentId) -> Result<(), PortError> {
        let dir = self.root.join("documents").join(document_id.to_string());
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(PortError::new("remove document files", e)),
        }
    }

    fn remove_page_files(&self, document_id: DocumentId, page_id: PageId) -> Result<(), PortError> {
        let dir = self
            .root
            .join(Self::page_dir_relative(document_id, page_id));
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(PortError::new("remove page files", e)),
        }
    }

    fn resolve(&self, relative: &Path) -> PathBuf {
        self.root.join(relative)
    }

    fn remove_unreferenced(
        &self,
        referenced: &HashSet<PathBuf>,
        grace: Duration,
    ) -> Result<CleanupSummary, PortError> {
        let cutoff = SystemTime::now()
            .checked_sub(grace)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut summary = CleanupSummary::default();
        // Only page images live under `documents/`; the database and
        // anything else in the root are never touched. The folder itself
        // stays even when empty.
        self.sweep(Path::new("documents"), referenced, cutoff, &mut summary)?;
        Ok(summary)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn imports_copy_and_removes_page_files() {
        let root = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("cam.jpeg");
        fs::write(&src, b"pixels").unwrap();

        let store = LocalPageFileStore::new(root.path()).unwrap();
        let (d, p) = (DocumentId::new(), PageId::new());
        let rel = store
            .import_original(d, p, ImageFormat::Jpeg, &src)
            .unwrap();

        assert!(rel.is_relative());
        assert!(rel.ends_with("original.jpg"));
        assert_eq!(fs::read(store.resolve(&rel)).unwrap(), b"pixels");
        assert!(src.exists(), "source must be copied, not moved");

        store.remove_page_files(d, p).unwrap();
        assert!(!store.resolve(&rel).exists());
        // Idempotent.
        store.remove_page_files(d, p).unwrap();

        let rel = store
            .import_original(d, PageId::new(), ImageFormat::Jpeg, &src)
            .unwrap();
        store.remove_document_files(d).unwrap();
        assert!(!store.resolve(&rel).exists());
        assert!(!root.path().join("documents").join(d.to_string()).exists());
        store.remove_document_files(d).unwrap();
    }

    #[test]
    fn processed_paths_are_unique_and_inside_the_page_directory() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalPageFileStore::new(root.path()).unwrap();
        let (d, p) = (DocumentId::new(), PageId::new());

        let a = store.new_processed_path(d, p);
        let b = store.new_processed_path(d, p);

        assert_ne!(a, b);
        assert!(a.starts_with(LocalPageFileStore::page_dir_relative(d, p)));
        assert_eq!(a.extension().unwrap(), "jpg");

        fs::create_dir_all(store.resolve(&a).parent().unwrap()).unwrap();
        fs::write(store.resolve(&a), b"jpeg").unwrap();
        store.remove_file(&a).unwrap();
        assert!(!store.resolve(&a).exists());
        store.remove_file(&a).unwrap(); // idempotent
    }

    #[test]
    fn failed_import_leaves_no_partial_file() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalPageFileStore::new(root.path()).unwrap();
        let (d, p) = (DocumentId::new(), PageId::new());

        let missing = root.path().join("missing.png");
        assert!(
            store
                .import_original(d, p, ImageFormat::Png, &missing)
                .is_err()
        );

        let page_dir = root
            .path()
            .join(LocalPageFileStore::page_dir_relative(d, p));
        assert_eq!(fs::read_dir(page_dir).unwrap().count(), 0);
    }

    /// Makes `path` look last modified an hour and a bit ago.
    fn age(path: &Path) {
        let past = SystemTime::now() - Duration::from_secs(3_700);
        fs::File::options()
            .write(true)
            .open(path)
            .or_else(|_| fs::File::open(path))
            .unwrap()
            .set_modified(past)
            .unwrap();
    }

    const HOUR: Duration = Duration::from_secs(3_600);

    #[test]
    fn cleanup_removes_old_unreferenced_files_only() {
        let root = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("cam.jpg");
        fs::write(&src, b"pixels").unwrap();
        let store = LocalPageFileStore::new(root.path()).unwrap();
        let (d, p) = (DocumentId::new(), PageId::new());

        let original = store
            .import_original(d, p, ImageFormat::Jpeg, &src)
            .unwrap();
        let stale = store.new_processed_path(d, p);
        let fresh = store.new_processed_path(d, p);
        fs::write(store.resolve(&stale), b"old render").unwrap();
        fs::write(store.resolve(&fresh), b"rendering now").unwrap();
        fs::write(root.path().join("nolva.sqlite3"), b"db").unwrap();
        for file in [&original, &stale] {
            age(&store.resolve(file));
        }
        age(&root.path().join("nolva.sqlite3"));

        let referenced = HashSet::from([original.clone()]);
        let summary = store.remove_unreferenced(&referenced, HOUR).unwrap();

        assert_eq!(
            summary,
            CleanupSummary {
                files_removed: 1,
                bytes_freed: 10,
            }
        );
        assert!(store.resolve(&original).exists(), "referenced");
        assert!(!store.resolve(&stale).exists(), "orphan");
        assert!(store.resolve(&fresh).exists(), "too recent to judge");
        assert!(
            root.path().join("nolva.sqlite3").exists(),
            "outside documents/"
        );
    }

    #[test]
    fn cleanup_removes_folders_of_deleted_documents() {
        let root = tempfile::tempdir().unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("cam.png");
        fs::write(&src, b"png").unwrap();
        let store = LocalPageFileStore::new(root.path()).unwrap();
        let (d, p) = (DocumentId::new(), PageId::new());
        let original = store.import_original(d, p, ImageFormat::Png, &src).unwrap();
        let page_dir = store.resolve(&LocalPageFileStore::page_dir_relative(d, p));
        let doc_dir = root.path().join("documents").join(d.to_string());
        age(&store.resolve(&original));
        for dir in [
            &page_dir,
            &page_dir.parent().unwrap().to_path_buf(),
            &doc_dir,
        ] {
            age(dir);
        }

        let summary = store.remove_unreferenced(&HashSet::new(), HOUR).unwrap();

        assert_eq!(summary.files_removed, 1);
        assert!(!doc_dir.exists());
        assert!(
            root.path().join("documents").exists(),
            "the root folder stays"
        );
        // Nothing left: a second run is a no-op.
        let again = store.remove_unreferenced(&HashSet::new(), HOUR).unwrap();
        assert_eq!(again, CleanupSummary::default());
    }

    #[test]
    fn cleanup_without_documents_folder_is_a_no_op() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalPageFileStore::new(root.path()).unwrap();
        let summary = store.remove_unreferenced(&HashSet::new(), HOUR).unwrap();
        assert_eq!(summary, CleanupSummary::default());
    }

    #[test]
    fn copies_files_into_new_folders() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalPageFileStore::new(root.path()).unwrap();
        let (d, p) = (DocumentId::new(), PageId::new());
        let from = store.new_processed_path(d, p);
        fs::create_dir_all(store.resolve(&from).parent().unwrap()).unwrap();
        fs::write(store.resolve(&from), b"jpeg").unwrap();

        let to = store.new_processed_path(DocumentId::new(), PageId::new());
        store.copy_file(&from, &to).unwrap();

        assert_eq!(fs::read(store.resolve(&to)).unwrap(), b"jpeg");
        assert!(store.resolve(&from).exists());
        let missing = store.new_processed_path(d, PageId::new());
        assert!(
            store
                .copy_file(&missing, &store.new_processed_path(d, p))
                .is_err()
        );
    }
}
