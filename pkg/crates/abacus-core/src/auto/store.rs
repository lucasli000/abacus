//! Task #81：AutoEngine 运行历史持久化层（SQLite）
//!
//! ## 范围
//! 仅持久化 **运行历史**——每次 Pipeline.run() 的输入/输出/状态。
//! Pipeline 定义本身留内存（与 Skill/LSP 等子系统一致：运行时注册，重启需重新注册）。
//!
//! ## Schema
//! ```sql
//! CREATE TABLE pipeline_runs (
//!   id INTEGER PRIMARY KEY AUTOINCREMENT,
//!   pipeline_id TEXT NOT NULL,
//!   state TEXT NOT NULL,           -- "Completed" | "Failed:<msg>"
//!   results_json TEXT NOT NULL,    -- JSON array of {step_id, success, output}
//!   started_at INTEGER NOT NULL,   -- Unix epoch seconds
//!   ended_at INTEGER NOT NULL,
//!   duration_ms INTEGER NOT NULL
//! );
//! CREATE INDEX idx_runs_pipeline_id ON pipeline_runs(pipeline_id);
//! CREATE INDEX idx_runs_started_at ON pipeline_runs(started_at DESC);
//! ```
//!
//! ## 引用关系
//! - 被 `AutoEngine.fire / fire_pipeline` 在每次运行后写入
//! - 被 `AutoEngine.recent_runs / runs_by_pipeline` 查询
//! 生命周期：随 AutoEngine 创建/销毁；无 background thread，纯同步写入（短事务）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::pipeline::{PipelineRunResult, PipelineState, StepResult};

/// 单次 Pipeline 运行的持久化记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineRunRecord {
    pub id: i64,
    pub pipeline_id: String,
    pub state: String,
    pub step_count: usize,
    pub started_at: i64,
    pub ended_at: i64,
    pub duration_ms: i64,
}

/// 单 step 的序列化形式
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StepRecord {
    step_id: String,
    success: bool,
    output: String,
}

impl From<&StepResult> for StepRecord {
    fn from(s: &StepResult) -> Self {
        Self {
            step_id: s.step_id.clone(),
            success: s.success,
            output: s.output.clone(),
        }
    }
}

pub struct AutoStore {
    conn: Arc<Mutex<Connection>>,
    db_path: PathBuf,
}

