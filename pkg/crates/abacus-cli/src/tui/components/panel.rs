//! Panel — 右侧看板 (含 Tab 路由 + 子面板渲染)
//!
//! 从 mod.rs 拆出的面板渲染组件集合。
//!
//! ## 引用关系
//! - 被 modes/chat.rs、team.rs、meeting.rs、plan.rs 通过 `render_panel` 调用
//! - 内部调用 super::render_card_bar (色条卡片)
//! - 内部调用 super::format_duration_ms_padded (from block_detail via mod.rs glob import)
//! - 内部使用 crate::tui::{state, theme, markdown, util, cost}
//!
//! ## 生命周期
//! - 面板可见时每帧渲染；不持有状态

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::tui::i18n::t;
use crate::tui::state::{AppState, Focus};
use abacus_ui_kit::{Section, SectionStack, TextRole};

/// render_card_bar lives in super::card; re-used here for panel content areas.
use super::card::render_card_bar;
use super::panel_sections::{FocusSection, LlmSection, LocalSection, PalaceSection, TimelineSection, ToolsSection};

// ════════════════════════════════════════════════════════════════
// Panel public entry point
// ════════════════════════════════════════════════════════════════

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
fn build_tab_spans<'a>(labels: &'a [String], active: usize, theme: &abacus_ui_kit::Theme) -> Vec<Span<'a>> {
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

    // V40: Scene tab 已合并 Stockroom 内容——单 tab 布局
    // Stockroom 的记忆宫殿/工具仓/技能引擎内嵌到 Scene 顶部
    let labels: Vec<String> = vec![
        label_with_count(t("panel.scene"), state.trace_events.len()),
    ];
    let content = render_panel_header(f, state, inner, &labels, 0);
    render_tab_scene(f, state, content)
}

