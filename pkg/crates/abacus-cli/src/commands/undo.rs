//! commands::undo — Phase 5 file-undo CLI subcommands
//!
//! ## 引用关系
//! - 上游：`crate::commands::{UndoArgs, RedoArgs, HistoryArgs}` 解析 + main.rs 派发
//! - 依赖：`abacus_core::undo::{UndoEngine, ...}`、`abacus_core::paths::current_project_dir`
//!
//! ## 生命周期
//! - 每次 `abacus undo|redo|history` 调用时新建 UndoEngine（指向 cwd 项目目录）
//! - 不 attach 到 CoreLoop（CLI 是无状态短任务）
//! - redo 栈为空（CLI 进程级），仅 abacus undo 重做"上次撤销"在同一进程内有效
//!
//! ## 输出格式
//! - markdown：人类可读，对齐 TUI render 风格
//! - json：脚本友好，所有字段保留

use abacus_core::paths::current_project_dir;
use abacus_core::undo::{HistoryEntry, OpKind, UndoAction, UndoConflict, UndoEngine, UndoResult};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::sync::Arc;

use super::{HistoryArgs, RedoArgs, UndoArgs};
use crate::tui::util::safe_prefix;

/// `abacus undo` 入口
pub async fn handle_undo(args: &UndoArgs) -> Result<(), String> {
    let engine = Arc::new(UndoEngine::new(current_project_dir()));

    let result = match (args.seq, args.turn, &args.session) {
        // /undo seq <N> --session <S> 或 --seq <N>（含 session 默认值）
        (Some(seq), _, _) => {
            let session = args.session.as_deref()
                .ok_or_else(|| "--session required when using --seq".to_string())?;
            engine.undo_seq(session, seq).await
                .map_err(|e| format!("undo seq={seq} failed: {e}"))
                .map(|r| vec![r])
        }
        // --turn <N> --session <S>
        (None, Some(turn), Some(session)) => {
            engine.undo_turn(session, turn).await
                .map_err(|e| format!("undo turn={turn} failed: {e}"))
        }
        (None, Some(_), None) => {
            return Err("--session required when using --turn".into());
        }
        // 无 seq / turn → undo last
        (None, None, _) => {
            engine.undo_last(args.session.as_deref()).await
                .map_err(|e| format!("undo last failed: {e}"))
                .map(|r| vec![r])
        }
    }?;

    print_undo_results(&result, &args.format);
    Ok(())
}

/// `abacus redo` 入口
pub async fn handle_redo(args: &RedoArgs) -> Result<(), String> {
    let engine = Arc::new(UndoEngine::new(current_project_dir()));
    let r = engine.redo(&args.session).await
        .map_err(|e| format!("redo failed: {e}"))?;
    print_undo_results(&[r], &args.format);
    Ok(())
}

/// `abacus history` 入口
pub async fn handle_history(args: &HistoryArgs) -> Result<(), String> {
    let engine = Arc::new(UndoEngine::new(current_project_dir()));

    let entries = if args.project {
        let since = parse_since(&args.since)?;
        engine.timeline(since).map_err(|e| format!("timeline failed: {e}"))?
    } else {
        engine.history(args.session.as_deref(), args.limit)
            .map_err(|e| format!("history failed: {e}"))?
    };

    match args.format.as_str() {
        "json" => print_history_json(&entries),
        _ => print_history_markdown(&entries, args.project, &args.since),
    }
    Ok(())
}

// ─── --since 解析（1h / 30m / 7d / RFC3339） ────────────────────

