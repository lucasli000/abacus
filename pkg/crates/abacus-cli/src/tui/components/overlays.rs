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
use abacus_ui_kit::{SemanticIntent, Strength, TextRole};

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
    // suggested_action 行：Some 时额外占 1 行
    let suggestion_line = if dialog.suggested_action.is_some() { 1 } else { 0 };
    let content_h = (6 + detail_lines + suggestion_line) as u16;
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
            format!("  … +{}{}", remaining, t("msg.more_lines"))
        } else {
            format!("  … +{}{}", remaining, t("msg.hidden_expand"))
        };
        lines.push(Line::from(Span::styled(
            hint,
            state.theme.text_style(TextRole::Caption),
        )));
    }

    lines.push(Line::raw(""));

    // 系统建议行：带颜色 badge，让用户直观看到引擎对此请求的判断
    match dialog.suggested_action {
        Some(true) => {
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    " ✓ 系统建议允许 ",
                    Style::default()
                        .fg(crate::tui::effects::auto_contrast_fg(state.theme.success))
                        .bg(state.theme.success)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(t("confirm.safe"), Style::default().fg(state.theme.muted)),
            ]));
        }
        Some(false) => {
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    " ⚠ 系统建议拒绝 ",
                    Style::default()
                        .fg(crate::tui::effects::auto_contrast_fg(state.theme.error))
                        .bg(state.theme.error)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(t("confirm.risky"), Style::default().fg(state.theme.muted)),
            ]));
        }
        None => {} // 系统无法判断，不加辅助行，保持简洁
    }

    // 倒计时行：剩余秒数 + 超时后的默认动作（颜色与动作语义一致）
    let remaining = dialog.remaining_secs();
    let (timeout_label, timeout_color) = if dialog.risk == ConfirmRisk::High {
        (t("confirm.deny"), state.theme.error)
    } else {
        (t("confirm.allow"), state.theme.success)
    };
    let countdown_color = if remaining <= 3 {
        state.theme.error
    } else if remaining <= 6 {
        state.theme.gold
    } else {
        state.theme.muted
    };
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("⏱ ", Style::default().fg(countdown_color)),
        Span::styled(
            format!("{}s", remaining),
            Style::default().fg(countdown_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(t("confirm.after"), Style::default().fg(countdown_color)),
        Span::styled(timeout_label, Style::default().fg(timeout_color).add_modifier(Modifier::BOLD)),
    ]));

    // 按键按钮行：selected 项加箭头 + UNDERLINED
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
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};
    use crate::tui::util::{display_width, truncate_to_width, pad_to_width};

    let candidates = &state.completion_candidates;
    if candidates.is_empty() { return; }

    let is_slash = candidates.first().map(|c| c.starts_with('/')).unwrap_or(false);
    let frame = f.area();

    // ── 宽度：input_area × 80%（稍宽一些给 2 列留空间）────────
    // P2 修复：最小宽 30 （> render_min_terminal_warning 的 20 保护阈値）
    let popup_w: u16 = (input_area.width as u32 * 80 / 100)
        .max(30).min(frame.width as u32) as u16;

    // ── 列数：内容宽 >= 52 用 2 列，否则 1 列 ──────────────────
    let inner_w = popup_w.saturating_sub(2) as usize; // 减去左右边框
    let ncols: usize = if is_slash && inner_w >= 52 { 2 } else { 1 };
    let col_w: usize = if ncols == 2 { (inner_w - 1) / 2 } else { inner_w }; // -1 for │ separator

    // 名字列宽：取所有候选最长名 + 1，但不超过 col_w × 45%
    let max_name_w: usize = candidates.iter()
        .map(|c| display_width(c.as_str()))
        .max().unwrap_or(8)
        .min(col_w * 45 / 100)
        .max(6);
    // 描述列宽：col_w - marker(2) - name(max_name_w) - gap(1)
    let desc_w: usize = col_w.saturating_sub(max_name_w + 3);

    // ── 高度：候选行数（grid 行数 = ceil(total / ncols)）+ 边框 ─
    let total = candidates.len();
    let nrows_total = total.div_ceil(ncols);
    let max_h_by_messages: u16 = (messages_area.height as u32 * 45 / 100).max(3) as u16;
    let max_h_by_above: u16 = input_area.y.saturating_sub(1).max(3);
    let max_h: u16 = max_h_by_messages.min(max_h_by_above);
    let popup_h: u16 = ((nrows_total + 2) as u16).min(max_h).max(3);

    // ── 位置：贴 input 上方，左对齐 ────────────────────────────
    let popup_y: u16 = input_area.y.saturating_sub(popup_h);
    let popup_x: u16 = input_area.x.min(frame.width.saturating_sub(popup_w));
    let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);

    f.render_widget(Clear, popup_area);

    // P7 修复：标题按弹窗宽度截断，防止窄终端超出
    let title_full = " ↑↓ Tab · Enter · Esc ";
    let title: String = crate::tui::util::truncate_to_width(title_full, popup_w.saturating_sub(4) as usize);
    let block = Block::default()
        .title(title.clone())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(state.theme.primary))
        .style(Style::default().bg(state.theme.elevated));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let visible_rows: usize = inner.height as usize;
    // 选中项所在行（grid row）
    let selected_row = state.completion_index / ncols;
    let scroll_start_row: usize = if nrows_total <= visible_rows {
        0
    } else {
        let half = visible_rows / 2;
        selected_row.saturating_sub(half).min(nrows_total - visible_rows)
    };

    let mut lines: Vec<Line> = Vec::new();
    for row_i in scroll_start_row..scroll_start_row + visible_rows {
        if row_i >= nrows_total { break; }

        let mut spans: Vec<Span<'static>> = Vec::new();
        for col_i in 0..ncols {
            let idx = row_i * ncols + col_i;

            // 列分隔符
            if col_i > 0 {
                spans.push(Span::styled(
                    "│",
                    Style::default().fg(state.theme.border).add_modifier(Modifier::DIM),
                ));
            }

            if idx >= total {
                // 空白填充（最后一行可能列不满）
                spans.push(Span::raw(" ".repeat(col_w)));
                continue;
            }

            let candidate = &candidates[idx];
            let is_selected = idx == state.completion_index;

            // 描述（仅斜杠命令有）
            let desc: String = if is_slash {
                crate::tui::slash_commands::command_desc_by_name(candidate)
                    .map(|d| {
                        // 只取第一段（' - ' 前，或全部），截断到 desc_w
                        let short = d.split(" - ").next().unwrap_or(d);
                        truncate_to_width(short, desc_w)
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };

            // 名称截断 + 对齐
            let name_trunc = truncate_to_width(candidate, max_name_w);
            let name_padded = pad_to_width(&name_trunc, max_name_w);

            let marker = if is_selected { "❯ " } else { "  " };
            let marker_style = if is_selected {
                Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // P5 修复：选中项用显式 accent 背景色而非 REVERSED，避免浅色主题下对比度不足
            let name_style = if is_selected {
                Style::default()
                    .fg(crate::tui::effects::auto_contrast_fg(state.theme.accent))
                    .bg(state.theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(state.theme.text)
            };
            let desc_style = Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM);

            // P3 修复：滚动指示符独立于 desc，无描述时也能显示
            let scroll_hint = if nrows_total > visible_rows && col_i == 0 {
                if row_i == scroll_start_row && scroll_start_row > 0 { "↑ " }
                else if row_i == scroll_start_row + visible_rows - 1
                    && scroll_start_row + visible_rows < nrows_total { "↓ " }
                else { "" }
            } else { "" };

            spans.push(Span::styled(marker.to_string(), marker_style));
            spans.push(Span::styled(name_padded, name_style));
            if !scroll_hint.is_empty() {
                // 将滚动指示符附加在行尾（占用 2 字符）
                spans.push(Span::styled(scroll_hint.to_string(),
                    Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM)));
            }
            if desc_w > 0 && !desc.is_empty() {
                spans.push(Span::raw(" "));
                let avail_desc = if scroll_hint.is_empty() { desc_w } else { desc_w.saturating_sub(2) };
                spans.push(Span::styled(truncate_to_width(&desc, avail_desc), desc_style));
            }
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), inner);
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
    // 2026-05-28: 全屏编辑器覆盖所有其他 overlay
    if state.input_state == crate::tui::state::InputState::Editor {
        render_fullscreen_editor(f, state);
        return;
    }
    render_toasts(f, state);
    render_confirm_dialog(f, state, input_area);
    // P1 z-order 修复：confirm_dialog 存在时不渲染其他弹窗，避免遇策弹窗被遗漏覆盖
    if state.confirm_dialog.is_none() {
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
}

/// 命令参数 picker popup（V13）
///
/// 引用关系：state.picker = Some(...) 时由 render_overlays 调用
/// 生命周期：picker 打开期间每帧绘制；Enter/Esc 关闭后由后续帧不再调用
/// 设计意图：`/model`/`/theme`/`/thinking` 等斜杠命令输入即弹出
///           可视化选择器，箭头选 + Enter 确认
pub fn render_picker_popup(f: &mut ratatui::Frame, state: &AppState, input_area: Rect) {
    use crate::tui::state::PickerKind;
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let Some(p) = state.picker.as_ref() else { return; };
    if p.items.is_empty() { return; }

    // 标题 — 各 picker 语义化
    let title = match p.kind {
        PickerKind::Model    => format!("{} ({})", t("overlay.model_picker"), p.items.len()),
        PickerKind::Theme    => format!("{} ({})", t("overlay.theme_picker"), p.items.len()),
        PickerKind::Thinking => t("overlay.thinking_picker").to_string(),
        PickerKind::Mode     => t("picker.mode").to_string(),
        PickerKind::Review   => t("picker.review").to_string(),
        PickerKind::Resume   => format!(" {} ({}) ", t("picker.resume"), p.items.len()),
        PickerKind::History  => format!(" {} ({}) ", t("picker.history"), p.items.len()),
        PickerKind::Meeting  => format!(" 🧠 {} ", t("mode.meeting")),
        PickerKind::Preset   => t("picker.preset").to_string(),
    };
    let frame = f.area();

    // 宽度：输入框的 6/8，下限 40（与 confirm dialog 统一）
    let popup_w = (input_area.width * 6 / 8).max(40).min(frame.width);
    let group_overhead = p.groups.as_ref().map(|g| g.len()).unwrap_or(0);
    let slider_overhead = if p.show_thinking_slider { 2 } else { 0 };
    // 底部键位提示行（所有 picker 都有）+ Review picker 的 strict toggle 行
    let hint_overhead = 1;
    let strict_overhead = if matches!(p.kind, PickerKind::Review) { 1 } else { 0 };
    let content_lines = p.items.len() + group_overhead + slider_overhead + hint_overhead + strict_overhead;
    // 高度上限：消息区（input_area.y 近似消息框高度）的 1/3
    let msg_area_h = input_area.y as usize;
    // picker 弹窗高度上限：取消息区 1/2，至少 10
    // 原来 1/3.max(6) 在 24 行终端下上限=7，去掉 border+slider 只剩 2 行，模型名称几乎看不到
    let max_h = (msg_area_h / 2).max(10);
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

    let mut lines: Vec<Line> = Vec::new();

    // 渲染项闭包 — 固定 4 字符前缀（marker+空格），无冗余 padding
    let render_item = |idx: usize, lines: &mut Vec<Line>| {
        let label = &p.labels[idx];
        let id = &p.items[idx];
        let is_sel = idx == p.selected;
        let is_cur = p.current == Some(idx);
        let marker = if is_sel && is_cur { "▶●" }
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
        spans.push(Span::styled(label.clone(), row_style));
        if matches!(p.kind, PickerKind::Theme) {
            let t = abacus_ui_kit::from_name(id);
            spans.push(Span::raw(" "));
            spans.push(Span::styled("██", Style::default().fg(t.primary)));
            spans.push(Span::styled("██", Style::default().fg(t.accent)));
            spans.push(Span::styled("██", Style::default().fg(t.success)));
        }
        lines.push(Line::from(spans));
    };

    // 分组渲染（含滚动）— 先建全量行列表，追踪选中行视觉位置，再截取可见窗口
    // Fix: 之前分组不滚动，模型多时超出高度部分不可见
    // 底部保留行：thinking slider(2) + hint(1) + strict toggle(1)
    let bottom_reserved = if p.show_thinking_slider { 2 } else { 0 }
        + 1  // hint 行
        + if matches!(p.kind, PickerKind::Review) { 1 } else { 0 };
    let item_max_visible = max_visible.saturating_sub(bottom_reserved);

    if let Some(ref groups) = p.groups {
        let mut selected_visual: usize = 0;
        let mut visual_line: usize = 0;
        for (provider, range) in groups {
            lines.push(Line::from(vec![
                Span::styled(format!(" ─ {}", provider), Style::default().fg(state.theme.muted)),
            ]));
            visual_line += 1;
            for idx in range.clone() {
                if idx == p.selected { selected_visual = visual_line; }
                render_item(idx, &mut lines);
                visual_line += 1;
            }
        }
        // P6 修复：滚动时保留分组标题行。
        // 计算滚动偏移：选中行尽量居中，不超出边界
        let mut scroll_start = if lines.len() <= item_max_visible {
            0
        } else {
            selected_visual.saturating_sub(item_max_visible / 2)
                .min(lines.len().saturating_sub(item_max_visible))
        };
        // 如果 scroll_start 指向分组第一个项（scroll_start-1 是组标题），往上展一行包含标题
        if scroll_start > 0 {
            // 组标题行形式为 " ─ provider"，以单行加组名识别
            let prev_line_text: String = lines[scroll_start - 1]
                .spans.iter().map(|s| s.content.as_ref()).collect();
            if prev_line_text.trim_start().starts_with('─') {
                scroll_start -= 1; // 包含该组标题
            }
        }
        lines.drain(0..scroll_start);
        lines.truncate(item_max_visible);
    } else {
        // Theme / Thinking：简单滚动窗口
        let scroll_start = if p.items.len() <= item_max_visible {
            0
        } else {
            p.selected.saturating_sub(2).min(p.items.len().saturating_sub(item_max_visible))
        };
        for idx in scroll_start..(scroll_start + item_max_visible).min(p.items.len()) {
            render_item(idx, &mut lines);
        }
    }

    // 底部 thinking 调节器 (Model picker 专属)
    // Fix: 使用 THINKING_SLIDER_DEPTHS 单一真相（含 max），之前硬编码 4 档漏掉 max
    if p.show_thinking_slider {
        lines.push(Line::raw("")); // 空行分隔
        let depths = crate::tui::state::AppState::THINKING_SLIDER_DEPTHS;
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

    // Review picker：strict toggle 行
    if matches!(p.kind, PickerKind::Review) {
        let (box_char, box_style) = if p.review_strict {
            ("■", Style::default().fg(state.theme.error).add_modifier(Modifier::BOLD))
        } else {
            ("□", Style::default().fg(state.theme.muted))
        };
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(box_char, box_style),
            Span::styled(
                " --strict  verdict≠pass 时阻断执行",
                if p.review_strict {
                    Style::default().fg(state.theme.error)
                } else {
                    Style::default().fg(state.theme.muted)
                },
            ),
        ]));
    }

    // 底部键位提示行（所有 picker）— 让用户发现隐藏操作
    let hint = match p.kind {
        PickerKind::Model    => t("picker.hint_model"),
        PickerKind::Theme    => t("picker.hint_theme"),
        PickerKind::Thinking => t("picker.hint_thinking"),
        PickerKind::Mode     => t("picker.hint_mode"),
        PickerKind::Review   => t("picker.hint_review"),
        PickerKind::Resume   => t("picker.hint_session"),
        PickerKind::History  => t("picker.hint_history"),
        PickerKind::Meeting  => t("picker.hint_generic"),
        PickerKind::Preset   => t("picker.hint_preset"),
    };
    lines.push(Line::from(vec![
        Span::styled(hint, Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM)),
    ]));

    f.render_widget(Paragraph::new(lines), inner);
}

