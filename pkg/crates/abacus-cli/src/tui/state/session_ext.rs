//! AppState 会话/焦点/滚动/模式/提示方法提取

use super::{
    AppState, AbacusMode, DashboardTab, Focus, InputState, MAGNET_SUPPRESS_MS,
    MAGNET_TOAST_THROTTLE_MS, MsgContent, PanelSection, PanelTab, ScrollAction, Toast,
};
use super::Message;
use abacus_ui_kit::Theme;
use std::time::Instant;

impl AppState {
    pub fn set_focus(&mut self, new_focus: Focus) {
        if self.focus != new_focus {
            self.focus = new_focus;
            self.focus_changed_at = Some(Instant::now());
        }
    }

    pub fn cycle_dashboard_tab(&mut self) {
        self.dashboard_tab = match self.dashboard_tab {
            DashboardTab::Health => DashboardTab::Auto,
            DashboardTab::Auto => DashboardTab::Health,
        };
        self.dashboard_scroll = 0;
    }

    pub fn cycle_focus(&mut self) {
        let chat_with_commands =
            matches!(self.mode, AbacusMode::Clarify) && !self.commands.is_empty();
        let candidates: Vec<Focus> = [
            Focus::Input,
            Focus::Panel,
            Focus::CommandHint,
        ]
        .into_iter()
        .filter(|f| match f {
            Focus::Input => true,
            Focus::Panel => self.panel_visible,
            Focus::CommandHint => chat_with_commands,
        })
        .collect();

        if candidates.is_empty() {
            return;
        }
        let cur_pos = candidates.iter().position(|f| *f == self.focus);
        let next = match cur_pos {
            Some(i) => candidates[(i + 1) % candidates.len()],
            None => candidates[0],
        };
        self.set_focus(next);
    }

    pub fn note_focus_change(&mut self) {
        self.focus_changed_at = Some(Instant::now());
    }

    pub fn focus_pulsing(&self) -> bool {
        self.focus_changed_at
            .map(|t| t.elapsed().as_millis() < 200)
            .unwrap_or(false)
    }

    pub fn record_keypress(&mut self) {
        self.last_user_keypress_at = Some(Instant::now());
    }

    pub fn try_magnet_focus(&mut self, target: Focus, section: PanelSection) {
        if !matches!(self.mode, AbacusMode::Clarify) {
            return;
        }
        if let Some(t) = self.last_user_keypress_at {
            if t.elapsed().as_millis() < MAGNET_SUPPRESS_MS {
                return;
            }
        }
        let did_switch = self.focus != target;
        if did_switch {
            self.set_focus(target);
        }
        if self.panel_visible {
            self.panel_scroll_section = section;
        }
        if did_switch {
            let allow_toast = self.last_magnet_toast_at
                .map(|t| t.elapsed().as_millis() >= MAGNET_TOAST_THROTTLE_MS)
                .unwrap_or(true);
            if allow_toast {
                self.add_toast(
                    "→ 焦点已自动切到时间线（Esc 回输入栏）",
                    std::time::Duration::from_millis(1500),
                );
                self.last_magnet_toast_at = Some(Instant::now());
            }
        }
    }

    pub fn set_mode(&mut self, mode: AbacusMode) {
        if self.mode != mode {
            self.scroll_by_mode.insert(self.mode, self.scroll);
            let restored = self.scroll_by_mode.get(&mode).copied().unwrap_or(0);
            self.set_scroll(ScrollAction::Restore(restored));
        }
        self.mode = mode;
        self.theme.set_mode_color(mode.label());

        let allowed = PanelTab::all(mode);
        match self.panel_tab {
            PanelTab::Custom(_) => {}
            other if !allowed.contains(&other) => {
                self.panel_tab = PanelTab::Timeline;
            }
            _ => {}
        }
    }

    pub fn set_scroll(&mut self, action: ScrollAction) {
        match &action {
            ScrollAction::Up(_) | ScrollAction::Down(_) | ScrollAction::Absolute(_) => {
                self.user_scrolled_away.set(true);
            }
            ScrollAction::ToBottom => {
                self.user_scrolled_away.set(false);
            }
            ScrollAction::AnchorAdjust { .. } | ScrollAction::Restore(_) => {}
        }
        let total = self.last_total_lines.get();
        let vis = self.last_visible_h.get();
        let max = if total == 0 { 10_000 } else { total.saturating_sub(vis) };
        let new = match action {
            ScrollAction::ToBottom => 0,
            ScrollAction::Up(n) => (self.scroll + n).min(max),
            ScrollAction::Down(n) => self.scroll.saturating_sub(n),
            ScrollAction::Absolute(n) => n.min(max),
            ScrollAction::AnchorAdjust { after_rows, before_rows } => {
                if after_rows >= before_rows {
                    self.scroll.saturating_add(after_rows - before_rows).min(max)
                } else {
                    self.scroll.saturating_sub(before_rows - after_rows)
                }
            }
            ScrollAction::Restore(n) => n.min(max),
        };
        self.scroll = new;
        self.mark_render_dirty();
    }

