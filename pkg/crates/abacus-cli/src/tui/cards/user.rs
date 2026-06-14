//! UserCard —— 用户消息输入卡
//!
//! V42-B 内置 Card 之一。实现 [`MessageCard`] trait。
//!
//! ## 内容
//!
//! - 用户输入的纯文本 (可能含 markdown)
//! - Header: `> You · {time}`
//! - Body: 文本内容, markdown 渲染
//!
//! ## 折叠策略
//!
//! 默认 Expanded —— 用户消息通常短, 不需要折叠。

use abacus_ui_kit::prelude::*;
use abacus_ui_kit::SectionContext;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::tui::markdown;

/// 用户消息卡片
#[derive(Debug)]
pub struct UserCard {
    id: u64,
    text: String,
    time: String,
    streaming: CardStreaming,
}

impl UserCard {
    pub fn new(id: u64, text: impl Into<String>, time: impl Into<String>) -> Self {
        Self {
            id,
            text: text.into(),
            time: time.into(),
            streaming: CardStreaming::Static,
        }
    }

    /// 流式期间更新文本 (用户 typing 时不应调用, 仅用于 mid-turn signal 注入)
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
    }

    pub fn set_streaming(&mut self, s: CardStreaming) {
        self.streaming = s;
    }
}

impl MessageCard for UserCard {
    fn kind(&self) -> CardKind {
        kinds::USER
    }

    fn id(&self) -> u64 {
        self.id
    }

    fn header(&self, ctx: &dyn SectionContext) -> CardHeader {
        CardHeader::new(
            format!("\u{25b8} You"),
            self.time.clone(),
        )
        .with_color(ctx.theme().user)
    }

    fn streaming(&self) -> CardStreaming {
        self.streaming
    }

    fn text_content(&self) -> String {
        self.text.clone()
    }

    fn default_collapse(&self) -> CardCollapse {
        CardCollapse::Expanded
    }

    fn body_height(&self, ctx: &dyn SectionContext, max_width: u16, _collapse: CardCollapse) -> u16 {
        if self.text.is_empty() {
            return 1;
        }
        let lines = markdown::render_markdown_bounded(&self.text,
            ctx.theme(), true, max_width as usize
        );
        lines.len() as u16
    }

    fn render_body(
        &self,
        f: &mut Frame,
        ctx: &dyn SectionContext,
        inner: Rect,
        _collapse: CardCollapse,
    ) {
        if self.text.is_empty() {
            let p = Paragraph::new(Line::from(Span::styled(
                "(empty)",
                Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM),
            )));
            f.render_widget(p, inner);
            return;
        }
        let styled = markdown::render_markdown_bounded(
            &self.text,
            ctx.theme(), true, inner.width as usize,
        );
        let lines = markdown::styled_lines_to_lines(&styled);
        let p = Paragraph::new(lines)
            .wrap(Wrap { trim: false });
        f.render_widget(p, inner);
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
        fn theme(&self) -> &Theme { &self.0 }
    }

    fn ctx() -> Ctx { Ctx(Theme::brand()) }

    #[test]
    fn user_card_basic() {
        let card = UserCard::new(1, "hello", "09:23");
        assert_eq!(card.kind(), kinds::USER);
        assert_eq!(card.id(), 1);
        assert_eq!(card.streaming(), CardStreaming::Static);
        assert_eq!(card.default_collapse(), CardCollapse::Expanded);
    }

    #[test]
    fn user_card_header() {
        let card = UserCard::new(1, "hi", "10:00");
        let h = card.header(&ctx());
        assert_eq!(h.title, "\u{25b8} You");
        assert_eq!(h.trailing, "10:00");
    }

    #[test]
    fn user_card_body_height_non_empty() {
        let card = UserCard::new(1, "hello world", "09:23");
        let h = card.body_height(&ctx(), 80, CardCollapse::Expanded);
        assert!(h > 0);
    }

    #[test]
    fn user_card_render_does_not_panic() {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let card = UserCard::new(1, "hello **world**", "09:23");
        let _ = terminal.draw(|f| {
            card.render_body(f, &ctx(), Rect::new(0, 0, 40, 10), CardCollapse::Expanded);
        });
    }
}
