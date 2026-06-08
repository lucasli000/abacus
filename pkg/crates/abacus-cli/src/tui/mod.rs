//! Abacus TUI — 统一终端交互入口
//!
//! 设计规范来源: ABACUS-TUI-DESIGN-SPEC.md v1.0
//!
//! 入口: `cargo run --bin tui_main`
//!
//! 支持 Clarify / Plan / Team / Meeting 四种模式切换。
//! 快捷键参考: Ctrl+C 退出, Esc 暂停, Tab 切换焦点, Ctrl+B 看板, Ctrl+Enter 发送。
//!
//! V42: theme + Section trait 已迁至 `abacus-ui-kit` crate（跨 crate 公开契约）。
//! 内部 `crate::tui::theme::*` 路径已全部改为 `abacus_ui_kit::*`。

pub mod api;
pub mod cards;
pub mod clipboard;
pub mod components;
pub mod cost;
pub mod i18n;
pub mod effects;
pub mod event;
pub mod extensions;
pub mod layout;
pub mod markdown;
pub mod md_stream;
pub mod expert_config;
pub mod meeting_cache;
pub mod modes;
pub mod run;
pub mod setup;
pub mod slash_commands;
pub mod state;
pub mod syntax;
pub mod theme;
pub mod util;
