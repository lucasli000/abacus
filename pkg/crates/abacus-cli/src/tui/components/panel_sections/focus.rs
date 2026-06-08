//! Focus Section —— 当前关注信息（按场景分支显示）
//!
//! ## 4 个分支
//!
//! | 触发条件 | 内容 |
//! |---|---|
//! | `processing_phase` 以 `"planning"` 开头 | Plan 任务卡片 + 进度条 ━╌ |
//! | `processing_phase` 以 `"team"` 开头 | Team 任务卡片 + DAG 依赖标记 |
//! | `mode == Meeting` | 专家列表 + 阶段状态（发言中/已结束/等待）|
//! | 默认（A+E 融合）| 流式 thinking 预览（顶部）+ 会话快照（session_goal + last_user + last_ai + model stats）|
//!
//! ## State 依赖
//!
//! - `processing_phase` —— 分支选择
//! - `mode` —— Meeting 分支判定
//! - `tasks` —— Plan/Team 任务列表
//! - `experts` —— Meeting 专家列表
//! - `session_goal` —— Plan/默认显示
//! - `streaming_thinking` —— 默认分支顶部预览
//! - `messages` —— 默认分支 last_user / last_ai 预览
//! - `tool_records` —— 默认分支调用统计
//! - `session_tokens.per_model` —— 默认分支模型分布
//! - `is_streaming` + `turn_count` —— visible 判定

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use abacus_ui_kit::{Section, SectionContext};

use crate::tui::components::section_ctx::downcast_app_state;
use crate::tui::i18n::t;
use crate::tui::state::{AbacusMode, AppState, ExpertStatus, MsgContent, MsgRole, TaskStatus, ToolStatus};

use super::content_width;

pub struct FocusSection;

impl Default for FocusSection {
    fn default() -> Self {
        Self
    }
}

impl Section for FocusSection {
    fn id(&self) -> &str {
        "focus"
    }

    fn title(&self) -> &str {
        "panel.focus"
    }

    fn min_height(&self) -> u16 {
        3
    }

    fn preferred_height(&self, available: u16) -> u16 {
        available.max(self.min_height())
    }

    fn visible(&self, ctx: &dyn SectionContext) -> bool {
        let Some(state) = downcast_app_state(ctx) else {
            return true; // 第三方 ctx 默认显示
        };
        state.is_streaming_active()
            || !state.processing_phase.is_empty()
            || state.mode == AbacusMode::Meeting
            || state.turn_count > 0
    }

    fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect) {
        let Some(state) = downcast_app_state(ctx) else {
            return;
        };
        let theme = ctx.theme();
        let w = content_width(area.width);

        // 分支路由 —— 顺序: Plan > Team > Meeting > 默认（融合）
        if state.processing_phase.starts_with("planning") {
            render_planning(f, state, theme, area, w);
        } else if state.processing_phase.starts_with("team") {
            render_team(f, state, theme, area, w);
        } else if state.mode == AbacusMode::Meeting {
            render_meeting(f, state, theme, area, w);
        } else {
            render_default(f, state, theme, area, w);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 子渲染函数 —— 每个分支独立测试
// ═══════════════════════════════════════════════════════════════════════════

/// 通用 header 渲染器（带 ─ 填充）
fn push_header(lines: &mut Vec<Line>, label: &str, theme: &abacus_ui_kit::Theme, area_w: usize) {
    let dim = Style::default().fg(theme.muted).add_modifier(Modifier::DIM);
    let muted = Style::default().fg(theme.muted);
    let fill = area_w.saturating_sub(label.len() + 5).min(14);
    lines.push(Line::from(vec![
        Span::styled("  ─ ", dim),
        Span::styled(label.to_string(), muted),
        Span::styled(format!(" {}", "─".repeat(fill)), dim),
    ]));
}

/// Plan 分支 —— 任务卡片 + ━╌ 进度条
fn render_planning(
    f: &mut Frame,
    state: &AppState,
    theme: &abacus_ui_kit::Theme,
    area: Rect,
    w: usize,
) {
    let dim = Style::default().fg(theme.muted).add_modifier(Modifier::DIM);
    let muted = Style::default().fg(theme.muted);
    let txt = Style::default().fg(theme.text);
    let ok = Style::default().fg(theme.success);
    let mut lines: Vec<Line> = Vec::new();

    let total = state.tasks.len();
    let done = state
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Done)
        .count();
    push_header(
        &mut lines,
        &format!("Focus · {} {}/{}", t("focus.planning"), done, total),
        theme,
        w,
    );

    if let Some(ref goal) = state.session_goal {
        lines.push(Line::from(vec![
            Span::styled("    ", dim),
            Span::styled(goal.chars().take(w).collect::<String>(), txt),
        ]));
    }

    for task in state.tasks.iter().take(4) {
        let (icon, color) = task_icon_color(task.status, theme);
        lines.push(Line::from(vec![
            Span::styled(format!("    {} ", icon), Style::default().fg(color)),
            Span::styled(
                task.title.chars().take(w.saturating_sub(6)).collect::<String>(),
                txt,
            ),
        ]));
    }

    if total > 0 {
        let bw = w.saturating_sub(8).min(10);
        let filled = (done * bw / total).min(bw);
        lines.push(Line::from(vec![
            Span::styled("    ", dim),
            Span::styled("━".repeat(filled), ok),
            Span::styled("╌".repeat(bw - filled), dim),
            Span::styled(format!(" {}/{}", done, total), muted),
        ]));
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// Team 分支 —— 任务卡片 + 依赖标记
fn render_team(
    f: &mut Frame,
    state: &AppState,
    theme: &abacus_ui_kit::Theme,
    area: Rect,
    w: usize,
) {
    let dim = Style::default().fg(theme.muted).add_modifier(Modifier::DIM);
    let txt = Style::default().fg(theme.text);
    let mut lines: Vec<Line> = Vec::new();

    let total = state.tasks.len();
    let done = state
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Done)
        .count();
    push_header(
        &mut lines,
        &format!("Focus · {} {}/{}", t("focus.team"), done, total),
        theme,
        w,
    );

    for task in state.tasks.iter().take(4) {
        let (icon, color) = task_icon_color(task.status, theme);
        let extra = if !task.deps.is_empty() {
            format!(" ← {}", task.deps.join(","))
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("    {} ", icon), Style::default().fg(color)),
            Span::styled(
                task.title.chars().take(w.saturating_sub(6)).collect::<String>(),
                txt,
            ),
            Span::styled(extra, dim),
        ]));
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// Meeting 分支 —— 专家列表 + 阶段状态
fn render_meeting(
    f: &mut Frame,
    state: &AppState,
    theme: &abacus_ui_kit::Theme,
    area: Rect,
    w: usize,
) {
    let dim = Style::default().fg(theme.muted).add_modifier(Modifier::DIM);
    let txt = Style::default().fg(theme.text);
    let mut lines: Vec<Line> = Vec::new();

    let total = state.experts.len();
    let active = state
        .experts
        .iter()
        .filter(|e| matches!(e.status, ExpertStatus::Active))
        .count();
    push_header(
        &mut lines,
        &format!("Focus · 会诊 {}/{}", active, total),
        theme,
        w,
    );

    for e in &state.experts {
        let (icon, color) = match e.status {
            ExpertStatus::Active => ("▸", theme.success),
            ExpertStatus::Done => ("✓", theme.success),
            ExpertStatus::Idle => ("·", theme.muted),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("    {} ", icon), Style::default().fg(color)),
            Span::styled(
                format!("{:<12}", e.name.chars().take(12).collect::<String>()),
                txt,
            ),
            Span::styled(e.domain.chars().take(8).collect::<String>(), dim),
        ]));
    }

    let phase = if total == 0 {
        t("focus.waiting")
    } else if active > 0 {
        t("focus.speaking")
    } else {
        t("focus.done")
    };
    lines.push(Line::from(vec![
        Span::styled("    ", dim),
        Span::styled(
            format!("{}: {}", t("focus.phase"), phase),
            Style::default().fg(theme.accent),
        ),
    ]));

    f.render_widget(Paragraph::new(lines), area);
}

