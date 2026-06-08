//! Health Tab —— ScriptHook 状态仪表盘
//!
//! ## 渲染内容（2 行示意，待对接真实 hook_stats）
//!
//! ```text
//!  ◇ 已注册 0  已触发 0  失败 0
//!  ✓ 最近: --
//! ```
//!
//! ## State 依赖（当前）
//!
//! - `dashboard_scroll` —— 上下滚动
//! - 未来对接 `state.hook_stats`（尚未实现的 ScriptHook 状态字段）

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use abacus_ui_kit::{DashboardTab, SectionContext};

use crate::tui::components::section_ctx::downcast_app_state;

pub struct HealthTab;

impl Default for HealthTab {
    fn default() -> Self {
        Self
    }
}

impl DashboardTab for HealthTab {
    fn id(&self) -> &str {
        "health"
    }

    fn label(&self) -> &str {
        "dash.health"
    }

    fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect) {
        let Some(state) = downcast_app_state(ctx) else {
            return;
        };
        let theme = ctx.theme();
        let muted = Style::default().fg(theme.muted);
        let dim = Style::default().fg(theme.muted).add_modifier(Modifier::DIM);
        let txt = Style::default().fg(theme.text);
        let ok = Style::default().fg(theme.success);

        let mut lines: Vec<Line> = Vec::new();

        // 占位示意 —— 实际应从 state.hook_stats 读取
        lines.push(Line::from(vec![
            Span::styled(" \u{25c7} ", muted),
            Span::styled(
                format!(
                    "{} 0  {}",
                    crate::tui::i18n::t("panel.hook_registered"),
                    crate::tui::i18n::t("panel.hook_triggered")
                ),
                txt,
            ),
            Span::styled(" 0  ", muted),
            Span::styled(
                format!("{} 0", crate::tui::i18n::t("panel.hook_failed")),
                ok,
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" \u{2713} ", muted),
            Span::styled(
                format!("{}: --", crate::tui::i18n::t("panel.hook_last")),
                dim,
            ),
        ]));

        // 滚动
        let scroll = state.dashboard_scroll;
        let vis = area.height as usize;
        if lines.len() > vis {
            let end = lines.len().saturating_sub(scroll);
            let start = end.saturating_sub(vis);
            lines = lines[start..end].to_vec();
            if scroll > 0 && !lines.is_empty() {
                lines[0] = Line::from(vec![
                    Span::styled(" \u{2191} ", muted),
                    Span::styled(
                        format!("{} {}", scroll, crate::tui::i18n::t("dash.jobs")),
                        dim,
                    ),
                ]);
            }
        }
        f.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::components::section_ctx::AppContext;
    use crate::tui::state::{AbacusMode, AppState};

    #[test]
    fn health_tab_metadata() {
        let t = HealthTab;
        assert_eq!(t.id(), "health");
    }

    #[test]
    fn health_tab_renders() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let tab = HealthTab;
        let state = AppState::new(AbacusMode::Clarify);
        let ctx = AppContext::new(&state);
        let backend = TestBackend::new(30, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 30, 3);
                tab.render(f, &ctx, area);
            })
            .unwrap();
    }
}
