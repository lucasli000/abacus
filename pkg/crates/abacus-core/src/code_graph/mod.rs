//! CodeGraph — 代码知识图谱引擎
//!
//! ## 职责
//! 提供多语言代码分析能力：符号索引、调用图、依赖图、结构分析。
//! 基于 tree-sitter 进行 AST 解析，SQLite FTS5 提供全文搜索。
//!
//! ## 架构
//! ```text
//! CodeGraphManager (入口)
//!   ├── Indexer (并行索引引擎)
//!   │     └── LanguageAnalyzer trait (可插拔语言解析器)
//!   │           ├── RustAnalyzer
//!   │           ├── TypeScriptAnalyzer
//!   │           ├── PythonAnalyzer
//!   │           └── GoAnalyzer
//!   ├── QueryEngine (FTS5 搜索 + 图遍历)
//!   ├── AnalyzeEngine (impact/cycles/coupling)
//!   └── FileWatcherManager (增量索引触发)
//! ```
//!
//! ## 依赖 (external)
//! - `tree-sitter`: AST 解析框架
//! - `tree-sitter-{rust,typescript,python,go}`: 语言 grammar
//! - `rusqlite`: 共享 knowledge.db（FTS5 + WAL）
//! - `rayon`: 并行文件解析
//! - `notify`: 文件系统监听
//! - `sha1`: 文件/符号内容指纹
//!
//! ## 依赖 (internal)
//! - `crate::knowledge_store::KnowledgeStore`: 共享 DB 连接
//!
//! ## 引用关系
//! - 被 `tool/builtin/cg.rs` 通过 ToolExecutor 调用
//! - 被 `CoreLoop::enable_code_graph()` 初始化
//!
//! ## 生命周期
//! - 创建：`CoreLoop::enable_code_graph(workspace)` 时
//! - 存活：与 CoreLoop 同生命周期
//! - 销毁：CoreLoop drop 时，FileWatcher 优雅停止

pub mod analyze;
pub mod indexer;
pub mod lang;
pub mod query;
pub mod schema;
pub mod watcher;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::{Mutex, RwLock};

use self::indexer::{IndexReport, IndexStrategy, Indexer};
use self::query::QueryEngine;
use self::analyze::AnalyzeEngine;
use self::watcher::FileWatcherManager;

// ─── 公共类型 ──────────────────────────────────────────────────────────────

/// 符号类型枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Interface,
    Class,
    Module,
    Constant,
    TypeAlias,
    Macro,
    Variable,
    Field,
    EnumVariant,
}

impl SymbolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Interface => "interface",
            Self::Class => "class",
            Self::Module => "module",
            Self::Constant => "constant",
            Self::TypeAlias => "type_alias",
            Self::Macro => "macro",
            Self::Variable => "variable",
            Self::Field => "field",
            Self::EnumVariant => "enum_variant",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "function" => Some(Self::Function),
            "method" => Some(Self::Method),
            "struct" => Some(Self::Struct),
            "enum" => Some(Self::Enum),
            "trait" => Some(Self::Trait),
            "interface" => Some(Self::Interface),
            "class" => Some(Self::Class),
            "module" => Some(Self::Module),
            "constant" => Some(Self::Constant),
            "type_alias" => Some(Self::TypeAlias),
            "macro" => Some(Self::Macro),
            "variable" => Some(Self::Variable),
            "field" => Some(Self::Field),
            "enum_variant" => Some(Self::EnumVariant),
            _ => None,
        }
    }
}

/// 可见性
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Visibility {
    Public,
    PublicCrate,
    Private,
    Protected, // Python/TS/Go
}

impl Visibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Public => "pub",
            Self::PublicCrate => "pub(crate)",
            Self::Private => "private",
            Self::Protected => "protected",
        }
    }
}

/// 符号记录（从源码提取后的结构化表示）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Symbol {
    pub id: String,
    pub name: String,
    pub kind: SymbolKind,
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub end_line: Option<u32>,
    pub signature: Option<String>,
    pub doc_comment: Option<String>,
    pub visibility: Visibility,
    pub parent_id: Option<String>,
    pub hash: String,
}

/// 文件级依赖关系
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileDep {
    pub source_file: String,
    pub target_file: String,
    pub dep_kind: DepKind,
}

/// 依赖类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DepKind {
    Use,         // Rust use, TS import, Python import, Go import
    Module,      // Rust mod, TS namespace
    Impl,        // trait/interface implementation
    TraitBound,  // generic bounds
    Inherit,     // Python/TS class inheritance
}

impl DepKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Use => "use",
            Self::Module => "mod",
            Self::Impl => "impl",
            Self::TraitBound => "trait_bound",
            Self::Inherit => "inherit",
        }
    }
}

/// Trait/Interface 实现关系
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImplRelation {
    pub trait_name: String,
    pub impl_name: String,
    pub impl_file: String,
    pub impl_line: u32,
}

