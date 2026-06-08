//! Dashboard Tabs —— 仪表盘可注册 tab 实现集
//!
//! V42 引入：把 `extras.rs::render_shortcuts_hints` 内的 ~160 行渲染拆为独立 DashboardTab,
//! 每个 tab 实现 [`abacus_ui_kit::DashboardTab`] trait, 通过 [`DashboardRegistry`] 注册。
//!
//! ## 内置 tab 清单
//!
//! | id | 文件 | 内容 |
//! |---|---|---|
//! | `health` | [`health::HealthTab`] | ScriptHook 触发统计（当前占位, 待对接 hook_stats）|
//! | `auto` | [`auto::AutoTab`] | JobRunner 健康状态 + 任务列表 + uptime |
//!
//! ## 注册顺序
//!
//! [`crate::tui::extensions::register_builtin_dashboard_tabs`] 按 `["health", "auto"]` 顺序注册。
//! 用户可通过 config.toml `[tui.dashboard] tabs = [...]` 自定义启用列表。

pub mod auto;
pub mod health;

pub use auto::AutoTab;
pub use health::HealthTab;
