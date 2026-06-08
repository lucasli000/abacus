// ════════════════════════════════════════════════════════════════
// Bars — TopBar / StatusBar / InputBar 渲染函数
// ════════════════════════════════════════════════════════════════
//
// 引用关系：被 modes/chat.rs 等模式 render 方法调用
// 生命周期：每帧渲染，无副作用，无缓存

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::tui::i18n::t;
use crate::tui::state::{AppState, Focus, InputState};
use abacus_ui_kit::{SemanticIntent, Strength, TextRole};
use crate::tui::util::display_width;

use super::format_duration_ms;

// ════════════════════════════════════════════════════════════════
// TopBar — 顶部状态栏
// ════════════════════════════════════════════════════════════════

pub fn render_top_bar(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let width = area.width as usize;

    // ── 左侧: spinner + ABACUS ▸ session_title · mode ──
    let mut left = Vec::new();

    // 状态指示器 — Thinking 时旋转动画
    let (status_icon, status_color) = if state.paused {
        ("⏸", state.theme.semantic_fg(SemanticIntent::Warning))
    } else if matches!(state.input_state, InputState::Thinking | InputState::Executing | InputState::Outputting) {
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
    left.push(Span::styled(format!(" {} ", status_icon), Style::default().fg(status_color)));

    // V34: plan_mode 字段已删除（/plan-prefix 功能已随 plan_mode 字段移除）

    // ABACUS logo（纯文本）
    left.push(Span::styled(
        "ABACUS",
        Style::default()
            .fg(state.theme.mode)
            .add_modifier(Modifier::BOLD),
    ));
    left.push(Span::styled(" ▸ ", Style::default().fg(state.theme.border).add_modifier(Modifier::DIM)));

    // session title（alias 优先 → summary → mode label）
    let title: String = if let Some(alias) = state.session_alias.as_deref().filter(|a| !a.is_empty()) {
        alias.to_string()
    } else if !state.session_summary.is_empty() {
        state.session_summary.clone()
    } else {
        state.mode.label().to_string()
    };
    // 截断 title 防止超出（预留右侧 model_name 空间：至少 model.len() + 4）
    let model_name = &state.model_name;
    let right_reserved = model_name.len() + 4; // " · " + model + padding
    let max_title_chars = width.saturating_sub(right_reserved + 20); // 20 = left prefix 估算
    let truncated_title: String = title.chars().take(max_title_chars).collect();
    left.push(Span::styled(
        truncated_title,
        state.theme.text_style(TextRole::BodyEmphasis),
    ));

    // Mode 标签不在 TopBar 重复显示（StatusBar 已有唯一模式指示）

    // V35: 策略执行徽章 — Plan/Team 策略运行时显示在模式标签旁
    // 检测依据: processing_phase 前缀（run.rs 写入时已加前缀 plan/team）
    // 引用关系: run.rs 设置 state.processing_phase → 此处读取
    if state.processing_phase.starts_with("planning") {
        left.push(Span::styled(
            format!(" [PLAN]"),
            Style::default().fg(state.theme.gold).add_modifier(Modifier::BOLD),
        ));
    } else if state.processing_phase.starts_with("team") {
        left.push(Span::styled(
            format!(" [TEAM]"),
            Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD),
        ));
    }

    // ── 右侧: model_name ──
    let right = Span::styled(
        format!("{} ", model_name),
        Style::default().fg(state.theme.muted),
    );

    // 计算左右间距，确保右侧不溢出
    let left_len: usize = left.iter().map(|s| display_width(s.content.as_ref())).sum();
    let right_len = display_width(right.content.as_ref());
    let gap = width.saturating_sub(left_len + right_len);

    let mut spans = left;
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }
    spans.push(right);

    f.render_widget(Paragraph::new(Line::from(spans)), area);
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
    let muted_dim = state.theme.text_style(TextRole::Caption);

    // ── 左侧：mode 标签（mode_color）──
    let mode_label = match state.mode {
        crate::tui::state::AbacusMode::Clarify => t("mode.clarify"),
        crate::tui::state::AbacusMode::Meeting => t("mode.meeting"),
    };
    let mode_color = state.theme.mode;
    let status_icon = if state.connection_error { "⚠" }
        else if state.paused { "⏸" }
        else { "●" };
    let status_color = if state.connection_error { state.theme.error }
        else if state.paused { state.theme.semantic_fg(SemanticIntent::Warning) }
        else { state.theme.success };
    let mut left = vec![
        Span::styled(format!("{} ", status_icon), Style::default().fg(status_color)),
        Span::styled(mode_label, Style::default().fg(mode_color).add_modifier(Modifier::BOLD)),
    ];
    // 网络异常时在 mode 后追加红色提示
    if state.connection_error {
        left.push(Span::styled(
            t("status.network_error"),
            Style::default().fg(state.theme.error).add_modifier(Modifier::BOLD),
        ));
    }

    // ── 中间：过渡提示（5s）> 策略阶段（着色）> 普通 phase ──
    // V35: 优先级: transition_hint(5s) > strategy phase(accent) > normal(muted)
    let phase_shown = if let Some((hint, at)) = &state.transition_hint {
        if at.elapsed().as_secs() < 5 {
            // 过渡提示 5s 内展示，accent 色区分
            left.push(Span::styled(
                format!(" · {}", hint),
                Style::default().fg(state.theme.accent),
            ));
            true
        } else { false }
    } else { false };

    if !phase_shown && !state.processing_phase.is_empty() {
        // 策略运行时用 accent 色，普通状态用 muted
        let phase_style = if state.processing_phase.starts_with("planning")
            || state.processing_phase.starts_with("team") {
            Style::default().fg(state.theme.accent)
        } else {
            muted_dim
        };
        left.push(Span::styled(
            format!(" · {}", state.processing_phase),
            phase_style,
        ));
    }

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

    // ── 右侧：token 计数（紧凑格式）+ 快捷键 hint ──
    let real_tokens = state.session_tokens.total_tokens as usize;
    let tok_str = if real_tokens > 0 { format_ctx(real_tokens) } else { "0".to_string() };
    let right_hint = if state.paused {
        t("hint.paused")
    } else if matches!(state.input_state, InputState::Completing) {
        t("hint.completing")
    } else if state.focus == Focus::Panel && state.panel_visible {
        t("hint.panel_focus")
    } else if state.focus == Focus::CommandHint {
        t("hint.cmd_focus")
    } else {
        t("hint.input_default")
    };
    let right_style = if state.paused {
        state.theme.semantic_style(SemanticIntent::Warning, Strength::Default)
    } else {
        muted_dim
    };

    let right_spans = vec![
        Span::styled(format!("{} tok  ", tok_str), muted_dim),
        Span::styled(right_hint, right_style),
    ];

    // 走 tui::util::display_width 统一治理
    let available = area.width.saturating_sub(2) as usize;
    let left_len: usize = left.iter().map(|s| display_width(s.content.as_ref())).sum();
    let right_len: usize = right_spans.iter().map(|s| display_width(s.content.as_ref())).sum();
    let gap = available.saturating_sub(left_len + right_len);

    let mut spans = left;
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }
    spans.extend(right_spans);

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

    let _cursor_color = state.input_bar_color(); // 保留调用以备后续光标颜色定制
    let mut input_lines: Vec<Line> = Vec::new();

    // ── 顶行：状态指示（spinner + phase + elapsed）──
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
    let phase = if state.processing_phase.is_empty() { String::new() }
        else { format!(" {}", state.processing_phase) };
    // Mode 标签不在 InputBar 重复显示（StatusBar 已有唯一模式指示）
    let (status_text, status_color) = match state.input_state {
        InputState::Thinking => (format!("{} {}{}{}", spinner(), t("event.thinking"), phase, elapsed), state.theme.accent),
        InputState::Executing => (format!("{} {}{}{}", spinner(), t("event.working"), phase, elapsed), state.theme.gold),
        InputState::Outputting => (format!("{} {}{}", spinner(), t("event.outputting"), elapsed), state.theme.success),
        InputState::Paused => (format!("⏸ {}", t("hint.paused")), state.theme.semantic_fg(SemanticIntent::Warning)),
        _ if state.engine_handle.is_some() && state.inline_suggestion.is_some() => {
            (format!("● {} · Tab ↵", t("event.ready")), state.theme.success)
        }
        _ if state.engine_handle.is_some() => (format!("● {}", t("event.ready")), state.theme.success),
        _ => (format!("● {}", t("event.ready")), state.theme.muted),
    };
    input_lines.push(Line::from(vec![
        Span::styled(status_text, Style::default().fg(status_color)),
    ]));

    // ── 中间：输入文本区（自适应高度 + soft-wrap）──
    // V40: 支持 soft-wrap——超出框宽的行自动视觉折行
    let text_area_h = inner.height.saturating_sub(2).max(1) as usize;
    let wrap_width = inner.width.saturating_sub(0) as usize; // 可用渲染宽度

    // Soft-wrap：将每个逻辑行按 wrap_width 拆分为多个视觉行
    // WrappedLine 用指针差值直接计算在 state.input 中的字节范围（零误差）
    struct WrappedLine<'a> {
        text: &'a str,
        logical_line: usize,
        byte_start: usize,
        byte_end: usize,
    }
    let input_ptr = state.input.as_ptr() as usize;
    let logical_lines: Vec<&str> = state.input.lines().collect();
    let mut wrapped: Vec<WrappedLine> = Vec::new();
    for (li, line) in logical_lines.iter().enumerate() {
        if line.is_empty() {
            let byte_off = line.as_ptr() as usize - input_ptr;
            wrapped.push(WrappedLine { text: "", logical_line: li, byte_start: byte_off, byte_end: byte_off });
        } else {
            let chars: Vec<char> = line.chars().collect();
            let widths: Vec<usize> = chars.iter().map(|c| crate::tui::util::char_width(*c)).collect();
            let mut pos = 0;
            while pos < chars.len() {
                let mut w = 0usize;
                let mut end = pos;
                while end < chars.len() {
                    let cw = widths[end];
                    if cw == 0 { end += 1; continue; }
                    if w + cw > wrap_width && end > pos { break; }
                    w += cw;
                    end += 1;
                }
                let start_byte: usize = chars[..pos].iter().map(|c| c.len_utf8()).sum();
                let end_byte: usize = chars[..end].iter().map(|c| c.len_utf8()).sum();
                let seg = &line[start_byte..end_byte];
                let byte_off = seg.as_ptr() as usize - input_ptr;
                wrapped.push(WrappedLine {
                    text: seg,
                    logical_line: li,
                    byte_start: byte_off,
                    byte_end: byte_off + seg.len(),
                });
                pos = end;
            }
        }
    }

    // 滚动：用字节范围定位光标所在的 visual line
    let cursor_wrapped_idx = {
        let mut idx = wrapped.len().saturating_sub(1);
        for (wi, wl) in wrapped.iter().enumerate() {
            if state.cursor_pos >= wl.byte_start && state.cursor_pos <= wl.byte_end {
                idx = wi;
                break;
            }
        }
        idx
    };
    let start = if wrapped.len() <= text_area_h {
        0
    } else if cursor_wrapped_idx >= text_area_h {
        cursor_wrapped_idx + 1 - text_area_h
    } else {
        0
    };
    let end = (start + text_area_h).min(wrapped.len());

    let show_placeholder = state.input.is_empty() && matches!(state.input_state, InputState::Ready);
    let cursor_visible_line = cursor_wrapped_idx.saturating_sub(start);

    if show_placeholder {
        // V41: AwaitingApproval 时 placeholder 变为策略选择提示
        let placeholder_text = if matches!(
            state.plan_phase,
            Some(crate::tui::state::PlanPhase::AwaitingApproval { .. })
        ) {
            t("status.plan_strategy")
        } else {
            "Ask anything..."
        };
        let placeholder_color = if matches!(
            state.plan_phase,
            Some(crate::tui::state::PlanPhase::AwaitingApproval { .. })
        ) {
            state.theme.accent
        } else {
            state.theme.muted
        };
        input_lines.push(Line::from(vec![
            Span::styled(
                placeholder_text,
                Style::default().fg(placeholder_color).add_modifier(Modifier::ITALIC),
            ),
        ]));
        for _ in 1..text_area_h {
            input_lines.push(Line::raw(""));
        }
    } else {
        for vi in start..end {
            let wl = &wrapped[vi];
            let mut spans: Vec<Span> = vec![
                Span::styled(wl.text.to_string(), Style::default().fg(state.theme.text)),
            ];
            // 内联建议：仅在最后一个视觉行 + 光标在末尾时
            let is_last_wrapped = vi == wrapped.len().saturating_sub(1);
            if is_last_wrapped && state.cursor_pos == state.input.len() {
                if let Some(sugg) = &state.inline_suggestion {
                    let full_input = state.input.trim();
                    if let Some(r) = sugg.strip_prefix(full_input).filter(|r| !r.is_empty()) {
                        spans.push(Span::styled(
                            r,
                            Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM),
                        ));
                    }
                }
            }
            input_lines.push(Line::from(spans));
        }
        // 填充剩余空行
        for _ in (end - start)..text_area_h {
            input_lines.push(Line::raw(""));
        }
    }
    // ── 底行：左侧模式标识 + 右侧操作提示 ──
    // 注：token 统计已迁移到健康仪表盘（extras.rs render_dashboard_health）

    let is_busy = matches!(state.input_state,
        InputState::Thinking | InputState::Executing | InputState::Outputting);
    let right_hint_text = if is_busy { t("hint.esc_cancel") } else { "⏎ Enter" };
    let right_style = if is_busy {
        state.theme.text_style(TextRole::Caption)
    } else {
        Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)
    };

    // 底行：右侧操作提示（mode 标签仅在 StatusBar 显示，不重复）
    let right_w = display_width(right_hint_text);
    let fill = inner.width.saturating_sub(right_w as u16).max(1);

    let bottom_spans = vec![
        Span::raw(" ".repeat(fill as usize)),
        Span::styled(right_hint_text, right_style),
    ];
    input_lines.push(Line::from(bottom_spans));

    // 清空 inner 区域防残影（text_area_h 变化时旧行内容残留）
    f.render_widget(Clear, inner);

    f.render_widget(Paragraph::new(input_lines), inner);

    // 光标定位：考虑 soft-wrap——visual line 可能不是逻辑行起点
    {
        let cursor_visual_line = cursor_visible_line;
        // 计算光标所在 WrappedLine 在其逻辑行内的起始 display width
        let visual_col_offset = if cursor_wrapped_idx < wrapped.len() {
            let wl = &wrapped[cursor_wrapped_idx];
            // 指针差值得到段在逻辑行内的字节偏移
            let line_ptr = logical_lines[wl.logical_line].as_ptr() as usize;
            let seg_offset_bytes = wl.byte_start.saturating_sub(line_ptr - input_ptr);
            // 逐字符累加 display width，不超过 seg 的字节边界
            let mut bw = 0usize;
            let mut dw = 0usize;
            for c in logical_lines[wl.logical_line].chars() {
                let cw = crate::tui::util::char_width(c);
                let new_bw = bw + c.len_utf8();
                if new_bw > seg_offset_bytes { break; }
                bw = new_bw;
                dw += cw;
            }
            dw
        } else { 0 };
        let col_in_visual = state.cursor_col.saturating_sub(visual_col_offset);
        let cursor_x = inner.x + col_in_visual as u16;
        let cursor_y = inner.y + 1 + cursor_visual_line as u16;
        f.set_cursor_position((cursor_x, cursor_y));
    }
}

/// 格式化上下文窗口大小为人类可读：1_000_000 → "1M", 500_000 → "500K"
pub(crate) fn format_ctx(n: usize) -> String {
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
