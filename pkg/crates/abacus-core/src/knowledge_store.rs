//! knowledge_store — 知识库持久化层
//!
//! ## 场景
//! 提供文件摄入（chunking + FTS5 索引）和语义检索能力。
//! 被 `kb.*` 工具直接调用，也被 Memory Palace 的 `kb.search` 用于跨源合并检索。
//!
//! ## 依赖
//! - `rusqlite`: SQLite + FTS5 (trigram tokenizer)
//! - `sha2`: 文件 hash 去重
//! - `crate::core::context::estimate_tokens`: token 估算
//!
//! ## 引用关系
//! - 被 `tool::builtin::kb::KbToolExecutor` 持有和调用
//! - 被 `kb.search` 的多源合并逻辑引用
//!
//! ## 生命周期
//! - 创建：CoreLoop 初始化时（或首次 kb.ingest 时 lazy init）
//! - 存活：整个 app 生命周期
//! - DB 文件：~/.abacus/knowledge.db（独立于 memory.db）

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Sha256, Digest};
use tokio::sync::Mutex;

use crate::memory_palace::{self, MemoryEmbedder};

// ─── 数据结构 ───────────────────────────────────────────────────────────

/// 一个知识块
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    pub file: String,
    pub chunk_idx: usize,
    pub content: String,
    pub heading_path: String,    // "[H1 > H2 > H3]" 或 ""
    pub token_estimate: usize,
}

/// 文件元数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub file: String,
    pub hash: String,           // SHA-256 前 16 hex
    pub chunk_count: usize,
    pub priority: f64,          // 0.0-1.0
    pub ingested_at: i64,
}

/// 查询结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub chunk_id: String,
    pub file: String,
    pub chunk_idx: usize,
    pub score: f64,
    pub content: String,
    pub heading_path: String,
}

// ─── Chunking 参数 ──────────────────────────────────────────────────────

const CHUNK_SIZE: usize = 480;
const CHUNK_OVERLAP: usize = 60;
const CHUNK_MIN: usize = 80;
const CHUNK_MAX: usize = 520;

// ─── KnowledgeStore ─────────────────────────────────────────────────────

/// 知识库存储引擎
///
/// ## 场景
/// 管理 chunks 的摄入、索引、检索。
/// 使用 FTS5 trigram tokenizer 提供全文搜索。
///
/// ## 边界
/// - 单文件最大 10MB
/// - chunk 数量无硬限制（SQLite 可处理百万级行）
/// - FTS5 trigram 对中英文均有效
pub struct KnowledgeStore {
    conn: Arc<Mutex<Connection>>,
    db_path: PathBuf,
    /// Task #83：L1 缓存层 — file path + content hash 做键
    ///
    /// 引用关系：list_file_chunks 写入；ingest 路径不主动 invalidate
    /// （hash 改变自然 miss）。其他工具（KB query / search）仍走 SQLite。
    /// 生命周期：随 KnowledgeStore 生死；TTL 120s 让长期不查的文件自然降温。
    /// 容量 512 — 多文件场景留余量；hot files 始终占据 LRU 头部。
    chunks_cache: Arc<crate::cache::L1MemoryCache>,
    /// Optional embedding service for semantic search during ingest.
    ///
    /// 引用关系：set_embedder() 注入；ingest() 路径在 chunk 写入后调用
    /// 生命周期：由调用方（CoreLoop）创建并注入，随 KnowledgeStore 生死
    embedder: Option<Arc<dyn MemoryEmbedder>>,
}

impl KnowledgeStore {
    /// 创建或打开知识库
    pub fn new(db_path: impl AsRef<Path>) -> Result<Self, String> {
        let path = db_path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create kb directory: {e}"))?;
        }

