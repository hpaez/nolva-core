use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use document_engine::{
    Crop, Document, DocumentId, DocumentRepository, NewPage, Page, PageId, PortError,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params};

use crate::migrations;

/// [`DocumentRepository`] backed by a single SQLite connection.
///
/// One connection guarded by a mutex is enough for a mobile app and keeps
/// write ordering trivial. WAL mode lets a future read-only connection run
/// concurrently if profiling ever shows contention.
#[derive(Debug)]
pub struct SqliteDocumentRepository {
    conn: Mutex<Connection>,
}

impl SqliteDocumentRepository {
    /// Opens (creating if needed) the database at `path` and migrates it.
    pub fn open(path: &Path) -> Result<Self, PortError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| PortError::new("open database", e))?;
        Self::init(conn)
    }

    /// In-memory database, for tests.
    pub fn open_in_memory() -> Result<Self, PortError> {
        let conn = Connection::open_in_memory().map_err(|e| PortError::new("open database", e))?;
        Self::init(conn)
    }

    fn init(mut conn: Connection) -> Result<Self, PortError> {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )
        .map_err(|e| PortError::new("configure database", e))?;
        migrations::run(&mut conn).map_err(|e| PortError::new("migrate database", e))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A poisoned lock only means another thread panicked mid-call; SQLite
        // itself rolled back any open transaction, so the connection is usable.
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }
}

const DOCUMENT_COLUMNS: &str = "d.id, d.title, d.created_at_ms, d.updated_at_ms, \
     (SELECT COUNT(*) FROM pages p WHERE p.document_id = d.id)";

fn document_from_row(row: &Row<'_>) -> rusqlite::Result<Document> {
    Ok(Document {
        id: parse_id(row, 0)?,
        title: row.get(1)?,
        created_at_ms: row.get(2)?,
        updated_at_ms: row.get(3)?,
        page_count: row.get(4)?,
    })
}

const PAGE_COLUMNS: &str =
    "id, document_id, position, original_path, created_at_ms, crop, processed_path";

fn page_from_row(row: &Row<'_>) -> rusqlite::Result<Page> {
    let original: String = row.get(3)?;
    let crop: Option<String> = row.get(5)?;
    let processed: Option<String> = row.get(6)?;
    Ok(Page {
        id: parse_id::<PageId>(row, 0)?,
        document_id: parse_id(row, 1)?,
        position: row.get(2)?,
        original_path: PathBuf::from(original),
        crop: crop.map(|c| crop_from_db(&c)).transpose().map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, e.into())
        })?,
        processed_path: processed.map(PathBuf::from),
        created_at_ms: row.get(4)?,
    })
}

