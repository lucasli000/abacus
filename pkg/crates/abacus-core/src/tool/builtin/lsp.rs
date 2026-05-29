//! LSP built-in tools — LLM 可调用的代码智能工具
//!
//! ## 注册工具（9个）
//! | Tool | Description |
//! |------|-------------|
//! | lsp.goto_definition | 跳转到符号定义位置 |
//! | lsp.find_references | 查找所有引用 |
//! | lsp.hover | 获取类型信息和文档注释 |
//! | lsp.document_symbol | 列出文件中所有符号 |
//! | lsp.workspace_symbol | 跨文件符号搜索 |
//! | lsp.diagnostics | 获取文件错误/警告 |
//! | lsp.goto_implementation | 跳转到 trait/接口实现 |
//! | lsp.call_hierarchy_incoming | 查找调用者（上游） |
//! | lsp.call_hierarchy_outgoing | 查找被调用函数（下游） |
//!
//! ## 依赖
//! - `crate::lsp::LspManager`: 管理语言服务器连接
//!
//! ## 引用关系
//! - 被 `builtin::register_all()` + `register_lsp_executors()` 调用注册
//! - `LspManager` 由 `CoreLoop` 在初始化时创建并注入
//!
//! ## 生命周期
//! - 注册：CoreLoop::new() 阶段
//! - 执行：每次 LLM 调用工具时

use std::sync::Arc;

use abacus_types::{
    ToolCost, ToolEffectiveness, ToolHandle, ToolId,
    ToolProvider, ToolSchema, ToolSecurity, ToolState, VisibilityTier,
};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::lsp::LspManager;
use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};

// ─── Schema 注册 ────────────────────────────────────────────────────────────

/// 注册 LSP 工具 schema（不含 executor，executor 在 register_lsp_executors 中注入）
pub async fn register(registry: &ToolRegistry) {
    let tools: &[(&str, &str, Value)] = &[
        (
            "lsp_goto_definition",
            // V29.13: 原 description 155 字节超 schema_lint warn threshold (150). 缩到 124 字节
            // 语义对 LLM 函数选择无影响 — tool description 只作上下文提示
            "Jump to a symbol's definition. Returns file path, line, char. Use to locate where a function/struct/variable is defined.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "Absolute or relative path to the source file"},
                    "line":      {"type": "integer", "description": "Line number (1-based)"},
                    "character": {"type": "integer", "description": "Character offset (1-based)"}
                },
                "required": ["file_path", "line", "character"]
            }),
        ),
        (
            "lsp_find_references",
            "Find all references to a symbol across the workspace. Returns list of {file, line, character}. Use to understand where a function/type is used.",
            json!({
                "type": "object",
                "properties": {
                    "file_path":           {"type": "string"},
                    "line":                {"type": "integer", "description": "Line number (1-based)"},
                    "character":           {"type": "integer", "description": "Character offset (1-based)"},
                    "include_declaration": {"type": "boolean", "description": "Include the symbol declaration itself (default: true)"}
                },
                "required": ["file_path", "line", "character"]
            }),
        ),
        (
            "lsp_hover",
            // V29.13: 原 152 字节 > schema_lint warn (150). 缩到 130
            "Get type info and docs for a symbol at a position. Returns type signature and doc comments. Faster than reading file to infer types.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "line":      {"type": "integer", "description": "Line number (1-based)"},
                    "character": {"type": "integer", "description": "Character offset (1-based)"}
                },
                "required": ["file_path", "line", "character"]
            }),
        ),
        (
            "lsp_document_symbol",
            // V29.13: 原 169 字节 > 150. 缩到 138
            "List all symbols (functions, structs, classes, vars) in a file with kinds and line numbers. Use to understand file structure without full read.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "Path to the source file"}
                },
                "required": ["file_path"]
            }),
        ),
        (
            "lsp_workspace_symbol",
            // V29.13: 原 170 字节 > 150. 缩到 130
            "Search symbols by name across workspace. Returns matches with file paths and line numbers. Use to locate a function/type without its file.",
            json!({
                "type": "object",
                "properties": {
                    "query":         {"type": "string", "description": "Symbol name or partial name to search"},
                    "language_hint": {"type": "string", "description": "Language server to use (rust/typescript/python/go etc.), defaults to rust"}
                },
                "required": ["query"]
            }),
        ),
        (
            "lsp_diagnostics",
            "Get compiler errors and warnings for a file. Returns list of {severity, line, message}. Use to check if a file has errors after making changes.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "Path to the source file to check"}
                },
                "required": ["file_path"]
            }),
        ),
        (
            "lsp_goto_implementation",
            // V29.13: 原 169 字节 > 150. 缩到 132
            "Find all implementations of a trait or interface. Returns list of {file, line, character}. Use to jump from a trait def to implementations.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "Absolute or relative path to the source file"},
                    "line":      {"type": "integer", "description": "Line number (1-based)"},
                    "character": {"type": "integer", "description": "Character offset (1-based)"}
                },
                "required": ["file_path", "line", "character"]
            }),
        ),
        (
            "lsp_call_hierarchy_incoming",
            "Find all callers of a function — who calls this function. Returns list of {name, file, line}. Use to trace upstream call chains.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "line":      {"type": "integer", "description": "Line number (1-based)"},
                    "character": {"type": "integer", "description": "Character offset (1-based)"}
                },
                "required": ["file_path", "line", "character"]
            }),
        ),
        (
            "lsp_call_hierarchy_outgoing",
            "Find all functions called by a function — what does this function call. Returns list of {name, file, line}. Use to trace downstream execution flow.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "line":      {"type": "integer", "description": "Line number (1-based)"},
                    "character": {"type": "integer", "description": "Character offset (1-based)"}
                },
                "required": ["file_path", "line", "character"]
            }),
        ),
    ];

    for (name, desc, params) in tools {
        registry.register(ToolHandle {
            id: ToolId(name.to_string()),
            schema: ToolSchema {
                name: name.to_string(),
                description: desc.to_string(),
                parameters: params.clone(),
                returns: None,
                security: Some(ToolSecurity {
                    confirm_required: false,
                    allowed_paths: None,
                    max_size_mb: None,
                    needs_sandbox: false,
                }),
                cost: Some(ToolCost { tokens: 64, latency: "200ms".into(), risk: "low".into() }),
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: true,
                                schema_stable: false,            },
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness {
                tool_id: ToolId(name.to_string()),
                composite_score: 0.75,
                tier: VisibilityTier::A,
                cooldown_remaining: 0,
                blocked_by_env: false,
                insufficient_data: true,
            },
        }).await;
    }
}

