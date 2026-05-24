// ════════════════════════════════════════════════════════════════
// Bars — TopBar / StatusBar / InputBar 渲染函数
// ════════════════════════════════════════════════════════════════
//
// 引用关系：被 modes/chat.rs 等模式 render 方法调用
// 生命周期：每帧渲染，无副作用，无缓存

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::tui::state::{AbacusMode, AppState, Focus, InputState};
use crate::tui::theme::{SemanticIntent, Strength, TextRole};
use crate::tui::util::display_width;

use super::format_duration_ms;

// ════════════════════════════════════════════════════════════════
// TopBar — 顶部状态栏
// ════════════════════════════════════════════════════════════════

pub fn render_top_bar(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let mut spans = Vec::new();
    let width = area.width as usize;

    // 1. 状态指示器（最左侧）— Thinking 时用旋转动画
    let (status_icon, status_color) = if state.paused {
        ("⏸", state.theme.semantic_fg(SemanticIntent::Warning))
    } else if matches!(state.input_state, InputState::Thinking | InputState::Executing | InputState::Outputting) {
        // 旋转动画: 每 150ms 切换一帧（基于 op_started_at 的 elapsed）
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let elapsed_ms = state.op_started_at
            .map(|t| t.elapsed().as_millis() as usize)
            .unwrap_or(0);
        let frame_idx = (elapsed_ms / 150) % SPINNER.len();
        (SPINNER[frame_idx], state.theme.accent)
    } else if state.engine_handle.is_some() {
        ("●", state.theme.success)
    } else {
        ("○", state.theme.muted)
    };
    spans.push(Span::styled(format!(" {} ", status_icon), Style::default().fg(status_color)));

    // V29.9 (C1): plan-mode 视觉指示 — 启用时左侧亮显 [PLAN], 一轮即清
    //   引用: state.plan_mode (cmd_plan 写, run.rs pending_text 取后立即翻 false)
    //   生命周期: 启用 → 显示 → 一次发送 → 自动隐藏
    if state.plan_mode {
        spans.push(Span::styled(
            "[PLAN] ",
            Style::default()
                .fg(state.theme.semantic_fg(SemanticIntent::Warning))
                .add_modifier(Modifier::BOLD),
        ));
    }

    // V33: AbacusMode stepper — 顶栏展示 4 模式 DAG 流转可视化
    // V34-1: 加入 ✓/★ 状态符号 — 三态语义（✓=已完成 / ★=当前 / 无符号=未来）
    // 引用关系：state.mode 决定当前 step 高亮；其他态分流到 success(已过去) / muted(未来)
    // 设计：固定显示 3 个 step（澄清 ▸ 会诊|规划 ▸ 执行），DAG 起点固定为澄清
    // 生命周期：每帧 render 时根据 state.mode 重新计算，无副作用、无缓存
    if width >= 50 {
        let cur = state.mode;
        let accent_bold = Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD);
        let success = Style::default().fg(state.theme.success);
        let muted = Style::default().fg(state.theme.muted);
        let arrow_style = Style::default().fg(state.theme.border).add_modifier(Modifier::DIM);

        // step 1: 澄清（DAG 起点 — 只能是「当前」或「已过去」，永无"未来"态）
        // V34-1: ★ 当前 / ✓ 已过去
        let (s1_prefix, s1_style) = if cur == AbacusMode::Clarify {
            ("★ ", accent_bold)
        } else {
            ("✓ ", success)
        };
        spans.push(Span::styled(s1_prefix, s1_style));
        spans.push(Span::styled(AbacusMode::Clarify.display_zh(), s1_style));
        spans.push(Span::styled(" ▸ ", arrow_style));

        // step 2: 会诊 / 规划（按当前路径高亮）
        // V34-1: 当前 → ★ + 具体 label；已过去 → ✓ + 复合 label；未来 → 无符号
        let (s2_prefix, label2, s2_style) = match cur {
            AbacusMode::Meeting => ("★ ", AbacusMode::Meeting.display_zh(), accent_bold),
            AbacusMode::Plan => ("★ ", AbacusMode::Plan.display_zh(), accent_bold),
            AbacusMode::Team => ("✓ ", "会诊/规划", success), // 已经过去（路径已合并）
            AbacusMode::Clarify => ("  ", "会诊/规划", muted), // 未来态（占两格保持对齐）
        };
        spans.push(Span::styled(s2_prefix, s2_style));
        spans.push(Span::styled(label2, s2_style));
        spans.push(Span::styled(" ▸ ", arrow_style));

        // step 3: 执行（DAG 终点 — 只能是「当前」或「未来」，无"已过去"态）
        // V34-1: ★ 当前 / 无符号 未来
        let (s3_prefix, s3_style) = if cur == AbacusMode::Team {
            ("★ ", accent_bold)
        } else {
            ("  ", muted) // 未来态（占两格保持对齐）
        };
        spans.push(Span::styled(s3_prefix, s3_style));
        spans.push(Span::styled(AbacusMode::Team.display_zh(), s3_style));
        spans.push(Span::styled(" ▸ ", arrow_style));
    }

    // Logo + mode
    spans.push(Span::styled(
        "ABACUS",
        Style::default()
            .fg(state.theme.mode)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(" ▸ ", Style::default().fg(state.theme.border).add_modifier(Modifier::DIM)));

    // 2. 会话别名/总结
    //   V29.9: session_alias 优先(用户显式 /rename 设置, 强语义);
    //          无 alias → fallback 到 session_summary(自动生成);
    //          summary 也空 → 显示当前模式 label
    //   引用关系: state.session_alias 由 cmd_rename 写入, SessionExport 持久化
    if let Some(alias) = state.session_alias.as_deref().filter(|a| !a.is_empty()) {
        spans.push(Span::styled(
            alias,
            state.theme.text_style(TextRole::BodyEmphasis),
        ));
        // alias 后追加 summary 作为副标题(若存在), 节流以防顶栏过长
        if width >= 70 && !state.session_summary.is_empty() {
            spans.push(Span::styled(" · ", Style::default().fg(state.theme.border).add_modifier(Modifier::DIM)));
            let sub: String = state.session_summary.chars().take(30).collect();
            spans.push(Span::styled(
                sub,
                Style::default().fg(state.theme.muted),
            ));
        }
    } else {
        let summary = if state.session_summary.is_empty() {
            state.mode.label()
        } else {
            &state.session_summary
        };
        spans.push(Span::styled(
            summary,
            state.theme.text_style(TextRole::BodyEmphasis),
        ));
    }

    // 事件/工具调用计数
    // V28: 切到 trace_events SSOT(events 字段不再写入)
    let action_count = state.tool_records.len() + state.trace_events.len();
    if action_count > 0 {
        spans.push(Span::styled(
            format!(" {} ", action_count.min(99)),
            Style::default().fg(state.theme.muted),
        ));
    }

    // 模型简称
    // O(1) DoubleEndedIterator end-pick；避免 last() 遍历整串
    let model_short = state.model_name.split('-').next_back().unwrap_or(&state.model_name);
    if width >= 60 {
        spans.push(Span::styled(" · ", Style::default().fg(state.theme.border).add_modifier(Modifier::DIM)));
        spans.push(Span::styled(
            model_short,
            Style::default().fg(state.theme.muted),
        ));
    }

    // 输入字数统计
    if width >= 70 && !state.input.is_empty() {
        spans.push(Span::styled(" · ", Style::default().fg(state.theme.border).add_modifier(Modifier::DIM)));
        spans.push(Span::styled(
            format!("{}字", state.input.chars().count()),
            Style::default().fg(state.theme.muted),
        ));
    }

    // 动态快捷键提示（V32：按当前 focus / panel 状态精确给提示）
    if width >= 65 {
        let hint = if state.focus == Focus::Panel && state.panel_visible {
            "Tab/S-Tab切Tab · ↑↓滚动 · Esc回输入"
        } else if state.focus == Focus::CommandHint {
            "↑↓选命令 · Enter填充 · Esc回输入"
        } else if matches!(state.input_state, InputState::Thinking | InputState::Executing | InputState::Outputting) {
            "Esc取消"
        } else if !state.panel_visible {
            // V32 · 看板隐藏时显式告知如何打开
            "Ctrl+I 显示看板 · Ctrl+B 切焦点"
        } else {
            "Ctrl+B切焦点 · / 命令 · Tab补全"
        };
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            hint,
            state.theme.text_style(TextRole::Caption),
        ));
    }

    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line), area);
    // 无下划线——色条风格不需要额外水平分隔
}



