//! LLM Section —— 当前 provider + model + ctx% + token + cost
//!
//! ## 渲染内容（3 行）
//!
//! ```text
//!  ─ LLM ──────────────
//!  ● deepseek  v4-flash  thinking:high
//!     ▓▓▓▓▓░░░░░  45%  9.2k/20k
//!     输入 12.1k  输出 856  缓存 32%  费用 ¥0.04
//! ```
//!
//! ## State 依赖
//!
//! - `active_provider_id` —— 头行 provider 标签
//! - `provider_statuses` —— 头行 status_icon 颜色（●/✗/·）
//! - `model_name` —— 头行 model display name
//! - `thinking_depth` —— 头行 thinking 深度
//! - `ctx_live_tokens` + `context_window` —— 进度条 + 百分比
//! - `session_tokens.latest_prompt_tokens` —— ctx_live_tokens 兜底来源
//! - `session_tokens.prompt_tokens / completion_tokens / cached_tokens / cost_cny` —— 统计行
//! - `session_tokens.compress_count` —— 压缩次数（可选 trailing 标记）
//!
//! ## 视觉契约
//!
//! - ctx% < 50% → success 色; 50-80% → gold; > 80% → error
//! - cost > ¥0.001 时才显示 cost 字段（避免 ¥0.00 噪声）

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use abacus_ui_kit::{Section, SectionContext};

use crate::tui::components::bars::format_ctx;
use crate::tui::components::section_ctx::downcast_app_state;

use super::{content_width, render_section_header};

/// LLM 状态 Section
///
/// Zero-sized struct —— 无状态, 跨帧复用
pub struct LlmSection;

impl Default for LlmSection {
    fn default() -> Self {
        Self
    }
}

impl Section for LlmSection {
    fn id(&self) -> &str {
        "llm"
    }
    fn order(&self) -> u32 {
        10
    }

    fn title(&self) -> &str {
        // i18n key, 由实现侧自行翻译
        "panel.llm"
    }

    fn min_height(&self) -> u16 {
        4 // header + 3 内容行
    }

    fn preferred_height(&self, _available: u16) -> u16 {
        4
    }

