//! Abacus TUI Components — 公共组件库
//!
//! 设计规范来源: ABACUS-TUI-DESIGN-SPEC.md v1.0
//!
//! 组件:
//!   Card — 带阴影/圆角的卡片容器
//!   Toast — 右上角通知
//!   TopBar — 顶部标题栏
//!   StatusBar — 底部状态栏
//!   InputBar — 输入区域
//!   MessageList — 消息流列表
//!   Panel — 右侧看板 (含 Tab)
//!   ExpertList — 专家列表 (Meeting/Team 共用)
//!
//! ## ⚠ 代码审查 @2025-01-23 (严重)
//! 本文件 213KB 是最大单体文件。包含：
//! - 消息行构建 (build_message_lines)
//! - 卡片渲染 (CardBuilder)
//! - 输入栏、状态栏、顶栏
//! - 面板、Tab、时间线
//! - Toast、确认对话框、补全弹窗、文件选择器、设置模态框
//! - 全局背景、极简终端警告
//!
//! 建议拆分为:
//!   components/card.rs   — CardBuilder + render_card_bar
//!   components/messages.rs — build_message_lines + render_messages + render_messages_in_card
//!   components/input_bar.rs — render_input_bar_focused
//!   components/panel.rs — render_panel + render_panel_* + build_tab_spans
//!   components/overlays.rs — toasts + confirm_dialog + completion_popup + picker_popup + settings_modal
//!
//! 拆分不改变公开 API (pub fn)，纯内部重组。

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListDirection, Paragraph, Widget};

use crate::tui::effects;
use crate::tui::markdown::{self, LineType};
use crate::tui::state::{
    AbacusMode, AppState, BlockKind, ExpertStatus, Focus, InputState, MsgContent, MsgRole,
    PanelTab, TaskStatus,
};
use crate::tui::theme::{SemanticIntent, Strength, TextRole, Theme};

// ════════════════════════════════════════════════════════════════
// 共享消息行构建 (避免 render_messages / render_messages_in_card 重复)
// ════════════════════════════════════════════════════════════════

/// 代码块超过此行数时折叠（Ctrl+E 展开全部）
const CODE_BLOCK_MAX_LINES: u32 = 20;

/// V28.3 (PR9): trace_part_positions out-param 让 render_messages_in_card 在 scroll 之后
/// 把 line_idx 转换为绝对屏幕 y, 写入 state.message_trace_row_map 供鼠标点击反查。
/// 元素: (line_idx_in_returned_lines, msg_idx_in_messages_slice, part_idx_for_toggle_block)
/// part_idx 计数语义: Block + Trace 共用空间(同 toggle_block 内部计数),Stream 不计入。
fn build_message_lines(
    messages: &[crate::tui::state::Message],
    scroll: usize,
    theme: &Theme,
    selection: &Option<crate::tui::state::TextSelection>,
    max_width: u16,
    stream_cursor: usize,
    compact: bool,
    code_blocks_expanded: bool,
    trace_events: &[crate::tui::state::TraceEvent],
    trace_part_positions: &mut Vec<(usize, usize, usize)>,
    // V28.4: focused event 锚点 — 双视图同步高亮该 event（消息侧 Trace 子块加 bg）
    // 引用关系：被 line ~290 处 `focused_event_id == Some(*id)` 用于 is_focused 判定
    focused_event_id: Option<u64>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    // 色条风格：┃ + 缩进（替代旧版 gutter 空格）
    // 缩进层级：header=1空格, content=3空格, code=4空格
    let bar_indent = 4usize; // ┃ + 3空格（内容行）
    let content_width = (max_width as usize).saturating_sub(bar_indent + 1);

    // V30 复制修复：selection 高亮范围。由于 build_message_lines 过程中 markdown 渲染 +
    // word-wrap + code fold 会打乱 “span 原始 char offset” 映射，本阶段采用 msg-级高亮：
    // 所有落在 [s_msg, e_msg] 区间内的 msg 生成的行都加 REVERSED 修饰。
    // 字符级视觉高亮作为后续优化项，记在 TODO 里。
    let sel_msg_range: Option<(usize, usize)> = selection.as_ref().map(|s| {
        let lo = s.start_msg_idx.min(s.end_msg_idx);
        let hi = s.start_msg_idx.max(s.end_msg_idx);
        (lo, hi)
    });
    let in_selected_msg = |idx: usize| -> bool {
        sel_msg_range.map(|(lo, hi)| idx >= lo && idx <= hi).unwrap_or(false)
    };

    // 空消息列表：显示欢迎提示（非 streaming 时）
    if messages.is_empty() && stream_cursor == 0 {
        let hint_style = theme.text_style(TextRole::Hint);
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled("输入问题开始对话，/help 查看命令", hint_style),
        ]));
        return lines;
    }

    // 性能保护：只渲染 scroll 之后最多 50 条消息（约 200 行可见区域上限）
    let max_visible_msgs = 50;
    for (visible_idx, msg) in messages.iter().skip(scroll).take(max_visible_msgs).enumerate() {
        let actual_idx = scroll + visible_idx;
        let msg_lines_start = lines.len(); // V30: 记录本 msg 第一行下标，末尾一次性 paint selection bg
        // V28.3: 计数 Block+Trace 在本 msg.parts 中的位置(对齐 toggle_block 内部计数)
        let mut bi: usize = 0;
        let is_user = matches!(msg.role, MsgRole::User);
        let (_display_name, role_color, role_icon) = match &msg.role {
            MsgRole::User => ("You", theme.user, "🙋"),
            MsgRole::Session => ("Abacus", theme.session, "🤖"),
            MsgRole::Expert(name) => (name.as_str(), theme.expert, "🧠"),
        };

        // ── 色条（贯穿该消息所有行）──
        let bar = Span::styled("┃", Style::default().fg(role_color));

        // ── Header: ┃ icon name · time ──
        let display_name = match &msg.role {
            MsgRole::User => "You",
            MsgRole::Session => "Abacus",
            MsgRole::Expert(name) => name.as_str(),
        };
        let badge = Span::styled(
            format!("{} {}", role_icon, display_name),
            Style::default().fg(role_color).add_modifier(Modifier::BOLD),
        );
        let ts = Span::styled(
            format!(" · {}", msg.time),
            theme.text_style(TextRole::Caption),
        );
        lines.push(Line::from(vec![bar.clone(), Span::raw(" "), badge, ts]));

        for part in &msg.parts {
            match part {
                MsgContent::Stream(text) => {
                    // Markdown 渲染 + word-wrap + 代码块折叠
                    // V27: 把可用内容宽度传入 markdown 层,让表格按宽度预算缩列宽
                    let styled_lines = markdown::render_markdown_bounded(text, theme, is_user, content_width);
                    let muted_dim = theme.text_style(TextRole::Caption);
                    // 代码块折叠追踪（per-Stream 段）
                    let mut in_cb = false;        // 是否在代码块中
                    let mut cb_line_count = 0u32; // 当前代码块已显示的行数
                    let mut cb_hidden = 0u32;     // 当前代码块被折叠的行数
                    for styled in &styled_lines {
                        // ── 代码块边界追踪 ──────────────────────────────────
                        if styled.line_type == LineType::CodeFence {
                            if !in_cb {
                                in_cb = true;
                                cb_line_count = 0;
                                cb_hidden = 0;
                            } else {
                                in_cb = false;
                                // 关闭 fence 前插入折叠提示行
                                if cb_hidden > 0 {
                                    lines.push(Line::from(vec![
                                        bar.clone(),
                                        Span::raw("   "),
                                        Span::styled(
                                            format!("↳ +{} 行  Ctrl+E 展开全部", cb_hidden),
                                            muted_dim,
                                        ),
                                    ]));
                                }
                            }
                        } else if styled.line_type == LineType::Code && in_cb && !code_blocks_expanded {
                            if cb_line_count >= CODE_BLOCK_MAX_LINES {
                                cb_hidden += 1;
                                continue; // 跳过超出行，不加入 lines
                            }
                            cb_line_count += 1;
                        }

                        let rline = markdown::styled_line_to_ratatui(styled, &bar, theme);
                        // V27: 表格行已在 markdown 层完成宽度收缩,且 box-drawing 字符不可拆分
                        // → 豁免 word-wrap,直接 push;否则会把 │┌┴┘ 切成两半导致渲染断裂
                        if styled.line_type == LineType::Table {
                            lines.push(rline);
                            continue;
                        }
                        // Word-wrap：对超宽行按 content_width 拆分
                        let line_w = rline.spans.iter()
                            .map(|s| crate::tui::util::display_width(s.content.as_ref()))
                            .sum::<usize>();
                        if line_w <= content_width + bar_indent {
                            lines.push(rline);
                        } else {
                            // 超宽行需要拆分：提取纯文本内容并 word-wrap
                            // 色条+缩进 由 styled_line_to_ratatui 已添加在前两个 span
                            // 实际文本从第 2 个 span 之后开始
                            let indent_str = match styled.line_type {
                                LineType::Code => "    ",
                                _ => "   ",
                            };
                            // 合并所有内容 span 的文本
                            let full_text: String = styled.spans.iter().map(|s| s.text.as_str()).collect();
                            let text_style = styled.spans.first()
                                .map(|s| s.style)
                                .unwrap_or(Style::default().fg(theme.text));
                            // word-wrap
                            let wrap_width = content_width;
                            let mut remaining = full_text.as_str();
                            while !remaining.is_empty() {
                                let mut width = 0;
                                let mut take = 0;
                                let mut last_boundary = 0;
                                for (i, ch) in remaining.char_indices() {
                                    let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                                    if width + ch_w > wrap_width { break; }
                                    width += ch_w;
                                    take = i + ch.len_utf8();
                                    if ch == ' ' || ch == '-' || ch == '/'
                                        || unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) > 1 {
                                        last_boundary = take;
                                    }
                                }
                                if take < remaining.len() && last_boundary > 0 && last_boundary > take / 2 {
                                    take = last_boundary;
                                }
                                if take == 0 {
                                    take = remaining.chars().next().map(|c| c.len_utf8()).unwrap_or(remaining.len());
                                }
                                lines.push(Line::from(vec![
                                    bar.clone(),
                                    Span::raw(indent_str.to_string()),
                                    Span::styled(remaining[..take].to_string(), text_style),
                                ]));
                                remaining = &remaining[take..];
                            }
                        }
                    }
                }
                MsgContent::Block {
                    kind,
                    summary,
                    collapsed,
                    detail,
                } => {
                    // V11: ToolCall 失败时染红，给错误调用以视觉警示
                    //   通过 summary 首字符 ✗ 检测（归档时按 ToolStatus::Failed 写入）
                    let (icon, block_color) = match kind {
                        BlockKind::Think => ("💭", theme.accent),
                        BlockKind::ToolCall => {
                            if summary.starts_with('✗') {
                                ("⚙", theme.error)
                            } else {
                                ("⚙", theme.gold)
                            }
                        }
                        BlockKind::Checklist => ("☐", theme.success),
                    };
                    let arrow = if *collapsed { "▸" } else { "▾" };
                    // V28.3: 记录 Block summary 行位置(让消息侧 ▸/▾ 也支持点击 toggle)
                    trace_part_positions.push((lines.len(), actual_idx, bi));
                    lines.push(Line::from(vec![
                        bar.clone(),
                        Span::raw("   "),
                        Span::styled(
                            format!("{} {} {}", arrow, icon, summary),
                            Style::default().fg(block_color).add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                    if !*collapsed {
                        // V12: 按 BlockKind 分流详情渲染——避免"一坨纯文本"
                        //   Think     → markdown 渲染（思考链常含列表/加粗/代码引用）
                        //   ToolCall  → JSON pretty-print + InlineCode 染色（args 实为 output JSON）
                        //   Checklist → Caption（清单结构简单，保留旧行为）
                        let detail_lines = render_block_detail(detail, kind, theme);
                        for dl in detail_lines {
                            // 每行加 bar 前缀和缩进，保持视觉一致
                            let mut spans: Vec<Span> = vec![bar.clone(), Span::raw("     ")];
                            spans.extend(dl.spans);
                            lines.push(Line::from(spans));
                        }
                    }
                    bi += 1; // V28.3: 在 part 计数空间(Block + Trace)递增
                }
                // V28 (T5): Trace 折叠/展开完整渲染
                // - 折叠态: 单行 ▸ trace · N行思考 · M工具 · X.Ys
                // - 展开态: 头行 ▾ trace + 每个 event 一子块,thinking ≤30行折叠 / tool ≤20行
                MsgContent::Trace { event_ids, collapsed, expanded_event_ids } => {
                    use crate::tui::state::TraceKind;

                    // 聚合摘要数字 — 按 ids 反查 trace_events,miss 跳过(悬挂引用降级)
                    let referenced: Vec<&crate::tui::state::TraceEvent> = event_ids.iter()
                        .filter_map(|id| trace_events.iter().find(|e| e.id == *id))
                        .collect();
                    let mut think_lines = 0usize;
                    let mut tool_count = 0usize;
                    let mut total_dur_ms: u64 = 0;
                    for ev in &referenced {
                        match &ev.kind {
                            TraceKind::Thinking { lines: l, .. } => think_lines += l,
                            TraceKind::ToolCall { .. } => tool_count += 1,
                            _ => {}
                        }
                        if let Some(d) = ev.duration_ms { total_dur_ms += d; }
                    }
                    let dur_part = {
                        let d = format_duration_ms(total_dur_ms);
                        if d.is_empty() { String::new() } else { format!(" · {}", d) }
                    };

                    // V28 (T7 V1): Ctrl+E 全局开关同时驱动 Trace 展开
                    // 优先级: code_blocks_expanded(全局展开)> !collapsed(单 part 展开)
                    let effectively_expanded = code_blocks_expanded || !*collapsed;
                    let arrow = if effectively_expanded { "▾" } else { "▸" };
                    // V28.3: 记录 Trace summary 行位置(让消息侧 ▸/▾ 也支持点击 toggle)
                    trace_part_positions.push((lines.len(), actual_idx, bi));
                    // V29.12: Trace summary 用分段着色 — arrow 用 accent 暗示可交互,
                    //   "trace" 标签用 muted,数字统计用 Caption(DIM);
                    //   之前全 Caption 导致视觉权重过低,与 Block summary 不对称
                    // V29.12: summary 只显示非零项,避免 "0行思考" / "0工具" 噪声
                    let mut summary_parts: Vec<String> = Vec::new();
                    if think_lines > 0 { summary_parts.push(format!("{}行思考", think_lines)); }
                    if tool_count > 0 { summary_parts.push(format!("{}工具", tool_count)); }
                    if !dur_part.is_empty() { summary_parts.push(dur_part.trim_start_matches(" · ").to_string()); }
                    let summary_suffix = if summary_parts.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", summary_parts.join(" · "))
                    };
                    lines.push(Line::from(vec![
                        bar.clone(),
                        Span::raw("   "),
                        Span::styled(
                            format!("{} ", arrow),
                            Style::default().fg(theme.accent),
                        ),
                        Span::styled("trace", Style::default().fg(theme.muted)),
                        Span::styled(summary_suffix, theme.text_style(TextRole::Caption)),
                    ]));

                    // 展开态:按 event_ids 顺序铺子块,miss 显示 [event 已过期] 优雅降级
                    // V29.12: 连续同名 ToolCall 合并展示 — 渲染层分组,不改数据结构
                    if effectively_expanded {
                        // ── 分组: 连续同名 ToolCall id 归入同一 run ──
                        let runs = group_consecutive_tool_runs(event_ids, trace_events);

                        for run in &runs {
                            let event_start = lines.len();
                            // 合并 run 的 focused 判断: 任一子 event 被 focus 则整组高亮
                            let is_focused = run.iter().any(|id| focused_event_id == Some(*id));

                            if run.len() == 1 {
                                // ── 单条: 原样渲染 ──
                                let id = &run[0];
                                let ev = trace_events.iter().find(|e| e.id == *id);
                                match ev {
                                    None => {
                                        lines.push(Line::from(vec![
                                            bar.clone(),
                                            Span::raw("     "),
                                            Span::styled(
                                                format!("[event #{} 已过期]", id),
                                                theme.text_style(TextRole::Hint),
                                            ),
                                        ]));
                                    }
                                    Some(ev) => {
                                        let fully_expanded = code_blocks_expanded || expanded_event_ids.contains(id);
                                        let max_lines_think = if fully_expanded { 0 } else { 30 };
                                        let max_lines_tool = if fully_expanded { 0 } else { 20 };
                                        render_single_trace_event(
                                            ev, &bar, theme, max_lines_think, max_lines_tool,
                                            &mut lines,
                                        );
                                    }
                                }
                            } else {
                                // ── 多条合并: ⚙ name ×N · 状态聚合 · 总耗时 ──
                                render_merged_tool_run(
                                    run, trace_events, &bar, theme,
                                    code_blocks_expanded, expanded_event_ids,
                                    &mut lines,
                                );
                            }

                            // V28.4: focused event 的所有子行回填 highlight bg
                            if is_focused {
                                let focus_bg = Style::default().bg(theme.surface);
                                for ln in &mut lines[event_start..] {
                                    ln.style = focus_bg;
                                }
                            }
                        }
                    }
                    bi += 1; // V28.3: Trace 在 part 计数空间(Block + Trace)递增
                }
            }
        }

        // V30 复制修复：本 msg 生成的行都加上 REVERSED 修饰作选中反馈。
        // 设计说明：
        //   - 仅选中状态为 Some 且 actual_idx 在 [s_msg, e_msg] 区间时生效
        //   - 字符级 visual highlight 作为后续优化项（TODO），需要 build_message_lines
        //     内部重构 markdown wrap 路径以跟踪每个 span 的原始 char offset
        //   - REVERSED 适应任何主题色，无需额外 Theme 字段
        if in_selected_msg(actual_idx) {
            for ln in &mut lines[msg_lines_start..] {
                for sp in &mut ln.spans {
                    sp.style = sp.style.add_modifier(Modifier::REVERSED);
                }
            }
        }

        // ── Breathing spacing（消息内容结束后空 1 行）──
        // Compact 模式不加空行，Comfortable 模式加呼吸间距
        if !compact {
            lines.push(Line::raw(""));
        }
    }

    // ── 流式输出：完整实时消息（thinking + tools + text + cursor）──
    // 参考 Go 版 fmtStreamMsg：显示完整的流式会话状态
    if stream_cursor > 0 && !messages.is_empty() {
        let bar = Span::styled("┃", Style::default().fg(theme.session));

        // Breathing space
        lines.push(Line::raw(""));

        // Header: ┃ 🤖 Abacus · now
        let badge = Span::styled(
            "🤖 Abacus",
            Style::default().fg(theme.session).add_modifier(Modifier::BOLD),
        );
        let ts = Span::styled(
            " · now",
            theme.text_style(TextRole::Caption),
        );
        lines.push(Line::from(vec![bar.clone(), Span::raw(" "), badge, ts]));

        // Thinking block（如果有累积的 thinking 文本）
        // 注意：streaming_thinking 通过 state 传入——这里用 messages 数组的最后元素判断
        // 实际在渲染时 stream_cursor > 0 意味着 state.is_streaming = true
        // thinking/tools/text 由外部传入或通过最后一条消息的 streaming_* 字段获取
        // 由于 build_message_lines 只接收 messages slice，streaming 状态需要额外处理
        // → 这部分在 render_messages_in_card 中通过直接访问 state 补充

        // 闪烁光标（500ms 周期）
        let cursor_visible = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_millis() / 500) % 2 == 0;
        if cursor_visible {
            lines.push(Line::from(vec![
                bar.clone(),
                Span::raw("   "),
                Span::styled("▌", Style::default().fg(theme.session)),
            ]));
        } else {
            lines.push(Line::from(vec![bar, Span::raw("   ")]));
        }
    }
    lines
}

// ════════════════════════════════════════════════════════════════
// 消息行数估算 / 屏幕坐标反查 (V29.10: 从 event/mod.rs 移入此模块)
// ════════════════════════════════════════════════════════════════
//
// ## 不变量 (B11)
// 这两个 fn 是 build_message_lines 真实渲染行数的"近似镜像"。维护契约:
//   - 任何对 build_message_lines 增删行的改动 → 同步更新 estimate_msg_rows
//   - 任何对屏幕布局(top_bar/input/status 高度)的改动 → 同步更新 screen_row_to_msg_idx 边界
//
// ## V29.12 精度收敛补充
//   - Block detail 不走 wrap (与 build_message_lines line 230-235 一致)——以前错走 wrap math
//     导致高估, 已修为按 BlockKind 分流:
//       Think/Checklist → detail.lines().count() (markdown reflow 漂移 ±2~5 行, 已文档化)
//       ToolCall        → JSON pretty-print 后的 ·lines().count() (包含 400/200 截断逻辑)
//   - Trace 展开态粗估 events × 5 (header 1 + detail 平均 4 行)——仅用于 hit-test 容差
//
// ## 已知漂移范围 (未修, 成本/收益不划算)
//   - markdown table 缩列后行数 (Block.Think 用 markdown 渲染)
//   - Stream 中 code fence 折叠 (CODE_BLOCK_MAX_LINES)
//   - Trace 展开子块内 thinking/tool detail 实际行数 (估算用平均 4 代替真值)
//   - 修复路径: 让 build_message_lines 写出 per-msg 行数 cache (与 trace_part_positions 同机制)
//     已记录为未来 V30+ 选项
//
// ## 引用关系
//   - estimate_msg_rows: scroll_to_message (event/mod.rs), screen_row_to_msg_idx, V29.11 锚定
//   - screen_row_to_msg_idx: 鼠标点击 hit-test (event/mod.rs handle_mouse 两处)
//
// ## 历史
//   - V28.1 引入两 fn 作为"timeline 点击 → 跳消息"基础设施
//   - V29.10 移入 components/mod.rs 与 build_message_lines 同模块, 强约束维护契约

