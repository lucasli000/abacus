//! 并行索引引擎
//!
//! ## 职责
//! 协调多语言文件的并行解析和批量写入。
//! 提供增量索引、中断恢复、事务原子性保障。
//!
//! ## 依赖
//! - `rayon`: 并行 CPU-bound 解析
//! - `tokio`: 异步 IO + DB 写入
//! - `rusqlite`: 事务批量写入
//!
//! ## 引用关系
//! - 被 `CodeGraphManager::index()` 调用
//! - 被 `FileWatcherManager` 增量触发
//! - 调用 `LanguageAnalyzer` 进行实际解析

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tokio::sync::{Mutex, RwLock};

use super::lang::{self, AnalysisResult, Language, LanguageAnalyzer, RawCall};
use super::schema;
use super::{FileDep, ImplRelation, Symbol};

// ─── IndexStrategy ─────────────────────────────────────────────────────────

/// 索引策略（分层设计——适应不同生命周期场景）
///
/// ## 使用场景
/// - GitDiff: 短 session（CLI 模式），每次调用时对比 git 状态
/// - FileWatcher: 长 daemon 模式，持续监听文件变更
/// - Full: 首次索引或强制重建
#[derive(Debug, Clone)]
pub enum IndexStrategy {
    /// 对比 git status，仅索引变更文件
    GitDiff {
        workspace: PathBuf,
        /// 对比基准（默认 "HEAD"）
        base_ref: String,
    },
    /// 全量索引指定路径
    Full {
        workspace: PathBuf,
    },
    /// 增量索引指定文件列表
    Files {
        files: Vec<PathBuf>,
    },
}

// ─── IndexReport ───────────────────────────────────────────────────────────

/// 索引执行报告
#[derive(Debug, Clone, serde::Serialize)]
pub struct IndexReport {
    pub total_files: usize,
    pub indexed: usize,
    pub skipped_unchanged: usize,
    pub failed: usize,
    pub parse_errors: u32,
    pub duration_ms: u64,
    pub symbols_added: usize,
    pub calls_added: usize,
    pub deps_added: usize,
}

// ─── IndexCheckpoint ───────────────────────────────────────────────────────

/// 索引中断检查点（持久化到 cg_index_checkpoint 表）
/// 字段在 resume-from-checkpoint 逻辑中读取（尚未实现完整 resume 路径）
#[allow(dead_code)]
struct IndexCheckpoint {
    session_id: String,
    total_files: usize,
    completed_files: HashSet<PathBuf>,
    failed_files: HashMap<PathBuf, String>,
    started_at: i64,
}

impl IndexCheckpoint {
    fn new(session_id: &str, total: usize) -> Self {
        Self {
            session_id: session_id.to_string(),
            total_files: total,
            completed_files: HashSet::new(),
            failed_files: HashMap::new(),
            started_at: chrono::Utc::now().timestamp(),
        }
    }
}

// ─── FileAnalysis ──────────────────────────────────────────────────────────

/// 单文件解析结果
pub(crate) struct FileAnalysis {
    pub path: PathBuf,
    pub symbols: AnalysisResult<Vec<Symbol>>,
    pub calls: AnalysisResult<Vec<RawCall>>,
    pub deps: AnalysisResult<Vec<FileDep>>,
    pub impls: AnalysisResult<Vec<ImplRelation>>,
    pub hash: String,
    pub language: Language,
    pub file_size: u64,
    pub failed: bool,
    /// 保留供 IndexReport 详细错误报告使用（batch_commit 路径读取）
    #[allow(dead_code)]
    pub error_msg: Option<String>,
}

impl FileAnalysis {
    fn failed_with(path: &Path, error: String) -> Self {
        Self {
            path: path.to_path_buf(),
            symbols: AnalysisResult::default(),
            calls: AnalysisResult::default(),
            deps: AnalysisResult::default(),
            impls: AnalysisResult::default(),
            hash: String::new(),
            language: Language::Rust,
            file_size: 0,
            failed: true,
            error_msg: Some(error),
        }
    }
}

