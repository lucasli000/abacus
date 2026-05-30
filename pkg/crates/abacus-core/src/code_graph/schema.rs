//! CodeGraph DB Schema — 表定义与 migration
//!
//! ## 职责
//! 管理 knowledge.db 中 cg_* 前缀的所有表。
//! 提供 `ensure_tables()` 作为 idempotent migration 入口。
//!
//! ## 表结构
//! | 表名 | 类型 | 用途 |
//! |------|------|------|
//! | cg_symbols | 实体表 | 符号记录（函数/类型/trait 等） |
//! | cg_symbols_fts | FTS5 虚拟表 | 符号名 + 签名全文搜索 |
//! | cg_call_graph | 关系表 | 函数间调用关系 |
//! | cg_dep_graph | 关系表 | 文件间依赖关系 |
//! | cg_impl_graph | 关系表 | Trait/Interface 实现关系 |
//! | cg_file_meta | 元数据表 | 文件索引状态（hash/时间/错误数） |
//! | cg_index_checkpoint | 状态表 | 索引中断恢复点 |
//!
//! ## 依赖
//! - `rusqlite::Connection`: 共享 knowledge.db 连接
//!
//! ## 引用关系
//! - 被 `CodeGraphManager::new()` 调用 ensure_tables()
//! - 被 `Indexer::batch_commit()` 写入数据
//! - 被 `QueryEngine` 读取数据
//!
//! ## 设计约束
//! - 所有 DDL 使用 `IF NOT EXISTS`（幂等）
//! - FTS5 使用 trigram tokenizer（与 KB 一致）
//! - WAL 模式由 knowledge.db 全局配置，此处不重复设置

use rusqlite::{Connection, Result};

/// Schema 版本号（用于未来 migration）
pub const SCHEMA_VERSION: u32 = 1;

/// 确保所有 CodeGraph 表存在（幂等操作）
///
/// 在 CodeGraphManager::new() 时调用一次。
/// 使用 `IF NOT EXISTS` 确保重复调用安全。
///
/// ## 事务
/// 整个 migration 在一个事务内完成，失败则回滚（不留半成品表）。
pub fn ensure_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_V1)
}

const MIGRATION_V1: &str = r#"
BEGIN;

-- ═══════════════════════════════════════════════════════════════
-- cg_symbols: 符号实体表
-- ═══════════════════════════════════════════════════════════════
-- 每个符号（函数/结构体/trait/类/接口等）一条记录。
-- symbol_id = sha1(file + ":" + name + ":" + kind + ":" + line)
-- 确保同一位置的同一符号有唯一且确定性的 ID。
CREATE TABLE IF NOT EXISTS cg_symbols (
    symbol_id   TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL,
    file        TEXT NOT NULL,
    line        INTEGER NOT NULL,
    col         INTEGER NOT NULL,
    end_line    INTEGER,
    signature   TEXT,
    doc_comment TEXT,
    visibility  TEXT NOT NULL DEFAULT 'private',
    parent_id   TEXT,
    hash        TEXT NOT NULL,
    language    TEXT NOT NULL DEFAULT 'rust'
);

-- 索引：按文件查询（索引/删除旧符号时用）
CREATE INDEX IF NOT EXISTS idx_cg_symbols_file ON cg_symbols(file);
-- 索引：按名称精确查找（callee 解析时用）
CREATE INDEX IF NOT EXISTS idx_cg_symbols_name ON cg_symbols(name);
-- 索引：按 kind 过滤
CREATE INDEX IF NOT EXISTS idx_cg_symbols_kind ON cg_symbols(kind);

-- ═══════════════════════════════════════════════════════════════
-- cg_symbols_fts: FTS5 全文搜索（trigram tokenizer）
-- ═══════════════════════════════════════════════════════════════
-- 支持对符号名、签名、文档注释的模糊搜索。
-- content-less 模式（content=''）：数据存在 cg_symbols 表，
-- FTS5 只维护倒排索引，避免数据重复存储。
CREATE VIRTUAL TABLE IF NOT EXISTS cg_symbols_fts USING fts5(
    name,
    signature,
    doc_comment,
    content='cg_symbols',
    content_rowid='rowid',
    tokenize='trigram'
);

-- FTS5 同步触发器：INSERT
CREATE TRIGGER IF NOT EXISTS cg_symbols_ai AFTER INSERT ON cg_symbols BEGIN
    INSERT INTO cg_symbols_fts(rowid, name, signature, doc_comment)
    VALUES (new.rowid, new.name, new.signature, new.doc_comment);
END;

-- FTS5 同步触发器：DELETE
CREATE TRIGGER IF NOT EXISTS cg_symbols_ad AFTER DELETE ON cg_symbols BEGIN
    INSERT INTO cg_symbols_fts(cg_symbols_fts, rowid, name, signature, doc_comment)
    VALUES ('delete', old.rowid, old.name, old.signature, old.doc_comment);
END;

