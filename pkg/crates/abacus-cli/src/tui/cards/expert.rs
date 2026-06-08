//! ExpertCard —— Meeting 模式专家卡
//!
//! V42-B 内置 Card。实现 [`MessageCard`] trait。
//! 语义同 LlmCard，但 header 含专家名 + 使用 theme.expert 色。

use abacus_ui_kit::prelude::*;
use abacus_ui_kit::SectionContext;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::tui::md_stream::StreamingMd;

pub struct ExpertCard {
    id: u64,
    name: String,
    model: String,
    thinking: Option<String>,
    reply_md: StreamingMd,
    streaming: CardStreaming,
    reply_text: String,
}

impl ExpertCard {
    pub fn new(id: u64, name: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            model: model.into(),
            thinking: None,
            reply_md: StreamingMd::new(),
            streaming: CardStreaming::Active,
            reply_text: String::new(),
        }
    }

    pub fn append_thinking(&mut self, delta: &str) {
        if self.thinking.is_none() { self.thinking = Some(String::new()); }
        if let Some(ref mut t) = self.thinking { t.push_str(delta); }
    }

    pub fn append_reply(&mut self, delta: &str) {
        self.reply_text.push_str(delta);
        self.reply_md.append(delta);
    }

    pub fn set_streaming(&mut self, s: CardStreaming) {
        self.streaming = s;
    }

    fn reply_preview(&self) -> String {
        self.reply_text.lines().next().unwrap_or("").to_string()
    }
}

impl MessageCard for ExpertCard {
    fn kind(&self) -> CardKind { kinds::EXPERT }
    fn id(&self) -> u64 { self.id }

    fn header(&self, ctx: &dyn SectionContext) -> CardHeader {
        let title = format!("◆ {} · {}", self.name, self.model);
        CardHeader::new(title, "")
            .with_color(ctx.theme().expert)
            .with_preview(self.reply_preview())
    }

    fn streaming(&self) -> CardStreaming { self.streaming }
    fn default_collapse(&self) -> CardCollapse { CardCollapse::Expanded }

    fn body_height(&self, _ctx: &dyn SectionContext, _max_width: u16, collapse: CardCollapse) -> u16 {
        match collapse {
            CardCollapse::Headless => 0,
            CardCollapse::Collapsed => 1,
            CardCollapse::Expanded => {
                let mut h = 0u16;
                if let Some(ref t) = self.thinking {
                    h = h.saturating_add(1);
                    h = h.saturating_add(t.lines().count().min(5) as u16);
                    if t.lines().count() > 5 { h = h.saturating_add(1); }
                    h = h.saturating_add(1);
                }
                // reply_md needs &mut; estimate from text lines
                let reply_lines = self.reply_text.lines().count();
                h = h.saturating_add(reply_lines.max(1) as u16);
                h
            }
        }
    }

    fn render_body(&self, f: &mut Frame, ctx: &dyn SectionContext, inner: Rect, collapse: CardCollapse) {
        match collapse {
            CardCollapse::Headless => return,
            CardCollapse::Collapsed => {
                let preview = self.reply_preview();
                let text = if preview.is_empty() { "(replying…)" } else { preview.as_str() };
                let p = Paragraph::new(Line::from(Span::styled(text, Style::default().fg(ctx.theme().text))));
                f.render_widget(p, inner);
            }
            CardCollapse::Expanded => {
                let mut lines: Vec<Line> = Vec::new();
                if let Some(ref t) = self.thinking {
                    let accent = ctx.theme().accent;
                    lines.push(Line::from(vec![
                        Span::styled("  ╭─ ", Style::default().fg(accent).add_modifier(Modifier::DIM)),
                        Span::styled("~ thinking", Style::default().fg(accent)),
                    ]));
                    for line in t.lines().take(5) {
                        lines.push(Line::from(Span::styled(
                            format!("  │ {}", line),
                            Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM),
                        )));
                    }
                    if t.lines().count() > 5 {
                        lines.push(Line::from(Span::styled(
                            format!("  │ …{} more lines", t.lines().count() - 5),
                            Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM),
                        )));
                    }
                    lines.push(Line::from(Span::styled("  ╰─", Style::default().fg(accent).add_modifier(Modifier::DIM))));
                    lines.push(Line::raw(""));
                }
                // V42-B 升级: reply 加 1 字符前导 padding, 避免贴左边框
                if self.reply_text.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "(replying…)",
                        Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM),
                    )));
                } else {
                    for line in self.reply_text.lines() {
                        let mut spans = vec![Span::raw(" ")];
                        spans.push(Span::styled(line.to_string(), Style::default().fg(ctx.theme().text)));
                        lines.push(Line::from(spans));
                    }
                }
                let p = Paragraph::new(lines).wrap(Wrap { trim: false });
                f.render_widget(p, inner);
            }
        }
    }
}