/// 解析时间窗口字符串。返回 since DateTime<Utc>（now - duration）
///
/// 支持格式：
/// - "1h" / "30m" / "7d" / "60s" / "2w"
/// - RFC3339 绝对时间："2026-05-23T10:00:00Z"
pub fn parse_since(s: &str) -> Result<DateTime<Utc>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("--since cannot be empty".into());
    }

    // RFC3339 优先
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    // 后缀单位：s/m/h/d/w
    let (num_str, unit) = match s.chars().last() {
        Some(c) if !c.is_ascii_digit() => (&s[..s.len() - c.len_utf8()], c),
        _ => return Err(format!("invalid --since: {s} (expected '1h' / '30m' / RFC3339)")),
    };

    let n: i64 = num_str.parse()
        .map_err(|_| format!("invalid --since number: {num_str}"))?;
    if n < 0 {
        return Err("--since cannot be negative".into());
    }

    let duration = match unit {
        's' => chrono::Duration::seconds(n),
        'm' => chrono::Duration::minutes(n),
        'h' => chrono::Duration::hours(n),
        'd' => chrono::Duration::days(n),
        'w' => chrono::Duration::weeks(n),
        other => return Err(format!("invalid --since unit: '{other}' (expected s/m/h/d/w)")),
    };

    Ok(Utc::now() - duration)
}

// ─── 输出格式化 ──────────────────────────────────────────────

fn print_undo_results(results: &[UndoResult], format: &str) {
    match format {
        "json" => {
            let arr: Vec<_> = results.iter().map(undo_result_to_json).collect();
            println!("{}", serde_json::to_string_pretty(&json!(arr)).unwrap_or_default());
        }
        _ => {
            for r in results {
                println!("{}", format_undo_md(r));
            }
        }
    }
}

fn undo_result_to_json(r: &UndoResult) -> serde_json::Value {
    json!({
        "seq": r.seq,
        "session_id": r.session_id,
        "path": r.path.to_string_lossy(),
        "action": format!("{:?}", r.action),
        "conflict": r.conflict.as_ref().map(conflict_to_json),
    })
}

fn conflict_to_json(c: &UndoConflict) -> serde_json::Value {
    match c {
        UndoConflict::ExternalModification { observed_sha256, expected_sha256 } => json!({
            "type": "ExternalModification",
            "observed_sha256": observed_sha256,
            "expected_sha256": expected_sha256,
        }),
        UndoConflict::FileGone => json!({"type": "FileGone"}),
        UndoConflict::DirectoryNotEmpty { entries } => json!({
            "type": "DirectoryNotEmpty",
            "entries": entries,
        }),
        UndoConflict::DestinationOccupied => json!({"type": "DestinationOccupied"}),
    }
}

fn format_undo_md(r: &UndoResult) -> String {
    let action_str = match r.action {
        UndoAction::RestoredContent => "restored content",
        UndoAction::RemovedFile => "removed file",
        UndoAction::RemovedDir => "removed empty dir",
        UndoAction::ReverseMoved => "reverse moved",
        UndoAction::Aborted => "aborted (conflict)",
    };
    let path_str = r.path.to_string_lossy();
    let header = format!("undo seq={} session={} action={} path={}",
        r.seq, safe_prefix(&r.session_id, 8), action_str, path_str);

    if let Some(c) = &r.conflict {
        let detail = match c {
            UndoConflict::ExternalModification { observed_sha256, expected_sha256 } =>
                format!("external_modification expected={} observed={}",
                    &expected_sha256[..16], &observed_sha256[..16]),
            UndoConflict::FileGone => "file_gone".to_string(),
            UndoConflict::DirectoryNotEmpty { entries } =>
                format!("directory_not_empty entries={}", entries.join(",")),
            UndoConflict::DestinationOccupied => "destination_occupied".to_string(),
        };
        format!("{header}\n  conflict: {detail}")
    } else {
        header
    }
}

fn print_history_json(entries: &[HistoryEntry]) {
    let arr: Vec<_> = entries.iter().map(history_entry_to_json).collect();
    println!("{}", serde_json::to_string_pretty(&json!(arr)).unwrap_or_default());
}

fn history_entry_to_json(e: &HistoryEntry) -> serde_json::Value {
    json!({
        "seq": e.seq,
        "session_id": e.session_id,
        "turn": e.turn,
        "timestamp": e.timestamp.to_rfc3339(),
        "tool": e.tool,
        "path": e.path,
        "op": op_kind_str(&e.op),
        "undone": e.undone,
    })
}

