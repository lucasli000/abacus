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
            " [PLAN]".to_string(),
            Style::default().fg(state.theme.gold).add_modifier(Modifier::BOLD),
        ));
    } else if state.processing_phase.starts_with("team") {
        left.push(Span::styled(
            " [TEAM]".to_string(),
            Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD),
        ));
    }

    // ── 中间: tokens + context% + cost (V42-B+ UX 改进)
    //   参考 OpenCode 截图: "39,413  20% ($0.29)"
    //   紧凑单行汇总，不与 title 抢空间（窄终端时自动截断）
    let metrics_mid: Vec<Span> = build_metrics_spans(state);

    // ── 右侧: model_name ──
    let right = Span::styled(
        format!("{} ", model_name),
        Style::default().fg(state.theme.accent),
    );

    // 计算左右间距，确保右侧不溢出
    let left_len: usize = left.iter().map(|s| display_width(s.content.as_ref())).sum();
    let mid_len: usize = metrics_mid.iter().map(|s| display_width(s.content.as_ref())).sum();
    let right_len = display_width(right.content.as_ref());
    let gap = width.saturating_sub(left_len + mid_len + right_len);

    let mut spans = left;
    // 只有 metrics 非空且有空间时才插入
    if !metrics_mid.is_empty() && gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(metrics_mid);
    } else if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }
    spans.push(right);

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// V42-B+: 构建 TopBar 中间的紧凑指标 spans（tokens + context% + cost）
///
/// 视觉格式（参考 OpenCode 截图）：
///   "39,413  20% ($0.29)"    (完整)
///   "39,413  20%"             (cost=0 时省略 cost)
///   "39,413"                  (context_window=0 时省略 percentage)
///
/// 颜色：
///   - tokens: text（主色）
///   - context%: muted（次要）
///   - cost: gold（强调但不大声）
fn build_metrics_spans(state: &AppState) -> Vec<Span<'_>> {
    let mut spans: Vec<Span> = Vec::new();
    let tokens = state.session_tokens.total_tokens;
    if tokens == 0 {
        return spans;
    }
    let sep_style = Style::default().fg(state.theme.muted);

    // tokens with thousands separator
    let tokens_str = format_tokens(tokens);
    spans.push(Span::styled(tokens_str, Style::default().fg(state.theme.text)));

    // context% if context_window > 0
    let ctx_window = state.context_window;
    if ctx_window > 0 {
        let ctx_used = state.ctx_live_tokens.max(state.session_tokens.latest_prompt_tokens);
        let pct = ((ctx_used as f64 / ctx_window as f64) * 100.0).round() as u32;
        // 超过 90% 显示警告色
        let pct_color = if pct >= 90 {
            state.theme.error
        } else if pct >= 70 {
            state.theme.gold
        } else {
            state.theme.muted
        };
        spans.push(Span::styled("  ", sep_style));
        spans.push(Span::styled(format!("{}%", pct), Style::default().fg(pct_color)));
    }

    // cost if > 0
    let cost = state.session_tokens.cost_cny;
    if cost > 0.0 {
        spans.push(Span::styled("  ", sep_style));
        let cost_str = format_cost(cost);
        spans.push(Span::styled(
            format!("({})", cost_str),
            Style::default().fg(state.theme.gold),
        ));
    }

    spans
}

/// 格式化 token 数为带千分位的紧凑形式：1234 → "1,234", 12345 → "12K"
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        if n % 1_000_000 == 0 {
            format!("{}M", n / 1_000_000)
        } else {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        }
    } else if n >= 10_000 {
        // 4 位以上用 K 简化（节省 TopBar 空间）
        format!("{}K", n / 1_000)
    } else {
        // 4 位以下完整显示
        n.to_string()
    }
}