/// 渲染设置模态框
pub fn render_settings_modal(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    // B6：背景遮罩用 elevated 颜色，避免亮色主题下纯黑突兀（与暗色主题保持一致体感）
    let block = Block::default()
        .style(Style::default().bg(state.theme.elevated));
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
        ("1. API Key", if std::env::var("ABACUS_API_KEY").is_ok() || std::env::var("DEEPSEEK_API_KEY").is_ok() { t("overlay.configured").into() } else { t("overlay.not_configured").into() }, "~/.abacus/provider.toml".into()),
        ("2. Model", state.model_name.clone(), t("overlay.model_cycle").into()),
        ("3. Thinking", state.thinking_depth.clone(), "off→low→med→high".into()),
        ("4. Theme", state.theme.name.into(), t("overlay.theme_cycle").into()),
        ("5. Close", "".into(), "[Esc]".into()),
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

// ════════════════════════════════════════════════════════════════
// 全屏编辑器 (2026-05-28)
// ════════════════════════════════════════════════════════════════

/// 全屏编辑器渲染 — 覆盖整个终端区域
///
/// 布局：
///   ╭─ 编辑器 (Ctrl+S 发送 · Esc 取消) ──────────╮
///   │  1 │ 文本内容...                              │
///   │  2 │ ...                                      │
///   ╰─ Ln 1, Col 0 · 2 lines · 42 chars ──────────╯
///
/// 引用关系：render_overlays 在 InputState::Editor 时调用
/// 生命周期：编辑器打开期间每帧绘制
fn render_fullscreen_editor(f: &mut ratatui::Frame, state: &AppState) {
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let frame = f.area();
    let ed = match &state.editor_state {
        Some(e) => e,
        None => return,
    };

    // 外框
    let title = t("picker.editor");
    let total_lines = state.input.matches('\n').count() + 1;
    let total_chars = state.input.chars().count();
    let bottom_info = format!(
        " Ln {}, Col {} · {} lines · {} chars ",
        state.cursor_line + 1, state.cursor_col + 1, total_lines, total_chars
    );

    let block = Block::default()
        .title(title)
        .title_bottom(Line::from(Span::styled(&bottom_info, Style::default().fg(state.theme.muted))))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(state.theme.accent))
        .style(Style::default().bg(state.theme.bg));

    f.render_widget(Clear, frame);
    let inner = block.inner(frame);
    f.render_widget(block, frame);

    if inner.width < 10 || inner.height < 3 { return; }

    // 行号宽度（动态）：至少 3 位 + 1 分隔
    let line_num_width: u16 = {
        let digits = format!("{}", total_lines).len().max(3);
        (digits + 1) as u16  // "NNN│"
    };
    let content_width = inner.width.saturating_sub(line_num_width) as usize;
    let visible_h = inner.height as usize;

    // 滚动计算
    let scroll_top = ed.scroll_top;
    let lines: Vec<&str> = state.input.split('\n').collect();

    // 写入 last_visible_h 供键盘侧精确计算 PgUp/PgDn（Cell 允许 &self 下修改）
    if let Some(ref ed) = state.editor_state {
        ed.last_visible_h.set(visible_h);
    }

    // 代码围栏状态：只扫描 0..scroll_top 确定初始态（O(scroll_top) 但避免 O(total_lines) 每帧分配）
    // 然后在渲染循环中逐行追踪
    let mut fence_active = false;
    for i in 0..scroll_top.min(lines.len()) {
        if lines[i].trim_start().starts_with("```") {
            fence_active = !fence_active;
        }
    }

    // 渲染可见行
    let mut render_lines: Vec<Line> = Vec::with_capacity(visible_h);
    for vis_row in 0..visible_h {
        let line_idx = scroll_top + vis_row;
        if line_idx >= lines.len() {
            // 空行（编辑区超过文本行数）
            let num_span = Span::styled(
                format!("{:>width$}│", "~", width = (line_num_width - 1) as usize),
                Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM),
            );
            render_lines.push(Line::from(vec![num_span]));
            continue;
        }

        let line_text = lines[line_idx];
        let is_cursor_line = line_idx == state.cursor_line;

        // 行号
        let num_style = if is_cursor_line {
            Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(state.theme.muted)
        };
        let num_span = Span::styled(
            format!("{:>width$}│", line_idx + 1, width = (line_num_width - 1) as usize),
            num_style,
        );

        // 内容（带 Markdown 语法着色 + code fence 状态）
        // 逐行追踪围栏状态（toggle on ``` line，内部行标记 in_fence）
        let is_fence_marker = line_text.trim_start().starts_with("```");
        let is_in_fence = if is_fence_marker {
            fence_active = !fence_active;
            false // ``` 行本身不标记为 in_fence（有自己的围栏行样式）
        } else {
            fence_active
        };
        let content_spans = highlight_markdown_line(line_text, state, content_width, is_in_fence);

        let mut spans = vec![num_span];
        spans.extend(content_spans);
        render_lines.push(Line::from(spans));
    }

    let para = Paragraph::new(render_lines);
    f.render_widget(para, inner);

    // 光标定位
    let visual_line = state.cursor_line.saturating_sub(scroll_top);
    if visual_line < visible_h {
        let cursor_x = inner.x.saturating_add(line_num_width).saturating_add(state.cursor_col as u16);
        let cursor_y = inner.y.saturating_add(visual_line as u16);
        if cursor_x < inner.x.saturating_add(inner.width) && cursor_y < inner.y.saturating_add(inner.height) {
            f.set_cursor_position((cursor_x, cursor_y));
        }
    }
}

