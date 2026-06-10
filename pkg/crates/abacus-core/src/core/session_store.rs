//! SQLite-backed SessionStore — persists session snapshots to disk.
//!
//! ## Dependencies
//! - `rusqlite`: SQLite database with bundled libsqlite3
//! - `abacus_types::KernelError`: error type
//! - `abacus_types::context::SessionSnapshot`: data model
//!
//! ## References
//! - Implements: `SessionStore` trait (context.rs)
//! - Called by: `ContextManager` for cold storage of session snapshots
//! - Database path: configurable via `SqliteSessionStore::new(path)`

use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use abacus_types::KernelError;

use crate::core::context::{SessionSnapshot, SessionStore};

/// SQLite-backed implementation of [`SessionStore`].
///
/// Stores session snapshots in a local SQLite database with FTS5 full-text search.
/// Thread-safe via Arc<Mutex<Connection>>.
pub struct SqliteSessionStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl SqliteSessionStore {
    /// Create a new SQLite session store, initializing the schema if needed.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, KernelError> {
        let conn = rusqlite::Connection::open(&path)
            .map_err(|e| KernelError::Other(format!("sqlite open: {e}")))?;

        // Enable WAL mode for better concurrent read performance
        crate::db_util::apply_standard_pragmas(&conn)
            .map_err(|e| KernelError::Other(format!("sqlite pragma: {e}")))?;

        // Create session_snapshots table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS session_snapshots (
                session_id TEXT PRIMARY KEY,
                turn_count INTEGER NOT NULL DEFAULT 0,
                summary TEXT NOT NULL DEFAULT '',
                token_estimate INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                key_decisions TEXT NOT NULL DEFAULT '[]'
            )",
            [],
        ).map_err(|e| KernelError::Other(format!("sqlite create table: {e}")))?;

        // Create FTS5 virtual table for full-text search
        conn.execute(
            "CREATE VIRTUAL TABLE IF NOT EXISTS session_search USING fts5(
                session_id, summary, key_decisions,
                content='session_snapshots',
                content_rowid='rowid'
            )",
            [],
        ).map_err(|e| KernelError::Other(format!("sqlite fts5: {e}")))?;

        // Create triggers to keep FTS5 index in sync
        conn.execute_batch("
            CREATE TRIGGER IF NOT EXISTS session_ai AFTER INSERT ON session_snapshots BEGIN
                INSERT INTO session_search(rowid, session_id, summary, key_decisions)
                VALUES (new.rowid, new.session_id, new.summary, new.key_decisions);
            END;
            CREATE TRIGGER IF NOT EXISTS session_ad AFTER DELETE ON session_snapshots BEGIN
                INSERT INTO session_search(session_search, rowid, session_id, summary, key_decisions)
                VALUES('delete', old.rowid, old.session_id, old.summary, old.key_decisions);
            END;
            CREATE TRIGGER IF NOT EXISTS session_au AFTER UPDATE ON session_snapshots BEGIN
                INSERT INTO session_search(session_search, rowid, session_id, summary, key_decisions)
                VALUES('delete', old.rowid, old.session_id, old.summary, old.key_decisions);
                INSERT INTO session_search(rowid, session_id, summary, key_decisions)
                VALUES (new.rowid, new.session_id, new.summary, new.key_decisions);
            END;
        ").map_err(|e| KernelError::Other(format!("sqlite triggers: {e}")))?;

        // W2 (RFC-0001v2): ColdTier message_blocks table + FTS5
        conn.execute(
            "CREATE TABLE IF NOT EXISTS message_blocks (
                recall_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                turn_start INTEGER NOT NULL,
                turn_end INTEGER NOT NULL,
                summary TEXT NOT NULL DEFAULT '',
                content_json TEXT NOT NULL DEFAULT '[]',
                key_decisions TEXT NOT NULL DEFAULT '[]',
                original_tokens INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            )", [],
        ).map_err(|e| KernelError::Other(format!("sqlite create message_blocks: {e}")))?;

        conn.execute(
            "CREATE VIRTUAL TABLE IF NOT EXISTS block_search USING fts5(
                recall_id, summary, key_decisions,
                content='message_blocks', content_rowid='rowid'
            )", [],
        ).map_err(|e| KernelError::Other(format!("sqlite block_search fts5: {e}")))?;

        conn.execute_batch("
            CREATE TRIGGER IF NOT EXISTS block_ai AFTER INSERT ON message_blocks BEGIN
                INSERT INTO block_search(rowid, recall_id, summary, key_decisions)
                VALUES (new.rowid, new.recall_id, new.summary, new.key_decisions);
            END;
            CREATE TRIGGER IF NOT EXISTS block_ad AFTER DELETE ON message_blocks BEGIN
                INSERT INTO block_search(block_search, rowid, recall_id, summary, key_decisions)
                VALUES('delete', old.rowid, old.recall_id, old.summary, old.key_decisions);
            END;
            CREATE TRIGGER IF NOT EXISTS block_au AFTER UPDATE ON message_blocks BEGIN
                INSERT INTO block_search(block_search, rowid, recall_id, summary, key_decisions)
                VALUES('delete', old.rowid, old.recall_id, old.summary, old.key_decisions);
                INSERT INTO block_search(rowid, recall_id, summary, key_decisions)
                VALUES (new.rowid, new.recall_id, new.summary, new.key_decisions);
            END;
        ").map_err(|e| KernelError::Other(format!("sqlite block triggers: {e}")))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Create an in-memory session store (useful for testing).
    pub fn in_memory() -> Result<Self, KernelError> {
        Self::new(":memory:")
    }
}