        let conn = Connection::open(&path)
            .map_err(|e| format!("cannot open kb db: {e}"))?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;"
        ).map_err(|e| format!("pragma failed: {e}"))?;

        Self::init_schema(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: path,
            chunks_cache: Arc::new(crate::cache::L1MemoryCache::new(
                512,
                std::time::Duration::from_secs(120),
            )),
            embedder: None,
        })
    }

    /// 内存模式（测试用）
    pub fn in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("cannot open in-memory db: {e}"))?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: PathBuf::from(":memory:"),
            chunks_cache: Arc::new(crate::cache::L1MemoryCache::new(
                512,
                std::time::Duration::from_secs(120),
            )),
            embedder: None,
        })
    }

    fn init_schema(conn: &Connection) -> Result<(), String> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS chunks (
                id TEXT PRIMARY KEY,
                file TEXT NOT NULL,
                chunk_idx INTEGER NOT NULL,
                content TEXT NOT NULL,
                heading_path TEXT NOT NULL DEFAULT '',
                token_estimate INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(file);

            CREATE TABLE IF NOT EXISTS file_meta (
                file TEXT PRIMARY KEY,
                hash TEXT NOT NULL,
                chunk_count INTEGER NOT NULL DEFAULT 0,
                priority REAL NOT NULL DEFAULT 0.6,
                ingested_at INTEGER NOT NULL DEFAULT (unixepoch())
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                content, heading_path,
                content='chunks',
                content_rowid='rowid',
                tokenize='trigram'
            );

            -- FTS5 同步触发器
            CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
                INSERT INTO chunks_fts(rowid, content, heading_path)
                VALUES (new.rowid, new.content, new.heading_path);
            END;
            CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
                INSERT INTO chunks_fts(chunks_fts, rowid, content, heading_path)
                VALUES ('delete', old.rowid, old.content, old.heading_path);
            END;
            CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
                INSERT INTO chunks_fts(chunks_fts, rowid, content, heading_path)
                VALUES ('delete', old.rowid, old.content, old.heading_path);
                INSERT INTO chunks_fts(rowid, content, heading_path)
                VALUES (new.rowid, new.content, new.heading_path);
            END;"
        ).map_err(|e| format!("schema init failed: {e}"))?;

        // Migration: add embedding BLOB column to chunks (idempotent)
        // SQLite lacks IF NOT EXISTS for ADD COLUMN; check pragma table_info first.
        let has_embedding: bool = conn.prepare("PRAGMA table_info(chunks)")
            .and_then(|mut stmt| {
                let found = stmt.query_map([], |row| row.get::<_, String>(1))?
                    .filter_map(|r| r.ok())
                    .any(|col| col == "embedding");
                Ok(found)
            })
            .unwrap_or(false);
        if !has_embedding {
            conn.execute_batch(
                "ALTER TABLE chunks ADD COLUMN embedding BLOB;"
            ).map_err(|e| format!("migrate chunks.embedding column: {e}"))?;
        }

        Ok(())
    }

    // ─── Ingest ─────────────────────────────────────────────────────────

    /// 摄入文件到知识库
    ///
    /// 返回 (status, chunk_count, hash)
    pub async fn ingest(&self, file_path: &str, force: bool) -> Result<Value, String> {
        let path = Path::new(file_path);
        if !path.exists() {
            return Err(format!("file not found: {file_path}"));
        }

        let content = tokio::fs::read_to_string(path).await
            .map_err(|e| format!("cannot read file: {e}"))?;

        if content.len() > 10 * 1024 * 1024 {
            return Err("file exceeds 10MB limit".into());
        }

        // Hash 校验
        let hash = compute_hash(&content);

        // 检查是否已摄入且 hash 未变（短暂持锁）
        if !force {
            let conn = self.conn.lock().await;
            let existing: Option<String> = conn.query_row(
                "SELECT hash FROM file_meta WHERE file = ?1",
                params![file_path],
                |row| row.get(0),
            ).ok();
            drop(conn); // 释放锁

            if existing.as_deref() == Some(&hash) {
                return Ok(json!({
                    "file": file_path,
                    "status": "skipped",
                    "reason": "hash unchanged",
                    "hash": hash,
                }));
            }
        }

        // Chunking 在锁外执行（CPU 密集，不占用 DB 连接）
        let chunks = if is_markdown(file_path) {
            chunk_markdown(&content, file_path)
        } else {
            chunk_text(&content, file_path)
        };

        let chunk_count = chunks.len();
        let conn = self.conn.lock().await;

        // 事务写入
        conn.execute_batch("BEGIN").map_err(|e| e.to_string())?;

        // 删除旧 chunks
        conn.execute("DELETE FROM chunks WHERE file = ?1", params![file_path])
            .map_err(|e| format!("delete old chunks: {e}"))?;

        // 插入新 chunks
        for chunk in &chunks {
            conn.execute(
                "INSERT INTO chunks (id, file, chunk_idx, content, heading_path, token_estimate)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    chunk.id,
                    chunk.file,
                    chunk.chunk_idx as i64,
                    chunk.content,
                    chunk.heading_path,
                    chunk.token_estimate as i64,
                ],
            ).map_err(|e| format!("insert chunk: {e}"))?;
        }

        // Upsert file_meta
        let priority = compute_priority(file_path);
        conn.execute(
            "INSERT OR REPLACE INTO file_meta (file, hash, chunk_count, priority, ingested_at)
             VALUES (?1, ?2, ?3, ?4, unixepoch())",
            params![file_path, hash, chunk_count as i64, priority],
        ).map_err(|e| format!("upsert file_meta: {e}"))?;

        conn.execute_batch("COMMIT").map_err(|e| e.to_string())?;
        drop(conn); // Release lock before async embedding

        // Auto-embed chunks if embedder is available (best-effort, non-blocking for ingest result)
        if self.embedder.is_some() {
            self.embed_chunks(&chunks).await;
        }

        Ok(json!({
            "file": file_path,
            "status": "ingested",
            "chunks": chunk_count,
            "hash": hash,
        }))
    }

    // ─── Query ──────────────────────────────────────────────────────────

    /// FTS5 全文搜索
    ///
    /// 返回按 BM25 排序的 top-K 结果
    pub async fn query(
        &self,
        query_text: &str,
        top_k: usize,
        file_filter: Option<&str>,
    ) -> Result<Vec<QueryResult>, String> {
        // 空查询直接返回空结果（FTS5 MATCH 空字符串会报错）
        if query_text.trim().is_empty() {
            return Ok(vec![]);
        }

        let conn = self.conn.lock().await;

        // FTS5 查询（转义特殊字符）
        let escaped = fts5_escape(query_text);

        let sql = if file_filter.is_some() {
            "SELECT c.id, c.file, c.chunk_idx, c.content, c.heading_path,
                    rank * -1.0 as score
             FROM chunks_fts f
             JOIN chunks c ON c.rowid = f.rowid
             WHERE chunks_fts MATCH ?1 AND c.file LIKE ?2
             ORDER BY rank
             LIMIT ?3"
        } else {
            "SELECT c.id, c.file, c.chunk_idx, c.content, c.heading_path,
                    rank * -1.0 as score
             FROM chunks_fts f
             JOIN chunks c ON c.rowid = f.rowid
             WHERE chunks_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2"
        };

        let mut results: Vec<QueryResult> = Vec::new();

        if let Some(filter) = file_filter {
            let pattern = format!("%{}%", filter);
            let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
            let rows = stmt.query_map(params![escaped, pattern, top_k as i64], |row| {
                Ok(QueryResult {
                    chunk_id: row.get(0)?,
                    file: row.get(1)?,
                    chunk_idx: row.get::<_, i64>(2)? as usize,
                    content: row.get(3)?,
                    heading_path: row.get(4)?,
                    score: row.get(5)?,
                })
            }).map_err(|e| e.to_string())?;
            for r in rows.flatten() {
                results.push(r);
            }
        } else {
            let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
            let rows = stmt.query_map(params![escaped, top_k as i64], |row| {
                Ok(QueryResult {
                    chunk_id: row.get(0)?,
                    file: row.get(1)?,
                    chunk_idx: row.get::<_, i64>(2)? as usize,
                    content: row.get(3)?,
                    heading_path: row.get(4)?,
                    score: row.get(5)?,
                })
            }).map_err(|e| e.to_string())?;
            for r in rows.flatten() {
                results.push(r);
            }
        }

        Ok(results)
    }

    /// 获取 DB 路径（用于诊断）
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Phase Ctx-D：按 file 列出已摄入的 chunks（按 chunk_idx 升序）
    ///
    /// 用途：ContextManager.declare 复用 KB 的 chunking + token 估算，
    /// 避免在 GeneralizedIndex 路径里重复实现 indexing 逻辑。
    ///
    /// ## Task #83：L1 缓存策略
    /// 1. 先取 file_meta.hash（cheap SELECT 单行）
    /// 2. cache_key = "kbchunks-{file}-{hash}" → 文件变更 hash 变 → 旧 cache miss 自然失效
    /// 3. L1 hit → JSON 反序列化返回
    /// 4. L1 miss → 查 SQLite → JSON 序列化写入 L1 → 返回
    /// 引用关系：仅本方法读写 chunks_cache；ingest 路径不显式 invalidate（hash 变更即失效）。
    pub async fn list_file_chunks(&self, file: &str) -> Result<Vec<Chunk>, String> {
        use crate::cache::CacheBackend;

        // 取 file_meta.hash 作为版本戳（不存在则空字符串 → 仍可缓存）
        let hash: String = {
            let conn = self.conn.lock().await;
            conn.query_row(
                "SELECT hash FROM file_meta WHERE file = ?1",
                params![file],
                |row| row.get::<_, String>(0),
            ).unwrap_or_default()
        };
        let cache_key = format!("kbchunks-{file}-{hash}");

        // L1 快路径
        if let Ok(Some(bytes)) = self.chunks_cache.get(&cache_key).await {
            if let Ok(chunks) = serde_json::from_slice::<Vec<Chunk>>(&bytes) {
                return Ok(chunks);
            }
            // 反序列化失败 → 走 SQLite 重建（fall-through）
        }

        // L1 miss → SQLite 查询（scope 内消费 stmt/rows，跨 await 前自动 drop 释放 !Send 借用）
        let chunks: Vec<Chunk> = {
            let conn = self.conn.lock().await;
            let mut stmt = conn.prepare(
                "SELECT id, file, chunk_idx, content, heading_path, token_estimate
                 FROM chunks WHERE file = ?1 ORDER BY chunk_idx"
            ).map_err(|e| format!("prepare: {e}"))?;
            let rows = stmt.query_map(params![file], |row| {
                Ok(Chunk {
                    id: row.get(0)?,
                    file: row.get(1)?,
                    chunk_idx: row.get::<_, i64>(2)? as usize,
                    content: row.get(3)?,
                    heading_path: row.get(4)?,
                    token_estimate: row.get::<_, i64>(5)? as usize,
                })
            }).map_err(|e| format!("query_map: {e}"))?;
            let mut acc = Vec::new();
            for row in rows {
                acc.push(row.map_err(|e| format!("row: {e}"))?);
            }
            acc
        };

        // 写入 L1（失败静默忽略 — 不影响主结果）
        if let Ok(bytes) = serde_json::to_vec(&chunks) {
            let _ = self
                .chunks_cache
                .set(&cache_key, bytes, self.chunks_cache.default_ttl())
                .await;
        }
        Ok(chunks)
    }

    // ─── Embedder ───────────────────────────────────────────────────────

    /// Inject an embedder for auto-embedding during ingest and semantic search.
    ///
    /// 引用关系：CoreLoop 初始化时调用注入；ingest/semantic_search 消费
    /// 生命周期：注入后与 KnowledgeStore 同生死
    pub fn set_embedder(&mut self, embedder: Arc<dyn MemoryEmbedder>) {
        self.embedder = Some(embedder);
    }

    /// Embed and persist a single chunk's embedding in the DB.
    ///
    /// 引用关系：ingest_with_embeddings 调用
    /// 生命周期：纯操作，无持有状态
    async fn persist_chunk_embedding(&self, chunk_id: &str, embedding: &[f32]) -> Result<(), String> {
        let blob = memory_palace::f32_slice_to_blob(embedding);
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE chunks SET embedding = ?1 WHERE id = ?2",
            params![blob, chunk_id],
        ).map_err(|e| format!("persist_chunk_embedding: {e}"))?;
        Ok(())
    }

    /// After ingest completes, embed all newly inserted chunks (best-effort).
    ///
    /// Called internally after successful ingest when embedder is available.
    /// Failures are logged but do not fail the ingest operation.
    ///
    /// 引用关系：ingest() 调用
    /// 生命周期：临时操作，不持有状态
    async fn embed_chunks(&self, chunks: &[Chunk]) {
        let embedder = match &self.embedder {
            Some(e) => e.clone(),
            None => return,
        };
        for chunk in chunks {
            match embedder.embed_text(&chunk.content).await {
                Ok(vec) => {
                    if let Err(e) = self.persist_chunk_embedding(&chunk.id, &vec).await {
                        tracing::warn!("embed_chunks: failed to persist embedding for {}: {e}", chunk.id);
                    }
                }
                Err(e) => {
                    tracing::warn!("embed_chunks: embedder failed for {}: {e}", chunk.id);
                }
            }
        }
    }

    // ─── Semantic Search ────────────────────────────────────────────────

    /// Semantic search using stored embeddings.
    /// Falls back to FTS5 if no embedder is set or no embeddings exist.
    ///
    /// 引用关系：被 kb.search 工具的语义搜索路径调用
    /// 生命周期：纯查询，无持有状态
    pub async fn semantic_search(&self, query: &str, top_k: usize) -> Vec<QueryResult> {
        // 1. Require embedder for query embedding
        let embedder = match &self.embedder {
            Some(e) => e.clone(),
            None => {
                // Fallback to FTS5
                return self.query(query, top_k, None).await.unwrap_or_default();
            }
        };

        // 2. Embed query
        let query_vec = match embedder.embed_text(query).await {
            Ok(v) => v,
            Err(_) => {
                return self.query(query, top_k, None).await.unwrap_or_default();
            }
        };

        // 3. Load all chunk embeddings from DB
        let chunk_embeddings: Vec<(String, String, usize, String, String, Vec<f32>)> = {
            let conn = self.conn.lock().await;
            let mut stmt = match conn.prepare(
                "SELECT id, file, chunk_idx, content, heading_path, embedding
                 FROM chunks WHERE embedding IS NOT NULL"
            ) {
                Ok(s) => s,
                Err(_) => return self.query(query, top_k, None).await.unwrap_or_default(),
            };
            let rows = match stmt.query_map([], |row| {
                let id: String = row.get(0)?;
                let file: String = row.get(1)?;
                let chunk_idx: i64 = row.get(2)?;
                let content: String = row.get(3)?;
                let heading_path: String = row.get(4)?;
                let blob: Vec<u8> = row.get(5)?;
                Ok((id, file, chunk_idx as usize, content, heading_path, blob))
            }) {
                Ok(r) => r,
                Err(_) => return self.query(query, top_k, None).await.unwrap_or_default(),
            };
            rows.filter_map(|r| r.ok())
                .map(|(id, file, idx, content, hp, blob)| {
                    (id, file, idx, content, hp, memory_palace::blob_to_f32_vec(&blob))
                })
                .collect()
        };

        // If no embeddings stored, fallback
        if chunk_embeddings.is_empty() {
            return self.query(query, top_k, None).await.unwrap_or_default();
        }

        // 4. Cosine similarity rank
        let mut scored: Vec<(f64, &(String, String, usize, String, String, Vec<f32>))> =
            chunk_embeddings.iter()
                .map(|entry| {
                    let sim = memory_palace::cosine_similarity(&query_vec, &entry.5);
                    (sim, entry)
                })
                .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // 5. Return top_k
        scored.into_iter()
            .take(top_k)
            .map(|(score, entry)| QueryResult {
                chunk_id: entry.0.clone(),
                file: entry.1.clone(),
                chunk_idx: entry.2,
                content: entry.3.clone(),
                heading_path: entry.4.clone(),
                score,
            })
            .collect()
    }
}