// ════════════════════════════════════════════════════════════════
// StatusBar — 底部状态栏
// ════════════════════════════════════════════════════════════════

/// StatusBar — 底部状态栏（Go 版风格：无上划线，简洁格式）
///
/// 格式：`● t3 · 4ev                    Cmd+↑↓  Ctrl+B  Ctrl+D  Esc`
///
/// 引用关系：被 modes/chat.rs 等调用
/// 生命周期：每帧渲染
pub fn render_status_bar(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let status_icon = if state.paused { "⏸" } else { "●" };
    let status_color = if state.paused { state.theme.semantic_fg(SemanticIntent::Warning) } else { state.theme.success };
    let muted_dim = state.theme.text_style(TextRole::Caption);

    // V28: 切到 trace_events SSOT
    let evt_count = state.trace_events.len();
    let mut left = vec![
        Span::styled(format!("{} ", status_icon), Style::default().fg(status_color)),
        Span::styled(format!("t{} · {}ev", state.turn_count, evt_count), muted_dim),
    ];
    // LSP 诊断指示器（有错误/警告时显示）
    if state.lsp_diag_errors > 0 {
        left.push(Span::styled(
            format!("  ✗{}", state.lsp_diag_errors),
            state.theme.semantic_style(SemanticIntent::Danger, Strength::Default),
        ));
    }
    if state.lsp_diag_warnings > 0 {
        left.push(Span::styled(
            format!("  ⚠{}", state.lsp_diag_warnings),
            state.theme.semantic_style(SemanticIntent::Warning, Strength::Default),
        ));
    }

    // K1 焦点反馈：根据焦点 + 输入状态派生 Tab 意图 hint
    // V28.6 (PR12-3): paused 优先级最高 — 用户最需要知道"怎么退出暂停态"
    // V32: 三档焦点对应文案（Focus::Input 默认 / Panel / CommandHint）
    let right_text = if state.paused {
        "⏸ 已暂停 · Esc 继续"
    } else if matches!(state.input_state, InputState::Completing) {
        "Tab 候选 · Enter 确认 · Esc 取消"
    } else if state.focus == Focus::Panel && state.panel_visible {
        "[ ] 切看板Tab · ↑↓ 滚动 · Esc 回输入"
    } else if state.focus == Focus::CommandHint {
        "↑↓ 选命令 · Enter 填充 · Esc 回输入"
    } else {
        // Focus::Input（默认）
        "Tab 缩进 · Ctrl+Tab AI补全 · Ctrl+I 面板"
    };
    // paused 时 right hint 用 warning 色高亮 — 与 left status_icon 颜色一致, 视觉绑定
    let right_style = if state.paused {
        state.theme.semantic_style(SemanticIntent::Warning, Strength::Default)
    } else {
        muted_dim
    };
    let right = Span::styled(right_text, right_style);

    // 走 tui::util::display_width 统一治理（防止 chars 陷阱第 6 现场）
    let available = area.width.saturating_sub(2) as usize;
    let left_len: usize = left.iter().map(|s| display_width(s.content.as_ref())).sum();
    let right_len = display_width(right.content.as_ref());
    let gap = available.saturating_sub(left_len + right_len);

    let mut spans = left;
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }
    spans.push(right);

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ════════════════════════════════════════════════════════════════
// InputBar — 输入区域 (圆角边框 + 动态颜色 + 光标位置)
// ════════════════════════════════════════════════════════════════

