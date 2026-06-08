//! V42-B Phase 13: Session v3 → v4 透明升级
//!
//! ## 背景
//!
//! V40 时期的 session 文件用 `messages: Vec<Message>` 存储对话历史。
//! V42-B 升级为 `cards: CardStream`, 数据模型完全不同。
//!
//! 为了保证 V40 session 文件在 V42-B 仍可加载, 提供透明迁移:
//! - 检测 v3 格式 (有 `messages` 字段, 无 `cards` 字段)
//! - 备份 v3 文件到 `.v3_backup/<uuid>.json` (与 .v3_backup/ 同级)
//! - 返回 v4 格式 (本 Phase 暂不实际转换 messages → cards,
//!   保留 `messages` 字段以兼容 V40 渲染路径, Phase 14 切换)
//!
//! ## 迁移流程
//!
//! ```text
//! 1. load_session_from_path() 读 .json
//! 2. detect_session_version() 判定 1/2/3/4
//! 3. v3 → migrate_v3_to_v4(): 复制 v3 内容 + 备份 v3 到 .v3_backup/
//! 4. apply_session_export() 应用 v4 数据
//! ```
//!
//! ## Phase 14 衔接
//!
//! 完整迁移需要把 v3 messages 转换为 v4 cards。
//! Phase 14 切换渲染路径 (render_cards 替代 render_messages_in_card) 后,
//! 会调用本模块的 `migrate_messages_to_cards()` 把 Vec<Message> 转换
//! 为 CardStream, 写回 session 文件。

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Session 版本号
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionVersion {
    /// V28 早期: events 数组, 无 version 字段
    V1,
    /// V28+: trace_events + next_trace_id, version=2
    V2,
    /// V40: messages 数组, 无 version 字段或 version 缺省
    V3,
    /// V42-B: cards 字段 (本 Phase 暂未启用, 保持 V3 + 备份)
    V4,
}

impl SessionVersion {
    /// 从 JSON 导出判定版本号
    pub fn detect(export: &Value) -> Self {
        // 优先看 version 字段
        if let Some(v) = export.get("version").and_then(|v| v.as_u64()) {
            return match v {
                1 => SessionVersion::V1,
                2 => SessionVersion::V2,
                3 => SessionVersion::V3,
                _ => SessionVersion::V4,
            };
        }
        // 无 version 字段: 看是否有 messages 数组 (V40 特征)
        if export.get("messages").is_some() {
            SessionVersion::V3
        } else if export.get("events").is_some() {
            SessionVersion::V1
        } else if export.get("trace_events").is_some() {
            SessionVersion::V2
        } else {
            // 默认按 V3 处理 (空 session 也算 V3)
            SessionVersion::V3
        }
    }
}

/// 迁移 v3 → v4 (本 Phase: 仅备份 v3, 不实际转换 messages → cards)
///
/// ## 当前实现
///
/// - 不修改 `export` 内容 (保持 V40 messages 字段)
/// - 备份 v3 源文件到 `<session_dir>/.v3_backup/<uuid>.json`
/// - 返回 Ok(V3 原 export, 等 Phase 14 切换时再做 messages → cards)
///
/// ## Phase 14 计划
///
/// - 调用 `migrate_messages_to_cards(export)` 把 messages 转换并写入 cards 字段
/// - 写回 session 文件, version=4
/// - 删除 messages 字段 (节省空间)
pub fn migrate_v3_to_v4(
    export: Value,
    source_path: &Path,
) -> std::io::Result<Value> {
    // 1. 备份 v3 文件
    backup_v3_file(export.clone(), source_path)?;

    // 2. 本 Phase 暂不实际转换, 返回原 export
    // Phase 14 会在这里插入 migrate_messages_to_cards
    Ok(export)
}

/// 把 v3 文件备份到 `<session_dir>/.v3_backup/<uuid>.json`
fn backup_v3_file(export: Value, source_path: &Path) -> std::io::Result<()> {
    let session_dir = source_path
        .parent()
        .ok_or_else(|| std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session path has no parent directory",
        ))?;

    let backup_dir = session_dir.join(".v3_backup");
    std::fs::create_dir_all(&backup_dir)?;

    // 文件名: <source_stem>.json (保留原 UUID)
    let backup_path = backup_dir.join(
        source_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("unknown.json"))
    );

    // 已存在备份 → 加 .<n> 后缀避免覆盖
    let final_path = unique_backup_path(&backup_path);

    let json = serde_json::to_string_pretty(&export)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&final_path, json)?;
    Ok(())
}

