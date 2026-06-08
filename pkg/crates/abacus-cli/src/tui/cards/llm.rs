//! LlmCard —— LLM 思考+回复卡
//!
//! V42-B 内置 Card 之一。实现 [`MessageCard`] trait。
//!
//! ## 内容
//!
//! - Thinking 文本（可选, 可折叠）
//! - Reply markdown（流式增量）
//! - Header: `● LLM · {model} · think:{level}`
//!
//! ## 折叠策略
//!
//! 默认 Expanded —— reply 是核心内容, 不折叠。
//! Collapsed 时 thinking 隐藏, 仅显示 reply 首句 preview。

use abacus_ui_kit::prelude::*;
use abacus_ui_kit::SectionContext;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::tui::md_stream::StreamingMd;

/// LLM 思考+回复卡片
pub struct LlmCard {
    id: u64,
    model: String,
    thinking_level: String,
    /// thinking 文本（累积）
    thinking: Option<String>,
    /// reply markdown 流式渲染器
    reply_md: StreamingMd,
    /// 流式状态（由 CardStream 管理, 但卡片内部也保留副本供查询）
    streaming: CardStreaming,
    /// reply 文本原始累积（用于 body_height 估算）
    reply_text: String,
}

impl LlmCard {
    pub fn new(
        id: u64,
        model: impl Into<String>,
        thinking_level: impl Into<String>,
    ) -> Self {
        Self {
            id,
            model: model.into(),
            thinking_level: thinking_level.into(),
            thinking: None,
            reply_md: StreamingMd::new(),
            streaming: CardStreaming::Active,
            reply_text: String::new(),
        }
    }

    /// 追加 thinking delta
    pub fn append_thinking(&mut self, delta: &str) {
        if self.thinking.is_none() {
            self.thinking = Some(String::new());
        }
        if let Some(ref mut t) = self.thinking {
            t.push_str(delta);
        }
    }

    /// 追加 reply delta
    pub fn append_reply(&mut self, delta: &str) {
        self.reply_text.push_str(delta);
        self.reply_md.append(delta);
    }

    /// 设置 thinking 完成（turn 结束时调用）
    pub fn finish_thinking(&mut self) {
        // thinking 文本已累积, 无需额外操作
    }

    pub fn set_streaming(&mut self, s: CardStreaming) {
        self.streaming = s;
    }

    /// 供复制功能读取 reply 纯文本
    /// V42-B: extract_selection_text 调用, 复制全部 LLM reply
    pub fn reply_text_for_copy(&self) -> String {
        self.reply_text.clone()
    }

    /// V42-B: reply 文本长度（避免 clone 仅取长度）
    pub fn reply_text_len(&self) -> usize {
        self.reply_text.len()
    }

    /// V42-B: thinking 文本长度（避免 clone 仅取长度）
    pub fn thinking_text_len(&self) -> usize {
        self.thinking.as_ref().map_or(0, |t| t.len())
    }

    /// 取累积的 thinking 文本（V42-B: 替代 V40 `state.streaming_thinking` 字段读）
    ///
    /// 返回 `None` 表示本 LlmCard 还没有 thinking 内容。
    pub fn thinking_text(&self) -> Option<String> {
        self.thinking.clone()
    }

    /// V42-B: 取 reply_text 内部字段的可变引用（用于 V40 take 模式兼容）
    ///
    /// 注意：append_reply 会更新 reply_md 流式解析器；取走后需调 freeze() 关闭流式。
    pub fn reply_text_field(&mut self) -> &mut String {
        &mut self.reply_text
    }

    /// V42-B: 取并清空 thinking 文本（用于 fatal/cancel 时 take 模式）
    pub fn take_thinking(&mut self) -> Option<String> {
        self.thinking.take()
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
        let title = format!("● LLM · {} · think:{}", self.model, self.thinking_level);
        CardHeader::new(title, "")
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
                // 仅 preview 行
                let preview = self.reply_preview();
                if preview.is_empty() { 1 } else { 1 }
            }
            CardCollapse::Expanded => {
                let mut h = 0u16;
                // thinking 部分（最多 5 行 + 1 行标签）
                if let Some(ref t) = self.thinking {
                    let think_lines = t.lines().count().min(5);
                    h = h.saturating_add(1); // "~ thinking" 标签
                    h = h.saturating_add(think_lines as u16);
                    if t.lines().count() > 5 {
                        h = h.saturating_add(1); // "…N more lines"
                    }
                    h = h.saturating_add(1); // 空行分隔
                }
                // reply 部分 — 实际行数
                let reply_lines = self.reply_text.lines().count().max(1) as u16;
                h = h.saturating_add(reply_lines);
                if h == 0 { 1 } else { h }
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
                let text = if preview.is_empty() {
                    "(replying…)"
                } else {
                    preview.as_str()
                };
                let p = Paragraph::new(Line::from(Span::styled(
                    text,
                    Style::default().fg(ctx.theme().text),
                )));
                f.render_widget(p, inner);
            }
            CardCollapse::Expanded => {
                let mut lines: Vec<Line> = Vec::new();
                // thinking 部分
                if let Some(ref t) = self.thinking {
                    let accent = ctx.theme().accent;
                    lines.push(Line::from(vec![
                        Span::styled("  ╭─ ", Style::default().fg(accent).add_modifier(Modifier::DIM)),
                        Span::styled("~ thinking", Style::default().fg(accent)),
                    ]));
                    let think_lines: Vec<&str> = t.lines().take(5).collect();
                    for line in &think_lines {
                        lines.push(Line::from(Span::styled(
                            format!("  │ {}", line),
                            Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM),
                        )));
                    }
                    let total_lines = t.lines().count();
                    if total_lines > 5 {
                        lines.push(Line::from(Span::styled(
                            format!("  │ …{} more lines", total_lines - 5),
                            Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM),
                        )));
                    }
                    lines.push(Line::from(Span::styled(
                        "  ╰─",
                        Style::default().fg(accent).add_modifier(Modifier::DIM),
                    )));
                    lines.push(Line::raw(""));
                }
                // reply 部分 — 渲染实际内容
                if self.reply_text.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "(replying…)",
                        Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM),
                    )));
                } else {
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
        let card = LlmCard::new(1, "gpt-4", "high");
        assert_eq!(card.kind(), kinds::LLM);
        assert_eq!(card.id(), 1);
        assert_eq!(card.streaming(), CardStreaming::Active);
    }

    #[test]
    fn llm_card_append_reply() {
        let mut card = LlmCard::new(1, "gpt-4", "high");
        card.append_reply("hello");
        card.append_reply(" world");
        assert_eq!(card.reply_text, "hello world");
    }

    #[test]
    fn llm_card_render_does_not_panic() {
        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let card = LlmCard::new(1, "gpt-4", "high");
        terminal.draw(|f| {
            card.render_body(f, &ctx(), Rect::new(0, 0, 40, 20), CardCollapse::Expanded);
        });
    }
}
