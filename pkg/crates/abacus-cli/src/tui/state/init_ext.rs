//! AppState 构造函数 + V42-B 兼容包装提取

use super::{
    completion, AbacusMode, AppState, Cell, CardStream, DashboardTab, Focus, HashSet,
    InputState, PanelSection, PanelTab, RefCell, SessionTokenStats, SimpleScrollOffset,
    TaskRegistry, Theme,
};
use std::collections::VecDeque;
use tui_textarea::TextArea as TuiTextArea;

impl AppState {
    pub fn new(mode: AbacusMode) -> Self {
        Self::new_with_sections(mode, None)
    }

    pub(crate) fn new_session_defaults(mode: AbacusMode, panel_layout: Vec<String>) -> Self {
        Self {
            theme: Theme::init(),
            mode,
            mode_artifact: None,
            last_review: None,
            last_review_strict: false,
            pending_review_parses: 0,
            pending_review_strict: false,
            auto_review_plan: false,
            review_history: std::collections::VecDeque::with_capacity(20),
            pending_review_kind: crate::tui::api::ReviewKind::Plan,
            review_required: false,
            review_max_age_secs: 600,
            runtime_temperature: None,
            runtime_max_tokens: None,
            runtime_context_ratio: None,
            runtime_tool_limit: None,
            runtime_timeout: None,
            runtime_router: None,
            runtime_dedup: None,
            active_preset: None,
            pending_meeting_execution: None,
            preserved_input: None,
            meeting_suggested_this_session: false,
            session_id: uuid::Uuid::new_v4().to_string(),
            model_name: String::new(),
            active_provider_id: String::new(),
            provider_statuses: Vec::new(),
            available_models: Vec::new(),
            available_providers: Vec::new(),
            pending_model_fetch: false,
            thinking_depth: "off".to_string(),
            context_window: 1_000_000,
            model_max_context: 1_000_000,
            config_mtime: None,
            ctx_live_tokens: 0,
            ctx_estimate_at: None,
            session_summary: String::new(),
            turn_count: 0,
            session_alias: None,
            session_goal: None,
            pending_turnkey_plan: None,
            messages: VecDeque::new(),
            scroll: 0,
            user_scrolled_away: std::cell::Cell::new(false),
            scroll_by_mode: std::collections::HashMap::new(),
            last_visible_h: std::cell::Cell::new(0),
            last_total_lines: std::cell::Cell::new(0),
            last_timeline_visible: std::cell::Cell::new(0),
            input: String::new(),
            input_state: InputState::Ready,
            pre_compress_input_state: None,
            pending_compress_input: None,
            connection_error: false,
            cursor_pos: 0,
            cursor_line: 0,
            cursor_col: 0,
            textarea: {
                let mut ta = TuiTextArea::default();
                ta.set_placeholder_text("Ask anything...");
                ta.set_placeholder_style(
                    ratatui::style::Style::default()
                        .fg(ratatui::style::Color::DarkGray)
                        .add_modifier(ratatui::style::Modifier::ITALIC),
                );
                ta.set_cursor_style(
                    ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::REVERSED),
                );
                std::cell::RefCell::new(ta)
            },
            focus: Focus::Input,
            panel_visible: true,
            panel_tab: PanelTab::Timeline,
            dashboard_tab: DashboardTab::Health,
            dashboard_scroll: 0,
            auto_health: abacus_core::auto::AutoHealth::default(),
            stream_cursor: 0,
            cmd_scroll: 0,
            cmd_selected: 0,
            commands: crate::tui::slash_commands::command_inventory(),
            events: Vec::new(),
            trace_events: Vec::new(),
            trace_event_index: std::collections::HashMap::new(),
            tool_freq_cache: std::cell::RefCell::new(None),
            tool_freq_dirty: std::cell::Cell::new(true),
            next_trace_id: 0,
            streaming_trace_ids: Vec::new(),
            timeline_expanded_ids: HashSet::new(),
            timeline_row_map: RefCell::new(Vec::new()),
            cmd_row_map: RefCell::new(Vec::new()),
            message_trace_row_map: RefCell::new(Vec::new()),
            focused_event_id: None,
            tool_records: Vec::new(),
            tool_health: std::collections::HashMap::new(),
            thinking_text: String::new(),
            experts: Vec::new(),
            expert_names_cache: HashSet::new(),
            tasks: Vec::new(),
            toasts: Vec::new(),
            cards: CardStream::new(),
            message_scroll_y: 0,
            last_msg_area: RefCell::new(ratatui::layout::Rect::new(0, 0, 0, 0)),
            last_content_width: Cell::new(80),
            running: true,
            paused: false,
            compact: false,
            resize_debounce_frames: 0,
            ctrl_c_last: None,
            op_started_at: None,
            accumulated_elapsed: std::time::Duration::ZERO,
            engine_handle: None,
            palace_data: None,
            local_health: None,
            engine_tx: None,
            task_registry: TaskRegistry::new(),
            pending_text: None,
            completion: completion::CompletionEngine::new(),
            input_history: Vec::new(),
            pending_inputs: VecDeque::new(),
            pending_send: false,
            history_index: None,
            pending_file_completion: None,
            pending_ai_completion: None,
            text_selection: None,
            pending_slash_command: None,
            plan_phase: None,
            show_settings: false,
            settings_focus: 0,
            settings_input: String::new(),
            session_tokens: SessionTokenStats::default(),
            transition_hint: None,
            processing_phase: String::new(),
            processing_step: 0,
            processing_total_steps: 0,
            rendered_lines_dirty: std::cell::Cell::new(true),
            frame_dirty: std::cell::Cell::new(true),
            streaming_content_dirty: std::cell::Cell::new(false),
            cached_base_msg_count: std::cell::Cell::new(0),
            info_panel_text: String::new(),
            info_panel_auto_open: false,
            picker: None,
            editor_state: None,
            theme_preview_open: false,
            cached_msg_rows: RefCell::new(Vec::new()),
            cached_width: RefCell::new(0),
            streaming_enabled: true,
            show_streaming_trace: true,
            streaming_complete: false,
            streaming_tools: Vec::new(),
            streaming_timeline: Vec::new(),
            expanded_block_ids: std::cell::RefCell::new(std::collections::HashSet::new()),
            streaming_md: std::cell::RefCell::new(None),
            flash_state: crate::tui::effects::FlashState::new(),
            anim_tick: std::cell::Cell::new(0),
            code_blocks_expanded: false,
            lsp_diag_errors: 0,
            lsp_diag_warnings: 0,
            section_registry: crate::tui::extensions::new_section_registry(),
            dashboard_registry: crate::tui::extensions::new_dashboard_registry(),
            panel_layout,
            confirm_dialog: None,
            pending_confirmation_response: None,
            always_allow: std::collections::HashSet::new(),
            pending_mcip_confirmations: Vec::new(),
            timeline_scroll: SimpleScrollOffset::with_follow_tail(true),
            timeline_groups_cache: Vec::new(),
            timeline_cache_len: 0,
            knowledge_scroll: SimpleScrollOffset::with_follow_tail(true),
            panel_scroll_section: PanelSection::Timeline,
            knowledge_calls: Vec::new(),
            focus_changed_at: None,
            last_user_keypress_at: None,
            last_magnet_toast_at: None,
        }
    }

    pub fn new_with_sections(mode: AbacusMode, panel_sections: Option<Vec<String>>) -> Self {
        let mut theme = Theme::init();
        theme.set_mode_color(mode.label());

        // 读取 config.toml [tui.panel] sections 覆盖默认布局
        let panel_layout: Vec<String> = if let Some(sections) = panel_sections {
            let reg = crate::tui::extensions::new_section_registry();
            sections.into_iter().filter(|id| {
                if reg.contains(id) {
                    true
                } else {
                    tracing::warn!("unknown panel section `{id}` in config [tui.panel] sections, ignoring");
                    false
                }
            }).collect()
        } else {
            crate::tui::extensions::default_panel_layout()
                .iter()
                .map(|s| s.to_string())
                .collect()
        };

        let mut state = Self::new_session_defaults(mode, panel_layout);
        state.theme = theme;
        state
    }
}
