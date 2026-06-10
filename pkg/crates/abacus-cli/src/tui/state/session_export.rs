//! Session 持久化：导入 / 导出 (原本在 tui/run.rs, C-12 拆出)
//!
//! ## 职责
//! - `save_session` — 把当前 `AppState` 写入 `~/.abacus/projects/<cwd>/sessions/<uuid>.json`
//! - `load_last_session` / `load_session_by_uuid` — 从磁盘恢复 session
//! - `session_path` — 推导 last-session pointer 对应的路径
//!
//! ## 与 tui/run.rs 的协作
//! - 本模块的 `apply_session_export` 不再直接调用 `save_always_allow`；
//!   由 `load_session_from_path` 在外部调用 (避免循环依赖)

use std::collections::VecDeque;
use std::path::Path;

use crate::tui::state::AppState;

// ─── save_session ───────────────────────────────────────────────────

/// - 文件命名用 state.session_id (UUID)，多实例不互覆盖
/// - 额外写 last_session_uuid 文本 pointer（项目内）以支持 "恢复上次"语义
///
/// V28 (T9): SessionExport 升级到 v2 — 把 events: Vec<EventEntry> 替换为
/// trace_events: Vec<TraceEvent> + next_trace_id: u64(SSOT 直接持久化)。
/// 旧 v1 文件由 load_last_session 自动 migration 到 v2 形态(events → Generic kind)。
pub fn save_session(state: &AppState) -> std::io::Result<()> {
    use serde::Serialize;
    #[derive(Serialize)]
    struct SessionExport {
        version: u32,
        session_id: String,
        model_name: String,
        thinking_depth: String,
        turn_count: u32,
        session_summary: String,
        messages: Vec<crate::tui::state::Message>,
        trace_events: Vec<crate::tui::state::TraceEvent>,
        next_trace_id: u64,
        #[serde(skip_serializing)]
        _always_allow_legacy: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_alias: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_goal: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<abacus_types::AbacusMode>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode_artifact: Option<abacus_types::ModeArtifact>,
        #[serde(skip_serializing_if = "session_tokens_is_empty")]
        session_tokens: Option<crate::tui::state::SessionTokenStats>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_review: Option<crate::tui::api::ReviewReport>,
        #[serde(default, skip_serializing_if = "bool_is_false")]
        last_review_strict: bool,
        #[serde(default, skip_serializing_if = "bool_is_false")]
        auto_review_plan: bool,
        #[serde(default, skip_serializing_if = "review_history_is_empty")]
        review_history: VecDeque<crate::tui::api::ReviewReport>,
        #[serde(default, skip_serializing_if = "bool_is_false")]
        review_required: bool,
        #[serde(default = "default_review_max_age_secs")]
        review_max_age_secs: u64,
        saved_at: String,
    }

    fn bool_is_false(v: &bool) -> bool { !*v }

    fn session_tokens_is_empty(opt: &Option<crate::tui::state::SessionTokenStats>) -> bool {
        match opt {
            None => true,
            Some(s) => s.total_tokens == 0 && s.cost_cny == 0.0 && s.per_model.is_empty(),
        }
    }

    fn review_history_is_empty(v: &VecDeque<crate::tui::api::ReviewReport>) -> bool {
        v.is_empty()
    }

    fn default_review_max_age_secs() -> u64 { 600 }

    let export = SessionExport {
        version: 2,
        session_id: state.session_id.clone(),
        model_name: state.model_name.clone(),
        thinking_depth: state.thinking_depth.clone(),
        turn_count: state.turn_count,
        session_summary: state.session_summary.clone(),
        messages: state.messages.iter().cloned().collect(),
        trace_events: state.trace_events.clone(),
        next_trace_id: state.next_trace_id,
        _always_allow_legacy: Vec::new(),
        session_alias: state.session_alias.clone(),
        session_goal: state.session_goal.clone(),
        mode: (state.mode != abacus_types::AbacusMode::Clarify).then_some(state.mode),
        mode_artifact: state.mode_artifact.clone(),
        session_tokens: if state.session_tokens.total_tokens == 0
            && state.session_tokens.cost_cny == 0.0
            && state.session_tokens.per_model.is_empty()
        {
            None
        } else {
            Some(state.session_tokens.clone())
        },
        last_review: state.last_review.clone(),
        last_review_strict: state.last_review_strict,
        auto_review_plan: state.auto_review_plan,
        review_history: state.review_history.clone(),
        review_required: state.review_required,
        review_max_age_secs: state.review_max_age_secs,
        saved_at: chrono::Utc::now().to_rfc3339(),
    };
    let dir = abacus_core::paths::current_sessions_dir();
    std::fs::create_dir_all(&dir)?;

    let filename = format!("{}.json", state.session_id);
    let path = dir.join(&filename);
    let json = serde_json::to_string_pretty(&export)?;

    let tmp_path = dir.join(format!(".{}.json.tmp", state.session_id));
    std::fs::write(&tmp_path, &json)?;
    std::fs::rename(&tmp_path, &path)?;

    let pointer = dir.join("last_session_uuid");
    let _ = std::fs::write(&pointer, &state.session_id);

    const SESSION_KEEP: usize = 50;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut snapshots: Vec<(std::path::PathBuf, std::time::SystemTime)> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                let is_session = p.extension().and_then(|x| x.to_str()) == Some("json")
                    && !p.file_name().and_then(|x| x.to_str()).map(|n| n.starts_with('.')).unwrap_or(false);
                if !is_session { return None; }
                e.metadata().ok().and_then(|m| m.modified().ok()).map(|mt| (p, mt))
            })
            .collect();
        snapshots.sort_by_key(|s| s.1);
        if snapshots.len() > SESSION_KEEP {
            for (old, _) in &snapshots[..snapshots.len() - SESSION_KEEP] {
                let _ = std::fs::remove_file(old);
            }
        }
    }
    Ok(())
}