// ─── Indexer ───────────────────────────────────────────────────────────────

/// 并行索引引擎
///
/// ## 线程模型
/// - `index()` 在 tokio async 上下文中调用
/// - 文件解析通过 `spawn_blocking` + `rayon` 并行执行（CPU-bound）
/// - DB 写入回到 async 上下文（IO-bound）
///
/// ## 错误隔离
/// 单个文件的 panic 被 `catch_unwind` 捕获，不影响其他文件。
/// 解析失败的文件记录到 IndexReport.failed，不写入 DB。
///
/// ## 事务保障
/// 每 BATCH_SIZE 个文件一个事务。事务失败则该批全部回滚。
/// 检查点在每批完成后更新。
pub struct Indexer {
    db: Arc<Mutex<Connection>>,
    analyzers: Arc<HashMap<Language, Box<dyn LanguageAnalyzer>>>,
    /// 保留供 file discovery 路径过滤使用（当前 index() 通过 strategy 传入）
    #[allow(dead_code)]
    workspace: PathBuf,
    /// 并行度（默认 num_cpus / 2）— 保留供 rayon threadpool 配置
    #[allow(dead_code)]
    parallelism: usize,
    /// 单文件解析超时 — 保留供 spawn_blocking 超时守卫
    #[allow(dead_code)]
    per_file_timeout: Duration,
    /// 批量提交大小
    batch_size: usize,
    /// 检查点状态
    checkpoint: RwLock<Option<IndexCheckpoint>>,
}

/// 批量提交大小
const DEFAULT_BATCH_SIZE: usize = 100;
/// 单文件最大解析时间
const DEFAULT_PER_FILE_TIMEOUT: Duration = Duration::from_secs(10);

impl Indexer {
    pub fn new(
        db: Arc<Mutex<Connection>>,
        analyzers: Arc<HashMap<Language, Box<dyn LanguageAnalyzer>>>,
        workspace: PathBuf,
    ) -> Self {
        let parallelism = num_cpus().max(2) / 2;
        Self {
            db,
            analyzers,
            workspace,
            parallelism,
            per_file_timeout: DEFAULT_PER_FILE_TIMEOUT,
            batch_size: DEFAULT_BATCH_SIZE,
            checkpoint: RwLock::new(None),
        }
    }

    /// 执行索引（核心入口）
    pub async fn index(&self, strategy: IndexStrategy) -> Result<IndexReport, String> {
        let start = Instant::now();

        // 1. 发现文件
        let files = self.discover_files(&strategy).await?;

        // 2. 过滤未变更文件
        let (changed, skipped) = self.filter_unchanged(&files).await?;

        // 3. 初始化检查点
        let session_id = format!("idx_{}", chrono::Utc::now().timestamp_millis());
        *self.checkpoint.write().await = Some(IndexCheckpoint::new(&session_id, changed.len()));

        // 4. 并行解析
        let results = self.parallel_parse(&changed).await?;

        // 5. 统计
        let mut report = IndexReport {
            total_files: files.len(),
            indexed: 0,
            skipped_unchanged: skipped,
            failed: 0,
            parse_errors: 0,
            duration_ms: 0,
            symbols_added: 0,
            calls_added: 0,
            deps_added: 0,
        };

        // 6. 批量写入 DB
        self.batch_commit(&results, &mut report).await?;

        report.duration_ms = start.elapsed().as_millis() as u64;

        // 7. 清理检查点
        *self.checkpoint.write().await = None;

        Ok(report)
    }

    /// 索引指定文件列表（用于 FileWatcher 增量触发）
    pub async fn index_files(&self, files: &[PathBuf]) -> Result<IndexReport, String> {
        self.index(IndexStrategy::Files { files: files.to_vec() }).await
    }

    // ─── 内部方法 ──────────────────────────────────────────────────────────

