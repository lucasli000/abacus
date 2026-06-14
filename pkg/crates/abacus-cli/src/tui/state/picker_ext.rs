//! AppState Picker 相关方法提取
//!
//! 打开各类 picker 的逻辑（Model/Theme/Thinking/Mode/Review/Meeting/Resume/History/Generic）。

use super::{AppState, EditorState, InputState, PickerKind, PickerState};

impl AppState {
    /// 设计:
    ///   - groups 按 provider 名分组(DeepSeek/Qwen/...)
    ///   - show_thinking_slider=true 渲染底部 thinking 行, ←→ 调整深度
    ///   - selected 跨分组用 items 索引(分组只是渲染形态, 不改 selected 语义)
    pub fn open_picker_model(&mut self) {
        const STATIC_GROUPS: &[(&str, &[(&str, &str)])] = &[
            ("DeepSeek", &[
                ("deepseek-chat",     "通用对话"),
                ("deepseek-reasoner", "推理增强"),
                ("deepseek-v4-flash", "最快响应 (low latency)"),
                ("deepseek-v4-pro",   "最强推理 (deep reasoning)"),
            ]),
        ];

        let mut items: Vec<String> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        let mut groups: Vec<(String, std::ops::Range<usize>)> = Vec::new();

        if !self.available_models.is_empty() {
            const KNOWN_DESCS: &[(&str, &str)] = &[
                ("deepseek-chat",     "通用对话"),
                ("deepseek-reasoner", "推理增强"),
                ("deepseek-v4-flash", "最快响应"),
                ("deepseek-v4-pro",   "最强推理"),
            ];
            if !self.available_providers.is_empty() {
                for (provider_id, models) in &self.available_providers {
                    let start = items.len();
                    for model_name in models {
                        let desc = KNOWN_DESCS.iter()
                            .find(|(k, _)| *k == model_name.as_str())
                            .map(|(_, d)| *d)
                            .unwrap_or("");
                        items.push(model_name.clone());
                        labels.push(if desc.is_empty() {
                            model_name.clone()
                        } else {
                            format!("{:<22}  {}", model_name, desc)
                        });
                    }
                    let end = items.len();
                    if end > start {
                        groups.push((provider_id.clone(), start..end));
                    }
                }
            } else {
                fn infer_provider(id: &str) -> &'static str {
                    let lower = id.to_lowercase();
                    if lower.starts_with("deepseek") { "DeepSeek" }
                    else if lower.starts_with("qwen") { "Qwen" }
                    else if lower.starts_with("gpt") || lower.starts_with("o1") || lower.starts_with("o3") || lower.starts_with("chatgpt") { "OpenAI" }
                    else if lower.starts_with("claude") { "Anthropic" }
                    else if lower.starts_with("gemini") { "Gemini" }
                    else if lower.starts_with("glm") || lower.starts_with("zhipu") { "智谱" }
                    else if lower.starts_with("moonshot") || lower.starts_with("kimi") { "Moonshot" }
                    else if lower.starts_with("doubao") || lower.starts_with("ark") { "火山引擎" }
                    else { "其他" }
                }
                let mut provider_order: Vec<&'static str> = Vec::new();
                let mut provider_items: std::collections::HashMap<&'static str, Vec<(&str, &str)>> = std::collections::HashMap::new();
                for id in &self.available_models {
                    let prov = infer_provider(id);
                    let desc = KNOWN_DESCS.iter()
                        .find(|(k, _)| *k == id.as_str())
                        .map(|(_, d)| *d)
                        .unwrap_or("");
                    if !provider_items.contains_key(prov) {
                        provider_order.push(prov);
                    }
                    provider_items.entry(prov).or_default().push((id.as_str(), desc));
                }
                for prov in &provider_order {
                    let start = items.len();
                    if let Some(models) = provider_items.get(prov) {
                        for (id, desc) in models {
                            items.push((*id).to_string());
                            labels.push(if desc.is_empty() {
                                (*id).to_string()
                            } else {
                                format!("{:<22}  {}", id, desc)
                            });
                        }
                    }
                    let end = items.len();
                    if end > start {
                        groups.push((prov.to_string(), start..end));
                    }
                }
            }
        } else {
            for (provider, models) in STATIC_GROUPS {
                let start = items.len();
                for (id, desc) in *models {
                    items.push((*id).to_string());
                    labels.push(format!("{:<22}  {}", id, desc));
                }
                let end = items.len();
                if end > start {
                    groups.push((provider.to_string(), start..end));
                }
            }
        }