/// 估算一条消息的渲染行数（与 build_message_lines 逻辑一致, 见上方不变量注释）
pub(crate) fn estimate_msg_rows(msg: &crate::tui::state::Message, content_width: usize) -> usize {
    use crate::tui::state::{MsgContent, BlockKind};
    let mut rows = 0usize;
    // 角色标 + 时间戳 = 1 行
    rows += 1;
    for part in &msg.parts {
        match part {
            MsgContent::Stream(text) => {
                // Stream 经 markdown wrap 后产出, 近似用 wrap math (build line 142-191)
                for line in text.lines() {
                    if line.is_empty() {
                        rows += 1;
                        continue;
                    }
                    let dw = crate::tui::util::display_width(line);
                    rows += if dw <= content_width { 1 } else { (dw + content_width - 1) / content_width };
                }
            }
            MsgContent::Block { collapsed, detail, kind, .. } => {
                rows += 1; // header line: arrow + icon + summary
                if !*collapsed {
                    // V29.12: build_message_lines (line 230-235) 对 Block detail 不做 wrap,
                    //   每个 render_block_detail 输出行 → 1 屏幕行. 按 kind 分流:
                    rows += match kind {
                        BlockKind::Think | BlockKind::Checklist => {
                            // markdown / Caption 路径: 以输入行近似 (markdown reflow 漂移 ±2~5)
                            detail.lines().count().max(1)
                        }
                        BlockKind::ToolCall => {
                            // 镜像 render_block_detail_with_limit ToolCall 路径:
                            //   1) JSON pretty-print (一行 JSON 会被展开多行)
                            //   2) 400 行上限 → 截到 200 + 1 行被截断提示
                            let pretty = serde_json::from_str::<serde_json::Value>(detail.trim())
                                .ok()
                                .and_then(|v| serde_json::to_string_pretty(&v).ok());
                            let text = pretty.as_deref().unwrap_or(detail);
                            let total = text.lines().count().max(1);
                            if total > 400 { 200 + 1 } else { total }
                        }
                    };
                }
            }
            // V29.12: Trace 展开态粗估 — events × 5 (header 1 行 + detail 平均 4 行)
            //   真实形态: thinking 子块 1 + min(N,30); tool 子块 1 + min(M,20) + (被截断?1:0)
            //   合并渲染后实际行数更少 (N 条同名 → 1 header + N 摘要 ≈ N+1), 估算偏高
            //   仅用于 hit-test 容差, 高估比低估安全 (不误中下方消息)
            MsgContent::Trace { collapsed, event_ids, .. } => {
                rows += 1; // summary 行
                if !*collapsed {
                    rows += event_ids.len().saturating_mul(5);
                }
            }
        }
    }
    rows + 1 // trailing blank separator
}

/// 将屏幕行号转换为消息索引（按实际渲染行数映射）
pub(crate) fn screen_row_to_msg_idx(
    row: u16,
    terminal_rows: u16,
    scroll: usize,
    messages: &[crate::tui::state::Message],
    chat_width: u16,
) -> Option<usize> {
    let msg_area_start = 1u16; // after top bar
    let msg_area_end = terminal_rows.saturating_sub(7); // before input + status
    if row < msg_area_start || row >= msg_area_end {
        return None;
    }
    let screen_row = (row - msg_area_start) as usize;

    let content_width = (chat_width as usize).saturating_sub(5).max(20);
    let mut acc = 0usize;
    for (idx, msg) in messages.iter().enumerate().skip(scroll) {
        let h = estimate_msg_rows(msg, content_width);
        if screen_row < acc + h {
            return Some(idx);
        }
        acc += h;
    }
    messages.len().checked_sub(1)
}

/// V30 复制修复：屏幕 (row, col) → (msg_idx, char_idx_in_flat_text)
///
/// ## char_idx 语义
/// `char_idx` 是 msg 平铺文本（Stream parts 拼接）中的字符偏移，
/// 与 extract_selection_text 拼接顺序一致。如果 msg 含 Block/Trace，正文阈限在 Stream 部分按
/// 字符精度定位；Block/Trace 区域入友返 Stream 总长度作鬥近似。
///
/// ## 返回 None 场景
/// - row 超出消息区
/// - 位置落在 msg 头部 role 标签 / 时间戳行（char_idx=0 还是能返回，仅完全 OOB 返 None）
///
/// ## 与原生渲染 wrap 近似度
/// build_message_lines 的真实 wrap 逻辑这里未 100% 镜像（markdown reflow / code fence
/// 折叠 / wrap 点选择 三项可能偏移 ±几个字符）。对于文本选中这里可接受——选中
/// 起点偏多几个字符是可视反馈 + 拖动微调的，与“点不中”不同性质。
///
/// ## 引用关系
/// - event/mod.rs handle_mouse Down/Drag 调用，写入 TextSelection.start/end_char_idx
/// - 读取端：extract_selection_text + build_message_lines 高亮渲染
pub(crate) fn screen_pos_to_msg_char(
    row: u16,
    col: u16,
    terminal_rows: u16,
    scroll: usize,
    messages: &[crate::tui::state::Message],
    chat_width: u16,
) -> Option<(usize, usize)> {
    use crate::tui::state::MsgContent;
    let msg_idx = screen_row_to_msg_idx(row, terminal_rows, scroll, messages, chat_width)?;
    let msg = messages.get(msg_idx)?;

    // 定位 msg 起始屏幕行
    let msg_area_start = 1u16;
    let screen_row = (row.saturating_sub(msg_area_start)) as usize;
    let content_width = (chat_width as usize).saturating_sub(5).max(20);
    let mut acc = 0usize;
    for (idx, m) in messages.iter().enumerate().skip(scroll) {
        if idx == msg_idx { break; }
        acc += estimate_msg_rows(m, content_width);
    }
    let row_in_msg = screen_row.saturating_sub(acc);

    // bar_indent (build_message_lines line 60: 4 spaces) + border padding
    // 列反查从 content 区域起点算起：渲染 = border(1) + padding(1) + bar(1) + indent(3) = 6
    let content_col_start: u16 = 6;
    let col_in_content = col.saturating_sub(content_col_start);

    // row 0 = role header (“user · 12:34” 那一行) → char_idx = 0
    if row_in_msg == 0 {
        return Some((msg_idx, 0));
    }
    let stream_row = row_in_msg.saturating_sub(1);

    // 仅在 Stream parts 上做 char-级定位；定位到超过 Stream 总长 → 返回 Stream 总长作鬥近似
    let stream_text: String = msg.parts.iter().filter_map(|p| match p {
        MsgContent::Stream(s) => Some(s.clone()),
        _ => None,
    }).collect::<Vec<_>>().join("");
    if stream_text.is_empty() {
        return Some((msg_idx, 0));
    }

    // 按 content_width 拆行（与 build_message_lines wrap math 近似），累加 char_idx
    let mut char_offset_at_line_start = 0usize;
    let mut current_visual_row = 0usize;
    for line in stream_text.split('\n') {
        let line_chars: Vec<char> = line.chars().collect();
        if line_chars.is_empty() {
            if current_visual_row == stream_row {
                return Some((msg_idx, char_offset_at_line_start));
            }
            current_visual_row += 1;
            char_offset_at_line_start += 1; // '\n'
            continue;
        }
        // 模拟 wrap：按 unicode width 累加到 content_width 装不下时换行
        let mut line_idx_in_content = 0usize;
        let mut col_acc: usize = 0;
        let mut start_char_idx_of_visual_line = char_offset_at_line_start;
        let mut idx_in_line = 0usize;
        while idx_in_line < line_chars.len() {
            let c = line_chars[idx_in_line];
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
            if col_acc + w > content_width && col_acc > 0 {
                // 到换行点：如果 stream_row 落在当前 visual line，定位后返回
                if current_visual_row == stream_row {
                    let click_char = locate_char_in_segment(
                        &line_chars[(idx_in_line - line_idx_in_content)..idx_in_line],
                        col_in_content as usize,
                    );
                    return Some((msg_idx, start_char_idx_of_visual_line + click_char));
                }
                current_visual_row += 1;
                start_char_idx_of_visual_line = char_offset_at_line_start + idx_in_line;
                line_idx_in_content = 0;
                col_acc = 0;
            }
            col_acc += w;
            idx_in_line += 1;
            line_idx_in_content += 1;
        }
        // 本逻辑行的最后一个 visual line
        if current_visual_row == stream_row {
            let segment_start = line_chars.len() - line_idx_in_content;
            let click_char = locate_char_in_segment(
                &line_chars[segment_start..],
                col_in_content as usize,
            );
            return Some((msg_idx, start_char_idx_of_visual_line + click_char));
        }
        current_visual_row += 1;
        char_offset_at_line_start += line.chars().count() + 1; // +1 for '\n'
    }

    // stream_row 超过 Stream 总高（点在 Block/Trace 区域） → 返回 Stream 总长作鬥近似边界
    Some((msg_idx, stream_text.chars().count()))
}

/// 在一个 visual line 的字符 segment 内按列偏移找字符下标（unicode width 加权）
fn locate_char_in_segment(seg: &[char], target_col: usize) -> usize {
    let mut col_acc: usize = 0;
    for (i, c) in seg.iter().enumerate() {
        let w = unicode_width::UnicodeWidthChar::width(*c).unwrap_or(1);
        if col_acc + w > target_col {
            return i;
        }
        col_acc += w;
    }
    seg.len()
}

// ════════════════════════════════════════════════════════════════
// Card — 圆角卡片容器 (含阴影)
// ════════════════════════════════════════════════════════════════

pub struct Card<'a> {
    block: Block<'a>,
    background: Color,
    shadow: bool,
    focused: bool,
    z: u8,
    theme: &'a Theme,
}

impl<'a> Card<'a> {
    pub fn new(theme: &'a Theme) -> Self {
        Self {
            block: Block::default().borders(Borders::ALL).border_type(BorderType::Rounded),
            background: theme.surface,
            shadow: true,
            focused: false,
            z: crate::tui::theme::z_index::CARD_BG,
            theme,
        }
    }

    pub fn title<T: Into<Line<'a>>>(mut self, title: T) -> Self {
        self.block = self.block.title(title);
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self.z = if focused {
            crate::tui::theme::z_index::STATE_HIGHLIGHT
        } else {
            crate::tui::theme::z_index::CARD_BORDER
        };
        self
    }

    pub fn background(mut self, color: Color) -> Self {
        self.background = color;
        self
    }

    pub fn no_shadow(mut self) -> Self {
        self.shadow = false;
        self
    }

    pub fn inner(&self, area: Rect) -> Rect {
        let ba = self.block.inner(area);
        if self.shadow {
            Rect::new(ba.x, ba.y, ba.width.saturating_sub(1), ba.height.saturating_sub(1))
        } else {
            ba
        }
    }
}

impl<'a> Widget for Card<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let bs = if self.focused {
            Style::default().fg(self.theme.primary)
        } else {
            Style::default().fg(self.theme.border)
        };
        self.block
            .border_style(bs)
            .style(Style::default().bg(self.background))
            .render(area, buf);

        if self.shadow && area.width > 3 && area.height > 3 {
            // 右阴影 — 1 列（安全边界检查：必须在 buffer area 内）
            for y in area.top() + 2..area.bottom() {
                let x = area.right();
                if buf.area.contains((x, y).into()) {
                    buf[(x, y)]
                        .set_symbol("░")
                        .set_fg(self.theme.border)
                        .set_bg(self.theme.bg);
                }
            }
            // 下阴影 — 1 行
            for x in area.left() + 2..area.right().saturating_add(1) {
                let y = area.bottom();
                if buf.area.contains((x, y).into()) {
                    buf[(x, y)]
                        .set_symbol("░")
                        .set_fg(self.theme.border)
                        .set_bg(self.theme.bg);
                }
            }
        }
    }
}

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

    // 动态高度：标题(1) + 操作(1) + 详情(N) + 空行(1) + 倒计时(1) + 按钮(1) + 边框(2)
    // B7：未展开时折叠到 3 行，展开时最多 8 行；总数超出则加一行 "+N more" 提示
    let max_visible = if dialog.details_expanded { 8 } else { 3 };
    let visible_count = dialog.details.len().min(max_visible);
    let has_more = dialog.details.len() > max_visible;
    let detail_lines = visible_count + if has_more { 1 } else { 0 };
    let mut popup_h = (6 + detail_lines) as u16;
    // K3c：popup 宽度基于整个 frame 而非 input_area，避免窄终端下越界
    let frame_size = f.area();
    let popup_w: u16 = std::cmp::max(40, std::cmp::min(64, frame_size.width.saturating_sub(4)));

    // K3a 位置自适应：优先上方 → 不足走下方 → 再不足居中并 cap 高度
    let above_space = input_area.y;
    let below_space = frame_size.height
        .saturating_sub(input_area.y.saturating_add(input_area.height));
    let popup_y = if above_space > popup_h {
        input_area.y - popup_h - 1
    } else if below_space > popup_h {
        input_area.y + input_area.height + 1
    } else {
        popup_h = std::cmp::min(popup_h, (frame_size.height * 3) / 4);
        frame_size.height.saturating_sub(popup_h) / 2
    };
    // V23b：与 picker_popup 一致——左对齐到 input_area.x
    //   用户偏好：confirm/picker 两个弹窗都靠左跟消息卡片左边界对齐，视觉一致
    let popup_x = input_area.x.min(frame_size.width.saturating_sub(popup_w));
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

    let tw = 34u16;
    let toast_h: u16 = 3; // 卡片高度
    let toast_gap: u16 = 1; // 卡片间距
    let x = 1u16; // 左侧弹出（原为 f.area().width.saturating_sub(tw + 2)）
    // B11：max_y 减去 toast_h 而非硬编码 3，确保最后一个 toast 不被截
    let max_y = f.area().height.saturating_sub(toast_h);
    let mut y = 2u16;

    let now = Instant::now();
    for toast in &state.toasts {
        if y > max_y {
            break;
        }
        // 渐隐效果: 最后 800ms 逐渐 dim
        let remaining = toast.expire_at.duration_since(now);
        let is_fading = remaining < std::time::Duration::from_millis(800);
        let dim_modifier = if is_fading { Modifier::DIM } else { Modifier::empty() };

        let area = Rect::new(x, y, tw, 3);
        let border_color = if is_fading { state.theme.muted } else { state.theme.border };
        let card = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color).add_modifier(dim_modifier))
            .bg(state.theme.surface);
        card.render(area, f.buffer_mut());

        // subtle top accent line (安全边界检查)
        let buf_area = f.area();
        if !is_fading {
            for cx in x + 1..x + tw - 1 {
                if buf_area.contains((cx, y).into()) {
                    f.buffer_mut()[(cx, y)]
                        .set_symbol("▄")
                        .set_fg(state.theme.accent)
                        .set_bg(state.theme.surface);
                }
            }
        }

        let inner = Rect::new(x + 2, y + 1, tw - 4, 1);
        // B10：截断时显式加 "…" 让用户感知信息被裁
        // 留位：边距 4 + "◈ " 2 + 截断尾 "…" 1 = 7（safe margin）
        let max_msg_chars = (tw as usize).saturating_sub(7);
        let total_chars = toast.message.chars().count();
        let display_msg: String = if total_chars > max_msg_chars {
            let mut s: String = toast.message.chars().take(max_msg_chars).collect();
            s.push('…');
            s
        } else {
            toast.message.clone()
        };
        let text_color = if is_fading { state.theme.muted } else { state.theme.text };
        let line = Line::from(vec![
            Span::styled("◈ ", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD | dim_modifier)),
            Span::styled(display_msg, Style::default().fg(text_color).add_modifier(dim_modifier)),
        ]);
        f.render_widget(Paragraph::new(line), inner);
        // B11：间距 = toast_h + gap，与 max_y 计算保持一致
        y += toast_h + toast_gap;
    }
}

// ════════════════════════════════════════════════════════════════
// TopBar — 顶部栏 (Logo + Session + Model + 模式标识)
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
    } else if matches!(state.input_state, crate::tui::state::InputState::Completing) {
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
    use crate::tui::util::display_width;
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
        crate::tui::state::InputState::Thinking | crate::tui::state::InputState::Executing | crate::tui::state::InputState::Outputting => {
            input_lines.push(Line::from(vec![
                Span::styled(format!("⟳ {} · Esc 暂停", state.model_name), muted),
            ]));
        }
        crate::tui::state::InputState::Paused => {
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
        crate::tui::state::InputState::Thinking => (format!("{} Thinking{}{}{}", spinner(), phase_sep, phase, elapsed), state.theme.accent),
        crate::tui::state::InputState::Executing => (format!("{} Working{}{}{}", spinner(), phase_sep, phase, elapsed), state.theme.gold),
        crate::tui::state::InputState::Outputting => (format!("{} Outputting{}", spinner(), elapsed), state.theme.success),
        crate::tui::state::InputState::Paused => ("⏸ Paused".into(), state.theme.semantic_fg(SemanticIntent::Warning)),
        _ if state.engine_handle.is_some() => ("● Ready".into(), state.theme.success),
        _ => ("● Ready".into(), state.theme.muted),
    };
    // 右侧提示：繁忙时显示 Esc 取消，否则 ⏎ Enter
    let is_busy = matches!(state.input_state,
        crate::tui::state::InputState::Thinking |
        crate::tui::state::InputState::Executing |
        crate::tui::state::InputState::Outputting);
    let right_hint_text = if is_busy { "Esc 取消" } else { "⏎ Enter" };
    let right_style = if is_busy {
        state.theme.text_style(TextRole::Caption)
    } else {
        Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)
    };
    // 走 tui::util::display_width 统一治理（CJK 文本如"Esc 取消"按显示列宽算 fill）
    use crate::tui::util::display_width;
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
fn format_ctx(n: usize) -> String {
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

// ════════════════════════════════════════════════════════════════
// MessageList — 消息流 (Stream + Block 混排渲染)
// ════════════════════════════════════════════════════════════════

pub fn render_messages(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let inner = Rect {
        x: area.x + 2,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };

    if !state.rendered_lines_dirty.get()
        && *state.cached_width.borrow() == inner.width
        && state.stream_cursor == 0
        && !state.is_streaming
    {
        let cached = state.cached_lines.borrow();
        f.render_widget(List::new(cached.clone()).direction(ListDirection::TopToBottom), inner);
        return;
    }

    // V28.3: 此路径(render_messages) 被简化版调用,trace_part_positions 暂不使用(未做 scroll 转换)
    let mut _trace_pos_unused: Vec<(usize, usize, usize)> = Vec::new();
    let mut lines = build_message_lines(
        &state.messages, 0, &state.theme, &state.text_selection, inner.width,
        state.stream_cursor, state.compact, state.code_blocks_expanded,
        &state.trace_events, &mut _trace_pos_unused, state.focused_event_id,
    );

    // Auto-scroll to bottom
    let visible_h = inner.height as usize;
    if lines.len() > visible_h {
        lines = lines.split_off(lines.len() - visible_h);
    }

    *state.cached_lines.borrow_mut() = lines.clone();
    *state.cached_width.borrow_mut() = inner.width;
    state.rendered_lines_dirty.set(false);
    f.render_widget(List::new(lines).direction(ListDirection::TopToBottom), inner);
}

/// 消息列表（色条风格，无边框，带左缩进，自动滚动到底部）
///
/// 渲染策略（和 Go 版一致）：
///   1. 渲染所有消息为 lines（不 skip）
///   2. 如果 lines 超出可见高度，取最后 N 行（auto-scroll to bottom）
///   3. 传给 List widget 显示
///
/// 引用关系：被 modes/chat.rs、modes/team.rs、modes/meeting.rs 调用
/// 设计：色条 ┃ 替代 Card 边框，消息自带角色标识
/// V28.5: streaming 期间在消息框顶部边框上绘制单向循环渐变光带
///
/// 设计:
/// - 8 cell 宽光带, 强度梯度 [DIM, DIM, normal, BOLD, BOLD, normal, DIM, DIM] 模拟"软"光晕
/// - 单向循环: 光带从左侧进入(预滑入区 -bar_len ~ 0), 滑过整个边框宽度, 完全滑出右端后跳回
/// - tick 由调用方每帧 += 1 推进, 1 cell/frame × 20 FPS ≈ 80-col 4 秒一周期
/// - 仅 patch frame buffer 顶部行 cell 的 style.fg + Modifier, 不改字符(保留 ─/╭/╮)
///   避免破坏 Block::Rounded 视觉契约
///
/// 引用关系:
/// - 调用方: `render_messages_in_card` (主消息框) + 后续可能扩展到其它 streaming-aware 卡片
/// - 数据源: `AppState.anim_tick` (Cell<u64>, 内部可变)
/// - 不持有任何状态, 纯函数
fn paint_streaming_top_shimmer(buf: &mut Buffer, area: Rect, state: &AppState) {
    // 至少需要 4 cells (左角 ╭ + 至少 2 cell 横线 + 右角 ╮) 才有意义
    if area.width < 4 || area.height < 1 {
        return;
    }
    // 跳过左右两个圆角字符, 只刷中间的横线段
    let inner_x = area.x + 1;
    let inner_w = area.width.saturating_sub(2);
    if inner_w == 0 {
        return;
    }

    const BAR_LEN: u16 = 8;
    // 强度梯度: 1=DIM, 2=normal, 3=BOLD; 两侧弱中间强, 模拟软光晕
    const INTENSITIES: [u8; BAR_LEN as usize] = [1, 1, 2, 3, 3, 2, 1, 1];

    // 让光带完整滑出右端后才跳回, span = inner_w + bar_len
    // tick 每帧 +1, 推进 anim_tick 在调用入口处完成
    let span = (inner_w + BAR_LEN) as u64;
    // V28.6 (PR12-4): 周期恒定 ~3.5 秒,不论终端宽度
    //   tick × 50ms (每帧) = 经过的毫秒数
    //   mod PERIOD_MS 得到 [0..PERIOD_MS) 区间
    //   按比例映射到 [0..span) 即可,窄屏每帧推进 <1 cell, 宽屏每帧推进 >1 cell
    //   不再受 "tick 每帧 +1 = cell 每帧 +1" 约束 — 解耦帧率与速率
    const PERIOD_MS: u64 = 3500;
    const FRAME_MS: u64 = 50;
    let now_ms = state.anim_tick.get().saturating_mul(FRAME_MS);
    let progress = (now_ms % PERIOD_MS) as f64 / PERIOD_MS as f64; // 0.0..1.0
    let phase = (progress * span as f64) as i32 - BAR_LEN as i32;
    // phase 范围: [-BAR_LEN .. inner_w-1]
    //   负值表示光带"还没完全进入可见区"; 正值表示光带头部已露出

    let top_y = area.y;
    let primary = state.theme.primary;

    for i in 0..BAR_LEN as usize {
        let x = phase + i as i32;
        // 越界则跳过(光带边缘溢出可见区时只画进入了的部分)
        if x < 0 || (x as u16) >= inner_w {
            continue;
        }
        let cell_x = inner_x + x as u16;
        let cell = &mut buf[(cell_x, top_y)];
        let mut style = Style::default().fg(primary);
        match INTENSITIES[i] {
            1 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::BOLD),
            _ => {}
        }
        // V28.6 (PR12-4 续): 光带覆盖处字符 ─ → ━ (heavy horizontal U+2501),
        //   与看板内 ┃ 色条 + panel/CommandHint 焦点上边框 ━ 同属 box drawings heavy 家族。
        //   视觉契约: 滑过瞬间该 cell 变粗 + 着色, 下一帧 msg_block.render 重画 ─ 灰色,
        //   形成"光带扫过时加亮加粗"的物理感, 而不是只变色不变形。
        cell.set_symbol("━");
        cell.set_style(style);
    }
}

