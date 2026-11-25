use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use document_engine::{
    Annotation, Crop, Document, DocumentId, DocumentRepository, Folder, FolderId, NewPage, Page,
    PageFilter, PageId, PageText, Point, PortError, Rotation, Signature, SignatureId, TextBounds,
    TextLine, TextMatch,
};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior, params,
};

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
     (SELECT COUNT(*) FROM pages p WHERE p.document_id = d.id), \
     (SELECT COALESCE(p.processed_path, p.original_path) FROM pages p \
      WHERE p.document_id = d.id ORDER BY p.position LIMIT 1), \
     d.trashed_at_ms, d.folder_id";

fn document_from_row(row: &Row<'_>) -> rusqlite::Result<Document> {
    Ok(Document {
        id: parse_id(row, 0)?,
        title: row.get(1)?,
        created_at_ms: row.get(2)?,
        updated_at_ms: row.get(3)?,
        page_count: row.get(4)?,
        cover_path: row.get::<_, Option<String>>(5)?.map(PathBuf::from),
        trashed_at_ms: row.get(6)?,
        folder_id: row
            .get::<_, Option<String>>(7)?
            .map(|id| {
                id.parse().map_err(|e: document_engine::DocumentError| {
                    rusqlite::Error::FromSqlConversionFailure(
                        7,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })
            })
            .transpose()?,
        // Sizes come from the files, not the database.
        size_bytes: 0,
    })
}

/// Turns user input into an FTS5 query: every word must match, each as a
/// prefix ("fact" finds "factura"). Words are quoted, so operators and
/// punctuation typed by the user are taken literally, never as syntax.
/// `None` when the input has no searchable characters.
fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

const FOLDER_COLUMNS: &str = "f.id, f.name, f.created_at_ms, f.updated_at_ms, \
     (SELECT COUNT(*) FROM documents d WHERE d.folder_id = f.id AND d.trashed_at_ms IS NULL)";

fn folder_from_row(row: &Row<'_>) -> rusqlite::Result<Folder> {
    Ok(Folder {
        id: parse_id(row, 0)?,
        name: row.get(1)?,
        created_at_ms: row.get(2)?,
        updated_at_ms: row.get(3)?,
        document_count: row.get(4)?,
    })
}

const PAGE_COLUMNS: &str = "id, document_id, position, original_path, created_at_ms, crop, processed_path, filter, rotation";

fn page_from_row(row: &Row<'_>) -> rusqlite::Result<Page> {
    let original: String = row.get(3)?;
    let crop: Option<String> = row.get(5)?;
    let processed: Option<String> = row.get(6)?;
    let filter: String = row.get(7)?;
    Ok(Page {
        id: parse_id::<PageId>(row, 0)?,
        document_id: parse_id(row, 1)?,
        position: row.get(2)?,
        original_path: PathBuf::from(original),
        crop: crop.map(|c| crop_from_db(&c)).transpose().map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, e.into())
        })?,
        filter: filter_from_db(&filter).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, e.into())
        })?,
        rotation: Rotation::from_degrees(row.get(8)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Integer, e.into())
        })?,
        processed_path: processed.map(PathBuf::from),
        created_at_ms: row.get(4)?,
    })
}

fn page_document(tx: &Transaction<'_>, id: PageId) -> rusqlite::Result<Option<String>> {
    tx.query_row(
        "SELECT document_id FROM pages WHERE id = ?1",
        params![id.to_string()],
        |r| r.get(0),
    )
    .optional()
}

/// Page ids of a document in their current order.
fn page_order(tx: &Transaction<'_>, document_id: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = tx.prepare("SELECT id FROM pages WHERE document_id = ?1 ORDER BY position")?;
    stmt.query_map(params![document_id], |r| r.get(0))?
        .collect()
}

/// Assigns positions `0..n` following `order`.
///
/// `UNIQUE(document_id, position)` is checked row by row, so shifting pages
/// one slot at a time would collide halfway. Instead, every page first moves
/// past the current maximum (a range nothing occupies), then each takes its
/// final slot, which is free by then.
fn renumber(tx: &Transaction<'_>, document_id: &str, order: &[String]) -> rusqlite::Result<()> {
    let offset: i64 = tx.query_row(
        "SELECT COALESCE(MAX(position), 0) + 1 FROM pages WHERE document_id = ?1",
        params![document_id],
        |r| r.get(0),
    )?;
    tx.execute(
        "UPDATE pages SET position = position + ?2 WHERE document_id = ?1",
        params![document_id, offset],
    )?;
    let mut stmt = tx.prepare("UPDATE pages SET position = ?2 WHERE id = ?1")?;
    for (position, id) in order.iter().enumerate() {
        stmt.execute(params![id, position as i64])?;
    }
    Ok(())
}

/// An annotation as stored: see migration 8.
struct AnnotationRow<'a> {
    kind: &'static str,
    color: u32,
    size: f32,
    geometry: String,
    text: Option<&'a str>,
}

impl<'a> From<&'a Annotation> for AnnotationRow<'a> {
    fn from(annotation: &'a Annotation) -> Self {
        match annotation {
            Annotation::Ink {
                strokes,
                color,
                width,
            } => Self {
                kind: "ink",
                color: *color,
                size: *width,
                geometry: strokes_to_db(strokes),
                text: None,
            },
            Annotation::Highlight {
                left,
                top,
                right,
                bottom,
                color,
            } => Self {
                kind: "highlight",
                color: *color,
                size: 0.0,
                geometry: format!("{left},{top},{right},{bottom}"),
                text: None,
            },
            Annotation::Text {
                text,
                x,
                y,
                size,
                color,
            } => Self {
                kind: "text",
                color: *color,
                size: *size,
                geometry: format!("{x},{y}"),
                text: Some(text),
            },
        }
    }
}