        if !self.model_name.is_empty() && !items.contains(&self.model_name) {
            items.insert(0, self.model_name.clone());
            labels.insert(0, format!("{:<22}  (当前配置)", &self.model_name));
            for (_, range) in &mut groups {
                *range = (range.start + 1)..(range.end + 1);
            }
            groups.insert(0, ("自定义".to_string(), 0..1));
        }

        let current = items.iter().position(|m| m == &self.model_name);
        self.picker = Some(PickerState {
            kind: PickerKind::Model,
            selected: current.unwrap_or(0),
            current,
            items,
            labels,
            groups: Some(groups),
            show_thinking_slider: true,
            opened_at: std::time::Instant::now(),
            review_strict: false,
        });
        self.mark_render_dirty();
    }

    /// 打开主题 picker — 列出 Theme::all_names，selected 设为当前主题位置
    pub fn open_picker_theme(&mut self) {
        let names = abacus_ui_kit::Theme::all_names();
        let items: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        let labels = items.clone();
        let current = items.iter().position(|n| n == self.theme.name);
        self.picker = Some(PickerState {
            kind: PickerKind::Theme,
            selected: current.unwrap_or(0),
            current,
            items,
            labels,
            groups: None,
            show_thinking_slider: false,
            opened_at: std::time::Instant::now(),
            review_strict: false,
        });
        self.mark_render_dirty();
    }

    /// 打开思考深度 picker — off/low/medium/high/max
    pub fn open_picker_thinking(&mut self) {
        let items: Vec<String> = Self::THINKING_SLIDER_DEPTHS.iter().map(|s| s.to_string()).collect();
        let labels = vec![
            "off    — 关闭思考链".to_string(),
            "low    — 简短推理".to_string(),
            "medium — 中等推理".to_string(),
            "high   — 深度推理（默认）".to_string(),
            "max    — 最大预算（贵）".to_string(),
        ];
        let current = items.iter().position(|d| d == &self.thinking_depth);
        self.picker = Some(PickerState {
            kind: PickerKind::Thinking,
            selected: current.unwrap_or(3),
            current,
            items,
            labels,
            groups: None,
            show_thinking_slider: false,
            opened_at: std::time::Instant::now(),
            review_strict: false,
        });
        self.mark_render_dirty();
    }

    /// 打开模式切换 picker（Clarify / Meeting）
    pub fn open_picker_mode(&mut self) {
        let items = vec!["clarify".to_string(), "meeting".to_string()];
        let labels = vec![
            "Clarify   需求澄清与方案对齐".to_string(),
            "Meeting   多专家会诊审议".to_string(),
        ];
        let current_mode = match self.mode {
            crate::tui::state::AbacusMode::Clarify  => Some(0usize),
            crate::tui::state::AbacusMode::Meeting  => Some(1usize),
        };
        self.picker = Some(PickerState {
            kind: PickerKind::Mode,
            selected: current_mode.unwrap_or(0),
            current: current_mode,
            items,
            labels,
            groups: None,
            show_thinking_slider: false,
            opened_at: std::time::Instant::now(),
            review_strict: false,
        });
        self.mark_render_dirty();
    }

    /// 打开审查类型 picker（plan / diff / security）
    pub fn open_picker_review(&mut self) {
        let items = vec![
            "plan".to_string(),
            "diff".to_string(),
            "security".to_string(),
        ];
        let labels = vec![
            "plan       审查规划方案与任务分解".to_string(),
            "diff       审查代码变更（git diff 风格）".to_string(),
            "security   安全审计（OWASP + 权限检查）".to_string(),
        ];
        self.picker = Some(PickerState {
            kind: PickerKind::Review,
            selected: 0,
            current: None,
            items,
            labels,
            groups: None,
            show_thinking_slider: false,
            opened_at: std::time::Instant::now(),
            review_strict: false,
        });
        self.mark_render_dirty();
    }

    /// V35: 打开 Meeting 操作 picker
    pub fn open_picker_meeting(&mut self) {
        let expert_count = crate::tui::expert_config::load_experts().len();
        let items = vec![
            "meeting".to_string(),
            "expert".to_string(),
            "meeting-list".to_string(),
        ];
        let labels = vec![
            format!("进入会诊       召集专家开始多角色会议"),
            format!("专家配置 ({}位)  /expert list | add | set | remove", expert_count),
            format!("历史记录       浏览历史会议结论"),
        ];
        self.picker = Some(PickerState {
            kind: PickerKind::Meeting,
            selected: 0,
            current: if self.mode == crate::tui::state::AbacusMode::Meeting { Some(0) } else { None },
            items,
            labels,
            groups: None,
            show_thinking_slider: false,
            opened_at: std::time::Instant::now(),
            review_strict: false,
        });
        self.mark_render_dirty();
    }

    /// 打开历史 session 恢复 picker
    pub fn open_picker_resume(&mut self) {
        let dir = abacus_core::paths::current_sessions_dir();
        let mut entries: Vec<(String, std::time::SystemTime)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                let is_session = p.extension().and_then(|x| x.to_str()) == Some("json")
                    && !p.file_name().and_then(|x| x.to_str())
                        .map(|n| n.starts_with('.')).unwrap_or(false);
                if !is_session { continue; }
                let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or("").to_string();
                if let Ok(mt) = e.metadata().and_then(|m| m.modified()) {
                    entries.push((stem, mt));
                }
            }
        }
        entries.sort_by_key(|e| std::cmp::Reverse(e.1));
        entries.truncate(10);

        if entries.is_empty() {
            self.add_toast("暂无历史 session", std::time::Duration::from_secs(3));
            return;
        }

        let current_id = self.session_id.clone();
        let mut items = Vec::new();
        let mut labels = Vec::new();
        let mut current_idx = None;

        for (i, (uuid, mt)) in entries.iter().enumerate() {
            let dt: chrono::DateTime<chrono::Local> = (*mt).into();
            let now = chrono::Local::now();
            let time_str = if dt.date_naive() == now.date_naive() {
                dt.format("今天 %H:%M").to_string()
            } else if (now.date_naive() - dt.date_naive()).num_days() == 1 {
                dt.format("昨天 %H:%M").to_string()
            } else {
                dt.format("%m/%d %H:%M").to_string()
            };
            let short_id: String = uuid.chars().take(8).collect();
            let alias = if uuid == &current_id {
                format!("▶ {} （当前）", &short_id)
            } else {
                format!("  {}", &short_id)
            };
            let label = format!("{}  {}  {}", alias, time_str, uuid);
            items.push(uuid.clone());
            labels.push(label);
            if uuid == &current_id {
                current_idx = Some(i);
            }
        }

        self.picker = Some(PickerState {
            kind: PickerKind::Resume,
            selected: current_idx.unwrap_or(0),
            current: current_idx,
            items,
            labels,
            groups: None,
            show_thinking_slider: false,
            opened_at: std::time::Instant::now(),
            review_strict: false,
        });
        self.mark_render_dirty();
    }

    /// 打开输入历史 picker（最近 20 条，选中重发）
    pub fn open_picker_history(&mut self) {
        if self.input_history.is_empty() {
            self.add_toast("暂无输入历史", std::time::Duration::from_secs(2));
            return;
        }
        let recent: Vec<String> = self.input_history.iter().rev().take(20).cloned().collect();
        let labels: Vec<String> = recent.iter().map(|h| {
            let truncated: String = h.chars().take(60).collect();
            if h.chars().count() > 60 {
                format!("{}…", truncated)
            } else {
                truncated
            }
        }).collect();

        self.picker = Some(PickerState {
            kind: PickerKind::History,
            selected: 0,
            current: None,
            items: recent,
            labels,
            groups: None,
            show_thinking_slider: false,
            opened_at: std::time::Instant::now(),
            review_strict: false,
        });
        self.mark_render_dirty();
    }

    /// 通用 picker 打开方法 — 供不需要特殊初始化逻辑的 picker 使用
    pub fn open_picker_generic(&mut self, kind: PickerKind, items: Vec<String>, labels: Vec<String>) {
        self.picker = Some(PickerState {
            kind,
            selected: 0,
            current: None,
            items,
            labels,
            groups: None,
            show_thinking_slider: false,
            opened_at: std::time::Instant::now(),
            review_strict: false,
        });
        self.mark_render_dirty();
    }

    /// 打开全屏编辑器
    pub fn open_editor(&mut self) {
        if self.input_state == InputState::Editor { return; }
        self.editor_state = Some(EditorState {
            scroll_top: 0,
            opened_at: std::time::Instant::now(),
            last_visible_h: std::cell::Cell::new(20),
            selection_anchor: None,
        });
        self.input_state = InputState::Editor;
        self.mark_render_dirty();
    }

    /// 关闭全屏编辑器
    pub fn close_editor(&mut self) {
        self.editor_state = None;
        self.input_state = InputState::Ready;
        self.mark_render_dirty();
    }

    /// 安全设置 busy 态（Editor 态不覆盖）
    pub fn set_busy_state(&mut self, target: InputState) {
        if self.input_state == InputState::Editor { return; }
        self.input_state = target;
    }
}
