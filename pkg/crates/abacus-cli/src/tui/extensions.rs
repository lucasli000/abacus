//! TUI 扩展点 —— Section / DashboardTab 注册 + config.toml 启用控制
//!
//! V42 引入：暴露 [`SectionRegistry`] + [`DashboardRegistry`] 给 Agent 应用扩展自定义看板/仪表盘。
//!
//! ## 功能清单
//!
//! - [`register_builtin_sections`] —— 注册 6 个内置看板 Section
//! - [`register_builtin_dashboard_tabs`] —— 注册 2 个内置仪表盘 tab
//! - [`default_panel_layout`] —— 默认看板布局（id 顺序）
//! - [`default_dashboard_tabs`] —— 默认仪表盘 tab id 顺序
//!
//! ## Agent 应用扩展示例
//!
//! ```ignore
//! use abacus_cli::tui::extensions;
//! use abacus_ui_kit::Section;
//!
//! struct QuantPositionsSection;
//! impl Section for QuantPositionsSection { /* ... */ }
//!
//! // 在 app 启动时
//! let mut sections = extensions::new_section_registry();
//! sections.register(Box::new(QuantPositionsSection));
//!
//! // 启用列表（覆盖 default_panel_layout）
//! let layout = vec!["llm", "tools", "com.example.quant.positions", "timeline"];
//! let stack = sections.build_stack(&layout);
//! ```

use abacus_ui_kit::{DashboardRegistry, SectionRegistry};

use crate::tui::components::dashboard_tabs::{AutoTab, HealthTab};
use crate::tui::components::panel_sections::{
    FocusSection, LlmSection, LocalSection, PalaceSection, TimelineSection, ToolsSection,
};

/// 创建一个 SectionRegistry 并注册全部 6 个内置 Section
///
/// 注册顺序与 [`default_panel_layout`] 对齐, 但注册顺序无关 —— 渲染顺序由
/// `build_stack(&ids)` 的 ids 决定。
pub fn register_builtin_sections(registry: &mut SectionRegistry) {
    registry.register(Box::new(LlmSection));
    registry.register(Box::new(ToolsSection));
    registry.register(Box::new(LocalSection));
    registry.register(Box::new(PalaceSection));
    registry.register(Box::new(TimelineSection));
    registry.register(Box::new(FocusSection));
}

/// 创建一个 DashboardRegistry 并注册全部 2 个内置 tab
///
/// 注册顺序决定 cycle 顺序（Health → Auto → 回环）。
pub fn register_builtin_dashboard_tabs(registry: &mut DashboardRegistry) {
    registry.register(Box::new(HealthTab));
    registry.register(Box::new(AutoTab));
}

/// 默认看板 Section 启用列表 + 渲染顺序
///
/// 用户可通过 config.toml `[tui.panel] sections = [...]` 覆盖。
///
/// 顺序对应当前 V40 视觉布局：
/// - 上半: LLM + Tools + Local + Palace (Stockroom 区块)
/// - 中: Timeline
/// - 下: Focus (visible 时显示)
pub const fn default_panel_layout() -> &'static [&'static str] {
    &["llm", "tools", "local", "palace", "timeline", "focus"]
}

/// 默认仪表盘 tab 启用列表 + 切换顺序
pub const fn default_dashboard_tabs() -> &'static [&'static str] {
    &["health", "auto"]
}

/// 一键创建带全部内置 Section 的 registry（便利 API）
pub fn new_section_registry() -> SectionRegistry {
    let mut r = SectionRegistry::new();
    register_builtin_sections(&mut r);
    r
}

/// 一键创建带全部内置 tab 的 registry（便利 API）
pub fn new_dashboard_registry() -> DashboardRegistry {
    let mut r = DashboardRegistry::new();
    register_builtin_dashboard_tabs(&mut r);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_sections_all_registered() {
        let r = new_section_registry();
        for id in default_panel_layout() {
            assert!(
                r.contains(id),
                "default_panel_layout 引用了未注册的 id: {}",
                id
            );
        }
    }

    #[test]
    fn builtin_dashboard_tabs_all_registered() {
        let r = new_dashboard_registry();
        assert_eq!(r.active_id(), Some("health")); // 首个注册 = 默认 active
        assert_eq!(r.tabs().len(), 2);
    }

    #[test]
    fn default_panel_layout_has_6_sections() {
        assert_eq!(default_panel_layout().len(), 6);
    }

    #[test]
    fn default_dashboard_tabs_has_2_tabs() {
        assert_eq!(default_dashboard_tabs().len(), 2);
    }
}
