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
