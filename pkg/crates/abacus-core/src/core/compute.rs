//! Compute — async discipline layer
//!
//! Pre-compiled regex patterns (OnceLock) + CPU offload helper.
//! Prevents blocking the tokio runtime with heavy computation.

use std::sync::OnceLock;
use regex::Regex;

/// Pre-compiled patterns for code file indexing.
pub struct CodePatterns {
    pub fn_def: Regex,
    pub struct_def: Regex,
    pub impl_def: Regex,
    pub trait_def: Regex,
    pub enum_def: Regex,
    pub mod_def: Regex,
    pub type_def: Regex,
}

/// Pre-compiled patterns for text/markdown indexing.
pub struct TextPatterns {
    pub heading: Regex,
    pub paragraph_split: Regex,
}

static CODE_PATTERNS: OnceLock<CodePatterns> = OnceLock::new();
static TEXT_PATTERNS: OnceLock<TextPatterns> = OnceLock::new();

pub fn code_patterns() -> &'static CodePatterns {
    CODE_PATTERNS.get_or_init(|| CodePatterns {
        fn_def: Regex::new(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)\s*[<(]").expect("BUG: invalid literal regex fn_def"),
        struct_def: Regex::new(r"^\s*(?:pub\s+)?struct\s+(\w+)").expect("BUG: invalid literal regex struct_def"),
        impl_def: Regex::new(r"^\s*(?:pub\s+)?(?:unsafe\s+)?impl(?:<[^>]*>)?\s+(\w+)").expect("BUG: invalid literal regex impl_def"),
        trait_def: Regex::new(r"^\s*(?:pub\s+)?(?:unsafe\s+)?trait\s+(\w+)").expect("BUG: invalid literal regex trait_def"),
        enum_def: Regex::new(r"^\s*(?:pub\s+)?enum\s+(\w+)").expect("BUG: invalid literal regex enum_def"),
        mod_def: Regex::new(r"^\s*(?:pub\s+)?mod\s+(\w+)").expect("BUG: invalid literal regex mod_def"),
        type_def: Regex::new(r"^\s*(?:pub\s+)?type\s+(\w+)").expect("BUG: invalid literal regex type_def"),
    })
}

pub fn text_patterns() -> &'static TextPatterns {
    TEXT_PATTERNS.get_or_init(|| TextPatterns {
        heading: Regex::new(r"^(#{1,6})\s+(.+)").expect("BUG: invalid literal regex heading"),
        paragraph_split: Regex::new(r"\n\s*\n").expect("BUG: invalid literal regex paragraph_split"),
    })
}

/// Offload CPU-intensive work to blocking thread pool.
/// Use for: file indexing, large JSON serialization, regex-heavy parsing.
pub async fn offload<F, T>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .expect("BUG: blocking task panicked")
}