pub fn render_messages_in_card(
    f: &mut ratatui::Frame,
    state: &AppState,
    area: Rect,
    _focus: Focus,
) {
    // 消息区圆角边框 + 背景填充
    let msg_block = Block::default()
        .border_type(BorderType::Rounded)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(state.theme.border))
        .style(Style::default().bg(state.theme.bg));
    let inner = msg_block.inner(area);
    msg_block.render(area, f.buffer_mut());

    // V28.5: streaming 期间顶部边框光带动效 — 必须在 msg_block.render 之后,
    // 否则会被 Block 重绘覆盖。tick 推进也在这里(只 streaming 时才动, 节能)
    // 注: 消息卡按 V25/V26 设计不参与 Focus 循环(Focus 只在 Panel ↔ CommandHint 切),
    //     因此不画 focus 上边框反馈;焦点反馈仅在 panel/CommandHint 卡片上呈现(对称律仅适用于"参与焦点的卡片")
    if state.is_streaming {
        state.anim_tick.set(state.anim_tick.get().wrapping_add(1));
        paint_streaming_top_shimmer(f.buffer_mut(), area, state);
    }

    // 渲染缓存复用（streaming 或 dirty 时禁用缓存，每帧重新渲染）
    // is_streaming: 流式文本持续增长，需要每帧刷新
    // stream_cursor > 0: 光标闪烁动画
    // rendered_lines_dirty: 新消息到达
    if !state.rendered_lines_dirty.get()
        && *state.cached_width.borrow() == inner.width
        && state.stream_cursor == 0
        && !state.is_streaming
    {
        let cached = state.cached_lines.borrow();
        f.render_widget(List::new(cached.clone()).direction(ListDirection::TopToBottom), inner);
        return;
    }

    // 渲染所有消息（scroll=0 表示从头开始，不跳过任何消息）
    // V28.3: 收集 Trace/Block summary 行的 (line_idx, msg_idx, part_idx),
    // 后面 scroll 转换为绝对屏幕 y 写入 state.message_trace_row_map
    let mut trace_part_positions: Vec<(usize, usize, usize)> = Vec::new();
    let mut lines = build_message_lines(
        &state.messages, 0, &state.theme, &state.text_selection, inner.width,
        state.stream_cursor, state.compact, state.code_blocks_expanded,
        &state.trace_events, &mut trace_part_positions, state.focused_event_id,
    );

    // ── 流式消息：追加 streaming 状态（thinking + tools + text）──
    // build_message_lines 只渲染 header + cursor，这里补充完整的流式内容
    if state.is_streaming {
        let bar = Span::styled("┃", Style::default().fg(state.theme.session));

        // V14 修复：build_message_lines 仅在 stream_cursor>0 时追加 🤖 Abacus ghost header；
        //          在 stream_cursor==0（流式刚启动、TextDelta 尚未到达）时本函数必须自己补 header，
        //          否则 thinking/tools 直接挂在 user 消息下方，视觉上像 user 在 thinking。
        if state.stream_cursor == 0 {
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                bar.clone(),
                Span::raw(" "),
                Span::styled(
                    "🤖 Abacus",
                    Style::default().fg(state.theme.session).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" · now", state.theme.text_style(TextRole::Caption)),
            ]));
            // 占位光标行（thinking/tools/text 通过 saturating_sub(1) 插到此行之前）
            lines.push(Line::from(vec![bar.clone(), Span::raw("   ")]));
        }

        // V38: Streaming trace + text 顺序渲染（修复同时弹出问题）
        // 渲染顺序严格遵循生成时序：① thinking → ② tools → ③ 分隔线 → ④ response text
        // 使用统一的累积 insert_offset 确保顺序正确，不再各自独立计算 insert_pos
        let stream_insert_base = lines.len().saturating_sub(1);
        let mut stream_offset: usize = 0;
        let content_w = inner.width.saturating_sub(5) as usize;

        // ── Phase 1: Thinking（仅 show_streaming_trace=true 时展示）──
        if state.show_streaming_trace && !state.streaming_thinking.is_empty() {
            let think_style = Style::default().fg(state.theme.muted).add_modifier(Modifier::ITALIC);
            let think_all_lines: Vec<&str> = state.streaming_thinking.lines().collect();
            let total = think_all_lines.len();
            let max_show = 20;
            let start = total.saturating_sub(max_show);
            let visible = &think_all_lines[start..];

            // Header
            lines.insert(stream_insert_base + stream_offset, Line::from(vec![
                bar.clone(),
                Span::raw("   "),
                Span::styled(format!("💭 Thinking · {}行", total), state.theme.text_style(TextRole::Caption)),
            ]));
            stream_offset += 1;

            for tline in visible {
                let truncated: String = tline.chars().take(content_w).collect();
                lines.insert(stream_insert_base + stream_offset, Line::from(vec![
                    bar.clone(),
                    Span::raw("   "),
                    Span::styled(if truncated.is_empty() { " ".to_string() } else { truncated }, think_style),
                ]));
                stream_offset += 1;
            }
            if total > max_show {
                lines.insert(stream_insert_base + stream_offset, Line::from(vec![
                    bar.clone(),
                    Span::raw("   "),
                    Span::styled(format!("↳ +{} 行", total - max_show), state.theme.text_style(TextRole::Caption)),
                ]));
                stream_offset += 1;
            }
        }

        // ── Phase 2: Tools（仅 show_streaming_trace=true 时展示完整内容）──
        if state.show_streaming_trace && !state.streaming_tools.is_empty() {
            use crate::tui::state::StreamingToolStatus;
            for (name, status, duration_ms, trace_id) in state.streaming_tools.iter().rev().take(10) {
                let (icon, color) = match status {
                    StreamingToolStatus::Running => ("⟳", state.theme.gold),
                    StreamingToolStatus::Success => ("✓", state.theme.success),
                    StreamingToolStatus::Failed => ("✗", state.theme.error),
                };
                let dur = duration_ms.map(|ms| format!(" {}ms", ms)).unwrap_or_default();
                lines.insert(stream_insert_base + stream_offset, Line::from(vec![
                    bar.clone(),
                    Span::raw("   "),
                    Span::styled(format!("⚙ {}{} {}", name, dur, icon), Style::default().fg(color)),
                ]));
                stream_offset += 1;
                // 参数和输出
                if let Some(ev) = state.trace_events.iter().find(|e| e.id == *trace_id) {
                    if let crate::tui::state::TraceKind::ToolCall { args, output, .. } = &ev.kind {
                        if !args.is_empty() {
                            let args_preview: String = args.chars().take(content_w).collect();
                            lines.insert(stream_insert_base + stream_offset, Line::from(vec![
                                bar.clone(),
                                Span::raw("   "),
                                Span::styled(args_preview, state.theme.text_style(TextRole::Caption)),
                            ]));
                            stream_offset += 1;
                        }
                        if let Some(out) = output {
                            let out_preview: String = out.chars().take(content_w).collect();
                            lines.insert(stream_insert_base + stream_offset, Line::from(vec![
                                bar.clone(),
                                Span::raw("   "),
                                Span::styled(out_preview, Style::default().fg(state.theme.success)),
                            ]));
                            stream_offset += 1;
                        }
                    }
                }
            }
        }

        // ── Phase 3: 分隔线（trace 有内容且 text 也有内容时插入）──
        if stream_offset > 0 && !state.streaming_text.is_empty() {
            lines.insert(stream_insert_base + stream_offset, Line::from(vec![
                bar.clone(),
                Span::raw("   "),
                Span::styled("╌╌╌╌╌╌╌╌", Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM)),
            ]));
            stream_offset += 1;
        }

        // [V36 折叠] 以下 thinking/tools 详细渲染仅在 show_stream_details=true 时启用
        // 当前默认折叠（上方单行摘要已足够），保留代码供 /show-details 命令复活
        if false && !state.streaming_thinking.is_empty() {
            let think_all_lines: Vec<&str> = state.streaming_thinking.lines().collect();
            let total = think_all_lines.len();
            let max_show = 20;
            let start = total.saturating_sub(max_show);
            let visible = &think_all_lines[start..];
            let content_w = inner.width.saturating_sub(7) as usize; // bar(1) + spacing(3) + margin

            // V14 修复：原 -2 把内容插到 Abacus header 之前，导致 thinking/tools 视觉上挂在 user 下
            //          改为 -1：插在闪烁光标(末尾)之前、🤖 Abacus header(倒2)之后
            let insert_pos = lines.len().saturating_sub(1);
            // Header
            lines.insert(insert_pos, Line::from(vec![
                bar.clone(),
                Span::raw("   "),
                Span::styled("💭 ", Style::default().fg(state.theme.accent)),
                Span::styled(
                    format!("Thinking · {}行", total),
                    state.theme.text_style(TextRole::Caption),
                ),
            ]));
            // Content lines —— V11: 真 word-wrap 而非 `…` 截断
            //   按 unicode display width 切片，超长行折行到下一行（保持思考完整可见）
            //   空格优先断行；找不到空格时按字符宽度硬切（中文场景）
            let think_style = Style::default().fg(state.theme.muted).add_modifier(Modifier::ITALIC);
            let mut row_offset = 0usize;
            for tline in visible.iter() {
                let mut remaining = *tline;
                if remaining.is_empty() {
                    // 空行也保留视觉留白
                    lines.insert(insert_pos + 1 + row_offset, Line::from(vec![bar.clone()]));
                    row_offset += 1;
                    continue;
                }
                while !remaining.is_empty() {
                    // 取至 content_w 宽（display width，处理中文双宽）
                    let mut width = 0usize;
                    let mut take = 0usize;
                    let mut last_break = 0usize;
                    for (i, ch) in remaining.char_indices() {
                        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                        if width + cw > content_w { break; }
                        width += cw;
                        take = i + ch.len_utf8();
                        if ch == ' ' || cw > 1 {
                            last_break = take;
                        }
                    }
                    // 没切到任何字符 → 至少切一字符避免死循环
                    if take == 0 {
                        take = remaining.chars().next().map(|c| c.len_utf8()).unwrap_or(remaining.len());
                    }
                    // 优先在空格/中文边界折行（last_break > 50% 才用）
                    if take < remaining.len() && last_break > 0 && last_break > take / 2 {
                        take = last_break;
                    }
                    let chunk = &remaining[..take];
                    lines.insert(insert_pos + 1 + row_offset, Line::from(vec![
                        bar.clone(),
                        Span::raw("   "),
                        Span::styled(chunk.to_string(), think_style),
                    ]));
                    row_offset += 1;
                    remaining = remaining[take..].trim_start();
                }
            }
            // V29.12: 超出 max_show 行时显示隐藏行数提示（与落档后 Trace 折叠提示对称）
            let hidden_lines = total.saturating_sub(max_show);
            if hidden_lines > 0 {
                let hint_pos = lines.len().saturating_sub(1);
                lines.insert(hint_pos, Line::from(vec![
                    bar.clone(),
                    Span::raw("   "),
                    Span::styled(
                        format!("↳ +{} 行（落档后可完整展开）", hidden_lines),
                        state.theme.text_style(TextRole::Caption),
                    ),
                ]));
            }
            // V28 (T6): thinking 段结束 → 如果后面还有 tools 或 text, 追加 ╌╌╌╌ 细分隔
            // (PANEL-DESIGN-SPEC §7 字符 U+254C, 与 panel 子分块分隔保持一致)
            let has_more = !state.streaming_tools.is_empty() || !state.streaming_text.is_empty();
            if has_more {
                let sep_pos = lines.len().saturating_sub(1);
                lines.insert(sep_pos, Line::from(vec![
                    bar.clone(),
                    Span::raw("   "),
                    Span::styled(
                        "╌╌╌╌╌╌╌╌",
                        Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM),
                    ),
                ]));
            }
        }

        // Tools（最多显示 10 个）—— V11：区分成功/失败/进行中三态 + 显示耗时
        // [V36 折叠] 详细 tools 列表渲染仅在 show_stream_details=true 时启用
        if false && !state.streaming_tools.is_empty() {
            use crate::tui::state::StreamingToolStatus;
            // V14 修复：原 -2 把内容插到 Abacus header 之前，导致 thinking/tools 视觉上挂在 user 下
            //          改为 -1：插在闪烁光标(末尾)之前、🤖 Abacus header(倒2)之后
            let insert_pos = lines.len().saturating_sub(1);
            let tools_to_show = state.streaming_tools.iter().rev().take(10);
            // V28: 4 元组末位 trace_id 不参与渲染,用 _ 析构
            // V29.12: 视觉风格与落档后 Trace ToolCall 子块对齐 —
            //   流式: ⚙ name · ✓/✗/⟳ · dur (同级 L1 缩进, 与 text 平行)
            //   落档: ⚙ name · ✓/✗/⟳ · dur (Trace 子块 L2 缩进)
            //   图标/色彩/排列一致,仅缩进层级因所属容器不同而异
            for (name, status, duration_ms, _trace_id) in tools_to_show {
                let (status_icon, status_color) = match status {
                    StreamingToolStatus::Success => ("✓", state.theme.success),
                    StreamingToolStatus::Failed  => ("✗", state.theme.error),
                    StreamingToolStatus::Running => ("⟳", state.theme.gold),
                };
                let mut spans: Vec<Span<'static>> = vec![
                    bar.clone(),
                    Span::raw("   "),
                    Span::styled("⚙ ".to_string(), Style::default().fg(state.theme.gold)),
                    Span::styled(name.to_string(), Style::default().fg(state.theme.gold).add_modifier(Modifier::BOLD)),
                    Span::raw(" · "),
                    Span::styled(status_icon.to_string(), Style::default().fg(status_color)),
                ];
                match (status, duration_ms) {
                    (StreamingToolStatus::Running, _) => {
                        spans.push(Span::styled(" …".to_string(), state.theme.text_style(TextRole::Caption)));
                    }
                    (_, Some(ms)) => {
                        let dur = format_duration_ms_padded(*ms);
                        spans.push(Span::styled(dur, state.theme.text_style(TextRole::Caption)));
                    }
                    _ => {}
                }
                lines.insert(insert_pos, Line::from(spans));
            }
            // 如果有更多未显示的工具
            let hidden = state.streaming_tools.len().saturating_sub(10);
            if hidden > 0 {
                lines.insert(insert_pos, Line::from(vec![
                    bar.clone(),
                    Span::raw("   "),
                    Span::styled(format!("(+{} more tools)", hidden), state.theme.text_style(TextRole::Caption)),
                ]));
            }
            // V28 (T6): tools 段结束 → 如果后面还有 streaming_text, 追加 ╌╌╌╌ 细分隔
            if !state.streaming_text.is_empty() {
                let sep_pos = lines.len().saturating_sub(1);
                lines.insert(sep_pos, Line::from(vec![
                    bar.clone(),
                    Span::raw("   "),
                    Span::styled(
                        "╌╌╌╌╌╌╌╌",
                        Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM),
                    ),
                ]));
            }
        }

        // Streaming text — 整段 markdown 渲染（每帧重解析）
        //
        // ST5 修复：之前按 \n boundary 增量解析 → markdown 多行结构（代码块/列表/
        // 引用块）跨 chunk delta 时丢失语义：
        //   - chunk 1: "```rust\nfn main() {\n"
        //   - 第一帧解析 → pulldown-cmark 在 EOF 虚拟闭合 fence，已 push 到缓存
        //   - chunk 2: "  println!();\n}\n```\n"
        //   - 第二帧独立解析这段 → 4 空格被识别为 indented code block + 空 fence
        //   - 最终渲染：rust fence 提前关闭 + 后续内容散为普通文本，完全错乱
        //
        // 修复策略：每帧整段重解析 streaming_text。pulldown-cmark 高性能解析器，
        // 100KB 流式文本解析 < 1ms（每帧 50ms 预算的 2%），完全可承受。
        // 增量缓存（streaming_parsed_lines/len）字段保留为 reset_streaming 兼容，
        // 但本路径不再使用——下一次重构可清理。
        // ── Phase 4: Response text（统一使用 stream_offset 保持顺序）──
        if !state.streaming_text.is_empty() {
            // 统一缩进：所有内容（文本/代码/表格）使用 bar + "   "（3空格）
            // 代码块继续行额外 1 空格对齐（视觉上与 fence 标记缩进一致）
            let styled_lines = markdown::render_markdown_bounded(&state.streaming_text, &state.theme, false, content_w);
            for styled in &styled_lines {
                let rline = markdown::styled_line_to_ratatui(styled, &bar, &state.theme);
                // 表格行豁免 word-wrap (box-drawing 字符不可拆)
                if styled.line_type == LineType::Table {
                    lines.insert(stream_insert_base + stream_offset, rline);
                    stream_offset += 1;
                    continue;
                }
                let line_w: usize = rline.spans.iter()
                    .map(|s| crate::tui::util::display_width(s.content.as_ref()))
                    .sum();
                if line_w <= content_w + 4 {
                    lines.insert(stream_insert_base + stream_offset, rline);
                    stream_offset += 1;
                } else {
                    // 超宽行 word-wrap — 统一缩进 "   "（3空格，与所有内容对齐）
                    let indent_str = "   ";
                    let full_text: String = styled.spans.iter().map(|s| s.text.as_str()).collect();
                    let text_style = styled.spans.first().map(|s| s.style)
                        .unwrap_or(Style::default().fg(state.theme.text));
                    let mut remaining = full_text.as_str();
                    while !remaining.is_empty() {
                        let mut width = 0;
                        let mut take = 0;
                        let mut last_b = 0;
                        for (i, ch) in remaining.char_indices() {
                            let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                            if width + ch_w > content_w { break; }
                            width += ch_w;
                            take = i + ch.len_utf8();
                            if ch == ' ' || ch == '-' || ch == '/'
                                || unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) > 1 {
                                last_b = take;
                            }
                        }
                        if take < remaining.len() && last_b > 0 && last_b > take / 2 { take = last_b; }
                        if take == 0 { take = remaining.chars().next().map(|c| c.len_utf8()).unwrap_or(remaining.len()); }
                        lines.insert(stream_insert_base + stream_offset, Line::from(vec![
                            bar.clone(), Span::raw(indent_str.to_string()),
                            Span::styled(remaining[..take].to_string(), text_style),
                        ]));
                        stream_offset += 1;
                        remaining = &remaining[take..];
                    }
                }
            }
        }
    }

    // 视窗滚动：scroll=0 表示自动跟随底部，>0 表示向上偏移 N 行
    // V29.5 (B2/B12): streaming 期间不再强制 scroll=0 —— 用户主动向上看历史时,
    //   下面的 streaming 不应把视图甩到底; 只有 scroll==0 时才 auto-follow
    //   设计意图: scroll>0 视为"用户已离开底部进入浏览", 直到用户按 End 或 Home 显式回底
    let visible_h = inner.height as usize;
    let scroll_offset = state.scroll;
    // V29.5: 切片前抓总行数, 让 last_total_lines 反映真实总量(切片后 lines.len()==visible_h)
    let total_before_slice = lines.len();
    let (visible_start, visible_end) = if total_before_slice > visible_h {
        // V29.5 (B1): scroll 上限 clamp —— 越界时夹到顶部, 显示 [0..visible_h]
        let max_scroll = total_before_slice.saturating_sub(visible_h);
        let clamped = scroll_offset.min(max_scroll);
        let end = total_before_slice.saturating_sub(clamped);
        let start = end.saturating_sub(visible_h);
        lines = lines[start..end].to_vec();
        (start, end)
    } else {
        (0, total_before_slice)
    };
    // V29.5: 缓存最近一帧的尺寸, 让 handle_chat_scroll_key 做 clamp / PageUp 半屏
    state.last_visible_h.set(visible_h);
    state.last_total_lines.set(total_before_slice);
    // V29.11 (B4): 同时缓存内容宽度, 让 Space 折叠锚定能估算最后一条 msg 行数变化
    //   bar(1) + indent(3) + margin(1) = 5, 与 build_message_lines 内部 content_width 计算一致
    state.last_content_width.set((inner.width as usize).saturating_sub(5));

    // V28.3: 把 trace_part_positions(line_idx → msg_idx + part_idx)转换成
    // (绝对屏幕 y → msg_idx + part_idx) 写入 state.message_trace_row_map
    // 仅可见范围内的位置参与映射(不可见的不能被点击)
    let mut row_map = state.message_trace_row_map.borrow_mut();
    row_map.clear();
    for (line_idx, msg_idx, part_idx) in &trace_part_positions {
        if *line_idx >= visible_start && *line_idx < visible_end {
            let abs_y = inner.y.saturating_add((*line_idx - visible_start) as u16);
            row_map.push((abs_y, *msg_idx, *part_idx));
        }
    }
    drop(row_map);

    // ── Line Flash 效果：新输出的行短暂高亮背景 ──
    // E1 修复：K6 重构注释承诺"按内容 hash 精确匹配，不再按底部偏移漂移"，
    // 但渲染端原先仍用 `total - flash_count - 2 .. total - 2` 位置偏移；
    // 现按行内容 hash 精确匹配 flash_state.is_flashing(hash)，与 K6 设计一致
    if state.is_streaming && state.flash_state.active_flash_count() > 0 {
        for line in lines.iter_mut() {
            // 拼接行内所有 span 的可见文本作为 hash 输入（与 mark_new_lines 输入一致）
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let h = effects::FlashState::hash_line(&content);
            if state.flash_state.is_flashing(h) {
                let taken = std::mem::take(line);
                *line = effects::apply_flash_style(taken, state.theme.surface);
            }
        }
    }

    *state.cached_lines.borrow_mut() = lines.clone();
    *state.cached_width.borrow_mut() = inner.width;
    state.rendered_lines_dirty.set(false);
    f.render_widget(List::new(lines).direction(ListDirection::TopToBottom), inner);
}

/// Streaming 光标闪烁效果（附加到消息列表末尾，使用色条风格）
///
/// 引用关系：被流式输出逻辑调用（当 build_message_lines 未处理时的 fallback）
/// 生命周期：is_streaming=true 时激活，流式结束后停止
pub fn render_streaming_cursor(lines: &mut Vec<Line<'_>>, state: &AppState) {
    if !state.is_streaming { return; }
    let bar = Span::styled("┃", Style::default().fg(state.theme.session));
    // 闪烁光标（500ms 周期）
    let cursor_visible = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis() / 500) % 2 == 0;
    if cursor_visible {
        lines.push(Line::from(vec![
            bar,
            Span::raw("   "),
            Span::styled("▌", Style::default().fg(state.theme.session)),
        ]));
    } else {
        lines.push(Line::from(vec![
            bar,
            Span::raw("   "),
        ]));
    }
}

// ════════════════════════════════════════════════════════════════
// Panel — 右侧看板 (Tab 切换 + 内容区)
// ════════════════════════════════════════════════════════════════

/// V23: 给 area 加左侧 ┃ 色条, 返回剩余的内容区 Rect
/// 设计意图:
///   - 色条作为视觉锚点贯通整个内容高度 (panel "卡片型" 风格)
///   - 内容渲染代码不需要关心色条 — 关注点分离
///   - 三模式所有 PanelTab 内容区(overview/team_board/meeting_agenda/custom)统一调用
/// 引用关系: 被 render_panel 的 Clarify/Plan/Team/Meeting 四分支调用
/// 生命周期: 每帧渲染; 不持有状态
fn render_card_bar(f: &mut ratatui::Frame, theme: &crate::tui::theme::Theme, area: Rect) -> Rect {
    let split_h = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Horizontal)
        .constraints([
            ratatui::layout::Constraint::Length(1),  // ┃ 色条列
            ratatui::layout::Constraint::Min(1),     // 内容列
        ])
        .split(area);

    let bar_style = Style::default().fg(theme.primary);
    let bar_lines: Vec<Line> = (0..area.height)
        .map(|_| Line::styled("┃", bar_style))
        .collect();
    f.render_widget(Paragraph::new(bar_lines), split_h[0]);

    split_h[1]
}

