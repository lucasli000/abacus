//! Tools Section —— 工具调用统计 + 健康状态 + 分类计数
//!
//! ## 渲染内容（3 行）
//!
//! ```text
//!  ─ Tools ────────────
//!     内置 42 外部 0  成功 100%
//!     调用 9  外部 0  工作流 4
//! ```
//!
//! ## State 依赖
//!
//! - `tool_health` —— 总可用数 + 外部 (mcp__) 数
//! - `tool_records` —— 调用总数 + 成功率 + 分类 (mcp__ / skill / agent)
//! - `experts` —— Agent 数（Meeting 模式时 > 0）

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use abacus_ui_kit::{Section, SectionContext};

use crate::tui::components::section_ctx::downcast_app_state;
use crate::tui::state::ToolStatus;

use super::{content_width, render_section_header};

pub struct ToolsSection;

impl Default for ToolsSection {
    fn default() -> Self {
        Self
    }
}

impl Section for ToolsSection {
    fn id(&self) -> &str {
        "tools"
    }
    fn order(&self) -> u32 {
        20
    }

    fn title(&self) -> &str {
        "panel.tools"
    }

    fn min_height(&self) -> u16 {
        3 // header + 2 行
    }

    fn preferred_height(&self, _available: u16) -> u16 {
        3
    }

    fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect) {
        let Some(state) = downcast_app_state(ctx) else {
            return;
        };
        let theme = ctx.theme();
        let w = content_width(area.width);
        let dim = Style::default().fg(theme.muted).add_modifier(Modifier::DIM);

        let mut lines: Vec<Line> = Vec::new();
        render_section_header(&mut lines, crate::tui::i18n::t("panel.tools"), w, theme);

        let avail = state
            .tool_health
            .values()
            .filter(|h| !h.blocked_by_env)
            .count();
        let mcp_count = state
            .tool_health
            .keys()
            .filter(|k| k.starts_with("mcp__"))
            .count();
        let tc = state.tool_records.len();
        let sc = state
            .tool_records
            .iter()
            .filter(|r| matches!(r.status, ToolStatus::Success))
            .count();
        let rate = if tc > 0 { sc * 100 / tc } else { 0 };

        lines.push(Line::from(vec![
            Span::styled("    ", dim),
            Span::styled(
                format!(
                    "{} {}  {} {}",
                    crate::tui::i18n::t("panel.builtin"),
                    avail.saturating_sub(mcp_count),
                    crate::tui::i18n::t("panel.external"),
                    mcp_count,
                ),
                Style::default().fg(theme.text),
            ),
            // V42-B: 0 次调用时不显示成功率，避免 "成功 100%" 误导
            if tc > 0 {
                Span::styled(
                    format!("  {} {}%", crate::tui::i18n::t("panel.success"), rate),
                    if rate >= 80 {
                        Style::default().fg(theme.success)
                    } else {
                        Style::default().fg(theme.gold)
                    },
                )
            } else {
                Span::raw("")
            },
        ]));

        let mcp_calls = state
            .tool_records
            .iter()
            .filter(|r| r.name.starts_with("mcp__"))
            .count();
        let skill_calls = state
            .tool_records
            .iter()
            .filter(|r| !r.name.contains("__") && !r.name.starts_with("mcp_"))
            .count();
        let agent_count = state.experts.len();
        let mut call_parts = vec![
            Span::styled("    ", dim),
            Span::styled(
                format!("{} {}", crate::tui::i18n::t("panel.calls"), tc),
                Style::default().fg(theme.text),
            ),
        ];
        if mcp_calls > 0 {
            call_parts.push(Span::styled(
                format!("  {} {}", crate::tui::i18n::t("panel.external"), mcp_calls),
                Style::default().fg(theme.muted),
            ));
        }
        if skill_calls > 0 {
            call_parts.push(Span::styled(
                format!(
                    "  {} {}",
                    crate::tui::i18n::t("panel.workflow"),
                    skill_calls
                ),
                Style::default().fg(theme.muted),
            ));
        }
        if agent_count > 0 {
            call_parts.push(Span::styled(
                format!("  {} {}", crate::tui::i18n::t("panel.agent"), agent_count),
                Style::default().fg(theme.muted),
            ));
        }
        lines.push(Line::from(call_parts));

        f.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::components::section_ctx::AppContext;
    use crate::tui::state::{AbacusMode, AppState};

    #[test]
    fn tools_section_metadata() {
        let s = ToolsSection;
        assert_eq!(s.id(), "tools");
        assert_eq!(s.min_height(), 3);
    }

    #[test]
    fn tools_section_renders_empty_state() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let s = ToolsSection;
        let state = AppState::new(AbacusMode::Clarify);
        let ctx = AppContext::new(&state);
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 3);
                s.render(f, &ctx, area);
            })
            .unwrap();
    }
}
