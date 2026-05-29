//! db — Built-in SQLite database tools
//!
//! ## 场景
//! Abacus 内嵌 DB 操作，脱离 filengine MCP 独立运行。
//! 默认 DB 路径: ~/.abacus/memory.db，可通过参数 `db` 覆盖。
//!
//! ## 依赖
//! - `rusqlite`: SQLite 连接和查询
//! - `abacus_types`: 工具注册类型
//! - `crate::tool`: ToolExecutor trait + ToolRegistry
//!
//! ## 引用关系
//! - 被 `builtin::mod.rs::register_all()` 调用注册
//! - 被 `CoreLoop::process_turn()` 通过 ToolRegistry 执行
//!
//! ## 注册工具 (8)
//! | Tool | Confirm | Risk | Description |
//! |------|---------|------|-------------|
//! | db.info | no | low | 数据库元信息（路径/大小/表数） |
//! | db.list_tables | no | low | 列出用户表（排除 sqlite_*） |
//! | db.table_schema | no | low | 表结构（列名/类型/PK） |
//! | db.query | no | medium | 参数化 SQL 执行 |
//! | db.create_record | no | low | 插入记录 |
//! | db.read_records | no | low | 条件查询 + 分页 |
//! | db.update_records | yes | medium | 条件更新（conditions 必填） |
//! | db.delete_records | yes | medium | 条件删除（conditions 必填） |

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use abacus_types::{
    KernelError, ToolCost, ToolEffectiveness, ToolHandle, ToolId, ToolProvider,
    ToolSchema, ToolSecurity, ToolState,
};
use async_trait::async_trait;
use rusqlite::{params_from_iter, types::Value as SqlValue, Connection};
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};

// ─── 常量 ───────────────────────────────────────────────────────────────

// 默认 DB 路径来自 crate::paths，遵循 ABACUS_HOME 覆盖。
// 不再 hardcode "~/.abacus/memory.db"——见 paths::memory_db()。

// ─── Executor ───────────────────────────────────────────────────────────

/// DB 工具执行器
///
/// ## 场景
/// 接收 LLM 发出的 db.* 工具调用，执行 SQLite 操作并返回 JSON 结果。
///
/// ## 生命周期
/// - 创建：`register()` 时构造
/// - 存活：与 ToolRegistry 同生命周期（整个 app）
/// - 连接：lazy init，每个 DB path 一个连接缓存
pub struct DbToolExecutor {
    default_path: PathBuf,
    connections: Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>>,
}

impl Default for DbToolExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl DbToolExecutor {
    pub fn new() -> Self {
        let default_path = crate::paths::memory_db();
        Self {
            default_path,
            connections: Mutex::new(HashMap::new()),
        }
    }