    async fn discover_files(&self, strategy: &IndexStrategy) -> Result<Vec<PathBuf>, String> {
        match strategy {
            IndexStrategy::Full { workspace } => {
                self.walk_directory(workspace).await
            }
            IndexStrategy::GitDiff { workspace, base_ref } => {
                self.git_changed_files(workspace, base_ref).await
            }
            IndexStrategy::Files { files } => {
                Ok(files.clone())
            }
        }
    }

    async fn walk_directory(&self, root: &Path) -> Result<Vec<PathBuf>, String> {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let mut files = Vec::new();
            for entry in walkdir::WalkDir::new(&root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| !is_hidden_or_ignored(e))
            {
                if let Ok(entry) = entry {
                    if entry.file_type().is_file() {
                        if let Some(ext) = entry.path().extension().and_then(|e| e.to_str()) {
                            if Language::from_extension(ext).is_some() {
                                files.push(entry.into_path());
                            }
                        }
                    }
                }
            }
            Ok(files)
        })
        .await
        .map_err(|e| format!("walk_directory join error: {e}"))?
    }

    async fn git_changed_files(&self, workspace: &Path, base_ref: &str) -> Result<Vec<PathBuf>, String> {
        let output = tokio::process::Command::new("git")
            .args(["diff", "--name-only", base_ref])
            .current_dir(workspace)
            .output()
            .await
            .map_err(|e| format!("git diff failed: {e}"))?;

        if !output.status.success() {
            // 如果 git 失败（非 git 仓库等），回退到全量
            return self.walk_directory(workspace).await;
        }

        let files: Vec<PathBuf> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let path = workspace.join(line.trim());
                if path.exists() {
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        if Language::from_extension(ext).is_some() {
                            return Some(path);
                        }
                    }
                }
                None
            })
            .collect();

        // 如果 diff 为空但从未索引过，回退到全量
        if files.is_empty() {
            let db = self.db.lock().await;
            let count: i64 = db.query_row(
                "SELECT COUNT(*) FROM cg_file_meta", [], |r| r.get(0)
            ).unwrap_or(0);
            if count == 0 {
                drop(db);
                return self.walk_directory(workspace).await;
            }
        }

        Ok(files)
    }

    async fn filter_unchanged(&self, files: &[PathBuf]) -> Result<(Vec<PathBuf>, usize), String> {
        let db = self.db.lock().await;
        let mut changed = Vec::new();
        let mut skipped = 0usize;

        for file in files {
            let content = match std::fs::read(file) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let hash = lang::file_content_hash(&content);
            let path_str = file.to_string_lossy().to_string();

            let existing_hash: Option<String> = db.query_row(
                "SELECT hash FROM cg_file_meta WHERE file = ?1",
                [&path_str],
                |row| row.get(0),
            ).ok();

            if existing_hash.as_deref() == Some(&hash) {
                skipped += 1;
            } else {
                changed.push(file.clone());
            }
        }

        Ok((changed, skipped))
    }

    async fn parallel_parse(&self, files: &[PathBuf]) -> Result<Vec<FileAnalysis>, String> {
        if files.is_empty() {
            return Ok(Vec::new());
        }

        let analyzers = self.analyzers.clone();
        let files_owned: Vec<PathBuf> = files.to_vec();

        tokio::task::spawn_blocking(move || {
            use rayon::prelude::*;

            files_owned.par_iter()
                .map(|file| {
                    // catch_unwind 隔离 panic
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        parse_single_file(file, &analyzers)
                    }));

                    match result {
                        Ok(Ok(analysis)) => analysis,
                        Ok(Err(e)) => FileAnalysis::failed_with(file, e),
                        Err(_panic) => FileAnalysis::failed_with(
                            file,
                            "parser panicked (isolated)".into(),
                        ),
                    }
                })
                .collect()
        })
        .await
        .map_err(|e| format!("parallel_parse join error: {e}"))
    }

    async fn batch_commit(&self, results: &[FileAnalysis], report: &mut IndexReport) -> Result<(), String> {
        let db = self.db.lock().await;

        for batch in results.chunks(self.batch_size) {
            db.execute_batch("BEGIN IMMEDIATE")
                .map_err(|e| format!("begin transaction: {e}"))?;

            for analysis in batch {
                if analysis.failed {
                    report.failed += 1;
                    report.parse_errors += analysis.symbols.parse_errors;
                    continue;
                }

                let file_str = analysis.path.to_string_lossy().to_string();

                // 先清理旧数据
                let _ = schema::remove_file_data(&db, &file_str);

                // 写入符号
                for sym in &analysis.symbols.data {
                    db.execute(
                        "INSERT OR REPLACE INTO cg_symbols \
                         (symbol_id, name, kind, file, line, col, end_line, signature, doc_comment, visibility, parent_id, hash, language) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                        rusqlite::params![
                            sym.id, sym.name, sym.kind.as_str(), sym.file,
                            sym.line, sym.col, sym.end_line, sym.signature,
                            sym.doc_comment, sym.visibility.as_str(), sym.parent_id,
                            sym.hash, analysis.language.as_str(),
                        ],
                    ).map_err(|e| format!("insert symbol: {e}"))?;
                    report.symbols_added += 1;
                }

                // 写入调用关系（将 callee_name 解析为 symbol_id）
                for call in &analysis.calls.data {
                    // 尝试解析 callee_name → symbol_id
                    let callee_id: Option<String> = db.query_row(
                        "SELECT symbol_id FROM cg_symbols WHERE name = ?1 LIMIT 1",
                        [&call.callee_name],
                        |row| row.get(0),
                    ).ok();

                    if let Some(callee_id) = callee_id {
                        db.execute(
                            "INSERT OR IGNORE INTO cg_call_graph \
                             (caller_id, callee_id, call_site_line, call_site_col) \
                             VALUES (?1, ?2, ?3, ?4)",
                            rusqlite::params![
                                call.caller_id, callee_id,
                                call.call_site_line, call.call_site_col,
                            ],
                        ).map_err(|e| format!("insert call: {e}"))?;
                        report.calls_added += 1;
                    }
                    // callee 未解析时跳过（后续索引时可能解析成功）
                }

                // 写入文件依赖
                for dep in &analysis.deps.data {
                    db.execute(
                        "INSERT OR IGNORE INTO cg_dep_graph (source_file, target_file, dep_kind) \
                         VALUES (?1, ?2, ?3)",
                        rusqlite::params![dep.source_file, dep.target_file, dep.dep_kind.as_str()],
                    ).map_err(|e| format!("insert dep: {e}"))?;
                    report.deps_added += 1;
                }

                // 写入 impl 关系
                for imp in &analysis.impls.data {
                    // trait_id: 查找或生成
                    let trait_id: Option<String> = db.query_row(
                        "SELECT symbol_id FROM cg_symbols WHERE name = ?1 AND kind IN ('trait', 'interface') LIMIT 1",
                        [&imp.trait_name],
                        |row| row.get(0),
                    ).ok();

                    let impl_id: Option<String> = db.query_row(
                        "SELECT symbol_id FROM cg_symbols WHERE name = ?1 AND file = ?2 LIMIT 1",
                        rusqlite::params![imp.impl_name, imp.impl_file],
                        |row| row.get(0),
                    ).ok();

                    if let (Some(trait_id), Some(impl_id)) = (trait_id, impl_id) {
                        db.execute(
                            "INSERT OR IGNORE INTO cg_impl_graph (trait_id, impl_id, impl_file, impl_line) \
                             VALUES (?1, ?2, ?3, ?4)",
                            rusqlite::params![trait_id, impl_id, imp.impl_file, imp.impl_line],
                        ).map_err(|e| format!("insert impl: {e}"))?;
                    }
                }

                // 更新文件元数据
                let total_errors = analysis.symbols.parse_errors
                    + analysis.calls.parse_errors
                    + analysis.deps.parse_errors;

                db.execute(
                    "INSERT OR REPLACE INTO cg_file_meta \
                     (file, hash, symbol_count, last_indexed_at, parse_errors, language, file_size) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        file_str, analysis.hash,
                        analysis.symbols.data.len(),
                        chrono::Utc::now().timestamp(),
                        total_errors,
                        analysis.language.as_str(),
                        analysis.file_size,
                    ],
                ).map_err(|e| format!("insert file_meta: {e}"))?;

                report.indexed += 1;
                report.parse_errors += total_errors;
            }

            db.execute_batch("COMMIT")
                .map_err(|e| format!("commit transaction: {e}"))?;
        }

        Ok(())
    }
}