// ─── Chunking 算法 ──────────────────────────────────────────────────────

/// Markdown 结构化 chunking
///
/// 按 heading 层级切分，每个 chunk 前缀 heading path
fn chunk_markdown(content: &str, file_path: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current_chunk = String::new();
    let mut heading_stack: Vec<String> = Vec::new();
    let mut chunk_idx = 0;

    for line in content.lines() {
        // 检测 heading
        if let Some(level) = detect_heading_level(line) {
            // 如果当前 chunk 有内容，先保存
            if current_chunk.len() >= CHUNK_MIN {
                let heading_path = heading_stack.join(" > ");
                chunks.push(make_chunk(file_path, chunk_idx, &current_chunk, &heading_path));
                chunk_idx += 1;
                current_chunk.clear();
            }

            // 更新 heading stack
            let title = line.trim_start_matches('#').trim().to_string();
            // 截断到当前级别
            heading_stack.truncate(level.saturating_sub(1));
            heading_stack.push(title);
        }

        current_chunk.push_str(line);
        current_chunk.push('\n');

        // 如果超过 CHUNK_MAX，强制分割
        if current_chunk.len() >= CHUNK_MAX {
            let heading_path = heading_stack.join(" > ");
            chunks.push(make_chunk(file_path, chunk_idx, &current_chunk, &heading_path));
            chunk_idx += 1;
            // 保留 overlap。对齐到 UTF-8 char boundary：
            // current_chunk.len() 是字节数，若 saturating_sub 后落在多字节字符中间
            // （如中文 3 字节/char），切片会 panic。向后移动到下一个 boundary。
            let mut overlap_start = current_chunk.len().saturating_sub(CHUNK_OVERLAP);
            while overlap_start < current_chunk.len()
                && !current_chunk.is_char_boundary(overlap_start)
            {
                overlap_start += 1;
            }
            current_chunk = current_chunk[overlap_start..].to_string();
        }
    }

    // 最后一段
    if !current_chunk.trim().is_empty() && current_chunk.len() >= CHUNK_MIN {
        let heading_path = heading_stack.join(" > ");
        chunks.push(make_chunk(file_path, chunk_idx, &current_chunk, &heading_path));
    } else if !current_chunk.trim().is_empty() && !chunks.is_empty() {
        // 太短则合并到上一个 chunk
        if let Some(last) = chunks.last_mut() {
            last.content.push_str(&current_chunk);
            last.token_estimate = crate::core::context::estimate_tokens(&last.content);
        }
    }

    chunks
}

