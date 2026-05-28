//! MetricStore — SQLite-backed time-series storage for deduction engine
//!
//! ## 场景
//! 每轮对话结束时，采集工具指标和 context 使用率快照，写入 SQLite。
//! 供 O3 信号分解 / A5 观察者污染检测 / context 退化预测 消费。
//!
//! ## 依赖
//! - `rusqlite`: 持久化存储（bundled）
//!
//! ## 引用关系
//! - 被 `DeductionEngine` 持有（lifecycle = app lifetime）
//! - `record_turn_metrics()` 被 `CoreLoop` 的 post-turn 钩子调用
//! - `load_tool_history()` / `load_context_history()` 被 analysis 函数调用
//!
//! ## 边界
//! - 默认 DB 路径: ~/.abacus/deduction_metrics.db
//! - WAL mode 避免写冲突
//! - 保留最近 30 天的数据，超期自动删除

use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use abacus_types::ToolId;

// 默认路径来自 crate::paths，遵循 ABACUS_HOME 覆盖。
// 不再 hardcode "~/.abacus/deduction_metrics.db"——见 paths::deduction_metrics_db()。
const RETENTION_DAYS: i64 = 30;

/// A single snapshot of tool metrics at one turn.
#[derive(Debug, Clone)]
pub struct ToolMetricPoint {
    pub tool_id: ToolId,
    pub turn_number: u32,
    pub session_id: String,
    pub timestamp_ms: i64,
    pub adoption_rate: f64,
    pub success_rate: f64,
    pub trend: f64,
    pub composite_score: f64,
    pub visibility_tier: String,
    pub opportunities: u64,
    pub invocations: u64,
    pub successes: u64,
    pub avg_latency_ms: f64,
}

/// A snapshot of context window usage at one turn.
#[derive(Debug, Clone)]
pub struct ContextUsagePoint {
    pub turn_number: u32,
    pub session_id: String,
    pub timestamp_ms: i64,
    pub usage_pct: f64,
    pub max_tokens: usize,
    pub current_tokens: usize,
    pub was_compressed: bool,
}

/// A snapshot of prompt structure at one turn.
#[derive(Debug, Clone)]
pub struct PromptStructurePoint {
    pub turn_number: u32,
    pub session_id: String,
    pub timestamp_ms: i64,
    pub layer_count: usize,
    pub tool_count: usize,
    pub tool_set_hash: i64,
    pub has_thinking: bool,
}

/// Time-series metric store backed by SQLite.
pub struct MetricStore {
    conn: Arc<Mutex<Connection>>,
}

