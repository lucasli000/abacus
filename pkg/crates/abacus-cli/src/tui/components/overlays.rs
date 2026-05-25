//! Overlay 组件 — 弹窗/对话框/通知等浮层渲染
//!
//! 从 mod.rs 提取的公共 overlay 函数：
//! - render_confirm_dialog — 权限确认弹窗
//! - render_toasts — 左上角浮动通知
//! - render_min_terminal_warning — 极小终端保护
//! - render_completion_popup — 三模式补全弹窗
//! - render_overlays — 三模式公共 overlay 入口
//! - render_picker_popup — 命令参数 picker
//! - render_settings_modal — 设置模态框
//!
//! 引用关系：被 modes/chat|team|meeting::render 调用
//! 生命周期：每帧按 state 决定可见性

use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

use crate::tui::i18n::t;
use crate::tui::state::AppState;
use crate::tui::theme::{SemanticIntent, Strength, TextRole};

// ════════════════════════════════════════════════════════════════
// Toast — 左上角浮动通知
// ════════════════════════════════════════════════════════════════

/// 权限确认弹窗 — 在 InputBar 正上方弹出
///
/// 视觉设计：
///   ╭─ ⚠ 文件写入确认 ──────────────────────────╮
///   │                                            │
///   │  操作: edit → src/main.rs                  │
///   │  详情: 修改第 42-58 行                     │
///   │                                            │
///   │        [Y 确认]        [N 拒绝]            │
///   ╰────────────────────────────────────────────╯
///
/// 引用关系：被 modes/chat.rs 等在 input 区域上方渲染
/// 生命周期：confirm_dialog = Some 时显示，用户响应后清除
/// 权限确认弹窗 — 自适应内容高度，支持扩展选项
///
/// 视觉设计（Medium 风险 - 文件写入）：
///   ╭─ ⚠ 文件写入确认 ──────────────────────────╮
///   │  操作: edit → src/main.rs                  │
///   │  + fn handle_error(err)                    │  ← diff 预览
///   │  - fn old_handler()                        │
///   │                                            │
///   │  [Y 确认]  [N 拒绝]  [D 查看Diff]  [A 总是允许] │
///   ╰────────────────────────────────────────────╯
///
/// 视觉设计（High 风险 - 删除）：
///   ╭─ 🔴 ⚠ 文件删除确认 ───────────────────────╮
///   │  操作: rm → config/secrets.yaml            │
///   │  ⚠ 此操作不可撤销                          │
///   │                                            │
///   │  [Y 确认]  [N 拒绝]                        │
///   ╰────────────────────────────────────────────╯
pub fn render_confirm_dialog(f: &mut ratatui::Frame, state: &AppState, input_area: Rect) {
    use crate::tui::state::{ConfirmRisk, ConfirmType};
    use ratatui::widgets::Clear;

    let dialog = match &state.confirm_dialog {
        Some(d) => d,
        None => return,
    };

    // V40: 统一弹窗布局规范（与 picker_popup 一致）
    //   宽度：输入框宽度 × 6/8
    //   高度：内容自适应，上限 = 消息框高度 1/3
    //   位置：靠左对齐 input_area.x，向上弹出
    let max_visible = if dialog.details_expanded { 8 } else { 3 };
    let visible_count = dialog.details.len().min(max_visible);
    let has_more = dialog.details.len() > max_visible;
    let detail_lines = visible_count + if has_more { 1 } else { 0 };
    let content_h = (6 + detail_lines) as u16;
    let frame_size = f.area();
    // 宽度：输入框 6/8
    let popup_w: u16 = (input_area.width * 6 / 8).max(40).min(frame_size.width);
    // 高度：上限 = 消息区（≈ input_area.y）的 1/3
    let msg_area_h = input_area.y;
    let max_h = (msg_area_h / 3).max(8);
    let popup_h = content_h.min(max_h).min(frame_size.height);
    // 位置：靠左，向上弹出
    let popup_y = input_area.y.saturating_sub(popup_h);
    let popup_x = input_area.x;
    let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);

    // K3b 穿透根治：Clear 后用 elevated 填底，杠杀背景残影
    f.render_widget(Clear, popup_area);
    f.render_widget(
        Block::default().style(Style::default().bg(state.theme.elevated)),
        popup_area,
    );

    // 边框颜色 + 图标
    let (border_color, risk_icon) = match dialog.risk {
        ConfirmRisk::Low => (state.theme.accent, "ℹ"),
        ConfirmRisk::Medium => (state.theme.gold, "⚠"),
        ConfirmRisk::High => (state.theme.error, "🔴"),
    };

    let block = Block::default()
        .title(format!(" {} {} ", risk_icon, dialog.title))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(state.theme.elevated));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // ── 内容渲染 ──
    let mut lines: Vec<Line> = Vec::new();

    // 操作行
    let action_icon = match &dialog.confirm_type {
        ConfirmType::FileWrite => "📝",
        ConfirmType::FileDelete => "🗑",
        ConfirmType::ShellExec => "💻",
        ConfirmType::NetworkRequest => "🌐",
        ConfirmType::BatchOperation { .. } => "📦",
        ConfirmType::PrivilegeEscalation => "🔑",
        ConfirmType::Custom => "•",
    };
    lines.push(Line::from(vec![
        Span::styled(format!(" {} ", action_icon), Style::default().fg(border_color)),
        Span::styled(&dialog.action, state.theme.text_style(TextRole::BodyEmphasis)),
    ]));

    // 详情行（diff 预览/文件列表/命令等）
    // B7+B9：受 details_expanded 控制行数；超出显示 "+N more (D 展开)"
    for detail in dialog.details.iter().take(visible_count) {
        // Diff 着色：+ 绿, - 红, 其他 muted
        let style = if detail.starts_with('+') {
            state.theme.semantic_style(SemanticIntent::Success, Strength::Default)
        } else if detail.starts_with('-') {
            state.theme.semantic_style(SemanticIntent::Danger, Strength::Default)
        } else if detail.starts_with('⚠') || detail.starts_with("此操作") {
            state.theme.semantic_style(SemanticIntent::Danger, Strength::Strong)
        } else {
            Style::default().fg(state.theme.muted)
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(detail, style),
        ]));
    }
    if has_more {
        let remaining = dialog.details.len() - visible_count;
        let hint = if dialog.details_expanded {
            format!("  … +{} 行更多（已超 8 行上限）", remaining)
        } else {
            format!("  … +{} 行隐藏（按 D 展开）", remaining)
        };
        lines.push(Line::from(Span::styled(
            hint,
            state.theme.text_style(TextRole::Caption),
        )));
    }

    lines.push(Line::raw(""));

    // 倒计时 + 超时行为提示
    let remaining = dialog.remaining_secs();
    let timeout_hint = if dialog.risk == ConfirmRisk::High {
        format!("{}s 后自动拒绝", remaining)
    } else {
        format!("{}s 后自动允许", remaining)
    };
    let countdown_color = if remaining <= 3 {
        state.theme.error
    } else if remaining <= 5 {
        state.theme.gold
    } else {
        state.theme.muted
    };
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(format!("⏱ {}", timeout_hint), Style::default().fg(countdown_color).add_modifier(Modifier::BOLD)),
    ]));

    // V25：动态从 dialog.options 渲染按钮，selected 项加箭头标识 + REVERSED
    //   中文 IME 用户用 ↑↓/Tab 切换 selected，看箭头知道 Enter 会触发哪个
    let mut btn_spans: Vec<Span> = vec![Span::raw(" ")];
    for (idx, opt) in dialog.options.iter().enumerate() {
        if idx > 0 { btn_spans.push(Span::raw(" ")); }
        let is_selected = idx == dialog.selected;
        let (fg, bg) = match opt.key {
            'Y' => (crate::tui::effects::auto_contrast_fg(state.theme.success), state.theme.success),
            'A' => (crate::tui::effects::auto_contrast_fg(state.theme.accent), state.theme.accent),
            'N' => (crate::tui::effects::auto_contrast_fg(state.theme.error), state.theme.error),
            _   => (crate::tui::effects::auto_contrast_fg(state.theme.surface), state.theme.surface),
        };
        let label = if is_selected {
            format!("▶ {} {} ◀", opt.key, opt.label)
        } else {
            format!(" {} {} ", opt.key, opt.label)
        };
        let mut style = Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD);
        if is_selected { style = style.add_modifier(Modifier::UNDERLINED); }
        btn_spans.push(Span::styled(label, style));
    }
    lines.push(Line::from(btn_spans));

    f.render_widget(Paragraph::new(lines), inner);
}

