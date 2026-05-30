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
        (DashboardTab::Health, "健康"),
        (DashboardTab::Auto, "自动化"),
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

/// 健康仪表盘：模型 + 上下文 + 轮次/压缩（紧凑 2-3 行）
///
/// V40 重设计：去掉进度条，纯文本紧凑排列
/// 第一行：模型名 · 思考档位
/// 第二行：⬡ 上下文已用/总量 · turns N · cmp N
fn render_dashboard_health(f: &mut ratatui::Frame, state: &AppState, area: Rect) {
    use crate::tui::components::bars::format_ctx;

    let mut lines: Vec<Line> = Vec::new();
    let muted = Style::default().fg(state.theme.muted);
    let dim = Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM);

    // 第一行：provider(状态) · 模型名 · 思考档位
    // provider 来自 config.yaml providers[].id（真实配置，非推断）
    // 状态指示：● = 可用（discover 成功），✗ = 不可用，— = 未检测
    let provider_label = if state.active_provider_id.is_empty() { "—" } else { &state.active_provider_id };
    let (status_icon, status_color) = match state.provider_statuses.iter()
        .find(|(id, _, _)| id == &state.active_provider_id)
    {
        Some((_, true, _)) => ("●", state.theme.success),
        Some((_, false, _)) => ("✗", state.theme.error),
        None => ("·", state.theme.muted),
    };
    lines.push(Line::from(vec![
        Span::styled(format!("  {}{} ", status_icon, provider_label), Style::default().fg(status_color)),
        Span::styled(&state.model_name, Style::default().fg(state.theme.accent)),
        Span::styled(" · ", dim),
        Span::styled(&state.thinking_depth, Style::default().fg(state.theme.text)),
    ]));

    // 第二行：⬡ 已用/系统设定/LLM最大 · turns N · cmp N
    let raw_used = if state.ctx_live_tokens > 0 {
        state.ctx_live_tokens as usize
    } else {
        state.session_tokens.latest_prompt_tokens as usize
    };
    let configured = state.context_window;       // 系统设定有效窗口
    let model_max = state.model_max_context;     // LLM 物理最大
    // 防御性 clamp：显示值不超过系统设定窗口
    let used = if configured > 0 { raw_used.min(configured) } else { raw_used };
    let ctx_color = match (used * 100).checked_div(configured).unwrap_or(0) {
        0..=49 => state.theme.success,
        50..=79 => state.theme.gold,
        _ => state.theme.error,
    };

    // 格式：⬡ 19.9K/500K/1M  (已用/设定/最大)
    // 当 configured == model_max 时省略最后一段（无 ratio 配置）
    let ctx_text = if configured == model_max || model_max == 0 {
        format!("{}/{}", format_ctx(used), format_ctx(configured))
    } else {
        format!("{}/{}/{}", format_ctx(used), format_ctx(configured), format_ctx(model_max))
    };

    let mut row: Vec<Span> = vec![
        Span::styled("  ⬡ ", Style::default().fg(ctx_color)),
        Span::styled(ctx_text, Style::default().fg(ctx_color)),
        Span::styled(format!(" · turns {}", state.turn_count), muted),
    ];
    let comp = state.session_tokens.compress_count;
    if comp > 0 {
        row.push(Span::styled(format!(" · cmp {}", comp), muted));
    }
    lines.push(Line::from(row));

    let visible = area.height as usize;
    if lines.len() > visible { lines.truncate(visible); }
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
        // 未启用状态
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("  ⚡ ", muted),
            Span::styled("未启用", muted),
        ]));
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("  配置 ~/.abacus/auto.yaml", Style::default().fg(state.theme.muted).add_modifier(Modifier::DIM)),
        ]));
    } else {
        // 概览行
        let total = health.total_count();
        let active = health.active_count();
        let failed = health.failed_count();
        lines.push(Line::from(vec![
            Span::styled("  任务  ", muted),
            Span::styled(format!("{}", total), Style::default().fg(state.theme.text)),
            Span::styled("  运行  ", muted),
            Span::styled(format!("{}", active), Style::default().fg(state.theme.success)),
            if failed > 0 {
                Span::styled(format!("  失败 {}", failed), Style::default().fg(state.theme.error))
            } else {
                Span::raw("")
            },
        ]));
        lines.push(Line::raw(""));

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
            let last_run_str = job.last_run.map(|t| {
                let secs = t.elapsed().as_secs();
                if secs < 60 { format!("{}s前", secs) }
                else if secs < 3600 { format!("{}m前", secs / 60) }
                else { format!("{}h前", secs / 3600) }
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
                Span::styled("  运行  ", muted),
                Span::styled(uptime_str, Style::default().fg(state.theme.text)),
            ]));
        }
    }

    let visible = area.height as usize;
    if lines.len() > visible { lines.truncate(visible); }
    f.render_widget(Paragraph::new(lines), area);
}