impl MetricStore {
    /// Open or create the metrics database.
    pub fn new(path: Option<PathBuf>) -> Result<Self, String> {
        let path = path.unwrap_or_else(crate::paths::deduction_metrics_db);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        let conn = Connection::open(&path).map_err(|e| format!("open db: {e}"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;"
        ).map_err(|e| format!("pragma: {e}"))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tool_metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                tool_id TEXT NOT NULL,
                turn_number INTEGER NOT NULL,
                session_id TEXT NOT NULL,
                timestamp_ms INTEGER NOT NULL,
                adoption_rate REAL NOT NULL DEFAULT 0,
                success_rate REAL NOT NULL DEFAULT 0,
                trend REAL NOT NULL DEFAULT 0,
                composite_score REAL NOT NULL DEFAULT 0,
                visibility_tier TEXT NOT NULL DEFAULT 'A',
                opportunities INTEGER NOT NULL DEFAULT 0,
                invocations INTEGER NOT NULL DEFAULT 0,
                successes INTEGER NOT NULL DEFAULT 0,
                avg_latency_ms REAL NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_tool_metrics_tool ON tool_metrics(tool_id, timestamp_ms);
            CREATE INDEX IF NOT EXISTS idx_tool_metrics_ts ON tool_metrics(timestamp_ms);

            CREATE TABLE IF NOT EXISTS context_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                turn_number INTEGER NOT NULL,
                session_id TEXT NOT NULL,
                timestamp_ms INTEGER NOT NULL,
                usage_pct REAL NOT NULL,
                max_tokens INTEGER NOT NULL,
                current_tokens INTEGER NOT NULL,
                was_compressed INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_context_ts ON context_usage(timestamp_ms);

            CREATE TABLE IF NOT EXISTS prompt_structure (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                turn_number INTEGER NOT NULL,
                session_id TEXT NOT NULL,
                timestamp_ms INTEGER NOT NULL,
                layer_count INTEGER NOT NULL DEFAULT 0,
                tool_count INTEGER NOT NULL DEFAULT 0,
                tool_set_hash INTEGER NOT NULL DEFAULT 0,
                has_thinking INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_prompt_ts ON prompt_structure(timestamp_ms);"
        ).map_err(|e| format!("create tables: {e}"))?;

        let store = Self { conn: Arc::new(Mutex::new(conn)) };
        // Schedule cleanup of data older than 30 days (runs async on first tokio tick)
        let store_clone = store.conn.clone();
        tokio::spawn(async move {
            let cutoff_ms = (chrono::Utc::now() - chrono::Duration::days(30)).timestamp_millis();
            let conn = store_clone.lock().await;
            for table in &["tool_metrics", "context_usage", "prompt_structure"] {
                let _ = conn.execute(
                    &format!("DELETE FROM {} WHERE timestamp_ms < ?1", table),
                    [cutoff_ms],
                );
            }
        });
        Ok(store)
    }

    /// Create an in-memory store (testing).
    pub fn in_memory() -> Result<Self, String> {
        // Skip cleanup for in-memory (no persistence)
        let path = PathBuf::from(":memory:");
        let conn = Connection::open(&path).map_err(|e| format!("open db: {e}"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;"
        ).map_err(|e| format!("pragma: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tool_metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT, tool_id TEXT NOT NULL,
                turn_number INTEGER NOT NULL, session_id TEXT NOT NULL,
                timestamp_ms INTEGER NOT NULL, adoption_rate REAL DEFAULT 0,
                success_rate REAL DEFAULT 0, trend REAL DEFAULT 0,
                composite_score REAL DEFAULT 0, visibility_tier TEXT DEFAULT 'A',
                opportunities INTEGER DEFAULT 0, invocations INTEGER DEFAULT 0,
                successes INTEGER DEFAULT 0, avg_latency_ms REAL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS context_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT, turn_number INTEGER NOT NULL,
                session_id TEXT NOT NULL, timestamp_ms INTEGER NOT NULL,
                usage_pct REAL NOT NULL, max_tokens INTEGER NOT NULL,
                current_tokens INTEGER NOT NULL, was_compressed INTEGER DEFAULT 0);
            CREATE TABLE IF NOT EXISTS prompt_structure (
                id INTEGER PRIMARY KEY AUTOINCREMENT, turn_number INTEGER NOT NULL,
                session_id TEXT NOT NULL, timestamp_ms INTEGER NOT NULL,
                layer_count INTEGER DEFAULT 0, tool_count INTEGER DEFAULT 0,
                tool_set_hash INTEGER DEFAULT 0, has_thinking INTEGER DEFAULT 0);"
        ).map_err(|e| format!("create tables: {e}"))?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Remove data older than `days` from all metric tables.
    pub async fn cleanup_old_data(&self, days: u32) -> Result<usize, String> {
        let cutoff_ms = (chrono::Utc::now() - chrono::Duration::days(days as i64)).timestamp_millis();
        let conn = self.conn.lock().await;
        let mut total = 0usize;
        for table in &["tool_metrics", "context_usage", "prompt_structure"] {
            let n = conn.execute(
                &format!("DELETE FROM {} WHERE timestamp_ms < ?1", table),
                [cutoff_ms],
            ).map_err(|e| format!("cleanup {}: {}", table, e))?;
            total += n;
        }
        Ok(total)
    }

    /// Record tool metrics for one turn (called per tool after process_turn).
    pub async fn record_tool_metrics(&self, points: &[ToolMetricPoint]) -> Result<(), String> {
        let conn = self.conn.lock().await;
        for p in points {
            conn.execute(
                "INSERT INTO tool_metrics (tool_id, turn_number, session_id, timestamp_ms,
                 adoption_rate, success_rate, trend, composite_score, visibility_tier,
                 opportunities, invocations, successes, avg_latency_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    p.tool_id.0, p.turn_number, p.session_id, p.timestamp_ms,
                    p.adoption_rate, p.success_rate, p.trend, p.composite_score,
                    p.visibility_tier, p.opportunities, p.invocations, p.successes,
                    p.avg_latency_ms,
                ],
            ).map_err(|e| format!("insert tool metric: {e}"))?;
        }
        Ok(())
    }

    /// Record context usage for one turn.
    pub async fn record_context_usage(&self, point: &ContextUsagePoint) -> Result<(), String> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO context_usage (turn_number, session_id, timestamp_ms, usage_pct, max_tokens, current_tokens, was_compressed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                point.turn_number, point.session_id, point.timestamp_ms,
                point.usage_pct, point.max_tokens, point.current_tokens,
                point.was_compressed as i32,
            ],
        ).map_err(|e| format!("insert context usage: {e}"))?;
        Ok(())
    }

