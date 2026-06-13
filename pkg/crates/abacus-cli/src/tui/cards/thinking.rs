//! ThinkingCard —— LLM 思考过程独立卡片
//!
//! V42-B 拆卡重构：将 thinking 从 LlmCard 中剥离，独立成卡。
//! 物理时序：Thinking chunks 先到达，TextDelta 后到达，二者不会并发。
//!
//! ## 设计
//!
//! - Header: `◐ Think · {model}`
//! - Body: 纯文本（每行左侧保留 1 空格缩进，避免贴粗色条）
//! - 默认 Collapsed（thinking 非核心内容，节省空间）
//! - 颜色: theme.accent（与 thinking 语义一致）

use abacus_ui_kit::prelude::*;
use abacus_ui_kit::SectionContext;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

/// Thinking 过程卡片
pub struct ThinkingCard {
    id: u64,
    model: String,
    text: String,
    streaming: CardStreaming,
}

impl ThinkingCard {
    pub fn new(id: u64, model: impl Into<String>) -> Self {
        Self {
            id,
            model: model.into(),
            text: String::new(),
            streaming: CardStreaming::Active,
        }
    }

    pub fn append(&mut self, delta: &str) {
        self.text.push_str(delta);
    }

    pub fn set_streaming(&mut self, s: CardStreaming) {
        self.streaming = s;
    }

    /// 取累积文本（用于落档 / 复制）
    pub fn text_for_copy(&self) -> String {
        self.text.clone()
    }

    /// 文本长度（零拷贝）
    pub fn text_len(&self) -> usize {
        self.text.len()
    }

    fn preview(&self) -> String {
        self.text.lines().next().unwrap_or("").to_string()
    }
}

impl MessageCard for ThinkingCard {
    fn kind(&self) -> CardKind {
        kinds::THINKING
    }

    fn id(&self) -> u64 {
        self.id
    }

    fn header(&self, ctx: &dyn SectionContext) -> CardHeader {
        CardHeader::new(format!("\u{25c8} Think  {}", self.model), "")
            .with_color(ctx.theme().accent)
            .with_preview(self.preview())
    }

    fn streaming(&self) -> CardStreaming {
        self.streaming
    }

    fn default_collapse(&self) -> CardCollapse {
        CardCollapse::Collapsed
    }

    fn body_height(
        &self,
        ctx: &dyn SectionContext,
        max_width: u16,
        collapse: CardCollapse,
    ) -> u16 {
        match collapse {
            CardCollapse::Headless => 0,
            CardCollapse::Collapsed => 1,
            CardCollapse::Expanded => {
                if self.text.is_empty() {
                    return 1;
                }
                // V42-B: 使用与 render_body 相同的 markdown 渲染计算行数
                let styled = crate::tui::markdown::render_markdown_bounded(
                    &self.text, ctx.theme(), false, max_width.saturating_sub(3) as usize
                );
                styled.len().max(1) as u16
            }
        }
    }

    fn render_body(
        &self,
        f: &mut Frame,
        ctx: &dyn SectionContext,
        inner: Rect,
        collapse: CardCollapse,
    ) {
        match collapse {
            CardCollapse::Headless => return,
            CardCollapse::Collapsed => {
                let preview = self.preview();
                let (text, style) = if preview.is_empty() {
                    (
                        "思考中…",
                        Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM),
                    )
                } else {
                    (preview.as_str(), Style::default().fg(ctx.theme().text))
                };
                let p = Paragraph::new(Line::from(Span::styled(text, style)));
                f.render_widget(p, inner);
            }
            CardCollapse::Expanded => {
                let mut lines: Vec<Line> = Vec::new();
                let border_style = Style::default().fg(ctx.theme().border);
                if self.text.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "(thinking\u{2026})",
                        Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM),
                    )));
                } else {
                    // V42-B: 复用 markdown 渲染，左侧加 │ 缩进线
                    let styled = crate::tui::markdown::render_markdown_bounded(
                        &self.text, ctx.theme(), false, inner.width.saturating_sub(3) as usize
                    );
                    let md_lines = crate::tui::markdown::styled_lines_to_lines(&styled);
                    for md_line in md_lines {
                        let mut spans = vec![Span::styled("\u{2502} ", border_style)];
                        spans.extend(md_line.spans);
                        lines.push(Line::from(spans));
                    }
                }
                let p = Paragraph::new(lines).wrap(Wrap { trim: false });
                f.render_widget(p, inner);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_ui_kit::Theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    struct Ctx(Theme);
    impl SectionContext for Ctx {
        fn theme(&self) -> &Theme {
            &self.0
        }
    }

    fn ctx() -> Ctx {
        Ctx(Theme::brand())
    }

    #[test]
    fn thinking_card_basic() {
        let card = ThinkingCard::new(1, "gpt-4");
        assert_eq!(card.kind(), kinds::THINKING);
        assert_eq!(card.id(), 1);
        assert_eq!(card.streaming(), CardStreaming::Active);
        assert_eq!(card.default_collapse(), CardCollapse::Collapsed);
    }

    #[test]
    fn thinking_card_append() {
        let mut card = ThinkingCard::new(1, "gpt-4");
        card.append("hello");
        card.append(" world");
        assert_eq!(card.text, "hello world");
    }

    #[test]
    fn thinking_card_render_does_not_panic() {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let card = ThinkingCard::new(1, "gpt-4");
        let _ = terminal.draw(|f| {
            card.render_body(f, &ctx(), Rect::new(0, 0, 40, 10), CardCollapse::Expanded);
        });
    }
}