// ─── load_last_session ─────────────────────────────────────────────

/// 从 ~/.abacus/sessions/latest.json 恢复上次会话
///
/// V28 (T9): v1 → v2 migration:
///   - v2: 直接反序列化 trace_events + next_trace_id(SSOT 真相源)
///   - v1 / 缺 version: 老 events: Vec<EventEntry> 转成 TraceEvent::Generic, 顺序分配 id 0..N
///   - 旧 messages 中遗留的 Block(Think/ToolCall) 原样保留(渲染层兼容,T5 不删 Block 路径)
pub fn load_last_session(state: &mut AppState) -> std::io::Result<bool> {
    let path = session_path();
    if !path.exists() { return Ok(false); }
    load_session_from_path(state, &path)
}

// ─── load_session_by_uuid ──────────────────────────────────────────

/// V29.9 (C2): 按 uuid 加载特定 session — /resume 命令用
pub fn load_session_by_uuid(state: &mut AppState, uuid: &str) -> std::io::Result<bool> {
    let dir = abacus_core::paths::current_sessions_dir();
    let path = dir.join(format!("{}.json", uuid));
    if !path.exists() { return Ok(false); }
    load_session_from_path(state, &path)
}

// ─── load_session_from_path ────────────────────────────────────────

fn load_session_from_path(state: &mut AppState, path: &Path) -> std::io::Result<bool> {
    let json = std::fs::read_to_string(path)?;
    let export: serde_json::Value = serde_json::from_str(&json)?;

    let export = match crate::tui::state::session_migrate::SessionVersion::detect(&export) {
        crate::tui::state::session_migrate::SessionVersion::V3 => {
            crate::tui::state::session_migrate::migrate_v3_to_v4(export, path)?
        }
        _ => export,
    };

    apply_session_export(state, &export);
    rebuild_cards_from_messages(state);
    Ok(true)
}

