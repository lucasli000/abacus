//! Built-in tool implementations.
//!
//! Each sub-module registers one or more tools in the global [`ToolRegistry`].
//! New tools should be added here and wired into [`register_all`].
//!
//! ## Registered Tools
//!
//! | Module | Tools | Description |
//! |--------|-------|-------------|
//! | `filengine` | `filengine.*` | File system operations (read/write/search/glob) |
//! | `code_exec` | `code.execute` | Rhai scripting engine for local data ops |
//! | `db` | `db.*` | SQLite database operations (query/CRUD/schema) |
//!
//! ## Dependencies
//!
//! Each sub-module manages its own dependencies. See individual module docs.

pub mod filengine;
pub mod code_exec;
pub mod db;
pub mod kb;
pub mod orchestrate;
pub mod lsp;
pub mod result;

/// Register all built-in tool groups (schemas only for tools needing external deps).
///
/// Called once during `CoreLoop::new()` initialization.
/// Order of registration does not affect runtime behavior.
///
/// Note: kb::register_executors() and lsp::register_executors() must be called
/// separately with their respective manager instances.
pub async fn register_all(registry: &super::ToolRegistry) {
    filengine::register(registry).await;
    // V24 修复：filengine schemas 注册了但 executors 漏注册——LLM 看到 fs.search/fs.ls 等
    // 工具能调用，但 dispatch 时 registry.execute() 找不到 executor → "no executor for tool"
    // FilengineToolExecutor 是无状态的（session 通过 ExecutionContext 动态注入），register_all
    // 阶段一次性绑定即可。
    filengine::register_executors(registry).await;
    code_exec::CodeExecutorTool::register(registry).await;
    db::register(registry).await;
    db::register_executors(registry).await;
    kb::register(registry).await;
    // kb::register_executors() called from CoreLoop::new() with store + palace args
    orchestrate::register(registry).await;
    orchestrate::register_executors(registry).await;
    // Task #85：LSP schema 改为懒注册——CoreLoop::enable_lsp() 时同步注册 schema + executor
    // 不启用 LSP → 10 个 lsp.* schema 都不进 LLM 视野，省 ~1500 tokens/轮
    // lsp::register(registry).await;
    // lsp::register_executors() called via CoreLoop::enable_lsp() after initialization
    result::register(registry).await;
}

#[cfg(test)]
mod schema_lint_invariants {
    //! V29.14: 编译期 (cargo test) 静态检查所有 builtin tool 的 description 长度
    //!
    //! 设计意图：
    //!   - schema_lint Warn threshold = 150 字节
    //!   - 即使 V29.14 已让 Warn 不再默认 panic, 我们仍想让"超阈值"在 PR 阶段被发现
    //!   - 该测试在 cargo test 时跑, 任何新加的 builtin tool 超 150 字节会立刻 fail
    //!   - 真正的"漂移"窗口收窄到"绕过 cargo test merge" — 走 CI 的话不可能
    //!
    //! 历史:
    //!   - V29.13 用户启动 TUI 撞 lsp.goto_definition 描述 155 字节 panic
    //!   - 根因之一是 lint 阈值偏严; 之二是没有静态校验, 漂移漏到 runtime
    //!   - V29.14 双修: lint Warn 不 default-panic + 此测试堵住未来漂移源
    use super::*;
    use crate::tool::ToolRegistry;

    /// schema_lint::DescTooLong 配置的 warn_at 阈值(150). 与该常量同步即可
    const DESC_WARN_BYTES: usize = 150;

    #[tokio::test]
    async fn all_builtin_descriptions_under_warn_threshold() {
        let registry = ToolRegistry::new();
        register_all(&registry).await;
        // LSP 默认懒注册, 强制注册以覆盖
        crate::tool::builtin::lsp::register(&registry).await;

        let tools = registry.all_tools().await;
        let mut violators: Vec<(String, usize, String)> = Vec::new();
        for h in &tools {
            let len = h.schema.description.len();
            if len > DESC_WARN_BYTES {
                let preview: String = h.schema.description.chars().take(60).collect();
                violators.push((h.id.0.clone(), len, preview));
            }
        }
        if !violators.is_empty() {
            let mut msg = format!(
                "{} builtin tool description(s) exceed {} bytes (schema_lint warn):\n",
                violators.len(), DESC_WARN_BYTES
            );
            for (id, len, preview) in &violators {
                msg.push_str(&format!("  - {} [{} bytes] {}...\n", id, len, preview));
            }
            msg.push_str("Trim each to ≤150 bytes (one sentence). See V29.13 lsp.rs for examples.");
            panic!("{}", msg);
        }
    }
}