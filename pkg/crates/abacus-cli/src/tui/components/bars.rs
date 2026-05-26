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

use crate::tui::i18n::t;
use crate::tui::state::{AppState, Focus, InputState};
use crate::tui::theme::{SemanticIntent, Strength, TextRole};
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

    // [PLAN] 指示（临时态）
    if state.plan_mode {
        left.push(Span::styled(
            "[PLAN] ",
            Style::default()
                .fg(state.theme.semantic_fg(SemanticIntent::Warning))
                .add_modifier(Modifier::BOLD),
        ));
    }

    // ABACUS logo
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

    // · 当前模式（i18n）
    let mode_label = match state.mode {
        crate::tui::state::AbacusMode::Clarify => t("mode.clarify"),
        crate::tui::state::AbacusMode::Meeting => t("mode.meeting"),
        crate::tui::state::AbacusMode::Plan => t("mode.plan"),
        crate::tui::state::AbacusMode::Team => t("mode.team"),
    };
    left.push(Span::styled(" · ", Style::default().fg(state.theme.border).add_modifier(Modifier::DIM)));
    left.push(Span::styled(
        mode_label,
        Style::default().fg(state.theme.accent),
    ));

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
        crate::tui::state::AbacusMode::Plan => t("mode.plan"),
        crate::tui::state::AbacusMode::Team => t("mode.team"),
    };
    let mode_color = state.theme.mode;
    let status_icon = if state.paused { "⏸" } else { "●" };
    let status_color = if state.paused { state.theme.semantic_fg(SemanticIntent::Warning) } else { state.theme.success };
    let mut left = vec![
        Span::styled(format!("{} ", status_icon), Style::default().fg(status_color)),
        Span::styled(mode_label, Style::default().fg(mode_color).add_modifier(Modifier::BOLD)),
    ];

    // ── 中间：processing_phase（活跃时）──
    if !state.processing_phase.is_empty() {
        left.push(Span::styled(
            format!(" · {}",  state.processing_phase),
            muted_dim,
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

    let cursor_color = state.input_bar_color();
    let mut input_lines: Vec<Line> = Vec::new();
    let muted = state.theme.text_style(TextRole::Caption);

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
    let mode_label = match state.mode {
        crate::tui::state::AbacusMode::Clarify => t("mode.clarify"),
        crate::tui::state::AbacusMode::Meeting => t("mode.meeting"),
        crate::tui::state::AbacusMode::Plan => t("mode.plan"),
        crate::tui::state::AbacusMode::Team => t("mode.team"),
    };
    let (status_text, status_color) = match state.input_state {
        InputState::Thinking => (format!("{} {} {} {}{}", spinner(), t("event.thinking"), mode_label, phase, elapsed), state.theme.accent),
        InputState::Executing => (format!("{} {} {} {}{}", spinner(), t("event.working"), mode_label, phase, elapsed), state.theme.gold),
        InputState::Outputting => (format!("{} {} {}{}", spinner(), t("event.outputting"), mode_label, elapsed), state.theme.success),
        InputState::Paused => (format!("⏸ {}", t("hint.paused")), state.theme.semantic_fg(SemanticIntent::Warning)),
        _ if state.engine_handle.is_some() => (format!("● {} · {}", t("event.ready"), mode_label), state.theme.success),
        _ => (format!("● {} · {}", t("event.ready"), mode_label), state.theme.muted),
    };
    input_lines.push(Line::from(vec![
        Span::styled(status_text, Style::default().fg(status_color)),
    ]));

    // ── 中间：输入文本区（始终 2 行）──
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

    let cursor_visible_line = if state.cursor_line >= start && state.cursor_line < start + visible_lines.len() {
        state.cursor_line - start
    } else {
        visible_lines.len().saturating_sub(1)
    };
    // placeholder：输入为空且 Ready 态时显示 muted italic 提示
    let show_placeholder = state.input.is_empty() && matches!(state.input_state, InputState::Ready);
    for (i, line) in visible_lines.iter().enumerate() {
        if show_placeholder && i == 0 {
            // placeholder: 保留一个细条作为输入行视觉锚点
            input_lines.push(Line::from(vec![
                Span::styled(
                    "Ask anything...",
                    Style::default().fg(state.theme.muted).add_modifier(Modifier::ITALIC),
                ),
            ]));
        } else {
            // 光标行和普通行都直接渲染原始文本
            // 终端光标由 f.set_cursor_position() 定位，无需 ▎ 字符占位
            // （▎ 会把光标后的文字右移 1 列，产生视觉偏移）
            input_lines.push(Line::styled(line.to_string(), Style::default().fg(state.theme.text)));
        }
        let _ = cursor_color; // 光标颜色由终端控制，此变量不再用于渲染
    }

    // ── 底行：左侧 thinking_depth · 百分比 已用/上限，右侧 ⏎ Enter / Esc ──
    // 性能修复：直接使用 session_tokens（引擎返回的真实值），不再每帧遍历全部消息估算
    let real_tokens = state.session_tokens.latest_prompt_tokens as usize;
    let (pct, used_str, max_str) = if state.context_window > 0 && real_tokens > 0 {
        let pct = (real_tokens * 100 / state.context_window).min(99);
        (pct, format_ctx(real_tokens), format_ctx(state.context_window))
    } else if state.context_window > 0 {
        // 引擎未返回 token 数时显示 0%（不遍历消息估算）
        (0, "0".to_string(), format_ctx(state.context_window))
    } else {
        (0, "?".to_string(), "?".to_string())
    };
    let pct_color = if pct >= 80 { state.theme.error }
        else if pct >= 50 { state.theme.gold }
        else { state.theme.success };

    let is_busy = matches!(state.input_state,
        InputState::Thinking | InputState::Executing | InputState::Outputting);
    let right_hint_text = if is_busy { t("hint.esc_cancel") } else { "⏎ Enter" };
    let right_style = if is_busy {
        state.theme.text_style(TextRole::Caption)
    } else {
        Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)
    };

    // 左侧：model · thinking_depth · 百分比 已用/上限
    let model_short = state.model_name.split('-').next_back().unwrap_or(&state.model_name);
    let ctx_left = vec![
        Span::styled(format!("{} · {} · ", model_short, state.thinking_depth), muted),
        Span::styled(format!("{}%", pct), Style::default().fg(pct_color).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {}/{}", used_str, max_str), muted),
    ];
    let ctx_left_width: usize = ctx_left.iter().map(|s| display_width(s.content.as_ref())).sum();
    let fill = inner.width.saturating_sub(
        (ctx_left_width + display_width(right_hint_text)) as u16,
    ).max(1);

    let mut bottom_spans = ctx_left;
    bottom_spans.push(Span::raw(" ".repeat(fill as usize)));
    bottom_spans.push(Span::styled(right_hint_text, right_style));
    input_lines.push(Line::from(bottom_spans));

    f.render_widget(Paragraph::new(input_lines), inner);

    // 始终显示光标
    {
        let cursor_x = inner.x + state.cursor_col as u16;
        let cursor_y = inner.y + 1 + cursor_visible_line as u16;
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