/// Markdown 行级语法着色（零依赖，支持 inline parse）
///
/// 参数 in_code_block: 此行是否在 ``` 围栏内部（由调用方跟踪状态）
fn highlight_markdown_line<'a>(
    line: &'a str,
    state: &AppState,
    _max_width: usize,
    in_code_block: bool,
) -> Vec<Span<'a>> {
    let trimmed = line.trim_start();

    // 围栏内部行 → 代码色（muted + DIM）
    if in_code_block {
        return vec![Span::styled(
            line,
            Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM),
        )];
    }

    // 围栏标记行本身
    if trimmed.starts_with("```") {
        return vec![Span::styled(
            line,
            Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM),
        )];
    }

    // 标题
    if trimmed.starts_with("# ") || trimmed.starts_with("## ") || trimmed.starts_with("### ")
        || trimmed.starts_with("#### ")
    {
        return vec![Span::styled(
            line,
            Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD),
        )];
    }

    // 列表项
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        let indent = line.len() - trimmed.len();
        let prefix_end = indent + 2;
        let mut spans = vec![
            Span::styled(&line[..prefix_end], Style::default().fg(state.theme.gold)),
        ];
        spans.extend(parse_inline_markdown(&line[prefix_end..], state));
        return spans;
    }

    // 数字列表
    if trimmed.len() > 2 && trimmed.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
        && trimmed.contains(". ")
    {
        let indent = line.len() - trimmed.len();
        if let Some(dot_pos) = trimmed.find(". ") {
            let prefix_end = indent + dot_pos + 2;
            let mut spans = vec![
                Span::styled(&line[..prefix_end], Style::default().fg(state.theme.gold)),
            ];
            spans.extend(parse_inline_markdown(&line[prefix_end..], state));
            return spans;
        }
    }

    // 有 inline 标记 → 逐字符 parse
    if line.contains('`') || line.contains("**") {
        return parse_inline_markdown(line, state);
    }

    // 默认：普通文本
    vec![Span::styled(line, Style::default().fg(state.theme.text))]
}

