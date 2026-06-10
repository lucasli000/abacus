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

    /// 将 output 绑定到最后一个 ToolCall event（替代 push 独立 Generic event）
    pub fn set_last_call_output(&mut self, out: String) {
        for event in self.events.iter_mut().rev() {
            if let TraceKind::ToolCall { ref mut output, .. } = event.kind {
                *output = Some(out);
                break;
            }
        }
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
    fn default_collapse(&self) -> CardCollapse { CardCollapse::Collapsed }

    fn body_height(&self, _ctx: &dyn SectionContext, max_width: u16, collapse: CardCollapse) -> u16 {
        match collapse {
            CardCollapse::Headless => 0,
            CardCollapse::Collapsed => 1,
            CardCollapse::Expanded => {
                let mut h = 0u16;
                let avail = max_width.saturating_sub(4).max(20) as usize;
                for event in &self.events {
                    if let TraceKind::ToolCall { name, args, output, .. } = &event.kind {
                        let lower = name.to_lowercase();
                        if lower == "fs_edit" || lower == "fs_write" {
                            h = h.saturating_add(15); // diff 视图上限
                            continue;
                        }
                        // 主行：args 在可用宽度内的 wrap 行数（宽松上限）
                        let args_text = args.lines().next().unwrap_or("").trim();
                        let args_len = args_text.len();
                        let args_lines = ((args_len / avail) + 1).min(4) as u16;
                        h = h.saturating_add(args_lines.max(1));
                        // output 行：同样在可用宽度内 wrap
                        if let Some(out) = output {
                            let out_len = out.trim().len();
                            let out_lines = ((out_len / avail) + 1).min(4) as u16;
                            h = h.saturating_add(out_lines.max(1));
                        }
                        // 每组之间加 1 行呼吸空行（避免过密）
                        h = h.saturating_add(1);
                    }
                }
                // 去掉最后一组的空行
                h = h.saturating_sub(1);
                h.max(1)
            }
        }
    }

    fn render_body(&self, f: &mut Frame, ctx: &dyn SectionContext, inner: Rect, collapse: CardCollapse) {
        match collapse {
            CardCollapse::Headless => return,
            CardCollapse::Collapsed => {
                let summary = build_collapsed_summary(&self.events, &self.tool_name);
                let p = Paragraph::new(Line::from(Span::styled(
                    summary,
                    Style::default().fg(ctx.theme().text),
                )));
                f.render_widget(p, inner);
            }
            CardCollapse::Expanded => {
                let mut lines: Vec<Line> = Vec::new();
                let event_count = self.events.len();
                for (idx, event) in self.events.iter().enumerate() {
                    if let TraceKind::ToolCall { name, args, output, status } = &event.kind {
                        // fs_edit / fs_write 走 diff 视图
                        let lower = name.to_lowercase();
                        if lower == "fs_edit" || lower == "fs_write" {
                            if let Some(diff_lines) = crate::tui::components::block_detail::try_render_edit_diff_with_output(
                                name, args, output.as_deref(), ctx.theme(), 12,
                            ) {
                                lines.extend(diff_lines);
                                continue;
                            }
                        }
                        // 通用工具调用展示：主行（状态+工具名+args）
                        let (status_icon, color) = status_icon_and_color(*status, ctx);
                        let mut main_spans = vec![
                            Span::styled(format!("{} ", status_icon), Style::default().fg(color)),
                            Span::styled(name.clone(), Style::default().fg(ctx.theme().text).add_modifier(Modifier::BOLD)),
                        ];
                        let args_text = args.lines().next().unwrap_or("").trim();
                        if !args_text.is_empty() {
                            main_spans.push(Span::styled(
                                format!(" · {}", args_text),
                                Style::default().fg(ctx.theme().muted),
                            ));
                        }
                        lines.push(Line::from(main_spans));

                        // output 行：浅缩进 + → 前缀，不再主动截断
                        if let Some(out) = output {
                            let out_trimmed = out.trim();
                            if !out_trimmed.is_empty() {
                                let mut out_spans = vec![
                                    Span::styled("  → ", Style::default().fg(ctx.theme().muted)),
                                ];
                                // output 如果是多行，拆成多个 Line
                                let out_lines: Vec<&str> = out_trimmed.lines().collect();
                                if out_lines.is_empty() {
                                    out_spans.push(Span::styled(out_trimmed.to_string(), Style::default().fg(ctx.theme().text)));
                                    lines.push(Line::from(out_spans));
                                } else {
                                    // 第一行接在 → 后面
                                    out_spans.push(Span::styled(out_lines[0].to_string(), Style::default().fg(ctx.theme().text)));
                                    lines.push(Line::from(out_spans));
                                    // 后续行对齐到 → 后的起始列（2 空格 + "→ " = 4 字符缩进）
                                    for line in &out_lines[1..] {
                                        lines.push(Line::from(vec![
                                            Span::styled("    ", Style::default().fg(ctx.theme().muted)),
                                            Span::styled(line.to_string(), Style::default().fg(ctx.theme().text)),
                                        ]));
                                    }
                                }
                            }
                        }

                        // 组间空行（最后一条不加）
                        if idx + 1 < event_count {
                            lines.push(Line::raw(""));
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

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 内部辅助
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn status_icon_and_color(status: ToolStatus, ctx: &dyn SectionContext) -> (&'static str, ratatui::style::Color) {
    match status {
        ToolStatus::Running => ("●", ctx.theme().muted),
        ToolStatus::Success => ("✓", ctx.theme().success),
        ToolStatus::Failed => ("✗", ctx.theme().error),
    }
}

/// 构建 Collapsed 态单行横向摘要
fn build_collapsed_summary(events: &[TraceEvent], tool_name: &str) -> String {
    // 找最后一个 ToolCall
    for event in events.iter().rev() {
        if let TraceKind::ToolCall { name, args, status, .. } = &event.kind {
            let icon = match status {
                ToolStatus::Running => "●",
                ToolStatus::Success => "✓",
                ToolStatus::Failed => "✗",
            };
            let param = extract_key_param(args);
            if let Some(p) = param {
                return format!("{} {} · {}", icon, name, p);
            } else {
                return format!("{} {}", icon, name);
            }
        }
    }
    format!("{} · running…", tool_name)
}

/// 从 args 中提取关键参数（path/command 优先）
fn extract_key_param(args: &str) -> Option<String> {
    // 尝试简单 JSON 匹配："path": "..." 或 "command": "..."
    for key in &["\"path\"", "\"command\"", "\"file\""] {
        if let Some(start) = args.find(key) {
            let after = &args[start + key.len()..];
            if let Some(colon) = after.find(':') {
                let val_start = after[colon + 1..].trim_start();
                if let Some(first_quote) = val_start.find('"') {
                    let rest = &val_start[first_quote + 1..];
                    if let Some(end_quote) = rest.find('"') {
                        let val = &rest[..end_quote];
                        if !val.is_empty() {
                            return Some(truncate_display(val, 30));
                        }
                    }
                }
            }
        }
    }
    // fallback：取第一行非空内容
    args.lines()
        .find(|l| !l.trim().is_empty())
        .map(|s| truncate_display(s.trim(), 30))
}

/// 按显示宽度截断字符串（CJK 安全）
fn truncate_display(s: &str, max_w: usize) -> String {
    use crate::tui::util::display_width;
    if display_width(s) <= max_w {
        return s.to_string();
    }
    let mut w = 0usize;
    let mut end = 0usize;
    for (i, ch) in s.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > max_w.saturating_sub(1) {
            break;
        }
        w += cw;
        end = i + ch.len_utf8();
    }
    format!("{}…", &s[..end])
}
