//! Local Section —— MLX 本地模型健康（embedding + reranker + 缓存）
//!
//! ## 渲染内容（最多 3 行，未连接时 2 行）
//!
//! ```text
//!  ─ Local ────────────
//!   ✓ embedding  ✓ reranker  模式 mlx
//!     块 187  缓存 1.2k
//! ```
//!
//! ## State 依赖
//!
//! - `mlx_health` —— Option<MlxHealth>; None 时显示"未连接"

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use abacus_ui_kit::{Section, SectionContext};

use crate::tui::components::section_ctx::downcast_app_state;

use super::{content_width, render_section_header};

pub struct LocalSection;

impl Default for LocalSection {
    fn default() -> Self {
        Self
    }
}

impl Section for LocalSection {
    fn id(&self) -> &str {
        "local"
    }

    fn title(&self) -> &str {
        "panel.local"
    }

    fn min_height(&self) -> u16 {
        2 // header + 至少 1 行
    }

    fn preferred_height(&self, _available: u16) -> u16 {
        3 // header + 最多 2 行内容
    }

    fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect) {
        let Some(state) = downcast_app_state(ctx) else {
            return;
        };
        let theme = ctx.theme();
        let w = content_width(area.width);
        let dim = Style::default().fg(theme.muted).add_modifier(Modifier::DIM);

        let mut lines: Vec<Line> = Vec::new();
        render_section_header(&mut lines, crate::tui::i18n::t("panel.local"), w, theme);

        if let Some(ref mlx) = state.mlx_health {
            lines.push(Line::from(vec![
                Span::styled("    ", dim),
                Span::styled(
                    if mlx.embedding_running {
                        format!("\u{2713} {}", crate::tui::i18n::t("panel.embedding"))
                    } else {
                        format!("\u{2717} {}", crate::tui::i18n::t("panel.embedding"))
                    },
                    if mlx.embedding_running {
                        Style::default().fg(theme.success)
                    } else {
                        Style::default().fg(theme.error)
                    },
                ),
                Span::styled("  ", dim),
                Span::styled(
                    if mlx.reranker_running {
                        format!("\u{2713} {}", crate::tui::i18n::t("panel.reranker"))
                    } else {
                        format!("\u{2717} {}", crate::tui::i18n::t("panel.reranker"))
                    },
                    if mlx.reranker_running {
                        Style::default().fg(theme.success)
                    } else {
                        Style::default().fg(theme.error)
                    },
                ),
                Span::styled(
                    format!("  {} {}", crate::tui::i18n::t("panel.mode"), mlx.mode),
                    Style::default().fg(theme.muted),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    ", dim),
                Span::styled(
                    format!(
                        "{} {}  {} {}",
                        crate::tui::i18n::t("panel.chunks"),
                        mlx.knowledge_chunks,
                        crate::tui::i18n::t("panel.cache"),
                        mlx.embeddings_cached
                    ),
                    Style::default().fg(theme.text),
                ),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("    ", dim),
                Span::styled("\u{00b7} \u{672a}\u{8fde}\u{63a5}", Style::default().fg(theme.muted)),
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
    fn local_section_metadata() {
        let s = LocalSection;
        assert_eq!(s.id(), "local");
    }

    #[test]
    fn local_section_renders_without_mlx() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let s = LocalSection;
        let state = AppState::new(AbacusMode::Clarify); // mlx_health = None 默认
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