/// 默认分支 —— 流式 thinking 预览（顶部）+ 会话快照（session_goal + last_user + last_ai + model stats）
fn render_default(
    f: &mut Frame,
    state: &AppState,
    theme: &abacus_ui_kit::Theme,
    area: Rect,
    w: usize,
) {
    let muted = Style::default().fg(theme.muted);
    let dim = Style::default().fg(theme.muted).add_modifier(Modifier::DIM);
    let txt = Style::default().fg(theme.text);
    let gold = Style::default().fg(theme.gold);
    let mut lines: Vec<Line> = Vec::new();

    // A+E 融合: 顶部 streaming thinking 预览
    let active_thinking = state.active_llm_thinking();
    if state.is_streaming_active() && !active_thinking.is_empty() {
        let think_lines: Vec<&str> = active_thinking
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let total = think_lines.len();
        let visible = if total > 3 {
            &think_lines[total - 3..]
        } else {
            &think_lines[..]
        };
        for l in visible {
            lines.push(Line::from(vec![
                Span::styled("    ", dim),
                Span::styled(
                    crate::tui::util::truncate_to_width(l, w.saturating_sub(2)),
                    Style::default().fg(theme.accent),
                ),
            ]));
        }
        if total > 3 {
            lines.push(Line::from(vec![
                Span::styled("    ", dim),
                Span::styled(format!("… {}行", total), dim),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled("    ", dim),
            Span::styled("─".repeat(w.saturating_sub(4)), dim),
        ]));
    }

    // header
    let title = if state.is_streaming_active() {
        format!("Focus · {}", t("focus.processing"))
    } else {
        format!("Focus \u{00b7} {} {}", t("panel.round"), state.turn_count)
    };
    push_header(&mut lines, &title, theme, w);

    // session_goal
    if let Some(ref goal) = state.session_goal {
        lines.push(Line::from(vec![
            Span::styled("    ", dim),
            Span::styled(
                format!(
                    "  {}: {}",
                    t("panel.session_goal"),
                    goal.chars().take(w.saturating_sub(8)).collect::<String>()
                ),
                txt,
            ),
        ]));
    }

    // 最近 user / ai 消息预览
    if state.turn_count > 0 {
        if let Some(user_msg) = state
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, MsgRole::User))
        {
            let preview: String = user_msg
                .parts
                .iter()
                .filter_map(|p| {
                    if let MsgContent::Stream(s) = p {
                        Some(s.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(w.saturating_sub(10))
                .collect();
            if !preview.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("    ", dim),
                    Span::styled(format!("  {}: {}", t("panel.last_user"), preview), txt),
                ]));
            }
        }
        if let Some(ai_msg) = state
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, MsgRole::Session))
        {
            let preview: String = ai_msg
                .parts
                .iter()
                .filter_map(|p| {
                    if let MsgContent::Stream(s) = p {
                        Some(s.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(w.saturating_sub(10))
                .collect();
            if !preview.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("    ", dim),
                    Span::styled(format!("  {}: {}", t("panel.last_ai"), preview), txt),
                ]));
            }
        }
    }

    // 模型分布 + 调用统计 + cost
    let tc = state.tool_records.len();
    let sc = state
        .tool_records
        .iter()
        .filter(|r| matches!(r.status, ToolStatus::Success))
        .count();
    let mut model_parts = vec![Span::styled("    ", dim)];
    let mut models: Vec<(String, u32)> = state
        .session_tokens
        .per_model
        .iter()
        .map(|(id, stats)| (id.clone(), stats.turns))
        .collect();
    models.sort_by_key(|(_, turns)| std::cmp::Reverse(*turns));
    for (i, (id, turns)) in models.iter().enumerate() {
        if i >= 2 {
            model_parts.push(Span::styled(format!("+{}", models.len() - 2), muted));
            break;
        }
        model_parts.push(Span::styled(
            format!(
                "{} {} {}",
                abacus_types::lookup_model_or_default(id)
                    .display_name
                    .chars()
                    .take(12)
                    .collect::<String>(),
                turns,
                t("panel.round")
            ),
            Style::default().fg(theme.accent),
        ));
        if i == 0 && models.len() > 1 {
            model_parts.push(Span::styled("  ", dim));
        }
    }
    if tc > 0 {
        model_parts.push(Span::styled(
            format!("  {} {} {}%", t("panel.calls"), tc, sc * 100 / tc),
            muted,
        ));
    }
    if state.session_tokens.cost_cny > 0.001 {
        model_parts.push(Span::styled(
            format!("  \u{00a5}{:.2}", state.session_tokens.cost_cny),
            gold,
        ));
    }
    lines.push(Line::from(model_parts));

    f.render_widget(Paragraph::new(lines), area);
}

/// Task 图标 + 颜色映射 —— Plan/Team 共享
fn task_icon_color(status: TaskStatus, theme: &abacus_ui_kit::Theme) -> (&'static str, ratatui::style::Color) {
    match status {
        TaskStatus::Done => ("✓", theme.success),
        TaskStatus::InProgress => ("›", theme.accent),
        TaskStatus::Blocked => ("!", theme.error),
        TaskStatus::Pending => ("·", theme.muted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::components::section_ctx::AppContext;

    #[test]
    fn focus_section_metadata() {
        let s = FocusSection;
        assert_eq!(s.id(), "focus");
        assert_eq!(s.min_height(), 3);
    }

    #[test]
    fn focus_visible_when_streaming() {
        let s = FocusSection;
        let mut state = AppState::new(AbacusMode::Clarify);
        state.begin_streaming_session();
        let ctx = AppContext::new(&state);
        assert!(s.visible(&ctx));
    }

    #[test]
    fn focus_visible_when_meeting_mode() {
        let s = FocusSection;
        let state = AppState::new(AbacusMode::Meeting);
        let ctx = AppContext::new(&state);
        assert!(s.visible(&ctx));
    }

    #[test]
    fn focus_invisible_when_idle_clarify() {
        let s = FocusSection;
        let state = AppState::new(AbacusMode::Clarify);
        let ctx = AppContext::new(&state);
        // 空状态 Clarify: 不 streaming + 无 processing_phase + 非 Meeting + turn_count=0
        assert!(!s.visible(&ctx));
    }

    #[test]
    fn focus_section_renders_planning_branch() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let s = FocusSection;
        let mut state = AppState::new(AbacusMode::Clarify);
        state.processing_phase = "planning".to_string();
        let ctx = AppContext::new(&state);
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 6);
                s.render(f, &ctx, area);
            })
            .unwrap();
    }

    #[test]
    fn focus_section_renders_meeting_branch() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let s = FocusSection;
        let state = AppState::new(AbacusMode::Meeting);
        let ctx = AppContext::new(&state);
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 6);
                s.render(f, &ctx, area);
            })
            .unwrap();
    }
}