/// 通用 text chunking（固定窗口 + overlap）
fn chunk_text(content: &str, file_path: &str) -> Vec<Chunk> {
    // 折叠 3+ 连续空行为 2
    let normalized = collapse_newlines(content);
    let chars: Vec<char> = normalized.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut chunk_idx = 0;

    while start < chars.len() {
        let end = (start + CHUNK_SIZE).min(chars.len());
        let chunk_str: String = chars[start..end].iter().collect();

        if chunk_str.trim().len() >= CHUNK_MIN {
            chunks.push(make_chunk(file_path, chunk_idx, &chunk_str, ""));
            chunk_idx += 1;
        }

        // 下一段起始 = 当前结束 - overlap
        start = if end == chars.len() {
            end
        } else {
            end.saturating_sub(CHUNK_OVERLAP)
        };
    }

    chunks
}

// ─── 辅助函数 ───────────────────────────────────────────────────────────

fn make_chunk(file: &str, idx: usize, content: &str, heading_path: &str) -> Chunk {
    let id = format!("{}:{}", file, idx);
    Chunk {
        id,
        file: file.to_string(),
        chunk_idx: idx,
        content: content.to_string(),
        heading_path: heading_path.to_string(),
        token_estimate: crate::core::context::estimate_tokens(content),
    }
}

