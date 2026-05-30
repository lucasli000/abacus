//! 语言分析器抽象层 — 可插拔多语言支持
//!
//! ## 职责
//! 定义 `LanguageAnalyzer` trait，新语言只需实现此 trait 即可接入 CodeGraph。
//! 管理语言注册表（Language → Box<dyn LanguageAnalyzer>）。
//!
//! ## 依赖
//! - `tree-sitter`: AST 解析框架
//!
//! ## 引用关系
//! - 被 `Indexer::parallel_parse()` 在并行解析时调用
//! - 被 `CodeGraphManager::new()` 初始化注册表
//!
//! ## 设计约束
//! - 所有 LanguageAnalyzer 实现必须 Send + Sync（并行调用）
//! - extract_* 方法必须处理部分解析（tree-sitter ERROR 节点）
//! - 不持有 Parser 状态——Parser 由 Indexer 按线程管理

pub mod go;
pub mod python;
pub mod rust_lang;
pub mod typescript;

use std::collections::HashMap;

use super::{FileDep, ImplRelation, Symbol};

// ─── Language 枚举 ─────────────────────────────────────────────────────────

/// 支持的编程语言
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Go,
}

impl Language {
    /// 从文件扩展名推断语言
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "ts" | "tsx" => Some(Self::TypeScript),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "py" | "pyi" => Some(Self::Python),
            "go" => Some(Self::Go),
            _ => None,
        }
    }

    /// 语言标识字符串
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Go => "go",
        }
    }

    /// 该语言的文件扩展名列表
    pub fn extensions(&self) -> &[&str] {
        match self {
            Self::Rust => &["rs"],
            Self::TypeScript => &["ts", "tsx"],
            Self::JavaScript => &["js", "jsx", "mjs", "cjs"],
            Self::Python => &["py", "pyi"],
            Self::Go => &["go"],
        }
    }
}

// ─── 分析结果类型 ──────────────────────────────────────────────────────────

/// 解析结果——成功数据 + 错误统计
///
/// 设计意图：不因部分解析失败丢弃全部结果。
/// tree-sitter 的 ERROR 节点不会阻止其他节点的解析，
/// 所以一个有语法错误的文件仍能产出有效符号。
#[derive(Debug, Clone)]
pub struct AnalysisResult<T> {
    /// 成功提取的数据
    pub data: T,
    /// tree-sitter ERROR 节点数量
    pub parse_errors: u32,
    /// 错误节点的字节范围（用于诊断）
    pub error_ranges: Vec<(usize, usize)>,
}

impl<T: Default> Default for AnalysisResult<T> {
    fn default() -> Self {
        Self {
            data: T::default(),
            parse_errors: 0,
            error_ranges: Vec::new(),
        }
    }
}

impl<T> AnalysisResult<T> {
    pub fn ok(data: T) -> Self {
        Self { data, parse_errors: 0, error_ranges: Vec::new() }
    }

    pub fn with_errors(data: T, errors: u32, ranges: Vec<(usize, usize)>) -> Self {
        Self { data, parse_errors: errors, error_ranges: ranges }
    }
}

/// 原始调用记录（callee 尚未解析为 symbol_id）
///
/// Indexer 在 commit 阶段将 callee_name 解析为 symbol_id：
/// 1. 精确匹配：同文件内 name 完全匹配
/// 2. 路径匹配：module::func 模式
/// 3. 未解析：记录为 unresolved（不丢弃，后续索引新文件时可能解析成功）
#[derive(Debug, Clone)]
pub struct RawCall {
    /// 调用者的 symbol_id
    pub caller_id: String,
    /// 被调用者名称（可能含路径如 "module::func" 或 "obj.method"）
    pub callee_name: String,
    /// 调用所在行
    pub call_site_line: u32,
    /// 调用所在列
    pub call_site_col: u32,
}

// ─── LanguageAnalyzer trait ────────────────────────────────────────────────

/// 语言分析器 trait — 新语言实现此 trait 即可零改动接入
///
/// ## 实现约束
/// - **Send + Sync**：并行索引时跨线程调用
/// - **无状态**：不持有 tree-sitter Parser（由 Indexer 管理）
/// - **部分解析**：遇到 ERROR 节点时继续提取已成功解析的部分
/// - **可重入**：同一文件可能被重复调用（增量索引时）
///
/// ## 实现指南
/// 1. 使用 `tree_sitter::Tree` 遍历 AST
/// 2. 通过 `node.kind()` 匹配目标节点类型
/// 3. 提取节点文本作为符号名/签名
/// 4. 生成确定性 symbol_id（sha1(file + name + kind + line)）
pub trait LanguageAnalyzer: Send + Sync {
    /// 该分析器处理的语言
    fn language(&self) -> Language;

    /// 从 AST 提取所有符号
    ///
    /// 遍历 tree-sitter 语法树，提取函数/类型/trait/接口等符号定义。
    /// 返回的 Symbol.id 必须确定性（相同输入 → 相同 ID）。
    fn extract_symbols(
        &self,
        tree: &tree_sitter::Tree,
        source: &[u8],
        file_path: &str,
    ) -> AnalysisResult<Vec<Symbol>>;