/// 防止覆盖已有备份, 加 .<n> 后缀
fn unique_backup_path(base: &Path) -> PathBuf {
    if !base.exists() {
        return base.to_path_buf();
    }
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("backup");
    let ext = base
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("json");
    for n in 1..1000 {
        let candidate = base.with_file_name(format!("{}.{}.{}", stem, n, ext));
        if !candidate.exists() {
            return candidate;
        }
    }
    base.to_path_buf()
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Phase 14 占位: messages → cards 转换
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 把 V40 `Vec<Message>` 转换为 V42-B Card 列表 (JSON 形式)
///
/// ## 输入
/// - `export`: V3 session JSON, 包含 `messages: Vec<Message>`
///
/// ## 输出
/// - JSON 对象, 包含 `cards` 字段 (Card 列表)
/// - 不修改原 `messages` 字段 (Phase 14 切换渲染后才会删)
///
/// ## Phase 14 实施
///
/// 本函数目前是占位, 返回空 `cards` 数组。Phase 14 实施完整转换:
/// 1. 遍历 messages
/// 2. 每条 Message 按 role 分发:
///    - User → 1 UserCard
///    - Session → 1+ Card (LlmCard / AbacusCard / Block 转换)
///    - Expert → 1 ExpertCard
/// 3. 写入 `cards` 字段
pub fn migrate_messages_to_cards(_export: &Value) -> Value {
    // Phase 13 占位: 返回空 cards
    // Phase 14 实施完整转换 (见函数文档)
    serde_json::json!({
        "cards": []
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detect_v1_by_events_field() {
        let v = json!({"events": [{"id": 1, "time": "09:00", "category": "llm", "content": "hi"}]});
        assert_eq!(SessionVersion::detect(&v), SessionVersion::V1);
    }

    #[test]
    fn detect_v2_by_trace_events_field() {
        let v = json!({
            "version": 2,
            "trace_events": [],
            "next_trace_id": 0
        });
        assert_eq!(SessionVersion::detect(&v), SessionVersion::V2);
    }

    #[test]
    fn detect_v2_by_trace_events_no_version() {
        let v = json!({
            "trace_events": [],
            "next_trace_id": 0
        });
        // 无 version 字段, 有 trace_events → V2
        assert_eq!(SessionVersion::detect(&v), SessionVersion::V2);
    }

    #[test]
    fn detect_v3_by_messages_field() {
        let v = json!({
            "messages": [{"role": "User", "parts": [], "time": "09:00"}]
        });
        assert_eq!(SessionVersion::detect(&v), SessionVersion::V3);
    }

    #[test]
    fn detect_v4_by_version_4() {
        let v = json!({"version": 4, "cards": []});
        assert_eq!(SessionVersion::detect(&v), SessionVersion::V4);
    }

    #[test]
    fn detect_empty_defaults_to_v3() {
        let v = json!({});
        assert_eq!(SessionVersion::detect(&v), SessionVersion::V3);
    }

    #[test]
    fn migrate_v3_to_v4_creates_backup() {
        // 用临时目录
        let tmp = std::env::temp_dir().join(format!("abacus-migrate-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let v3_path = tmp.join("test-session.json");
        let v3_content = json!({
            "version": 3,
            "messages": [{"role": "User", "parts": [], "time": "09:00"}]
        });
        std::fs::write(&v3_path, serde_json::to_string(&v3_content).unwrap()).unwrap();

        // 调用迁移
        let result = migrate_v3_to_v4(v3_content.clone(), &v3_path).unwrap();
        // 本 Phase 暂不转换, 返回原 export
        assert_eq!(result, v3_content);

        // 备份应存在
        let backup_dir = tmp.join(".v3_backup");
        assert!(backup_dir.exists());
        let backup_file = backup_dir.join("test-session.json");
        assert!(backup_file.exists());

        // 清理
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn unique_backup_path_avoids_overwrite() {
        let tmp = std::env::temp_dir().join(format!("abacus-unique-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let base = tmp.join("session.json");
        std::fs::write(&base, "{}").unwrap();
        let unique = unique_backup_path(&base);
        assert!(unique.to_string_lossy().contains(".1."));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn migrate_messages_to_cards_returns_empty() {
        // Phase 13 占位: 返回空 cards
        let result = migrate_messages_to_cards(&json!({}));
        assert_eq!(result, json!({"cards": []}));
    }
}


