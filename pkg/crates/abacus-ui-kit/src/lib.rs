//! # abacus-ui-kit
//!
//! Abacus TUI 公开契约 crate —— 提供主题、可组合 Section、可注册仪表盘 tab 的跨 crate 扩展能力。
//!
//! ## 设计定位
//!
//! 这是 Agent 应用与 abacus TUI 之间的**唯一公开 UI 边界**。
//!
//! - `abacus-types` 保持纯数据契约（零 UI 依赖）
//! - `abacus-server` 不依赖本 crate（HTTP 不需要 UI）
//! - `abacus-cli` 持有完整 TUI 实现，依赖本 crate 的 trait 与 Theme
//! - 第三方 Agent 应用通过本 crate 注入自定义看板/仪表盘
//!
//! ## 公开 API 速查
//!
//! - [`Theme`] + [`TextRole`] + [`SemanticIntent`] + [`Strength`] — 主题色与语义角色
//! - [`Section`] + [`SectionContext`] + [`SectionStack`] + [`SectionRegistry`] — 看板可组合区块
//! - [`DashboardTab`] + [`DashboardRegistry`] — 仪表盘可注册 tab
//! - [`prelude`] — 常用项一次性 import
//!
//! ## 跨 crate 扩展示例
//!
//! ```ignore
//! use abacus_ui_kit::prelude::*;
//!
//! struct MyQuantSection;
//!
//! impl Section for MyQuantSection {
//!     fn id(&self) -> &str { "com.example.quant.positions" }
//!     fn title(&self) -> &str { "持仓盈亏" }
//!     fn min_height(&self) -> u16 { 4 }
//!     fn render(&self, f: &mut ratatui::Frame, ctx: &dyn SectionContext, area: ratatui::layout::Rect) {
//!         let theme = ctx.theme();
//!         // ... 自定义渲染逻辑
//!     }
//! }
//!
//! // 应用启动时
//! let mut registry = SectionRegistry::new();
//! registry.register(Box::new(MyQuantSection));
//! ```

pub mod card;
pub mod card_render;
pub mod card_stream;
pub mod hooks;
pub mod scrollable_stack;
pub mod section;
pub mod theme;

// ── 顶层 re-export ──
pub use card::{default_color_for_kind, CardCollapse, CardHeader, CardHit, CardKind, CardStreaming, MessageCard};
pub use card::kinds;
pub use card_render::{card_total_height, hit_test_card, paint_card_top_shimmer, render_card};
pub use card_stream::CardStream;
pub use scrollable_stack::{ScrollLayout, ScrollPosition, ScrollableStack};
pub use hooks::{PulseGate, ShimmerPhase, SpinnerFrame, SPINNER_FRAMES};
pub use section::{
    DashboardRegistry, DashboardTab, Section, SectionContext, SectionRegistry, SectionStack,
};
pub use theme::{
    ColorCapability, SemanticIntent, Strength, TextRole, Theme,
};
// theme 模块内的子模块 + helper fn 一并 re-export, 兼容老路径用法
pub use theme::{brand, mode_color, z_index};
pub use theme::{ansi256_fallback, from_name, relative_luminance, wcag_contrast};

/// Prelude —— 常用项一次性 import
///
/// ```ignore
/// use abacus_ui_kit::prelude::*;
/// ```
pub mod prelude {
    pub use crate::card::{default_color_for_kind, CardCollapse, CardHeader, CardHit, CardKind, CardStreaming, MessageCard};
    pub use crate::card::kinds;
    pub use crate::card_render::{card_total_height, hit_test_card, paint_card_top_shimmer, render_card};
    pub use crate::card_stream::CardStream;
    pub use crate::scrollable_stack::{ScrollLayout, ScrollPosition, ScrollableStack};
    pub use crate::hooks::{PulseGate, ShimmerPhase, SpinnerFrame};
    pub use crate::section::{
        DashboardRegistry, DashboardTab, Section, SectionContext, SectionRegistry, SectionStack,
    };
    pub use crate::theme::{SemanticIntent, Strength, TextRole, Theme};
}