/// 格式化费用：< 0.01 显示 "<0.01"；>= 0.01 显示 "X.XX"（保留 2 位）
fn format_cost(c: f64) -> String {
    if c < 0.01 {
        "<0.01".to_string()
    } else {
        format!("¥{:.2}", c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tokens_small() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(1), "1");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1234), "1234");
        assert_eq!(format_tokens(9999), "9999");
    }

    #[test]
    fn format_tokens_large_compact() {
        assert_eq!(format_tokens(10_000), "10K");
        assert_eq!(format_tokens(39_413), "39K");
        assert_eq!(format_tokens(100_000), "100K");
        assert_eq!(format_tokens(999_999), "999K");
    }

    #[test]
    fn format_tokens_million() {
        assert_eq!(format_tokens(1_000_000), "1M");
        assert_eq!(format_tokens(2_500_000), "2.5M");
        assert_eq!(format_tokens(10_000_000), "10M");
    }

    #[test]
    fn format_cost_small() {
        assert_eq!(format_cost(0.0), "<0.01");
        assert_eq!(format_cost(0.005), "<0.01");
        assert_eq!(format_cost(0.29), "¥0.29");
        assert_eq!(format_cost(1.5), "¥1.50");
        assert_eq!(format_cost(100.0), "¥100.00");
    }

    #[test]
    fn build_metrics_spans_empty_when_no_tokens() {
        use crate::tui::state::{AbacusMode, AppState};
        let state = AppState::new(AbacusMode::Clarify);
        let spans = build_metrics_spans(&state);
        assert!(spans.is_empty(), "no tokens → empty metrics");
    }

    #[test]
    fn build_metrics_spans_tokens_only() {
        use crate::tui::state::{AbacusMode, AppState};
        let mut state = AppState::new(AbacusMode::Clarify);
        state.session_tokens.total_tokens = 1234;
        state.context_window = 0; // 关闭 context%
        state.session_tokens.latest_prompt_tokens = 0;
        let spans = build_metrics_spans(&state);
        assert_eq!(spans.len(), 1, "tokens-only (no context%, no cost): 1 span");
    }

    #[test]
    fn build_metrics_spans_with_context_pct() {
        use crate::tui::state::{AbacusMode, AppState};
        let mut state = AppState::new(AbacusMode::Clarify);
        state.session_tokens.total_tokens = 5000;
        state.context_window = 10000;
        state.session_tokens.latest_prompt_tokens = 2000;
        let spans = build_metrics_spans(&state);
        // 5000 + "  " + "20%"
        assert!(spans.len() >= 3, "with context% should have ≥3 spans");
        let combined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(combined.contains("5000"), "should contain token count");
        assert!(combined.contains("20%"), "should contain context%");
    }

    #[test]
    fn build_metrics_spans_high_pct_uses_warning_color() {
        use crate::tui::state::{AbacusMode, AppState};
        let mut state = AppState::new(AbacusMode::Clarify);
        state.session_tokens.total_tokens = 1000;
        state.context_window = 1000;
        state.session_tokens.latest_prompt_tokens = 950;
        let spans = build_metrics_spans(&state);
        let combined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(combined.contains("95%"), "should show 95%");
    }

    #[test]
    fn build_metrics_spans_full() {
        use crate::tui::state::{AbacusMode, AppState};
        let mut state = AppState::new(AbacusMode::Clarify);
        state.session_tokens.total_tokens = 39_413;
        state.session_tokens.latest_prompt_tokens = 8000;
        state.context_window = 40_000;
        state.session_tokens.cost_cny = 0.29;
        let spans = build_metrics_spans(&state);
        let combined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(combined.contains("39K"), "should contain token count");
        assert!(combined.contains("20%"), "should contain context%");
        assert!(combined.contains("¥0.29"), "should contain cost");
    }
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

    // V42-B+: 焦点 bg 使用 surface（比 bg 亮一级）— 让用户视觉上感知"我现在在这"
    let focused_bg = if state.focus == Focus::Input {
        state.theme.surface
    } else {
        state.theme.bg
    };
    let bar_color = state.input_bar_color();

    // Phase 2: textarea 是 SSoT，input() 后已同步到 state.input，无需额外 sync

    // ── 状态指示行文本 ──
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
    let (status_text, _status_color) = match state.input_state {
        InputState::Thinking => (format!("{} {}{}{}", spinner(), t("event.thinking"), phase, elapsed), state.theme.accent),
        InputState::Executing => (format!("{} {}{}{}", spinner(), t("event.working"), phase, elapsed), state.theme.gold),
        InputState::Outputting => (format!("{} {}{}", spinner(), t("event.outputting"), elapsed), state.theme.success),
        InputState::Paused => (format!("⏸ {}", t("hint.paused")), state.theme.semantic_fg(SemanticIntent::Warning)),
        _ if state.engine_handle.is_some() && state.completion.suggestion.is_some() => {
            (format!("● {} · Tab ↵", t("event.ready")), state.theme.success)
        }
        _ if state.engine_handle.is_some() => (format!("● {}", t("event.ready")), state.theme.success),
        _ => (format!("● {}", t("event.ready")), state.theme.muted),
    };

    // ── 构建 textarea 的 Block（边框 + 状态指示 title）──
    let input_block = Block::default()
        .border_type(BorderType::Rounded)
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" {} ", status_text),
            Style::default().fg(_status_color),
        ))
        .border_style(Style::default().fg(bar_color).bg(focused_bg))
        .padding(Padding::horizontal(1))
        .style(Style::default().bg(focused_bg));

    // ── 配置 textarea 样式 ──
    {
        let mut ta = state.textarea.borrow_mut();
        ta.set_block(input_block);
        ta.set_style(Style::default().fg(state.theme.text).bg(focused_bg));
        // 隐藏 tui-textarea 的 REVERSED cursor（我们用真实终端 cursor）
        ta.set_cursor_style(Style::default().fg(focused_bg).bg(focused_bg));
        ta.set_cursor_line_style(Style::default());
        // placeholder
        let placeholder_text = if matches!(
            state.plan_phase,
            Some(crate::tui::state::PlanPhase::AwaitingApproval { .. })
        ) {
            t("status.plan_strategy").to_string()
        } else {
            "Ask anything...".to_string()
        };
        ta.set_placeholder_text(&placeholder_text);
        let placeholder_color = if matches!(
            state.plan_phase,
            Some(crate::tui::state::PlanPhase::AwaitingApproval { .. })
        ) {
            state.theme.accent
        } else {
            state.theme.muted
        };
        ta.set_placeholder_style(
            Style::default().fg(placeholder_color).add_modifier(Modifier::ITALIC),
        );
    }

    // ── 渲染 textarea widget ──
    {
        let ta = state.textarea.borrow();
        f.render_widget(&*ta, area);
    }

    // ── 焦点 thick 上边框叠加 ──
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

    // ── 底行：dynamic keyboard hint bar ──
    let inner = Block::default()
        .border_type(BorderType::Rounded)
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .inner(area);

    let is_busy = matches!(state.input_state,
        InputState::Thinking | InputState::Executing | InputState::Outputting);
    let right_hint_text = if is_busy { t("hint.esc_cancel") } else { "⏎ Enter" };
    let right_style = if is_busy {
        state.theme.text_style(TextRole::Caption)
    } else {
        Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)
    };

    let bottom_hints: Vec<(&str, &str)> = if is_busy {
        vec![]
    } else {
        match state.input_state {
            InputState::Ready => vec![
                ("/", t("hint.slash_command")),
                ("Tab", t("hint.completing")),
                ("Ctrl+B", t("hint.focus_panel")),
            ],
            InputState::Completing => vec![
                ("Tab", t("hint.completing")),
                ("Esc", t("hint.cancel")),
            ],
            _ => vec![],
        }
    };

    let mut bottom_spans: Vec<Span> = Vec::new();
    let hint_key_style = Style::default().fg(state.theme.accent);
    let hint_sep_style = Style::default().fg(state.theme.muted);
    let hint_desc_style = state.theme.text_style(TextRole::Caption);
    let mut first_hint = true;
    for (key, desc) in bottom_hints {
        if !first_hint {
            bottom_spans.push(Span::styled("  ·  ", hint_sep_style));
        }
        first_hint = false;
        bottom_spans.push(Span::styled(key, hint_key_style));
        bottom_spans.push(Span::styled(format!(" {}", desc), hint_desc_style));
    }

    let right_w = display_width(right_hint_text);
    let left_w: usize = bottom_spans.iter().map(|s| display_width(s.content.as_ref())).sum();
    let fill = inner.width.saturating_sub((left_w + right_w + 1) as u16).max(1);

    if !bottom_spans.is_empty() {
        bottom_spans.push(Span::raw(" ".repeat(fill as usize)));
    } else {
        bottom_spans.push(Span::raw(" ".repeat(inner.width.saturating_sub(right_w as u16).max(1) as usize)));
    }
    bottom_spans.push(Span::styled(right_hint_text, right_style));

    // hint bar 渲染在 block 底部（inner 最后一行）
    if inner.height >= 1 {
        let hint_y = inner.y + inner.height - 1;
        let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
        f.render_widget(Paragraph::new(Line::from(bottom_spans)).style(Style::default().bg(focused_bg)), hint_area);
    }

    // ── 真实终端光标定位 ──
    // tui-textarea 的 cursor 位置是相对于 widget area 的 (row, col)
    // 需要转换为绝对终端坐标
    {
        let ta = state.textarea.borrow();
        let (cursor_row, cursor_col) = ta.cursor();
        // inner 区域的起始位置 + cursor 在 inner 中的偏移
        // 但 tui-textarea 渲染在 area（含 border），所以 cursor 是相对于 area 的
        // Block border 占 1 行（上）+ 1 列（左右），padding 1 列
        // 实际 cursor 绝对位置 = area.y + 1(border top) + cursor_row, area.x + 1(border left) + 1(padding) + cursor_col
        let abs_x = area.x.saturating_add(2).saturating_add(cursor_col as u16);
        let abs_y = area.y.saturating_add(1).saturating_add(cursor_row as u16);
        // 确保 cursor 在可见区域内
        if abs_y < area.y + area.height && abs_x < area.x + area.width {
            f.set_cursor_position((abs_x, abs_y));
        }
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