pub fn render_toasts(f: &mut ratatui::Frame, state: &AppState) {
    if state.toasts.is_empty() {
        return;
    }

    let screen = f.area();
    let now = Instant::now();
    // 消息框顶部正中间，向下堆叠
    // toast 宽度自适应内容（min 20, max screen_width * 60%）
    let max_toast_w = (screen.width as usize * 60 / 100).max(20).min(80) as u16;
    let toast_h: u16 = 3;
    let toast_gap: u16 = 0;
    let mut y = 2u16; // 顶部留 2 行（top bar 下方）

    for toast in &state.toasts {
        if y + toast_h > screen.height.saturating_sub(4) {
            break;
        }
        let remaining = toast.expire_at.duration_since(now);
        let is_fading = remaining < std::time::Duration::from_millis(800);
        let dim_modifier = if is_fading { Modifier::DIM } else { Modifier::empty() };

        // 自适应宽度：内容长度 + 边距(6) + icon(2)，clamp 到 [20, max_toast_w]
        let content_chars = toast.message.chars().count();
        let tw = ((content_chars + 8) as u16).clamp(20, max_toast_w);
        // 水平居中
        let x = screen.width.saturating_sub(tw) / 2;

        let area = Rect::new(x, y, tw, toast_h);
        let border_color = if is_fading { state.theme.muted } else { state.theme.accent };
        let card = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color).add_modifier(dim_modifier))
            .bg(state.theme.surface);
        card.render(area, f.buffer_mut());

        let inner = Rect::new(x + 2, y + 1, tw.saturating_sub(4), 1);
        // 截断显示
        let max_msg_chars = (tw as usize).saturating_sub(6);
        let display_msg: String = if content_chars > max_msg_chars {
            let mut s: String = toast.message.chars().take(max_msg_chars.saturating_sub(1)).collect();
            s.push('…');
            s
        } else {
            toast.message.clone()
        };
        let text_color = if is_fading { state.theme.muted } else { state.theme.text };
        let line = Line::from(vec![
            Span::styled(" ", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD | dim_modifier)),
            Span::styled(display_msg, Style::default().fg(text_color).add_modifier(dim_modifier)),
        ]);
        f.render_widget(Paragraph::new(line), inner);
        y += toast_h + toast_gap;
    }
}