    /// 获取或创建 DB 连接（WAL mode + busy_timeout）
    ///
    /// ## 策略
    /// 快路径：缓存命中时仅持锁微秒级
    /// 慢路径：miss 时释放锁 → 创建连接（IO）→ 重新加锁 → 双检插入
    async fn get_conn(&self, db_param: Option<&str>) -> Result<Arc<Mutex<Connection>>, KernelError> {
        let path = match db_param {
            Some(p) => expand_tilde(p),
            None => self.default_path.clone(),
        };

        // 快路径：缓存命中
        {
            let conns = self.connections.lock().await;
            if let Some(conn) = conns.get(&path) {
                return Ok(conn.clone());
            }
        } // 释放锁

        // 慢路径：在锁外创建连接（IO 操作不阻塞其他请求）
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e|
                KernelError::Other(format!("cannot create db directory: {e}")))?;
        }

        let conn = Connection::open(&path).map_err(|e|
            KernelError::Other(format!("cannot open db: {e}")))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;"
        ).map_err(|e| KernelError::Other(format!("pragma failed: {e}")))?;

        let arc = Arc::new(Mutex::new(conn));

        // 双检插入：重新加锁，检查是否已被其他线程创建
        let mut conns = self.connections.lock().await;
        if let Some(existing) = conns.get(&path) {
            return Ok(existing.clone()); // 另一个线程先创建了
        }
        conns.insert(path, arc.clone());
        Ok(arc)
    }

    // ─── Tool implementations ────────────────────────────────────────

    async fn db_info(&self, params: Value) -> abacus_types::Result<Value> {
        let db_param = params.get("db").and_then(|v| v.as_str());
        let path = match db_param {
            Some(p) => expand_tilde(p),
            None => self.default_path.clone(),
        };

        let exists = path.exists();
        let size = if exists {
            std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };

        let table_count = if exists {
            let conn = self.get_conn(db_param).await?;
            let c = conn.lock().await;
            let mut stmt = c.prepare(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"
            ).map_err(|e| KernelError::Other(e.to_string()))?;
            stmt.query_row([], |row| row.get::<_, i64>(0))
                .unwrap_or(0)
        } else {
            0
        };

        Ok(json!({
            "dbPath": path.display().to_string(),
            "exists": exists,
            "size": size,
            "tableCount": table_count,
        }))
    }

    async fn db_list_tables(&self, params: Value) -> abacus_types::Result<Value> {
        let db_param = params.get("db").and_then(|v| v.as_str());
        let conn = self.get_conn(db_param).await?;
        let c = conn.lock().await;

        let mut stmt = c.prepare(
            "SELECT name, sql FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
        ).map_err(|e| KernelError::Other(e.to_string()))?;

        let tables: Vec<Value> = stmt.query_map([], |row| {
            Ok(json!({
                "name": row.get::<_, String>(0)?,
                "sql": row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            }))
        }).map_err(|e| KernelError::Other(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(json!(tables))
    }

    async fn db_table_schema(&self, params: Value) -> abacus_types::Result<Value> {
        let table = get_str(&params, "tableName")?;
        let db_param = params.get("db").and_then(|v| v.as_str());
        let conn = self.get_conn(db_param).await?;
        let c = conn.lock().await;

        let mut stmt = c.prepare(&format!("PRAGMA table_info(\"{}\")", escape_ident(table)))
            .map_err(|e| KernelError::Other(e.to_string()))?;

        let columns: Vec<Value> = stmt.query_map([], |row| {
            Ok(json!({
                "cid": row.get::<_, i64>(0)?,
                "name": row.get::<_, String>(1)?,
                "type": row.get::<_, String>(2)?,
                "notNull": row.get::<_, bool>(3)?,
                "defaultValue": row.get::<_, Option<String>>(4)?,
                "primaryKey": row.get::<_, bool>(5)?,
            }))
        }).map_err(|e| KernelError::Other(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();

        if columns.is_empty() {
            return Err(KernelError::Other(format!("table '{}' not found", table)));
        }
        Ok(json!(columns))
    }

    async fn db_query(&self, params: Value) -> abacus_types::Result<Value> {
        let sql = get_str(&params, "sql")?;
        let db_param = params.get("db").and_then(|v| v.as_str());
        let values = params.get("values").and_then(|v| v.as_array());

        // 安全检查：禁止危险 SQL 语句
        let sql_upper = sql.trim().to_uppercase();
        if sql_upper.starts_with("ATTACH") || sql_upper.starts_with("DETACH") {
            return Err(KernelError::Other("ATTACH/DETACH not allowed for security".into()));
        }

        let conn = self.get_conn(db_param).await?;
        let c = conn.lock().await;

        let mut stmt = c.prepare(sql)
            .map_err(|e| KernelError::Other(format!("SQL prepare error: {e}")))?;

        let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();

        // 转换参数
        let sql_params: Vec<SqlValue> = match values {
            Some(arr) => arr.iter().map(json_to_sql).collect(),
            None => vec![],
        };

        let rows: Vec<Value> = stmt.query_map(params_from_iter(sql_params.iter()), |row| {
            let mut obj = Map::new();
            for (i, name) in col_names.iter().enumerate() {
                let val = sql_val_to_json(row, i);
                obj.insert(name.clone(), val);
            }
            Ok(Value::Object(obj))
        }).map_err(|e| KernelError::Other(format!("SQL query error: {e}")))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(json!(rows))
    }

    async fn db_create_record(&self, params: Value) -> abacus_types::Result<Value> {
        let table = get_str(&params, "table")?;
        let data = get_object(&params, "data")?;
        let db_param = params.get("db").and_then(|v| v.as_str());

        if data.is_empty() {
            return Err(KernelError::Other("data must not be empty".into()));
        }

        let conn = self.get_conn(db_param).await?;
        let c = conn.lock().await;

        let columns: Vec<String> = data.keys().map(|k| format!("\"{}\"", escape_ident(k))).collect();
        let placeholders: Vec<&str> = (0..data.len()).map(|_| "?").collect();
        let sql = format!(
            "INSERT INTO \"{}\" ({}) VALUES ({})",
            escape_ident(table),
            columns.join(", "),
            placeholders.join(", ")
        );

        let values: Vec<SqlValue> = data.values().map(json_to_sql).collect();
        c.execute(&sql, params_from_iter(values.iter()))
            .map_err(|e| KernelError::Other(format!("insert error: {e}")))?;

        let rowid = c.last_insert_rowid();
        Ok(json!({
            "message": "Record created successfully",
            "insertedId": rowid,
        }))
    }

    async fn db_read_records(&self, params: Value) -> abacus_types::Result<Value> {
        let table = get_str(&params, "table")?;
        let conditions = params.get("conditions").and_then(|v| v.as_object());
        let limit = params.get("limit").and_then(|v| v.as_u64());
        let offset = params.get("offset").and_then(|v| v.as_u64());
        let db_param = params.get("db").and_then(|v| v.as_str());

        let conn = self.get_conn(db_param).await?;
        let c = conn.lock().await;

        let mut sql = format!("SELECT * FROM \"{}\"", escape_ident(table));
        let mut sql_values: Vec<SqlValue> = vec![];

        if let Some(conds) = conditions {
            if !conds.is_empty() {
                let where_parts: Vec<String> = conds.keys()
                    .map(|k| format!("\"{}\" = ?", escape_ident(k)))
                    .collect();
                sql.push_str(" WHERE ");
                sql.push_str(&where_parts.join(" AND "));
                sql_values.extend(conds.values().map(json_to_sql));
            }
        }

        if let Some(lim) = limit {
            sql.push_str(&format!(" LIMIT {}", lim));
        }
        if let Some(off) = offset {
            sql.push_str(&format!(" OFFSET {}", off));
        }

        let mut stmt = c.prepare(&sql)
            .map_err(|e| KernelError::Other(format!("read error: {e}")))?;
        let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();

        let rows: Vec<Value> = stmt.query_map(params_from_iter(sql_values.iter()), |row| {
            let mut obj = Map::new();
            for (i, name) in col_names.iter().enumerate() {
                obj.insert(name.clone(), sql_val_to_json(row, i));
            }
            Ok(Value::Object(obj))
        }).map_err(|e| KernelError::Other(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(json!(rows))
    }

    async fn db_update_records(&self, params: Value) -> abacus_types::Result<Value> {
        let table = get_str(&params, "table")?;
        let data = get_object(&params, "data")?;
        let conditions = get_object(&params, "conditions")?;
        let db_param = params.get("db").and_then(|v| v.as_str());

        if conditions.is_empty() {
            return Err(KernelError::Other(
                "conditions 不能为空（防止全表更新）。如需更新全表请使用 db_query 执行原始 SQL".into()));
        }
        if data.is_empty() {
            return Err(KernelError::Other("data must not be empty".into()));
        }

        let conn = self.get_conn(db_param).await?;
        let c = conn.lock().await;

        let set_parts: Vec<String> = data.keys()
            .map(|k| format!("\"{}\" = ?", escape_ident(k)))
            .collect();
        let where_parts: Vec<String> = conditions.keys()
            .map(|k| format!("\"{}\" = ?", escape_ident(k)))
            .collect();

        let sql = format!(
            "UPDATE \"{}\" SET {} WHERE {}",
            escape_ident(table),
            set_parts.join(", "),
            where_parts.join(" AND ")
        );

        let mut values: Vec<SqlValue> = data.values().map(json_to_sql).collect();
        values.extend(conditions.values().map(json_to_sql));

        let affected = c.execute(&sql, params_from_iter(values.iter()))
            .map_err(|e| KernelError::Other(format!("update error: {e}")))?;

        Ok(json!({
            "message": "Records updated successfully",
            "rowsAffected": affected,
        }))
    }

    /// Wrapping-A：db_mutate 统一入口——按 op 字段路由到 create/update/delete
    ///
    /// ## 引用关系
    /// - 上游：ToolExecutor::execute "db_mutate" 路径
    /// - 下游：复用既有 db_create_record / db_update_records / db_delete_records（无业务逻辑改动）
    ///
    /// ## 合并动机
    /// 之前对 LLM 暴露 4 个 mutation 工具，schema 重复且 LLM 需要在 4 份近似 description 间挑选。
    /// 合并为 1 个 `db_mutate(op, ...)` 让 LLM 学一份 schema 即可 cover 全部写操作。
    /// `db.read_records` 保留独立（idempotent=true，进 dedup 路径需要语义一致）。
    async fn db_mutate(&self, params: Value) -> abacus_types::Result<Value> {
        let op = params.get("op").and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other(
                "db.mutate: 'op' field required (one of: create | update | delete)".into()
            ))?;
        match op {
            "create" => self.db_create_record(params).await,
            "update" => self.db_update_records(params).await,
            "delete" => self.db_delete_records(params).await,
            other => Err(KernelError::Other(format!(
                "db.mutate: unknown op '{}', expected one of: create | update | delete",
                other
            ))),
        }
    }

    async fn db_delete_records(&self, params: Value) -> abacus_types::Result<Value> {
        let table = get_str(&params, "table")?;
        let conditions = get_object(&params, "conditions")?;
        let db_param = params.get("db").and_then(|v| v.as_str());

        if conditions.is_empty() {
            return Err(KernelError::Other(
                "conditions 不能为空（防止全表删除）。如需清空表请使用 db_query 执行 DELETE FROM table".into()));
        }

        let conn = self.get_conn(db_param).await?;
        let c = conn.lock().await;

        let where_parts: Vec<String> = conditions.keys()
            .map(|k| format!("\"{}\" = ?", escape_ident(k)))
            .collect();

        let sql = format!(
            "DELETE FROM \"{}\" WHERE {}",
            escape_ident(table),
            where_parts.join(" AND ")
        );

        let values: Vec<SqlValue> = conditions.values().map(json_to_sql).collect();
        let affected = c.execute(&sql, params_from_iter(values.iter()))
            .map_err(|e| KernelError::Other(format!("delete error: {e}")))?;

        Ok(json!({
            "message": "Records deleted successfully",
            "rowsAffected": affected,
        }))
    }
}

#[async_trait]
impl ToolExecutor for DbToolExecutor {
    async fn execute(&self, tool_id: &ToolId, params: Value, _ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        // 单一命名约定：ToolId 即 schema.name（"db_query" 等下划线形态），与 LLM 协议合规。
        match tool_id.0.as_str() {
            "db_info" => self.db_info(params).await,
            "db_list_tables" => self.db_list_tables(params).await,
            "db_table_schema" => self.db_table_schema(params).await,
            "db_query" => self.db_query(params).await,
            // Wrapping-A：合并入口（对 LLM 暴露）
            "db_mutate" => self.db_mutate(params).await,
            "db_read_records" => self.db_read_records(params).await,
            // Backward-compat：旧 tool_id 在 executor 仍可路由——内部测试 / 既存集成脚本继续工作
            // schema 层已不再注册（LLM 不可见），但 ToolRegistry 若被外部直接调用仍 dispatch
            "db_create_record" => self.db_create_record(params).await,
            "db_update_records" => self.db_update_records(params).await,
            "db_delete_records" => self.db_delete_records(params).await,
            _ => Err(KernelError::Other(format!("unknown db tool: {}", tool_id.0))),
        }
    }
}

// ─── Schema definitions ─────────────────────────────────────────────────

fn db_schema(name: &str, desc: &str, props: Value, required: &[&str],
             confirm: bool, tokens: u32, latency: &str, risk: &str) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: desc.into(),
        parameters: json!({
            "type": "object",
            "properties": props,
            "required": required,
        }),
        returns: None,
        security: Some(ToolSecurity {
            allowed_paths: None,
            max_size_mb: None,
            confirm_required: confirm,
            needs_sandbox: false,
        }),
        cost: Some(ToolCost { tokens, latency: latency.into(), risk: risk.into() }),
        examples: Vec::new(),
        applicable_task_kinds: None,
        // Phase β-G: db.read/info/list_tables/schema 是 idempotent；query 看 SQL 但保守 false
        idempotent: matches!(name,
            "db_read_records" | "db_info" | "db_list_tables" | "db_table_schema"),
        // P0-C2: db.* schema 在运行时不变，参与 KV prefix cache
        schema_stable: true,
    }
}

pub fn schemas() -> Vec<ToolSchema> {
    let db_prop = json!({"type": "string", "description": "数据库文件路径(默认 ~/.abacus/memory.db)"});
    vec![
        db_schema("db_info", "获取数据库元信息（路径/大小/表数量）",
            json!({"db": db_prop.clone()}), &[], false, 16, "5ms", "low"),
        db_schema("db_list_tables", "列出数据库中的所有用户表",
            json!({"db": db_prop.clone()}), &[], false, 32, "5ms", "low"),
        db_schema("db_table_schema", "获取表的列结构信息",
            json!({"tableName": {"type": "string", "description": "表名"},
                   "db": db_prop.clone()}),
            &["tableName"], false, 32, "5ms", "low"),
        db_schema("db_query", "执行参数化 SQL 语句，返回所有行",
            json!({"sql": {"type": "string", "description": "SQL 语句"},
                   "values": {"type": "array", "description": "参数值列表(对应 ? 占位符)"},
                   "db": db_prop.clone()}),
            &["sql"], false, 64, "50ms", "medium"),
        db_schema("db_read_records", "按条件读取记录",
            json!({"table": {"type": "string", "description": "表名"},
                   "conditions": {"type": "object", "description": "等值过滤条件(AND)"},
                   "limit": {"type": "integer", "description": "最大行数"},
                   "offset": {"type": "integer", "description": "偏移量"},
                   "db": db_prop.clone()}),
            &["table"], false, 48, "10ms", "low"),
        // Wrapping-A：合并 create/update/delete → db.mutate(op, ...)
        // 1 个 schema 替代 3 个，约 -150 tokens；LLM 看一份 description 即知所有写操作。
        // 注意：op=update|delete 时 conditions 必填（业务校验在 db_mutate 委托的子方法内完成）
        db_schema("db_mutate", "数据库写操作统一入口：op=create(插入) | update(更新，conditions 必填) | delete(删除，conditions 必填)",
            json!({
                "op": {"type": "string", "enum": ["create", "update", "delete"], "description": "写操作类型"},
                "table": {"type": "string", "description": "目标表名"},
                "data": {"type": "object", "description": "create/update 时的列→值映射；delete 时忽略"},
                "conditions": {"type": "object", "description": "update/delete 必填的 WHERE 条件(防全表操作)；create 时忽略"},
                "db": db_prop.clone()
            }),
            &["op", "table"], true, 48, "10ms", "medium"),
    ]
}

// ─── Registration ───────────────────────────────────────────────────────

pub async fn register(registry: &ToolRegistry) {
    for s in schemas() {
        registry.register(ToolHandle {
            id: ToolId(s.name.clone()),
            schema: s,
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        }).await;
    }
}

pub async fn register_executors(registry: &ToolRegistry) {
    let executor = Arc::new(DbToolExecutor::new());
    for s in schemas() {
        registry.register_executor(ToolId(s.name.clone()), executor.clone()).await;
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn escape_ident(s: &str) -> String {
    s.replace('"', "\"\"")
}

fn get_str<'a>(v: &'a Value, key: &str) -> Result<&'a str, KernelError> {
    v.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| KernelError::Other(format!("missing required parameter: {key}")))
}

fn get_object<'a>(v: &'a Value, key: &str) -> Result<&'a Map<String, Value>, KernelError> {
    v.get(key)
        .and_then(|v| v.as_object())
        .ok_or_else(|| KernelError::Other(format!("missing required object parameter: {key}")))
}

