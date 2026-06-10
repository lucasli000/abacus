//! AbacusCard —— Abacus 本地工作卡
//!
//! V42-B 内置 Card（最复杂）。实现 [`MessageCard`] trait。
//!
//! ## 内容
//!
//! - 工具调用 (ToolCall trace events)
//! - Generic 事件
//! - EditDiff 视图 (fs_edit 等)
//! - Header: `● Abacus · {tool_name}`
//!
//! ## 折叠策略
//!
//! 默认 Collapsed —— 工具调用详情默认折叠, 只显示路径/命令摘要。

use abacus_ui_kit::prelude::*;
use abacus_ui_kit::SectionContext;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::tui::state::{TraceEvent, TraceKind, ToolStatus};

/// Abacus 工作卡片
pub struct AbacusCard {
    id: u64,
    /// 主工具名 (如 "fs_edit", "bash")
    tool_name: String,
    /// 关联的 trace events (按时间顺序)
    events: Vec<TraceEvent>,
    streaming: CardStreaming,
}

impl AbacusCard {
    pub fn new(id: u64, tool_name: impl Into<String>) -> Self {
        Self {
            id,
            tool_name: tool_name.into(),
            events: Vec::new(),
            streaming: CardStreaming::Active,
        }
    }

    pub fn push_event(&mut self, event: TraceEvent) {
        self.events.push(event);
    }

    /// V42-B 升级路径: 暴露 events 字段 (给 state.collect_trace_events 用)
    pub fn events_ref(&self) -> &Vec<TraceEvent> {
        &self.events
    }

    pub fn set_streaming(&mut self, s: CardStreaming) {
        self.streaming = s;
    }

    /// 供复制功能读取 events 纯文本
    /// V42-B: extract_selection_text 调用, 复制所有 trace events 内容
    /// 格式: 每 event 一行, 形如 "[tool: name] content" 或 "[thinking] content"
    pub fn events_text_for_copy(&self) -> String {
        use crate::tui::state::TraceKind;
        let mut out = String::new();
        for ev in &self.events {
            match &ev.kind {
                TraceKind::Generic { content } => {
                    out.push_str(content);
                    out.push('\n');
                }
                TraceKind::Thinking { text, .. } => {
                    out.push_str("[thinking]\n");
                    out.push_str(text);
                    if !text.ends_with('\n') { out.push('\n'); }
                }
                TraceKind::ToolCall { name, args, output, .. } => {
                    out.push_str(&format!("[tool: {}]\n", name));
                    if !args.is_empty() {
                        out.push_str("args: ");
                        out.push_str(args);
                        out.push('\n');
                    }
                    if let Some(o) = output {
                        out.push_str("output: ");
                        out.push_str(o);
                        if !o.ends_with('\n') { out.push('\n'); }
                    }
                }
                TraceKind::Reply { tokens } => {
                    out.push_str(&format!("[reply: {} tokens]\n", tokens));
                }
            }
        }
        out
    }
}

impl MessageCard for AbacusCard {
    fn kind(&self) -> CardKind { kinds::ABACUS }
    fn id(&self) -> u64 { self.id }

    fn header(&self, ctx: &dyn SectionContext) -> CardHeader {
        let title = format!("● Abacus · {}", self.tool_name);
        CardHeader::new(title, "")
            .with_color(ctx.theme().abacus)
    }

    fn streaming(&self) -> CardStreaming { self.streaming }
    fn default_collapse(&self) -> CardCollapse { CardCollapse::Expanded }

    fn body_height(&self, _ctx: &dyn SectionContext, _max_width: u16, collapse: CardCollapse) -> u16 {
        match collapse {
            CardCollapse::Headless => 0,
            CardCollapse::Collapsed => {
                // 单行摘要: 工具名 + 状态
                1
            }
            CardCollapse::Expanded => {
                // 每个 event 至少 1 行
                self.events.len().max(1) as u16
            }
        }
    }

    fn render_body(&self, f: &mut Frame, ctx: &dyn SectionContext, inner: Rect, collapse: CardCollapse) {
        match collapse {
            CardCollapse::Headless => return,
            CardCollapse::Collapsed => {
                let summary = if self.events.is_empty() {
                    format!("{} · running…", self.tool_name)
                } else {
                    format!("{} · {} events", self.tool_name, self.events.len())
                };
                let p = Paragraph::new(Line::from(Span::styled(
                    summary,
                    Style::default().fg(ctx.theme().text),
                )));
                f.render_widget(p, inner);
            }
            CardCollapse::Expanded => {
                let mut lines: Vec<Line> = Vec::new();
                for event in &self.events {
                    match &event.kind {
                        TraceKind::Generic { content } => {
                            lines.push(Line::from(Span::styled(
                                format!("   ○ {}", content),
                                Style::default().fg(ctx.theme().muted),
                            )));
                        }
                        TraceKind::Thinking { text, .. } => {
                            lines.push(Line::from(Span::styled(
                                format!("   ~ thinking: {}", text.lines().next().unwrap_or("")),
                                Style::default().fg(ctx.theme().accent).add_modifier(Modifier::DIM),
                            )));
                        }
                        TraceKind::ToolCall { name, args, output, status } => {
                            let status_icon = match status {
                                ToolStatus::Running => "●",
                                ToolStatus::Success => "✓",
                                ToolStatus::Failed => "✗",
                            };
                            let color = match status {
                                ToolStatus::Success => ctx.theme().success,
                                ToolStatus::Failed => ctx.theme().error,
                                _ => ctx.theme().muted,
                            };
                            lines.push(Line::from(vec![
                                Span::styled(format!("   {} ", status_icon), Style::default().fg(color)),
                                Span::styled(name.clone(), Style::default().fg(ctx.theme().text).add_modifier(Modifier::BOLD)),
                                Span::styled(format!(" · {}", args.lines().next().unwrap_or("")), Style::default().fg(ctx.theme().muted)),
                            ]));
                            if let Some(ref out) = output {
                                for line in out.lines().take(3) {
                                    lines.push(Line::from(Span::styled(
                                        format!("     │ {}", line),
                                        Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM),
                                    )));
                                }
                                if out.lines().count() > 3 {
                                    lines.push(Line::from(Span::styled(
                                        "     │ …",
                                        Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM),
                                    )));
                                }
                            }
                        }
                        TraceKind::Reply { tokens } => {
                            lines.push(Line::from(Span::styled(
                                format!("   ● reply · {} tokens", tokens),
                                Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM),
                            )));
                        }
                    }
                }
                if lines.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "(working…)",
                        Style::default().fg(ctx.theme().muted).add_modifier(Modifier::DIM),
                    )));
                }
                let p = Paragraph::new(lines).wrap(Wrap { trim: false });
                f.render_widget(p, inner);
            }
        }
    }
}