/// Phase 3 去重：公共 Panel header 渲染（Tab 栏 + 分隔线 + 内容区分割）
///
/// 四模式分支共享相同的 Layout(1+1+Min(2)) + build_tab_spans + separator 逻辑。
/// 本函数统一渲染 tab + sep，返回 content area（已经过 render_card_bar）。
///
/// 引用关系：被 render_panel 的 Clarify/Meeting 两分支调用（V34: Team/Plan 已降级为执行策略）
/// 生命周期：每帧渲染，纯函数
fn render_panel_header(
    f: &mut ratatui::Frame,
    state: &AppState,
    inner: Rect,
    tab_labels: &[String],
    tab_idx: usize,
) -> Rect {
    let layout = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Length(1), // Tab 栏
            ratatui::layout::Constraint::Length(1), // 分隔线
            ratatui::layout::Constraint::Min(2),    // 内容
        ])
        .split(inner);

    let tab_spans = build_tab_spans(tab_labels, tab_idx, &state.theme);
    f.render_widget(Paragraph::new(Line::from(tab_spans)), layout[0]);

    let sep = "─".repeat(inner.width as usize);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(&*sep, Style::default().fg(state.theme.border).add_modifier(Modifier::DIM)))),
        layout[1],
    );

    render_card_bar(f, &state.theme, layout[2])
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

    let mut lines: Vec<Line> = Vec::new();
    let max_w = (area.width as usize).saturating_sub(2);

    // V28.1: 清空 row map 准备本帧重建
    let mut row_map = state.timeline_row_map.borrow_mut();
    row_map.clear();

    // ═══ Section 1: Pipeline 执行进度 ═══
    lines.push(Line::from(vec![
        Span::styled(t("panel.pipeline"), Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
    ]));

    // 从 trace_events 提取执行步骤（ToolCall 类型）
    let tool_events: Vec<&crate::tui::state::TraceEvent> = state.trace_events.iter()
        .filter(|e| matches!(e.kind, TraceKind::ToolCall { .. }))
        .collect();

    if tool_events.is_empty() && state.active_llm_thinking().is_empty() && !state.is_streaming_active() {
        lines.push(Line::styled(" —", Style::default().fg(state.theme.muted)));
    } else {
        // Thinking 进度
        let think_events: Vec<&crate::tui::state::TraceEvent> = state.trace_events.iter()
            .filter(|e| matches!(e.kind, TraceKind::Thinking { .. }))
            .collect();
        if !think_events.is_empty() || !state.active_llm_thinking().is_empty() {
            let think_lines: usize = think_events.iter().map(|e| {
                if let TraceKind::Thinking { lines, .. } = &e.kind { *lines } else { 0 }
            }).sum();
            let total_lines = think_lines + state.active_llm_thinking().lines().count();
            lines.push(Line::from(vec![
                Span::styled(" ✓ ", Style::default().fg(state.theme.success)),
                Span::styled(format!("{} {}行", t("timeline.thinking"), total_lines), Style::default().fg(state.theme.text)),
            ]));
        }

        // 工具执行列表（最近 N 个）
        let max_tools_shown = ((area.height as usize) / 2).max(3).min(8);
        let skip = tool_events.len().saturating_sub(max_tools_shown);
        if skip > 0 {
            lines.push(Line::from(vec![
                Span::styled(format!(" … {} {}", skip, t("timeline.earlier")), state.theme.text_style(TextRole::Caption)),
            ]));
        }
        for evt in tool_events.iter().skip(skip) {
            if let TraceKind::ToolCall { name, status, args, .. } = &evt.kind {
                let (icon, color) = match status {
                    ToolStatus::Success => ("✓", state.theme.success),
                    ToolStatus::Failed => ("✗", state.theme.error),
                    ToolStatus::Running => ("⏳", state.theme.gold),
                };
                let dur = evt.duration_ms
                    .map(|ms| format!(" {:.1}s", ms as f64 / 1000.0))
                    .unwrap_or_default();
                // 提取路径/URL 等上下文
                let context: String = serde_json::from_str::<serde_json::Value>(args).ok()
                    .and_then(|json| {
                        json.get("path").or(json.get("file_path")).and_then(|v| v.as_str())
                            .map(|p| {
                                let parts: Vec<&str> = p.rsplitn(3, '/').collect();
                                if parts.len() >= 2 { format!(" {}/{}", parts[1], parts[0]) }
                                else { format!(" {}", p) }
                            })
                            .or_else(|| json.get("command").and_then(|v| v.as_str())
                                .map(|c| { let s = if c.len() > 20 { &c[..18] } else { c }; format!(" `{}`", s) }))
                            .or_else(|| json.get("query").or(json.get("pattern")).and_then(|v| v.as_str())
                                .map(|q| { let s = if q.len() > 15 { &q[..13] } else { q }; format!(" \"{}\"", s) }))
                    })
                    .unwrap_or_default();
                let text = format!("{}{}{}", name, context, dur);
                let truncated = crate::tui::util::truncate_to_width(&text, max_w.saturating_sub(4));
                // row_map 记录
                let abs_y = area.y.saturating_add(lines.len() as u16);
                row_map.push((abs_y, evt.id));
                lines.push(Line::from(vec![
                    Span::styled(format!(" {} ", icon), Style::default().fg(color)),
                    Span::styled(truncated, Style::default().fg(state.theme.text)),
                ]));
            }
        }

        // 当前正在执行的工具（streaming 期间）
        if state.is_streaming_active() {
            for (name, status, _, _) in state.streaming_tools.iter() {
                if matches!(status, crate::tui::state::StreamingToolStatus::Running) {
                    lines.push(Line::from(vec![
                        Span::styled(" ⏳ ", Style::default().fg(state.theme.gold)),
                        Span::styled(name.clone(), Style::default().fg(state.theme.gold)),
                    ]));
                }
            }
        }
    }

    // ═══ Section 2: 文件变更追踪 ═══
    // 从 tool_events 中提取编辑/写入过的文件
    let mut changed_files: Vec<(String, &str)> = Vec::new(); // (path, type: M/A)
    for evt in &tool_events {
        if let TraceKind::ToolCall { name, args, status, .. } = &evt.kind {
            if !matches!(status, ToolStatus::Success) { continue; }
            let lower = name.to_lowercase();
            if lower.contains("edit") || lower.contains("write") {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(args) {
                    if let Some(p) = json.get("path").or(json.get("file_path")).and_then(|v| v.as_str()) {
                        let short: String = {
                            let parts: Vec<&str> = p.rsplitn(3, '/').collect();
                            if parts.len() >= 2 { format!("{}/{}", parts[1], parts[0]) }
                            else { p.to_string() }
                        };
                        let change_type = if lower.contains("write") { "A" } else { "M" };
                        // 去重
                        if !changed_files.iter().any(|(f, _)| *f == short) {
                            changed_files.push((short, change_type));
                        }
                    }
                }
            }
        }
    }

    if !changed_files.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(t("panel.changes"), Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" · {}", changed_files.len()), Style::default().fg(state.theme.muted)),
        ]));
        for (path, ctype) in changed_files.iter().take(6) {
            let (prefix, color) = match *ctype {
                "A" => ("A", state.theme.success),
                _ => ("M", state.theme.gold),
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", prefix), Style::default().fg(color)),
                Span::styled(path.clone(), Style::default().fg(state.theme.text)),
            ]));
        }
        if changed_files.len() > 6 {
            lines.push(Line::from(vec![
                Span::styled(format!("   +{} more", changed_files.len() - 6), state.theme.text_style(TextRole::Caption)),
            ]));
        }
    }

    // ═══ 底部统计 ═══
    let total_tools = tool_events.len();
    let succeeded = tool_events.iter().filter(|e| {
        matches!(&e.kind, TraceKind::ToolCall { status: ToolStatus::Success, .. })
    }).count();
    let failed = tool_events.iter().filter(|e| {
        matches!(&e.kind, TraceKind::ToolCall { status: ToolStatus::Failed, .. })
    }).count();
    if total_tools > 0 {
        lines.push(Line::raw(""));
        let total_dur: u64 = tool_events.iter().filter_map(|e| e.duration_ms).sum();
        lines.push(Line::from(vec![
            Span::styled(
                format!(" ✓{} ✗{} · {:.1}s", succeeded, failed, total_dur as f64 / 1000.0),
                state.theme.text_style(TextRole::Caption),
            ),
        ]));
    }

    state.last_timeline_visible.set(area.height as usize);
    drop(row_map);
    f.render_widget(Paragraph::new(lines), area);
}


