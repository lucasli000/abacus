//! Palace Section —— 记忆宫殿快照（知识 + 行为）
//!
//! ## 渲染内容（最多 3 行，加载中 2 行）
//!
//! ```text
//!  ─ Palace ───────────
//!     知识 245  rust:42  bug:31  cli:18  到期 3
//!     行为 18  活跃 12  高频: debug,plan,review
//! ```
//!
//! ## State 依赖
//!
//! - `palace_data` —— Option<PalaceSnapshot>; None 时显示"加载中"

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use abacus_ui_kit::{Section, SectionContext};

use crate::tui::components::section_ctx::downcast_app_state;

use super::{content_width, render_section_header};

pub struct PalaceSection;

impl Default for PalaceSection {
    fn default() -> Self {
        Self
    }
}

impl Section for PalaceSection {
    fn id(&self) -> &str {
        "palace"
    }
    fn order(&self) -> u32 {
        40
    }

    fn title(&self) -> &str {
        "panel.palace"
    }

    fn min_height(&self) -> u16 {
        2 // header + 至少 1 行
    }

    fn preferred_height(&self, _available: u16) -> u16 {
        3 // header + 知识行 + 行为行
    }

    fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect) {
        let Some(state) = downcast_app_state(ctx) else {
            return;
        };
        let theme = ctx.theme();
        let w = content_width(area.width);
        let dim = Style::default().fg(theme.muted).add_modifier(Modifier::DIM);

        let mut lines: Vec<Line> = Vec::new();
        render_section_header(&mut lines, crate::tui::i18n::t("panel.palace"), w, theme);

        if let Some(ref snap) = state.palace_data {
            // ── 知识行 ──
            let mut k_parts = vec![Span::styled("    ", dim)];
            if snap.knowledge_total > 0 {
                k_parts.push(Span::styled(
                    format!(
                        "{} {}",
                        crate::tui::i18n::t("panel.knowledge"),
                        snap.knowledge_total
                    ),
                    Style::default().fg(theme.text),
                ));
                for (domain, cnt) in snap.knowledge_domains.iter().take(3) {
                    let d: String = domain.chars().take(8).collect();
                    k_parts.push(Span::styled(
                        format!("  {}:{}", d, cnt),
                        Style::default().fg(theme.muted),
                    ));
                }
                if snap.knowledge_due > 0 {
                    k_parts.push(Span::styled(
                        format!(
                            "  {} {}",
                            crate::tui::i18n::t("panel.due"),
                            snap.knowledge_due
                        ),
                        Style::default().fg(theme.gold),
                    ));
                }
            }
            if k_parts.len() > 1 {
                lines.push(Line::from(k_parts));
            }

            // ── 行为行 ──
            let mut b_parts = vec![Span::styled("    ", dim)];
            if snap.behavior_count > 0 {
                b_parts.push(Span::styled(
                    format!(
                        "{} {}",
                        crate::tui::i18n::t("panel.behavior"),
                        snap.behavior_count
                    ),
                    Style::default().fg(theme.text),
                ));
                b_parts.push(Span::styled(
                    format!(
                        "  {} {}",
                        crate::tui::i18n::t("panel.active"),
                        snap.behavior_active
                    ),
                    Style::default().fg(theme.muted),
                ));
                if !snap.behavior_top_tags.is_empty() {
                    let tags: Vec<String> = snap
                        .behavior_top_tags
                        .iter()
                        .map(|(tag, _)| tag.chars().take(6).collect())
                        .collect();
                    b_parts.push(Span::styled(
                        format!(
                            "  {}: {}",
                            crate::tui::i18n::t("panel.high_freq"),
                            tags.join(",")
                        ),
                        Style::default().fg(theme.muted),
                    ));
                }
            }
            if b_parts.len() > 1 {
                lines.push(Line::from(b_parts));
            }
        } else {
            lines.push(Line::from(vec![
                Span::styled("    ", dim),
                Span::styled("\u{00b7} \u{52a0}\u{8f7d}\u{4e2d}", Style::default().fg(theme.muted)),
            ]));
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
    fn palace_section_metadata() {
        let s = PalaceSection;
        assert_eq!(s.id(), "palace");
    }

    #[test]
    fn palace_section_renders_loading() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let s = PalaceSection;
        let state = AppState::new(AbacusMode::Clarify); // palace_data = None
        let ctx = AppContext::new(&state);
        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 2);
                s.render(f, &ctx, area);
            })
            .unwrap();
    }
}