/// V32 · 看板 tab 标签计数 indicator
///
/// 把 label 加 "·N" 后缀让用户一眼看到该 tab 有多少内容（"摘要·12 │ 任务·3"）
/// 0 计数省略后缀避免噪声。
///
/// ## 引用关系
/// - 调用方：render_panel Clarify/Team/Meeting/Plan 分支构造 tab_labels 时使用
/// - 数据源：caller 传入对应 count（trace_events.len / tasks.len / experts.len）
fn label_with_count(base: &str, count: usize) -> String {
    if count == 0 {
        base.to_string()
    } else {
        format!("{}·{}", base, count)
    }
}

/// V16: 构建 Tab 标签 spans（Team / Meeting 共用）
/// 样式: active = "▸ {名}" accent BOLD | inactive = " {名}" muted | sep = " │ " border DIM
/// 引用关系: 被 render_panel 的 Team/Meeting 分支调用
/// 生命周期: 每帧渲染时按 panel_tab 状态构造
fn build_tab_spans<'a>(labels: &'a [String], active: usize, theme: &crate::tui::theme::Theme) -> Vec<Span<'a>> {
    let mut spans: Vec<Span<'a>> = Vec::with_capacity(labels.len() * 2);
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" │ ", Style::default().fg(theme.border).add_modifier(Modifier::DIM)));
        }
        if i == active {
            spans.push(Span::styled(
                format!("▸ {}", label),
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                format!("  {}", label),
                Style::default().fg(theme.muted),
            ));
        }
    }
    spans
}

/// 右侧看板 — 模式自适应布局
///
/// Chat 模式：两区块纵向排列（时间线 + 记忆），无 Tab
/// Team 模式：Tab [总览 | 任务] — 总览=Chat两区块，任务=专家状态+任务看板
/// Meeting 模式：Tab [总览 | 议程] — 总览=Chat两区块，议程=专家列表+决策记录
///
/// 引用关系：被 modes/chat.rs、team.rs、meeting.rs 调用
/// 生命周期：面板可见时每帧渲染
pub fn render_panel(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    use crate::tui::state::AbacusMode;

    // K1 焦点反馈：focused → Thick + primary；非 focused → Rounded + border
    // V26: 焦点反馈从"整边框 Thick+primary"改为"上边框 primary, 其他三边保持 Rounded+border"
    //      旧设计副作用: ① Thick 切换让边框字符宽度跳变(╭─╮ → ┏━┓), 内容视觉位移
    //                  ② 整边框变色与已有"primary 色条贯通内容"重复, 视觉过载
    //      新设计: 单一上边变色(类 macOS 窗口活跃标题栏), 焦点定位明确且不抢戏
    //      实现: 始终画 Rounded+border 全边框, focus 时再覆盖 Borders::TOP 为 primary
    // focus_pulsing(200ms)追加 BOLD 强调(仅作用于上边框)
    let focused = state.focus == Focus::Panel;
    let panel_block = Block::default()
        .border_type(BorderType::Rounded)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(state.theme.border));

    let inner = panel_block.inner(area);
    f.render_widget(panel_block, area);

    // V26.1: focus 时叠加上边框 primary, 缩小 area 避开两端角字符 ╭╮
    //        ratatui Block 的 render_top_side 会从 area.left() 画到 area.right(),
    //        用 horizontal_top(─) 覆盖整行——若 area 包含两端, 会把 ╭╮ 角覆写成 ─
    //        而 top_left_corner 仅在 Borders 同时含 LEFT|TOP 时才画, 单 TOP 不修复
    //        修复: top_overlay 的 area 只覆盖中间段 [x+1, x+width-1), 保留两端角
    if focused && area.width >= 3 {
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
        // V28.6 (PR12-1 续): focus 上边框由 ─ 升级为 ━ (BorderType::Thick),
        //   解决"焦点反馈太细"问题。area 已经缩进过, 不会覆盖 ╭╮ 角字符,
        //   所以圆角主体保留, 只是中间横线段加粗 + 着色 — V26 旧担忧不复存在
        let top_overlay = Block::default()
            .borders(Borders::TOP)
            .border_type(BorderType::Thick)
            .border_style(top_style);
        f.render_widget(top_overlay, top_segment);
    }

    match state.mode {
        AbacusMode::Clarify => {
            // V16: Clarify 也加单元素 Tab 栏，与 Plan/Team/Meeting 顶部结构对齐
            // 设计意图: 三模式 panel 起手统一 [Tab + sep + 内容]，避免 Chat 内容裸起步带来的视觉断层
            // 引用关系: 复用 build_tab_spans helper；单 label "总览" 与 Team/Meeting 的首 Tab 同义
            // 生命周期: 每帧渲染；Chat 模式不切换 Tab，active 始终 0
            let layout = ratatui::layout::Layout::default()
                .direction(ratatui::layout::Direction::Vertical)
                .constraints([
                    ratatui::layout::Constraint::Length(1), // Tab 栏（静态单标签）
                    ratatui::layout::Constraint::Length(1), // 分隔线
                    ratatui::layout::Constraint::Min(2),    // 内容
                ])
                .split(inner);

            // V33 场景化拆分: 「现场」(timeline + 实体/工具) + 「量化」(📊 + 知识宫殿层级)
            // 引用关系: 与 Team/Meeting/Plan 一致, 都用 现场/量化 双 tab 范式
            let chat_labels: Vec<String> = vec![
                label_with_count("现场", state.trace_events.len()),
                "量化".to_string(),
            ];
            let tab_idx = match state.panel_tab {
                PanelTab::Quant => 1,
                _ => 0,
            };
            let tab_spans = build_tab_spans(&chat_labels, tab_idx, &state.theme);
            f.render_widget(Paragraph::new(Line::from(tab_spans)), layout[0]);

            let sep = "─".repeat(inner.width as usize);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(&*sep, Style::default().fg(state.theme.border).add_modifier(Modifier::DIM)))),
                layout[1],
            );

            // V23: 内容区统一加色条卡片
            let content = render_card_bar(f, &state.theme, layout[2]);
            match state.panel_tab {
                PanelTab::Quant => render_tab_quant(f, state, content),
                _ => render_panel_overview(f, state, content),
            }
        }
        AbacusMode::Team => {
            // Team: Tab 栏 + 内容
            let layout = ratatui::layout::Layout::default()
                .direction(ratatui::layout::Direction::Vertical)
                .constraints([
                    ratatui::layout::Constraint::Length(1), // Tab 栏
                    ratatui::layout::Constraint::Length(1), // 分隔线
                    ratatui::layout::Constraint::Min(2),    // 内容
                ])
                .split(inner);

            // V33 场景化: 现场 / 任务 / 量化 三 tab + 用户自定义
            let tab_labels: Vec<String> =
                std::iter::once(label_with_count("现场", state.trace_events.len()))
                    .chain(std::iter::once(label_with_count("任务", state.tasks.len())))
                    .chain(std::iter::once("量化".to_string()))
                    .chain(state.custom_tabs.iter().map(|ct| ct.name.clone()))
                    .collect();
            let tab_idx = match state.panel_tab {
                PanelTab::Tasks => 1,
                PanelTab::Quant => 2,
                PanelTab::Custom(i) => 3 + i,
                _ => 0,
            };
            let tab_spans = build_tab_spans(&tab_labels, tab_idx, &state.theme);
            f.render_widget(Paragraph::new(Line::from(tab_spans)), layout[0]);

            let sep = "─".repeat(inner.width as usize);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(&*sep, Style::default().fg(state.theme.border).add_modifier(Modifier::DIM)))),
                layout[1],
            );

            // V23: PanelTab 内容统一加色条卡片
            let content = render_card_bar(f, &state.theme, layout[2]);
            match state.panel_tab {
                PanelTab::Tasks => render_panel_team_board(f, state, content),
                PanelTab::Quant => render_tab_quant(f, state, content),
                PanelTab::Custom(idx) => render_custom_tab(f, state, content, idx),
                _ => render_panel_overview(f, state, content),
            }
        }
        AbacusMode::Meeting => {
            // Meeting: Tab 栏 + 内容
            let layout = ratatui::layout::Layout::default()
                .direction(ratatui::layout::Direction::Vertical)
                .constraints([
                    ratatui::layout::Constraint::Length(1),
                    ratatui::layout::Constraint::Length(1),
                    ratatui::layout::Constraint::Min(2),
                ])
                .split(inner);

            // V33 场景化: 现场 / 议程 / 量化 三 tab + 用户自定义
            // 议程 tab 复用 PanelTab::Tasks 索引位（meeting 主体是专家列表 + 决策记录，但路由用 Tasks 简化）
            let tab_labels: Vec<String> =
                std::iter::once(label_with_count("现场", state.trace_events.len()))
                    .chain(std::iter::once(label_with_count("议程", state.experts.len())))
                    .chain(std::iter::once("量化".to_string()))
                    .chain(state.custom_tabs.iter().map(|ct| ct.name.clone()))
                    .collect();
            let tab_idx = match state.panel_tab {
                PanelTab::Tasks => 1,
                PanelTab::Quant => 2,
                PanelTab::Custom(i) => 3 + i,
                _ => 0,
            };
            let tab_spans = build_tab_spans(&tab_labels, tab_idx, &state.theme);
            f.render_widget(Paragraph::new(Line::from(tab_spans)), layout[0]);

            let sep = "─".repeat(inner.width as usize);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(&*sep, Style::default().fg(state.theme.border).add_modifier(Modifier::DIM)))),
                layout[1],
            );

            // V23: PanelTab 内容统一加色条卡片
            let content = render_card_bar(f, &state.theme, layout[2]);
            match state.panel_tab {
                PanelTab::Tasks => render_panel_meeting_agenda(f, state, content),
                PanelTab::Quant => render_tab_quant(f, state, content),
                PanelTab::Custom(idx) => render_custom_tab(f, state, content, idx),
                _ => render_panel_overview(f, state, content),
            }
        }
        AbacusMode::Plan => {
            // V33 Plan: Tab 栏 + 内容（与 Team/Meeting 同布局）
            // 两个 Tab："摘要" / "任务"（Plan 输出预览，Tasks tab 复用 team_board 渲染）
            let layout = ratatui::layout::Layout::default()
                .direction(ratatui::layout::Direction::Vertical)
                .constraints([
                    ratatui::layout::Constraint::Length(1),
                    ratatui::layout::Constraint::Length(1),
                    ratatui::layout::Constraint::Min(2),
                ])
                .split(inner);

            // V33 场景化 Plan: 现场 / 任务 / 量化 三 tab + 用户自定义
            let tab_labels: Vec<String> =
                std::iter::once(label_with_count("现场", state.trace_events.len()))
                    .chain(std::iter::once(label_with_count("任务", state.tasks.len())))
                    .chain(std::iter::once("量化".to_string()))
                    .chain(state.custom_tabs.iter().map(|ct| ct.name.clone()))
                    .collect();
            let tab_idx = match state.panel_tab {
                PanelTab::Tasks => 1,
                PanelTab::Quant => 2,
                PanelTab::Custom(i) => 3 + i,
                _ => 0,
            };
            let tab_spans = build_tab_spans(&tab_labels, tab_idx, &state.theme);
            f.render_widget(Paragraph::new(Line::from(tab_spans)), layout[0]);

            let sep = "─".repeat(inner.width as usize);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(&*sep, Style::default().fg(state.theme.border).add_modifier(Modifier::DIM)))),
                layout[1],
            );

            let content = render_card_bar(f, &state.theme, layout[2]);
            match state.panel_tab {
                // Plan 任务 tab 复用 team_board（同 TaskCard 数据结构）
                PanelTab::Tasks => render_panel_team_board(f, state, content),
                PanelTab::Quant => render_tab_quant(f, state, content),
                PanelTab::Custom(idx) => render_custom_tab(f, state, content, idx),
                _ => render_panel_overview(f, state, content),
            }
        }
    }
}

/// 面板总览区块(Clarify 摘要 / Plan·Team·Meeting 的"摘要"Tab)
/// V23: 色条逻辑已迁出到 render_card_bar (render_panel 统一调用),
///      此函数只负责内容区垂直布局: timeline / 细分隔 / memory
/// 引用关系: 被三模式的 PanelTab::Overview 分支调用
/// 生命周期: 每帧渲染; 不持有状态
fn render_panel_overview(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    // V24: timeline 占 60% (原 40%), 给"事件流"主导地位
    //      memory 用 Min(15) 兜底, 容纳记忆+工具+统计 三子分块(~15 行)
    //      小屏(area.height<25)下 ratatui 按权重缩比, Min(15) 软约束保底
    let sections = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Percentage(60),  // V24: 40 → 60 (timeline 主导)
            ratatui::layout::Constraint::Length(1),       // 细分隔 ╌╌╌
            ratatui::layout::Constraint::Min(15),         // V24: 3 → 15 (memory 三子分块紧凑靠底)
        ])
        .split(area);

    render_tab_timeline(f, state, sections[0]);

    // 细分隔: 与 L1 标题对齐(col=1 起), 8 个 ╌ 看起来精致而不喧闹
    let dotted_style = Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM);
    f.render_widget(
        Paragraph::new(Line::styled(" ╌╌╌╌╌╌╌╌", dotted_style)),
        sections[1],
    );

    render_tab_memory(f, state, sections[2]);
}

/// Team 模式专属：任务看板（专家状态 + Task Kanban）
fn render_panel_team_board(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    // V23: 与色条卡片型对齐 — L1 标题 col=1, meta `· N`, 子分块间细分隔 ╌╌
    // 引用关系: state.experts / state.tasks 来自 Team 模式状态机
    let dotted_sep = Line::styled(
        " ╌╌╌╌╌╌╌╌",
        Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM),
    );

    // ── 团队 ──
    let active_count = state.experts.iter().filter(|e| matches!(e.status, ExpertStatus::Active)).count();
    lines.push(Line::from(vec![
        Span::styled(" 🧠 团队", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" · {}/{}", active_count, state.experts.len()),
            Style::default().fg(state.theme.muted),
        ),
    ]));

    if state.experts.is_empty() {
        lines.push(Line::styled("   —", Style::default().fg(state.theme.muted)));
    } else {
        for expert in &state.experts {
            let (status_icon, sc) = match expert.status {
                ExpertStatus::Active => ("●", state.theme.success),
                ExpertStatus::Idle => ("◌", state.theme.muted),
                ExpertStatus::Done => ("✓", state.theme.success),
            };
            // V28.7: confidence == 0.0 表示 orchestrator 未提供置信度——显示 "—" 不造伪数据
            let conf_span = if expert.confidence > 0.0 {
                Span::styled(format!("{:.0}%", expert.confidence * 100.0), Style::default().fg(state.theme.gold))
            } else {
                Span::styled("—", Style::default().fg(state.theme.muted))
            };
            lines.push(Line::from(vec![
                Span::styled(format!("   {} ", status_icon), Style::default().fg(sc)),
                Span::styled(&expert.name, state.theme.text_style(TextRole::BodyEmphasis)),
                Span::styled(format!(" · {} · ", expert.domain), Style::default().fg(state.theme.muted)),
                conf_span,
            ]));
        }
    }

    // ── 任务 ──
    lines.push(dotted_sep.clone());
    let done_count = state.tasks.iter().filter(|t| t.status == TaskStatus::Done).count();
    lines.push(Line::from(vec![
        Span::styled(" 📋 任务", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" · {}/{}", done_count, state.tasks.len()),
            Style::default().fg(state.theme.muted),
        ),
    ]));

    if state.tasks.is_empty() {
        lines.push(Line::styled("   —", Style::default().fg(state.theme.muted)));
    } else {
        for task in &state.tasks {
            let (icon, tc) = match task.status {
                TaskStatus::Pending => ("◌", state.theme.muted),
                TaskStatus::InProgress => ("●", state.theme.accent),
                TaskStatus::Done => ("✓", state.theme.success),
                TaskStatus::Blocked => ("⚠", state.theme.error),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("   {} ", icon), Style::default().fg(tc)),
                Span::styled(&task.title, Style::default().fg(state.theme.text)),
            ]));
            // 进度条 (二级缩进 col=5)
            let bar_len = 10;
            let filled = (task.progress as usize * bar_len / 100).min(bar_len);
            let empty = bar_len - filled;
            lines.push(Line::from(vec![
                Span::raw("     "),
                Span::styled("█".repeat(filled), Style::default().fg(tc)),
                Span::styled("░".repeat(empty), Style::default().fg(state.theme.border)),
                Span::styled(format!(" {}% · {}", task.progress, task.assignee), Style::default().fg(state.theme.muted)),
            ]));
            if !task.deps.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(format!("     依赖: {}", task.deps.join(", ")), state.theme.text_style(TextRole::Caption)),
                ]));
            }
        }
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// Meeting 模式专属：议程看板（专家列表 + 决策记录）
fn render_panel_meeting_agenda(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    // V23: 与色条卡片型对齐 — L1 标题 col=1, meta `· N`, 子分块间细分隔 ╌╌
    // 引用关系: state.experts / state.messages 来自 Meeting 模式状态机
    let dotted_sep = Line::styled(
        " ╌╌╌╌╌╌╌╌",
        Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM),
    );

    // ── 参会者 ──
    let speaking_count = state.experts.iter().filter(|e| matches!(e.status, ExpertStatus::Active)).count();
    lines.push(Line::from(vec![
        Span::styled(" 🎙 参会者", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" · {}/{}", speaking_count, state.experts.len()),
            Style::default().fg(state.theme.muted),
        ),
    ]));

    if state.experts.is_empty() {
        lines.push(Line::styled("   —", Style::default().fg(state.theme.muted)));
    } else {
        for expert in &state.experts {
            let (status_icon, sc) = match expert.status {
                ExpertStatus::Active => ("🔊", state.theme.success),
                ExpertStatus::Idle => ("🔇", state.theme.muted),
                ExpertStatus::Done => ("✓", state.theme.success),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("   {} ", status_icon), Style::default().fg(sc)),
                Span::styled(&expert.name, Style::default().fg(state.theme.expert).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" ({})", expert.domain), Style::default().fg(state.theme.muted)),
            ]));
        }
    }

    // ── 决策 ──
    lines.push(dotted_sep.clone());
    // V23: 决策计数 — 用 Session 角色消息总数(后续 take(3) 仍只显示前 3,但 meta 反映总数)
    let total_decisions = state.messages.iter()
        .filter(|m| matches!(m.role, MsgRole::Session))
        .count();
    lines.push(Line::from(vec![
        Span::styled(" 📝 决策", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" · {}", total_decisions),
            Style::default().fg(state.theme.muted),
        ),
    ]));

    // 从消息中提取共识（Session 角色的最后几条）
    let decisions: Vec<&str> = state.messages.iter()
        .rev()
        .filter(|m| matches!(m.role, MsgRole::Session))
        .flat_map(|m| m.parts.iter().filter_map(|p| match p {
            MsgContent::Stream(s) => Some(s.as_str()),
            _ => None,
        }))
        .take(3)
        .collect();

    if decisions.is_empty() {
        lines.push(Line::styled("   —", Style::default().fg(state.theme.muted)));
    } else {
        for (i, d) in decisions.iter().enumerate() {
            let summary: String = d.chars().take(30).collect();
            let display = if d.chars().count() > 30 { format!("{}…", summary) } else { summary };
            lines.push(Line::from(vec![
                Span::styled(format!("   {}. ", i + 1), Style::default().fg(state.theme.gold)),
                Span::styled(display, Style::default().fg(state.theme.text)),
            ]));
        }
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// 自定义 Tab 通用渲染器 — 根据 TabTemplate 分派渲染
///
/// 支持模板：KeyValue / Table / ProgressBars / Sparkline / FreeText / Mixed
/// 引用关系：被 render_panel 的 PanelTab::Custom(idx) 分支调用
fn render_custom_tab(f: &mut ratatui::Frame, state: &AppState, area: Rect, idx: usize) {
    use crate::tui::state::{TabTemplate, TabRowKind};

    let tab = match state.custom_tabs.get(idx) {
        Some(t) => t,
        None => {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(" (Tab not found)", Style::default().fg(state.theme.muted)))),
                area,
            );
            return;
        }
    };

    let mut lines: Vec<Line> = Vec::new();

    if tab.content.is_empty() {
        lines.push(Line::from(Span::styled("  (无数据)", Style::default().fg(state.theme.muted))));
        f.render_widget(Paragraph::new(lines), area);
        return;
    }

    match &tab.template {
        TabTemplate::KeyValue => {
            for row in &tab.content {
                let color = resolve_color_hint(&row.color_hint, state);
                lines.push(Line::from(vec![
                    Span::styled(format!("  {} ", row.label), Style::default().fg(state.theme.muted)),
                    Span::styled(&row.value, Style::default().fg(color)),
                ]));
            }
        }
        TabTemplate::ProgressBars => {
            for row in &tab.content {
                let pct = match &row.kind {
                    TabRowKind::Progress { percent } => *percent,
                    _ => row.numeric.map(|n| n as u8).unwrap_or(0),
                };
                let bar_len = 12;
                let filled = (pct as usize * bar_len / 100).min(bar_len);
                let empty = bar_len - filled;
                let color = resolve_color_hint(&row.color_hint, state);
                lines.push(Line::from(vec![
                    Span::styled(format!("  {} ", row.label), Style::default().fg(state.theme.text)),
                    Span::styled("█".repeat(filled), Style::default().fg(color)),
                    Span::styled("░".repeat(empty), Style::default().fg(state.theme.border)),
                    Span::styled(format!(" {}%", pct), Style::default().fg(state.theme.muted)),
                ]));
            }
        }
        TabTemplate::Sparkline { width } => {
            for row in &tab.content {
                if let TabRowKind::Sparkline { values } = &row.kind {
                    let spark_chars = "▁▂▃▄▅▆▇█";
                    let max_val = values.iter().cloned().fold(f64::MIN, f64::max).max(1.0);
                    let min_val = values.iter().cloned().fold(f64::MAX, f64::min);
                    let range = (max_val - min_val).max(0.01);
                    let spark: String = values.iter().rev().take(*width).rev().map(|v| {
                        let idx = ((v - min_val) / range * 7.0) as usize;
                        spark_chars.chars().nth(idx.min(7)).unwrap_or('▁')
                    }).collect();
                    let color = resolve_color_hint(&row.color_hint, state);
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {} ", row.label), Style::default().fg(state.theme.muted)),
                        Span::styled(spark, Style::default().fg(color)),
                        Span::styled(format!(" {:.1}", values.last().unwrap_or(&0.0)), Style::default().fg(state.theme.text)),
                    ]));
                }
            }
        }
        TabTemplate::FreeText => {
            for row in &tab.content {
                let color = resolve_color_hint(&row.color_hint, state);
                lines.push(Line::from(Span::styled(format!("  {}", row.value), Style::default().fg(color))));
            }
        }
        TabTemplate::Table { columns } => {
            let header_spans: Vec<Span> = columns.iter().map(|col| {
                Span::styled(format!(" {:>8} ", col), Style::default().fg(state.theme.muted).add_modifier(Modifier::BOLD))
            }).collect();
            lines.push(Line::from(header_spans));
            for row in &tab.content {
                let cols: Vec<&str> = row.value.split('|').collect();
                let row_spans: Vec<Span> = cols.iter().map(|col| {
                    Span::styled(format!(" {:>8} ", col.trim()), Style::default().fg(state.theme.text))
                }).collect();
                lines.push(Line::from(row_spans));
            }
        }
        _ => {
            // Mixed 和其他：FreeText 降级
            for row in &tab.content {
                let color = resolve_color_hint(&row.color_hint, state);
                lines.push(Line::from(Span::styled(format!("  {}", row.value), Style::default().fg(color))));
            }
        }
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// 解析颜色提示字符串 → 实际 Color
fn resolve_color_hint(hint: &Option<String>, state: &AppState) -> Color {
    match hint.as_deref() {
        Some("success") => state.theme.success,
        Some("error") => state.theme.error,
        Some("gold") | Some("warning") => state.theme.gold,
        Some("accent") | Some("primary") => state.theme.accent,
        Some("muted") => state.theme.muted,
        _ => state.theme.text,
    }
}