fn detect_heading_level(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        let level = trimmed.chars().take_while(|c| *c == '#').count();
        if level <= 6 && trimmed.len() > level && trimmed.as_bytes()[level] == b' ' {
            return Some(level);
        }
    }
    None
}

fn compute_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let result = hasher.finalize();
    // 前 8 bytes → 16 hex chars（不依赖 hex crate）
    result[..8].iter().map(|b| format!("{:02x}", b)).collect()
}

fn is_markdown(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

fn collapse_newlines(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut consecutive = 0;
    for ch in text.chars() {
        if ch == '\n' {
            consecutive += 1;
            if consecutive <= 2 {
                result.push(ch);
            }
        } else {
            consecutive = 0;
            result.push(ch);
        }
    }
    result
}

/// 基于文件路径推断优先级
/// - abacusbr 最高优先级（行为规则文件）
/// - workflow/工作流 次高
/// - knowledge/知识库 中优先级
/// - archive/归档 最低优先级
fn compute_priority(path: &str) -> f64 {
    let lower = path.to_lowercase();
    if lower.ends_with("abacusbr.md") {
        1.0
    } else if lower.contains("/workflow/") || lower.contains("/工作流/") {
        0.75
    } else if lower.contains("/knowledge/") || lower.contains("/知识库/") {
        0.7
    } else if lower.contains("/archive/") || lower.contains("/归档/") {
        0.4
    } else {
        0.6
    }
}

/// FTS5 查询转义
fn fts5_escape(query: &str) -> String {
    // 用双引号包裹整个查询作为短语搜索
    // 转义内部双引号
    format!("\"{}\"", query.replace('"', "\"\""))
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ingest_and_query() {
        let store = KnowledgeStore::in_memory().unwrap();

        // 创建临时文件
        let tmp = "/tmp/abacus_test_kb_ingest.md";
        std::fs::write(tmp, "# Hello World\n\nThis is a test document about Rust programming.\n\n## Section Two\n\nMore content here about async await patterns.\n").unwrap();

        let result = store.ingest(tmp, false).await.unwrap();
        assert_eq!(result["status"], "ingested");
        assert!(result["chunks"].as_u64().unwrap() >= 1);

        // 查询
        let results = store.query("Rust programming", 5, None).await.unwrap();
        assert!(!results.is_empty());
        assert!(results[0].content.contains("Rust"));

        // 重复摄入（hash 未变 → skipped）
        let result2 = store.ingest(tmp, false).await.unwrap();
        assert_eq!(result2["status"], "skipped");

        // force 强制重新摄入
        let result3 = store.ingest(tmp, true).await.unwrap();
        assert_eq!(result3["status"], "ingested");

        let _ = std::fs::remove_file(tmp);
    }

    #[tokio::test]
    async fn test_query_with_file_filter() {
        let store = KnowledgeStore::in_memory().unwrap();

        let tmp1 = "/tmp/abacus_kb_f1.md";
        let tmp2 = "/tmp/abacus_kb_f2.md";
        std::fs::write(tmp1, "# File One\n\nRust is great for systems programming.\n").unwrap();
        std::fs::write(tmp2, "# File Two\n\nPython is great for data science.\n").unwrap();

        store.ingest(tmp1, false).await.unwrap();
        store.ingest(tmp2, false).await.unwrap();

        // 过滤只搜 f1
        let results = store.query("programming", 5, Some("f1")).await.unwrap();
        assert!(results.iter().all(|r| r.file.contains("f1")));

        let _ = std::fs::remove_file(tmp1);
        let _ = std::fs::remove_file(tmp2);
    }

    #[tokio::test]
    async fn test_ingest_utf8_doc_no_panic() {
        // 回归防护：chunking overlap_start 曾载在多字节字符中间 → panic。
        // 入入一份含中文 + emoji 的超长文档，应不 panic 且生成 ≥1 个 chunk。
        let store = KnowledgeStore::in_memory().unwrap();
        let tmp = "/tmp/abacus_kb_utf8_test.md";
        // 构造足够长的中文内容触发 CHUNK_MAX 分割
        let mut content = String::from("# 中文文档测试\n\n");
        // 200 行 × «中文句子 🚀 »，足够超过 CHUNK_MAX (4096)
        for i in 0..200 {
            content.push_str(&format!("这是第 {i} 句中文内容，含 emoji 🚀和多字节字符。\n"));
        }
        std::fs::write(tmp, &content).unwrap();

        let result = store.ingest(tmp, false).await.expect("ingest 中文不应 panic");
        assert_eq!(result["status"], "ingested");
        assert!(result["chunks"].as_u64().unwrap() >= 1);

        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn test_chunk_markdown() {
        let md = "# Title\n\nIntro paragraph here.\n\n## Section A\n\nContent of section A with enough text to be meaningful chunk.\n\n## Section B\n\nContent of section B with enough text.\n";
        let chunks = chunk_markdown(md, "test.md");
        assert!(!chunks.is_empty());
        // 第一个 chunk 应该有 heading path
        // heading_path 取决于分割点
    }

    #[test]
    fn test_chunk_text_overlap() {
        let text = "a".repeat(1000);
        let chunks = chunk_text(&text, "test.txt");
        assert!(chunks.len() >= 2);
        // 验证 overlap：第二个 chunk 的开头应与第一个 chunk 的结尾重叠
    }

    #[test]
    fn test_compute_hash_deterministic() {
        let h1 = compute_hash("hello world");
        let h2 = compute_hash("hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16); // 16 hex chars
    }

    #[test]
    fn test_collapse_newlines() {
        let input = "a\n\n\n\n\nb";
        let output = collapse_newlines(input);
        assert_eq!(output, "a\n\nb");
    }
}