    pub fn toggle_panel(&mut self) {
        self.panel_visible = !self.panel_visible;
    }

    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        self.input_state = if self.paused {
            InputState::Paused
        } else {
            InputState::Ready
        };
        if self.paused {
            if let Some(started) = self.op_started_at.take() {
                self.accumulated_elapsed += started.elapsed();
            }
        } else {
            self.op_started_at = Some(std::time::Instant::now() - self.accumulated_elapsed);
        }
    }

    pub fn add_toast(&mut self, message: impl Into<String>, duration: std::time::Duration) {
        let msg = message.into();
        if let Some(existing) = self.toasts.iter_mut().find(|t| t.message == msg) {
            existing.expire_at = Instant::now() + duration;
            return;
        }
        self.toasts.push(Toast {
            message: msg,
            expire_at: Instant::now() + duration,
        });
    }

    pub fn show_info(&mut self, text: impl Into<String>) {
        let s = text.into();
        if self.is_streaming_active() {
            self.add_toast("命令已收到，请等流式结束后查看", std::time::Duration::from_secs(2));
            self.info_panel_text = s;
            self.info_panel_auto_open = true;
            return;
        }
        let ts = chrono::Local::now().format("%H:%M").to_string();
        self.add_message(Message::new_session(
            vec![MsgContent::Stream(s)],
            &ts,
        ));
        self.mark_render_dirty();
    }

    pub fn cycle_thinking_depth(&mut self) -> &str {
        let next = match self.thinking_depth.as_str() {
            "off" => "low",
            "low" => "medium",
            "medium" => "high",
            "high" => "max",
            "max" => "off",
            _ => "high",
        };
        self.thinking_depth = next.to_string();
        next
    }

    pub const KNOWN_MODELS: &'static [&'static str] = &[
        "deepseek-v4-flash",
        "deepseek-v4-pro",
        "qwen-plus",
        "qwen-turbo",
    ];

    pub const THINKING_SLIDER_DEPTHS: &'static [&'static str] = &["off", "low", "medium", "high", "max"];

    pub const SETTINGS_ITEM_COUNT: usize = 5;

    pub fn cycle_model(&mut self) -> String {
        let names = Self::KNOWN_MODELS;
        let idx = names.iter().position(|n| *n == self.model_name.as_str()).unwrap_or(0);
        let next = names[(idx + 1) % names.len()];
        self.model_name = next.to_string();
        self.theme.apply_model_brand(next);
        if let Some(ref engine) = self.engine_handle {
            let core = engine.core.clone();
            let model = next.to_string();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    core.set_model_override(&model).await;
                })
            });
        }
        next.to_string()
    }

    pub fn cleanup_toasts(&mut self) {
        let now = Instant::now();
        self.toasts.retain(|t| t.expire_at > now);
    }

    pub fn reset_session(&mut self) {
        self.task_registry.cancel_all();

        let theme = std::mem::replace(&mut self.theme, Theme::init());
        let mode = self.mode;
        let engine_handle = self.engine_handle.take();
        let engine_tx = self.engine_tx.take();
        let section_registry = std::mem::replace(&mut self.section_registry, crate::tui::extensions::new_section_registry());
        let dashboard_registry = std::mem::replace(&mut self.dashboard_registry, crate::tui::extensions::new_dashboard_registry());
        let panel_layout = std::mem::take(&mut self.panel_layout);
        let always_allow = std::mem::take(&mut self.always_allow);
        let running = self.running;
        let streaming_enabled = self.streaming_enabled;
        let available_models = std::mem::take(&mut self.available_models);
        let available_providers = std::mem::take(&mut self.available_providers);
        let provider_statuses = std::mem::take(&mut self.provider_statuses);
        let local_health = self.local_health.take();
        let palace_data = self.palace_data.take();
        let model_name = self.model_name.clone();
        let active_provider_id = self.active_provider_id.clone();
        let context_window = self.context_window;
        let model_max_context = self.model_max_context;

        let defaults = Self::new_session_defaults(mode, panel_layout);
        *self = defaults;

        self.theme = theme;
        self.engine_handle = engine_handle;
        self.engine_tx = engine_tx;
        self.section_registry = section_registry;
        self.dashboard_registry = dashboard_registry;
        self.always_allow = always_allow;
        self.running = running;
        self.streaming_enabled = streaming_enabled;
        self.available_models = available_models;
        self.available_providers = available_providers;
        self.provider_statuses = provider_statuses;
        self.local_health = local_health;
        self.palace_data = palace_data;
        self.model_name = model_name;
        self.active_provider_id = active_provider_id;
        self.context_window = context_window;
        self.model_max_context = model_max_context;

        self.mark_dirty();
    }
}