/// 面板底部：会话统计摘要
/// 极小终端保护：宽度<20 或高度<5 时只显示提示，避免 layout split 产生 0 高 widget
/// 引用关系：render_overlays 在每个 mode 渲染最前调用
/// 生命周期：每帧检查；返回 true 表示已渲染提示，调用方应 return
pub fn render_min_terminal_warning(f: &mut ratatui::Frame) -> bool {
    if f.area().width < 20 || f.area().height < 5 {
        let msg = ratatui::widgets::Paragraph::new("终端太小，请调大窗口")
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
        PickerKind::Model    => " 🤖 选择模型 (↑↓ 选模型 · ←→ 调思考 · Enter 应用 · Esc 取消) ",
        PickerKind::Theme    => " 🎨 选择主题 (↑↓ Tab 移动, Enter 切换, Esc 取消) ",
        PickerKind::Thinking => " 💭 思考深度 (↑↓ Tab 移动, Enter 切换, Esc 取消) ",
    };
    let frame = f.area();

    // 计算尺寸：列宽取最长 label + 2(▶/●) + 12(主题色块预览) + 边框
    let widest: usize = p.labels.iter().map(|s| display_width(s.as_str())).max().unwrap_or(20);
    // 也考虑分组标题宽度
    let widest = if let Some(ref groups) = p.groups {
        let g_widest = groups.iter().map(|(name, _)| display_width(name) + 4).max().unwrap_or(0);
        widest.max(g_widest)
    } else { widest };
    let extra = if matches!(p.kind, PickerKind::Theme) { 14 } else { 4 }; // 主题需 7 色块×2
    let popup_w = ((widest + extra) as u16).max(36).min(frame.width).min(80);
    // V29.8: 分组渲染时为每个分组多保留 1 行(组标题); thinking slider 多保留 2 行(空行 + slider)
    let group_overhead = p.groups.as_ref().map(|g| g.len()).unwrap_or(0);
    let slider_overhead = if p.show_thinking_slider { 2 } else { 0 };
    let content_lines = p.items.len() + group_overhead + slider_overhead;
    let max_visible = 12usize.min(content_lines);
    let popup_h = (max_visible as u16 + 2).min(frame.height); // 2 = border 上下

    // 位置：与 completion popup 一致（上方→下方→居中）
    let above = input_area.y;
    let below = frame.height.saturating_sub(input_area.y.saturating_add(input_area.height));
    let popup_y = if above >= popup_h {
        input_area.y.saturating_sub(popup_h)
    } else if below >= popup_h {
        input_area.y + input_area.height
    } else {
        frame.height.saturating_sub(popup_h) / 2
    };
    // V23b：左对齐到 input_area.x，与 confirm_dialog 一致（用户偏好）
    let popup_x = input_area.x.min(frame.width.saturating_sub(popup_w));
    let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);

    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(state.theme.primary).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(state.theme.elevated));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // V29.8: 分组模式下不做滚动(假设 model 列表短), 简单全部显示
    //   未来若 model 多到需滚动, 可加 scroll_start 计算
    let mut lines: Vec<Line> = Vec::new();

    // 渲染模型项的闭包(单行)
    let render_item = |idx: usize, lines: &mut Vec<Line>| {
        let label = &p.labels[idx];
        let id = &p.items[idx];
        let is_sel = idx == p.selected;
        let is_cur = p.current == Some(idx);
        let marker = if is_sel && is_cur { "▶●" }
            else if is_sel { "▶ " }
            else if is_cur { " ●" }
            else { "  " };
        let row_style = if is_sel {
            state.theme.semantic_style(SemanticIntent::Success, Strength::Strong)
        } else if is_cur {
            Style::default().fg(state.theme.primary)
        } else {
            Style::default().fg(state.theme.text)
        };
        let mut spans: Vec<Span> = Vec::new();
        // V29.8: 分组模式下加 2 空格缩进, 让分组层级更明显
        let indent = if p.groups.is_some() { "  " } else { "" };
        spans.push(Span::styled(format!("{} {} ", indent, marker), row_style));
        spans.push(Span::styled(crate::tui::util::pad_to_width(label, widest), row_style));

        if matches!(p.kind, PickerKind::Theme) {
            let t = crate::tui::theme::from_name(id);
            spans.push(Span::raw(" "));
            spans.push(Span::styled("█", Style::default().fg(t.primary)));
            spans.push(Span::styled("█", Style::default().fg(t.accent)));
            spans.push(Span::styled("█", Style::default().fg(t.success)));
            spans.push(Span::styled("█", Style::default().fg(t.error)));
            spans.push(Span::styled("█", Style::default().fg(t.gold)));
            spans.push(Span::styled("█", Style::default().fg(t.muted)));
            spans.push(Span::styled("▓", Style::default().fg(t.text).bg(t.bg)));
        }
        lines.push(Line::from(spans));
    };

    // V29.8: 分组渲染分支
    if let Some(ref groups) = p.groups {
        for (provider, range) in groups {
            // 组标题: 灰色加粗, 不可选
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" ▾ {}", provider),
                    Style::default().fg(state.theme.muted).add_modifier(Modifier::BOLD),
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
        .title(" 设置 (Settings) ")
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
        "↑↓ 选择 · Enter 确认 · Esc 关闭",
        state.theme.text_style(TextRole::Caption),
    )));
    f.render_widget(hint, rows[5]);
}

/// Timeline tab — 简洁事件流（Go 版风格）
///
/// 格式：` [time] [icon] [content]`
/// 图标：llm=◐(accent), tool=⚙(gold), session=●(user), default=●(muted)
/// 自动滚动显示最新事件，无树形展开、无进度条
///
/// 引用关系：被 render_panel 的 tab match 调用
/// 生命周期：面板可见 + TabTimeline 选中时渲染
///
/// V28 (T8): 数据源从 state.events 切换到 state.trace_events(SSOT 单一真相)。
/// 文本按 TraceKind 重生成(Generic 同 content,Thinking/ToolCall/Reply 各有摘要),
/// 图标仍按 category 映射保持视觉兼容。
///
/// V28.1 (PR8): 鼠标点击展开 — 在 `state.timeline_expanded_ids` 集合中的 event 显示
/// inline 详情(限 3 行 + 折叠提示)。同时填 `state.timeline_row_map` 让 handle_mouse
/// 能反查"被点击的屏幕行 → event id"。
fn render_tab_timeline(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    use crate::tui::state::{TraceKind, ToolStatus};

    /// 单事件 inline 展开时的最大可见行数(thinking/tool 详情)
    const TIMELINE_DETAIL_MAX: usize = 3;

    let mut lines: Vec<Line> = Vec::new();
    let max_content = (area.width as usize).saturating_sub(12);

    // V28.1: 清空 row map 准备本帧重建
    let mut row_map = state.timeline_row_map.borrow_mut();
    row_map.clear();

    lines.push(Line::from(vec![
        Span::styled(" 📜 时间线", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {}", state.trace_events.len()), Style::default().fg(state.theme.muted)),
    ]));

    let max_events = (area.height as usize).saturating_sub(1);
    let total = state.trace_events.len();
    // V30 timeline 边界修复：
    //   1. 写 last_timeline_visible 给 event handler 作 clamp 依据（下次 += 后能重裁）
    //   2. 本帧渲染用 min 守门：超界 offset 临时裁取避免空白页（不回写 state，
    //      因为 render 函数是 &AppState 不可变。超界状态在下次 ScrollUp/Up 事件裁
    //      取后自然修正，或在 +new_event 后被新增的 total 带回合法区间。
    state.last_timeline_visible.set(max_events);
    let max_offset = total.saturating_sub(max_events);
    let effective_offset = state.timeline_scroll_offset.min(max_offset);
    let end = total.saturating_sub(effective_offset);
    let start = end.saturating_sub(max_events);

    if effective_offset > 0 && total > max_events {
        lines.push(Line::from(Span::styled(
            format!("   ↓ +{} 更新", effective_offset),
            state.theme.text_style(TextRole::Caption),
        )));
    }

    let event_count_before = lines.len();
    for evt in state.trace_events.iter().skip(start).take(end - start) {
        let (icon, ic) = match evt.category.as_str() {
            "llm" => ("◐", state.theme.accent),
            "tool" => ("⚙", state.theme.gold),
            "session" => ("●", state.theme.user),
            _ => ("○", state.theme.muted),
        };

        // V28.1: 已展开的 event 显示 ▾ 前缀,未展开显示 ▸(仅对有详情可展开的 kind 有意义)
        let is_expanded = state.timeline_expanded_ids.contains(&evt.id);
        let has_detail = matches!(&evt.kind,
            TraceKind::Thinking { .. } | TraceKind::ToolCall { .. });
        let arrow = if has_detail {
            if is_expanded { "▾ " } else { "▸ " }
        } else {
            "  " // 占位保持对齐
        };

        let raw_text = match &evt.kind {
            TraceKind::Generic { content } => content.clone(),
            TraceKind::Thinking { lines, .. } => format!("thinking · {}行", lines),
            TraceKind::ToolCall { name, status, .. } => {
                let status_icon = match status {
                    ToolStatus::Success => "✓",
                    ToolStatus::Failed => "✗",
                    ToolStatus::Running => "⟳",
                };
                let dur = evt.duration_ms.map(|ms| format_duration_ms_padded(ms)).unwrap_or_default();
                format!("{} · {}{}", name, status_icon, dur)
            }
            TraceKind::Reply { tokens } => format!("↩ reply · {} tok", tokens),
        };
        let content = crate::tui::util::truncate_to_width(&raw_text, max_content.saturating_sub(2));

        // V28.1: 记录这一行的 (绝对屏幕 y, event id) 用于鼠标点击反查
        // area.y + lines.len() = 该 line 渲染后的绝对 y(Paragraph 顺序铺行)
        let abs_y = area.y.saturating_add(lines.len() as u16);
        row_map.push((abs_y, evt.id));

        // V28.4: focused 锚点行加 highlight bg(theme.surface 同消息卡片背景,温和不刺眼)
        let is_focused = state.focused_event_id == Some(evt.id);
        let row_bg = if is_focused {
            Style::default().bg(state.theme.surface)
        } else {
            Style::default()
        };
        let line = Line::from(vec![
            Span::raw("   "),
            Span::styled(evt.time.clone(), state.theme.text_style(TextRole::Caption)),
            Span::raw(" "),
            Span::styled(icon, Style::default().fg(ic)),
            Span::raw(" "),
            Span::styled(arrow.to_string(), Style::default().fg(state.theme.muted)),
            Span::styled(content, Style::default().fg(state.theme.text)),
        ]).style(row_bg);
        lines.push(line);

        // V28.1: 展开态 — inline 渲染详情,限 TIMELINE_DETAIL_MAX 行
        if is_expanded && has_detail {
            let detail_lines: Vec<&str> = match &evt.kind {
                TraceKind::Thinking { text, .. } => text.lines().collect(),
                TraceKind::ToolCall { args, output, .. } => {
                    if let Some(out) = output {
                        if !out.is_empty() {
                            out.lines().collect()
                        } else { args.lines().collect() }
                    } else { args.lines().collect() }
                }
                _ => Vec::new(),
            };
            let detail_w = max_content.saturating_sub(3);
            let total_detail = detail_lines.len();
            let show_count = total_detail.min(TIMELINE_DETAIL_MAX);
            for d_line in detail_lines.iter().take(show_count) {
                let truncated = crate::tui::util::truncate_to_width(d_line, detail_w);
                // event 子行的 row_map 也指向同一 event id,点这些行也能 toggle 收回
                // V29.12 修复: 使用 area.y 偏移计算绝对屏幕 y (与主行 line 3120 一致)
                let abs_y_d = area.y.saturating_add(lines.len() as u16);
                row_map.push((abs_y_d, evt.id));
                lines.push(Line::from(vec![
                    Span::raw("        "),
                    Span::styled(truncated, state.theme.text_style(TextRole::Caption)),
                ]));
            }
            if total_detail > TIMELINE_DETAIL_MAX {
                let abs_y_more = area.y.saturating_add(lines.len() as u16);
                row_map.push((abs_y_more, evt.id));
                lines.push(Line::from(vec![
                    Span::raw("        "),
                    Span::styled(
                        format!("↳ +{} 行 (消息区 ▾ trace 看全部)", total_detail - TIMELINE_DETAIL_MAX),
                        state.theme.text_style(TextRole::Hint),
                    ),
                ]));
            }
        }
    }

    if lines.len() == event_count_before {
        lines.push(Line::styled("   —", Style::default().fg(state.theme.muted)));
    }

    drop(row_map); // 释放 borrow_mut 让 render_widget 能借 state(虽然此处不需要,谨慎)
    f.render_widget(Paragraph::new(lines), area);
}

/// 按 BlockKind 渲染 Block detail 内容(默认无软上限,保持旧 V12 行为)
///
/// V12: 替代之前所有 BlockKind 共用 plain Caption 的"一坨文本"展示
/// V28 (T5): 重构为 `render_block_detail_with_limit` 的薄 wrapper, max_lines=0 表示不限
///
/// 引用关系：被 build_message_lines 内 Block 展开分支调用
/// 生命周期：单次展开渲染，无持久缓存（detail 已固化在 message.parts）
fn render_block_detail<'a>(detail: &str, kind: &BlockKind, theme: &Theme) -> Vec<Line<'a>> {
    render_block_detail_with_limit(detail, kind, theme, 0)
}

/// V29.11: bash_exec 工具的 args 渲染为 shell 命令视图
///
/// 触发条件: tool name 是 bash_exec / bash.exec
/// 渲染:
///   $ command-text              ← theme.accent + 加粗
///   (workdir: /path/to/dir)    ← theme.muted, 仅在 args 含 workdir 时
/// 不限行: 命令本身通常 1-3 行, 不需要额外折叠
fn try_render_bash_exec<'a>(
    name: &str,
    args_json: &str,
    theme: &Theme,
    _max_total_lines: usize,
) -> Option<Vec<Line<'a>>> {
    let lower = name.to_lowercase();
    // ToolId 单一命名：注册时已是 "filengine_bash_exec"（无 sanitize 链路）。
    // 保留无前缀 "bash_exec" 作 demo 模式兜底；不再容忍带 . 的旧形态。
    if !matches!(lower.as_str(), "filengine_bash_exec" | "bash_exec") {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let command = json.get("command").and_then(|v| v.as_str()).unwrap_or("(empty)");
    let workdir = json.get("workdir").and_then(|v| v.as_str());

    let mut lines: Vec<Line<'a>> = Vec::new();
    // 命令行 — 多行命令每行都加 $ / > 前缀
    let cmd_lines: Vec<&str> = command.lines().collect();
    for (i, l) in cmd_lines.iter().enumerate() {
        let prefix = if i == 0 { "$ " } else { "> " };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{}{}", prefix, l),
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    // workdir 提示(可选)
    if let Some(wd) = workdir {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  (workdir: {})", wd),
                theme.text_style(TextRole::Caption),
            ),
        ]));
    }
    Some(lines)
}

// ═══ V29.12: 连续同名 ToolCall 合并展示 ═══════════════════════════════════
//
// 设计:
//   - 纯渲染层分组,trace_events 数据不改。timeline panel、/copy 不受影响。
//   - 仅相邻且同名的 ToolCall 归为一组 (run),中间插入其他 kind 则断开。
//   - 单条 run 照原渲染 (不丢信息); 多条合并为 `⚙ name ×N · 状态 · 总耗时` header
//     + 每条调用的关键参数单行摘要。
//
// 引用关系:
//   - 被 build_message_lines → Trace 展开态调用
//   - 引用 render_single_trace_event (渲染单条) / render_merged_tool_run (渲染合并组)
// 生命周期: 单次渲染帧

/// 对 event_ids 按连续同名 ToolCall 分组。
///
/// 返回 `Vec<Vec<u64>>`,每组内 id 的对应 trace event 同名且相邻。
/// 非 ToolCall 类型单独成组 (len=1)。
fn group_consecutive_tool_runs(
    event_ids: &[u64],
    trace_events: &[crate::tui::state::TraceEvent],
) -> Vec<Vec<u64>> {
    use crate::tui::state::TraceKind;

    let mut runs: Vec<Vec<u64>> = Vec::new();
    for id in event_ids {
        let tool_name = trace_events.iter().find(|e| e.id == *id).and_then(|ev| {
            if let TraceKind::ToolCall { ref name, .. } = ev.kind { Some(name.as_str()) } else { None }
        });
        // 尝试追加到前一 run (必须都是 ToolCall 且同名)
        let append = if let (Some(prev_run), Some(cur_name)) = (runs.last(), tool_name) {
            // 前一 run 的首 id 对应的 tool name
            let prev_name = trace_events.iter().find(|e| e.id == prev_run[0]).and_then(|ev| {
                if let TraceKind::ToolCall { ref name, .. } = ev.kind { Some(name.as_str()) } else { None }
            });
            prev_name == Some(cur_name)
        } else {
            false
        };
        if append {
            runs.last_mut().unwrap().push(*id);
        } else {
            runs.push(vec![*id]);
        }
    }
    runs
}

/// 渲染单条 trace event (非合并路径)。
///
/// 引用关系: 从 build_message_lines Trace 展开态调用
/// 生命周期: 单帧渲染,输出 push 到 `lines`
fn render_single_trace_event<'a>(
    ev: &crate::tui::state::TraceEvent,
    bar: &Span<'a>,
    theme: &Theme,
    max_lines_think: usize,
    max_lines_tool: usize,
    lines: &mut Vec<Line<'a>>,
) {
    use crate::tui::state::{TraceKind, ToolStatus};

    match &ev.kind {
        TraceKind::Thinking { text, lines: l_count } => {
            // V29.12: 消息内不重复显示 time (timeline panel 已有),
            //   与 ToolCall (`⚙ name · ✓ · dur`) 风格对称
            lines.push(Line::from(vec![
                bar.clone(),
                Span::raw("     "),
                Span::styled("💭 ", Style::default().fg(theme.accent)),
                Span::styled(
                    format!("Thinking · {}行", l_count),
                    theme.text_style(TextRole::Caption),
                ),
            ]));
            let detail_lines = render_block_detail_with_limit(
                text, &BlockKind::Think, theme, max_lines_think,
            );
            for dl in detail_lines {
                let mut spans: Vec<Span> = vec![bar.clone(), Span::raw("       ")];
                spans.extend(dl.spans);
                lines.push(Line::from(spans));
            }
        }
        TraceKind::ToolCall { name, args, output, status } => {
            let (status_icon, status_color) = match status {
                ToolStatus::Success => ("✓", theme.success),
                ToolStatus::Failed => ("✗", theme.error),
                ToolStatus::Running => ("⟳", theme.gold),
            };
            let dur_str = ev.duration_ms.map(|ms| format_duration_ms_padded(ms)).unwrap_or_default();
            lines.push(Line::from(vec![
                bar.clone(),
                Span::raw("     "),
                Span::styled("⚙ ", Style::default().fg(theme.gold)),
                Span::styled(name.clone(), Style::default().fg(theme.gold).add_modifier(Modifier::BOLD)),
                Span::raw(" · "),
                Span::styled(status_icon, Style::default().fg(status_color)),
                Span::styled(dur_str, theme.text_style(TextRole::Caption)),
            ]));
            if !args.is_empty() {
                // V29.11: 工具特化视图链
                let arg_lines = try_render_edit_diff(
                    name, args, theme, max_lines_tool,
                ).or_else(|| try_render_bash_exec(
                    name, args, theme, max_lines_tool,
                )).unwrap_or_else(|| render_block_detail_with_limit(
                    args, &BlockKind::ToolCall, theme, max_lines_tool,
                ));
                for dl in arg_lines {
                    let mut spans: Vec<Span> = vec![bar.clone(), Span::raw("       ")];
                    spans.extend(dl.spans);
                    lines.push(Line::from(spans));
                }
            }
            if let Some(out) = output {
                if !out.is_empty() {
                    lines.push(Line::from(vec![
                        bar.clone(),
                        Span::raw("       "),
                        Span::styled("→", theme.text_style(TextRole::Caption)),
                    ]));
                    let out_lines = render_block_detail_with_limit(
                        out, &BlockKind::ToolCall, theme, max_lines_tool,
                    );
                    for dl in out_lines {
                        let mut spans: Vec<Span> = vec![bar.clone(), Span::raw("       ")];
                        spans.extend(dl.spans);
                        lines.push(Line::from(spans));
                    }
                }
            }
        }
        TraceKind::Generic { content } => {
            lines.push(Line::from(vec![
                bar.clone(),
                Span::raw("     "),
                Span::styled(
                    format!("· {} · {}", ev.time, content),
                    theme.text_style(TextRole::Caption),
                ),
            ]));
        }
        TraceKind::Reply { tokens } => {
            lines.push(Line::from(vec![
                bar.clone(),
                Span::raw("     "),
                Span::styled(
                    format!("↩ reply · {} tok", tokens),
                    theme.text_style(TextRole::Caption),
                ),
            ]));
        }
    }
}