    /// 从 AST 提取函数调用关系
    ///
    /// 返回 RawCall 列表（callee_name 为字符串，由 Indexer 后续解析为 symbol_id）。
    fn extract_calls(
        &self,
        tree: &tree_sitter::Tree,
        source: &[u8],
        file_path: &str,
    ) -> AnalysisResult<Vec<RawCall>>;

    /// 从 AST 提取文件级依赖（import/use/require）
    fn extract_deps(
        &self,
        tree: &tree_sitter::Tree,
        source: &[u8],
        file_path: &str,
    ) -> AnalysisResult<Vec<FileDep>>;

    /// 从 AST 提取 trait/interface 实现关系
    fn extract_impls(
        &self,
        tree: &tree_sitter::Tree,
        source: &[u8],
        file_path: &str,
    ) -> AnalysisResult<Vec<ImplRelation>>;
}

// ─── 注册表 ───────────────────────────────────────────────────────────────

/// 创建语言分析器注册表（所有支持的语言）
///
/// 被 CodeGraphManager::new() 调用一次。
/// 返回的 HashMap 被 Arc 包装后共享给并行 Indexer。
pub fn create_analyzer_registry() -> HashMap<Language, Box<dyn LanguageAnalyzer>> {
    let mut registry: HashMap<Language, Box<dyn LanguageAnalyzer>> = HashMap::new();

    registry.insert(Language::Rust, Box::new(rust_lang::RustAnalyzer::new()));
    registry.insert(Language::TypeScript, Box::new(typescript::TypeScriptAnalyzer::new()));
    registry.insert(Language::JavaScript, Box::new(typescript::TypeScriptAnalyzer::new())); // JS 复用 TS parser
    registry.insert(Language::Python, Box::new(python::PythonAnalyzer::new()));
    registry.insert(Language::Go, Box::new(go::GoAnalyzer::new()));

    registry
}

/// 获取某语言的 tree-sitter Language 对象
pub fn tree_sitter_language(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::TypeScript | Language::JavaScript => {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        }
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
    }
}

// ─── 辅助函数 ─────────────────────────────────────────────────────────────

/// 生成确定性 symbol_id
///
/// 算法：sha1(file + ":" + name + ":" + kind + ":" + line)
/// 保证相同位置的相同符号永远产生相同 ID。
pub fn make_symbol_id(file: &str, name: &str, kind: &str, line: u32) -> String {
    use sha1::{Sha1, Digest};
    let input = format!("{file}:{name}:{kind}:{line}");
    let hash = Sha1::digest(input.as_bytes());
    format!("{:x}", hash)
}

/// 计算文件内容的 sha1 hash（用于增量索引判断）
pub fn file_content_hash(content: &[u8]) -> String {
    use sha1::{Sha1, Digest};
    let hash = Sha1::digest(content);
    format!("{:x}", hash)
}

/// 从 tree-sitter Node 提取文本
pub fn node_text<'a>(node: &tree_sitter::Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// 统计 tree 中的 ERROR 节点
pub fn count_errors(tree: &tree_sitter::Tree) -> (u32, Vec<(usize, usize)>) {
    let mut errors = 0u32;
    let mut ranges = Vec::new();
    let mut cursor = tree.walk();

    fn walk_errors(cursor: &mut tree_sitter::TreeCursor, errors: &mut u32, ranges: &mut Vec<(usize, usize)>) {
        loop {
            let node = cursor.node();
            if node.is_error() || node.is_missing() {
                *errors += 1;
                ranges.push((node.start_byte(), node.end_byte()));
            }
            if cursor.goto_first_child() {
                walk_errors(cursor, errors, ranges);
                cursor.goto_parent();
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    if cursor.goto_first_child() {
        walk_errors(&mut cursor, &mut errors, &mut ranges);
    }

    (errors, ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_from_extension() {
        assert_eq!(Language::from_extension("rs"), Some(Language::Rust));
        assert_eq!(Language::from_extension("ts"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("tsx"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("py"), Some(Language::Python));
        assert_eq!(Language::from_extension("go"), Some(Language::Go));
        assert_eq!(Language::from_extension("java"), None);
    }

    #[test]
    fn test_make_symbol_id_deterministic() {
        let id1 = make_symbol_id("src/main.rs", "main", "function", 10);
        let id2 = make_symbol_id("src/main.rs", "main", "function", 10);
        assert_eq!(id1, id2);

        // 不同行号 → 不同 ID
        let id3 = make_symbol_id("src/main.rs", "main", "function", 11);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_file_content_hash() {
        let h1 = file_content_hash(b"fn main() {}");
        let h2 = file_content_hash(b"fn main() {}");
        assert_eq!(h1, h2);

        let h3 = file_content_hash(b"fn main() { println!() }");
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_analyzer_registry_has_all_languages() {
        let registry = create_analyzer_registry();
        assert!(registry.contains_key(&Language::Rust));
        assert!(registry.contains_key(&Language::TypeScript));
        assert!(registry.contains_key(&Language::JavaScript));
        assert!(registry.contains_key(&Language::Python));
        assert!(registry.contains_key(&Language::Go));
    }
}