/// 从 state.messages 重建 state.cards (session load 后调用)
///
/// 遍历 messages 中的每条 Message, 按角色映射到对应 Card 类型:
/// - MsgRole::User     → push_user_message (UserCard)
/// - MsgRole::Session  → 创建 LlmCard 并 push_static
/// - MsgRole::Expert   → 创建 ExpertCard 并 push_static
fn rebuild_cards_from_messages(state: &mut AppState) {
    use crate::tui::state::MsgRole;
    use abacus_ui_kit::CardStreaming;

    let messages: Vec<crate::tui::state::Message> = state.messages.iter().cloned().collect();
    for msg in &messages {
        match msg.role {
            MsgRole::User => {
                let text = msg.parts.iter()
                    .find_map(|p| match p {
                        crate::tui::state::MsgContent::Stream(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .unwrap_or("")
                    .to_string();
                crate::tui::cards::writer::push_user_message(state, &text, &msg.time);
            }
            MsgRole::Session => {
                let id = state.cards.alloc_id();
                let text = msg.parts.iter()
                    .filter_map(|p| match p {
                        crate::tui::state::MsgContent::Stream(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let mut card = crate::tui::cards::LlmCard::new(id, "session");
                card.append_reply(&text);
                card.set_streaming(CardStreaming::Static);
                state.cards.push_static(Box::new(card));
            }
            MsgRole::Expert(ref name) => {
                let id = state.cards.alloc_id();
                let text = msg.parts.iter()
                    .filter_map(|p| match p {
                        crate::tui::state::MsgContent::Stream(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let mut card = crate::tui::cards::expert::ExpertCard::new(id, name, "default");
                card.append_reply(&text);
                card.set_streaming(CardStreaming::Static);
                state.cards.push_static(Box::new(card));
            }
        }
    }
}

// ─── apply_session_export ──────────────────────────────────────────

/// 把 SessionExport JSON 应用到 state(纯函数,便于单元测试)
///
/// V28: 显式区分 v1 vs v2 路径,v1 把 events 数组转成 TraceEvent::Generic 列表
fn apply_session_export(state: &mut AppState, export: &serde_json::Value) {
    use crate::tui::state::{TraceEvent, TraceKind};

    if let Some(sid) = export.get("session_id").and_then(|v| v.as_str()) {
        if !sid.is_empty() {
            state.session_id = sid.to_string();
        }
    }
    if let Some(name) = export.get("model_name").and_then(|v| v.as_str()) {
        state.model_name = name.to_string();
    }
    if let Some(tc) = export.get("turn_count").and_then(|v| v.as_u64()) {
        state.turn_count = tc as u32;
    }
    if let Some(s) = export.get("session_summary").and_then(|v| v.as_str()) {
        state.session_summary = s.to_string();
    }
    if let Some(td) = export.get("thinking_depth").and_then(|v| v.as_str()) {
        if !td.is_empty() {
            state.thinking_depth = td.to_string();
        }
    }
    if let Some(msgs) = export.get("messages") {
        if let Ok(msgs) = serde_json::from_value::<Vec<crate::tui::state::Message>>(msgs.clone()) {
            state.messages = msgs.into();
        }
    }

    let version = export.get("version").and_then(|v| v.as_u64()).unwrap_or(1);
    if version >= 2 {
        if let Some(te) = export.get("trace_events") {
            if let Ok(te) = serde_json::from_value::<Vec<TraceEvent>>(te.clone()) {
                state.trace_events = te;
            }
        }
        if let Some(nti) = export.get("next_trace_id").and_then(|v| v.as_u64()) {
            state.next_trace_id = nti;
        } else {
            state.next_trace_id = state.trace_events.last().map(|e| e.id + 1).unwrap_or(0);
        }
    } else {
        if let Some(evts) = export.get("events") {
            if let Ok(evts) = serde_json::from_value::<Vec<crate::tui::state::EventEntry>>(evts.clone()) {
                state.trace_events = evts.into_iter().enumerate().map(|(i, e)| TraceEvent {
                    id: i as u64,
                    time: e.time,
                    category: e.category,
                    level: e.level,
                    kind: TraceKind::Generic { content: e.content },
                    duration_ms: None,
                }).collect();
                state.next_trace_id = state.trace_events.len() as u64;
            }
        }
    }

    if let Some(s) = export.get("session_alias").and_then(|v| v.as_str()) {
        state.session_alias = (!s.is_empty()).then(|| s.to_string());
    }
    if let Some(s) = export.get("session_goal").and_then(|v| v.as_str()) {
        state.session_goal = (!s.is_empty()).then(|| s.to_string());
    }

    if let Some(m) = export.get("mode") {
        if let Ok(mode) = serde_json::from_value::<abacus_types::AbacusMode>(m.clone()) {
            state.set_mode(mode);
        }
    }
    if let Some(art) = export.get("mode_artifact") {
        if let Ok(artifact) = serde_json::from_value::<abacus_types::ModeArtifact>(art.clone()) {
            state.mode_artifact = Some(artifact);
        }
    }

    if let Some(st) = export.get("session_tokens") {
        if let Ok(tokens) = serde_json::from_value::<crate::tui::state::SessionTokenStats>(st.clone()) {
            state.session_tokens = tokens;
        }
    }

    if let Some(r) = export.get("last_review") {
        if let Ok(report) = serde_json::from_value::<crate::tui::api::ReviewReport>(r.clone()) {
            state.last_review = Some(report);
        }
    }
    if let Some(v) = export.get("last_review_strict").and_then(|x| x.as_bool()) {
        state.last_review_strict = v;
    }

    if let Some(v) = export.get("auto_review_plan").and_then(|x| x.as_bool()) {
        state.auto_review_plan = v;
    }

    if let Some(rh) = export.get("review_history") {
        if let Ok(history) = serde_json::from_value::<VecDeque<crate::tui::api::ReviewReport>>(rh.clone()) {
            state.review_history = history;
        }
    }

    if let Some(v) = export.get("review_required").and_then(|x| x.as_bool()) {
        state.review_required = v;
    }

    if let Some(v) = export.get("review_max_age_secs").and_then(|x| x.as_u64()) {
        state.review_max_age_secs = v;
    }
}

// ─── session_path ──────────────────────────────────────────────────

/// 返回 PathBuf 而非 Option 以保留与原签名的向后兼容。
fn session_path() -> std::path::PathBuf {
    let dir = abacus_core::paths::current_sessions_dir();
    let pointer = dir.join("last_session_uuid");
    if let Ok(uuid) = std::fs::read_to_string(&pointer) {
        let uuid = uuid.trim();
        if !uuid.is_empty() {
            return dir.join(format!("{uuid}.json"));
        }
    }
    // Fallback：返回预期不存在的路径（调用方以 .exists() 检查）
    dir.join(".no-last-session")
}

// ─── tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod session_migration_tests {
    use super::*;
    use crate::tui::state::{AppState, AbacusMode, TraceKind};

    #[test]
    fn v1_events_migrate_to_generic_trace_events() {
        let v1_json = serde_json::json!({
            "version": 1,
            "model_name": "gpt-4",
            "turn_count": 3,
            "session_summary": "test",
            "messages": [],
            "events": [
                { "time": "12:00", "category": "llm", "content": "开始", "level": "Info" },
                { "time": "12:01", "category": "tool", "content": "fs.read 完成", "level": "Notice" },
                { "time": "12:02", "category": "session", "content": "用户提交", "level": "Info" },
            ],
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &v1_json);

        assert_eq!(state.model_name, "gpt-4");
        assert_eq!(state.turn_count, 3);
        assert_eq!(state.trace_events.len(), 3);
        assert_eq!(state.next_trace_id, 3);
        for (i, ev) in state.trace_events.iter().enumerate() {
            assert_eq!(ev.id, i as u64);
            assert!(matches!(ev.kind, TraceKind::Generic { .. }), "v1 migration must be Generic");
        }
        assert_eq!(state.trace_events[0].category, "llm");
        assert_eq!(state.trace_events[1].category, "tool");
        assert_eq!(state.trace_events[2].category, "session");
    }

    #[test]
    fn v1_missing_version_treated_as_v1() {
        let json = serde_json::json!({
            "model_name": "x",
            "messages": [],
            "events": [
                { "time": "12:00", "category": "llm", "content": "hi", "level": "Info" },
            ],
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &json);
        assert_eq!(state.trace_events.len(), 1);
        assert!(matches!(state.trace_events[0].kind, TraceKind::Generic { .. }));
    }

    #[test]
    fn v2_round_trip_preserves_trace_events() {
        let v2_json = serde_json::json!({
            "version": 2,
            "model_name": "claude",
            "turn_count": 1,
            "session_summary": "v2",
            "messages": [],
            "trace_events": [
                {
                    "id": 5, "time": "10:00", "category": "llm", "level": "Info",
                    "duration_ms": null,
                    "kind": { "type": "Thinking", "text": "推理过程", "lines": 3 }
                },
                {
                    "id": 6, "time": "10:01", "category": "tool", "level": "Notice",
                    "duration_ms": 150,
                    "kind": {
                        "type": "ToolCall", "name": "filengine.fs.read", "args": "{}",
                        "output": "ok", "status": "Success"
                    }
                },
            ],
            "next_trace_id": 7,
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &v2_json);

        assert_eq!(state.trace_events.len(), 2);
        assert_eq!(state.next_trace_id, 7);
        assert_eq!(state.trace_events[0].id, 5);
        assert_eq!(state.trace_events[1].id, 6);
        match &state.trace_events[0].kind {
            TraceKind::Thinking { text, lines } => {
                assert_eq!(text, "推理过程");
                assert_eq!(*lines, 3);
            }
            _ => panic!("expected Thinking kind"),
        }
        match &state.trace_events[1].kind {
            TraceKind::ToolCall { name, status, .. } => {
                assert_eq!(name, "filengine.fs.read");
                assert!(matches!(status, crate::tui::state::ToolStatus::Success));
            }
            _ => panic!("expected ToolCall kind"),
        }
    }

    #[test]
    fn v2_missing_next_trace_id_falls_back_to_last_id_plus_1() {
        let json = serde_json::json!({
            "version": 2,
            "messages": [],
            "trace_events": [
                {
                    "id": 42, "time": "10:00", "category": "llm", "level": "Info",
                    "duration_ms": null,
                    "kind": { "type": "Generic", "content": "x" }
                },
            ],
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &json);
        assert_eq!(state.next_trace_id, 43, "missing next_trace_id falls back to last id+1");
    }

    #[test]
    fn v2_loads_session_alias_and_goal() {
        let json = serde_json::json!({
            "version": 2,
            "messages": [],
            "trace_events": [],
            "next_trace_id": 0,
            "session_alias": "feature-x",
            "session_goal": "connect turnkey to sandbox",
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &json);
        assert_eq!(state.session_alias.as_deref(), Some("feature-x"));
        assert_eq!(state.session_goal.as_deref(), Some("connect turnkey to sandbox"));
    }

    #[test]
    fn v2_missing_alias_and_goal_default_to_none() {
        let json = serde_json::json!({
            "version": 2,
            "messages": [],
            "trace_events": [],
            "next_trace_id": 0,
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &json);
        assert!(state.session_alias.is_none());
        assert!(state.session_goal.is_none());
    }

    #[test]
    fn v2_empty_alias_string_treated_as_none() {
        let json = serde_json::json!({
            "version": 2,
            "messages": [],
            "trace_events": [],
            "next_trace_id": 0,
            "session_alias": "",
            "session_goal": "",
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &json);
        assert!(state.session_alias.is_none());
        assert!(state.session_goal.is_none());
    }
}
