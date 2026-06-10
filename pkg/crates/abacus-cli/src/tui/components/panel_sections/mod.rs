//! Panel Sections — 看板可组合 Section 实现集
//!
//! V42 引入：把 `panel.rs::render_tab_scene` 的 ~250 行内联渲染拆为 6 个独立 Section,
//! 每个 Section 实现 [`abacus_ui_kit::Section`] trait, 通过 [`SectionRegistry`] 注册。
//!
//! ## 内置 Section 清单
//!
//! | id | 文件 | 内容 |
//! |---|---|---|
//! | `llm` | [`llm::LlmSection`] | Provider + Model + ctx% + token + cost |
//! | `tools` | [`tools::ToolsSection`] | 工具计数 + 成功率 + 分类调用统计 |
//! | `local` | [`local::LocalSection`] | MLX embedding + reranker + 块/缓存 |
//! | `palace` | [`palace::PalaceSection`] | 知识/行为宫殿快照 + 高频 tag |
//! | `timeline` | [`timeline::TimelineSection`] | 现场时间线分组（阶段标签）|
//! | `focus` | [`focus::FocusSection`] | Plan/Team/Meeting/默认 4 分支 focus 面板 |
//!
//! ## 注册顺序
//!
//! 由 [`crate::tui::extensions::register_builtin_sections`] 在应用启动时按
//! `["llm", "tools", "local", "palace", "timeline", "focus"]` 顺序注册。
//! 用户可通过 config.toml `[tui.panel] sections = [...]` 自定义启用列表。
//!
//! ## 共享 helper
//!
//! [`render_section_header`] —— 区段标题分隔线渲染，所有内置 Section 复用

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use abacus_ui_kit::Theme;

pub mod llm;
pub mod tools;
pub mod local;
pub mod palace;
pub mod timeline;
pub mod focus;

pub use llm::LlmSection;
pub use tools::ToolsSection;
pub use local::LocalSection;
pub use palace::PalaceSection;
pub use timeline::TimelineSection;
pub use focus::FocusSection;

/// Section 标题渲染 —— 所有内置 Section 复用
///
/// 渲染形如：`  ▸ LLM`
///
/// - 前缀 `  ▸ ` (dim muted)
/// - label (muted + BOLD)
pub(crate) fn render_section_header(
    lines: &mut Vec<Line>,
    label: &str,
    _width: usize,
    theme: &Theme,
) {
    let _ = _width;
    let dim = Style::default().fg(theme.muted).add_modifier(Modifier::DIM);
    lines.push(Line::from(vec![
        Span::styled("  \u{25b8} ", dim),
        Span::styled(label.to_string(), Style::default().fg(theme.muted).add_modifier(Modifier::BOLD)),
    ]));
}

/// 计算 section 内容可用宽度 —— 扣边距后的可写字符数
///
/// area.width - 4 (左右各 2 padding), 至少保留 10 列
pub(crate) fn content_width(area_width: u16) -> usize {
    (area_width as usize).saturating_sub(4).max(10)
}
