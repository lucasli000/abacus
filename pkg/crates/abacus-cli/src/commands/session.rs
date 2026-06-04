use color_eyre::eyre::Result;
use crate::OutputFormatter;
use super::SessionAction;

pub async fn handle_session(args: &super::SessionArgs, formatter: &mut Box<dyn OutputFormatter>) -> Result<()> {
    let db_path = abacus_core::paths::sessions_db();

    match &args.action {
        SessionAction::List => {
            if !db_path.exists() {
                formatter.format_message("session", "No sessions (database not created yet)", None);
                return Ok(());
            }
            let conn = rusqlite::Connection::open(&db_path)
                .map_err(|e| color_eyre::eyre::eyre!("DB open: {}", e))?;
            let mut stmt = conn.prepare(
                "SELECT session_id, turn_count, summary, updated_at FROM session_snapshots ORDER BY updated_at DESC LIMIT 20"
            ).map_err(|e| color_eyre::eyre::eyre!("{}", e))?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            }).map_err(|e| color_eyre::eyre::eyre!("{}", e))?;

            formatter.format_message("session", "Sessions:", None);
            let mut count = 0;
            for row in rows.flatten() {
                let (id, turns, summary, _updated) = row;
                let sum = if summary.is_empty() { "(no summary)".to_string() } else { summary };
                formatter.format_message("session", &format!("  {} | {} turns | {}", id, turns, sum), None);
                count += 1;
            }
            if count == 0 {
                formatter.format_message("session", "  (empty)", None);
            }
        }
        SessionAction::Show { id } => {
            if !db_path.exists() {
                formatter.format_error("NOT_FOUND", "Session database not found", None);
                return Ok(());
            }
            let conn = rusqlite::Connection::open(&db_path)
                .map_err(|e| color_eyre::eyre::eyre!("{}", e))?;
            match conn.query_row(
                "SELECT session_id, turn_count, summary, token_estimate FROM session_snapshots WHERE session_id = ?1",
                [id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?))
            ) {
                Ok((sid, turns, summary, tokens)) => {
                    formatter.format_message("session", &format!("Session: {}", sid), None);
                    formatter.format_message("session", &format!("  Turns: {} | Tokens: ~{}", turns, tokens), None);
                    formatter.format_message("session", &format!("  Summary: {}", if summary.is_empty() { "(none)" } else { &summary }), None);
                }
                Err(_) => formatter.format_error("NOT_FOUND", &format!("Session '{}' not found", id), None),
            }
        }
        SessionAction::New { title } => {
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let conn = rusqlite::Connection::open(&db_path)
                .map_err(|e| color_eyre::eyre::eyre!("{}", e))?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS session_snapshots (
                    session_id TEXT PRIMARY KEY, turn_count INTEGER DEFAULT 0,
                    summary TEXT DEFAULT '', token_estimate INTEGER DEFAULT 0,
                    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                    key_decisions TEXT DEFAULT '[]')"
            ).map_err(|e| color_eyre::eyre::eyre!("{}", e))?;
            let now = chrono::Utc::now().timestamp();
            let session_id = format!("sess_{}", now);
            let summary = title.as_deref().unwrap_or("");
            conn.execute(
                "INSERT INTO session_snapshots (session_id, summary, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
                rusqlite::params![session_id, summary, now],
            ).map_err(|e| color_eyre::eyre::eyre!("{}", e))?;
            formatter.format_message("session", &format!("[✓] Created: {}", session_id), None);
        }
        SessionAction::Delete { id } => {
            if !db_path.exists() {
                formatter.format_error("NOT_FOUND", "Database not found", None);
                return Ok(());
            }
            let conn = rusqlite::Connection::open(&db_path)
                .map_err(|e| color_eyre::eyre::eyre!("{}", e))?;
            let n = conn.execute("DELETE FROM session_snapshots WHERE session_id = ?1", [id])
                .map_err(|e| color_eyre::eyre::eyre!("{}", e))?;
            if n > 0 {
                formatter.format_message("session", &format!("[✓] Deleted: {}", id), None);
            } else {
                formatter.format_error("NOT_FOUND", &format!("'{}' not found", id), None);
            }
        }
    }
    Ok(())
}