    fn render(&self, f: &mut Frame, ctx: &dyn SectionContext, area: Rect) {
        let Some(state) = downcast_app_state(ctx) else {
            return;
        };
        let theme = ctx.theme();
        let w = content_width(area.width);
        let dim = Style::default().fg(theme.muted).add_modifier(Modifier::DIM);

        let mut lines: Vec<Line> = Vec::new();
        render_section_header(&mut lines, crate::tui::i18n::t("panel.llm"), w, theme);

        // ── 无 provider/model 时显示"未连接"，提前返回 ──
        if state.active_provider_id.is_empty() && state.model_name.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("    \u{25cc} ", Style::default().fg(theme.muted)),
                Span::styled(crate::tui::i18n::t("panel.not_connected"), Style::default().fg(theme.muted)),
            ]));
            f.render_widget(Paragraph::new(lines), area);
            return;
        }

        // ── 头行: provider + model + thinking ──
        // 从 provider_statuses 查找 display_name（第三字段），fallback 到 raw ID
        let provider_info = state
            .provider_statuses
            .iter()
            .find(|(id, _, _)| id == &state.active_provider_id);
        let provider_label = if state.active_provider_id.is_empty() {
            "\u{2014}"
        } else if let Some((_, _, Some(ref name))) = provider_info {
            name.as_str()
        } else {
            &state.active_provider_id
        };
        let (status_icon, status_color) = match provider_info {
            Some((_, true, _)) => ("\u{25cf}", theme.success),
            Some((_, false, _)) => ("\u{2717}", theme.error),
            None => ("\u{00b7}", theme.muted),
        };
        // model: 优先用原始 model_name，若 lookup 成功则用 display_name
        // V42-B FIX: 逻辑反转——lookup_model_or_default 命中时 id == model_name，
        // 此时使用 display_name；不匹配时（fallback 到 V4-Flash）显示原始 model_name
        let model_lookup = abacus_types::lookup_model_or_default(&state.model_name);
        let model_display = if model_lookup.id == state.model_name {
            // lookup 命中：显示官方 display_name
            model_lookup.display_name
        } else {
            // lookup 未命中（返回了 fallback），显示用户配置的原始 model_name
            state.model_name.as_str()
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", status_icon), Style::default().fg(status_color)),
            Span::styled(provider_label, Style::default().fg(status_color)),
            Span::raw("  "),
            Span::styled(model_display, Style::default().fg(theme.accent)),
            Span::raw("  "),
            Span::styled(
                crate::tui::i18n::t("panel.thinking"),
                Style::default().fg(theme.muted),
            ),
            Span::styled(
                format!(":{}", state.thinking_depth),
                Style::default().fg(theme.muted),
            ),
        ]));

        // ── ctx 进度条 ──
        // V42-B: context_window == 0 时（模型未确认），不显示进度条
        let raw_used = if state.ctx_live_tokens > 0 {
            state.ctx_live_tokens as usize
        } else {
            state.session_tokens.latest_prompt_tokens as usize
        };
        let max_ctx = state.context_window;
        if max_ctx > 0 {
            let used = raw_used.min(max_ctx);
            let pct = if used > 0 { (used as f64 * 100.0 / max_ctx as f64).min(100.0) as usize } else { 0 };
            let pc = if pct >= 80 {
                theme.error
            } else if pct >= 50 {
                theme.gold
            } else {
                theme.success
            };
            let bw = w.saturating_sub(14).min(12);
            let filled = (pct * bw / 100).min(bw);
            lines.push(Line::from(vec![
                Span::styled("    ", dim),
                Span::styled(
                    format!(
                        "{}{}",
                        "\u{2593}".repeat(filled),
                        "\u{2591}".repeat(bw - filled)
                    ),
                    Style::default().fg(pc),
                ),
                Span::styled(format!("  {}%", pct), Style::default().fg(pc).add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("  {}/{}", format_ctx(used), format_ctx(max_ctx)),
                    Style::default().fg(theme.muted),
                ),
            ]));
        }

        // ── 统计行: 输入 / 输出 / 缓存 / 费用 ──
        // V42-B: 无 token 消耗时隐藏统计行，避免显示 "输入 0 输出 0 缓存 0%" 噪声
        let inp = state.session_tokens.prompt_tokens;
        let out = state.session_tokens.completion_tokens;
        if inp > 0 || out > 0 {
            let cached = state.session_tokens.cached_tokens;
            let cpct = if inp > 0 { cached * 100 / inp } else { 0 };
            let cost = state.session_tokens.cost_cny;
            let mut tok_parts = vec![
                Span::styled("    ", dim),
                Span::styled(
                    format!(
                        "{} {}  {} {}  {} {}%",
                        crate::tui::i18n::t("panel.input"),
                        format_ctx(inp as usize),
                        crate::tui::i18n::t("panel.output"),
                        format_ctx(out as usize),
                        crate::tui::i18n::t("panel.cache"),
                        cpct
                    ),
                    Style::default().fg(theme.muted),
                ),
            ];
            if cost > 0.001 {
                // V42-B: 模型不在 pricing 表中时，费用不可靠，显示 "—"
                let cost_display = if model_lookup.id != state.model_name {
                    "\u{2014}".to_string()  // em-dash
                } else {
                    format!("\u{00a5}{:.2}", cost)
                };
                tok_parts.push(Span::styled(
                    format!("  {} {}", crate::tui::i18n::t("panel.cost"), cost_display),
                    Style::default().fg(theme.gold),
                ));
            }
            lines.push(Line::from(tok_parts));
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
    fn llm_section_metadata() {
        let s = LlmSection;
        assert_eq!(s.id(), "llm");
        assert_eq!(s.min_height(), 4);
        assert_eq!(s.preferred_height(20), 4);
    }

    #[test]
    fn llm_section_visible_always_true_by_default() {
        let s = LlmSection;
        let state = AppState::new(AbacusMode::Clarify);
        let ctx = AppContext::new(&state);
        assert!(s.visible(&ctx));
    }

    /// 验证: 喂空 state 不 panic（防御性测试）
    #[test]
    fn llm_section_renders_empty_state_without_panic() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let s = LlmSection;
        let state = AppState::new(AbacusMode::Clarify);
        let ctx = AppContext::new(&state);
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 4);
                s.render(f, &ctx, area);
            })
            .unwrap();
    }
}