/// V29.12: 渲染合并的 tool call run (连续 ≥2 次同名调用)。
///
/// 视觉:
/// ```text
/// ⚙ fs_read ×3 · ✓ · 35ms
///      /path/to/a.rs
///      /path/to/b.rs
///      /path/to/c.rs
/// ```
///
/// 策略:
///   - Header: `⚙ name ×N · 聚合状态 · 总耗时`
///   - 聚合状态: 全成功 → ✓; 有失败 → `✓M ✗K`; 全运行中 → ⟳
///   - 每条调用提取关键参数摘要 (path 字段优先; 无则取 args 首行截断到 60 字符)
///   - 编辑类工具(fs_edit/fs_write)仍显示 diff,不做摘要退化
///
/// 引用关系: build_message_lines → group 分支
/// 生命周期: 单帧渲染
fn render_merged_tool_run<'a>(
    run: &[u64],
    trace_events: &[crate::tui::state::TraceEvent],
    bar: &Span<'a>,
    theme: &Theme,
    code_blocks_expanded: bool,
    expanded_event_ids: &std::collections::HashSet<u64>,
    lines: &mut Vec<Line<'a>>,
) {
    use crate::tui::state::{TraceKind, ToolStatus};

    // 收集本组 events（部分可能已被 FIFO 裁剪）
    let events: Vec<&crate::tui::state::TraceEvent> = run.iter()
        .filter_map(|id| trace_events.iter().find(|e| e.id == *id))
        .collect();
    if events.is_empty() {
        // 全部过期: 显示占位提示
        lines.push(Line::from(vec![
            bar.clone(),
            Span::raw("     "),
            Span::styled(
                format!("[{} events 已过期]", run.len()),
                theme.text_style(TextRole::Hint),
            ),
        ]));
        return;
    }

    // 从首 event 取 tool name
    let tool_name = match &events[0].kind {
        TraceKind::ToolCall { ref name, .. } => name.clone(),
        _ => return, // 不应发生
    };

    // 聚合状态
    let mut ok = 0u32;
    let mut fail = 0u32;
    let mut running = 0u32;
    let mut total_dur_ms: u64 = 0;
    for ev in &events {
        if let TraceKind::ToolCall { status, .. } = &ev.kind {
            match status {
                ToolStatus::Success => ok += 1,
                ToolStatus::Failed => fail += 1,
                ToolStatus::Running => running += 1,
            }
        }
        if let Some(d) = ev.duration_ms { total_dur_ms += d; }
    }

    let status_text = if fail == 0 && running == 0 {
        "✓".to_string()
    } else if running > 0 {
        format!("⟳{}", running)
    } else {
        format!("✓{} ✗{}", ok, fail)
    };
    let status_color = if fail > 0 { theme.error } else if running > 0 { theme.gold } else { theme.success };

    let dur_str = {
        let d = format_duration_ms(total_dur_ms);
        if d.is_empty() { String::new() } else { format!("  {}", d) }
    };

    // Header: ⚙ name ×N · status · dur
    lines.push(Line::from(vec![
        bar.clone(),
        Span::raw("     "),
        Span::styled("⚙ ", Style::default().fg(theme.gold)),
        Span::styled(
            format!("{} ×{}", tool_name, events.len()),
            Style::default().fg(theme.gold).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" · "),
        Span::styled(status_text, Style::default().fg(status_color)),
        Span::styled(dur_str, theme.text_style(TextRole::Caption)),
    ]));

    // 判断是否为编辑类工具(需要 diff 而非摘要)
    let is_edit_tool = tool_name.contains("edit") || tool_name.contains("write");

    // 逐条摘要或 diff
    let fully_expanded = code_blocks_expanded || run.iter().any(|id| expanded_event_ids.contains(id));
    let max_lines_tool = if fully_expanded { 0 } else { 20 };

    for (ei, ev) in events.iter().enumerate() {
        if let TraceKind::ToolCall { args, output, status, .. } = &ev.kind {
            if is_edit_tool && !args.is_empty() {
                // 编辑工具: 尝试 diff 视图
                // 多条编辑间插入 path 标识以区分不同文件(避免 diff 连片)
                if events.len() > 1 {
                    let path_hint = extract_tool_param_summary(args);
                    lines.push(Line::from(vec![
                        bar.clone(),
                        Span::raw("       "),
                        Span::styled(
                            format!("{}. {}", ei + 1, path_hint),
                            Style::default().fg(theme.muted).add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
                let arg_lines = try_render_edit_diff(
                    &tool_name, args, theme, max_lines_tool,
                ).unwrap_or_else(|| {
                    // fallback: 提取 path 摘要（单条时已有 path_hint 不重复）
                    vec![Line::from(Span::styled(
                        extract_tool_param_summary(args),
                        theme.text_style(TextRole::Caption),
                    ))]
                });
                for dl in arg_lines {
                    let mut spans: Vec<Span> = vec![bar.clone(), Span::raw("       ")];
                    spans.extend(dl.spans);
                    lines.push(Line::from(spans));
                }
            } else if !args.is_empty() {
                // 非编辑工具: 单行摘要
                let summary = extract_tool_param_summary(args);
                let (si, sc) = match status {
                    ToolStatus::Success => ("✓", theme.success),
                    ToolStatus::Failed => ("✗", theme.error),
                    ToolStatus::Running => ("⟳", theme.gold),
                };
                let item_dur = ev.duration_ms.map(|ms| {
                    let d = format_duration_ms(ms); if d.is_empty() { d } else { format!(" {}", d) }
                }).unwrap_or_default();
                lines.push(Line::from(vec![
                    bar.clone(),
                    Span::raw("       "),
                    Span::styled(si, Style::default().fg(sc)),
                    Span::raw(" "),
                    Span::styled(summary, theme.text_style(TextRole::Caption)),
                    Span::styled(item_dur, theme.text_style(TextRole::Hint)),
                ]));
            }
            // output 合并时省略 (避免过于冗长),展开后会走单条路径显示
            if fully_expanded {
                if let Some(out) = output {
                    if !out.is_empty() {
                        lines.push(Line::from(vec![
                            bar.clone(),
                            Span::raw("       "),
                            Span::styled("→", theme.text_style(TextRole::Caption)),
                        ]));
                        let out_lines = render_block_detail_with_limit(
                            out, &BlockKind::ToolCall, theme, max_lines_tool,
                        );
                        for dl in out_lines {
                            let mut spans: Vec<Span> = vec![bar.clone(), Span::raw("       ")];
                            spans.extend(dl.spans);
                            lines.push(Line::from(spans));
                        }
                    }
                }
            }
        }
    }
}

/// 统一耗时格式化（M+S 展示）。
///
/// | 输入 ms | 输出 |
/// |---------|------|
/// | 0 | "" |
/// | 120 | "120ms" |
/// | 1500 | "1.5s" |
/// | 65000 | "1m5s" |
/// | 130000 | "2m10s" |
///
/// 引用关系: 被 trace 渲染 / streaming tools / timeline / 合并组 header 共用
/// 生命周期: 纯函数,无状态
fn format_duration_ms(ms: u64) -> String {
    // 0 = 无耗时数据（聚合场景下多个 None 求和为 0）→ 不显示
    // 调用方如需区分 "瞬间完成 (Some(0))" vs "无数据 (None)"，
    // 应在 .map() 外层处理 None → 不调用本函数
    if ms == 0 {
        return String::new();
    }
    if ms < 1000 {
        return format!("{}ms", ms);
    }
    let total_secs = ms / 1000;
    let frac_ms = ms % 1000;
    if total_secs < 60 {
        // < 1 分钟: 显示秒 + 小数 (如 1.5s / 45s)
        if frac_ms >= 100 {
            format!("{}.{}s", total_secs, frac_ms / 100)
        } else {
            format!("{}s", total_secs)
        }
    } else {
        // ≥ 1 分钟: M分S秒 (如 1m5s / 2m10s)
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        if secs > 0 {
            format!("{}m{}s", mins, secs)
        } else {
            format!("{}m", mins)
        }
    }
}

/// 同 format_duration_ms 但带前导双空格（用于 span 拼接场景）
fn format_duration_ms_padded(ms: u64) -> String {
    let s = format_duration_ms(ms);
    if s.is_empty() { s } else { format!("  {}", s) }
}

/// 从 tool args JSON 中提取关键参数作为单行摘要。
///
/// 优先级: `path` → `file_path` → `url` → `query` → `command` → 首行截断60字符
fn extract_tool_param_summary(args_json: &str) -> String {
    // 辅助: UTF-8 安全截断 — 按 char 数而非字节切,避免多字节字符中间切断 panic
    fn truncate_chars(s: &str, max: usize) -> String {
        if s.chars().count() <= max { s.to_string() }
        else { format!("{}…", s.chars().take(max).collect::<String>()) }
    }

    if let Ok(obj) = serde_json::from_str::<serde_json::Value>(args_json) {
        // 按优先级尝试提取有意义的字段
        for key in &["path", "file_path", "url", "query", "command", "pattern", "selector"] {
            if let Some(val) = obj.get(*key).and_then(|v| v.as_str()) {
                return truncate_chars(val, 60);
            }
        }
        // fallback: 序列化首 60 字符
        let s = serde_json::to_string(&obj).unwrap_or_default();
        truncate_chars(&s, 60)
    } else {
        // 非 JSON: 首行截断
        let first_line = args_json.lines().next().unwrap_or("");
        truncate_chars(first_line, 60)
    }
}

/// V29.11: 编辑类工具（Edit/Write 等）的 args 渲染为 +/- diff 视图
///
/// 触发条件:
///   - tool name 落在白名单（含 mcp__filengine__file_edit / file_write 与裸名）
///   - args JSON 可解析且含 path + (old/new_string|content) 字段
/// 返回: Some(lines) 跳过默认 JSON pretty 渲染; None 退回默认路径
/// 引用关系: build_message_lines TraceKind::ToolCall args 分支前置
/// 生命周期: 单次渲染, 不缓存 (args/output 字符串已 owned 在 trace_events 内)
/// 设计取舍:
///   - 简单 diff (全旧 - / 全新 +) 而非 LCS 智能对比 — 编辑工具的 old/new
///     通常已是聚焦 chunk, 简单视图够用; 后续要更准可引 `similar` crate
fn try_render_edit_diff<'a>(
    name: &str,
    args_json: &str,
    theme: &Theme,
    max_total_lines: usize,
) -> Option<Vec<Line<'a>>> {
    let lower = name.to_lowercase();
    // ToolId 单一命名：filengine register 直接产 "filengine_fs_edit"/"filengine_fs_write"
    // 保留无前缀 "fs_edit"/"fs_write" 作 demo / 测试 fixture 兜底
    let is_edit = matches!(lower.as_str(), "filengine_fs_edit" | "fs_edit");
    let is_write = matches!(lower.as_str(), "filengine_fs_write" | "fs_write");
    if !is_edit && !is_write { return None; }

    let json: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let path = json.get("path")
        .or_else(|| json.get("file_path"))
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)");

    let mut lines: Vec<Line<'a>> = Vec::new();
    // 头行: 📝 path
    lines.push(Line::from(vec![
        Span::styled("📝 ", Style::default().fg(theme.accent)),
        Span::styled(
            path.to_string(),
            Style::default().fg(theme.muted).add_modifier(Modifier::BOLD),
        ),
    ]));

    let (old, new) = if is_edit {
        let o = json.get("old_string")
            .or_else(|| json.get("old_text"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let n = json.get("new_string")
            .or_else(|| json.get("new_text"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        (o.to_string(), n.to_string())
    } else {
        // Write 是新建/全量覆盖, 无旧内容
        let c = json.get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        (String::new(), c.to_string())
    };

    render_simple_diff(&mut lines, &old, &new, theme, max_total_lines);
    Some(lines)
}

/// LCS diff 渲染 — 基于 `similar` crate 的行级差分
///
/// 引用关系: 仅 try_render_edit_diff 内部调用
/// 限行算法: max_total_lines==0 不限; >0 时超额后截断 + 省略提示
/// 设计取舍:
///   - 用 similar::TextDiff::from_lines (Myers LCS, O(N·D))
///   - Equal 行显示为 context（theme.muted, 无前缀符号），但只保留变更临近 ±1 行
///   - Insert → `+ line` 绿; Delete → `- line` 红; 远离变更的 Equal 跳过
///   - Write (old 为空) 时 similar 全产出 Insert — 效果等同旧实现
fn render_simple_diff<'a>(
    lines: &mut Vec<Line<'a>>,
    old: &str,
    new: &str,
    theme: &Theme,
    max_total_lines: usize,
) {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(old, new);
    let mut rendered: Vec<Line<'a>> = Vec::new();
    let mut insert_count = 0usize;
    let mut delete_count = 0usize;

    // 收集所有变更 ops, 带上下文(±1 行 Equal)
    let changes: Vec<_> = diff.iter_all_changes().collect();
    let total_changes = changes.len();
    // 标记哪些 Equal 行要显示(距最近 Insert/Delete ≤1 行)
    let mut show_equal = vec![false; total_changes];
    for (i, c) in changes.iter().enumerate() {
        if c.tag() != ChangeTag::Equal {
            // 标记前后 ±1 行 Equal 为可见
            if i > 0 && changes[i - 1].tag() == ChangeTag::Equal { show_equal[i - 1] = true; }
            if i + 1 < total_changes && changes[i + 1].tag() == ChangeTag::Equal { show_equal[i + 1] = true; }
        }
    }

    let mut skipped_run = false;
    for (i, change) in changes.iter().enumerate() {
        let text = change.value().trim_end_matches('\n');
        match change.tag() {
            ChangeTag::Delete => {
                if skipped_run { rendered.push(Line::from(vec![Span::styled("  ···", theme.text_style(TextRole::Caption))])); skipped_run = false; }
                rendered.push(Line::from(vec![
                    Span::styled(format!("- {}", text), Style::default().fg(theme.error)),
                ]));
                delete_count += 1;
            }
            ChangeTag::Insert => {
                if skipped_run { rendered.push(Line::from(vec![Span::styled("  ···", theme.text_style(TextRole::Caption))])); skipped_run = false; }
                rendered.push(Line::from(vec![
                    Span::styled(format!("+ {}", text), Style::default().fg(theme.success)),
                ]));
                insert_count += 1;
            }
            ChangeTag::Equal => {
                if show_equal[i] {
                    if skipped_run { rendered.push(Line::from(vec![Span::styled("  ···", theme.text_style(TextRole::Caption))])); skipped_run = false; }
                    rendered.push(Line::from(vec![
                        Span::styled(format!("  {}", text), Style::default().fg(theme.muted)),
                    ]));
                } else {
                    skipped_run = true;
                }
            }
        }
    }

    // 限行裁剪
    let shown = if max_total_lines > 0 && rendered.len() > max_total_lines {
        let mut truncated: Vec<Line<'a>> = rendered.into_iter().take(max_total_lines).collect();
        truncated.push(Line::from(vec![
            Span::styled(
                format!("  ↳ ... 省略 (总 {} 行 diff)", total_changes),
                theme.text_style(TextRole::Caption),
            ),
        ]));
        truncated
    } else {
        rendered
    };
    lines.extend(shown);

    // 统计 footer
    lines.push(Line::from(vec![
        Span::styled(
            format!("  ↳ + {} 行 / − {} 行", insert_count, delete_count),
            theme.text_style(TextRole::Caption),
        ),
    ]));
}

/// V28: 带行数软上限的 detail 渲染。`max_lines = 0` 表示不限(旧行为);
/// `max_lines > 0` 时超过则截到 max_lines 并追加 `↳ +N 行 Ctrl+E 展开全部` 提示行。
///
/// 注意:ToolCall 内部还有 400/200 行硬上限(超长 tool output 兜底),与软上限独立生效。
/// 调用方决定 max_lines:Trace 中 thinking=30, tool=20;Block 直接展开传 0。
///
/// 引用关系: 被 render_block_detail (传 0) 和 build_message_lines Trace 分支(传 30/20) 调用
fn render_block_detail_with_limit<'a>(detail: &str, kind: &BlockKind, theme: &Theme, max_lines: usize) -> Vec<Line<'a>> {
    let lines: Vec<Line<'a>> = match kind {
        BlockKind::Think => {
            // 走 markdown 渲染——空 bar，让思考块按结构化文本展示
            let empty_bar = Span::raw("");
            let styled_lines = markdown::render_markdown(detail, theme, false);
            styled_lines.iter()
                .map(|s| markdown::styled_line_to_ratatui(s, &empty_bar, theme))
                .collect()
        }
        BlockKind::ToolCall => {
            // 尝试 JSON pretty-print；失败则降级为 plain
            let pretty = serde_json::from_str::<serde_json::Value>(detail.trim())
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok());
            let text = pretty.as_deref().unwrap_or(detail);
            // 限长保护：超过 400 行只显示前 200 + 截断提示
            let all_lines: Vec<&str> = text.lines().collect();
            let total = all_lines.len();
            let truncated = total > 400;
            let mut out: Vec<Line> = all_lines.iter()
                .take(if truncated { 200 } else { total })
                .map(|l| Line::from(vec![
                    Span::styled(l.to_string(), theme.text_style(TextRole::InlineCode)),
                ]))
                .collect();
            if truncated {
                out.push(Line::from(vec![
                    Span::styled(
                        format!("… (已截断，原始 {} 行；用 /export 导出完整)", total),
                        theme.text_style(TextRole::Hint),
                    ),
                ]));
            }
            out
        }
        BlockKind::Checklist => {
            detail.lines()
                .map(|l| Line::from(vec![
                    Span::styled(l.to_string(), theme.text_style(TextRole::Caption)),
                ]))
                .collect()
        }
    };

    // V28: 应用软上限 — 超出 max_lines 时截断并追加折叠提示行
    if max_lines > 0 && lines.len() > max_lines {
        let hidden = lines.len() - max_lines;
        let mut limited: Vec<Line<'a>> = lines.into_iter().take(max_lines).collect();
        limited.push(Line::from(vec![
            Span::styled(
                format!("↳ +{} 行  Ctrl+E 展开全部", hidden),
                theme.text_style(TextRole::Caption),
            ),
        ]));
        return limited;
    }
    lines
}

/// 主题预览：把 12 套主题逐行渲染为色板，每行包含主要语义色块
///
/// 引用关系：被 render_tab_memory 在 state.theme_preview_open 为 true 时优先调用
/// 生命周期：单次绘制；不持有状态
/// 设计意图：用户选择前可视化对比，不必"切完才知道效果"
fn render_theme_preview(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    use crate::tui::theme::{from_name, Theme};
    let names = Theme::all_names();
    let mut lines: Vec<Line> = Vec::new();

    // 标题行 + 分隔
    lines.push(Line::from(vec![
        Span::styled(
            " 主题预览 ".to_string(),
            state.theme.text_style(TextRole::H1),
        ),
        Span::styled(
            "(用 /theme <name> 切换，Esc 关闭)".to_string(),
            state.theme.text_style(TextRole::Caption),
        ),
    ]));
    lines.push(Line::raw(""));

    // 表头
    lines.push(Line::from(vec![
        Span::styled(format!("{:<16}", "name"), state.theme.text_style(TextRole::Caption)),
        Span::styled(format!("{:<6}", "prim"), state.theme.text_style(TextRole::Caption)),
        Span::styled(format!("{:<6}", "accnt"), state.theme.text_style(TextRole::Caption)),
        Span::styled(format!("{:<6}", "text"), state.theme.text_style(TextRole::Caption)),
        Span::styled(format!("{:<6}", "muted"), state.theme.text_style(TextRole::Caption)),
        Span::styled(format!("{:<6}", "succ"), state.theme.text_style(TextRole::Caption)),
        Span::styled(format!("{:<6}", "err"), state.theme.text_style(TextRole::Caption)),
        Span::styled(format!("{:<6}", "gold"), state.theme.text_style(TextRole::Caption)),
        Span::styled(format!("{:<6}", "bg"), state.theme.text_style(TextRole::Caption)),
    ]));
    lines.push(Line::raw(""));

    // 每个主题一行：name + 7 个色块
    let block = "████";
    for name in names {
        let t = from_name(name);
        // 当前主题打 ▶ 标记
        let marker = if t.name == state.theme.name { "▶ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(format!("{}{:<14}", marker, t.name), state.theme.text_style(TextRole::BodyEmphasis)),
            Span::styled(format!("{:<6}", block), Style::default().fg(t.primary)),
            Span::styled(format!("{:<6}", block), Style::default().fg(t.accent)),
            Span::styled(format!("{:<6}", block), Style::default().fg(t.text)),
            Span::styled(format!("{:<6}", block), Style::default().fg(t.muted)),
            Span::styled(format!("{:<6}", block), Style::default().fg(t.success)),
            Span::styled(format!("{:<6}", block), Style::default().fg(t.error)),
            Span::styled(format!("{:<6}", block), Style::default().fg(t.gold)),
            // bg 色块用 bg 着色，文字 auto-contrast 保证可见
            Span::styled(
                format!("{:<6}", "  ▓▓"),
                Style::default().fg(t.text).bg(t.bg),
            ),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        " 提示：色块对比不同主题的 7 个语义色 + bg；▶ 为当前主题".to_string(),
        state.theme.text_style(TextRole::Hint),
    )));

    f.render_widget(Paragraph::new(lines), area);
}

/// V33 「现场」memory section
///
/// 设计意图：服务"跟现场"用户场景——只展示当下激活的实体/工具/知识小计，
/// 不展开宫殿层级树，不显示成本统计（那些归到「量化」tab）。
///
/// 引用关系：
///   - 被 render_panel_overview 调用作为下半区块
///   - 数据源：state.messages (Expert 消息→实体名)、state.tool_records (工具去重)、
///             state.knowledge_calls (条数+总次数小计)
///   - 与 render_tab_quant 共用 state 字段但口径不同：现场=活跃 unique 数，量化=累计调用次数
///
/// 排版口径（与现有看板风格延续）：
///   - L1 标题 col=1（accent + BOLD）+ meta · N
///   - L1.5 子标题 col=3（BodyEmphasis）
///   - 数据行 col=4（muted label + text value）
///   - 子分块间用 dotted_sep（8 ╌）替代空行
fn render_tab_memory(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    if state.theme_preview_open {
        render_theme_preview(f, state, area);
        return;
    }
    let _mem_area: Rect = if !state.info_panel_text.is_empty() {
        let parts = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([
                ratatui::layout::Constraint::Min(4),
                ratatui::layout::Constraint::Length(1),
                ratatui::layout::Constraint::Min(1),
            ])
            .split(area);
        // 用空 bar（info panel 无色条侧边栏）调用 styled_line_to_ratatui，
        // 复用消息区一致的标题加粗 / 列表缩进 / 代码行样式
        let empty_bar = Span::raw("");
        let styled = markdown::render_markdown(&state.info_panel_text, &state.theme, false);
        let md_lines: Vec<Line> = styled
            .iter()
            .map(|s| markdown::styled_line_to_ratatui(s, &empty_bar, &state.theme))
            .collect();
        f.render_widget(Paragraph::new(md_lines), parts[0]);
        f.render_widget(Paragraph::new(Line::from(Span::styled(
            "─".repeat(parts[1].width as usize),
            Style::default().fg(state.theme.border).add_modifier(Modifier::DIM),
        ))), parts[1]);
        parts[2]
    } else {
        area
    };

    // V17.1: model_short / engine_status 已迁出（top_bar 独立计算并展示，避免双源不一致）
    let summaries = state.messages.iter()
        .filter_map(|m| match &m.role { crate::tui::state::MsgRole::User => Some(()), _ => None })
        .count();

    let mut expert_names: Vec<&str> = state.messages.iter()
        .filter_map(|m| match &m.role { crate::tui::state::MsgRole::Expert(n) => Some(n.as_str()), _ => None })
        .collect();
    expert_names.sort();
    expert_names.dedup();

    let mut tool_names: Vec<&str> = state.tool_records.iter().map(|r| r.name.as_str()).collect();
    tool_names.sort();
    tool_names.dedup();

    // V17: IA 重组——3 个 L1 子分块"记忆/工具/统计"
    // 设计意图: 用户认知导向分组(记住了什么/能用什么/有多少) > 旧的"模式/计量/实体..." 技术导向
    // 引用关系: 与 timeline 子分块共同构成 Tab"摘要"的 4 部分(timeline 在 render_tab_timeline)
    let mut lines: Vec<Line> = Vec::new();

    // V19: 子分块之间细分隔字符串(共享 helper) — 与 L1 标题对齐 col=1 起, 8 ╌
    let dotted_sep = Line::styled(
        " ╌╌╌╌╌╌╌╌",
        Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM),
    );

    // ════════════════════════════════════════════════════════════
    // 🧠 记忆 (L1) — 当前会话的认知上下文
    //   meta: 实体数 / 知识调用数 (两个维度的简洁聚合)
    // ════════════════════════════════════════════════════════════
    lines.push(Line::from(vec![
        Span::styled(" 🧠 记忆", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" · {}/{}", expert_names.len(), state.knowledge_calls.len()),
            Style::default().fg(state.theme.muted),
        ),
    ]));

    // 👥 激活实体 (L1.5)
    lines.push(Line::styled(
        "   👥 激活实体",
        state.theme.text_style(TextRole::BodyEmphasis),
    ));
    if !expert_names.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("    专家  ", Style::default().fg(state.theme.muted)),
            Span::styled(expert_names.join(", "), Style::default().fg(state.theme.text)),
        ]));
    } else {
        lines.push(Line::styled("    —", Style::default().fg(state.theme.muted)));
    }

    // 📚 知识 (L1.5) — V33 现场版：仅一行小计（条数 + 总次数），层级树移到「量化」tab
    // 口径：知识=knowledge_calls 实体条数；总次数=各实体 count 累加；与「量化」同源不同精度
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "   📚 知识",
        state.theme.text_style(TextRole::BodyEmphasis),
    ));
    if state.knowledge_calls.is_empty() {
        lines.push(Line::styled("    —", Style::default().fg(state.theme.muted)));
    } else {
        let total_calls: u32 = state.knowledge_calls.iter().map(|e| e.count).sum();
        lines.push(Line::from(vec![
            Span::styled("    实体 ", Style::default().fg(state.theme.muted)),
            Span::styled(format!("{}", state.knowledge_calls.len()), state.theme.text_style(TextRole::BodyEmphasis)),
            Span::styled("  调用 ", Style::default().fg(state.theme.muted)),
            Span::styled(format!("{} 次", total_calls), Style::default().fg(state.theme.text)),
            Span::styled("  · 详情见「量化」", state.theme.text_style(TextRole::Caption)),
        ]));
    }

    // V17.1: 删除 📦 可沉淀 子分块（保留意图见旧版注释）

    // ════════════════════════════════════════════════════════════
    // 🔧 工具 (L1) — 当前可调用的能力
    //   meta: 工具调用总数
    // ════════════════════════════════════════════════════════════
    lines.push(dotted_sep.clone());  // 子分块间细分隔(替代空行)
    lines.push(Line::from(vec![
        Span::styled(" 🔧 工具", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" · {}", state.tool_records.len()),
            Style::default().fg(state.theme.muted),
        ),
    ]));
    if state.tool_records.is_empty() {
        lines.push(Line::styled("   —", Style::default().fg(state.theme.muted)));
    } else {
        // V32: 口径统一
        // - 旧版混用：MCP/系统是 unique 工具数，"总计"是 calls 数 → 用户看不出来"为啥 MCP+系统 ≠ 总计"
        // - 新版：MCP/系统/合计都用 unique 数（同维度），"调用 N 次"独立成行
        // - mcp 命名约定：MCP 工具实际名是 `mcp__xxx__yyy` 双下划线，starts_with("mcp_") 已覆盖
        //   原 `starts_with("mcp.")` 是死分支（无该命名规范），删
        let mcp_count = tool_names.iter().filter(|n| n.starts_with("mcp_")).count();
        let sys_count = tool_names.len().saturating_sub(mcp_count);
        let total_unique = tool_names.len();
        let total_calls = state.tool_records.len();
        lines.push(Line::from(vec![
            Span::styled("   MCP ", Style::default().fg(state.theme.muted)),
            Span::styled(format!("{}", mcp_count), Style::default().fg(state.theme.text)),
            Span::styled("  系统 ", Style::default().fg(state.theme.muted)),
            Span::styled(format!("{}", sys_count), Style::default().fg(state.theme.text)),
            Span::styled("  种类 ", Style::default().fg(state.theme.muted)),
            Span::styled(format!("{}", total_unique), state.theme.text_style(TextRole::BodyEmphasis)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("   调用 ", Style::default().fg(state.theme.muted)),
            Span::styled(format!("{} 次", total_calls), state.theme.text_style(TextRole::BodyEmphasis)),
        ]));
    }

    // V33: 📊 统计 + 知识宫殿层级树已迁出到 render_tab_quant（量化 tab）
    // 现场 tab 只保留：实体激活 / 知识小计 / 工具小计 — 服务"跟现场"用户场景
    // 用户引导：知识区已加 "· 详情见「量化」" caption，提示去量化 tab 看完整层级
    // V33 注：summaries 计算仍保留（早期 let 绑定），让 lint 静默
    let _ = summaries;

    // 滚动截取：offset=0 auto-scroll to bottom, offset>0 向上偏移
    let visible_h = area.height as usize;
    if lines.len() > visible_h {
        let end = lines.len().saturating_sub(state.knowledge_scroll_offset);
        let start = end.saturating_sub(visible_h);
        lines = lines[start..end].to_vec();
        // 滚动指示器
        if state.knowledge_scroll_offset > 0 {
            if let Some(first) = lines.first_mut() {
                *first = Line::from(Span::styled(
                    format!(" ↓ +{} 更新", state.knowledge_scroll_offset),
                    state.theme.text_style(TextRole::Caption),
                ));
            }
        }
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// V33 「量化」tab — 复盘视角，单独 Tab 承载会话统计 + 知识宫殿全量层级树
///
/// 设计意图：把"复盘量化"用户场景从「现场」抽出来。「现场」回答"现在 Agent 在干什么"，
/// 「量化」回答"这次会话花了多少代价、查了哪些知识"——不同关注焦点，独立 Tab 承载。
///
/// 引用关系：
///   - 被 render_panel 4 mode 分支调用（PanelTab::Quant 命中）
///   - 数据源：state.turn_count / state.messages（User/Session/Expert 角色计数）/
///             state.trace_events.len() / state.session_tokens.* /
///             state.knowledge_calls（按 palace > domain > entity 三层聚合）
///   - 与 render_tab_memory 同源不同精度：知识小计 vs 全量层级树；不重复展示工具/实体
///
/// 排版口径（与现场 tab 同构，确保两 tab 视觉同源）：
///   - L1 标题 col=1（accent + BOLD）
///   - 数据行 col=4（muted label + text/emphasis value）
///   - 子分块间用 dotted_sep（8 ╌）
///   - 滚动复用 state.knowledge_scroll_offset（与现场共享同一 scroll bus）
///
/// 生命周期：每帧渲染；不持有状态
fn render_tab_quant(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    // V33 滚动支持：与现场 tab 共用 knowledge_scroll_offset
    let mut lines: Vec<Line> = Vec::new();
    let dotted_sep = Line::styled(
        " ╌╌╌╌╌╌╌╌",
        Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM),
    );

    // ════════════════════════════════════════════════════════════
    // 📊 统计 (L1) — 量化指标
    //   引用关系：
    //     - state.turn_count: 用户提交计数（state.add_message 时 +1）
    //     - summaries: messages 中 User 角色数（与 turn_count 等价但 derive 抗漂移）
    //     - state.session_tokens: run.rs 在 EngineResponse.stats 抵达时累加（含 cost_*）
    //     - state.trace_events: V28 SSOT，所有 LLM 思考/工具调用/事件
    //   维度准确性（V28.7 重构记录）：
    //     - 旧 "total" 含 system 内部消息，与"对话来回"语义不符
    //     - 新 "对话: 你 N · AI M" 让用户直观感知双方贡献
    //     - Token 拆 输入/输出/缓存命中率三行，提升对成本结构的可读性
    //     - 费用估算 USD + CNY 折算（汇率 7.2 静态，避免实时依赖）
    // ════════════════════════════════════════════════════════════
    let summaries = state.messages.iter()
        .filter(|m| matches!(m.role, crate::tui::state::MsgRole::User))
        .count();
    let ai_count = state.messages.iter()
        .filter(|m| matches!(m.role, crate::tui::state::MsgRole::Session | crate::tui::state::MsgRole::Expert(_)))
        .count();

    lines.push(Line::styled(
        " 📊 统计",
        Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::from(vec![
        Span::styled("   模式  ", Style::default().fg(state.theme.muted)),
        Span::styled(state.mode.label(), Style::default().fg(state.theme.mode).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("   轮次  ", Style::default().fg(state.theme.muted)),
        Span::styled(state.turn_count.to_string(), state.theme.text_style(TextRole::BodyEmphasis)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("   对话  ", Style::default().fg(state.theme.muted)),
        Span::styled(format!("你 {} · AI {}", summaries, ai_count), Style::default().fg(state.theme.text)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("   事件  ", Style::default().fg(state.theme.muted)),
        Span::styled(state.trace_events.len().to_string(), Style::default().fg(state.theme.text)),
    ]));

    // ── Token 子分块（仅当有数据） ──
    if state.session_tokens.total_tokens > 0 {
        let prompt = state.session_tokens.prompt_tokens;
        let completion = state.session_tokens.completion_tokens;
        let cached = state.session_tokens.cached_tokens;
        let total = state.session_tokens.total_tokens;
        let cache_hit_pct = if prompt > 0 {
            (cached as f64 / prompt as f64 * 100.0).min(100.0)
        } else {
            0.0
        };

        let mut input_spans = vec![
            Span::styled("   输入  ", Style::default().fg(state.theme.muted)),
            Span::styled(format!("{}", prompt), Style::default().fg(state.theme.text)),
        ];
        if cached > 0 {
            input_spans.push(Span::styled(
                format!(" (缓存 {} · {:.0}%)", cached, cache_hit_pct),
                state.theme.text_style(TextRole::Caption),
            ));
        }
        lines.push(Line::from(input_spans));

        // V30：输出行；如果有思考 tokens 子集（DeepSeek/OpenAI reasoning · Gemini thoughts），
        // 在同行加 "(思考 N · X%)" 透明披露——便于用户判断 thinking 模式的字节占比
        let thinking = state.session_tokens.thinking_tokens;
        let thinking_pct = if completion > 0 {
            (thinking as f64 / completion as f64 * 100.0).min(100.0)
        } else { 0.0 };
        let mut output_spans = vec![
            Span::styled("   输出  ", Style::default().fg(state.theme.muted)),
            Span::styled(format!("{}", completion), Style::default().fg(state.theme.text)),
        ];
        if thinking > 0 {
            output_spans.push(Span::styled(
                format!(" (思考 {} · {:.0}%)", thinking, thinking_pct),
                state.theme.text_style(TextRole::Caption),
            ));
        }
        lines.push(Line::from(output_spans));
        lines.push(Line::from(vec![
            Span::styled("   合计  ", Style::default().fg(state.theme.muted)),
            Span::styled(format!("{}", total), state.theme.text_style(TextRole::BodyEmphasis)),
        ]));

        // ── 费用估算（V31: ¥ 主显，$ 次显） ──
        // 设计：DeepSeek 官方按 ¥ 计费，主显 ¥ 贴近用户实际付款；$ 经 FX 现算次显
        let cost_cny = state.session_tokens.cost_cny;
        let cost_usd = state.session_tokens.cost_usd;
        lines.push(Line::from(vec![
            Span::styled("   费用  ", Style::default().fg(state.theme.muted)),
            Span::styled(crate::tui::cost::format_cny(cost_cny), Style::default().fg(state.theme.gold).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" ≈ {}", crate::tui::cost::format_usd(cost_usd)),
                state.theme.text_style(TextRole::Caption),
            ),
        ]));
    }

    // ════════════════════════════════════════════════════════════
    // V36-3: 🤖 模型分布 (L1) — per-model token + 费用 + turn 计数
    //   引用关系：state.session_tokens.per_model（run.rs 按 canonical model_id 累加）
    //   口径：按 cost_cny 倒序排列；显示 turns / total tokens / 占比 / cny
    //   设计意图：透明披露 escalation 真实开销分布（"以为用 Flash，实际跑 Pro"）
    //   显示阈值：≥1 个模型才显示
    // ════════════════════════════════════════════════════════════
    if !state.session_tokens.per_model.is_empty() {
        lines.push(dotted_sep.clone());
        let mut model_rows: Vec<(&String, &crate::tui::state::ModelTokenStats)> =
            state.session_tokens.per_model.iter().collect();
        model_rows.sort_by(|a, b| b.1.cost_cny.partial_cmp(&a.1.cost_cny).unwrap_or(std::cmp::Ordering::Equal));
        let total_cny: f64 = model_rows.iter().map(|(_, s)| s.cost_cny).sum::<f64>().max(0.0001);

        lines.push(Line::from(vec![
            Span::styled(" 🤖 模型分布", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" · {} 个模型", model_rows.len()),
                Style::default().fg(state.theme.muted),
            ),
        ]));
        for (model_id, mstats) in &model_rows {
            let pct = (mstats.cost_cny / total_cny * 100.0).round() as u32;
            let bar_w = ((pct as usize) * 16 / 100).max(1);
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled("█".repeat(bar_w), Style::default().fg(state.theme.gold)),
                Span::styled("░".repeat(16 - bar_w), Style::default().fg(state.theme.muted)),
                Span::styled(format!(" {:>3}% ", pct), state.theme.text_style(TextRole::BodyEmphasis)),
                Span::styled(model_id.as_str(), Style::default().fg(state.theme.accent)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("       ", Style::default()),
                Span::styled(
                    format!(
                        "{} 轮 · 输入 {} · 输出 {} · ",
                        mstats.turns,
                        mstats.prompt,
                        mstats.completion,
                    ),
                    state.theme.text_style(TextRole::Caption),
                ),
                Span::styled(
                    crate::tui::cost::format_cny(mstats.cost_cny),
                    Style::default().fg(state.theme.gold),
                ),
            ]));
        }
    }

    // ════════════════════════════════════════════════════════════
    // V39-4: 🎭 模式分布 (L1) — per-mode token + 费用 + turn 计数
    //   引用关系：state.session_tokens.per_mode（run.rs 按 state.mode.label() 累加）
    //   口径：按 cost_cny 倒序；显示 turns / total tokens / 占比 / cny
    //   设计意图：关注"在哪个会话阶段花费"（与 per_model 的"用哪个 LLM"正交）
    //   显示阈值：≥1 个 mode 才显示
    // ════════════════════════════════════════════════════════════
    if !state.session_tokens.per_mode.is_empty() {
        lines.push(dotted_sep.clone());
        let mut mode_rows: Vec<(&String, &crate::tui::state::ModelTokenStats)> =
            state.session_tokens.per_mode.iter().collect();
        mode_rows.sort_by(|a, b| b.1.cost_cny.partial_cmp(&a.1.cost_cny).unwrap_or(std::cmp::Ordering::Equal));
        let total_cny: f64 = mode_rows.iter().map(|(_, s)| s.cost_cny).sum::<f64>().max(0.0001);

        lines.push(Line::from(vec![
            Span::styled(" 🎭 模式分布", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" · {} 个阶段", mode_rows.len()),
                Style::default().fg(state.theme.muted),
            ),
        ]));
        for (mode_label, mstats) in &mode_rows {
            let pct = (mstats.cost_cny / total_cny * 100.0).round() as u32;
            let bar_w = ((pct as usize) * 16 / 100).max(1);
            // 中文 label 映射（per_mode key 来自 AbacusMode::label() 返回值，是小写）
            let zh = match mode_label.as_str() {
                "clarify" => "澄清",
                "meeting" => "会诊",
                "plan" => "规划",
                "team" => "执行",
                _ => mode_label.as_str(),
            };
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled("█".repeat(bar_w), Style::default().fg(state.theme.mode)),
                Span::styled("░".repeat(16 - bar_w), Style::default().fg(state.theme.muted)),
                Span::styled(format!(" {:>3}% ", pct), state.theme.text_style(TextRole::BodyEmphasis)),
                Span::styled(zh, Style::default().fg(state.theme.accent)),
                Span::styled(format!(" ({})", mode_label), state.theme.text_style(TextRole::Caption)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("       ", Style::default()),
                Span::styled(
                    format!("{} 轮 · 输入 {} · 输出 {} · ", mstats.turns, mstats.prompt, mstats.completion),
                    state.theme.text_style(TextRole::Caption),
                ),
                Span::styled(
                    crate::tui::cost::format_cny(mstats.cost_cny),
                    Style::default().fg(state.theme.gold),
                ),
            ]));
        }

        // V41-3: 占比警告 — 单一 mode 占总成本 ≥ 80% + 总成本 ≥ ¥1 时提示
        // 设计意图：检测"卡在某阶段"信号（澄清反复 / 执行重复 / 规划循环）
        // 阈值叠加：单纯 ratio 不够（首次进入即 100%）；total ≥ ¥1 确保有实质投入
        // 排除当前 mode：如果当前正在该阶段，不警告（用户主动选择）
        if total_cny >= 1.0 {
            if let Some((dominant_label, dominant_stats)) = mode_rows.first() {
                let dominant_pct = dominant_stats.cost_cny / total_cny;
                let cur_label = state.mode.label();
                if dominant_pct >= 0.80 && dominant_label.as_str() != cur_label {
                    let zh = match dominant_label.as_str() {
                        "clarify" => "澄清",
                        "meeting" => "会诊",
                        "plan" => "规划",
                        "team" => "执行",
                        _ => dominant_label.as_str(),
                    };
                    lines.push(Line::raw(""));
                    lines.push(Line::from(vec![
                        Span::styled("    ⚠ ", Style::default().fg(state.theme.semantic_fg(SemanticIntent::Warning)).add_modifier(Modifier::BOLD)),
                        Span::styled(
                            format!("{} 阶段占比 {:.0}%（{}）", zh, dominant_pct * 100.0, crate::tui::cost::format_cny(dominant_stats.cost_cny)),
                            state.theme.text_style(TextRole::BodyEmphasis),
                        ),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("      ", Style::default()),
                        Span::styled(
                            "可能卡在该阶段，考虑 /done 推进到下一步",
                            Style::default().fg(state.theme.muted),
                        ),
                    ]));
                }
            }
        }
    }

    // ════════════════════════════════════════════════════════════
    // V35-3: 🛠 工具调用频次 (L1) — top 5 + 横向条形
    //   引用关系：state.trace_events.kind == TraceKind::ToolCall（V28 SSOT）
    //   口径：按 name 聚合，按总次数排序；条形宽度按当前面板剩余空间归一化
    //   设计意图：让用户一眼看到"哪些工具被高频调用"，识别成本/效率热点
    //   生命周期：每帧重算，不缓存
    // ════════════════════════════════════════════════════════════
    {
        use std::collections::HashMap;
        let mut tool_counts: HashMap<&str, u32> = HashMap::new();
        let mut tool_failures: HashMap<&str, u32> = HashMap::new();
        for ev in &state.trace_events {
            if let crate::tui::state::TraceKind::ToolCall { name, status, .. } = &ev.kind {
                *tool_counts.entry(name.as_str()).or_insert(0) += 1;
                if matches!(status, crate::tui::state::ToolStatus::Failed) {
                    *tool_failures.entry(name.as_str()).or_insert(0) += 1;
                }
            }
        }
        if !tool_counts.is_empty() {
            lines.push(dotted_sep.clone());
            let mut sorted: Vec<(&str, u32)> = tool_counts.iter().map(|(k, v)| (*k, *v)).collect();
            sorted.sort_by_key(|x| std::cmp::Reverse(x.1));
            let total_calls: u32 = sorted.iter().map(|x| x.1).sum();
            let top5 = sorted.iter().take(5).cloned().collect::<Vec<_>>();
            let max_count = top5.first().map(|x| x.1).unwrap_or(1).max(1);

            lines.push(Line::from(vec![
                Span::styled(" 🛠 工具调用", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!(" · {} 次 · {} 种", total_calls, sorted.len()),
                    Style::default().fg(state.theme.muted),
                ),
            ]));
            for (name, count) in &top5 {
                let bar_w = ((*count as usize) * 16 / max_count as usize).max(1);
                let fail = tool_failures.get(name).copied().unwrap_or(0);
                let mut spans = vec![
                    Span::styled("    ", Style::default()),
                    Span::styled("█".repeat(bar_w), Style::default().fg(state.theme.accent)),
                    Span::styled("░".repeat(16 - bar_w), Style::default().fg(state.theme.muted)),
                    Span::styled(format!(" {:>3} ", count), state.theme.text_style(TextRole::BodyEmphasis)),
                    Span::styled(*name, Style::default().fg(state.theme.text)),
                ];
                if fail > 0 {
                    spans.push(Span::styled(
                        format!(" · 失败 {}", fail),
                        Style::default().fg(state.theme.error),
                    ));
                }
                lines.push(Line::from(spans));
            }
            if sorted.len() > 5 {
                lines.push(Line::from(vec![
                    Span::styled(format!("    +{} 种工具", sorted.len() - 5), state.theme.text_style(TextRole::Caption)),
                ]));
            }

            // V38-3: 失败率排序次区块 — 暴露最不靠谱的工具
            // 引用关系：tool_counts / tool_failures（已在上方扫描得到）
            // 触发阈值：≥3 次调用 + 失败率 > 20%；过低样本噪声大不进榜
            // 显示上限：top 3，避免过多干扰
            // 设计意图：高频与高失败率是两种独立维度——前者帮诊断"哪些值得优化"，后者帮诊断"哪些得修复"
            //
            // V39-3 注释（cli ↔ core 两层信号协作）：
            //   本区块基于 cli 端 trace_events 重算（即时、本会话）—— **诊断视图**
            //   abacus-core::tool::effectiveness::record_invocation 同步累积（自动、跨会话）—— **决策机制**
            //     core 的 evaluate() 自动算 tier，超阈值时通过 palace_demoted 强制 D tier
            //     cli 看到的"⚠ 失败率高"工具，core 大概率已自动降级（visibility threshold 截断）
            //   两层独立运转：cli 帮用户看到现象，core 自动消化决策；不需要 cli 主动调 core API
            //   未来若需在此处显示 core tier 标志，需通过 EngineHandle 异步查 effectiveness（render 同步上下文不便）
            let mut bad_tools: Vec<(&str, u32, u32, f64)> = tool_counts.iter()
                .filter_map(|(name, count)| {
                    let fail = tool_failures.get(*name).copied().unwrap_or(0);
                    if *count >= 3 && fail > 0 {
                        let rate = fail as f64 / *count as f64;
                        if rate > 0.20 {
                            Some((*name, *count, fail, rate))
                        } else { None }
                    } else { None }
                })
                .collect();
            if !bad_tools.is_empty() {
                bad_tools.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
                let top3 = &bad_tools[..bad_tools.len().min(3)];
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::styled("    ⚠ 失败率高 ", Style::default().fg(state.theme.error).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("({} 个工具)", bad_tools.len()), Style::default().fg(state.theme.muted)),
                ]));
                for (name, count, fail, rate) in top3 {
                    let pct = (*rate * 100.0).round() as u32;
                    lines.push(Line::from(vec![
                        Span::styled("      ", Style::default()),
                        Span::styled(format!("{:>3}%", pct), Style::default().fg(state.theme.error).add_modifier(Modifier::BOLD)),
                        Span::styled(format!(" ({}/{}) ", fail, count), state.theme.text_style(TextRole::Caption)),
                        Span::styled(*name, Style::default().fg(state.theme.text)),
                    ]));
                }
            }
        }
    }

    // ════════════════════════════════════════════════════════════
    // V35-3: 📈 轮次趋势 (L1) — 每轮 reply tokens sparkline
    //   引用关系：state.trace_events.kind == TraceKind::Reply { tokens }
    //   口径：按 trace 顺序取 tokens，归一化到 0..7 索引到 ▁▂▃▄▅▆▇█ 字符
    //   设计意图：一行字符让用户感知 token 消耗变化趋势（早期重 / 后期重 / 持平）
    //   显示阈值：≥2 个 reply 才显示（单点无趋势可言）
    // ════════════════════════════════════════════════════════════
    {
        let replies: Vec<u32> = state.trace_events.iter().filter_map(|ev| {
            if let crate::tui::state::TraceKind::Reply { tokens } = &ev.kind {
                Some(*tokens)
            } else { None }
        }).collect();
        if replies.len() >= 2 {
            const BARS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
            let max = *replies.iter().max().unwrap_or(&1).max(&1);
            let sparkline: String = replies.iter().map(|t| {
                let idx = ((*t as usize) * 7 / max as usize).min(7);
                BARS[idx]
            }).collect();
            let avg = replies.iter().sum::<u32>() / replies.len() as u32;
            lines.push(dotted_sep.clone());
            lines.push(Line::from(vec![
                Span::styled(" 📈 轮次趋势", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!(" · {} 轮 · 均 {} tok", replies.len(), avg),
                    Style::default().fg(state.theme.muted),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled(sparkline, Style::default().fg(state.theme.success)),
                Span::styled(format!(" max {}", max), Style::default().fg(state.theme.muted)),
            ]));
        }
    }

    // ════════════════════════════════════════════════════════════
    // 📚 知识宫殿 (L1) — 全量层级树 palace > domain > entity (top 5)
    //   口径：count = 各 entry.count 累加（"调用次数"），与现场 tab 知识小计同源
    //   引用关系：state.knowledge_calls 由 run.rs 在工具调用结束时累加
    // ════════════════════════════════════════════════════════════
    if !state.knowledge_calls.is_empty() {
        lines.push(dotted_sep.clone());

        use std::collections::BTreeMap;
        struct DomainGroup {
            count: u32,
            entities: Vec<(String, u32)>,
        }
        let mut palaces: BTreeMap<&str, BTreeMap<&str, DomainGroup>> = BTreeMap::new();
        let mut total_calls: u32 = 0;
        for entry in &state.knowledge_calls {
            total_calls += entry.count;
            let domain_map = palaces.entry(entry.palace.as_str()).or_default();
            let group = domain_map.entry(entry.domain.as_str()).or_insert_with(|| DomainGroup {
                count: 0,
                entities: Vec::new(),
            });
            group.count += entry.count;
            group.entities.push((entry.entity.clone(), entry.count));
        }

        lines.push(Line::from(vec![
            Span::styled(" 📚 知识宫殿", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" · {} 次 · {} 实体", total_calls, state.knowledge_calls.len()),
                Style::default().fg(state.theme.muted),
            ),
        ]));

        for (palace, domains) in &palaces {
            let palace_total: u32 = domains.values().map(|g| g.count).sum();
            lines.push(Line::from(vec![
                Span::styled(format!("    ▸ {} ", palace), Style::default().fg(state.theme.gold).add_modifier(Modifier::BOLD)),
                Span::styled(format!("({}次)", palace_total), Style::default().fg(state.theme.muted)),
            ]));
            for (domain, group) in domains {
                lines.push(Line::from(vec![
                    Span::styled(format!("      ▸ {} ", domain), Style::default().fg(state.theme.accent)),
                    Span::styled(format!("×{}", group.count), Style::default().fg(state.theme.muted)),
                ]));
                let mut sorted_entities = group.entities.clone();
                sorted_entities.sort_by_key(|b| std::cmp::Reverse(b.1));
                let show_count = sorted_entities.len().min(5);
                for (entity, count) in &sorted_entities[..show_count] {
                    lines.push(Line::from(vec![
                        Span::styled(format!("        {} ", entity), Style::default().fg(state.theme.text)),
                        Span::styled(format!("×{}", count), state.theme.text_style(TextRole::Caption)),
                    ]));
                }
                if sorted_entities.len() > 5 {
                    lines.push(Line::from(vec![
                        Span::styled(format!("        +{} more", sorted_entities.len() - 5), state.theme.text_style(TextRole::Caption)),
                    ]));
                }
            }
        }
    }

    // 滚动截取：与 render_tab_memory 同套机制（offset=0 自动贴底，offset>0 上滚）
    let visible_h = area.height as usize;
    if lines.len() > visible_h {
        let end = lines.len().saturating_sub(state.knowledge_scroll_offset);
        let start = end.saturating_sub(visible_h);
        lines = lines[start..end].to_vec();
        if state.knowledge_scroll_offset > 0 {
            if let Some(first) = lines.first_mut() {
                *first = Line::from(Span::styled(
                    format!(" ↓ +{} 更新", state.knowledge_scroll_offset),
                    state.theme.text_style(TextRole::Caption),
                ));
            }
        }
    }

    f.render_widget(Paragraph::new(lines), area);
}

