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

mod bars;
pub(crate) mod block_detail;
mod extras;
mod overlays;
mod panel;
pub use bars::*;
use block_detail::*;
pub use extras::*;
pub use overlays::*;
pub use panel::render_panel;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListDirection, Paragraph, Widget};

use crate::tui::effects;
use crate::tui::markdown::{self, LineType};
use crate::tui::state::{
    AppState, BlockKind, Focus, MsgContent, MsgRole,
};
use crate::tui::theme::{TextRole, Theme};

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
    messages: &std::collections::VecDeque<crate::tui::state::Message>,
    scroll: usize,
    theme: &Theme,
    selection: &Option<crate::tui::state::TextSelection>,
    max_width: u16,
    stream_cursor: usize,
    compact: bool,
    code_blocks_expanded: bool,
    trace_events: &[crate::tui::state::TraceEvent],
    trace_event_index: &std::collections::HashMap<u64, usize>,
    trace_part_positions: &mut Vec<(usize, usize, usize)>,
    // V28.4: focused event 锚点 — 双视图同步高亮该 event（消息侧 Trace 子块加 bg）
    // 引用关系：被 line ~290 处 `focused_event_id == Some(*id)` 用于 is_focused 判定
    focused_event_id: Option<u64>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    // 排版布局：参考英文 CLI 工具风格
    // 色条 "│" + 1 空格 + 内容（简洁留白，不过度缩进）
    let bar_indent = 2usize; // │ + 1空格
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
            Span::raw(" "),
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

        // ── 消息间空行分隔（首条消息除外）──
        if visible_idx > 0 {
            lines.push(Line::raw(""));
        }

        // ── 色条（贯穿该消息所有行）── ▎ 稍粗，突出消息边界
        let bar = Span::styled("▎", Style::default().fg(role_color));

        // ── Header: icon name (左对齐) + time (右对齐) ──
        // 名字用 role_color + BOLD；时间右侧贴边，中间空格填充
        let display_name = match &msg.role {
            MsgRole::User => "You",
            MsgRole::Session => "Abacus",
            MsgRole::Expert(name) => name.as_str(),
        };
        let badge_text = format!("{} {}", role_icon, display_name);
        let badge = Span::styled(
            badge_text.clone(),
            Style::default().fg(role_color).add_modifier(Modifier::BOLD),
        );
        let time_text = msg.time.clone();
        let ts = Span::styled(
            time_text.clone(),
            theme.text_style(TextRole::Caption),
        );
        // 计算填充空格：content_width - badge_len - time_len - 1(bar后空格)
        let badge_w = crate::tui::util::display_width(&badge_text);
        let time_w = crate::tui::util::display_width(&time_text);
        let header_gap = content_width.saturating_sub(badge_w + time_w + 1);
        lines.push(Line::from(vec![
            bar.clone(),
            Span::raw(" "),
            badge,
            Span::raw(" ".repeat(header_gap)),
            ts,
        ]));
        // header 后空一行再开始内容
        lines.push(Line::from(vec![bar.clone()]));

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
                                        Span::raw(" "),
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
                        if line_w <= content_width {
                            lines.push(rline);
                        } else {
                            // 超宽行需要拆分：提取纯文本内容并 word-wrap
                            // 色条+缩进 由 styled_line_to_ratatui 已添加在前两个 span
                            // 实际文本从第 2 个 span 之后开始
                            let indent_str = match styled.line_type {
                                LineType::Code => "  ",
                                _ => " ",
                            };
                            // 合并所有内容 span 的文本
                            let full_text: String = styled.spans.iter().map(|s| s.text.as_str()).collect();
                            let text_style = styled.spans.first()
                                .map(|s| s.style)
                                .unwrap_or(Style::default().fg(theme.text));
                            // word-wrap: 统一调用 util::word_wrap_segments
                            let segments = crate::tui::util::word_wrap_segments(&full_text, content_width);
                            for (seg_start, seg_end) in segments {
                                lines.push(Line::from(vec![
                                    bar.clone(),
                                    Span::raw(indent_str.to_string()),
                                    Span::styled(full_text[seg_start..seg_end].to_string(), text_style),
                                ]));
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
                        Span::raw(" "),
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
                        .filter_map(|id| trace_event_index.get(id).and_then(|&i| trace_events.get(i)))
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
                        Span::raw(" "),
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
                        let runs = group_consecutive_tool_runs(event_ids, trace_events, trace_event_index);

                        for run in &runs {
                            let event_start = lines.len();
                            // 合并 run 的 focused 判断: 任一子 event 被 focus 则整组高亮
                            let is_focused = run.iter().any(|id| focused_event_id == Some(*id));

                            if run.len() == 1 {
                                // ── 单条: 原样渲染 ──
                                let id = &run[0];
                                let ev = trace_event_index.get(id).and_then(|&i| trace_events.get(i));
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
                                    run, trace_events, trace_event_index, &bar, theme,
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
        let bar = Span::styled("▎", Style::default().fg(theme.session));

        // Breathing space
        lines.push(Line::raw(""));

        // Header: ┃ 🤖 Abacus     输出中  · now
        // stream_cursor > 0 意味着 TextDelta 已到达，状态固定为"输出中"
        let badge_text = "🤖 Abacus";
        let badge = Span::styled(
            badge_text,
            Style::default().fg(theme.session).add_modifier(Modifier::BOLD),
        );
        let status_badge = Span::styled("输出中", Style::default().fg(theme.success));
        let time_text = " · now";
        let ts = Span::styled(
            time_text,
            theme.text_style(TextRole::Caption),
        );
        let badge_w = crate::tui::util::display_width(badge_text);
        let status_w = crate::tui::util::display_width("输出中");
        let time_w = crate::tui::util::display_width(time_text);
        let hdr_content_w = (max_width as usize).saturating_sub(4);
        let header_gap = hdr_content_w.saturating_sub(badge_w + status_w + time_w + 2);
        lines.push(Line::from(vec![
            bar.clone(), Span::raw(" "), badge,
            Span::raw(" ".repeat(header_gap)),
            status_badge, Span::raw("  "), ts,
        ]));

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
                Span::raw(" "),
                Span::styled("▌", Style::default().fg(theme.session)),
            ]));
        } else {
            lines.push(Line::from(vec![bar, Span::raw(" ")]));
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
    messages: &std::collections::VecDeque<crate::tui::state::Message>,
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
    messages: &std::collections::VecDeque<crate::tui::state::Message>,
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
// TopBar — 顶部栏 (Logo + Session + Model + 模式标识)
// ════════════════════════════════════════════════════════════════


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
        &state.trace_events, &state.trace_event_index, &mut _trace_pos_unused, state.focused_event_id,
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

    // V40: 分区渲染 — streaming 期间复用 base lines，仅重建流式尾部
    //
    // 渲染策略分三级：
    //   L0: 完全无变化 → 复用 cached_lines（原有逻辑）
    //   L1: 仅 streaming 内容变化 → 复用 cached_base_lines + 重建 streaming appendix
    //   L2: base 消息变化 → 全量 rebuild
    //
    // 引用关系：
    //   - cached_base_lines: 存储 build_message_lines 对历史消息的结果
    //   - cached_base_msg_count: 上次缓存时的 messages.len()
    //   - streaming_content_dirty: run.rs chunk drain 后设置
    // 生命周期：cached_base_lines 在 messages 数量变化时失效

    // L0: 完全无变化 — 直接复用最终缓存（零 clone 路径）
    if !state.rendered_lines_dirty.get()
        && *state.cached_width.borrow() == inner.width
        && state.stream_cursor == 0
        && !state.is_streaming
        && !state.streaming_content_dirty.get()
    {
        // 直接借用渲染，不 clone
        let cached = state.cached_lines.borrow();
        if !cached.is_empty() {
            f.render_widget(List::new(cached.clone()).direction(ListDirection::TopToBottom), inner);
            return;
        }
    }

    // 决定是否需要重建 base lines
    let current_msg_count = state.messages.len();
    let base_cache_valid = state.is_streaming
        && state.cached_base_msg_count.get() == current_msg_count
        && *state.cached_width.borrow() == inner.width
        && !state.cached_base_lines.borrow().is_empty();

    let mut trace_part_positions: Vec<(usize, usize, usize)> = Vec::new();
    // P3 优化：L1 不再 clone cached_base_lines，而是 take 出来直接使用
    // 帧末（如果是 L1 路径）再写回。避免每帧 clone 数百行 Vec<Line>。
    let mut lines = if base_cache_valid {
        // L1: take 出 base lines（零 clone），streaming append 后写回
        std::mem::take(&mut *state.cached_base_lines.borrow_mut())
    } else {
        // L2: 全量重建（首次 / messages 变化 / 宽度变化）
        let built = build_message_lines(
            &state.messages, 0, &state.theme, &state.text_selection, inner.width,
            state.stream_cursor, state.compact, state.code_blocks_expanded,
            &state.trace_events, &state.trace_event_index, &mut trace_part_positions, state.focused_event_id,
        );
        state.cached_base_msg_count.set(current_msg_count);
        built
    };
    // 清除 streaming_content_dirty（本帧已处理）
    state.streaming_content_dirty.set(false);
    // 记录 base lines 长度，streaming 追加后用于截断回写缓存
    let base_lines_len = lines.len();

    // ── 流式消息：追加 streaming 状态（thinking + tools + text）──
    // build_message_lines 只渲染 header + cursor，这里补充完整的流式内容
    if state.is_streaming {
        let bar = Span::styled("▎", Style::default().fg(state.theme.session));

        // V14 修复：build_message_lines 仅在 stream_cursor>0 时追加 🤖 Abacus ghost header；
        //          在 stream_cursor==0（流式刚启动、TextDelta 尚未到达）时本函数必须自己补 header，
        //          否则 thinking/tools 直接挂在 user 消息下方，视觉上像 user 在 thinking。
        if state.stream_cursor == 0 {
            // ── 状态 badge：header 右侧展示当前阶段 ──
            // 引用关系：消费 streaming_tools / streaming_text_started / streaming_thinking
            // 生命周期：仅 is_streaming=true 且 stream_cursor==0 时渲染
            let status_badge: Span<'static> = {
                use crate::tui::state::StreamingToolStatus;
                if !state.streaming_tools.iter().any(|(_, s, _, _)| *s == StreamingToolStatus::Running)
                    && state.streaming_text_started
                {
                    // TextDelta 输出中
                    Span::styled("输出中", Style::default().fg(state.theme.success))
                } else if state.streaming_tools.iter().any(|(_, s, _, _)| *s == StreamingToolStatus::Running) {
                    // 工具执行中 — 显示最近运行的工具名
                    let running_name = state.streaming_tools.iter().rev()
                        .find(|(_, s, _, _)| *s == StreamingToolStatus::Running)
                        .map(|(n, _, _, _)| n.as_str())
                        .unwrap_or("tool");
                    Span::styled(format!("⚙ {}", running_name), Style::default().fg(state.theme.gold))
                } else if !state.streaming_thinking.is_empty() && !state.streaming_text_started {
                    // Thinking 阶段
                    Span::styled("💭 thinking", Style::default().fg(state.theme.accent))
                } else {
                    Span::raw("")
                }
            };

            // Header 构建：badge_text + gap + status_badge + "  · now"
            let badge_text = "🤖 Abacus";
            let badge_span = Span::styled(
                badge_text,
                Style::default().fg(state.theme.session).add_modifier(Modifier::BOLD),
            );
            let time_text = " · now";
            let ts_span = Span::styled(time_text, state.theme.text_style(TextRole::Caption));
            let badge_w = crate::tui::util::display_width(badge_text);
            let status_text = status_badge.content.to_string();
            let status_w = crate::tui::util::display_width(&status_text);
            let time_w = crate::tui::util::display_width(time_text);
            let content_w_hdr = (inner.width as usize).saturating_sub(4); // bar(1)+space(1)+margin
            let header_gap = content_w_hdr.saturating_sub(badge_w + status_w + time_w + 2);
            lines.push(Line::from(vec![
                bar.clone(),
                Span::raw(" "),
                badge_span,
                Span::raw(" ".repeat(header_gap)),
                status_badge,
                Span::raw("  "),
                ts_span,
            ]));
        }

        // V40: 统一时序流渲染 — 遍历 streaming_timeline 按到达顺序渲染
        // P7 优化：所有 streaming 内容直接 push（O(1)）
        let cursor_line: Option<Line<'static>> = None; // 不再需要占位行
        let content_w = inner.width.saturating_sub(5) as usize;

        for entry in state.streaming_timeline.iter() {
            use crate::tui::state::{TimelineEntry, StreamingToolStatus};
            match entry {
                TimelineEntry::Thinking { summary } => {
                    let display = if summary.is_empty() { "thinking..." } else { summary.as_str() };
                    lines.push(Line::from(vec![
                        bar.clone(),
                        Span::styled(" · ", Style::default().fg(state.theme.muted)),
                        Span::styled(
                            display.to_string(),
                            Style::default().fg(state.theme.muted).add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
                TimelineEntry::Tool { name, context, status, duration_ms, failure_kind, trace_id } => {
                    let (status_mark, mark_style) = match status {
                        StreamingToolStatus::Running => ("…", Style::default().fg(state.theme.gold)),
                        StreamingToolStatus::Success => ("✓", Style::default().fg(state.theme.success)),
                        StreamingToolStatus::Failed => ("✗", Style::default().fg(state.theme.error)),
                    };
                    let dur_text = match (status, duration_ms) {
                        (StreamingToolStatus::Running, _) => String::new(),
                        (_, Some(d)) => format!(" {:.1}s", *d as f64 / 1000.0),
                        _ => String::new(),
                    };
                    let failure_text = match (status, failure_kind.as_deref()) {
                        (StreamingToolStatus::Failed, Some(kind)) => format!(" ({})", kind.to_lowercase()),
                        _ => String::new(),
                    };
                    let context_span = if context.is_empty() {
                        Span::raw("")
                    } else {
                        Span::styled(format!(" {}", context), Style::default().fg(state.theme.accent))
                    };
                    lines.push(Line::from(vec![
                        bar.clone(),
                        Span::styled(" · ", Style::default().fg(state.theme.muted)),
                        Span::styled(name.clone(), Style::default().fg(state.theme.muted)),
                        context_span,
                        Span::styled(format!(" {}{}{}", status_mark, dur_text, failure_text), mark_style),
                    ]));

                    // 编辑类工具完成时：内联 diff 预览
                    if matches!(status, StreamingToolStatus::Success | StreamingToolStatus::Failed) {
                        if let Some(ev) = state.trace_event_index.get(trace_id)
                            .and_then(|&i| state.trace_events.get(i))
                        {
                            if let crate::tui::state::TraceKind::ToolCall { ref args, .. } = ev.kind {
                                if !args.is_empty() {
                                    if let Some(diff_lines) = block_detail::try_render_edit_diff(
                                        name, args, &state.theme, 8,
                                    ) {
                                        for dl in diff_lines {
                                            let mut spans: Vec<Span> = vec![bar.clone(), Span::raw("   ")];
                                            spans.extend(dl.spans);
                                            lines.push(Line::from(spans));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                TimelineEntry::ToolOutput { summary } => {
                    if !summary.is_empty() {
                        lines.push(Line::from(vec![
                            bar.clone(),
                            Span::raw("   "),
                            Span::styled(
                                format!("└ {}", summary),
                                Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM),
                            ),
                        ]));
                    }
                }
                TimelineEntry::Text { start, end: _ } => {
                    // 正文文本：通过 mdstream 渲染 streaming_text[start..end]
                    // 为简化实现，整个 streaming_text 交给 mdstream 统一渲染
                    // （mdstream 已增量缓存 committed blocks，此处只在首次 Text entry 时渲染全部）
                    // 后续 Text entries 跳过（因为 mdstream 渲染的是完整 streaming_text）
                    if *start == 0 {
                        // 首个 Text entry: 渲染整个 streaming_text
                        let styled_lines: Vec<crate::tui::markdown::StyledLine> = {
                            let mut smd_ref = state.streaming_md.borrow_mut();
                            if let Some(ref mut smd) = *smd_ref {
                                smd.all_styled(&state.theme, false, content_w)
                            } else {
                                crate::tui::markdown::render_markdown_bounded(
                                    &state.streaming_text, &state.theme, false, content_w,
                                )
                            }
                        };
                        for styled in &styled_lines {
                            let rline = crate::tui::markdown::styled_line_to_ratatui(styled, &bar, &state.theme);
                            if styled.line_type == crate::tui::markdown::LineType::Table {
                                lines.push(rline);
                                continue;
                            }
                            let line_w: usize = rline.spans.iter()
                                .map(|s| crate::tui::util::display_width(s.content.as_ref()))
                                .sum();
                            if line_w <= content_w + 4 {
                                lines.push(rline);
                            } else {
                                let indent_str = "   ";
                                let full_text: String = styled.spans.iter().map(|s| s.text.as_str()).collect();
                                let text_style = styled.spans.first().map(|s| s.style)
                                    .unwrap_or(Style::default().fg(state.theme.text));
                                let segments = crate::tui::util::word_wrap_segments(&full_text, content_w);
                                for (seg_start, seg_end) in segments {
                                    lines.push(Line::from(vec![
                                        bar.clone(), Span::raw(indent_str.to_string()),
                                        Span::styled(full_text[seg_start..seg_end].to_string(), text_style),
                                    ]));
                                }
                            }
                        }
                    }
                    // 非首个 Text entry: 跳过（mdstream 已渲染完整 streaming_text）
                }
                TimelineEntry::Iteration { number } => {
                    lines.push(Line::from(vec![
                        bar.clone(),
                        Span::styled(
                            format!(" ─ #{} ", number),
                            Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM),
                        ),
                    ]));
                }
            }
        }

        // Fallback: 如果 timeline 为空但 streaming_text 非空（兼容旧路径）
        if state.streaming_timeline.is_empty() && !state.streaming_text.is_empty() {
            let styled_lines: Vec<crate::tui::markdown::StyledLine> = {
                let mut smd_ref = state.streaming_md.borrow_mut();
                if let Some(ref mut smd) = *smd_ref {
                    smd.all_styled(&state.theme, false, content_w)
                } else {
                    crate::tui::markdown::render_markdown_bounded(
                        &state.streaming_text, &state.theme, false, content_w,
                    )
                }
            };
            for styled in &styled_lines {
                let rline = crate::tui::markdown::styled_line_to_ratatui(styled, &bar, &state.theme);
                lines.push(rline);
            }
        }

        // P7: push 回占位光标行（streaming 内容全部 push 到它前面完毕）
        if let Some(cl) = cursor_line {
            lines.push(cl);
        }
    }

    // 视窗滚动：scroll=0 表示自动跟随底部，>0 表示向上偏移 N 行
    // V29.5 (B2/B12): streaming 期间不再强制 scroll=0 —— 用户主动向上看历史时,
    //   下面的 streaming 不应把视图甩到底; 只有 scroll==0 时才 auto-follow
    //   设计意图: scroll>0 视为"用户已离开底部进入浏览", 直到用户按 End 或 Home 显式回底
    // P3: streaming 期间写回 base lines 缓存（只保留 base 部分，不含 streaming 追加）
    // 这样下一帧 L1 路径 take 出来的就是干净的 base lines
    if state.is_streaming && base_lines_len > 0 {
        let base_snapshot: Vec<Line<'static>> = lines[..base_lines_len].to_vec();
        *state.cached_base_lines.borrow_mut() = base_snapshot;
    } else if !state.is_streaming && !base_cache_valid {
        // L2 路径（非 streaming）：写入 base 缓存
        *state.cached_base_lines.borrow_mut() = lines.clone();
    }

    let visible_h = inner.height as usize;
    let scroll_offset = state.scroll;
    // V29.5: 切片前抓总行数, 让 last_total_lines 反映真实总量
    let total_before_slice = lines.len();
    // P9 优化：用 drain 代替 slice.to_vec()，直接截断 Vec 头尾
    let (visible_start, visible_end) = if total_before_slice > visible_h {
        let max_scroll = total_before_slice.saturating_sub(visible_h);
        let clamped = scroll_offset.min(max_scroll);
        let end = total_before_slice.saturating_sub(clamped);
        let start = end.saturating_sub(visible_h);
        // 截尾（O(1)）
        lines.truncate(end);
        // 截头（O(visible_h) 而非 O(total)）
        if start > 0 {
            lines.drain(..start);
        }
        (start, end)
    } else {
        (0, total_before_slice)
    };
    state.last_visible_h.set(visible_h);
    state.last_total_lines.set(total_before_slice);
    state.last_content_width.set((inner.width as usize).saturating_sub(5));

    // V28.3: trace_part_positions → screen row map
    let mut row_map = state.message_trace_row_map.borrow_mut();
    row_map.clear();
    for (line_idx, msg_idx, part_idx) in &trace_part_positions {
        if *line_idx >= visible_start && *line_idx < visible_end {
            let abs_y = inner.y.saturating_add((*line_idx - visible_start) as u16);
            row_map.push((abs_y, *msg_idx, *part_idx));
        }
    }
    drop(row_map);

    // Line Flash 效果（仅尾部行）
    if state.is_streaming && state.flash_state.active_flash_count() > 0 {
        let flash_count = state.flash_state.active_flash_count();
        let scan_start = lines.len().saturating_sub(flash_count + 2);
        for line in lines[scan_start..].iter_mut() {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let h = effects::FlashState::hash_line(&content);
            if state.flash_state.is_flashing(h) {
                let taken = std::mem::take(line);
                *line = effects::apply_flash_style(taken, state.theme.surface);
            }
        }
    }

    // P10 优化：move 而非 clone — lines 已是最终可见切片，直接 move 进缓存
    *state.cached_width.borrow_mut() = inner.width;
    state.rendered_lines_dirty.set(false);
    let lines_for_render = lines.clone(); // ratatui List 需要 owned Vec
    *state.cached_lines.borrow_mut() = lines;
    f.render_widget(List::new(lines_for_render).direction(ListDirection::TopToBottom), inner);
}

/// Streaming 光标闪烁效果（附加到消息列表末尾，使用色条风格）
///
/// 引用关系：被流式输出逻辑调用（当 build_message_lines 未处理时的 fallback）
/// 生命周期：is_streaming=true 时激活，流式结束后停止
pub fn render_streaming_cursor(lines: &mut Vec<Line<'_>>, state: &AppState) {
    if !state.is_streaming { return; }
    let bar = Span::styled("▎", Style::default().fg(state.theme.session));
    // 闪烁光标（500ms 周期）
    let cursor_visible = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis() / 500) % 2 == 0;
    if cursor_visible {
        lines.push(Line::from(vec![
            bar,
            Span::raw(" "),
            Span::styled("▌", Style::default().fg(state.theme.session)),
        ]));
    } else {
        lines.push(Line::from(vec![
            bar,
            Span::raw(" "),
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
        .map(|_| Line::styled("▏", bar_style))
        .collect();
    f.render_widget(Paragraph::new(bar_lines), split_h[0]);

    split_h[1]
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
