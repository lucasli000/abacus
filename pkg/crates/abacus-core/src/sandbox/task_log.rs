//! TaskLog — 沙箱执行日志持久化（SQLite）
//!
//! 每步执行及校验结果写入 task_logs 表，支持按 task/phase/step 检索。

use std::path::PathBuf;
use std::sync::Arc;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;


// 默认路径来自 crate::paths，遵循 ABACUS_HOME 覆盖；保留 const 名仅作 grep 锚点。
// 不再 hardcode "~/.abacus/task_logs.db"——见 paths::task_logs_db()。

/// 单步执行日志
#[derive(Debug, Clone)]
pub struct StepLog {
    pub task_id: String,
    pub phase_id: String,
    pub step_id: String,
    pub attempt: u32,
    pub run_model: String,
    pub verify_model: String,
    pub input_summary: String,
    pub output: String,
    pub verification_results: String,
    pub passed: bool,
    pub latency_ms: u64,
    pub timestamp_ms: i64,
}

/// 任务日志存储
pub struct TaskLogStore {
    conn: Arc<Mutex<Connection>>,
}

impl TaskLogStore {
    pub fn new(path: Option<PathBuf>) -> Result<Self, String> {
        let path = path.unwrap_or_else(crate::paths::task_logs_db);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        let conn = Connection::open(&path).map_err(|e| format!("open task_logs: {e}"))?;
        crate::db_util::apply_standard_pragmas(&conn)
            .map_err(|e| format!("pragma: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS task_logs (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 task_id TEXT NOT NULL,
                 phase_id TEXT NOT NULL,
                 step_id TEXT NOT NULL,
                 attempt INTEGER NOT NULL DEFAULT 1,
                 run_model TEXT NOT NULL DEFAULT '',
                 verify_model TEXT NOT NULL DEFAULT '',
                 input_summary TEXT NOT NULL DEFAULT '',
                 output TEXT NOT NULL DEFAULT '',
                 verification_results TEXT NOT NULL DEFAULT '[]',
                 passed INTEGER NOT NULL DEFAULT 0,
                 latency_ms INTEGER NOT NULL DEFAULT 0,
                 timestamp_ms INTEGER NOT NULL,
                 created_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE INDEX IF NOT EXISTS idx_task_logs_task ON task_logs(task_id);
             CREATE INDEX IF NOT EXISTS idx_task_logs_ts ON task_logs(timestamp_ms);"
        ).map_err(|e| format!("schema: {e}"))?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub fn in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        crate::db_util::apply_standard_pragmas(&conn)
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS task_logs (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 task_id TEXT NOT NULL, phase_id TEXT NOT NULL, step_id TEXT NOT NULL,
                 attempt INTEGER NOT NULL DEFAULT 1,
                 run_model TEXT NOT NULL DEFAULT '', verify_model TEXT NOT NULL DEFAULT '',
                 input_summary TEXT NOT NULL DEFAULT '', output TEXT NOT NULL DEFAULT '',
                 verification_results TEXT NOT NULL DEFAULT '[]',
                 passed INTEGER NOT NULL DEFAULT 0,
                 latency_ms INTEGER NOT NULL DEFAULT 0, timestamp_ms INTEGER NOT NULL,
                 created_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE INDEX IF NOT EXISTS idx_task_logs_task ON task_logs(task_id);"
        ).map_err(|e| e.to_string())?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// 写入一步执行日志
    pub async fn write_step(&self, log: &StepLog) -> Result<(), String> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO task_logs (task_id, phase_id, step_id, attempt, run_model, verify_model,
             input_summary, output, verification_results, passed, latency_ms, timestamp_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                log.task_id, log.phase_id, log.step_id, log.attempt,
                log.run_model, log.verify_model, log.input_summary,
                log.output, log.verification_results,
                log.passed as i32, log.latency_ms, log.timestamp_ms,
            ],
        ).map_err(|e| format!("write task_log: {e}"))?;
        Ok(())
    }

    /// 查询某 task 的全部日志
    pub async fn get_task_logs(&self, task_id: &str) -> Result<Vec<StepLog>, String> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT task_id, phase_id, step_id, attempt, run_model, verify_model,
             input_summary, output, verification_results, passed, latency_ms, timestamp_ms
             FROM task_logs WHERE task_id = ?1 ORDER BY id"
        ).map_err(|e| e.to_string())?;
        let rows = stmt.query_map(params![task_id], |row| {
            Ok(StepLog {
                task_id: row.get(0)?, phase_id: row.get(1)?, step_id: row.get(2)?,
                attempt: row.get(3)?, run_model: row.get(4)?, verify_model: row.get(5)?,
                input_summary: row.get(6)?, output: row.get(7)?,
                verification_results: row.get(8)?,
                passed: row.get::<_, i32>(9)? != 0,
                latency_ms: row.get(10)?, timestamp_ms: row.get(11)?,
            })
        }).map_err(|e| e.to_string())?;
        let mut logs = Vec::new();
        for row in rows.flatten() { logs.push(row); }
        Ok(logs)
    }

    /// 最近 N 条日志
    pub async fn recent(&self, limit: usize) -> Result<Vec<StepLog>, String> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT task_id, phase_id, step_id, attempt, run_model, verify_model,
             input_summary, output, verification_results, passed, latency_ms, timestamp_ms
             FROM task_logs ORDER BY id DESC LIMIT ?1"
        ).map_err(|e| e.to_string())?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(StepLog {
                task_id: row.get(0)?, phase_id: row.get(1)?, step_id: row.get(2)?,
                attempt: row.get(3)?, run_model: row.get(4)?, verify_model: row.get(5)?,
                input_summary: row.get(6)?, output: row.get(7)?,
                verification_results: row.get(8)?,
                passed: row.get::<_, i32>(9)? != 0,
                latency_ms: row.get(10)?, timestamp_ms: row.get(11)?,
            })
        }).map_err(|e| e.to_string())?;
        let mut logs = Vec::new();
        for row in rows.flatten() { logs.push(row); }
        logs.reverse();
        Ok(logs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_write_and_query() {
        let store = TaskLogStore::in_memory().unwrap();
        store.write_step(&StepLog {
            task_id: "t1".into(), phase_id: "p1".into(), step_id: "s1".into(),
            attempt: 1, run_model: "deepseek-v4".into(), verify_model: "deepseek-chat".into(),
            input_summary: "input".into(), output: "output".into(),
            verification_results: "[]".into(), passed: true,
            latency_ms: 1000, timestamp_ms: 1000,
        }).await.unwrap();

        let logs = store.get_task_logs("t1").await.unwrap();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].passed);
    }

    #[tokio::test]
    async fn test_recent() {
        let store = TaskLogStore::in_memory().unwrap();
        for i in 0..5 {
            store.write_step(&StepLog {
                task_id: "t".into(), phase_id: "p".into(), step_id: format!("s{i}"),
                attempt: 1, run_model: "m".into(), verify_model: "v".into(),
                input_summary: String::new(), output: String::new(),
                verification_results: "[]".into(), passed: true,
                latency_ms: 0, timestamp_ms: i * 1000,
            }).await.unwrap();
        }
        let recent = store.recent(3).await.unwrap();
        assert_eq!(recent.len(), 3);
    }
}
