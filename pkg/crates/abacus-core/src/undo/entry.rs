//! undo::entry — log.jsonl 单条记录定义
//!
//! ## 引用关系
//! - 写入方：`undo::logger::PendingEntry::commit()` append 到 log.jsonl
//! - 读取方：Phase 3 `UndoEngine`（撤销时反序列化决策）；Phase 4 TUI history 展示
//!
//! ## 生命周期
//! - 创建：每次 fs.write/edit/move/mkdir 成功后写入一行
//! - 销毁：N/A（append-only；超容量由 logger 修剪）
//! - undone/undone_at/undone_by_seq 字段在 Phase 3 撤销时**重写整文件**回填（log.jsonl 唯一非纯 append 路径）

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 写操作类型 — 决定 Phase 3 撤销分支
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// 创建新文件（before_snapshot=None）
    Create,
    /// 覆盖已存在文件（before_snapshot=Some）
    Overwrite,
    /// fs.edit（before_snapshot=Some，与 Overwrite 同算法）
    Edit,
    /// fs.move（无 snapshot，仅 src/dst）
    Move,
    /// fs.mkdir（无 snapshot）
    Mkdir,
}

/// 文件元数据快照（用于冲突检查）
///
/// Phase 3 撤销前会比对当前文件与 `after_size + after_sha256`：
/// 不一致 → ExternalModification 冲突
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub size: u64,
    /// 64-char hex sha256
    pub sha256: String,
    pub mtime: DateTime<Utc>,
}

/// log.jsonl 单条记录
///
/// ## 字段语义（与 docs/design/file-undo.md § 3.1 jsonl 示例对齐）
/// - `seq`：session 内严格递增，由 `UndoLogger.seq_counter` AtomicU64 派发
/// - `path`：原 tool args 中的 path（绝对或相对，按 filengine resolve 前的形态保留——撤销时重 resolve）
/// - `before_snapshot`：相对 session_undo_dir 的 snapshot 文件名（None = create op）
/// - `after_*`：commit 时实际落盘的内容元数据
/// - `before_*`：snapshot_before 调用时的旧文件元数据（None = create op）
/// - `move_to`：仅 Move op 有效，记目的路径
/// - `undone*`：Phase 3 撤销时回填
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub seq: u64,
    pub session_id: String,
    pub turn: u32,
    pub timestamp: DateTime<Utc>,
    /// 工具名（"fs_write" / "fs_edit" / "fs_move" / "fs_mkdir" — 单一 _ 命名约定）
    pub tool: String,
    pub path: String,
    pub op: OpKind,

    /// 仅对 write/edit 非空 — snapshot 文件名（不含目录）
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub before_snapshot: Option<String>,

    /// commit 时实际落盘的元数据（Move/Mkdir 也填，便于 undo 校验）
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub after_meta: Option<FileMeta>,

    /// snapshot_before 时的旧文件元数据（None = path 不存在 / Move/Mkdir）
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub before_meta: Option<FileMeta>,

    /// 仅 Move op 有效
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub move_to: Option<String>,

    pub undone: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub undone_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub undone_by_seq: Option<u64>,
}

impl LogEntry {
    /// 默认构造：合法 entry 但所有 optional 字段为 None
    pub fn new(seq: u64, session_id: String, turn: u32, tool: String, path: String, op: OpKind) -> Self {
        Self {
            seq, session_id, turn, tool, path, op,
            timestamp: Utc::now(),
            before_snapshot: None,
            after_meta: None,
            before_meta: None,
            move_to: None,
            undone: false,
            undone_at: None,
            undone_by_seq: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_serializes_minimal_fields() {
        let e = LogEntry::new(1, "sid".into(), 7, "fs_mkdir".into(), "/x".into(), OpKind::Mkdir);
        let s = serde_json::to_string(&e).unwrap();
        // None 字段全部跳过
        assert!(!s.contains("before_snapshot"));
        assert!(!s.contains("after_meta"));
        assert!(!s.contains("undone_at"));
        // 必填字段在
        assert!(s.contains("\"seq\":1"));
        assert!(s.contains("\"op\":\"mkdir\""));
        assert!(s.contains("\"undone\":false"));
    }

    #[test]
    fn entry_round_trip() {
        let e = LogEntry::new(42, "s".into(), 1, "fs_write".into(), "/p".into(), OpKind::Create);
        let s = serde_json::to_string(&e).unwrap();
        let back: LogEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(back.seq, 42);
        assert_eq!(back.op, OpKind::Create);
        assert_eq!(back.path, "/p");
    }

    #[test]
    fn op_kind_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&OpKind::Overwrite).unwrap(), "\"overwrite\"");
        assert_eq!(serde_json::to_string(&OpKind::Mkdir).unwrap(), "\"mkdir\"");
    }
}