// V33 已删除：mini_bar — 仅 render_tab_components 调用，组件 tab 下线后无 callsite。
//   未来需要图表型 helper 时按当时 theme/语义重新设计，不复用此版本。

// V33 已删除：render_tab_components / render_tab_tasks / render_task_kanban_inner
//   原因：PanelTab::Components 已从 enum 中移除（V33 场景化拆分），三函数都成 0 callsite 死代码。
//   工具/技能展示已简化迁移到「现场」tab 的 🔧 工具 子分块（render_tab_memory 内）；
//   任务/专家看板由 render_panel_team_board / render_panel_meeting_agenda 接管（同一渲染路径，
//   不再需要专用 Tasks tab 路由层）。
//   若未来需要"组件详情专用 tab"，重新设计场景边界后从头实装，不复用历史 placeholder。
//
// V33 注：旧 V34 占位 render_tab_quant（仅 token/cost 概要）也已删除——新版（render_tab_quant
//   行 ~4133）口径与「现场」tab 同源对齐，含完整 📊 统计 + 知识宫殿层级树。

// ════════════════════════════════════════════════════════════════
// ExpertList — 专家列表 (Meeting / Team 共用)
// ════════════════════════════════════════════════════════════════

pub fn render_expert_list(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .title(" 专家 ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(state.theme.border));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    for expert in &state.experts {
        let (status_icon, status_color) = match expert.status {
            ExpertStatus::Active => ("●", state.theme.success),
            ExpertStatus::Idle => ("◌", state.theme.muted),
            ExpertStatus::Done => ("✓", state.theme.accent),
        };

        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", status_icon),
                Style::default().fg(status_color),
            ),
            Span::styled(
                &expert.name,
                Style::default()
                    .fg(state.theme.expert)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(
                &expert.domain,
                Style::default()
                    .fg(state.theme.muted)
                    .add_modifier(Modifier::DIM),
            ),
        ]));

        // V28.7: confidence == 0.0 → "未评估"，>0 → 百分比（orchestrator 暂无评估机制时不造伪数据）
        let conf_label = if expert.confidence > 0.0 {
            format!("置信度: {:.0}%", expert.confidence * 100.0)
        } else {
            "置信度: —".to_string()
        };
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(conf_label, Style::default().fg(state.theme.muted)),
        ]));

        lines.push(Line::raw(""));
    }

    if lines.is_empty() {
        lines.push(Line::styled(
            " 暂无专家接入",
            Style::default().fg(state.theme.muted),
        ));
        lines.push(Line::styled(
            " 输入 /invite 邀请专家",
            state.theme.text_style(TextRole::Caption),
        ));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

// ════════════════════════════════════════════════════════════════
// TaskKanban — 任务看板 (Team 模式)
// ════════════════════════════════════════════════════════════════

pub fn render_task_kanban(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .title(" 任务看板 ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(state.theme.mode));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    if state.tasks.is_empty() {
        lines.push(Line::styled(
            " 暂无任务",
            Style::default().fg(state.theme.muted),
        ));
    } else {
        for task in &state.tasks {
            let (status_icon, status_color) = match task.status {
                TaskStatus::Pending => ("◌", state.theme.muted),
                TaskStatus::InProgress => ("●", state.theme.success),
                TaskStatus::Done => ("✓", state.theme.accent),
                TaskStatus::Blocked => ("⚠", state.theme.error),
            };

            let progress_filled = (task.progress as usize * 10 / 100).min(10);
            let progress_empty = 10 - progress_filled;
            let progress_text = format!("{}%", task.progress);

            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", status_icon),
                    Style::default().fg(status_color),
                ),
                Span::styled(
                    &task.title,
                    Style::default()
                        .fg(state.theme.text)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));

            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    format!("负责人: {}", task.assignee),
                    Style::default().fg(state.theme.muted),
                ),
                Span::raw("  ["),
                Span::styled(
                    "█".repeat(progress_filled),
                    Style::default().fg(state.theme.accent),
                ),
                Span::styled(
                    "░".repeat(progress_empty),
                    Style::default().fg(state.theme.muted),
                ),
                Span::styled(
                    format!("] {}", progress_text),
                    Style::default().fg(state.theme.muted),
                ),
            ]));

            if !task.deps.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(
                        format!("依赖: {}", task.deps.join(", ")),
                        Style::default()
                            .fg(state.theme.muted)
                            .add_modifier(Modifier::DIM),
                    ),
                ]));
            }

            lines.push(Line::raw(""));
        }
    }

    f.render_widget(Paragraph::new(lines), inner);
}