fn annotation_from_row(row: &Row<'_>) -> rusqlite::Result<Annotation> {
    let kind: String = row.get(0)?;
    let color: u32 = row.get(1)?;
    let size: f32 = row.get(2)?;
    let geometry: String = row.get(3)?;
    let numbers = |expected: usize| -> rusqlite::Result<Vec<f32>> {
        let values = floats(&geometry).map_err(|e| conversion_error(3, e))?;
        if values.len() == expected {
            Ok(values)
        } else {
            Err(conversion_error(
                3,
                format!("expected {expected} numbers: {geometry}"),
            ))
        }
    };
    Ok(match kind.as_str() {
        "ink" => Annotation::Ink {
            strokes: strokes_from_db(&geometry).map_err(|e| conversion_error(3, e))?,
            color,
            width: size,
        },
        "highlight" => {
            let v = numbers(4)?;
            Annotation::Highlight {
                left: v[0],
                top: v[1],
                right: v[2],
                bottom: v[3],
                color,
            }
        }
        "text" => {
            let v = numbers(2)?;
            Annotation::Text {
                text: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                x: v[0],
                y: v[1],
                size,
                color,
            }
        }
        other => {
            return Err(conversion_error(
                0,
                format!("unknown annotation kind: {other}"),
            ));
        }
    })
}

