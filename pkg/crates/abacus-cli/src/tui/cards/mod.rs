//! V42-B 内置卡片 —— 4 种角色卡片
//!
//! ## 架构
//!
//! 每张卡片实现 `abacus_ui_kit::MessageCard` trait, 通过 `CardStream` 管理生命周期。
//!
//! | 卡片 | 角色 | 内容 |
//! |------|------|------|
//! | UserCard   | kinds::USER   | 用户输入文本 |
//! | LlmCard    | kinds::LLM    | thinking + markdown reply |
//! | AbacusCard | kinds::ABACUS | 工具调用 / trace events / EditDiff |
//! | ExpertCard | kinds::EXPERT | 专家身份 + LLM 内容 |
//!
//! ## 依赖
//!
//! - `abacus_ui_kit::prelude::*` 提供核心类型
//! - `crate::tui::state::{TraceEvent, TraceKind, ToolStatus}` 提供 trace 数据
//! - `crate::tui::markdown` 提供 markdown → Line 转换
//! - `crate::tui::md_stream` 提供增量 markdown 解析 (LlmCard)

pub mod abacus;
pub mod expert;
pub mod hit_test;
pub mod llm;
pub mod render;
pub mod user;
pub mod writer;

pub use abacus::AbacusCard;
#[allow(unused_imports)] // 仅 v42b_card_stream.rs example 使用
pub use expert::ExpertCard;
pub use llm::LlmCard;
pub use user::UserCard;