    /// Record prompt structure for one turn.
    pub async fn record_prompt_structure(&self, point: &PromptStructurePoint) -> Result<(), String> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO prompt_structure (turn_number, session_id, timestamp_ms, layer_count, tool_count, tool_set_hash, has_thinking)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                point.turn_number, point.session_id, point.timestamp_ms,
                point.layer_count, point.tool_count, point.tool_set_hash,
                point.has_thinking as i32,
            ],
        ).map_err(|e| format!("insert prompt structure: {e}"))?;
        Ok(())
    }

    /// Load tool metric history for a specific tool (most recent N points).
    pub async fn load_tool_history(&self, tool_id: &str, limit: usize) -> Result<Vec<ToolMetricPoint>, String> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT tool_id, turn_number, session_id, timestamp_ms,
                    adoption_rate, success_rate, trend, composite_score, visibility_tier,
                    opportunities, invocations, successes, avg_latency_ms
             FROM tool_metrics
             WHERE tool_id = ?1
             ORDER BY timestamp_ms DESC
             LIMIT ?2"
        ).map_err(|e| format!("prepare: {e}"))?;

        let rows = stmt.query_map(params![tool_id, limit as i64], |row| {
            Ok(ToolMetricPoint {
                tool_id: ToolId(row.get::<_, String>(0)?),
                turn_number: row.get(1)?,
                session_id: row.get(2)?,
                timestamp_ms: row.get(3)?,
                adoption_rate: row.get(4)?,
                success_rate: row.get(5)?,
                trend: row.get(6)?,
                composite_score: row.get(7)?,
                visibility_tier: row.get(8)?,
                opportunities: row.get(9)?,
                invocations: row.get(10)?,
                successes: row.get(11)?,
                avg_latency_ms: row.get(12)?,
            })
        }).map_err(|e| format!("query: {e}"))?;

        let mut results = Vec::new();
        for row in rows.flatten() { results.push(row); }
        results.reverse();
        Ok(results)
    }

    /// Load all recent tool metrics (for cross-tool, cross-session analysis).
    pub async fn load_all_recent_metrics(&self, limit: usize) -> Result<Vec<ToolMetricPoint>, String> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT tool_id, turn_number, session_id, timestamp_ms,
                    adoption_rate, success_rate, trend, composite_score, visibility_tier,
                    opportunities, invocations, successes, avg_latency_ms
             FROM tool_metrics
             ORDER BY timestamp_ms DESC
             LIMIT ?1"
        ).map_err(|e| format!("prepare: {e}"))?;

        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(ToolMetricPoint {
                tool_id: ToolId(row.get::<_, String>(0)?),
                turn_number: row.get(1)?,
                session_id: row.get(2)?,
                timestamp_ms: row.get(3)?,
                adoption_rate: row.get(4)?,
                success_rate: row.get(5)?,
                trend: row.get(6)?,
                composite_score: row.get(7)?,
                visibility_tier: row.get(8)?,
                opportunities: row.get(9)?,
                invocations: row.get(10)?,
                successes: row.get(11)?,
                avg_latency_ms: row.get(12)?,
            })
        }).map_err(|e| format!("query: {e}"))?;

        let mut results = Vec::new();
        for row in rows.flatten() { results.push(row); }
        results.reverse();
        Ok(results)
    }

    /// Load context usage history (most recent N points).
    pub async fn load_context_history(&self, limit: usize) -> Result<Vec<ContextUsagePoint>, String> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT turn_number, session_id, timestamp_ms, usage_pct, max_tokens, current_tokens, was_compressed
             FROM context_usage
             ORDER BY timestamp_ms DESC
             LIMIT ?1"
        ).map_err(|e| format!("prepare: {e}"))?;

        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(ContextUsagePoint {
                turn_number: row.get(0)?,
                session_id: row.get(1)?,
                timestamp_ms: row.get(2)?,
                usage_pct: row.get(3)?,
                max_tokens: row.get(4)?,
                current_tokens: row.get(5)?,
                was_compressed: row.get::<_, i32>(6)? != 0,
            })
        }).map_err(|e| format!("query: {e}"))?;

        let mut results = Vec::new();
        for row in rows.flatten() { results.push(row); }
        results.reverse();
        Ok(results)
    }

    /// Load prompt structure history (most recent N points).
    pub async fn load_prompt_history(&self, limit: usize) -> Result<Vec<PromptStructurePoint>, String> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT turn_number, session_id, timestamp_ms, layer_count, tool_count, tool_set_hash, has_thinking
             FROM prompt_structure
             ORDER BY timestamp_ms DESC
             LIMIT ?1"
        ).map_err(|e| format!("prepare: {e}"))?;

        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(PromptStructurePoint {
                turn_number: row.get(0)?,
                session_id: row.get(1)?,
                timestamp_ms: row.get(2)?,
                layer_count: row.get(3)?,
                tool_count: row.get(4)?,
                tool_set_hash: row.get(5)?,
                has_thinking: row.get::<_, i32>(6)? != 0,
            })
        }).map_err(|e| format!("query: {e}"))?;

        let mut results = Vec::new();
        for row in rows.flatten() { results.push(row); }
        results.reverse();
        Ok(results)
    }

    /// Get all tool IDs that have been tracked, with recency info.
    pub async fn list_tracked_tools(&self) -> Result<Vec<(String, i64)>, String> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT tool_id, MAX(timestamp_ms) as last_seen
             FROM tool_metrics
             GROUP BY tool_id
             ORDER BY last_seen DESC"
        ).map_err(|e| format!("prepare: {e}"))?;

        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }).map_err(|e| format!("query: {e}"))?;

        let mut results = Vec::new();
        for row in rows.flatten() { results.push(row); }
        Ok(results)
    }

    /// Clean up old data beyond retention period.
    pub async fn purge_old(&self) -> Result<(), String> {
        let cutoff = chrono::Utc::now().timestamp_millis() - RETENTION_DAYS * 86400 * 1000;
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM tool_metrics WHERE timestamp_ms < ?1", params![cutoff])
            .map_err(|e| format!("purge tool: {e}"))?;
        conn.execute("DELETE FROM context_usage WHERE timestamp_ms < ?1", params![cutoff])
            .map_err(|e| format!("purge context: {e}"))?;
        conn.execute("DELETE FROM prompt_structure WHERE timestamp_ms < ?1", params![cutoff])
            .map_err(|e| format!("purge prompt: {e}"))?;
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_store_and_retrieve_tool_metrics() {
        let store = MetricStore::in_memory().unwrap();
        let points = vec![ToolMetricPoint {
            tool_id: ToolId("fs_read".into()),
            turn_number: 1,
            session_id: "s1".into(),
            timestamp_ms: 1000,
            adoption_rate: 0.8,
            success_rate: 0.95,
            trend: 0.1,
            composite_score: 0.75,
            visibility_tier: "A".into(),
            opportunities: 10,
            invocations: 8,
            successes: 7,
            avg_latency_ms: 50.0,
        }];
        store.record_tool_metrics(&points).await.unwrap();

        let history = store.load_tool_history("fs_read", 10).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].adoption_rate, 0.8);
    }

    #[tokio::test]
    async fn test_context_usage() {
        let store = MetricStore::in_memory().unwrap();
        let point = ContextUsagePoint {
            turn_number: 1, session_id: "s1".into(), timestamp_ms: 1000,
            usage_pct: 65.0, max_tokens: 128_000, current_tokens: 83_200,
            was_compressed: false,
        };
        store.record_context_usage(&point).await.unwrap();

        let history = store.load_context_history(10).await.unwrap();
        assert_eq!(history.len(), 1);
        assert!((history[0].usage_pct - 65.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_prompt_structure() {
        let store = MetricStore::in_memory().unwrap();
        let point = PromptStructurePoint {
            turn_number: 1, session_id: "s1".into(), timestamp_ms: 1000,
            layer_count: 6, tool_count: 14, tool_set_hash: 0xABCD,
            has_thinking: true,
        };
        store.record_prompt_structure(&point).await.unwrap();

        let history = store.load_prompt_history(10).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].layer_count, 6);
    }

    #[tokio::test]
    async fn test_cross_tool_query() {
        let store = MetricStore::in_memory().unwrap();
        store.record_tool_metrics(&[
            ToolMetricPoint {
                tool_id: ToolId("fs_read".into()), turn_number: 1, session_id: "s1".into(),
                timestamp_ms: 1000, adoption_rate: 0.8, success_rate: 0.9, trend: 0.0,
                composite_score: 0.7, visibility_tier: "A".into(),
                opportunities: 10, invocations: 8, successes: 7, avg_latency_ms: 10.0,
            },
            ToolMetricPoint {
                tool_id: ToolId("web_fetch".into()), turn_number: 1, session_id: "s1".into(),
                timestamp_ms: 1000, adoption_rate: 0.3, success_rate: 0.6, trend: -0.1,
                composite_score: 0.4, visibility_tier: "B".into(),
                opportunities: 10, invocations: 3, successes: 2, avg_latency_ms: 500.0,
            },
        ]).await.unwrap();

        let tools = store.list_tracked_tools().await.unwrap();
        assert_eq!(tools.len(), 2);
    }
}