/// 面板底部：会话统计摘要
/// 极小终端保护：宽度<20 或高度<5 时只显示提示，避免 layout split 产生 0 高 widget
/// 引用关系：render_overlays 在每个 mode 渲染最前调用
/// 生命周期：每帧检查；返回 true 表示已渲染提示，调用方应 return
pub fn render_min_terminal_warning(f: &mut ratatui::Frame) -> bool {
    if f.area().width < 20 || f.area().height < 5 {
        let msg = ratatui::widgets::Paragraph::new(t("overlay.terminal_too_small"))
            .alignment(ratatui::layout::Alignment::Center);
        f.render_widget(msg, f.area());
        return true;
    }
    false
}

/// 三模式共用补全弹窗
///
/// ## 几何契约（V32 重构）
/// - **宽度**：`input_area.width × 65%`（用户视觉规范），最小 16 列防过窄；不超 frame.width
/// - **高度**：随候选数自适应，但**上限 = `messages_area.height × 45%`**
///   （确保弹窗不会向上吃掉过多消息区，用户仍能看到至少 55% 的对话上下文）
/// - **位置**：固定贴 input_area 上方（弹窗底边 = input_area.y - 1）；
///   若上方空间不足则尽量贴顶
/// - **滚动**：候选数 > 可见行数时，按 completion_index 居中展示；上下显示 ↑/↓ 指示器
///
/// ## 引用关系
/// - 调用方：`render_overlays`（chat/team/meeting 三模式入口）
/// - 输入：`state.completion_candidates`（候选列表）+ `state.completion_index`（选中索引）
/// - 生命周期：`input_state == Completing && !candidates.is_empty()` 时每帧重绘
pub fn render_completion_popup(
    f: &mut ratatui::Frame,
    state: &AppState,
    input_area: Rect,
    messages_area: Rect,
) {
    use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

    let candidates = &state.completion_candidates;
    if candidates.is_empty() { return; }

    let frame = f.area();

    // ── 宽度：input_area.width × 65% ────────────────────────────
    // saturating arith 防 u16 溢出；max(16) 防过窄到无法显示候选；min(frame.width) 防越界
    let popup_w: u16 = (input_area.width as u32 * 65 / 100)
        .max(16) as u16;
    let popup_w = popup_w.min(frame.width);

    // ── 高度上限：messages_area.height × 45% ────────────────────
    // 同时不能向上突破 input_area 顶部（即弹窗底边 = input_area.y - 1，可用空间 = input_area.y）
    let max_h_by_messages: u16 = (messages_area.height as u32 * 45 / 100).max(3) as u16;
    let max_h_by_above: u16 = input_area.y.saturating_sub(1).max(3);
    let max_h: u16 = max_h_by_messages.min(max_h_by_above);

    // ── 实际高度：min(候选数 + 上下边框, max_h) ────────────────
    let total_h: u16 = (candidates.len() as u16).saturating_add(2); // +2 for top/bottom border
    let popup_h: u16 = total_h.min(max_h).max(3);

    // ── 位置：贴 input_area 上方，左对齐到 input_area.x ────────
    let popup_y: u16 = input_area.y.saturating_sub(popup_h);
    let popup_x: u16 = input_area.x.min(frame.width.saturating_sub(popup_w));
    let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);

    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(" 补全 (↑↓/Tab 选择 · Enter 确认 · Alt+1-9 直选 · Esc 取消) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(state.theme.primary))
        .style(Style::default().bg(state.theme.elevated));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // ── 滚动：可见行数 = inner.height；选中项居中策略 ──────────
    let visible_rows: usize = inner.height as usize;
    let total: usize = candidates.len();
    let scroll_start: usize = if total <= visible_rows {
        0
    } else {
        // 选中项居中：(half 上 + half 下) 让 completion_index 大致在视图中央
        let half = visible_rows / 2;
        state.completion_index.saturating_sub(half)
            .min(total - visible_rows)
    };

    // V32 · 选中行高亮强化：选中前缀 ❯ + 数字快捷提示（前 9 项 1-9，对应 Alt+1..9）
    // 非选中前缀用编号占位让对齐稳定，色弱用户也能凭数字识别选中项。
    let mut lines: Vec<Line> = Vec::new();
    for (i, candidate) in candidates.iter()
        .skip(scroll_start)
        .take(visible_rows)
        .enumerate()
    {
        let actual_idx = scroll_start + i;
        let is_selected = actual_idx == state.completion_index;
        // 数字快捷提示：actual_idx ∈ [0..9] → 显示 "1·".."9·"；之后用空白
        let num_hint: String = if actual_idx < 9 {
            format!("{}·", actual_idx + 1)
        } else {
            "  ".to_string()
        };
        let arrow = if is_selected { "❯" } else { " " };
        let prefix = format!("{} {} ", arrow, num_hint);
        let style = if is_selected {
            // 强化选中样式：reverse + BOLD 对色弱友好
            state.theme.semantic_style(SemanticIntent::Success, Strength::Strong)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(state.theme.muted)
        };
        // 滚动指示器：仅当列表确实溢出时显示
        let scroll_indicator = if total > visible_rows {
            if actual_idx == scroll_start && scroll_start > 0 { " ↑" }
            else if actual_idx == scroll_start + visible_rows - 1
                && scroll_start + visible_rows < total { " ↓" }
            else { "" }
        } else { "" };
        lines.push(Line::from(Span::styled(
            format!("{}{}{}", prefix, candidate, scroll_indicator),
            style,
        )));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// 三模式公共 overlay 渲染层（MD1+MD2 修复）
/// 顺序：toasts（最底）→ confirm_dialog（拦截输入需可见）→ completion_popup（输入区上方）
/// 引用关系：被 modes/chat|team|meeting::render 在 mode-specific 渲染完后调用
/// 生命周期：每帧渲染；按当前 state 决定哪些可见
/// 三模式 overlay 入口（toasts / confirm_dialog / completion popup / picker popup）
///
/// V32: 增加 `messages_area` 参数让 completion popup 能按消息区高度限制弹窗高度上限
/// （视觉契约：弹窗不超过消息区 45%）。其它 overlay 仅依赖 input_area 不受影响。
pub fn render_overlays(
    f: &mut ratatui::Frame,
    state: &AppState,
    input_area: Rect,
    messages_area: Rect,
) {
    render_toasts(f, state);
    render_confirm_dialog(f, state, input_area);
    if state.input_state == crate::tui::state::InputState::Completing
        && !state.completion_candidates.is_empty()
    {
        render_completion_popup(f, state, input_area, messages_area);
    }
    // V13: 命令参数 picker（/model /theme /thinking）— 在输入框上方弹出
    if state.picker.is_some() {
        render_picker_popup(f, state, input_area);
    }
}

/// 命令参数 picker popup（V13）
///
/// 引用关系：state.picker = Some(...) 时由 render_overlays 调用
/// 生命周期：picker 打开期间每帧绘制；Enter/Esc 关闭后由后续帧不再调用
/// 设计意图：`/model`/`/theme`/`/thinking` 等斜杠命令输入即弹出
///           可视化选择器，箭头选 + Enter 确认
pub fn render_picker_popup(f: &mut ratatui::Frame, state: &AppState, input_area: Rect) {
    use crate::tui::state::PickerKind;
    use crate::tui::util::display_width;
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let Some(p) = state.picker.as_ref() else { return; };
    if p.items.is_empty() { return; }

    let title = match p.kind {
        PickerKind::Model    => t("overlay.model_picker"),
        PickerKind::Theme    => t("overlay.theme_picker"),
        PickerKind::Thinking => t("overlay.thinking_picker"),
    };
    let frame = f.area();

    // 计算尺寸：列宽取最长 label + 2(▶/●) + 12(主题色块预览) + 边框
    let widest: usize = p.labels.iter().map(|s| display_width(s.as_str())).max().unwrap_or(20);
    // 也考虑分组标题宽度
    let widest = if let Some(ref groups) = p.groups {
        let g_widest = groups.iter().map(|(name, _)| display_width(name) + 4).max().unwrap_or(0);
        widest.max(g_widest)
    } else { widest };
    let _extra = if matches!(p.kind, PickerKind::Theme) { 14 } else { 4 };
    // V40: 弹窗布局规范 — 靠左，宽度为输入框 6/8，高度不超过消息框 1/3
    //   宽度：input_area.width * 6 / 8
    //   高度：内容自适应，上限 = 消息区高度 / 3（消息区 ≈ input_area.y）
    //   位置：左对齐 input_area.x，垂直方向向上弹出（输入框正上方）
    let popup_w = (input_area.width * 6 / 8).max(36).min(frame.width);
    let group_overhead = p.groups.as_ref().map(|g| g.len()).unwrap_or(0);
    let slider_overhead = if p.show_thinking_slider { 2 } else { 0 };
    let content_lines = p.items.len() + group_overhead + slider_overhead;
    // 高度上限：消息区（input_area.y 近似消息框高度）的 1/3
    let msg_area_h = input_area.y as usize;
    let max_h = (msg_area_h / 3).max(6);
    let popup_h = ((content_lines + 2) as u16) // +2 border
        .min(max_h as u16)
        .min(frame.height);

    // max_visible: 内容区可见行数（popup_h - 2 border）
    let max_visible = (popup_h.saturating_sub(2)) as usize;

    // 位置：靠左，向上弹出（紧贴输入框上方）
    let popup_y = input_area.y.saturating_sub(popup_h);
    let popup_x = input_area.x;
    let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);

    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        // V40: 与 toast 色彩一致 — accent 边框 + surface 背景，简洁统一
        .border_style(Style::default().fg(state.theme.accent))
        .bg(state.theme.surface);
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // V29.8: 分组模式下不做滚动(假设 model 列表短), 简单全部显示
    //   未来若 model 多到需滚动, 可加 scroll_start 计算
    let mut lines: Vec<Line> = Vec::new();

    // V40: 渲染项闭包 — 紧凑排版，固定 3 字符前缀（marker），无冗余 padding
    // 格式: " ▶● label" 或 "    label"（3 字符对齐前缀 + 1 空格）
    let render_item = |idx: usize, lines: &mut Vec<Line>| {
        let label = &p.labels[idx];
        let id = &p.items[idx];
        let is_sel = idx == p.selected;
        let is_cur = p.current == Some(idx);
        // 固定 3 字符宽前缀：确保所有行文本起始位置对齐
        let marker = if is_sel && is_cur { "▶●" }  // 2 宽（全角类）
            else if is_sel { " ▶" }
            else if is_cur { " ●" }
            else { "  " };
        let row_style = if is_sel {
            Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)
        } else if is_cur {
            Style::default().fg(state.theme.primary)
        } else {
            Style::default().fg(state.theme.text)
        };
        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled(format!(" {} ", marker), row_style));
        // 直接输出 label（不做 pad_to_width，避免多余空格浪费弹窗宽度）
        spans.push(Span::styled(label.clone(), row_style));

        if matches!(p.kind, PickerKind::Theme) {
            let t = crate::tui::theme::from_name(id);
            spans.push(Span::raw(" "));
            spans.push(Span::styled("██", Style::default().fg(t.primary)));
            spans.push(Span::styled("██", Style::default().fg(t.accent)));
            spans.push(Span::styled("██", Style::default().fg(t.success)));
        }
        lines.push(Line::from(spans));
    };

    // V40: 分组渲染 — 紧凑组标题 + 子项
    if let Some(ref groups) = p.groups {
        for (provider, range) in groups {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" ─ {}", provider),
                    Style::default().fg(state.theme.muted),
                ),
            ]));
            for idx in range.clone() {
                render_item(idx, &mut lines);
            }
        }
    } else {
        // 默认(Theme/Thinking): 简单滚动窗口
        let scroll_start = if p.items.len() <= max_visible {
            0
        } else {
            p.selected.saturating_sub(2).min(p.items.len() - max_visible)
        };
        for idx in scroll_start..(scroll_start + max_visible).min(p.items.len()) {
            render_item(idx, &mut lines);
        }
    }

    // V29.8: 底部 thinking 调节器 (Model picker 专属)
    if p.show_thinking_slider {
        lines.push(Line::raw("")); // 空行分隔
        let depths = ["off", "low", "medium", "high"];
        let cur_depth = state.thinking_depth.as_str();
        let mut slider_spans: Vec<Span> = Vec::new();
        slider_spans.push(Span::styled(
            " 思考深度 ",
            Style::default().fg(state.theme.muted),
        ));
        slider_spans.push(Span::styled(
            "◀ ",
            Style::default().fg(state.theme.primary).add_modifier(Modifier::BOLD),
        ));
        for (i, d) in depths.iter().enumerate() {
            if i > 0 {
                slider_spans.push(Span::styled(" · ", Style::default().fg(state.theme.border)));
            }
            let is_active = *d == cur_depth;
            slider_spans.push(Span::styled(
                d.to_string(),
                if is_active {
                    state.theme.semantic_style(SemanticIntent::Success, Strength::Strong)
                } else {
                    Style::default().fg(state.theme.muted)
                },
            ));
        }
        slider_spans.push(Span::styled(
            " ▶",
            Style::default().fg(state.theme.primary).add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(slider_spans));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

/// 渲染设置模态框
pub fn render_settings_modal(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    // B6：背景遮罩用主题 bg + DIM，避免亮色主题下纯黑突兀（与暗色主题保持一致体感）
    let block = Block::default()
        .style(Style::default().bg(state.theme.bg).add_modifier(Modifier::DIM));
    f.render_widget(block, area);

    // 设置卡片
    let w = 50.min(area.width);
    let h = 12.min(area.height);
    let x = (area.width - w) / 2;
    let y = (area.height - h) / 2;
    let modal_area = Rect::new(x, y, w, h);

    let settings_block = Block::default()
        .title(t("label.settings"))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(state.theme.accent))
        .style(Style::default().bg(state.theme.surface));

    let inner = settings_block.inner(modal_area);
    f.render_widget(settings_block, modal_area);
    let rows = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Min(1),
        ])
        .split(inner);

    let fields: [(&str, String, String); 5] = [
        ("1. API Key", if std::env::var("ABACUS_API_KEY").is_ok() || std::env::var("DEEPSEEK_API_KEY").is_ok() { "✓ 已配置".into() } else { "✗ 未配置".into() }, "只读 · 改 ~/.abacus/config.yaml".into()),
        ("2. Model", state.model_name.clone(), "Enter 循环 (4 内置)".into()),
        ("3. Thinking", state.thinking_depth.clone(), "off→low→med→high".into()),
        ("4. Theme", state.theme.name.into(), "Enter 循环 (12 主题)".into()),
        ("5. 关闭", "".into(), "[Esc]".into()),
    ];

    for (i, (label, value, hint)) in fields.iter().enumerate() {
        let is_focused = i == state.settings_focus;
        let style = if is_focused {
            Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(state.theme.text)
        };
        let line = Line::from(vec![
            Span::styled(format!(" {} ", label), style),
            Span::styled(value.clone(), if value.contains('✗') { Style::default().fg(state.theme.error) } else { Style::default().fg(if is_focused { state.theme.accent } else { state.theme.success }) }.add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {}", hint), state.theme.text_style(TextRole::Caption)),
        ]);
        f.render_widget(Paragraph::new(line), rows[i]);
    }

    // 提示
    let hint = Paragraph::new(Line::from(Span::styled(
        t("overlay.settings_hint"),
        state.theme.text_style(TextRole::Caption),
    )));
    f.render_widget(hint, rows[5]);
}