// ════════════════════════════════════════════════════════════════
// 全局背景渲染
// ════════════════════════════════════════════════════════════════

pub fn render_global_background(f: &mut ratatui::Frame, state: &AppState) {
    // 直接设置 buffer 背景色（零分配，比 Paragraph 快 10x）
    let area = f.area();
    let bg = state.theme.bg;
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut f.buffer_mut()[(x, y)];
            cell.set_symbol(" ");
            cell.set_bg(bg);
            cell.set_fg(state.theme.text);
        }
    }
}

// ════════════════════════════════════════════════════════════════
/// 命令行提示面板（输入框右侧，看板打开时显示，圆角边框，支持滚动）
pub fn render_shortcuts_hints(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    if area.width < 14 || area.height < 4 {
        return;
    }

    // V26: 焦点反馈对称化——上边框 primary, 其他三边 border (与 panel 统一)
    let is_focused = state.focus == crate::tui::state::Focus::CommandHint;
    let block = Block::default()
        .border_type(BorderType::Rounded)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(state.theme.border));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // V26.1: 同 panel — 缩小 area 避免覆盖角字符 ╭╮
    if is_focused && area.width >= 3 {
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
        // V28.6 (PR12-1 续): 与 panel 焦点反馈对称, 上边框粗线 ━ 与看板内 ┃ 色条同属
        //   box drawings heavy 家族, 字符粗细 + primary 色一致, 视觉锚点统一
        let top_overlay = Block::default()
            .borders(Borders::TOP)
            .border_type(BorderType::Thick)
            .border_style(top_style);
        f.render_widget(top_overlay, top_segment);
    }

    let all_commands = &state.commands;

    let cmd_style = Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(state.theme.muted);
    let sep_style = Style::default().fg(state.theme.border);
    // V13: 选中行用 accent 反色 + BOLD（仅焦点时高亮）
    let selected_cmd_style = Style::default()
        .fg(state.theme.bg)
        .bg(state.theme.accent)
        .add_modifier(Modifier::BOLD);
    let selected_desc_style = Style::default()
        .fg(state.theme.bg)
        .bg(state.theme.accent);

    // 计算可见行数（内框高度 - 标题行 - 底部状态行 = 内容行数）
    let content_rows = inner.height.saturating_sub(2) as usize;
    if content_rows == 0 {
        return;
    }

    // 自适应布局：双列（宽≥22）/ 单列（窄屏）
    let is_wide = inner.width >= 22;
    let cols_per_row = if is_wide { 2 } else { 1 };
    let total_rows = (all_commands.len() + cols_per_row - 1) / cols_per_row;
    let max_scroll = total_rows.saturating_sub(content_rows);
    let scroll = state.cmd_scroll.min(max_scroll);

    let mut lines: Vec<Line> = Vec::new();

    // V13: 标题加焦点提示
    let title = if is_focused {
        " ⌘ 命令 (↑↓ 选择 · Enter 填充 · 点击直填) "
    } else {
        " ⌘ 可用命令 (↑↓ 自动聚焦 · 点击直填) "
    };
    lines.push(Line::from(vec![
        Span::styled(title.to_string(), Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
    ]));

    let sel = state.cmd_selected.min(all_commands.len().saturating_sub(1));

    // V28.7: ── 双列对齐计算（修复上下行 │ 分界不齐）──
    // 引用关系：
    //   - 输入：inner.width（外层 block.inner 已减边框）、is_wide（是否双列）
    //   - 输出：left_col_w 固定左列总显示宽度
    //   - 分界 "  │  " 显示宽度恒为 5（│ 是单列字符）
    // 设计：左列 cmd_part(▶ /xxx) + " " + desc 按 display_width 截断+补白到 left_col_w，
    //   保证每行进入分界 │ 之前的视觉宽度恒等→分界自然对齐
    use crate::tui::util::{display_width, truncate_to_width};
    const SEP_DISPLAY_WIDTH: usize = 5; // "  │  "
    let inner_w = inner.width as usize;
    let left_col_w: usize = if is_wide {
        // 双列：减分界宽度后均分，左列略宽 1 列吸收奇数余数
        let remain = inner_w.saturating_sub(SEP_DISPLAY_WIDTH);
        remain.saturating_sub(remain / 2).max(8)
    } else {
        inner_w
    };

    // 把 (cmd, desc, is_sel) 渲染为 [cmd Span, desc Span, padding Span]，三段累计 display_width == col_w
    // padding 用 Span::raw 不染色——避免选中行的 bg 染到尾部空白产生视觉拖尾
    let render_cell = |cmd: &str, desc: &str, marker: &str, is_sel: bool, col_w: usize| -> Vec<Span<'static>> {
        let cmd_part = format!("{} {}", marker, cmd);
        let cmd_w = display_width(&cmd_part);
        // desc 可用宽度 = col_w - cmd_w - 1（cmd 与 desc 之间的空格）
        let desc_max = col_w.saturating_sub(cmd_w + 1);
        let desc_text = if display_width(desc) > desc_max {
            // 截断 + 省略号（… 占 1 列）
            let cut = desc_max.saturating_sub(1).max(1);
            let mut t = truncate_to_width(desc, cut);
            t.push('…');
            t
        } else {
            desc.to_string()
        };
        let used_w = cmd_w + 1 + display_width(&desc_text);
        let pad_w = col_w.saturating_sub(used_w);
        let (cs, ds) = if is_sel { (selected_cmd_style, selected_desc_style) } else { (cmd_style, desc_style) };
        let mut out = vec![
            Span::styled(cmd_part, cs),
            Span::styled(format!(" {}", desc_text), ds),
        ];
        if pad_w > 0 {
            out.push(Span::raw(" ".repeat(pad_w)));
        }
        out
    };

    // V32 · 填充 cmd_row_map：每个内容行的屏幕 y 与起始 cmd_idx 关系，给鼠标点击反查用
    // 渲染前清空旧映射避免悬挂；inner.y 已是 block 内框起点，加偏移得到屏幕 y
    {
        let mut m = state.cmd_row_map.borrow_mut();
        m.clear();
    }
    // 标题行已 push，所以内容行从 inner.y + 1 开始
    let content_y_base = inner.y + 1;

    // 内容行（自适应双列/单列）
    for row_idx in 0..content_rows {
        let cmd_idx = (scroll + row_idx) * cols_per_row;
        if cmd_idx >= all_commands.len() {
            break;
        }
        // 行 row_idx 的屏幕 y
        let screen_y = content_y_base.saturating_add(row_idx as u16);
        state.cmd_row_map.borrow_mut().push((screen_y, cmd_idx));

        let (cmd1, desc1) = &all_commands[cmd_idx];
        let is_sel1 = is_focused && cmd_idx == sel;
        let marker1 = if is_sel1 { "▶" } else { " " };
        let mut spans: Vec<Span<'static>> = render_cell(cmd1, desc1, marker1, is_sel1, left_col_w);

        if is_wide && cmd_idx + 1 < all_commands.len() {
            let (cmd2, desc2) = &all_commands[cmd_idx + 1];
            let is_sel2 = is_focused && cmd_idx + 1 == sel;
            let marker2 = if is_sel2 { "▶" } else { " " };
            spans.push(Span::styled("  │  ", sep_style));
            // 右列宽度：剩余可用 = inner_w - left_col_w - sep_width
            let right_col_w = inner_w.saturating_sub(left_col_w + SEP_DISPLAY_WIDTH);
            spans.extend(render_cell(cmd2, desc2, marker2, is_sel2, right_col_w));
        } else if is_wide {
            // 双列模式但右列无内容（命令总数为奇数最后一行）：补满左列即可，
            // 不画分界——避免 "│" 后悬空看着突兀。render_cell 已 padding 到 left_col_w。
        }

        lines.push(Line::from(spans));
    }

    // 底部状态行（V13: 含选中位置）
    let visible_count = content_rows * cols_per_row;
    let status_text = if is_focused {
        format!(" {} / {} 选中 · Enter 填充输入 ", sel + 1, all_commands.len())
    } else if all_commands.len() > visible_count {
        format!(" ↑↓滚动 · {}/{} ", scroll + 1, max_scroll + 1)
    } else {
        format!(" 共 {} 条 ｜ 全部可见 ", all_commands.len())
    };
    lines.push(Line::from(vec![
        Span::styled(status_text, state.theme.text_style(TextRole::Caption)),
    ]));

    f.render_widget(Paragraph::new(lines), inner);
}

// ─── V29.11 (T-DIFF): 编辑工具 diff 视图回归 ──────────────────
#[cfg(test)]
mod tool_diff_render_tests {
    //! 不变量:
    //! - Edit/Write 等白名单工具 → Some(lines)
    //! - 非编辑工具 (read_file/grep) → None (走默认 JSON pretty 路径)
    //! - args 非合法 JSON → None (容错降级)
    //! - 缺关键字段(空 old/new) → 仍渲染头行 + 空 diff(不 panic)
    use super::*;
    use crate::tui::theme::Theme;

    fn theme() -> Theme { Theme::brand() }

    #[test]
    fn fs_edit_renders_diff() {
        // abacus filengine 核心编辑工具 — schema.name 直接为 "fs_edit"（统一命名后无 sanitize 中间层）
        let args = r#"{"path": "/tmp/x.rs", "old_string": "let x = 1;\nlet y = 2;", "new_string": "let x = 10;\nlet y = 20;"}"#;
        let result = try_render_edit_diff("fs_edit", args, &theme(), 0);
        assert!(result.is_some(), "fs_edit 应触发 diff 视图");
        let lines = result.unwrap();
        // 头行(📝 path) + 2 旧行 + 2 新行 + footer = 6
        assert_eq!(lines.len(), 6);
    }

    #[test]
    fn fs_write_renders_full_new_as_added() {
        // schema.name 直接为 "fs_write"（统一命名后无 sanitize 中间层）
        let args = r#"{"path": "/tmp/y.rs", "content": "fn main() {}\n// new file"}"#;
        let result = try_render_edit_diff("fs_write", args, &theme(), 0);
        assert!(result.is_some());
        let lines = result.unwrap();
        // 头行 + 0 旧行 + 2 新行 + footer = 4
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn fully_qualified_filengine_prefix_matches() {
        // 单一命名约定后：注册名直接是 filengine_fs_edit / filengine_fs_write
        let args = r#"{"path": "/tmp/x.rs", "old_string": "a", "new_string": "b"}"#;
        assert!(try_render_edit_diff("filengine_fs_edit", args, &theme(), 0).is_some());
        let args_w = r#"{"path": "/tmp/y.rs", "content": "c"}"#;
        assert!(try_render_edit_diff("filengine_fs_write", args_w, &theme(), 0).is_some());
    }

    #[test]
    fn non_edit_tool_returns_none() {
        let args = r#"{"path": "/tmp/x.rs", "content": "anything"}"#;
        // abacus 非编辑工具
        assert!(try_render_edit_diff("fs_read", args, &theme(), 0).is_none());
        assert!(try_render_edit_diff("fs_grep", args, &theme(), 0).is_none());
        assert!(try_render_edit_diff("bash_exec", args, &theme(), 0).is_none());
        assert!(try_render_edit_diff("web_fetch", args, &theme(), 0).is_none());
    }

    #[test]
    fn malformed_json_returns_none() {
        // 非合法 JSON → 容错降级到默认渲染
        assert!(try_render_edit_diff("fs_edit", "not-json", &theme(), 0).is_none());
        assert!(try_render_edit_diff("fs_edit", "", &theme(), 0).is_none());
    }

    #[test]
    fn max_lines_caps_diff_with_lcs_truncation() {
        // LCS diff: 旧 4 行 / 新 6 行 — 完全不同内容, similar 产出 4 Delete + 6 Insert = 10 ops
        // max_total_lines=4 → 取前 4 行 + 省略提示(1) + footer(1) = 6
        // 加上头行(📝 path) = 7
        let old: String = (0..4).map(|i| format!("o{}\n", i)).collect();
        let new: String = (0..6).map(|i| format!("n{}\n", i)).collect();
        let args = serde_json::json!({
            "path": "/tmp/z.rs",
            "old_string": old.trim_end(),
            "new_string": new.trim_end(),
        }).to_string();
        let result = try_render_edit_diff("fs_edit", &args, &theme(), 4);
        assert!(result.is_some());
        let lines = result.unwrap();
        // 头行(1) + 4 diff 行 + 省略(1) + footer(1) = 7
        assert_eq!(lines.len(), 7);
    }

    #[test]
    fn empty_old_string_only_renders_new_as_added() {
        // fs_edit 时 old_string 空字符串 (新建场景, 比如往空文件写)
        let args = r#"{"path": "/tmp/x.rs", "old_string": "", "new_string": "fn main() {}"}"#;
        let result = try_render_edit_diff("fs_edit", args, &theme(), 0);
        assert!(result.is_some());
        let lines = result.unwrap();
        // 头行 + 1 新行 + footer = 3 (无旧行)
        assert_eq!(lines.len(), 3);
    }
}