-- FTS5 同步触发器：UPDATE
CREATE TRIGGER IF NOT EXISTS cg_symbols_au AFTER UPDATE ON cg_symbols BEGIN
    INSERT INTO cg_symbols_fts(cg_symbols_fts, rowid, name, signature, doc_comment)
    VALUES ('delete', old.rowid, old.name, old.signature, old.doc_comment);
    INSERT INTO cg_symbols_fts(rowid, name, signature, doc_comment)
    VALUES (new.rowid, new.name, new.signature, new.doc_comment);
END;

-- ═══════════════════════════════════════════════════════════════
-- cg_call_graph: 函数间调用关系
-- ═══════════════════════════════════════════════════════════════
-- 记录 caller → callee 的调用关系。
-- 同一对 (caller, callee) 可能有多个调用点（不同行），
-- 所以 PK 包含 call_site_line。
CREATE TABLE IF NOT EXISTS cg_call_graph (
    caller_id       TEXT NOT NULL,
    callee_id       TEXT NOT NULL,
    call_site_line  INTEGER NOT NULL,
    call_site_col   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (caller_id, callee_id, call_site_line)
);

-- 索引：查找某符号的所有调用者（cg_graph direction=callers）
CREATE INDEX IF NOT EXISTS idx_cg_call_graph_callee ON cg_call_graph(callee_id);
-- 索引：查找某符号调用了谁（cg_graph direction=callees）
CREATE INDEX IF NOT EXISTS idx_cg_call_graph_caller ON cg_call_graph(caller_id);

-- ═══════════════════════════════════════════════════════════════
-- cg_dep_graph: 文件间依赖关系
-- ═══════════════════════════════════════════════════════════════
-- 记录文件级的依赖关系（import/use/require/mod）。
-- 用于循环依赖检测和模块耦合分析。
CREATE TABLE IF NOT EXISTS cg_dep_graph (
    source_file TEXT NOT NULL,
    target_file TEXT NOT NULL,
    dep_kind    TEXT NOT NULL,
    PRIMARY KEY (source_file, target_file, dep_kind)
);

-- 索引：查找某文件的所有被依赖者（reverse deps）
CREATE INDEX IF NOT EXISTS idx_cg_dep_graph_target ON cg_dep_graph(target_file);

-- ═══════════════════════════════════════════════════════════════
-- cg_impl_graph: Trait/Interface 实现关系
-- ═══════════════════════════════════════════════════════════════
-- 记录 trait → impl 或 interface → class 的实现关系。
CREATE TABLE IF NOT EXISTS cg_impl_graph (
    trait_id    TEXT NOT NULL,
    impl_id     TEXT NOT NULL,
    impl_file   TEXT NOT NULL,
    impl_line   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (trait_id, impl_id)
);

-- 索引：查找某 trait 的所有实现
CREATE INDEX IF NOT EXISTS idx_cg_impl_graph_trait ON cg_impl_graph(trait_id);

-- ═══════════════════════════════════════════════════════════════
-- cg_file_meta: 文件索引元数据
-- ═══════════════════════════════════════════════════════════════
-- 每个已索引文件一条记录。用于增量索引判断：
-- - hash 未变 → 跳过重索引
-- - last_indexed_at → 计算索引新鲜度（degradation 信号）
-- - parse_errors → 标记解析不完整的文件
CREATE TABLE IF NOT EXISTS cg_file_meta (
    file            TEXT PRIMARY KEY,
    hash            TEXT NOT NULL,
    symbol_count    INTEGER NOT NULL DEFAULT 0,
    last_indexed_at INTEGER NOT NULL,
    parse_errors    INTEGER NOT NULL DEFAULT 0,
    language        TEXT NOT NULL DEFAULT 'rust',
    file_size       INTEGER NOT NULL DEFAULT 0
);

-- ═══════════════════════════════════════════════════════════════
-- cg_index_checkpoint: 索引中断恢复
-- ═══════════════════════════════════════════════════════════════
-- 长时间索引（大型代码库）的检查点。
-- 进程中断后恢复时读取此表，跳过已完成文件。
CREATE TABLE IF NOT EXISTS cg_index_checkpoint (
    session_id      TEXT PRIMARY KEY,
    workspace       TEXT NOT NULL,
    total_files     INTEGER NOT NULL DEFAULT 0,
    completed_count INTEGER NOT NULL DEFAULT 0,
    failed_count    INTEGER NOT NULL DEFAULT 0,
    started_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    status          TEXT NOT NULL DEFAULT 'running'
);

-- ═══════════════════════════════════════════════════════════════
-- cg_schema_version: Schema 版本追踪
-- ═══════════════════════════════════════════════════════════════
CREATE TABLE IF NOT EXISTS cg_schema_version (
    version     INTEGER PRIMARY KEY,
    applied_at  INTEGER NOT NULL
);

-- 记录当前版本（幂等：INSERT OR IGNORE）
INSERT OR IGNORE INTO cg_schema_version (version, applied_at)
VALUES (1, strftime('%s', 'now'));

COMMIT;
"#;