/// 检索结果质量信号
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub enum CgDegradation {
    /// 索引新鲜，结果完整
    Normal,
    /// 文件已修改但未重新索引（返回旧数据 + 警告）
    StaleIndex,
    /// 部分文件 tree-sitter 解析失败
    PartialParse,
    /// 目标文件/目录从未索引
    NoIndex,
}

impl CgDegradation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::StaleIndex => "StaleIndex",
            Self::PartialParse => "PartialParse",
            Self::NoIndex => "NoIndex",
        }
    }
}

// ─── CodeGraphManager ──────────────────────────────────────────────────────

/// CodeGraph 管理器 — 统一入口
///
/// ## 职责
/// 协调 Indexer/QueryEngine/AnalyzeEngine/FileWatcher 四大子系统。
///
/// ## 线程安全
/// 所有字段均 Arc 包装，支持跨 tokio task 共享。
///
/// ## 生命周期
/// 由 CoreLoop::enable_code_graph() 创建，与 CoreLoop 同生命周期。
pub struct CodeGraphManager {
    /// 共享 DB 连接（knowledge.db，含 cg_* 表）
    /// 保留供未来直接 schema migration / raw query 使用
    #[allow(dead_code)]
    db: Arc<Mutex<Connection>>,
    /// 工作空间根目录
    workspace: PathBuf,
    /// 索引引擎
    indexer: Arc<Indexer>,
    /// 查询引擎
    query_engine: Arc<QueryEngine>,
    /// 分析引擎
    analyze_engine: Arc<AnalyzeEngine>,
    /// 文件监听管理器（可选，仅长 daemon 模式启用）
    watcher: Option<Arc<FileWatcherManager>>,
    /// 当前索引策略（保留供 set_strategy() API 使用）
    #[allow(dead_code)]
    strategy: RwLock<IndexStrategy>,
}

impl CodeGraphManager {
    /// 创建 CodeGraphManager
    ///
    /// 流程：
    /// 1. 确保 cg_* 表存在（migration）
    /// 2. 初始化语言分析器注册表
    /// 3. 创建 Indexer/QueryEngine/AnalyzeEngine
    ///
    /// ## 参数
    /// - `db`: 共享 knowledge.db 连接
    /// - `workspace`: 项目根目录
    pub async fn new(db: Arc<Mutex<Connection>>, workspace: impl AsRef<Path>) -> Result<Self, String> {
        let workspace = workspace.as_ref().to_path_buf();

        // 确保 DB schema 就绪
        {
            let conn = db.lock().await;
            schema::ensure_tables(&conn).map_err(|e| format!("CodeGraph schema init failed: {e}"))?;
        }

        // 初始化语言分析器
        let analyzers = lang::create_analyzer_registry();

        let indexer = Arc::new(Indexer::new(
            db.clone(),
            Arc::new(analyzers),
            workspace.clone(),
        ));
        let query_engine = Arc::new(QueryEngine::new(db.clone()));
        let analyze_engine = Arc::new(AnalyzeEngine::new(db.clone()));

        let default_strategy = IndexStrategy::GitDiff {
            workspace: workspace.clone(),
            base_ref: "HEAD".into(),
        };

        Ok(Self {
            db,
            workspace,
            indexer,
            query_engine,
            analyze_engine,
            watcher: None,
            strategy: RwLock::new(default_strategy),
        })
    }

    /// 执行索引（委托给 Indexer）
    pub async fn index(&self, strategy: IndexStrategy) -> Result<IndexReport, String> {
        self.indexer.index(strategy).await
    }

    /// 启用文件监听模式（长 daemon）
    pub async fn enable_watcher(&mut self, debounce_ms: u64) -> Result<(), String> {
        let watcher = Arc::new(FileWatcherManager::new(
            self.indexer.clone(),
            std::time::Duration::from_millis(debounce_ms),
        ));
        watcher.start(&self.workspace).await?;
        self.watcher = Some(watcher);
        Ok(())
    }

    /// 停止文件监听
    pub async fn stop_watcher(&self) {
        if let Some(ref w) = self.watcher {
            w.stop().await;
        }
    }

    /// 是否有已索引的文件
    pub async fn has_indexed_files(&self) -> bool {
        self.query_engine.has_any_symbols().await
    }

    /// 获取查询引擎引用
    pub fn query(&self) -> &QueryEngine {
        &self.query_engine
    }

    /// 获取分析引擎引用
    pub fn analyze(&self) -> &AnalyzeEngine {
        &self.analyze_engine
    }

    /// 获取工作空间路径
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_kind_roundtrip() {
        let kinds = [
            SymbolKind::Function, SymbolKind::Method, SymbolKind::Struct,
            SymbolKind::Enum, SymbolKind::Trait, SymbolKind::Interface,
            SymbolKind::Class, SymbolKind::Module, SymbolKind::Constant,
            SymbolKind::TypeAlias, SymbolKind::Macro, SymbolKind::Variable,
            SymbolKind::Field, SymbolKind::EnumVariant,
        ];
        for kind in kinds {
            assert_eq!(SymbolKind::from_str(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn test_symbol_kind_from_unknown() {
        assert_eq!(SymbolKind::from_str("unknown_thing"), None);
    }
}
