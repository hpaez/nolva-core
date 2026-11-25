//! Forward-only schema migrations tracked with `PRAGMA user_version`.
//!
//! Never edit a migration that has shipped; append a new one instead.

use rusqlite::{Connection, Transaction};

const MIGRATIONS: &[&str] = &[
    // 1: documents and pages
    r#"
    CREATE TABLE documents (
        id            TEXT PRIMARY KEY NOT NULL,
        title         TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE pages (
        id            TEXT PRIMARY KEY NOT NULL,
        document_id   TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
        position      INTEGER NOT NULL CHECK (position >= 0),
        original_path TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL,
        UNIQUE (document_id, position)
    ) STRICT;

    CREATE INDEX documents_updated_at_idx ON documents(updated_at_ms DESC);
    "#,
    // 2: non-destructive crops. `crop` holds 8 comma-separated normalized
    // coordinates (TL, TR, BR, BL as x,y pairs); both NULL until cropped.
    r#"
    ALTER TABLE pages ADD COLUMN crop TEXT;
    ALTER TABLE pages ADD COLUMN processed_path TEXT;
    "#,
    // 3: page filters. Existing pages keep their look.
    r#"
    ALTER TABLE pages ADD COLUMN filter TEXT NOT NULL DEFAULT 'original'
        CHECK (filter IN ('original', 'grayscale', 'black_and_white', 'enhanced'));
    "#,
    // 4: recognized text. `source_path` is the image the text came from;
    // the text is current while it equals the page's displayed image.
    // `page_text_search` indexes `page_texts.text` by rowid and is kept in
    // step by triggers, which also fire on cascaded deletes.
    r#"
    CREATE TABLE page_texts (
        page_id          TEXT PRIMARY KEY NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
        source_path      TEXT NOT NULL,
        text             TEXT NOT NULL,
        recognized_at_ms INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE page_text_lines (
        page_id     TEXT NOT NULL REFERENCES page_texts(page_id) ON DELETE CASCADE,
        line_index  INTEGER NOT NULL CHECK (line_index >= 0),
        block_index INTEGER NOT NULL CHECK (block_index >= 0),
        text        TEXT NOT NULL,
        left        REAL NOT NULL,
        top         REAL NOT NULL,
        right       REAL NOT NULL,
        bottom      REAL NOT NULL,
        PRIMARY KEY (page_id, line_index)
    ) STRICT;

    CREATE VIRTUAL TABLE page_text_search USING fts5(
        text,
        tokenize = 'unicode61 remove_diacritics 2'
    );

    CREATE TRIGGER page_texts_after_insert AFTER INSERT ON page_texts BEGIN
        INSERT INTO page_text_search(rowid, text) VALUES (new.rowid, new.text);
    END;
    CREATE TRIGGER page_texts_after_delete AFTER DELETE ON page_texts BEGIN
        DELETE FROM page_text_search WHERE rowid = old.rowid;
    END;
    CREATE TRIGGER page_texts_after_update AFTER UPDATE OF text ON page_texts BEGIN
        DELETE FROM page_text_search WHERE rowid = old.rowid;
        INSERT INTO page_text_search(rowid, text) VALUES (new.rowid, new.text);
    END;
    "#,
    // 5: trash. NULL while the document isn't trashed.
    r#"
    ALTER TABLE documents ADD COLUMN trashed_at_ms INTEGER;
    CREATE INDEX documents_trashed_idx ON documents(trashed_at_ms)
        WHERE trashed_at_ms IS NOT NULL;
    "#,
    // 6: folders, one level deep. Deleting a folder keeps its documents,
    // which move back to the top level.
    r#"
    CREATE TABLE folders (
        id            TEXT PRIMARY KEY NOT NULL,
        name          TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL
    ) STRICT;

    ALTER TABLE documents ADD COLUMN folder_id TEXT
        REFERENCES folders(id) ON DELETE SET NULL;
    CREATE INDEX documents_folder_idx ON documents(folder_id);
    "#,
];

/// Schema 1 as shipped in v0.1, for migration tests.
#[cfg(test)]
pub(crate) const V1_FOR_TESTS: &str = MIGRATIONS[0];

/// Latest schema version known to this build.
pub(crate) fn latest_version() -> i64 {
    MIGRATIONS.len() as i64
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum MigrationError {
    #[error("database schema version {found} is newer than supported ({supported})")]
    TooNew { found: i64, supported: i64 },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

pub(crate) fn run(conn: &mut Connection) -> Result<(), MigrationError> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let latest = latest_version();
    if current > latest {
        // Opened by a newer app version (e.g. after a downgrade). Refuse
        // rather than risk corrupting data we don't understand.
        return Err(MigrationError::TooNew {
            found: current,
            supported: latest,
        });
    }

    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx: Transaction<'_> = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (index + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}