// V33+ 清理：render_tab_components / render_tab_tasks / render_task_kanban_inner /
//   render_panel_overview / render_compact_stats / render_panel_team_board /
//   render_panel_meeting_agenda / render_theme_preview / render_tab_memory /
//   render_tab_quant — 全部已删除，功能迁移到 render_tab_scene + render_tab_stockroom。



// ══════════════════════════════════════════════════════════════════════════════
// V35: 现场 Tab (render_tab_scene) + 仓库 Tab (render_tab_stockroom)
// ══════════════════════════════════════════════════════════════════════════════

fn render_tab_scene(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    use ratatui::layout::{Layout, Constraint, Direction};
    use ratatui::widgets::Paragraph;
    use ratatui::text::Line;

    // 视觉分隔仅保留空行，不再画 ╌╌╌╌ 横线
    let sep = Paragraph::new(Line::raw(""));

    // 判断 Focus 是否需要显示
    let focus_active = state.is_streaming_active()
        || !state.processing_phase.is_empty()
        || state.mode == crate::tui::state::AbacusMode::Meeting
        || state.turn_count > 0;

    // 3-5 段布局：Stockroom → 分隔线 → Timeline → ?[分隔线 → Focus]
    let secs = if focus_active {
        Layout::default().direction(Direction::Vertical)
            .constraints([
                Constraint::Max(14),       // Stockroom（上限 14，不撑大）
                Constraint::Length(1),     // 分隔线
                Constraint::Fill(1),       // Timeline（占剩余）
                Constraint::Length(1),     // 分隔线
                Constraint::Fill(1),       // Focus（与 Timeline 对半）
            ])
            .split(area)
    } else {
        Layout::default().direction(Direction::Vertical)
            .constraints([
                Constraint::Max(14),
                Constraint::Length(1),
                Constraint::Fill(1),
            ])
            .split(area)
    };

    // ── Stockroom 区块 ──
    let ctx = super::section_ctx::AppContext::new(state);
    let stockroom = SectionStack::new()
        .add(&LlmSection)
        .add(&ToolsSection)
        .add(&LocalSection)
        .add(&PalaceSection);
    stockroom.render(f, &ctx, secs[0]);

    f.render_widget(sep.clone(), secs[1]);

    // ── Timeline ──
    let timeline = TimelineSection;
    timeline.render(f, &ctx, secs[2]);

    if focus_active {
        f.render_widget(sep, secs[3]);
        // ── Focus ──
        let focus = FocusSection;
        focus.render(f, &ctx, secs[4]);
    }
}