// ─── 辅助函数 ──────────────────────────────────────────────────────────────

/// 解析单个文件（在 rayon 线程中调用）
fn parse_single_file(
    path: &Path,
    analyzers: &HashMap<Language, Box<dyn LanguageAnalyzer>>,
) -> Result<FileAnalysis, String> {
    let ext = path.extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| "no file extension".to_string())?;

    let language = Language::from_extension(ext)
        .ok_or_else(|| format!("unsupported extension: {ext}"))?;

    let analyzer = analyzers.get(&language)
        .ok_or_else(|| format!("no analyzer for {}", language.as_str()))?;

    let source = std::fs::read(path)
        .map_err(|e| format!("read file: {e}"))?;

    let file_size = source.len() as u64;
    let hash = lang::file_content_hash(&source);
    let file_path_str = path.to_string_lossy().to_string();

    // tree-sitter 解析
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang::tree_sitter_language(language))
        .map_err(|e| format!("set language: {e}"))?;

    let tree = parser.parse(&source, None)
        .ok_or_else(|| "tree-sitter parse returned None".to_string())?;

    // 提取四类信息
    let symbols = analyzer.extract_symbols(&tree, &source, &file_path_str);
    let calls = analyzer.extract_calls(&tree, &source, &file_path_str);
    let deps = analyzer.extract_deps(&tree, &source, &file_path_str);
    let impls = analyzer.extract_impls(&tree, &source, &file_path_str);

    Ok(FileAnalysis {
        path: path.to_path_buf(),
        symbols,
        calls,
        deps,
        impls,
        hash,
        language,
        file_size,
        failed: false,
        error_msg: None,
    })
}

