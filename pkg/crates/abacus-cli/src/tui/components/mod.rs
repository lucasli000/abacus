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
pub(crate) mod card;
mod extras;
mod overlays;
mod panel;
pub(crate) mod section_ctx;
pub(crate) mod dashboard_tabs;
pub(crate) mod panel_sections;
pub use bars::*;
use block_detail::*;
pub use extras::*;
pub use overlays::*;
pub use panel::render_panel;
pub use crate::tui::cards::render::render_cards;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Widget};

use crate::tui::state::AppState;
use abacus_ui_kit::Theme;

// ════════════════════════════════════════════════════════════════
// 共享消息行构建 (避免 render_messages / render_messages_in_card 重复)
// ════════════════════════════════════════════════════════════════

/// 超长行自动折行，最多 2 个 visual line，超时第二行尾部用 … 截断
fn wrap_or_truncate(text: &str, max_width: usize, indent_width: usize) -> Vec<String> {
    let usable = max_width.saturating_sub(indent_width).max(4);
    let segments = crate::tui::util::word_wrap_segments(text, usable);
    if segments.len() <= 2 {
        return segments.iter().map(|&(a, b)| text[a..b].to_string()).collect();
    }
    // 取前两段，第二段末尾用 … 截断
    let s1 = &text[segments[0].0..segments[0].1];
    let s2_start = segments[1].0;
    let truncated_end = text[s2_start..].char_indices()
        .scan(0usize, |w, (i, ch)| {
            let cw = crate::tui::util::char_width(ch);
            if *w + cw > usable.saturating_sub(1) { return None; }
            *w += cw;
            Some(s2_start + i + ch.len_utf8())
        })
        .last()
        .unwrap_or(segments[1].1);
    vec![s1.to_string(), format!("{}…", &text[s2_start..truncated_end])]
}

/// 代码块超过此行数时折叠（Ctrl+E 展开全部）
const CODE_BLOCK_MAX_LINES: u32 = 20;

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
                    let cw = content_width.max(1);
                    rows += if dw <= cw { 1 } else { (dw + cw - 1) / cw };
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

// ════════════════════════════════════════════════════════════════
// 屏幕坐标反查 (V30: 鼠标点击 / 拖拽选中) — LIVE
// ════════════════════════════════════════════════════════════════

/// V30 复制修复：屏幕 (row, col) → (msg_idx, char_idx_in_flat_text)
///
/// ## char_idx 语义
/// `char_idx` 是 msg 平铺文本（Stream parts 拼接）中的字符偏移。
/// 如果 msg 含 Block/Trace, 正文阈限在 Stream 部分按字符精度定位。
///
/// ## 返回 None 场景
/// - row 超出消息区
/// - 位置落在 msg 头部 role 标签 / 时间戳行
///
/// ## 引用关系
/// - event/mod.rs handle_mouse Down/Drag 调用
pub(crate) fn screen_pos_to_msg_char(
    row: u16,
    col: u16,
    terminal_rows: u16,
    scroll: usize,
    messages: &std::collections::VecDeque<crate::tui::state::Message>,
    chat_width: u16,
    cached_msg_rows: &[usize],
) -> Option<(usize, usize)> {
    use crate::tui::state::MsgContent;
    let msg_area_start = 1u16;
    let msg_area_end = terminal_rows.saturating_sub(7);
    if row < msg_area_start || row >= msg_area_end {
        return None;
    }
    let screen_row = (row - msg_area_start) as usize;
    let content_width = (chat_width as usize).saturating_sub(5).max(20);
    let use_cache = !cached_msg_rows.is_empty() && cached_msg_rows.len() == messages.len();
    // 找 msg_idx
    let mut msg_idx: Option<usize> = None;
    let mut acc = 0usize;
    for (idx, msg) in messages.iter().enumerate().skip(scroll) {
        let h = if use_cache { cached_msg_rows[idx] } else { estimate_msg_rows(msg, content_width) };
        if screen_row < acc + h {
            msg_idx = Some(idx);
            break;
        }
        acc += h;
    }
    let msg_idx = msg_idx?;
    let row_in_msg = screen_row.saturating_sub(acc);
    // 在 msg 内按 markdown 渲染近似反查 char_idx
    let mut char_idx = 0usize;
    let mut visual_row_in_msg = 0usize;
    for part in &messages[msg_idx].parts {
        if let MsgContent::Stream(text) = part {
            for line in text.split('\n') {
                if visual_row_in_msg == row_in_msg {
                    let line_chars: Vec<char> = line.chars().collect();
                    let mut col_acc = 0usize;
                    for c in &line_chars {
                        let w = unicode_width::UnicodeWidthChar::width(*c).unwrap_or(1);
                        if col_acc + w > (col.saturating_sub(6) as usize) {
                            return Some((msg_idx, char_idx));
                        }
                        col_acc += w;
                        char_idx += 1;
                    }
                    return Some((msg_idx, char_idx));
                }
                visual_row_in_msg += 1;
                char_idx += line.chars().count() + 1; // +1 for '\n'
            }
        }
    }
    Some((msg_idx, char_idx))
}