/// 清空所有 CodeGraph 数据（保留表结构）
///
/// 用于测试或强制重建索引场景。
pub fn clear_all_data(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN;
         DELETE FROM cg_symbols;
         DELETE FROM cg_call_graph;
         DELETE FROM cg_dep_graph;
         DELETE FROM cg_impl_graph;
         DELETE FROM cg_file_meta;
         DELETE FROM cg_index_checkpoint;
         COMMIT;"
    )
}

/// 删除某个文件的所有关联数据（重索引前清理）
///
/// 原子操作：事务内删除该文件的符号、调用关系、依赖关系。
pub fn remove_file_data(conn: &Connection, file_path: &str) -> Result<()> {
    // 先获取该文件的所有 symbol_id
    let mut stmt = conn.prepare(
        "SELECT symbol_id FROM cg_symbols WHERE file = ?1"
    )?;
    let symbol_ids: Vec<String> = stmt.query_map([file_path], |row| {
        row.get(0)
    })?.collect::<Result<Vec<_>>>()?;

    // 事务内删除
    conn.execute_batch("BEGIN")?;

    // 删除调用关系（caller 或 callee 是该文件的符号）
    for id in &symbol_ids {
        conn.execute("DELETE FROM cg_call_graph WHERE caller_id = ?1 OR callee_id = ?1", [id])?;
        conn.execute("DELETE FROM cg_impl_graph WHERE trait_id = ?1 OR impl_id = ?1", [id])?;
    }

    // 删除符号
    conn.execute("DELETE FROM cg_symbols WHERE file = ?1", [file_path])?;

    // 删除文件级依赖
    conn.execute(
        "DELETE FROM cg_dep_graph WHERE source_file = ?1 OR target_file = ?1",
        [file_path],
    )?;

    // 删除文件元数据
    conn.execute("DELETE FROM cg_file_meta WHERE file = ?1", [file_path])?;

    conn.execute_batch("COMMIT")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ensure_tables_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        // 第一次
        ensure_tables(&conn).unwrap();
        // 第二次（幂等）
        ensure_tables(&conn).unwrap();

        // 验证核心表存在（不含 FTS5 shadow tables 和虚拟表）
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name LIKE 'cg_%' AND name NOT LIKE '%_fts%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // cg_symbols, cg_call_graph, cg_dep_graph, cg_impl_graph, cg_file_meta,
        // cg_index_checkpoint, cg_schema_version = 7 tables
        assert_eq!(count, 7);
    }

    #[test]
    fn test_fts5_trigger_sync() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_tables(&conn).unwrap();

        // 插入符号
        conn.execute(
            "INSERT INTO cg_symbols (symbol_id, name, kind, file, line, col, visibility, hash, language)
             VALUES ('s1', 'process_turn', 'function', 'core/mod.rs', 100, 4, 'pub', 'abc123', 'rust')",
            [],
        ).unwrap();

        // FTS5 搜索应该能找到
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cg_symbols_fts WHERE cg_symbols_fts MATCH 'process'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_clear_all_data() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_tables(&conn).unwrap();

        conn.execute(
            "INSERT INTO cg_symbols (symbol_id, name, kind, file, line, col, visibility, hash, language)
             VALUES ('s1', 'foo', 'function', 'a.rs', 1, 0, 'pub', 'h1', 'rust')",
            [],
        ).unwrap();

        clear_all_data(&conn).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cg_symbols", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_remove_file_data_cascades() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_tables(&conn).unwrap();

        // 插入两个文件的符号
        conn.execute_batch(
            "INSERT INTO cg_symbols VALUES ('s1', 'foo', 'function', 'a.rs', 1, 0, NULL, NULL, NULL, 'pub', NULL, 'h1', 'rust');
             INSERT INTO cg_symbols VALUES ('s2', 'bar', 'function', 'b.rs', 1, 0, NULL, NULL, NULL, 'pub', NULL, 'h2', 'rust');
             INSERT INTO cg_call_graph VALUES ('s1', 's2', 10, 5);
             INSERT INTO cg_dep_graph VALUES ('a.rs', 'b.rs', 'use');
             INSERT INTO cg_file_meta VALUES ('a.rs', 'ha', 1, 1000, 0, 'rust', 100);
             INSERT INTO cg_file_meta VALUES ('b.rs', 'hb', 1, 1000, 0, 'rust', 200);"
        ).unwrap();

        // 删除 a.rs 的数据
        remove_file_data(&conn, "a.rs").unwrap();

        // a.rs 符号应该被删除
        let sym_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cg_symbols WHERE file = 'a.rs'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sym_count, 0);

        // b.rs 符号仍在
        let sym_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cg_symbols WHERE file = 'b.rs'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sym_count, 1);

        // 调用关系应被清理（s1 是 a.rs 的）
        let call_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cg_call_graph", [], |r| r.get(0))
            .unwrap();
        assert_eq!(call_count, 0);

        // 文件元数据清理
        let meta_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cg_file_meta WHERE file = 'a.rs'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(meta_count, 0);
    }
}