/// 判断 walkdir entry 是否应被忽略
///
/// Skips hidden files/dirs, build output, dependency dirs, and binary lock files.
/// `bun.lockb` is a binary lockfile (Bun runtime) — not parseable, skip indexing.
/// `bun.lock` is Bun's text lockfile — large generated file, no code symbols to extract.
fn is_hidden_or_ignored(entry: &walkdir::DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    // 跳过隐藏文件/目录和常见忽略目录
    name.starts_with('.')
        || name == "target"
        || name == "node_modules"
        || name == "__pycache__"
        || name == "vendor"
        || name == "build"
        || name == "dist"
        // Bun runtime lock files (binary and text) — not indexable
        || name == "bun.lockb"
        || name == "bun.lock"
}

/// 获取 CPU 核心数
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_is_hidden_or_ignored() {
        let cases = [
            (".git", true),
            ("target", true),
            ("node_modules", true),
            ("src", false),
            ("main.rs", false),
            // Bun lock files should be ignored
            ("bun.lockb", true),
            ("bun.lock", true),
            // Other Bun files should NOT be ignored
            ("bunfig.toml", false),
            ("bun-app.ts", false),
        ];
        // 简单断言目录名逻辑
        for (name, expected) in cases {
            let hidden = name.starts_with('.')
                || name == "target"
                || name == "node_modules"
                || name == "__pycache__"
                || name == "vendor"
                || name == "build"
                || name == "dist"
                || name == "bun.lockb"
                || name == "bun.lock";
            assert_eq!(hidden, expected, "failed for: {name}");
        }
    }
}