/// Rust's float `Display` is the shortest string that round-trips exactly.
fn crop_to_db(crop: &Crop) -> String {
    crop.corners()
        .iter()
        .flat_map(|&(x, y)| [x, y])
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn crop_from_db(value: &str) -> Result<Crop, String> {
    let numbers: Vec<f32> = value
        .split(',')
        .map(|n| n.trim().parse::<f32>().map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    let [x0, y0, x1, y1, x2, y2, x3, y3] = numbers[..] else {
        return Err(format!("expected 8 coordinates, got {}", numbers.len()));
    };
    Crop::new([(x0, y0), (x1, y1), (x2, y2), (x3, y3)]).map_err(|e| e.to_string())
}

fn parse_id<T: std::str::FromStr>(row: &Row<'_>, idx: usize) -> rusqlite::Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let raw: String = row.get(idx)?;
    raw.parse::<T>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Stored paths always use `/` so the database is portable across platforms.
fn path_to_db(path: &Path) -> Result<String, PortError> {
    let s = path
        .to_str()
        .ok_or_else(|| PortError::new("encode path", "path is not valid UTF-8"))?;
    Ok(s.replace('\\', "/"))
}

impl DocumentRepository for SqliteDocumentRepository {
    fn insert_document(&self, d: &Document) -> Result<(), PortError> {
        self.conn()
            .execute(
                "INSERT INTO documents (id, title, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![d.id.to_string(), d.title, d.created_at_ms, d.updated_at_ms],
            )
            .map_err(|e| PortError::new("insert document", e))?;
        Ok(())
    }

    fn get_document(&self, id: DocumentId) -> Result<Option<Document>, PortError> {
        self.conn()
            .query_row(
                &format!("SELECT {DOCUMENT_COLUMNS} FROM documents d WHERE d.id = ?1"),
                params![id.to_string()],
                document_from_row,
            )
            .optional()
            .map_err(|e| PortError::new("get document", e))
    }

    fn list_documents(&self) -> Result<Vec<Document>, PortError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {DOCUMENT_COLUMNS} FROM documents d
                 ORDER BY d.updated_at_ms DESC, d.id"
            ))
            .map_err(|e| PortError::new("list documents", e))?;
        stmt.query_map([], document_from_row)
            .and_then(Iterator::collect)
            .map_err(|e| PortError::new("list documents", e))
    }

    fn append_page(&self, p: &NewPage) -> Result<Page, PortError> {
        let original = path_to_db(&p.original_path)?;
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| PortError::new("append page", e))?;

        let position: u32 = tx
            .query_row(
                "SELECT COALESCE(MAX(position) + 1, 0) FROM pages WHERE document_id = ?1",
                params![p.document_id.to_string()],
                |r| r.get(0),
            )
            .map_err(|e| PortError::new("append page", e))?;

        tx.execute(
            "INSERT INTO pages (id, document_id, position, original_path, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                p.id.to_string(),
                p.document_id.to_string(),
                position,
                original,
                p.created_at_ms
            ],
        )
        .map_err(|e| PortError::new("append page", e))?;

        tx.execute(
            "UPDATE documents SET updated_at_ms = MAX(updated_at_ms, ?2) WHERE id = ?1",
            params![p.document_id.to_string(), p.created_at_ms],
        )
        .map_err(|e| PortError::new("append page", e))?;

        tx.commit().map_err(|e| PortError::new("append page", e))?;

        Ok(Page {
            id: p.id,
            document_id: p.document_id,
            position,
            original_path: p.original_path.clone(),
            crop: None,
            processed_path: None,
            created_at_ms: p.created_at_ms,
        })
    }

    fn list_pages(&self, document_id: DocumentId) -> Result<Vec<Page>, PortError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {PAGE_COLUMNS} FROM pages WHERE document_id = ?1 ORDER BY position"
            ))
            .map_err(|e| PortError::new("list pages", e))?;
        stmt.query_map(params![document_id.to_string()], page_from_row)
            .and_then(Iterator::collect)
            .map_err(|e| PortError::new("list pages", e))
    }

    fn get_page(&self, id: PageId) -> Result<Option<Page>, PortError> {
        self.conn()
            .query_row(
                &format!("SELECT {PAGE_COLUMNS} FROM pages WHERE id = ?1"),
                params![id.to_string()],
                page_from_row,
            )
            .optional()
            .map_err(|e| PortError::new("get page", e))
    }

    fn set_page_crop(
        &self,
        id: PageId,
        crop: &Crop,
        processed_path: &Path,
        updated_at_ms: i64,
    ) -> Result<Page, PortError> {
        let processed = path_to_db(processed_path)?;
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| PortError::new("set page crop", e))?;

        let changed = tx
            .execute(
                "UPDATE pages SET crop = ?2, processed_path = ?3 WHERE id = ?1",
                params![id.to_string(), crop_to_db(crop), processed],
            )
            .map_err(|e| PortError::new("set page crop", e))?;
        if changed != 1 {
            return Err(PortError::new(
                "set page crop",
                format!("page not found: {id}"),
            ));
        }
        tx.execute(
            "UPDATE documents SET updated_at_ms = MAX(updated_at_ms, ?2)
             WHERE id = (SELECT document_id FROM pages WHERE id = ?1)",
            params![id.to_string(), updated_at_ms],
        )
        .map_err(|e| PortError::new("set page crop", e))?;

        let page = tx
            .query_row(
                &format!("SELECT {PAGE_COLUMNS} FROM pages WHERE id = ?1"),
                params![id.to_string()],
                page_from_row,
            )
            .map_err(|e| PortError::new("set page crop", e))?;
        tx.commit()
            .map_err(|e| PortError::new("set page crop", e))?;
        Ok(page)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn doc(title: &str, updated: i64) -> Document {
        Document {
            id: DocumentId::new(),
            title: title.into(),
            created_at_ms: updated,
            updated_at_ms: updated,
            page_count: 0,
        }
    }

    fn new_page(document_id: DocumentId, at: i64) -> NewPage {
        let id = PageId::new();
        NewPage {
            id,
            document_id,
            original_path: PathBuf::from(format!(
                "documents/{document_id}/pages/{id}/original.jpg"
            )),
            created_at_ms: at,
        }
    }

    #[test]
    fn documents_round_trip_and_sort_by_recency() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let old = doc("old", 1);
        let new = doc("new", 2);
        repo.insert_document(&old).unwrap();
        repo.insert_document(&new).unwrap();

        let titles: Vec<_> = repo
            .list_documents()
            .unwrap()
            .into_iter()
            .map(|d| d.title)
            .collect();
        assert_eq!(titles, ["new", "old"]);
        assert_eq!(repo.get_document(old.id).unwrap(), Some(old));
        assert_eq!(repo.get_document(DocumentId::new()).unwrap(), None);
    }

    #[test]
    fn append_page_assigns_positions_and_touches_document() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 10);
        repo.insert_document(&d).unwrap();

        let p0 = repo.append_page(&new_page(d.id, 20)).unwrap();
        let p1 = repo.append_page(&new_page(d.id, 30)).unwrap();
        assert_eq!((p0.position, p1.position), (0, 1));

        let stored = repo.get_document(d.id).unwrap().unwrap();
        assert_eq!(stored.page_count, 2);
        assert_eq!(stored.updated_at_ms, 30);
        assert_eq!(repo.list_pages(d.id).unwrap(), vec![p0, p1]);
    }

    #[test]
    fn append_page_to_missing_document_fails() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        assert!(repo.append_page(&new_page(DocumentId::new(), 1)).is_err());
    }

    #[test]
    fn reopening_a_file_database_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite3");
        let d = doc("persisted", 1);
        SqliteDocumentRepository::open(&path)
            .unwrap()
            .insert_document(&d)
            .unwrap();

        let reopened = SqliteDocumentRepository::open(&path).unwrap();
        assert_eq!(reopened.get_document(d.id).unwrap(), Some(d));
    }

    #[test]
    fn crops_round_trip_and_touch_the_document() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 10);
        repo.insert_document(&d).unwrap();
        let page = repo.append_page(&new_page(d.id, 20)).unwrap();
        // Values that don't have short decimal representations.
        let crop =
            Crop::new([(0.1, 1.0 / 3.0), (0.9, 0.12345678), (0.87, 0.9), (0.0, 1.0)]).unwrap();

        let updated = repo
            .set_page_crop(page.id, &crop, Path::new("documents/x/processed-1.jpg"), 50)
            .unwrap();

        assert_eq!(updated.crop, Some(crop));
        assert_eq!(
            updated.processed_path,
            Some(PathBuf::from("documents/x/processed-1.jpg"))
        );
        assert_eq!(repo.get_page(page.id).unwrap(), Some(updated.clone()));
        assert_eq!(repo.list_pages(d.id).unwrap(), vec![updated]);
        assert_eq!(repo.get_document(d.id).unwrap().unwrap().updated_at_ms, 50);
    }

    #[test]
    fn set_crop_on_missing_page_fails() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let crop = Crop::new([(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]).unwrap();
        assert!(
            repo.set_page_crop(PageId::new(), &crop, Path::new("x.jpg"), 1)
                .is_err()
        );
        assert_eq!(repo.get_page(PageId::new()).unwrap(), None);
    }

    #[test]
    fn migrates_a_version_1_database_without_losing_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite3");
        let (doc_id, page_id) = (DocumentId::new(), PageId::new());
        {
            // A database exactly as v0.1 left it.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(super::super::migrations::V1_FOR_TESTS)
                .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute(
                "INSERT INTO documents VALUES (?1, 'old', 1, 1)",
                params![doc_id.to_string()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO pages VALUES (?1, ?2, 0, 'documents/a/original.jpg', 1)",
                params![page_id.to_string(), doc_id.to_string()],
            )
            .unwrap();
        }

        let repo = SqliteDocumentRepository::open(&path).unwrap();
        let page = repo.get_page(page_id).unwrap().unwrap();

        assert_eq!(
            page.original_path,
            PathBuf::from("documents/a/original.jpg")
        );
        assert_eq!((page.crop, page.processed_path), (None, None));
    }

    #[test]
    fn refuses_database_from_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", migrations::latest_version() + 1)
                .unwrap();
        }
        assert!(SqliteDocumentRepository::open(&path).is_err());
    }
}