impl AutoStore {
    /// 打开或创建 DB 文件（启用 WAL + busy_timeout）
    pub fn new(db_path: impl AsRef<Path>) -> Result<Self, String> {
        let path = db_path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create auto-store dir: {e}"))?;
        }
        let conn = Connection::open(&path).map_err(|e| format!("open auto-store: {e}"))?;
        Self::init_schema(&conn)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;",
        ).map_err(|e| format!("pragma: {e}"))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: path,
        })
    }

    /// 内存模式（测试用）
    pub fn in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| format!("open mem: {e}"))?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: PathBuf::from(":memory:"),
        })
    }

    fn init_schema(conn: &Connection) -> Result<(), String> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pipeline_runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pipeline_id TEXT NOT NULL,
                state TEXT NOT NULL,
                results_json TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                ended_at INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_runs_pipeline_id ON pipeline_runs(pipeline_id);
            CREATE INDEX IF NOT EXISTS idx_runs_started_at ON pipeline_runs(started_at DESC);"
        ).map_err(|e| format!("init schema: {e}"))?;
        Ok(())
    }

    /// 写入一次运行结果（短事务）
    ///
    /// 副作用：append-only INSERT；单次写入失败返回 Err，调用方决定是否告警/重试。
    pub async fn record_run(
        &self,
        result: &PipelineRunResult,
        started_at: i64,
        ended_at: i64,
    ) -> Result<i64, String> {
        let state_str = match &result.state {
            PipelineState::Completed => "Completed".to_string(),
            PipelineState::Failed(msg) => format!("Failed:{msg}"),
            PipelineState::Pending => "Pending".to_string(),
            PipelineState::Running => "Running".to_string(),
        };
        let steps: Vec<StepRecord> = result.results.iter().map(StepRecord::from).collect();
        let json = serde_json::to_string(&steps).map_err(|e| format!("encode: {e}"))?;
        let duration = (ended_at - started_at).max(0) * 1000;

        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO pipeline_runs
             (pipeline_id, state, results_json, started_at, ended_at, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![result.pipeline_id, state_str, json, started_at, ended_at, duration],
        ).map_err(|e| format!("insert: {e}"))?;
        Ok(conn.last_insert_rowid())
    }

    /// 查询最近 N 条运行记录（不含 step 详情）
    pub async fn recent_runs(&self, limit: usize) -> Result<Vec<PipelineRunRecord>, String> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, pipeline_id, state, results_json, started_at, ended_at, duration_ms
             FROM pipeline_runs ORDER BY started_at DESC LIMIT ?1"
        ).map_err(|e| format!("prepare: {e}"))?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            let json: String = row.get(3)?;
            let steps: Vec<StepRecord> = serde_json::from_str(&json).unwrap_or_default();
            Ok(PipelineRunRecord {
                id: row.get(0)?,
                pipeline_id: row.get(1)?,
                state: row.get(2)?,
                step_count: steps.len(),
                started_at: row.get(4)?,
                ended_at: row.get(5)?,
                duration_ms: row.get(6)?,
            })
        }).map_err(|e| format!("query: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("row: {e}"))?);
        }
        Ok(out)
    }

    /// 按 pipeline_id 查询历史
    pub async fn runs_by_pipeline(
        &self,
        pipeline_id: &str,
        limit: usize,
    ) -> Result<Vec<PipelineRunRecord>, String> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, pipeline_id, state, results_json, started_at, ended_at, duration_ms
             FROM pipeline_runs WHERE pipeline_id = ?1
             ORDER BY started_at DESC LIMIT ?2"
        ).map_err(|e| format!("prepare: {e}"))?;
        let rows = stmt.query_map(params![pipeline_id, limit as i64], |row| {
            let json: String = row.get(3)?;
            let steps: Vec<StepRecord> = serde_json::from_str(&json).unwrap_or_default();
            Ok(PipelineRunRecord {
                id: row.get(0)?,
                pipeline_id: row.get(1)?,
                state: row.get(2)?,
                step_count: steps.len(),
                started_at: row.get(4)?,
                ended_at: row.get(5)?,
                duration_ms: row.get(6)?,
            })
        }).map_err(|e| format!("query: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("row: {e}"))?);
        }
        Ok(out)
    }

    /// DB 路径（诊断）
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_result(id: &str, completed: bool) -> PipelineRunResult {
        PipelineRunResult {
            pipeline_id: id.to_string(),
            state: if completed {
                PipelineState::Completed
            } else {
                PipelineState::Failed("oops".into())
            },
            results: vec![StepResult {
                step_id: "s1".into(),
                success: completed,
                output: "out".into(),
            }],
        }
    }

    #[tokio::test]
    async fn record_and_recall() {
        let store = AutoStore::in_memory().unwrap();
        store.record_run(&mk_result("p1", true), 100, 105).await.unwrap();
        store.record_run(&mk_result("p2", false), 200, 210).await.unwrap();
        let recent = store.recent_runs(10).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].pipeline_id, "p2");
        assert!(recent[0].state.starts_with("Failed:"));
    }

    #[tokio::test]
    async fn filter_by_pipeline() {
        let store = AutoStore::in_memory().unwrap();
        store.record_run(&mk_result("p1", true), 100, 105).await.unwrap();
        store.record_run(&mk_result("p1", true), 200, 210).await.unwrap();
        store.record_run(&mk_result("p2", true), 300, 305).await.unwrap();
        let p1 = store.runs_by_pipeline("p1", 10).await.unwrap();
        assert_eq!(p1.len(), 2);
        let p2 = store.runs_by_pipeline("p2", 10).await.unwrap();
        assert_eq!(p2.len(), 1);
    }
}