/// LspManager 注入后注册 executor（含 LspManager 引用）
pub async fn register_executors(registry: &ToolRegistry, lsp: Arc<LspManager>, workspace_root: String) {
    let exec = Arc::new(LspToolExecutor { lsp, workspace_root });
    let tool_ids = [
        "lsp_goto_definition",
        "lsp_find_references",
        "lsp_hover",
        "lsp_document_symbol",
        "lsp_workspace_symbol",
        "lsp_diagnostics",
        "lsp_goto_implementation",
        "lsp_call_hierarchy_incoming",
        "lsp_call_hierarchy_outgoing",
    ];
    for id in &tool_ids {
        registry.register_executor(ToolId(id.to_string()), exec.clone()).await;
    }
}

// ─── Executor ───────────────────────────────────────────────────────────────

pub struct LspToolExecutor {
    lsp: Arc<LspManager>,
    workspace_root: String,
}

#[async_trait]
impl ToolExecutor for LspToolExecutor {
    async fn execute(&self, tool_id: &ToolId, params: Value, _ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        match self.dispatch(&tool_id.0, &params).await {
            Ok(output) => Ok(output),
            Err(e) => Ok(json!({ "error": e, "success": false })),
        }
    }
}

impl LspToolExecutor {
    async fn dispatch(&self, tool: &str, params: &Value) -> Result<Value, String> {
        let root = &self.workspace_root;

        match tool {
            "lsp_goto_definition" => {
                let file = str_param(params, "file_path")?;
                let line = u32_param(params, "line")?.saturating_sub(1); // 0-based
                let char = u32_param(params, "character")?.saturating_sub(1);
                self.lsp.goto_definition(file, line, char, root).await
            }
            "lsp_find_references" => {
                let file = str_param(params, "file_path")?;
                let line = u32_param(params, "line")?.saturating_sub(1);
                let char = u32_param(params, "character")?.saturating_sub(1);
                let incl = params.get("include_declaration")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                self.lsp.find_references(file, line, char, incl, root).await
            }
            "lsp_hover" => {
                let file = str_param(params, "file_path")?;
                let line = u32_param(params, "line")?.saturating_sub(1);
                let char = u32_param(params, "character")?.saturating_sub(1);
                self.lsp.hover(file, line, char, root).await
            }
            "lsp_document_symbol" => {
                let file = str_param(params, "file_path")?;
                self.lsp.document_symbol(file, root).await
            }
            "lsp_workspace_symbol" => {
                let query = str_param(params, "query")?;
                let lang = params.get("language_hint").and_then(|v| v.as_str());
                self.lsp.workspace_symbol(query, root, lang).await
            }
            "lsp_diagnostics" => {
                let file = str_param(params, "file_path")?;
                self.lsp.diagnostics(file, root).await
            }
            "lsp_goto_implementation" => {
                let file = str_param(params, "file_path")?;
                let line = u32_param(params, "line")?.saturating_sub(1);
                let char = u32_param(params, "character")?.saturating_sub(1);
                self.lsp.goto_implementation(file, line, char, root).await
            }
            "lsp_call_hierarchy_incoming" => {
                let file = str_param(params, "file_path")?;
                let line = u32_param(params, "line")?.saturating_sub(1);
                let char = u32_param(params, "character")?.saturating_sub(1);
                self.lsp.call_hierarchy_incoming(file, line, char, root).await
            }
            "lsp_call_hierarchy_outgoing" => {
                let file = str_param(params, "file_path")?;
                let line = u32_param(params, "line")?.saturating_sub(1);
                let char = u32_param(params, "character")?.saturating_sub(1);
                self.lsp.call_hierarchy_outgoing(file, line, char, root).await
            }
            other => Err(format!("unknown LSP tool: {other}")),
        }
    }
}

// ─── Parameter helpers ──────────────────────────────────────────────────────

fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, String> {
    params.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing required parameter '{key}'"))
}

fn u32_param(params: &Value, key: &str) -> Result<u32, String> {
    params.get(key)
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .ok_or_else(|| format!("missing required parameter '{key}' (integer)"))
}
