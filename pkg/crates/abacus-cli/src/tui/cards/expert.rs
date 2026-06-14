//! ExpertCard —— Meeting 模式专家回复卡
//!
//! V42-B 拆卡重构后：ExpertCard 只承载 reply markdown，thinking 已剥离到 ThinkingCard。
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
    reply_md: std::cell::RefCell<StreamingMd>,
    streaming: CardStreaming,
    reply_text: String,
}

impl ExpertCard {
    pub fn new(id: u64, name: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            model: model.into(),
            reply_md: std::cell::RefCell::new(StreamingMd::new()),
            streaming: CardStreaming::Active,
            reply_text: String::new(),
        }
    }

    pub fn append_reply(&mut self, delta: &str) {
        self.reply_text.push_str(delta);
        self.reply_md.borrow_mut().append(delta);
    }

    pub fn set_streaming(&mut self, s: CardStreaming) {
        self.streaming = s;
    }

    /// 供去重检测读取已累积的 reply 文本
    pub fn reply_text_for_copy(&self) -> String {
        self.reply_text.clone()
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
    fn is_empty(&self) -> bool { self.reply_text.is_empty() }
    fn default_collapse(&self) -> CardCollapse { CardCollapse::Expanded }

    fn body_height(&self, ctx: &dyn SectionContext, max_width: u16, collapse: CardCollapse) -> u16 {
        match collapse {
            CardCollapse::Headless => 0,
            CardCollapse::Collapsed => 1,
            CardCollapse::Expanded => {
                // 2026-06-11 FIX: 与 render_body 走相同的 markdown 路径计算高度
                // 之前用 self.reply_text.lines().count() 在 markdown 代码块场景会低估
                // (代码块 1 行原始文本 = N 行渲染输出)
                let styled = self.reply_md.borrow_mut().all_styled(
                    ctx.theme(), false, max_width as usize,
                );
                let h = styled.len();
                h.max(1) as u16
            }
        }
    }

    fn render_body(&self, f: &mut Frame, ctx: &dyn SectionContext, inner: Rect, collapse: CardCollapse) {
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
                // 2026-06-11 FIX: 走 StreamingMd 渲染 markdown, 与 body_height 一致
                let styled = self.reply_md.borrow_mut().all_styled(
                    ctx.theme(), false, inner.width as usize,
                );
                for sl in styled {
                    let spans: Vec<Span> = sl.spans.into_iter()
                        .map(|s| Span::styled(s.text, s.style))
                        .collect();
                    lines.push(Line::from(spans));
                }
                // fallback: md 渲染为空 (如首帧) → 显示原始文本
                if lines.is_empty() && !self.reply_text.is_empty() {
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
