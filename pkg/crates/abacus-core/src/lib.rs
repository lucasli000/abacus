//! Abacus — L1 Core Layer (merged with former L2 Engine)
//!
//! This crate contains the canonical core logic, engine implementations,
//! configuration, and secrets management. It replaces the historical
//! split between `abacus-core` (L1 facade) and `abacus-engine` (L2
//! implementation) which created a reverse dependency.
//!
//! ## Architecture
//! ```text
//! abacus-types (L0)
//!       ↓
//! abacus-core  (L1+L2) — CoreLoop, ToolRegistry, LLM providers, Config, Secrets
//!       ↓
//! abacus-orchestrator (L3) — Team, SubAgent, Plan, Meeting
//!       ↓
//! abacus-cli / abacus-server (L4)
//! ```

pub mod cache;
pub mod core;
pub use core::context::register_context_tools;
pub mod llm;
pub mod tool;
pub mod skill;
pub mod capability;
pub mod mcp;
pub mod mcip;
pub mod code_exec;
pub mod mag_chain;
pub mod script_hook;
pub mod memory_palace;
pub mod knowledge_store;
pub mod knowledge_extractor;
pub mod vllm_embedder;
pub mod local_provider;
pub mod paths;
pub mod process_registry;
pub mod deduction;
pub mod auto;
pub mod agent;
pub mod sandbox;
pub mod lsp;
pub mod code_graph;
pub mod undo;

// P2-P3: 智能化升级新子系统
pub mod reasoning;    // Self-Consistency (B3) + Tree of Thoughts (B2)
pub mod optimization; // OPRO Prompt优化器 (B1)
pub mod feedback;     // MT-GRPO 轨迹收集器 (B4)

// Local additions (historically in abacus-core facade)
pub mod config;
pub mod secrets;
pub mod validation;

/// SQLite PRAGMA 兼容层（rusqlite 0.32+）
///
/// rusqlite 0.32 起 `execute_batch` 不允许返回结果集的语句（如 PRAGMA journal_mode）。
/// 此模块提供 `apply_standard_pragmas()` 作为统一替代。
pub mod db_util {
    use rusqlite::Connection;

    /// 应用标准 WAL 性能 PRAGMA（兼容 rusqlite 0.32+）
    pub fn apply_standard_pragmas(conn: &Connection) -> Result<(), rusqlite::Error> {
        let pragmas = [
            "PRAGMA journal_mode=WAL",
            "PRAGMA synchronous=NORMAL",
            "PRAGMA busy_timeout=5000",
        ];
        for sql in pragmas {
            let mut stmt = conn.prepare(sql)?;
            let _ = stmt.raw_execute();
        }
        Ok(())
    }
}

// Canonical re-exports for downstream consumers — Core types
pub use core::CoreLoop;
pub use core::SessionState;
pub use core::CoreConfig;
pub use core::TurnResult;
pub use core::RequestContext;
pub use core::prompt_assembly::PromptAssembly;
pub use core::safety::SafetyGuard;
pub use core::safety::SafetyViolation;

// Context management
pub use core::context::ContextManager;
pub use core::context::SessionStore;
pub use core::context::SessionSnapshot;

// Tooling & capabilities
pub use tool::ToolRegistry;
pub use skill::SkillEngine;
pub use capability::CapabilityHub;

// LLM provider types
pub use llm::LlmProvider;
pub use llm::LlmRequest;
pub use llm::LlmResponse;
pub use llm::Message;
pub use llm::MessageContent;
pub use llm::MessageRole;
// L1：ThinkingConfig + ThinkingType 已删除，统一用 abacus_types::ThinkingIntent
pub use llm::ToolDefinition;
pub use llm::ToolFunctionSpec;
pub use llm::TokenUsage;
pub use llm::CachedSegment;
pub use llm::noop_provider::NoApiKeyProvider;

// Configuration & secrets
pub use config::ConfigManager;
pub use secrets::SecretsManager;
pub use secrets::SecretString;
pub use secrets::SecretType;
