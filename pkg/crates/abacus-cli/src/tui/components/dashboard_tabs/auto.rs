//! Auto Tab —— JobRunner 自动化健康状态
//!
//! ## 渲染内容（动态高度，取决于 jobs 数量）
//!
//! 未启用时（2 行）：
//! ```text
//!  ⚡ 自动化未启用
//!  ~/.abacus/auto.yaml
//! ```
//!
//! 启用时（概览 + 任务列表 + uptime）：
//! ```text
//!  Jobs 5  Running 2  Failed 0
//!   ⏰ ▸ backup-db                  ×42 ♻ 3秒前 1.2s
//!   👁 · watch-config               ×0
//!   ⚡ ✗ on-deploy                  ×3/2 ♻ 5分前|✕400ms
//!   Uptime  2h15m
//! ```
//!
//! ## State 依赖
//!
//! - `auto_health` —— 由 JobRunner.spawn 推送, run.rs 主循环 drain
//! - `dashboard_scroll` —— 上下滚动
//!
//! ## 图标语义
//!
//! - `⏰` Cron job  /  `👁` Watch job  /  `⚡` Event job
//! - `·` Idle / `▸` Running / `✗` Failed / `⏸` Paused

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use abacus_core::auto::{JobKind, JobState};
use abacus_ui_kit::{DashboardTab, SectionContext};

use crate::tui::components::section_ctx::downcast_app_state;

pub struct AutoTab;

impl Default for AutoTab {
    fn default() -> Self {
        Self
    }
}

impl DashboardTab for AutoTab {
    fn id(&self) -> &str {
        "auto"
    }

    fn label(&self) -> &str {
        "dash.auto"
    }

    fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect) {
        let Some(state) = downcast_app_state(ctx) else {
            return;
        };
        let theme = ctx.theme();
        let muted = Style::default().fg(theme.muted);
        let health = &state.auto_health;
        let mut lines: Vec<Line> = Vec::new();

        if !health.runner_active || health.jobs.is_empty() {
            // 未启用状态 (紧凑: 2 行不留空)
            lines.push(Line::from(vec![
                Span::styled(" ⚡ ", muted),
                Span::styled(crate::tui::i18n::t("dash.auto_disabled"), muted),
            ]));
            lines.push(Line::from(vec![Span::styled(
                " ~/.abacus/auto.yaml",
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            )]));
        } else {
            // 概览行 (紧凑)
            let total = health.total_count();
            let active = health.active_count();
            let failed = health.failed_count();
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", crate::tui::i18n::t("dash.jobs")),
                    muted,
                ),
                Span::styled(format!("{}", total), Style::default().fg(theme.text)),
                Span::styled(
                    format!("  {} ", crate::tui::i18n::t("dash.running")),
                    muted,
                ),
                Span::styled(format!("{}", active), Style::default().fg(theme.success)),
                if failed > 0 {
                    Span::styled(
                        format!("  {} {}", crate::tui::i18n::t("dash.failed"), failed),
                        Style::default().fg(theme.error),
                    )
                } else {
                    Span::raw("")
                },
            ]));

            // 任务列表 (最多 area.height - 3 条)
            let max_jobs = area.height.saturating_sub(3) as usize;
            for job in health.jobs.iter().take(max_jobs) {
                let kind_icon = match job.kind {
                    JobKind::Cron => "⏰",
                    JobKind::Watch => "👁",
                    JobKind::Event => "⚡",
                };
                let state_style = match job.state {
                    JobState::Idle => Style::default().fg(theme.muted),
                    JobState::Running => Style::default().fg(theme.success),
                    JobState::Failed => Style::default().fg(theme.error),
                    JobState::Paused => Style::default().fg(theme.gold),
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

                // 最近执行时间(相对)
                let last_run_str = job.last_run.map(|instant| {
                    let secs = instant.elapsed().as_secs();
                    if secs < 60 {
                        format!("{}{}", secs, crate::tui::i18n::t("time.sec_ago"))
                    } else if secs < 3600 {
                        format!("{}{}", secs / 60, crate::tui::i18n::t("time.min_ago"))
                    } else {
                        format!("{}{}", secs / 3600, crate::tui::i18n::t("time.hour_ago"))
                    }
                });
                // 耗时展示
                let dur_str = job.last_duration_ms.map(|ms| {
                    if ms < 1000 {
                        format!("{}ms", ms)
                    } else {
                        format!("{:.1}s", ms as f64 / 1000.0)
                    }
                });

                let mut spans = vec![
                    Span::raw("  "),
                    Span::raw(kind_icon),
                    Span::styled(format!(" {} ", state_char), state_style),
                    Span::styled(label, Style::default().fg(theme.text)),
                ];
                let stats_str = match (last_run_str, dur_str) {
                    (Some(t), Some(d)) => format!(
                        " {}/{} {}",
                        job.run_count,
                        job.fail_count,
                        if job.fail_count > 0 {
                            format!("♻ {}|✕{}", t, d)
                        } else {
                            format!("♻ {} {}", t, d)
                        }
                    ),
                    (Some(t), None) => format!(" ♻ {}", t),
                    _ => {
                        if job.run_count > 0 {
                            format!(" ×{}", job.run_count)
                        } else {
                            String::new()
                        }
                    }
                };
                if !stats_str.is_empty() {
                    spans.push(Span::styled(
                        stats_str,
                        Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
                    ));
                }
                lines.push(Line::from(spans));
            }

            // Uptime
            if health.uptime.as_secs() > 0 {
                lines.push(Line::raw(""));
                let mins = health.uptime.as_secs() / 60;
                let uptime_str = if mins >= 60 {
                    format!("{}h{}m", mins / 60, mins % 60)
                } else {
                    format!("{}m", mins)
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {}  ", crate::tui::i18n::t("dash.uptime")),
                        muted,
                    ),
                    Span::styled(uptime_str, Style::default().fg(theme.text)),
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
                    Span::styled(
                        format!("{} {}", scroll, crate::tui::i18n::t("dash.jobs")),
                        Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
                    ),
                ]);
            }
        }
        f.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::components::section_ctx::AppContext;
    use crate::tui::state::{AbacusMode, AppState};

    #[test]
    fn auto_tab_metadata() {
        let t = AutoTab;
        assert_eq!(t.id(), "auto");
    }

    #[test]
    fn auto_tab_renders_disabled() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let tab = AutoTab;
        let state = AppState::new(AbacusMode::Clarify); // auto_health 默认 runner_active=false
        let ctx = AppContext::new(&state);
        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 3);
                tab.render(f, &ctx, area);
            })
            .unwrap();
    }
}