/// 带焦点的输入框 (支持光标和焦点高亮)
/// 输入栏始终处于活跃状态（不参与焦点循环），使用 primary 色标识
pub fn render_input_bar_focused(f: &mut ratatui::Frame, state: &AppState, area: Rect, _focus: Focus) {
    use ratatui::widgets::block::Padding;

    // V32 · 三焦点视觉对称：
    //   - 输入框边框始终 primary（输入栏永远可接收字符，不变暗）
    //   - Focus::Input 时叠加 thick 上边框（与 Panel/CommandHint 同款锚点）
    //   - 200ms 脉冲叠 BOLD 提示刚切换
    let bar_color = state.theme.primary;

    let input_block = Block::default()
        .border_type(BorderType::Rounded)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(bar_color))
        .padding(Padding::horizontal(1))
        .style(Style::default().bg(state.theme.bg)); // 填充背景色，防止旧内容穿透

    let inner = input_block.inner(area);
    f.render_widget(input_block, area);

    // V32 · 焦点视觉强调：focus=Input 时叠加上边框 thick primary
    // 与 render_panel/render_shortcuts_hints 的视觉锚一致，三档焦点反馈对称
    let input_focused = state.focus == Focus::Input;
    if input_focused && area.width >= 3 {
        let mut top_style = Style::default().fg(state.theme.primary);
        if state.focus_pulsing() {
            top_style = top_style.add_modifier(Modifier::BOLD);
        }
        let top_segment = Rect {
            x: area.x.saturating_add(1),
            y: area.y,
            width: area.width.saturating_sub(2),
            height: 1,
        };
        let top_overlay = Block::default()
            .borders(Borders::TOP)
            .border_type(BorderType::Thick)
            .border_style(top_style);
        f.render_widget(top_overlay, top_segment);
    }

    let cursor_color = state.input_bar_color();
    let mut input_lines: Vec<Line> = Vec::new();

    // 顶栏提示：模型·思考深度·上下文占用(阈值变色)·模式
    let muted = state.theme.text_style(TextRole::Caption);
    match state.input_state {
        InputState::Thinking | InputState::Executing | InputState::Outputting => {
            input_lines.push(Line::from(vec![
                Span::styled(format!("⟳ {} · Esc 暂停", state.model_name), muted),
            ]));
        }
        InputState::Paused => {
            // V29.5: 文案对齐 — 与 StatusBar "Esc 继续" (line ~963) 统一,
            //   也与 Outputting 态 "Esc 暂停" (line ~1025) 形成"暂停 ↔ 继续"对称
            //   选"继续"而非"恢复": 更口语化, 符合"中断后继续"的二元直觉
            input_lines.push(Line::from(vec![
                Span::styled("⏸ 已暂停 · Esc 继续".to_string(), muted),
            ]));
        }
        _ => {
            let real_tokens = state.session_tokens.total_tokens as usize;
            let (pct, used_str, max_str) = if state.context_window > 0 {
                let used = if real_tokens > 0 { real_tokens } else {
                    // fallback: rough estimate
                    state.messages.iter()
                        .flat_map(|m| m.parts.iter())
                        .map(|p| match p {
                            crate::tui::state::MsgContent::Stream(s) => s.len(),
                            crate::tui::state::MsgContent::Block { summary, detail, .. } => summary.len() + detail.len(),
                            // V28: Trace 仅持有 u64 引用,不直接占用 token 估算字节;
                            // 真实 thinking/tool 内容在 trace_events 里(调用方按需访问)
                            crate::tui::state::MsgContent::Trace { event_ids, .. } => event_ids.len() * 8,
                        })
                        .sum::<usize>() / 3
                };
                let pct = (used * 100 / state.context_window).min(99);
                let used_str = format_ctx(used);
                let max_str = format_ctx(state.context_window);
                (pct, used_str, max_str)
            } else {
                (0, "?".to_string(), "?".to_string())
            };
            let pct_color = if pct >= 80 { state.theme.error }
                else if pct >= 50 { state.theme.gold }
                else { state.theme.success };
            input_lines.push(Line::from(vec![
                Span::styled(format!("{} · {} · ", state.model_name, state.thinking_depth), muted),
                Span::styled(format!("{}%", pct), Style::default().fg(pct_color).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {}/{} · {}", used_str, max_str, state.mode.label()), muted),
            ]));
        }
    };

    // 中间：输入文本区（始终 2 行）
    let display_lines: Vec<&str> = state.input.lines().collect();
    let start = display_lines.len().saturating_sub(2);
    let visible_lines: Vec<&str> = if display_lines.is_empty() {
        vec!["", ""]
    } else {
        let mut v: Vec<&str> = display_lines[start..].to_vec();
        while v.len() < 2 {
            v.push("");
        }
        v
    };

    // 使用缓存的 cursor_line（O(1)，无需每帧重新计算）
    let cursor_visible_line = if state.cursor_line >= start && state.cursor_line < start + visible_lines.len() {
        state.cursor_line - start
    } else {
        visible_lines.len().saturating_sub(1)
    };

    for (i, line) in visible_lines.iter().enumerate() {
        if i == cursor_visible_line {
            let char_pos = state.cursor_col.min(line.chars().count());
            let line_chars: Vec<char> = line.chars().collect();
            let split_pos = char_pos.min(line_chars.len());
            let left: String = line_chars[..split_pos].iter().collect();
            let right: String = line_chars[split_pos..].iter().collect();
            input_lines.push(Line::from(vec![
                Span::styled(left, Style::default().fg(state.theme.text)),
                Span::styled("▎", Style::default().fg(cursor_color)),
                Span::styled(right, Style::default().fg(state.theme.text)),
            ]));
        } else {
            input_lines.push(Line::styled(line.to_string(), Style::default().fg(state.theme.text)));
        }
    }

    // 底栏：左侧状态(spinner + phase + elapsed) ，右侧 ⏎ Enter / Esc 取消
    let spinner = || {
        let tick = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() / 120;
        match tick % 10 {
            0 => "⠋", 1 => "⠙", 2 => "⠹", 3 => "⠸", 4 => "⠼",
            5 => "⠴", 6 => "⠦", 7 => "⠧", 8 => "⠇", _ => "⠏",
        }
    };
    let total_secs = state.accumulated_elapsed.as_secs_f64()
        + state.op_started_at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
    let elapsed = if total_secs > 0.0 {
        let ms = (total_secs * 1000.0) as u64;
        let d = format_duration_ms(ms);
        if d.is_empty() { String::new() } else { format!(" {}", d) }
    } else { String::new() };
    let phase = &state.processing_phase;
    let phase_sep = if phase.is_empty() { "" } else { " " };
    let (status_text, status_color) = match state.input_state {
        InputState::Thinking => (format!("{} Thinking{}{}{}", spinner(), phase_sep, phase, elapsed), state.theme.accent),
        InputState::Executing => (format!("{} Working{}{}{}", spinner(), phase_sep, phase, elapsed), state.theme.gold),
        InputState::Outputting => (format!("{} Outputting{}", spinner(), elapsed), state.theme.success),
        InputState::Paused => ("⏸ Paused".into(), state.theme.semantic_fg(SemanticIntent::Warning)),
        _ if state.engine_handle.is_some() => ("● Ready".into(), state.theme.success),
        _ => ("● Ready".into(), state.theme.muted),
    };
    // 右侧提示：繁忙时显示 Esc 取消，否则 ⏎ Enter
    let is_busy = matches!(state.input_state,
        InputState::Thinking |
        InputState::Executing |
        InputState::Outputting);
    let right_hint_text = if is_busy { "Esc 取消" } else { "⏎ Enter" };
    let right_style = if is_busy {
        state.theme.text_style(TextRole::Caption)
    } else {
        Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)
    };
    // 走 tui::util::display_width 统一治理（CJK 文本如"Esc 取消"按显示列宽算 fill）
    let fill = inner.width.saturating_sub(
        (display_width(status_text.as_str()) + display_width(right_hint_text)) as u16,
    ).max(1);
    input_lines.push(Line::from(vec![
        Span::styled(status_text, Style::default().fg(status_color)),
        Span::raw(" ".repeat(fill as usize)),
        Span::styled(right_hint_text, right_style),
    ]));

    f.render_widget(Paragraph::new(input_lines), inner);

    // 始终显示光标
    {
        let cursor_x = inner.x + state.cursor_col as u16;
        let cursor_y = inner.y + 1 + cursor_visible_line as u16;
        f.set_cursor_position((cursor_x, cursor_y));
    }
}

/// 格式化上下文窗口大小为人类可读：1_000_000 → "1M", 500_000 → "500K"
pub(super) fn format_ctx(n: usize) -> String {
    if n >= 1_000_000 && n % 1_000_000 == 0 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 && n % 1_000 == 0 {
        format!("{}K", n / 1_000)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