/// JSON → SQLite 值转换
fn json_to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(if *b { 1 } else { 0 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                SqlValue::Real(f)
            } else {
                SqlValue::Text(n.to_string())
            }
        }
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Array(_) | Value::Object(_) => SqlValue::Text(v.to_string()),
    }
}

/// SQLite row → JSON 值转换
///
/// 使用 value_ref() 获取 SQLite 实际存储类型，避免 try-chain 的截断问题。
/// SQLite 动态类型系统：同一列不同行可能是不同类型。
fn sql_val_to_json(row: &rusqlite::Row, idx: usize) -> Value {
    use rusqlite::types::ValueRef;
    match row.get_ref(idx) {
        Ok(ValueRef::Null) => Value::Null,
        Ok(ValueRef::Integer(i)) => json!(i),
        Ok(ValueRef::Real(f)) => json!(f),
        Ok(ValueRef::Text(s)) => {
            // s 是 &[u8]，转为 UTF-8 字符串
            match std::str::from_utf8(s) {
                Ok(text) => json!(text),
                Err(_) => json!(format!("<binary {} bytes>", s.len())),
            }
        }
        Ok(ValueRef::Blob(b)) => json!(format!("<blob {} bytes>", b.len())),
        Err(_) => Value::Null,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_db_info_nonexistent() {
        let executor = DbToolExecutor::new();
        let result = executor.db_info(json!({"db": "/tmp/nonexistent_abacus_test.db"})).await.unwrap();
        assert_eq!(result["exists"], false);
        assert_eq!(result["tableCount"], 0);
    }

    #[tokio::test]
    async fn test_db_crud_lifecycle() {
        let executor = DbToolExecutor::new();
        let db_path = "/tmp/abacus_test_crud.db";

        // 清理
        let _ = std::fs::remove_file(db_path);

        // 创建表
        executor.db_query(json!({
            "sql": "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)",
            "db": db_path
        })).await.unwrap();

        // 插入
        let insert_result = executor.db_create_record(json!({
            "table": "users",
            "data": {"name": "Alice", "age": 30},
            "db": db_path
        })).await.unwrap();
        assert_eq!(insert_result["insertedId"], 1);

        // 读取
        let read_result = executor.db_read_records(json!({
            "table": "users",
            "conditions": {"name": "Alice"},
            "db": db_path
        })).await.unwrap();
        let rows = read_result.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["age"], 30);

        // 更新
        let update_result = executor.db_update_records(json!({
            "table": "users",
            "data": {"age": 31},
            "conditions": {"name": "Alice"},
            "db": db_path
        })).await.unwrap();
        assert_eq!(update_result["rowsAffected"], 1);

        // 删除
        let delete_result = executor.db_delete_records(json!({
            "table": "users",
            "conditions": {"name": "Alice"},
            "db": db_path
        })).await.unwrap();
        assert_eq!(delete_result["rowsAffected"], 1);

        // 验证空
        let empty = executor.db_read_records(json!({
            "table": "users",
            "db": db_path
        })).await.unwrap();
        assert_eq!(empty.as_array().unwrap().len(), 0);

        // 清理
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn test_db_list_tables() {
        let executor = DbToolExecutor::new();
        let db_path = "/tmp/abacus_test_tables.db";
        let _ = std::fs::remove_file(db_path);

        executor.db_query(json!({
            "sql": "CREATE TABLE t1 (id INTEGER); CREATE TABLE t2 (id INTEGER);",
            "db": db_path
        })).await.ok(); // Might fail on multi-statement, that's ok

        // Use individual creates
        executor.db_query(json!({"sql": "CREATE TABLE IF NOT EXISTS t1 (id INTEGER)", "db": db_path})).await.unwrap();
        executor.db_query(json!({"sql": "CREATE TABLE IF NOT EXISTS t2 (id INTEGER)", "db": db_path})).await.unwrap();

        let tables = executor.db_list_tables(json!({"db": db_path})).await.unwrap();
        let arr = tables.as_array().unwrap();
        assert!(arr.len() >= 2);

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn test_delete_requires_conditions() {
        let executor = DbToolExecutor::new();
        let result = executor.db_delete_records(json!({
            "table": "test",
            "conditions": {}
        })).await;
        assert!(result.is_err());
    }

    // ─── Wrapping-A：db.mutate 入口路由测试 ─────────────────────────────

    #[tokio::test]
    async fn test_db_mutate_routes_to_create_update_delete() {
        let executor = DbToolExecutor::new();
        let db_path = "/tmp/abacus_test_mutate.db";
        let _ = std::fs::remove_file(db_path);

        executor.db_query(json!({
            "sql": "CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY, name TEXT, n INTEGER)",
            "db": db_path
        })).await.unwrap();

        // create via mutate
        let r = executor.db_mutate(json!({
            "op": "create",
            "table": "items",
            "data": {"name": "x", "n": 1},
            "db": db_path
        })).await.unwrap();
        assert_eq!(r["insertedId"], 1);

        // update via mutate
        let r = executor.db_mutate(json!({
            "op": "update",
            "table": "items",
            "data": {"n": 2},
            "conditions": {"name": "x"},
            "db": db_path
        })).await.unwrap();
        assert_eq!(r["rowsAffected"], 1);

        // delete via mutate
        let r = executor.db_mutate(json!({
            "op": "delete",
            "table": "items",
            "conditions": {"name": "x"},
            "db": db_path
        })).await.unwrap();
        assert_eq!(r["rowsAffected"], 1);

        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn test_db_mutate_rejects_unknown_op() {
        let executor = DbToolExecutor::new();
        let r = executor.db_mutate(json!({
            "op": "drop_table",
            "table": "x"
        })).await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("unknown op"));
    }

    #[tokio::test]
    async fn test_db_mutate_requires_op_field() {
        let executor = DbToolExecutor::new();
        let r = executor.db_mutate(json!({"table": "x"})).await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("'op' field required"));
    }

    /// 验证 schemas() 真的少了 3 个 mutation——schema 表面契约
    #[test]
    fn test_db_schemas_collapsed_mutations() {
        let names: Vec<String> = schemas().iter().map(|s| s.name.clone()).collect();
        assert!(names.contains(&"db_mutate".to_string()));
        assert!(names.contains(&"db_read_records".to_string()));
        // 旧 mutation schema 已不再注册（LLM 不可见）
        assert!(!names.contains(&"db_create_record".to_string()));
        assert!(!names.contains(&"db_update_records".to_string()));
        assert!(!names.contains(&"db_delete_records".to_string()));
        assert_eq!(names.len(), 6, "8 - 3 + 1 = 6 个 schema");
    }
}