/// Inline markdown 解析：`code` 和 **bold** 片段着色
/// 返回按顺序排列的 Span 列表
fn parse_inline_markdown<'a>(text: &'a str, state: &AppState) -> Vec<Span<'a>> {
    let mut spans: Vec<Span<'a>> = Vec::new();
    let mut i = 0;
    let bytes = text.as_bytes();
    let len = bytes.len();
    let text_style = Style::default().fg(state.theme.text);
    let code_style = Style::default().fg(state.theme.muted);
    let bold_style = Style::default().fg(state.theme.text).add_modifier(Modifier::BOLD);

    while i < len {
        // 行内代码 `...`
        if bytes[i] == b'`' && i + 1 < len {
            // 不处理 ``` (那是围栏)
            if i + 2 < len && bytes[i + 1] == b'`' && bytes[i + 2] == b'`' {
                spans.push(Span::styled(&text[i..], text_style));
                break;
            }
            if let Some(end) = text[i + 1..].find('`') {
                let code_end = i + 1 + end;
                spans.push(Span::styled(&text[i..=code_end], code_style));
                i = code_end + 1;
                continue;
            }
        }
        // **bold**
        if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            if let Some(end) = text[i + 2..].find("**") {
                let bold_end = i + 2 + end + 2;
                spans.push(Span::styled(&text[i..bold_end], bold_style));
                i = bold_end;
                continue;
            }
        }
        // 普通文本：扫描到下一个特殊字符
        let start = i;
        while i < len && bytes[i] != b'`' && !(i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'*') {
            i += 1;
        }
        if i > start {
            spans.push(Span::styled(&text[start..i], text_style));
        }
        // 防止无限循环（单个 * 不匹配时跳过）
        if i < len && i == start {
            let ch_len = text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            spans.push(Span::styled(&text[i..i + ch_len], text_style));
            i += ch_len;
        }
    }
    if spans.is_empty() {
        spans.push(Span::styled(text, text_style));
    }
    spans
}
