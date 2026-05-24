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
pub mod memory_palace;
pub mod knowledge_store;
pub mod paths;
pub mod process_registry;
pub mod deduction;
pub mod auto;
pub mod sandbox;
pub mod lsp;
pub mod undo;

// Local additions (historically in abacus-core facade)
pub mod config;
pub mod secrets;
pub mod validation;

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
