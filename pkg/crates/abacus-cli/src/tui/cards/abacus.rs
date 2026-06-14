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
    /// V42-B: 解析后的工具输出（用于格式化渲染）
    parsed_outputs: Vec<Option<crate::tui::cards::writer::ToolOutputParsed>>,
}

impl AbacusCard {
    pub fn new(id: u64, tool_name: impl Into<String>) -> Self {
        Self {
            id,
            tool_name: tool_name.into(),
            events: Vec::new(),
            streaming: CardStreaming::Active,
            parsed_outputs: Vec::new(),
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

    /// V42-B: 存储解析后的工具输出
    pub fn set_last_call_parsed(&mut self, parsed: crate::tui::cards::writer::ToolOutputParsed) {
        self.parsed_outputs.push(Some(parsed));
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
        let title = format!("\u{2699} {}", self.tool_name);
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
                let _avail = max_width.saturating_sub(4).max(20) as usize;
                for event in &self.events {
                    if let TraceKind::ToolCall { name, args: _, output, .. } = &event.kind {
                        let lower = name.to_lowercase();
                        if lower == "fs_edit" || lower == "fs_write" {
                            h = h.saturating_add(15); // diff 视图上限
                            continue;
                        }
                        // V42-B: 主行（status_icon + command）= 1 行
                        h = h.saturating_add(1);

                        // 输出区域
                        if let Some(out) = output {
                            // 解析 JSON 提取 stdout
                            let parsed = serde_json::from_str::<serde_json::Value>(out).ok();
                            let stdout = parsed.as_ref()
                                .and_then(|j| j.get("stdout").and_then(|v| v.as_str()))
                                .unwrap_or("");
                            if !stdout.is_empty() {
                                // 命令回显行（│ ⤷ command）= 1 行
                                h = h.saturating_add(1);
                                // stdout 行（最多 10 行）
                                let stdout_line_count = stdout.lines().count().min(10) as u16;
                                h = h.saturating_add(stdout_line_count);
                                // 截断提示行
                                if stdout.lines().count() > 10 {
                                    h = h.saturating_add(1);
                                }
                            }
                        }
                        // 每组之间加 1 行呼吸空行
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
                let border_style = Style::default().fg(ctx.theme().border);
                let muted_style = Style::default().fg(ctx.theme().muted);
                let text_style = Style::default().fg(ctx.theme().text);
                let text_bold = Style::default().fg(ctx.theme().text).add_modifier(Modifier::BOLD);

                for (idx, event) in self.events.iter().enumerate() {
                    if let TraceKind::ToolCall { name, args, output, status } = &event.kind {
                        // fs_edit / fs_write 走 diff 视图
                        let lower = name.to_lowercase();
                        if lower == "fs_edit" || lower == "fs_write" {
                            if let Some(diff_lines) = crate::tui::components::block_detail::try_render_edit_diff_with_output(
                                name, args, output.as_deref(), ctx.theme(), 12,
                            ) {
                                lines.extend(diff_lines);
                                if idx + 1 < event_count {
                                    lines.push(Line::raw(""));
                                }
                                continue;
                            }
                        }

                        // V42-B: 格式化渲染 — 提取 command 和 stdout
                        let (status_icon, color) = status_icon_and_color(*status, ctx);
                        let parsed = output.as_ref().map(|o| {
                            crate::tui::cards::writer::parse_tool_output_from_str(name, o)
                        });

                        // 主行：状态图标 + 命令
                        let command = parsed.as_ref()
                            .map(|p| p.command.clone())
                            .unwrap_or_else(|| {
                                // 从 args JSON 提取 command 字段
                                serde_json::from_str::<serde_json::Value>(args)
                                    .ok()
                                    .and_then(|j| j.get("command").and_then(|v| v.as_str()).map(String::from))
                                    .unwrap_or_else(|| name.clone())
                            });
                        lines.push(Line::from(vec![
                            Span::styled(format!("{} ", status_icon), Style::default().fg(color)),
                            Span::styled(command, text_bold),
                        ]));

                        // 输出行：│ ⤷ command 回显 + │ stdout
                        if let Some(p) = &parsed {
                            if !p.stdout_full.is_empty() {
                                // 命令回显
                                lines.push(Line::from(vec![
                                    Span::styled("\u{2502} ", border_style),
                                    Span::styled("\u{2937} ", muted_style),
                                    Span::styled(p.command.clone(), muted_style),
                                ]));
                                // stdout（最多 10 行）
                                let stdout_lines: Vec<&str> = p.stdout_full.lines().collect();
                                let total_lines = stdout_lines.len();
                                for line in stdout_lines.iter().take(10) {
                                    lines.push(Line::from(vec![
                                        Span::styled("\u{2502} ", border_style),
                                        Span::styled("  ", text_style),
                                        Span::styled(line.to_string(), text_style),
                                    ]));
                                }
                                // 截断提示
                                if total_lines > 10 {
                                    lines.push(Line::from(vec![
                                        Span::styled("\u{2502} ", border_style),
                                        Span::styled(
                                            format!("  \u{2026} +{} lines", total_lines - 10),
                                            muted_style,
                                        ),
                                    ]));
                                }
                            }
                        } else if let Some(out) = output {
                            // fallback: 未解析的输出
                            let out_trimmed = out.trim();
                            if !out_trimmed.is_empty() {
                                for line in out_trimmed.lines().take(5) {
                                    lines.push(Line::from(vec![
                                        Span::styled("\u{2502} ", border_style),
                                        Span::styled("  ", text_style),
                                        Span::styled(line.to_string(), text_style),
                                    ]));
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
                        "(working\u{2026})",
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
    // V42-B+: 状态语义符号参考 OpenCode TUI 工具列表
    //   * tool_call  → tool_result  ✗ tool_failed  ⠋ tool_running
    // 设计意图：
    //   - Running 用旋转箭头，提示正在执行
    //   - Success 用 → 强调"产出了结果"，配合 muted 颜色弱化（OpenCode 风格）
    //   - Failed 用 ✗ 醒目颜色让用户快速定位失败
    match status {
        ToolStatus::Running => ("\u{2807}", ctx.theme().accent),     // ⠇ 旋转箭头
        ToolStatus::Success => ("\u{2192}", ctx.theme().muted),     // → 结果（弱化）
        ToolStatus::Failed  => ("\u{2717}", ctx.theme().error),     // ✗ 失败（醒目）
    }
}

/// 构建 Collapsed 态单行横向摘要
fn build_collapsed_summary(events: &[TraceEvent], tool_name: &str) -> String {
    // 找最后一个 ToolCall
    for event in events.iter().rev() {
        if let TraceKind::ToolCall { name, args, status, .. } = &event.kind {
            // V42-B+: 与 status_icon_and_color 保持一致的语义符号
            let icon = match status {
                ToolStatus::Running => "\u{2807}",     // ⠇
                ToolStatus::Success => "\u{2192}",     // →
                ToolStatus::Failed  => "\u{2717}",     // ✗
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

#[cfg(test)]
mod tests {
    use super::*;

    /// V42-B+: 验证 status_icon 语义符号映射（OpenCode 风格）
    /// Running → ⠇ (旋转箭头)
    /// Success → → (结果箭头，弱化)
    /// Failed → ✗ (失败醒目)
    #[test]
    fn status_icon_matches_semantic_symbols() {
        // 验证字符常量（不依赖 theme，颜色参数被忽略）
        let (running_icon, _) = status_icon_and_color(ToolStatus::Running, &TestCtx);
        let (success_icon, _) = status_icon_and_color(ToolStatus::Success, &TestCtx);
        let (failed_icon, _) = status_icon_and_color(ToolStatus::Failed, &TestCtx);

        assert_eq!(running_icon, "\u{2807}", "running should use spinner ⠇");
        assert_eq!(success_icon, "\u{2192}", "success should use arrow →");
        assert_eq!(failed_icon, "\u{2717}", "failed should use cross ✗");
    }

    /// V42-B+: collapsed summary 也使用相同的语义符号
    #[test]
    fn collapsed_summary_uses_same_icons() {
        use crate::tui::state::{EventLevel, TraceKind, TraceEvent};
        let events = vec![
            TraceEvent {
                id: 1,
                time: "12:00".into(),
                category: "tool".into(),
                level: EventLevel::Info,
                kind: TraceKind::ToolCall {
                    name: "fs_read".into(),
                    args: "{\"path\":\"/foo/bar.rs\"}".into(),
                    output: Some("...".into()),
                    status: ToolStatus::Success,
                },
                duration_ms: Some(50),
            },
        ];
        let summary = build_collapsed_summary(&events, "fs_read");
        assert!(summary.starts_with('\u{2192}'), "collapsed summary should start with → for success");
        assert!(summary.contains("fs_read"));
        assert!(summary.contains("/foo/bar.rs"));
    }

    /// 失败的 collapsed summary 应以 ✗ 开头
    #[test]
    fn collapsed_summary_failed_uses_cross() {
        use crate::tui::state::{EventLevel, TraceKind, TraceEvent};
        let events = vec![
            TraceEvent {
                id: 1,
                time: "12:00".into(),
                category: "tool".into(),
                level: EventLevel::Warning,
                kind: TraceKind::ToolCall {
                    name: "bash".into(),
                    args: "{}".into(),
                    output: Some("error".into()),
                    status: ToolStatus::Failed,
                },
                duration_ms: Some(100),
            },
        ];
        let summary = build_collapsed_summary(&events, "bash");
        assert!(summary.starts_with('\u{2717}'), "failed summary should start with ✗");
    }

    // 测试用 ctx（避免引入完整 Theme）
    use abacus_ui_kit::Theme;
    use abacus_ui_kit::section::SectionContext;
    struct TestTheme(Theme);
    impl SectionContext for TestTheme {
        fn theme(&self) -> &Theme { &self.0 }
    }
    static TEST_THEME: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();
    fn test_theme() -> &'static Theme {
        TEST_THEME.get_or_init(Theme::brand)
    }
    struct TestCtxWrap;
    impl SectionContext for TestCtxWrap {
        fn theme(&self) -> &Theme { test_theme() }
    }
    // TestCtx 是一个零大小类型别名
    use TestCtxWrap as TestCtx;
}
