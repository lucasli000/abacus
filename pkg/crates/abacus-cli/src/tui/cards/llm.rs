//! LlmCard —— LLM 回复卡
//!
//! V42-B 拆卡重构后：LlmCard 只承载 reply markdown，thinking 已剥离到 ThinkingCard。
//!
//! ## 内容
//!
//! - Reply markdown（流式增量）
//! - Header: `● LLM · {model}`
//!
//! ## 折叠策略
//!
//! 默认 Expanded —— reply 是核心内容, 不折叠。

use abacus_ui_kit::prelude::*;
use abacus_ui_kit::SectionContext;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::tui::md_stream::StreamingMd;

/// LLM 回复卡片
pub struct LlmCard {
    id: u64,
    model: String,
    /// reply markdown 流式渲染器
    reply_md: StreamingMd,
    /// 流式状态（由 CardStream 管理, 但卡片内部也保留副本供查询）
    streaming: CardStreaming,
    /// reply 文本原始累积（用于 body_height 估算）
    reply_text: String,
}

impl LlmCard {
    pub fn new(id: u64, model: impl Into<String>) -> Self {
        Self {
            id,
            model: model.into(),
            reply_md: StreamingMd::new(),
            streaming: CardStreaming::Active,
            reply_text: String::new(),
        }
    }

    /// 追加 reply delta
    pub fn append_reply(&mut self, delta: &str) {
        self.reply_text.push_str(delta);
        self.reply_md.append(delta);
    }

    pub fn set_streaming(&mut self, s: CardStreaming) {
        self.streaming = s;
    }

    /// 供复制功能读取 reply 纯文本
    pub fn reply_text_for_copy(&self) -> String {
        self.reply_text.clone()
    }

    /// reply 文本长度（避免 clone 仅取长度）
    pub fn reply_text_len(&self) -> usize {
        self.reply_text.len()
    }

    /// 取 reply_text 内部字段的可变引用（用于 V40 take 模式兼容）
    pub fn reply_text_field(&mut self) -> &mut String {
        &mut self.reply_text
    }

    /// 取 reply 首句（用于 Collapsed preview）
    fn reply_preview(&self) -> String {
        self.reply_text
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    }
}

impl MessageCard for LlmCard {
    fn kind(&self) -> CardKind {
        kinds::LLM
    }

    fn id(&self) -> u64 {
        self.id
    }

    fn header(&self, ctx: &dyn SectionContext) -> CardHeader {
        CardHeader::new(format!("● LLM · {}", self.model), "")
            .with_color(ctx.theme().session)
            .with_preview(self.reply_preview())
    }

    fn streaming(&self) -> CardStreaming {
        self.streaming
    }

    fn default_collapse(&self) -> CardCollapse {
        CardCollapse::Expanded
    }

    fn body_height(&self, _ctx: &dyn SectionContext, _max_width: u16, collapse: CardCollapse) -> u16 {
        match collapse {
            CardCollapse::Headless => 0,
            CardCollapse::Collapsed => {
                let preview = self.reply_preview();
                if preview.is_empty() { 1 } else { 1 }
            }
            CardCollapse::Expanded => {
                let reply_lines = self.reply_text.lines().count().max(1) as u16;
                reply_lines
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
                let preview = self.reply_preview();
                let (text, style) = if preview.is_empty() {
                    ("生成中…", Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM))
                } else {
                    (preview.as_str(), Style::default().fg(ctx.theme().text))
                };
                let p = Paragraph::new(Line::from(Span::styled(text, style)));
                f.render_widget(p, inner);
            }
            CardCollapse::Expanded => {
                let mut lines: Vec<Line> = Vec::new();
                if !self.reply_text.is_empty() {
                    for line in self.reply_text.lines() {
                        lines.push(Line::from(Span::styled(
                            format!(" {}", line),
                            Style::default().fg(ctx.theme().text),
                        )));
                    }
                }
                let p = Paragraph::new(lines).wrap(Wrap { trim: false });
                f.render_widget(p, inner);
            }
        }
    }
}

#[cfg(test)]
#[allow(unused_must_use)]
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
    fn llm_card_basic() {
        let card = LlmCard::new(1, "gpt-4");
        assert_eq!(card.kind(), kinds::LLM);
        assert_eq!(card.id(), 1);
        assert_eq!(card.streaming(), CardStreaming::Active);
    }

    #[test]
    fn llm_card_append_reply() {
        let mut card = LlmCard::new(1, "gpt-4");
        card.append_reply("hello");
        card.append_reply(" world");
        assert_eq!(card.reply_text, "hello world");
    }

    #[test]
    fn llm_card_render_does_not_panic() {
        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let card = LlmCard::new(1, "gpt-4");
        terminal.draw(|f| {
            card.render_body(f, &ctx(), Rect::new(0, 0, 40, 20), CardCollapse::Expanded);
        });
    }
}