/// Strokes as "x,y x,y;x,y". Rust's float `Display` round-trips exactly.
fn strokes_to_db(strokes: &[Vec<Point>]) -> String {
    strokes
        .iter()
        .map(|stroke| {
            stroke
                .iter()
                .map(|p| format!("{},{}", p.x, p.y))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn strokes_from_db(value: &str) -> Result<Vec<Vec<Point>>, String> {
    value
        .split(';')
        .map(|stroke| {
            stroke
                .split(' ')
                .map(|pair| {
                    let v = floats(pair)?;
                    match v.as_slice() {
                        [x, y] => Ok(Point::new(*x, *y)),
                        _ => Err(format!("bad point: {pair}")),
                    }
                })
                .collect()
        })
        .collect()
}

fn floats(value: &str) -> Result<Vec<f32>, String> {
    value
        .split(',')
        .map(|v| v.parse::<f32>().map_err(|e| format!("{v}: {e}")))
        .collect()
}

fn conversion_error(idx: usize, message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        rusqlite::types::Type::Text,
        message.into().into(),
    )
}

fn touch_document(
    tx: &Transaction<'_>,
    document_id: &str,
    updated_at_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE documents SET updated_at_ms = MAX(updated_at_ms, ?2) WHERE id = ?1",
        params![document_id, updated_at_ms],
    )?;
    Ok(())
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

fn filter_to_db(filter: PageFilter) -> &'static str {
    match filter {
        PageFilter::Original => "original",
        PageFilter::Grayscale => "grayscale",
        PageFilter::BlackAndWhite => "black_and_white",
        PageFilter::Enhanced => "enhanced",
    }
}

fn filter_from_db(value: &str) -> Result<PageFilter, String> {
    match value {
        "original" => Ok(PageFilter::Original),
        "grayscale" => Ok(PageFilter::Grayscale),
        "black_and_white" => Ok(PageFilter::BlackAndWhite),
        "enhanced" => Ok(PageFilter::Enhanced),
        other => Err(format!("unknown page filter: {other}")),
    }
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
                "INSERT INTO documents (id, title, created_at_ms, updated_at_ms, folder_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    d.id.to_string(),
                    d.title,
                    d.created_at_ms,
                    d.updated_at_ms,
                    d.folder_id.map(|f| f.to_string())
                ],
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
                 WHERE d.trashed_at_ms IS NULL
                 ORDER BY d.updated_at_ms DESC, d.id"
            ))
            .map_err(|e| PortError::new("list documents", e))?;
        stmt.query_map([], document_from_row)
            .and_then(Iterator::collect)
            .map_err(|e| PortError::new("list documents", e))
    }

    fn list_trashed_documents(&self) -> Result<Vec<Document>, PortError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {DOCUMENT_COLUMNS} FROM documents d
                 WHERE d.trashed_at_ms IS NOT NULL
                 ORDER BY d.trashed_at_ms DESC, d.id"
            ))
            .map_err(|e| PortError::new("list trashed documents", e))?;
        stmt.query_map([], document_from_row)
            .and_then(Iterator::collect)
            .map_err(|e| PortError::new("list trashed documents", e))
    }

    fn set_document_trashed(
        &self,
        id: DocumentId,
        trashed_at_ms: Option<i64>,
    ) -> Result<bool, PortError> {
        // Not an edit of the document: updated_at_ms stays, so a restored
        // document goes back to its place in the list.
        let changed = self
            .conn()
            .execute(
                "UPDATE documents SET trashed_at_ms = ?2 WHERE id = ?1",
                params![id.to_string(), trashed_at_ms],
            )
            .map_err(|e| PortError::new("trash document", e))?;
        Ok(changed > 0)
    }

    fn files_by_document(&self) -> Result<Vec<(DocumentId, PathBuf)>, PortError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(
                "SELECT document_id, original_path FROM pages
                 UNION ALL
                 SELECT document_id, processed_path FROM pages WHERE processed_path IS NOT NULL",
            )
            .map_err(|e| PortError::new("list document files", e))?;
        stmt.query_map([], |row| {
            Ok((parse_id(row, 0)?, PathBuf::from(row.get::<_, String>(1)?)))
        })
        .and_then(Iterator::collect)
        .map_err(|e| PortError::new("list document files", e))
    }

    fn insert_folder(&self, f: &Folder) -> Result<(), PortError> {
        self.conn()
            .execute(
                "INSERT INTO folders (id, name, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4)",
                params![f.id.to_string(), f.name, f.created_at_ms, f.updated_at_ms],
            )
            .map_err(|e| PortError::new("insert folder", e))?;
        Ok(())
    }

    fn get_folder(&self, id: FolderId) -> Result<Option<Folder>, PortError> {
        self.conn()
            .query_row(
                &format!("SELECT {FOLDER_COLUMNS} FROM folders f WHERE f.id = ?1"),
                params![id.to_string()],
                folder_from_row,
            )
            .optional()
            .map_err(|e| PortError::new("get folder", e))
    }

    fn list_folders(&self) -> Result<Vec<Folder>, PortError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {FOLDER_COLUMNS} FROM folders f ORDER BY f.name COLLATE NOCASE, f.id"
            ))
            .map_err(|e| PortError::new("list folders", e))?;
        stmt.query_map([], folder_from_row)
            .and_then(Iterator::collect)
            .map_err(|e| PortError::new("list folders", e))
    }

    fn rename_folder(
        &self,
        id: FolderId,
        name: &str,
        updated_at_ms: i64,
    ) -> Result<Option<Folder>, PortError> {
        let changed = self
            .conn()
            .execute(
                "UPDATE folders SET name = ?2, updated_at_ms = ?3 WHERE id = ?1",
                params![id.to_string(), name, updated_at_ms],
            )
            .map_err(|e| PortError::new("rename folder", e))?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_folder(id)
    }

    fn delete_folder(&self, id: FolderId) -> Result<bool, PortError> {
        // documents.folder_id is ON DELETE SET NULL: they move to the top level.
        let changed = self
            .conn()
            .execute("DELETE FROM folders WHERE id = ?1", params![id.to_string()])
            .map_err(|e| PortError::new("delete folder", e))?;
        Ok(changed > 0)
    }

    fn move_documents(
        &self,
        documents: &[DocumentId],
        folder: Option<FolderId>,
    ) -> Result<u32, PortError> {
        let ctx = "move documents";
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| PortError::new(ctx, e))?;
        let folder = folder.map(|f| f.to_string());
        let mut moved = 0;
        {
            let mut update = tx
                .prepare("UPDATE documents SET folder_id = ?2 WHERE id = ?1")
                .map_err(|e| PortError::new(ctx, e))?;
            for id in documents {
                moved += update
                    .execute(params![id.to_string(), folder])
                    .map_err(|e| PortError::new(ctx, e))? as u32;
            }
        }
        tx.commit().map_err(|e| PortError::new(ctx, e))?;
        Ok(moved)
    }

    fn trashed_before(&self, cutoff_ms: i64) -> Result<Vec<DocumentId>, PortError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached("SELECT id FROM documents WHERE trashed_at_ms < ?1")
            .map_err(|e| PortError::new("list expired trash", e))?;
        stmt.query_map(params![cutoff_ms], |row| parse_id(row, 0))
            .and_then(Iterator::collect)
            .map_err(|e| PortError::new("list expired trash", e))
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
            filter: PageFilter::Original,
            rotation: Rotation::None,
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

    fn rename_document(
        &self,
        id: DocumentId,
        title: &str,
        updated_at_ms: i64,
    ) -> Result<Option<Document>, PortError> {
        let changed = self
            .conn()
            .execute(
                "UPDATE documents SET title = ?2, updated_at_ms = MAX(updated_at_ms, ?3) WHERE id = ?1",
                params![id.to_string(), title, updated_at_ms],
            )
            .map_err(|e| PortError::new("rename document", e))?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_document(id)
    }

    fn delete_document(&self, id: DocumentId) -> Result<bool, PortError> {
        // Pages go with it: pages.document_id is ON DELETE CASCADE.
        let changed = self
            .conn()
            .execute(
                "DELETE FROM documents WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(|e| PortError::new("delete document", e))?;
        Ok(changed > 0)
    }

    fn delete_page(&self, id: PageId, updated_at_ms: i64) -> Result<bool, PortError> {
        let err = |e| PortError::new("delete page", e);
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(err)?;
        let Some(document_id) = page_document(&tx, id).map_err(err)? else {
            return Ok(false);
        };
        tx.execute("DELETE FROM pages WHERE id = ?1", params![id.to_string()])
            .map_err(err)?;
        let order = page_order(&tx, &document_id).map_err(err)?;
        renumber(&tx, &document_id, &order).map_err(err)?;
        touch_document(&tx, &document_id, updated_at_ms).map_err(err)?;
        tx.commit().map_err(err)?;
        Ok(true)
    }

    fn move_page(
        &self,
        id: PageId,
        to_position: u32,
        updated_at_ms: i64,
    ) -> Result<Vec<Page>, PortError> {
        let err = |e| PortError::new("move page", e);
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(err)?;
        let document_id = page_document(&tx, id)
            .map_err(err)?
            .ok_or_else(|| PortError::new("move page", format!("page not found: {id}")))?;

        let mut order = page_order(&tx, &document_id).map_err(err)?;
        let page = id.to_string();
        if let Some(from) = order.iter().position(|p| *p == page) {
            let moved = order.remove(from);
            let to = (to_position as usize).min(order.len());
            order.insert(to, moved);
        }
        renumber(&tx, &document_id, &order).map_err(err)?;
        touch_document(&tx, &document_id, updated_at_ms).map_err(err)?;

        let pages = {
            let mut stmt = tx
                .prepare(&format!(
                    "SELECT {PAGE_COLUMNS} FROM pages WHERE document_id = ?1 ORDER BY position"
                ))
                .map_err(err)?;
            stmt.query_map(params![document_id], page_from_row)
                .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
                .map_err(err)?
        };
        tx.commit().map_err(err)?;
        Ok(pages)
    }

    fn set_page_rendering(
        &self,
        id: PageId,
        crop: Option<&Crop>,
        filter: PageFilter,
        rotation: Rotation,
        processed_path: Option<&Path>,
        updated_at_ms: i64,
    ) -> Result<Page, PortError> {
        let processed = processed_path.map(path_to_db).transpose()?;
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| PortError::new("set page rendering", e))?;

        let changed = tx
            .execute(
                "UPDATE pages SET crop = ?2, processed_path = ?3, filter = ?4, rotation = ?5
                 WHERE id = ?1",
                params![
                    id.to_string(),
                    crop.map(crop_to_db),
                    processed,
                    filter_to_db(filter),
                    rotation.degrees()
                ],
            )
            .map_err(|e| PortError::new("set page rendering", e))?;
        if changed != 1 {
            return Err(PortError::new(
                "set page rendering",
                format!("page not found: {id}"),
            ));
        }
        tx.execute(
            "UPDATE documents SET updated_at_ms = MAX(updated_at_ms, ?2)
             WHERE id = (SELECT document_id FROM pages WHERE id = ?1)",
            params![id.to_string(), updated_at_ms],
        )
        .map_err(|e| PortError::new("set page rendering", e))?;

        let page = tx
            .query_row(
                &format!("SELECT {PAGE_COLUMNS} FROM pages WHERE id = ?1"),
                params![id.to_string()],
                page_from_row,
            )
            .map_err(|e| PortError::new("set page rendering", e))?;
        tx.commit()
            .map_err(|e| PortError::new("set page rendering", e))?;
        Ok(page)
    }

    fn referenced_files(&self) -> Result<Vec<PathBuf>, PortError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT original_path FROM pages
                 UNION ALL
                 SELECT processed_path FROM pages WHERE processed_path IS NOT NULL",
            )
            .map_err(|e| PortError::new("list referenced files", e))?;
        stmt.query_map([], |row| row.get::<_, String>(0).map(PathBuf::from))
            .and_then(Iterator::collect)
            .map_err(|e| PortError::new("list referenced files", e))
    }

    fn save_page_text(&self, text: &PageText, plain_text: &str) -> Result<(), PortError> {
        let ctx = "save page text";
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| PortError::new(ctx, e))?;
        let page_id = text.page_id.to_string();
        // Delete then insert (not REPLACE): the delete trigger then removes
        // the old entry from the search index.
        tx.execute(
            "DELETE FROM page_texts WHERE page_id = ?1",
            params![page_id],
        )
        .map_err(|e| PortError::new(ctx, e))?;
        tx.execute(
            "INSERT INTO page_texts (page_id, source_path, text, recognized_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                page_id,
                text.source_path.to_string_lossy(),
                plain_text,
                text.recognized_at_ms
            ],
        )
        .map_err(|e| PortError::new(ctx, e))?;
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO page_text_lines
                     (page_id, line_index, block_index, text, left, top, right, bottom)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .map_err(|e| PortError::new(ctx, e))?;
            for (index, line) in text.lines.iter().enumerate() {
                let b = line.bounds;
                insert
                    .execute(params![
                        page_id,
                        index as i64,
                        line.block,
                        line.text,
                        b.left,
                        b.top,
                        b.right,
                        b.bottom
                    ])
                    .map_err(|e| PortError::new(ctx, e))?;
            }
        }
        tx.commit().map_err(|e| PortError::new(ctx, e))
    }

    fn get_page_text(&self, page_id: PageId) -> Result<Option<PageText>, PortError> {
        let ctx = "get page text";
        let conn = self.conn();
        let id = page_id.to_string();
        let Some((source_path, recognized_at_ms)) = conn
            .query_row(
                "SELECT source_path, recognized_at_ms FROM page_texts WHERE page_id = ?1",
                params![id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|e| PortError::new(ctx, e))?
        else {
            return Ok(None);
        };
        let mut stmt = conn
            .prepare_cached(
                "SELECT block_index, text, left, top, right, bottom FROM page_text_lines
                 WHERE page_id = ?1 ORDER BY line_index",
            )
            .map_err(|e| PortError::new(ctx, e))?;
        let lines = stmt
            .query_map(params![id], |row| {
                let bounds = TextBounds::new(row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)
                    .map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Real,
                            e.into(),
                        )
                    })?;
                Ok(TextLine {
                    block: row.get(0)?,
                    text: row.get(1)?,
                    bounds,
                })
            })
            .and_then(Iterator::collect)
            .map_err(|e| PortError::new(ctx, e))?;
        Ok(Some(PageText {
            page_id,
            source_path: PathBuf::from(source_path),
            lines,
            recognized_at_ms,
        }))
    }

    fn pages_without_text(&self, limit: u32) -> Result<Vec<Page>, PortError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {PAGE_COLUMNS} FROM pages
                 WHERE NOT EXISTS (
                     SELECT 1 FROM page_texts t
                     WHERE t.page_id = pages.id
                       AND t.source_path = COALESCE(pages.processed_path, pages.original_path))
                   AND NOT EXISTS (
                     SELECT 1 FROM documents d
                     WHERE d.id = pages.document_id AND d.trashed_at_ms IS NOT NULL)
                 ORDER BY created_at_ms DESC, id
                 LIMIT ?1"
            ))
            .map_err(|e| PortError::new("list pages without text", e))?;
        stmt.query_map(params![limit], page_from_row)
            .and_then(Iterator::collect)
            .map_err(|e| PortError::new("list pages without text", e))
    }

    fn search_text(&self, query: &str, limit: u32) -> Result<Vec<TextMatch>, PortError> {
        let Some(fts) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let conn = self.conn();
        // Only current text counts: a page re-cropped since recognition
        // waits for its new text before it is found again.
        let mut stmt = conn
            .prepare_cached(
                "SELECT p.document_id, p.id, p.position,
                        snippet(page_text_search, 0, '', '', '…', 12)
                 FROM page_text_search
                 JOIN page_texts t ON t.rowid = page_text_search.rowid
                 JOIN pages p ON p.id = t.page_id
                 JOIN documents d ON d.id = p.document_id
                 WHERE page_text_search MATCH ?1
                   AND t.source_path = COALESCE(p.processed_path, p.original_path)
                   AND d.trashed_at_ms IS NULL
                 ORDER BY rank
                 LIMIT ?2",
            )
            .map_err(|e| PortError::new("search text", e))?;
        stmt.query_map(params![fts, limit], |row| {
            Ok(TextMatch {
                document_id: parse_id(row, 0)?,
                page_id: parse_id(row, 1)?,
                page_position: row.get(2)?,
                snippet: row.get(3)?,
            })
        })
        .and_then(Iterator::collect)
        .map_err(|e| PortError::new("search text", e))
    }

    fn get_annotations(&self, page_id: PageId) -> Result<Vec<Annotation>, PortError> {
        let ctx = "get annotations";
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT kind, color, size, geometry, text FROM page_annotations
                 WHERE page_id = ?1 ORDER BY position",
            )
            .map_err(|e| PortError::new(ctx, e))?;
        stmt.query_map(params![page_id.to_string()], annotation_from_row)
            .and_then(Iterator::collect)
            .map_err(|e| PortError::new(ctx, e))
    }

    fn set_annotations(
        &self,
        page_id: PageId,
        annotations: &[Annotation],
        updated_at_ms: i64,
    ) -> Result<(), PortError> {
        let ctx = "set annotations";
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| PortError::new(ctx, e))?;
        let document_id = page_document(&tx, page_id)
            .map_err(|e| PortError::new(ctx, e))?
            .ok_or_else(|| PortError::new(ctx, format!("page not found: {page_id}")))?;
        let id = page_id.to_string();
        tx.execute(
            "DELETE FROM page_annotations WHERE page_id = ?1",
            params![id],
        )
        .map_err(|e| PortError::new(ctx, e))?;
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO page_annotations (page_id, position, kind, color, size, geometry, text)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(|e| PortError::new(ctx, e))?;
            for (position, annotation) in annotations.iter().enumerate() {
                let row = AnnotationRow::from(annotation);
                insert
                    .execute(params![
                        id,
                        position as i64,
                        row.kind,
                        row.color,
                        row.size,
                        row.geometry,
                        row.text
                    ])
                    .map_err(|e| PortError::new(ctx, e))?;
            }
        }
        touch_document(&tx, &document_id, updated_at_ms).map_err(|e| PortError::new(ctx, e))?;
        tx.commit().map_err(|e| PortError::new(ctx, e))
    }

    fn insert_signature(&self, signature: &Signature) -> Result<(), PortError> {
        self.conn()
            .execute(
                "INSERT INTO signatures (id, strokes, aspect_ratio, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    signature.id.to_string(),
                    strokes_to_db(&signature.strokes),
                    signature.aspect_ratio,
                    signature.created_at_ms
                ],
            )
            .map(|_| ())
            .map_err(|e| PortError::new("insert signature", e))
    }

    fn list_signatures(&self) -> Result<Vec<Signature>, PortError> {
        let ctx = "list signatures";
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, strokes, aspect_ratio, created_at_ms FROM signatures
                 ORDER BY created_at_ms, id",
            )
            .map_err(|e| PortError::new(ctx, e))?;
        stmt.query_map([], |row| {
            let strokes: String = row.get(1)?;
            Ok(Signature {
                id: parse_id(row, 0)?,
                strokes: strokes_from_db(&strokes).map_err(|e| conversion_error(1, e))?,
                aspect_ratio: row.get(2)?,
                created_at_ms: row.get(3)?,
            })
        })
        .and_then(Iterator::collect)
        .map_err(|e| PortError::new(ctx, e))
    }

    fn delete_signature(&self, id: SignatureId) -> Result<bool, PortError> {
        self.conn()
            .execute(
                "DELETE FROM signatures WHERE id = ?1",
                params![id.to_string()],
            )
            .map(|n| n > 0)
            .map_err(|e| PortError::new("delete signature", e))
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
            cover_path: None,
            trashed_at_ms: None,
            folder_id: None,
            size_bytes: 0,
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
            .set_page_rendering(
                page.id,
                Some(&crop),
                PageFilter::Enhanced,
                Rotation::None,
                Some(Path::new("documents/x/processed-1.jpg")),
                50,
            )
            .unwrap();

        assert_eq!(updated.crop, Some(crop));
        assert_eq!(updated.filter, PageFilter::Enhanced);
        assert_eq!(
            updated.processed_path,
            Some(PathBuf::from("documents/x/processed-1.jpg"))
        );
        assert_eq!(repo.get_page(page.id).unwrap(), Some(updated.clone()));
        assert_eq!(repo.list_pages(d.id).unwrap(), vec![updated]);
        assert_eq!(repo.get_document(d.id).unwrap().unwrap().updated_at_ms, 50);

        // Back to the untouched original.
        let reset = repo
            .set_page_rendering(
                page.id,
                None,
                PageFilter::Original,
                Rotation::None,
                None,
                60,
            )
            .unwrap();
        assert_eq!(
            (reset.crop, reset.filter, reset.processed_path),
            (None, PageFilter::Original, None)
        );
    }

    #[test]
    fn every_filter_round_trips() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 1);
        repo.insert_document(&d).unwrap();
        let page = repo.append_page(&new_page(d.id, 2)).unwrap();
        assert_eq!(page.filter, PageFilter::Original);
        for filter in [
            PageFilter::Grayscale,
            PageFilter::BlackAndWhite,
            PageFilter::Enhanced,
            PageFilter::Original,
        ] {
            repo.set_page_rendering(
                page.id,
                None,
                filter,
                Rotation::None,
                Some(Path::new("p.jpg")),
                3,
            )
            .unwrap();
            assert_eq!(repo.get_page(page.id).unwrap().unwrap().filter, filter);
        }
    }

    #[test]
    fn documents_carry_their_first_page_as_cover() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 1);
        repo.insert_document(&d).unwrap();
        assert_eq!(repo.get_document(d.id).unwrap().unwrap().cover_path, None);

        let first = repo.append_page(&new_page(d.id, 2)).unwrap();
        let second = repo.append_page(&new_page(d.id, 3)).unwrap();
        let cover = || repo.list_documents().unwrap()[0].cover_path.clone();
        assert_eq!(cover(), Some(first.original_path.clone()));

        // The processed image wins, and reordering changes the cover.
        repo.set_page_rendering(
            first.id,
            None,
            PageFilter::Grayscale,
            Rotation::None,
            Some(Path::new("p.jpg")),
            4,
        )
        .unwrap();
        assert_eq!(cover(), Some(PathBuf::from("p.jpg")));
        repo.move_page(second.id, 0, 5).unwrap();
        assert_eq!(cover(), Some(second.original_path));
    }

    fn page_text(page: &Page, source: &str, lines: &[(u32, &str)]) -> PageText {
        PageText {
            page_id: page.id,
            source_path: PathBuf::from(source),
            lines: lines
                .iter()
                .map(|&(block, text)| TextLine {
                    block,
                    text: text.into(),
                    bounds: TextBounds::new(0.1, 0.2, 0.8, 0.25).unwrap(),
                })
                .collect(),
            recognized_at_ms: 9,
        }
    }

    fn save(repo: &SqliteDocumentRepository, text: &PageText) {
        repo.save_page_text(text, &text.plain_text()).unwrap();
    }

    fn search(repo: &SqliteDocumentRepository, query: &str) -> Vec<PageId> {
        repo.search_text(query, 50)
            .unwrap()
            .into_iter()
            .map(|m| m.page_id)
            .collect()
    }

    #[test]
    fn page_text_round_trips_and_replaces() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 1);
        repo.insert_document(&d).unwrap();
        let page = repo.append_page(&new_page(d.id, 2)).unwrap();
        assert_eq!(repo.get_page_text(page.id).unwrap(), None);

        let first = page_text(
            &page,
            "a.jpg",
            &[(0, "Factura"), (0, "Nº 12"), (1, "Total")],
        );
        save(&repo, &first);
        assert_eq!(repo.get_page_text(page.id).unwrap(), Some(first));

        let second = page_text(&page, "b.jpg", &[(0, "Boleta")]);
        save(&repo, &second);
        assert_eq!(repo.get_page_text(page.id).unwrap(), Some(second));
    }

    #[test]
    fn pages_without_text_tracks_the_displayed_image() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 1);
        repo.insert_document(&d).unwrap();
        let older = repo.append_page(&new_page(d.id, 2)).unwrap();
        let newer = repo.append_page(&new_page(d.id, 3)).unwrap();
        let pending = |limit| ids(&repo.pages_without_text(limit).unwrap());
        assert_eq!(pending(10), vec![newer.id, older.id], "newest first");
        assert_eq!(pending(1), vec![newer.id]);

        let original = older.original_path.to_string_lossy().into_owned();
        save(&repo, &page_text(&older, &original, &[(0, "hola")]));
        assert_eq!(pending(10), vec![newer.id]);

        // A new processed image makes the stored text outdated.
        repo.set_page_rendering(
            older.id,
            None,
            PageFilter::Grayscale,
            Rotation::None,
            Some(Path::new("g.jpg")),
            4,
        )
        .unwrap();
        assert_eq!(pending(10), vec![newer.id, older.id]);
    }

    #[test]
    fn search_ignores_case_and_accents_and_matches_prefixes() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 1);
        repo.insert_document(&d).unwrap();
        let invoice = repo.append_page(&new_page(d.id, 2)).unwrap();
        let receipt = repo.append_page(&new_page(d.id, 3)).unwrap();
        let path = |p: &Page| p.original_path.to_string_lossy().into_owned();
        save(
            &repo,
            &page_text(
                &invoice,
                &path(&invoice),
                &[(0, "FACTURA ELECTRÓNICA"), (1, "Dirección: Av. Perú 123")],
            ),
        );
        save(
            &repo,
            &page_text(
                &receipt,
                &path(&receipt),
                &[(0, "Boleta de venta"), (0, "Peru")],
            ),
        );

        assert_eq!(search(&repo, "electronica"), vec![invoice.id]);
        assert_eq!(
            search(&repo, "fact direc"),
            vec![invoice.id],
            "prefixes, all words"
        );
        assert_eq!(search(&repo, "boleta factura"), Vec::<PageId>::new());
        let mut peru = search(&repo, "perú");
        peru.sort_by_key(|id| id.to_string());
        let mut both = vec![invoice.id, receipt.id];
        both.sort_by_key(|id| id.to_string());
        assert_eq!(peru, both);

        let hit = &repo.search_text("boleta", 50).unwrap()[0];
        assert_eq!((hit.document_id, hit.page_position), (d.id, 1));
        assert!(hit.snippet.contains("Boleta"), "{}", hit.snippet);
    }

    #[test]
    fn search_input_is_never_parsed_as_syntax() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 1);
        repo.insert_document(&d).unwrap();
        let page = repo.append_page(&new_page(d.id, 2)).unwrap();
        let path = page.original_path.to_string_lossy().into_owned();
        save(
            &repo,
            &page_text(&page, &path, &[(0, "Total \"neto\" OR NOT 3/2026")]),
        );

        for query in [
            "\"", "*", "-", "NOT", "neto\"", "AND OR", "(", "3/2026", "text:x",
        ] {
            repo.search_text(query, 50)
                .unwrap_or_else(|e| panic!("{query:?}: {e}"));
        }
        assert_eq!(search(&repo, "\"neto"), vec![page.id]);
        assert_eq!(search(&repo, "3/2026"), vec![page.id]);
        assert!(search(&repo, "  -- ").is_empty());
    }

    #[test]
    fn search_skips_outdated_text_and_deleted_pages() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 1);
        repo.insert_document(&d).unwrap();
        let page = repo.append_page(&new_page(d.id, 2)).unwrap();
        let path = page.original_path.to_string_lossy().into_owned();
        save(&repo, &page_text(&page, &path, &[(0, "contrato")]));
        assert_eq!(search(&repo, "contrato"), vec![page.id]);

        repo.set_page_rendering(
            page.id,
            None,
            PageFilter::Grayscale,
            Rotation::None,
            Some(Path::new("g.jpg")),
            3,
        )
        .unwrap();
        assert!(search(&repo, "contrato").is_empty(), "outdated");
        save(&repo, &page_text(&page, "g.jpg", &[(0, "contrato")]));
        assert_eq!(search(&repo, "contrato"), vec![page.id]);

        repo.delete_document(d.id).unwrap();
        assert!(search(&repo, "contrato").is_empty());
        let conn = repo.conn();
        let indexed: i64 = conn
            .query_row("SELECT COUNT(*) FROM page_text_search", [], |r| r.get(0))
            .unwrap();
        let lines: i64 = conn
            .query_row("SELECT COUNT(*) FROM page_text_lines", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            (indexed, lines),
            (0, 0),
            "cascades clear the index and lines"
        );
    }

    #[test]
    fn fts_queries_quote_every_word() {
        assert_eq!(
            fts_query("  hola  mundo "),
            Some("\"hola\"* \"mundo\"*".into())
        );
        assert_eq!(fts_query("di\"jo"), Some("\"di\"\"jo\"*".into()));
        assert_eq!(fts_query(" - * "), None);
    }

    fn folder(name: &str) -> Folder {
        Folder {
            id: FolderId::new(),
            name: name.into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            document_count: 0,
        }
    }

    #[test]
    fn annotations_round_trip_in_order_and_go_with_their_page() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 1);
        repo.insert_document(&d).unwrap();
        let page = repo.append_page(&new_page(d.id, 2)).unwrap();
        let annotations = vec![
            Annotation::Highlight {
                left: 0.1,
                top: 1.0 / 3.0,
                right: 0.9,
                bottom: 0.4,
                color: 0xFF_EB_3B,
            },
            Annotation::Ink {
                strokes: vec![
                    vec![Point::new(0.123_456_79, 0.2), Point::new(0.3, 0.4)],
                    vec![Point::new(0.5, 0.6)],
                ],
                color: 0x1A_4D_8F,
                width: 0.004,
            },
            Annotation::Text {
                text: "Recibido, 3/2026;\nfirma: ñandú".into(),
                x: 0.05,
                y: 0.9,
                size: 0.03,
                color: 0,
            },
        ];

        repo.set_annotations(page.id, &annotations, 7).unwrap();

        assert_eq!(repo.get_annotations(page.id).unwrap(), annotations);
        assert_eq!(repo.get_document(d.id).unwrap().unwrap().updated_at_ms, 7);
        // Replacing keeps only the new set.
        repo.set_annotations(page.id, &annotations[1..2], 8)
            .unwrap();
        assert_eq!(repo.get_annotations(page.id).unwrap(), &annotations[1..2]);
        assert!(
            repo.set_annotations(PageId::new(), &annotations, 9)
                .is_err()
        );

        repo.delete_page(page.id, 10).unwrap();
        assert!(repo.get_annotations(page.id).unwrap().is_empty());
    }

    #[test]
    fn signatures_round_trip_oldest_first() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let signature = |at: i64| Signature {
            id: SignatureId::new(),
            strokes: vec![vec![Point::new(0.0, 0.5), Point::new(1.0, 0.25)]],
            aspect_ratio: 3.2,
            created_at_ms: at,
        };
        let (newer, older) = (signature(20), signature(10));
        repo.insert_signature(&newer).unwrap();
        repo.insert_signature(&older).unwrap();

        assert_eq!(repo.list_signatures().unwrap(), vec![older.clone(), newer]);
        assert!(repo.delete_signature(older.id).unwrap());
        assert!(!repo.delete_signature(older.id).unwrap());
        assert_eq!(repo.list_signatures().unwrap().len(), 1);
    }

    #[test]
    fn rotation_round_trips_and_existing_pages_start_upright() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 1);
        repo.insert_document(&d).unwrap();
        let page = repo.append_page(&new_page(d.id, 2)).unwrap();
        assert_eq!(
            repo.get_page(page.id).unwrap().unwrap().rotation,
            Rotation::None
        );

        let turned = repo
            .set_page_rendering(
                page.id,
                None,
                PageFilter::Original,
                Rotation::Clockwise270,
                Some(Path::new("r.jpg")),
                3,
            )
            .unwrap();

        assert_eq!(turned.rotation, Rotation::Clockwise270);
        assert_eq!(repo.get_page(page.id).unwrap().unwrap(), turned);
    }

    #[test]
    fn folders_count_documents_and_release_them_when_deleted() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let (bills, work) = (folder("cuentas"), folder("Archivo"));
        repo.insert_folder(&bills).unwrap();
        repo.insert_folder(&work).unwrap();
        let names: Vec<_> = repo
            .list_folders()
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["Archivo", "cuentas"]);

        let inside = Document {
            folder_id: Some(bills.id),
            ..doc("Luz", 1)
        };
        let (moved, trashed) = (doc("Agua", 2), doc("Gas", 3));
        for d in [&inside, &moved, &trashed] {
            repo.insert_document(d).unwrap();
        }
        assert_eq!(
            repo.get_document(inside.id).unwrap().unwrap().folder_id,
            Some(bills.id)
        );
        let ids = [moved.id, trashed.id, DocumentId::new()];
        assert_eq!(repo.move_documents(&ids, Some(bills.id)).unwrap(), 2);
        repo.set_document_trashed(trashed.id, Some(9)).unwrap();
        assert_eq!(
            repo.get_folder(bills.id).unwrap().unwrap().document_count,
            2
        );

        let renamed = repo.rename_folder(work.id, "Trabajo", 5).unwrap().unwrap();
        assert_eq!(
            (renamed.name.as_str(), renamed.updated_at_ms),
            ("Trabajo", 5)
        );
        assert_eq!(repo.rename_folder(FolderId::new(), "x", 5).unwrap(), None);

        assert!(repo.delete_folder(bills.id).unwrap());
        assert!(!repo.delete_folder(bills.id).unwrap());
        for id in [inside.id, moved.id, trashed.id] {
            assert_eq!(repo.get_document(id).unwrap().unwrap().folder_id, None);
        }
        assert_eq!(
            repo.move_documents(&[inside.id], Some(bills.id)).ok(),
            None,
            "folder must exist"
        );
    }

    #[test]
    fn files_by_document_lists_every_image() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 1);
        repo.insert_document(&d).unwrap();
        let page = repo.append_page(&new_page(d.id, 2)).unwrap();
        repo.set_page_rendering(
            page.id,
            None,
            PageFilter::Grayscale,
            Rotation::None,
            Some(Path::new("g.jpg")),
            3,
        )
        .unwrap();

        let mut files = repo.files_by_document().unwrap();
        files.sort_by(|a, b| a.1.cmp(&b.1));
        let mut expected = vec![(d.id, page.original_path), (d.id, PathBuf::from("g.jpg"))];
        expected.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(files, expected);
    }

    #[test]
    fn trashed_documents_are_listed_apart_and_hidden_from_search_and_ocr() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let (kept, trashed) = (doc("a", 1), doc("b", 2));
        repo.insert_document(&kept).unwrap();
        repo.insert_document(&trashed).unwrap();
        let page = repo.append_page(&new_page(trashed.id, 3)).unwrap();
        let path = page.original_path.to_string_lossy().into_owned();
        save(&repo, &page_text(&page, &path, &[(0, "contrato")]));
        let pending_before = repo.append_page(&new_page(trashed.id, 4)).unwrap();
        assert_eq!(
            ids(&repo.pages_without_text(10).unwrap()),
            vec![pending_before.id]
        );

        assert!(repo.set_document_trashed(trashed.id, Some(50)).unwrap());

        let listed: Vec<_> = repo
            .list_documents()
            .unwrap()
            .iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(listed, vec![kept.id]);
        let trash = repo.list_trashed_documents().unwrap();
        assert_eq!(
            (trash.len(), trash[0].trashed_at_ms, trash[0].page_count),
            (1, Some(50), 2)
        );
        assert!(search(&repo, "contrato").is_empty());
        assert!(repo.pages_without_text(10).unwrap().is_empty());
        assert_eq!(repo.trashed_before(51).unwrap(), vec![trashed.id]);
        assert!(repo.trashed_before(50).unwrap().is_empty());

        assert!(repo.set_document_trashed(trashed.id, None).unwrap());
        let restored = repo.get_document(trashed.id).unwrap().unwrap();
        assert_eq!(
            (restored.trashed_at_ms, restored.updated_at_ms),
            (None, trashed.updated_at_ms + 2)
        );
        assert_eq!(search(&repo, "contrato"), vec![page.id]);
        assert!(
            !repo
                .set_document_trashed(DocumentId::new(), Some(1))
                .unwrap()
        );
    }

    #[test]
    fn referenced_files_lists_originals_and_processed_images() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 1);
        repo.insert_document(&d).unwrap();
        assert!(repo.referenced_files().unwrap().is_empty());

        let first = repo.append_page(&new_page(d.id, 2)).unwrap();
        let second = repo.append_page(&new_page(d.id, 3)).unwrap();
        repo.set_page_rendering(
            first.id,
            None,
            PageFilter::Grayscale,
            Rotation::None,
            Some(Path::new("p.jpg")),
            4,
        )
        .unwrap();

        let mut files = repo.referenced_files().unwrap();
        files.sort();
        let mut expected = vec![
            first.original_path,
            second.original_path,
            PathBuf::from("p.jpg"),
        ];
        expected.sort();
        assert_eq!(files, expected);

        repo.delete_document(d.id).unwrap();
        assert!(repo.referenced_files().unwrap().is_empty());
    }

    fn ids(pages: &[Page]) -> Vec<PageId> {
        pages.iter().map(|p| p.id).collect()
    }

    #[test]
    fn rename_and_delete_documents() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("draft", 1);
        repo.insert_document(&d).unwrap();
        repo.append_page(&new_page(d.id, 2)).unwrap();

        let renamed = repo.rename_document(d.id, "final", 5).unwrap().unwrap();
        assert_eq!(
            (renamed.title.as_str(), renamed.updated_at_ms),
            ("final", 5)
        );
        assert_eq!(
            repo.rename_document(DocumentId::new(), "x", 5).unwrap(),
            None
        );

        assert!(repo.delete_document(d.id).unwrap());
        assert_eq!(repo.get_document(d.id).unwrap(), None);
        assert!(
            repo.list_pages(d.id).unwrap().is_empty(),
            "pages must cascade"
        );
        assert!(!repo.delete_document(d.id).unwrap());
    }

    #[test]
    fn deleting_pages_keeps_positions_contiguous() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 1);
        repo.insert_document(&d).unwrap();
        let p: Vec<_> = (0..5)
            .map(|i| repo.append_page(&new_page(d.id, i)).unwrap())
            .collect();

        assert!(repo.delete_page(p[0].id, 10).unwrap());
        assert!(repo.delete_page(p[2].id, 11).unwrap());
        assert!(!repo.delete_page(p[2].id, 12).unwrap());

        let pages = repo.list_pages(d.id).unwrap();
        assert_eq!(ids(&pages), vec![p[1].id, p[3].id, p[4].id]);
        assert_eq!(
            pages.iter().map(|p| p.position).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(repo.get_document(d.id).unwrap().unwrap().page_count, 3);
        // Appending after deletions still finds a free slot.
        let appended = repo.append_page(&new_page(d.id, 20)).unwrap();
        assert_eq!(appended.position, 3);
    }

    #[test]
    fn moving_pages_in_both_directions() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let d = doc("d", 1);
        repo.insert_document(&d).unwrap();
        let p: Vec<_> = (0..4)
            .map(|i| repo.append_page(&new_page(d.id, i)).unwrap().id)
            .collect();

        // Last to first, first to last, middle forward, out-of-range clamps.
        assert_eq!(
            ids(&repo.move_page(p[3], 0, 5).unwrap()),
            vec![p[3], p[0], p[1], p[2]]
        );
        assert_eq!(
            ids(&repo.move_page(p[3], 3, 6).unwrap()),
            vec![p[0], p[1], p[2], p[3]]
        );
        assert_eq!(
            ids(&repo.move_page(p[1], 2, 7).unwrap()),
            vec![p[0], p[2], p[1], p[3]]
        );
        let order = repo.move_page(p[0], 99, 8).unwrap();
        assert_eq!(ids(&order), vec![p[2], p[1], p[3], p[0]]);
        assert_eq!(
            order.iter().map(|p| p.position).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(repo.get_document(d.id).unwrap().unwrap().updated_at_ms, 8);
        assert!(repo.move_page(PageId::new(), 0, 9).is_err());
    }

    #[test]
    fn set_crop_on_missing_page_fails() {
        let repo = SqliteDocumentRepository::open_in_memory().unwrap();
        let crop = Crop::new([(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]).unwrap();
        assert!(
            repo.set_page_rendering(
                PageId::new(),
                Some(&crop),
                PageFilter::Original,
                Rotation::None,
                Some(Path::new("x.jpg")),
                1
            )
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
        assert_eq!(page.filter, PageFilter::Original);
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