/// Data 面板 — 关键指标紧凑展示，充分利用面板宽度
fn render_tab_data(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    use ratatui::style::{Style, Modifier};
    use crate::tui::components::bars::format_ctx;
    
    let w = (area.width as usize).saturating_sub(2).max(10);
    let label = Style::default().fg(state.theme.muted); // 标签色
    let val   = Style::default().fg(state.theme.text);   // 数值色
    let dim   = Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM);
    let _gold  = Style::default().fg(state.theme.gold).add_modifier(Modifier::BOLD);
    let mut lns: Vec<Line> = Vec::new();
    // ── 标题行 ──
    lns.push(Line::from(Span::styled("统计", Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD))));

    // ── context 进度条 ──
    let raw_used = if state.ctx_live_tokens > 0 {
        state.ctx_live_tokens as usize
    } else {
        state.session_tokens.latest_prompt_tokens as usize
    };
    let max_ctx = state.context_window;
    let used = if max_ctx > 0 { raw_used.min(max_ctx) } else { raw_used };
    if max_ctx > 0 && used > 0 {
        let pct = (used * 100 / max_ctx).min(100);
        let pc = if pct >= 80 { state.theme.error } else if pct >= 50 { state.theme.gold } else { state.theme.success };
        let bw = w.saturating_sub(8);
        let filled = (pct * bw / 100).min(bw);
        lns.push(Line::from(vec![
            Span::styled("ctx ", label),
            Span::styled("█".repeat(filled), Style::default().fg(pc)),
            Span::styled("░".repeat(bw - filled), dim),
            Span::raw(" "),
            Span::styled(format!("{}%", pct), Style::default().fg(pc).add_modifier(Modifier::BOLD)),
        ]));
        lns.push(Line::from(vec![
            Span::raw("  "), Span::styled(format_ctx(used), val),
            Span::styled(" / ", dim), Span::styled(format_ctx(max_ctx), val),
        ]));
    }

    // ── 第2行：回合统计（两行 key:value 表格）──
    {
        let uc = state.messages.iter().filter(|m| matches!(m.role, crate::tui::state::MsgRole::User)).count();
        let ac = state.messages.iter().filter(|m| matches!(m.role, crate::tui::state::MsgRole::Session | crate::tui::state::MsgRole::Expert(_))).count();
        let ev = state.trace_events.len();
        // 两行表格：固定 label 宽度对齐
        lns.push(Line::from(vec![
            Span::styled("  turns ", label),
            Span::styled(format!("{}", state.turn_count), Style::default().fg(state.theme.accent)),
            Span::styled(" · you ", label),
            Span::styled(format!("{}", uc), val),
            Span::styled(" · ai ", label),
            Span::styled(format!("{}", ac), val),
        ]));
        lns.push(Line::from(vec![
            Span::styled("  ev    ", label),
            Span::styled(format!("{}", ev), dim),
        ]));
    }

    // ── 第3行：token 输入/输出 + 缓存命中 ──
    {
        let inp = state.session_tokens.prompt_tokens;
        let out = state.session_tokens.completion_tokens;
        let cached = state.session_tokens.cached_tokens;
        let cpct = if inp > 0 { cached * 100 / inp } else { 0 };
        // 对齐的两行
        lns.push(Line::from(vec![
            Span::styled("  in    ", label),
            Span::styled(format!("{:<8}", format_ctx(inp as usize)), val),
            Span::styled("  out ", label),
            Span::styled(format!("{}", format_ctx(out as usize)), val),
        ]));
        lns.push(Line::from(vec![
            Span::styled("  cache ", label),
            Span::styled(format!("{}%", cpct), Style::default().fg(state.theme.success)),
        ]));
    }

    // cost（仅有数据时）
    if state.session_tokens.cost_cny > 0.001 {
        lns.push(Line::from(vec![
            Span::styled("  cost  ", label),
            Span::styled(
                crate::tui::cost::format_cny(state.session_tokens.cost_cny),
                Style::default().fg(state.theme.gold),
            ),
        ]));
    }

    // ── 压缩统计（仅发生过压缩时） ──
    let comp_n = state.session_tokens.compress_count;
    let comp_s = state.session_tokens.compress_tokens_saved;
    if comp_n > 0 {
        lns.push(Line::from(vec![
            Span::styled("  cmp   ", label),
            Span::styled(format!("{}× freed {}", comp_n, format_ctx(comp_s as usize)), dim),
        ]));
    }

    f.render_widget(Paragraph::new(lns), area);
}

