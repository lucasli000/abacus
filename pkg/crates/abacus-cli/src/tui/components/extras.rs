//! Extra render functions: expert list, task kanban, global background, shortcuts hints.
//!
//! Extracted from mod.rs to reduce file size. All functions are `pub(super)` and
//! re-exported via `pub use extras::*` in the parent module.
//!
//! ## References
//! - Consumed by: `crate::tui::components` (re-export) → layout/rendering code
//! - Depends on: `AppState`, `ExpertStatus`, `TaskStatus`, `Focus`, `Theme`, `TextRole`

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::tui::i18n::t;
use crate::tui::state::{AppState, ExpertStatus, TaskStatus};
use crate::tui::theme::TextRole;

// ════════════════════════════════════════════════════════════════
// ExpertList — 专家列表
// ════════════════════════════════════════════════════════════════

pub fn render_expert_list(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .title(t("label.expert"))
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
            Span::raw("  "),
            Span::styled(
                &expert.domain,
                Style::default()
                    .fg(state.theme.muted)
                    .add_modifier(Modifier::DIM),
            ),
        ]));

        // V28.7: confidence == 0.0 → "未评估"，>0 → 百分比（orchestrator 暂无评估机制时不造伪数据）
        let conf_label = if expert.confidence > 0.0 {
            format!("{}: {:.0}%", t("field.confidence"), expert.confidence * 100.0)
        } else {
            format!("{}: —", t("field.confidence"))
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(conf_label, Style::default().fg(state.theme.muted)),
        ]));

        lines.push(Line::raw(""));
    }

    if lines.is_empty() {
        lines.push(Line::styled(
            t("empty.experts"),
            Style::default().fg(state.theme.muted),
        ));
        lines.push(Line::styled(
            t("empty.invite_hint"),
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
        .title(t("label.kanban"))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(state.theme.mode));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    if state.tasks.is_empty() {
        lines.push(Line::styled(
            t("empty.tasks"),
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
                Span::raw("  "),
                Span::styled(
                    format!("{}: {}", t("field.owner"), task.assignee),
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
                    Span::raw("  "),
                    Span::styled(
                        format!("{}: {}", t("field.deps"), task.deps.join(", ")),
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
    // ratatui 双缓冲：每帧新 buffer 必须填充背景，否则未被 widget 覆盖的 cell
    // 在 diff 时会从"有色"变"无色"导致背景闪烁/消失。
    // 此操作对 200x50 终端约 10000 次 cell 写入（~0.1ms），不构成瓶颈。
    let area = f.area();
    let bg = state.theme.bg;
    let fg = state.theme.text;
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut f.buffer_mut()[(x, y)];
            cell.set_symbol(" ");
            cell.set_bg(bg);
            cell.set_fg(fg);
        }
    }
}


// ════════════════════════════════════════════════════════════════
/// V40: 仪表盘 — 双 tab（健康 | 自动化）
///
/// 替代旧的命令提示框。展示实时健康数据和自动化状态。
/// 引用关系：被 modes/common.rs 布局调用（原 render_shortcuts_hints 入口保留兼容）
/// 生命周期：每帧渲染
pub fn render_shortcuts_hints(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    use crate::tui::state::DashboardTab;
    use crate::tui::state::Focus;

    if area.width < 14 || area.height < 4 {
        return;
    }

    let is_focused = state.focus == Focus::CommandHint;
    let block = Block::default()
        .border_type(BorderType::Rounded)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(state.theme.border));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // 焦点上边框
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
        let top_overlay = Block::default()
            .borders(Borders::TOP)
            .border_type(BorderType::Thick)
            .border_style(top_style);
        f.render_widget(top_overlay, top_segment);
    }

    // Tab header
    let mut header_spans: Vec<Span> = Vec::new();
    let tabs: &[(DashboardTab, &str)] = &[
        (DashboardTab::Health, t("dash.health")),
        (DashboardTab::Auto, t("dash.auto")),
    ];
    for (i, (tab, label)) in tabs.iter().enumerate() {
        if i > 0 {
            header_spans.push(Span::styled("│", Style::default().fg(state.theme.border)));
        }
        if state.dashboard_tab == *tab {
            header_spans.push(Span::styled(
                format!("▸{}", label),
                Style::default().fg(state.theme.accent).add_modifier(Modifier::BOLD),
            ));
        } else {
            header_spans.push(Span::styled(
                format!(" {}", label),
                Style::default().fg(state.theme.muted),
            ));
        }
    }
    let header_area = Rect::new(inner.x, inner.y, inner.width, 1);
    f.render_widget(Paragraph::new(Line::from(header_spans)), header_area);

    // Content
    let content = Rect::new(inner.x, inner.y + 1, inner.width, inner.height.saturating_sub(1));
    match state.dashboard_tab {
        DashboardTab::Health => render_dashboard_health(f, state, content),
        DashboardTab::Auto => render_dashboard_auto(f, state, content),
    }
}

/// Hooks 仪表盘：展示 ScriptHook 注册/触发/失败状态
fn render_dashboard_health(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    let muted = Style::default().fg(state.theme.muted);
    let dim = Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM);
    let txt = Style::default().fg(state.theme.text);
    let ok = Style::default().fg(state.theme.success);

    // 示意数据——实际应从 state.hook_stats 读取
    lines.push(Line::from(vec![
        Span::styled(" \u{25c7} ", muted),
        Span::styled(format!("{} 0  {}", t("panel.hook_registered"), t("panel.hook_triggered")), txt),
        Span::styled(" 0  ", muted),
        Span::styled(format!("{} 0", t("panel.hook_failed")), ok),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" \u{2713} ", muted),
        Span::styled(format!("{}: --", t("panel.hook_last")), dim),
    ]));

    // 滚动
    let scroll = state.dashboard_scroll;
    let vis = area.height as usize;
    if lines.len() > vis {
        let end = lines.len().saturating_sub(scroll);
        let start = end.saturating_sub(vis);
        lines = lines[start..end].to_vec();
        if scroll > 0 && !lines.is_empty() {
            lines[0] = Line::from(vec![
                Span::styled(" \u{2191} ", muted),
                Span::styled(format!("{} {}", scroll, t("dash.jobs")), dim),
            ]);
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// 自动化仪表盘：显示 JobRunner 推送的健康数据
///
/// 引用关系：读取 state.auto_health（由 run.rs 从 JobRunner health_rx 更新）
fn render_dashboard_auto(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    use abacus_core::auto::{JobState, JobKind};

    let muted = Style::default().fg(state.theme.muted);
    let health = &state.auto_health;
    let mut lines: Vec<Line> = Vec::new();

    if !health.runner_active || health.jobs.is_empty() {
        // 未启用状态（紧凑：2行不留空）
        lines.push(Line::from(vec![
            Span::styled(" ⚡ ", muted),
            Span::styled(t("dash.auto_disabled"), muted),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" ~/.abacus/auto.yaml", Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM)),
        ]));
    } else {
        // 概览行（紧凑）
        let total = health.total_count();
        let active = health.active_count();
        let failed = health.failed_count();
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", t("dash.jobs")), muted),
            Span::styled(format!("{}", total), Style::default().fg(state.theme.text)),
            Span::styled(format!("  {} ", t("dash.running")), muted),
            Span::styled(format!("{}", active), Style::default().fg(state.theme.success)),
            if failed > 0 {
                Span::styled(format!("  {} {}", t("dash.failed"), failed), Style::default().fg(state.theme.error))
            } else {
                Span::raw("")
            },
        ]));

        // 任务列表（最多显示 area.height - 3 条）
        let max_jobs = area.height.saturating_sub(3) as usize;
        for job in health.jobs.iter().take(max_jobs) {
            let kind_icon = match job.kind {
                JobKind::Cron => "⏰",
                JobKind::Watch => "👁",
                JobKind::Event => "⚡",
            };
            let state_style = match job.state {
                JobState::Idle => Style::default().fg(state.theme.muted),
                JobState::Running => Style::default().fg(state.theme.success),
                JobState::Failed => Style::default().fg(state.theme.error),
                JobState::Paused => Style::default().fg(state.theme.gold),
            };
            let state_char = match job.state {
                JobState::Idle => "·",
                JobState::Running => "▸",
                JobState::Failed => "✗",
                JobState::Paused => "⏸",
            };

            // 截断 label 到可用宽度
            let label_max = area.width.saturating_sub(20) as usize;
            let label: String = job.label.chars().take(label_max).collect();

            // 最近执行时间（相对）
            let last_run_str = job.last_run.map(|instant| {
                let secs = instant.elapsed().as_secs();
                if secs < 60 { format!("{}{}", secs, t("time.sec_ago")) }
                else if secs < 3600 { format!("{}{}", secs / 60, t("time.min_ago")) }
                else { format!("{}{}", secs / 3600, t("time.hour_ago")) }
            });
            // 耗时展示
            let dur_str = job.last_duration_ms.map(|ms| {
                if ms < 1000 { format!("{}ms", ms) }
                else { format!("{:.1}s", ms as f64 / 1000.0) }
            });

            let mut spans = vec![
                Span::raw("  "),
                Span::raw(kind_icon),
                Span::styled(format!(" {} ", state_char), state_style),
                Span::styled(label, Style::default().fg(state.theme.text)),
            ];
            // 运行次数和耗时
            let stats_str = match (last_run_str, dur_str) {
                (Some(t), Some(d)) => format!(" {}/{} {}", job.run_count, job.fail_count, if job.fail_count > 0 { format!("♻ {}|✕{}", t, d) } else { format!("♻ {} {}", t, d) }),
                (Some(t), None) => format!(" ♻ {}", t),
                _ => if job.run_count > 0 { format!(" ×{}", job.run_count) } else { String::new() },
            };
            if !stats_str.is_empty() {
                spans.push(Span::styled(stats_str, Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM)));
            }
            lines.push(Line::from(spans));
        }

        // 运行时长
        if health.uptime.as_secs() > 0 {
            lines.push(Line::raw(""));
            let mins = health.uptime.as_secs() / 60;
            let uptime_str = if mins >= 60 {
                format!("{}h{}m", mins / 60, mins % 60)
            } else {
                format!("{}m", mins)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {}  ", t("dash.uptime")), muted),
                Span::styled(uptime_str, Style::default().fg(state.theme.text)),
            ]));
        }
    }

    // 滚动
    let scroll = state.dashboard_scroll;
    let vis = area.height as usize;
    if lines.len() > vis {
        let end = lines.len().saturating_sub(scroll);
        let start = end.saturating_sub(vis);
        lines = lines[start..end].to_vec();
        if scroll > 0 && !lines.is_empty() {
            lines[0] = Line::from(vec![
                Span::styled(" ↑ ", muted),
                Span::styled(format!("{} {}", scroll, t("dash.jobs")), Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM)),
            ]);
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}