#[async_trait::async_trait]
impl SessionStore for SqliteSessionStore {
    async fn save(&self, snapshot: SessionSnapshot) -> Result<(), KernelError> {
        let decisions_json = serde_json::to_string(&snapshot.key_decisions)
            .unwrap_or_else(|_| "[]".into());
        let now = chrono::Utc::now().timestamp();

        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO session_snapshots (session_id, turn_count, summary, token_estimate, created_at, updated_at, key_decisions)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(session_id) DO UPDATE SET
                turn_count = excluded.turn_count,
                summary = excluded.summary,
                token_estimate = excluded.token_estimate,
                updated_at = excluded.updated_at,
                key_decisions = excluded.key_decisions",
            rusqlite::params![
                snapshot.session_id,
                snapshot.turn_count,
                snapshot.summary,
                snapshot.token_estimate,
                snapshot.created_at,
                now,
                decisions_json,
            ],
        ).map_err(|e| KernelError::Other(format!("sqlite save: {e}")))?;

        Ok(())
    }

    async fn load_recent(&self, limit: usize) -> Result<Vec<SessionSnapshot>, KernelError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT session_id, turn_count, summary, token_estimate, created_at, key_decisions
             FROM session_snapshots
             ORDER BY updated_at DESC
             LIMIT ?1"
        ).map_err(|e| KernelError::Other(format!("sqlite prepare: {e}")))?;

        let rows = stmt.query_map(rusqlite::params![limit], |row| {
            let decisions_json: String = row.get(5)?;
            let key_decisions: Vec<String> = serde_json::from_str(&decisions_json).unwrap_or_else(|e| {
                tracing::warn!("failed to parse key_decisions JSON: {e}, raw: {}", &decisions_json[..decisions_json.len().min(200)]);
                Vec::new()
            });
            Ok(SessionSnapshot {
                session_id: row.get(0)?,
                turn_count: row.get(1)?,
                summary: row.get(2)?,
                token_estimate: row.get(3)?,
                created_at: row.get(4)?,
                key_decisions,
            })
        }).map_err(|e| KernelError::Other(format!("sqlite query: {e}")))?;

        let mut snapshots = Vec::new();
        for row in rows {
            snapshots.push(row.map_err(|e| KernelError::Other(format!("sqlite row: {e}")))?);
        }

        Ok(snapshots)
    }

    async fn save_block(&self, block: crate::core::context::BlockRecord) -> Result<(), KernelError> {
        let decisions_json = serde_json::to_string(&block.key_decisions)
            .unwrap_or_else(|_| "[]".into());
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO message_blocks (recall_id, session_id, turn_start, turn_end,
             summary, content_json, key_decisions, original_tokens, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(recall_id) DO UPDATE SET
                turn_start = excluded.turn_start,
                turn_end = excluded.turn_end,
                summary = excluded.summary",
            rusqlite::params![
                block.recall_id, block.session_id, block.turn_start, block.turn_end,
                block.summary, block.content_json, decisions_json,
                block.original_tokens, block.created_at,
            ],
        ).map_err(|e| KernelError::Other(format!("sqlite save_block: {e}")))?;
        Ok(())
    }

    async fn search_blocks(&self, query: &str, limit: usize) -> Result<Vec<crate::core::context::BlockResult>, KernelError> {
        let escaped = query.replace('"', "\"\"");
        let fts_query = format!("\"{}\"", escaped);
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT b.recall_id, b.summary, b.key_decisions,
                    b.original_tokens, b.turn_start, b.turn_end
             FROM message_blocks b
             JOIN block_search fts ON b.rowid = fts.rowid
             WHERE block_search MATCH ?1
             ORDER BY rank
             LIMIT ?2"
        ).map_err(|e| KernelError::Other(format!("sqlite prepare blocks: {e}")))?;

        let rows = stmt.query_map(rusqlite::params![fts_query, limit as i64], |row| {
            let decisions_json: String = row.get(2)?;
            let key_decisions: Vec<String> = serde_json::from_str(&decisions_json).unwrap_or_default();
            Ok(crate::core::context::BlockResult {
                recall_id: row.get(0)?,
                summary: row.get(1)?,
                key_decisions,
                original_tokens: row.get(3)?,
                turn_range: (row.get::<_, i64>(4)? as u32, row.get::<_, i64>(5)? as u32),
                score: 0.5,
            })
        }).map_err(|e| KernelError::Other(format!("sqlite query blocks: {e}")))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| KernelError::Other(format!("sqlite block row: {e}")))?);
        }
        Ok(results)
    }

    async fn search(&self, query: &str) -> Result<Vec<SessionSnapshot>, KernelError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT s.session_id, s.turn_count, s.summary, s.token_estimate, s.created_at, s.key_decisions
             FROM session_snapshots s
             JOIN session_search fts ON s.rowid = fts.rowid
             WHERE session_search MATCH ?1
             ORDER BY rank
             LIMIT 20"
        ).map_err(|e| KernelError::Other(format!("sqlite prepare: {e}")))?;

        // Escape FTS5 special characters
        let escaped = query.replace('"', "\"\"");
        let fts_query = format!("\"{}\"", escaped);

        let rows = stmt.query_map(rusqlite::params![fts_query], |row| {
            let decisions_json: String = row.get(5)?;
            let key_decisions: Vec<String> = serde_json::from_str(&decisions_json).unwrap_or_else(|e| {
                tracing::warn!("failed to parse key_decisions JSON: {e}, raw: {}", &decisions_json[..decisions_json.len().min(200)]);
                Vec::new()
            });
            Ok(SessionSnapshot {
                session_id: row.get(0)?,
                turn_count: row.get(1)?,
                summary: row.get(2)?,
                token_estimate: row.get(3)?,
                created_at: row.get(4)?,
                key_decisions,
            })
        }).map_err(|e| KernelError::Other(format!("sqlite query: {e}")))?;

        let mut snapshots = Vec::new();
        for row in rows {
            snapshots.push(row.map_err(|e| KernelError::Other(format!("sqlite row: {e}")))?);
        }

        Ok(snapshots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_save_and_load() {
        let store = SqliteSessionStore::in_memory().unwrap();
        let snapshot = SessionSnapshot {
            session_id: "test-1".into(),
            turn_count: 5,
            summary: "Test session".into(),
            token_estimate: 1200,
            created_at: 12345,
            key_decisions: vec!["chose A".into()],
        };
        store.save(snapshot).await.unwrap();
        let loaded = store.load_recent(10).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].session_id, "test-1");
        assert_eq!(loaded[0].turn_count, 5);
    }

    #[tokio::test]
    async fn test_update_existing() {
        let store = SqliteSessionStore::in_memory().unwrap();
        store.save(SessionSnapshot {
            session_id: "test-2".into(),
            turn_count: 1,
            summary: "initial".into(),
            token_estimate: 100,
            created_at: 12345,
            key_decisions: vec![],
        }).await.unwrap();

        store.save(SessionSnapshot {
            session_id: "test-2".into(),
            turn_count: 3,
            summary: "updated".into(),
            token_estimate: 300,
            created_at: 12345,
            key_decisions: vec!["new decision".into()],
        }).await.unwrap();

        let loaded = store.load_recent(10).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].turn_count, 3);
        assert_eq!(loaded[0].summary, "updated");
    }

    #[tokio::test]
    async fn test_search() {
        let store = SqliteSessionStore::in_memory().unwrap();
        store.save(SessionSnapshot {
            session_id: "search-1".into(),
            turn_count: 2,
            summary: "code review of rust async patterns".into(),
            token_estimate: 500,
            created_at: 12345,
            key_decisions: vec![],
        }).await.unwrap();

        store.save(SessionSnapshot {
            session_id: "search-2".into(),
            turn_count: 1,
            summary: "market analysis report".into(),
            token_estimate: 200,
            created_at: 12346,
            key_decisions: vec![],
        }).await.unwrap();

        let results = store.search("rust async").await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].session_id, "search-1");
    }
}