fn render_tab_stockroom(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    use ratatui::style::{Style, Modifier};
    use crate::tui::state::ToolStatus;
    let muted = Style::default().fg(state.theme.muted);
    let dim  = Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM);
    let ab   = Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD);
    let txt  = Style::default().fg(state.theme.text);
    let sep  = Line::styled(" ╌╌╌╌╌╌╌╌", dim);
    let mut lines: Vec<Line> = Vec::new();

    // ════════════════════════════════════════════════════════════
    // 记忆宫殿 — palace 本体结构 + 本轮调用记录
    // ════════════════════════════════════════════════════════════
    lines.push(Line::from(Span::styled(format!("{}", t("panel.knowledge")), ab)));
    if let Some(ref snap) = state.palace_data {
        if snap.behavior_count > 0 {
            lines.push(Line::from(vec![Span::styled(format!("  {}", t("palace.behavior")), Style::default().fg(state.theme.gold).add_modifier(Modifier::BOLD)), Span::styled(format!("  {}", snap.behavior_count), txt)]));
        }
        if !snap.knowledge_domains.is_empty() {
            lines.push(Line::from(Span::styled(format!("  {}", t("palace.knowledge")), Style::default().fg(state.theme.gold).add_modifier(Modifier::BOLD))));
            for (domain, cnt) in snap.knowledge_domains.iter().take(5) {
                let d: String = domain.chars().take(16).collect();
                lines.push(Line::from(vec![Span::styled(format!("    {}", d), Style::default().fg(state.theme.accent)), Span::styled(format!("  {}", cnt), txt)]));
            }
            if snap.knowledge_domains.len() > 5 {
                lines.push(Line::styled(format!("    +{}", snap.knowledge_domains.len() - 5), muted));
            }
        }
        // This-turn calls
        let mem: Vec<_> = state.knowledge_calls.iter().filter(|k| k.palace.starts_with("记忆/")).collect();
        if !mem.is_empty() {
            lines.push(Line::styled(format!("  {}", t("palace.this_turn")), dim));
            use std::collections::BTreeMap;
            let mut tree: BTreeMap<&str, u32> = BTreeMap::new();
            for kc in &mem { *tree.entry(kc.domain.as_str()).or_insert(0) += kc.count; }
            for (domain, cnt) in &tree {
                lines.push(Line::from(vec![Span::styled(format!("    {}", domain), Style::default().fg(state.theme.muted)), Span::styled(format!("  ×{}", cnt), txt)]));
            }
        }
    } else {
        lines.push(Line::styled(format!("  — {}", t("palace.loading")), muted));
    }

    // ════════════════════════════════════════════════════════════
    // 工具仓 — 注册能力 + 健康度 + 本轮调用
    // ════════════════════════════════════════════════════════════
    lines.push(sep.clone());
    lines.push(Line::from(Span::styled(t("panel.tools"), ab)));
    let hc = state.tool_health.len();
    if hc > 0 {
        let avail = state.tool_health.values().filter(|h| !h.blocked_by_env).count();
        let pct = avail * 100 / hc;
        lines.push(Line::from(vec![Span::styled("  健康 ", muted), Span::styled(format!("{}/{} · {}%", avail, hc, pct), txt)]));
        let mut tiers: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for h in state.tool_health.values() { *tiers.entry(h.tier.as_str()).or_insert(0) += 1; }
        let ts: Vec<String> = ["S","A","B","C","D"].iter().filter_map(|t| tiers.get(t).map(|n| format!("{} {}", t, n))).collect();
        if !ts.is_empty() { lines.push(Line::from(vec![Span::raw("  "), Span::styled(ts.join(" · "), muted)])); }
    }
    let blocked: Vec<_> = state.tool_health.iter().filter(|(_, h)| h.blocked_by_env).collect();
    if !blocked.is_empty() {
        lines.push(Line::from(Span::styled(format!("  阻断 {}", blocked.len()), Style::default().fg(state.theme.error))));
        for (nm, _) in blocked.iter().take(2) {
            let t: String = nm.rsplitn(2, "__").next().unwrap_or(nm).chars().take(18).collect();
            lines.push(Line::styled(format!("    {}", t), muted));
        }
    }
    let tc = state.tool_records.len();
    if tc > 0 {
        let sc = state.tool_records.iter().filter(|r| matches!(r.status, ToolStatus::Success)).count();
        lines.push(Line::from(vec![Span::styled("  调 ", muted), Span::styled(format!("{} · ✓{} ✗{}", tc, sc, tc - sc), txt)]));
        let mut freq: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
        for r in &state.tool_records { *freq.entry(r.name.as_str()).or_insert(0) += 1; }
        if let Some((tn, cnt)) = freq.iter().max_by_key(|(_, c)| *c) {
            let t: String = tn.rsplitn(2, "__").next().unwrap_or(tn).chars().take(14).collect();
            lines.push(Line::from(vec![Span::styled("  最 ", muted), Span::styled(format!("{} · {}次", t, cnt), txt)]));
        }
    }

    // ════════════════════════════════════════════════════════════
    // 技能引擎 — 可复用工作流调用
    // ════════════════════════════════════════════════════════════
    lines.push(sep.clone());
    lines.push(Line::from(Span::styled(t("panel.workflow"), ab)));
    let skills: Vec<_> = state.tool_records.iter().filter(|r| !r.name.contains("__") && !r.name.starts_with("mcp_")).collect();
    if skills.is_empty() {
        lines.push(Line::styled("  —", muted));
    } else {
        lines.push(Line::from(vec![Span::styled("  调 ", muted), Span::styled(format!("{} 次", skills.len()), txt)]));
        let mut freq: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
        for r in &skills { *freq.entry(r.name.as_str()).or_insert(0) += 1; }
        let mut fv: Vec<_> = freq.into_iter().collect();
        fv.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        let top: Vec<String> = fv.iter().take(2).map(|(n, c)| format!("{} ({}次)", n, c)).collect();
        if !top.is_empty() { lines.push(Line::from(vec![Span::styled("  常 ", muted), Span::styled(top.join(" · "), txt)])); }
    }

    let vis = area.height as usize;
    if lines.len() > vis {
        let end = lines.len().saturating_sub(state.knowledge_scroll_offset);
        let start = end.saturating_sub(vis);
        lines = lines[start..end].to_vec();
    }
    f.render_widget(Paragraph::new(lines), area);
}