/// C3: 鼠标坐标 → (card_idx, char_idx) 反查
///
/// 替代 `screen_pos_to_msg_char`，直接读 `state.cards` 而非 `state.messages`。
/// 逻辑与原函数一致，区别：
/// - 用 cards.len() 替代 messages.len()
/// - 用 card.text_content() 替代 messages[idx].parts 提取文本
/// - cached_msg_rows 来源不变（render_cards 已同步）
pub(crate) fn screen_pos_to_card_char(
    row: u16,
    col: u16,
    terminal_rows: u16,
    scroll: usize,
    cards: &abacus_ui_kit::CardStream,
    _chat_width: u16,
    cached_msg_rows: &[usize],
) -> Option<(usize, usize)> {
    let msg_area_start = 1u16;
    let msg_area_end = terminal_rows.saturating_sub(7);
    if row < msg_area_start || row >= msg_area_end {
        return None;
    }
    let screen_row = (row - msg_area_start) as usize;
    let card_count = cards.len();
    let use_cache = !cached_msg_rows.is_empty() && cached_msg_rows.len() == card_count;
    if !use_cache {
        return None; // 无缓存时无法定位（render_cards 每帧更新缓存）
    }
    // 找 card_idx
    let mut card_idx: Option<usize> = None;
    let mut acc = 0usize;
    for (idx, _card) in cards.iter().enumerate().skip(scroll) {
        let h = cached_msg_rows[idx];
        if h == 0 { continue; }
        if screen_row < acc + h {
            card_idx = Some(idx);
            break;
        }
        acc += h;
    }
    let card_idx = card_idx?;
    let row_in_card = screen_row.saturating_sub(acc);
    // 用 card.text_content() 提取文本，按行反查 char_idx
    let text = cards.iter().nth(card_idx)?.text_content();
    let mut char_idx = 0usize;
    let mut visual_row = 0usize;
    for line in text.split('\n') {
        if visual_row == row_in_card {
            let line_chars: Vec<char> = line.chars().collect();
            let mut col_acc = 0usize;
            for c in &line_chars {
                let w = unicode_width::UnicodeWidthChar::width(*c).unwrap_or(1);
                if col_acc + w > (col.saturating_sub(6) as usize) {
                    return Some((card_idx, char_idx));
                }
                col_acc += w;
                char_idx += 1;
            }
            return Some((card_idx, char_idx));
        }
        visual_row += 1;
        char_idx += line.chars().count() + 1; // +1 for '\n'
    }
    Some((card_idx, char_idx))
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

/// 压缩状态：顶部边框整条红色 + 500ms 明暗脉冲
/// 引用关系：被 render_messages_in_card 在 processing_phase 含"压缩"时调用
/// 生命周期：压缩期间每帧绘制；CompressEnd 后 processing_phase 清空，停止调用
fn paint_compress_top_border(buf: &mut Buffer, area: Rect, error_color: ratatui::style::Color) {
    if area.width < 4 || area.height < 1 { return; }
    let inner_x = area.x + 1;
    let inner_w = area.width.saturating_sub(2);
    let top_y = area.y;
    // 500ms 交替 BOLD/DIM，产生脉冲感
    let bold = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis() / 500) % 2 == 0;
    let style = if bold {
        Style::default().fg(error_color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(error_color)
    };
    for i in 0..inner_w {
        let cell = &mut buf[(inner_x + i, top_y)];
        cell.set_symbol("━");
        cell.set_style(style);
    }
}

// ════════════════════════════════════════════════════════════════
// Panel — 右侧看板 (Tab 切换 + 内容区)
// ════════════════════════════════════════════════════════════════

// render_card_bar 已迁至 components/card.rs (V42-B, theme: &abacus_ui_kit::Theme)



// ─── V29.11 (T-DIFF): 编辑工具 diff 视图回归 ──────────────────
#[cfg(test)]
mod tool_diff_render_tests {
    //! 不变量:
    //! - Edit/Write 等白名单工具 → Some(lines)
    //! - 非编辑工具 (read_file/grep) → None (走默认 JSON pretty 路径)
    //! - args 非合法 JSON → None (容错降级)
    //! - 缺关键字段(空 old/new) → 仍渲染头行 + 空 diff(不 panic)
    use super::*;
    use abacus_ui_kit::Theme;

    fn theme() -> Theme { Theme::brand() }

    #[test]
    fn fs_edit_renders_diff() {
        // abacus filengine 核心编辑工具 — schema.name 直接为 "fs_edit"（统一命名后无 sanitize 中间层）
        let args = r#"{"path": "/tmp/x.rs", "old_string": "let x = 1;\nlet y = 2;", "new_string": "let x = 10;\nlet y = 20;"}"#;
        let result = try_render_edit_diff("fs_edit", args, &theme(), 0);
        assert!(result.is_some(), "fs_edit 应触发 diff 视图");
        let lines = result.unwrap();
        // 头行(📝 path) + 分隔线 + 2 旧行 + 2 新行 + footer = 7
        assert_eq!(lines.len(), 7);
    }

    #[test]
    fn fs_write_renders_full_new_as_added() {
        // schema.name 直接为 "fs_write"（统一命名后无 sanitize 中间层）
        let args = r#"{"path": "/tmp/y.rs", "content": "fn main() {}\n// new file"}"#;
        let result = try_render_edit_diff("fs_write", args, &theme(), 0);
        assert!(result.is_some());
        let lines = result.unwrap();
        // 头行 + 分隔线 + 0 旧行 + 2 新行 + footer = 5
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn fully_qualified_filengine_prefix_matches() {
        // 单一命名约定后：注册名直接是 fs_edit / fs_write
        let args = r#"{"path": "/tmp/x.rs", "old_string": "a", "new_string": "b"}"#;
        assert!(try_render_edit_diff("fs_edit", args, &theme(), 0).is_some());
        let args_w = r#"{"path": "/tmp/y.rs", "content": "c"}"#;
        assert!(try_render_edit_diff("fs_write", args_w, &theme(), 0).is_some());
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
        // 头行(1) + 分隔线(1) + 4 diff 行 + 省略(1) + footer(1) = 8
        assert_eq!(lines.len(), 8);
    }

    #[test]
    fn empty_old_string_only_renders_new_as_added() {
        // fs_edit 时 old_string 空字符串 (新建场景, 比如往空文件写)
        let args = r#"{"path": "/tmp/x.rs", "old_string": "", "new_string": "fn main() {}"}"#;
        let result = try_render_edit_diff("fs_edit", args, &theme(), 0);
        assert!(result.is_some());
        let lines = result.unwrap();
        // 头行 + 分隔线 + 1 新行 + footer = 4 (无旧行)
        assert_eq!(lines.len(), 4);
    }
}