fn op_kind_str(o: &OpKind) -> &'static str {
    match o {
        OpKind::Create => "create",
        OpKind::Overwrite => "overwrite",
        OpKind::Edit => "edit",
        OpKind::Move => "move",
        OpKind::Mkdir => "mkdir",
    }
}

fn print_history_markdown(entries: &[HistoryEntry], project_mode: bool, since_label: &str) {
    if entries.is_empty() {
        if project_mode {
            println!("(no entries in last {since_label})");
        } else {
            println!("(no history)");
        }
        return;
    }

    let title = if project_mode {
        format!("# Project Timeline (last {since_label}, {} entries)\n", entries.len())
    } else {
        format!("# Undo History ({} entries)\n", entries.len())
    };
    println!("{title}");
    println!("| seq | session  | turn | tool      | op        | path | status |");
    println!("|----:|----------|-----:|-----------|-----------|------|--------|");
    for e in entries {
        let sid_short = safe_prefix(&e.session_id, 8);
        let status = if e.undone { "↺ undone" } else { "✓ active" };
        let path_short = if e.path.len() > 50 {
            format!("…{}", &e.path[e.path.len() - 47..])
        } else {
            e.path.clone()
        };
        println!("| {} | {} | {} | {} | {} | {} | {} |",
            e.seq, sid_short, e.turn, e.tool, op_kind_str(&e.op), path_short, status);
    }
}

// ─── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_since_seconds() {
        let r = parse_since("60s").unwrap();
        let now = Utc::now();
        let diff = now.signed_duration_since(r);
        assert!(diff.num_seconds() >= 59 && diff.num_seconds() <= 61);
    }

    #[test]
    fn parse_since_minutes_hours_days_weeks() {
        assert!(parse_since("30m").is_ok());
        assert!(parse_since("1h").is_ok());
        assert!(parse_since("7d").is_ok());
        assert!(parse_since("2w").is_ok());
    }

    #[test]
    fn parse_since_rfc3339() {
        let dt = parse_since("2026-05-23T10:00:00Z").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-05-23T10:00:00+00:00");
    }

    #[test]
    fn parse_since_invalid_unit() {
        assert!(parse_since("1y").is_err());
        assert!(parse_since("3").is_err());
        assert!(parse_since("").is_err());
        assert!(parse_since("abc").is_err());
    }

    #[test]
    fn parse_since_negative_rejected() {
        assert!(parse_since("-1h").is_err());
    }

    // ─── JSON 输出形态 ─────────────────────────────────────

    #[test]
    fn undo_result_json_contains_required_fields() {
        let r = UndoResult {
            seq: 7,
            session_id: "sess-test".into(),
            path: "/x.txt".into(),
            action: UndoAction::RemovedFile,
            conflict: None,
        };
        let v = undo_result_to_json(&r);
        assert_eq!(v["seq"], 7);
        assert_eq!(v["session_id"], "sess-test");
        assert_eq!(v["path"], "/x.txt");
        assert_eq!(v["action"], "RemovedFile");
        assert!(v["conflict"].is_null());
    }

    #[test]
    fn conflict_serializes_with_type_tag() {
        let c = UndoConflict::FileGone;
        let v = conflict_to_json(&c);
        assert_eq!(v["type"], "FileGone");

        let c2 = UndoConflict::DirectoryNotEmpty { entries: vec!["a".into(), "b".into()] };
        let v2 = conflict_to_json(&c2);
        assert_eq!(v2["type"], "DirectoryNotEmpty");
        assert_eq!(v2["entries"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn op_kind_str_round_trip() {
        assert_eq!(op_kind_str(&OpKind::Create), "create");
        assert_eq!(op_kind_str(&OpKind::Overwrite), "overwrite");
        assert_eq!(op_kind_str(&OpKind::Edit), "edit");
        assert_eq!(op_kind_str(&OpKind::Move), "move");
        assert_eq!(op_kind_str(&OpKind::Mkdir), "mkdir");
    }
}
