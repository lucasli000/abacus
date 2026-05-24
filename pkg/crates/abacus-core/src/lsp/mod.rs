//! LSP（Language Server Protocol）支持模块
//!
//! ## 架构
//! ```text
//! LspManager（全局单例，CoreLoop 持有）
//!   ├── LspClient(rust-analyzer, /workspace)
//!   ├── LspClient(typescript-language-server, /workspace)
//!   └── ...
//! ```
//!
//! ## 语言服务器自动检测
//! 根据文件扩展名选择合适的语言服务器。
//! 服务器必须已安装在 PATH 中；不存在时 tool 返回明确错误。
//!
//! ## 引用关系
//! - `LspManager` 被 `CoreLoop` 以 `Arc<LspManager>` 持有
//! - `LspToolExecutor` 调用 `LspManager` 执行操作
//! - 被 `tool/builtin/lsp.rs` 注册为 LLM 可调用工具

pub mod client;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::RwLock;

use client::{FileStateUpdate, LspClient, path_to_uri};

// ─── 语言服务器检测 ─────────────────────────────────────────────────────────

/// 根据文件路径推断语言标识
pub fn detect_language(file_path: &str) -> Option<&'static str> {
    let ext = Path::new(file_path).extension()?.to_str()?;
    match ext {
        "rs"                  => Some("rust"),
        "ts" | "tsx"          => Some("typescript"),
        "js" | "jsx" | "mjs"  => Some("javascript"),
        "py"                  => Some("python"),
        "go"                  => Some("go"),
        "java"                => Some("java"),
        "cpp" | "cc" | "cxx"  => Some("cpp"),
        "c"                   => Some("c"),
        "h" | "hpp"           => Some("cpp"),
        "lua"                 => Some("lua"),
        "rb"                  => Some("ruby"),
        "cs"                  => Some("csharp"),
        "kt"                  => Some("kotlin"),
        "swift"               => Some("swift"),
        _                     => None,
    }
}

/// 获取语言对应的默认服务器命令和参数
fn server_config(language: &str) -> Option<(&'static str, Vec<&'static str>)> {
    match language {
        "rust"       => Some(("rust-analyzer", vec![])),
        "typescript" => Some(("typescript-language-server", vec!["--stdio"])),
        "javascript" => Some(("typescript-language-server", vec!["--stdio"])),
        "python"     => Some(("pyright-langserver", vec!["--stdio"])),
        "go"         => Some(("gopls", vec![])),
        "java"       => Some(("jdtls", vec![])),
        "cpp" | "c"  => Some(("clangd", vec![])),
        "lua"        => Some(("lua-language-server", vec![])),
        "ruby"       => Some(("solargraph", vec!["stdio"])),
        "csharp"     => Some(("OmniSharp", vec!["-lsp"])),
        _            => None,
    }
}

// ─── LspManager ────────────────────────────────────────────────────────────

/// LSP 客户端管理器
///
/// 维护活跃的语言服务器连接池。
/// key = (workspace_root, language)
pub struct LspManager {
    clients: RwLock<HashMap<String, Arc<LspClient>>>,
}

impl LspManager {
    pub fn new() -> Self {
        Self { clients: RwLock::new(HashMap::new()) }
    }

    /// 获取或启动指定语言的客户端
    ///
    /// ## 并发安全（乐观并发）
    /// 1. 读锁快路径：已存在直接返回
    /// 2. 无锁启动：LspClient::start() 耗时，释放锁后执行
    /// 3. `entry().or_insert()`：写锁原子插入；若竞争者先插入则丢弃本次启动
    ///    的客户端（其 Drop 会发送 shutdown/exit 给语言服务器进程）
    async fn get_or_start(
        &self,
        language: &str,
        workspace_root: &str,
    ) -> Result<Arc<LspClient>, String> {
        let key = format!("{workspace_root}:{language}");

        // 快路径：读锁检查
        {
            let clients = self.clients.read().await;
            if let Some(c) = clients.get(&key) {
                return Ok(c.clone());
            }
        }

        // 无锁启动（耗时操作不持锁）
        let (cmd, args) = server_config(language)
            .ok_or_else(|| format!("no LSP server configured for language '{language}'"))?;
        let new_client = Arc::new(
            LspClient::start(cmd, &args, workspace_root).await
                .map_err(|e| format!("failed to start {cmd}: {e}"))?
        );

        // 乐观插入：若竞争者已插入则返回已有客户端，本次启动的客户端 Drop 触发 shutdown
        let inserted = {
            let mut clients = self.clients.write().await;
            clients.entry(key).or_insert_with(|| new_client.clone()).clone()
        };
        Ok(inserted)
    }

    /// 关闭并移除指定语言的客户端
    pub async fn shutdown(&self, language: &str, workspace_root: &str) {
        let key = format!("{workspace_root}:{language}");
        self.clients.write().await.remove(&key);
    }

    // ─── 核心 LSP 操作 ─────────────────────────────────────────────────────

    /// 跳转到定义
    ///
    /// 返回 `[{file, line, character}]` 列表（可能多个定义）
    pub async fn goto_definition(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
        workspace_root: &str,
    ) -> Result<Value, String> {
        let lang = detect_language(file_path)
            .ok_or_else(|| format!("unsupported file type: {file_path}"))?;
        let client = self.get_or_start(lang, workspace_root).await?;

        // 先通知 server 打开文件
        self.did_open_or_change_file(&client, file_path).await?;

        let params = text_document_position(file_path, line, character);
        let result = client.request("textDocument/definition", params).await?;
        Ok(format_locations(result))
    }

    /// 查找所有引用
    pub async fn find_references(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
        workspace_root: &str,
    ) -> Result<Value, String> {
        let lang = detect_language(file_path)
            .ok_or_else(|| format!("unsupported file type: {file_path}"))?;
        let client = self.get_or_start(lang, workspace_root).await?;
        self.did_open_or_change_file(&client, file_path).await?;

        let mut params = text_document_position(file_path, line, character);
        params["context"] = json!({ "includeDeclaration": include_declaration });
        let result = client.request("textDocument/references", params).await?;
        Ok(format_locations(result))
    }

    /// 悬停信息（类型、文档注释）
    pub async fn hover(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
        workspace_root: &str,
    ) -> Result<Value, String> {
        let lang = detect_language(file_path)
            .ok_or_else(|| format!("unsupported file type: {file_path}"))?;
        let client = self.get_or_start(lang, workspace_root).await?;
        self.did_open_or_change_file(&client, file_path).await?;

        let params = text_document_position(file_path, line, character);
        let result = client.request("textDocument/hover", params).await?;
        Ok(format_hover(result))
    }

    /// 文档所有符号（函数、结构体、变量等）
    pub async fn document_symbol(
        &self,
        file_path: &str,
        workspace_root: &str,
    ) -> Result<Value, String> {
        let lang = detect_language(file_path)
            .ok_or_else(|| format!("unsupported file type: {file_path}"))?;
        let client = self.get_or_start(lang, workspace_root).await?;
        self.did_open_or_change_file(&client, file_path).await?;

        let params = json!({ "textDocument": { "uri": path_to_uri(file_path) } });
        let result = client.request("textDocument/documentSymbol", params).await?;
        Ok(format_symbols(result, file_path))
    }

    /// 工作区符号搜索
    pub async fn workspace_symbol(
        &self,
        query: &str,
        workspace_root: &str,
        language_hint: Option<&str>,
    ) -> Result<Value, String> {
        // 工作区符号需要一个已启动的客户端；使用语言提示或第一个可用的
        let lang = language_hint.unwrap_or("rust");
        let client = self.get_or_start(lang, workspace_root).await?;

        let params = json!({ "query": query });
        let result = client.request("workspace/symbol", params).await?;
        Ok(format_workspace_symbols(result))
    }

    /// 获取文件诊断（错误/警告）
    ///
    /// ## 优先级
    /// 1. 推送诊断缓存（`textDocument/publishDiagnostics` 通知）— 实时且全面
    /// 2. Pull 模式（LSP 3.17+ `textDocument/diagnostic`）— 按需拉取
    /// 3. 两者均无结果时返回空数组
    pub async fn diagnostics(
        &self,
        file_path: &str,
        workspace_root: &str,
    ) -> Result<Value, String> {
        let lang = detect_language(file_path)
            .ok_or_else(|| format!("unsupported file type: {file_path}"))?;
        let client = self.get_or_start(lang, workspace_root).await?;
        self.did_open_or_change_file(&client, file_path).await?;

        // 优先返回服务器推送的诊断（实时性更好）
        let push = client.get_push_diagnostics(file_path).await;
        if !push.is_empty() {
            return Ok(json!({
                "file": file_path,
                "diagnostics": push,
                "source": "push"
            }));
        }

        // 回退：Pull 模式（LSP 3.17+ textDocument/diagnostic）
        let params = json!({ "textDocument": { "uri": path_to_uri(file_path) } });
        match client.request("textDocument/diagnostic", params).await {
            Ok(result) => Ok(format_diagnostics(result, file_path)),
            Err(_) => {
                // 服务器不支持 pull，也尚未推送 — 返回空
                Ok(json!({ "file": file_path, "diagnostics": [], "source": "none" }))
            }
        }
    }

    /// 跳转到接口实现
    ///
    /// 返回 `[{file, line, character}]` 列表（trait/接口的所有实现）
    pub async fn goto_implementation(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
        workspace_root: &str,
    ) -> Result<Value, String> {
        let lang = detect_language(file_path)
            .ok_or_else(|| format!("unsupported file type: {file_path}"))?;
        let client = self.get_or_start(lang, workspace_root).await?;
        self.did_open_or_change_file(&client, file_path).await?;
        let params = text_document_position(file_path, line, character);
        let result = client.request("textDocument/implementation", params).await?;
        Ok(format_locations(result))
    }

    /// 调用层次 — 谁调用了此函数（上游调用者）
    ///
    /// ## 协议
    /// LSP call hierarchy 分两步：
    /// 1. `textDocument/prepareCallHierarchy` 获取光标位置的 CallHierarchyItem
    /// 2. `callHierarchy/incomingCalls` 查找调用者
    pub async fn call_hierarchy_incoming(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
        workspace_root: &str,
    ) -> Result<Value, String> {
        let lang = detect_language(file_path)
            .ok_or_else(|| format!("unsupported file type: {file_path}"))?;
        let client = self.get_or_start(lang, workspace_root).await?;
        self.did_open_or_change_file(&client, file_path).await?;
        let prep = text_document_position(file_path, line, character);
        let items = client.request("textDocument/prepareCallHierarchy", prep).await?;
        let item = match items {
            Value::Array(mut arr) if !arr.is_empty() => arr.remove(0),
            _ => return Ok(json!([])),
        };
        let result = client.request("callHierarchy/incomingCalls", json!({ "item": item })).await?;
        Ok(format_call_hierarchy(result, "from"))
    }

    /// 调用层次 — 此函数调用了哪些函数（下游被调用者）
    ///
    /// ## 协议
    /// 1. `textDocument/prepareCallHierarchy` 获取 CallHierarchyItem
    /// 2. `callHierarchy/outgoingCalls` 查找被调用函数
    pub async fn call_hierarchy_outgoing(
        &self,
        file_path: &str,
        line: u32,
        character: u32,
        workspace_root: &str,
    ) -> Result<Value, String> {
        let lang = detect_language(file_path)
            .ok_or_else(|| format!("unsupported file type: {file_path}"))?;
        let client = self.get_or_start(lang, workspace_root).await?;
        self.did_open_or_change_file(&client, file_path).await?;
        let prep = text_document_position(file_path, line, character);
        let items = client.request("textDocument/prepareCallHierarchy", prep).await?;
        let item = match items {
            Value::Array(mut arr) if !arr.is_empty() => arr.remove(0),
            _ => return Ok(json!([])),
        };
        let result = client.request("callHierarchy/outgoingCalls", json!({ "item": item })).await?;
        Ok(format_call_hierarchy(result, "to"))
    }

    /// 通知服务器文件内容（支持增量同步）
    ///
    /// ## 同步策略（由 `check_file_state` 决定）
    /// - `FirstOpen`：发 `textDocument/didOpen`（全量）
    /// - `Changed`：发 `textDocument/didChange`（全文件 full document sync）
    /// - `Unchanged`：跳过通知
    ///
    /// ## 全文件同步说明
    /// LSP 支持差量和全文件两种同步方式。这里选择全文件同步
    /// （`TextDocumentSyncKind::Full`）——实现简单且对大多数语言服务器兼容。
    async fn did_open_or_change_file(&self, client: &LspClient, file_path: &str) -> Result<(), String> {
        let content = tokio::fs::read_to_string(file_path).await
            .unwrap_or_default();
        let lang_id = detect_language(file_path).unwrap_or("plaintext");
        let uri = path_to_uri(file_path);

        match client.check_file_state(file_path, &content).await {
            FileStateUpdate::Unchanged => Ok(()), // 内容未变，跳过

            FileStateUpdate::FirstOpen => {
                let params = json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": lang_id,
                        "version": 1,
                        "text": content,
                    }
                });
                if client.notify("textDocument/didOpen", params).await.is_err() {
                    // 发送失败 — 回滚状态保证下次重试
                    client.mark_closed(file_path).await;
                }
                Ok(())
            }

            FileStateUpdate::Changed { version } => {
                // 全文件同步：全量发送新内容，配单调递增的版本号
                let params = json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": content }]
                });
                // didChange 失败不必回滚（版本号已更新，下次访问将继续尝试发 didChange）
                let _ = client.notify("textDocument/didChange", params).await;
                Ok(())
            }
        }
    }
}

impl Default for LspManager {
    fn default() -> Self { Self::new() }
}

// ─── 辅助函数 ───────────────────────────────────────────────────────────────

fn text_document_position(file_path: &str, line: u32, character: u32) -> Value {
    json!({
        "textDocument": { "uri": path_to_uri(file_path) },
        "position": { "line": line, "character": character }
    })
}

/// 将 LSP Location/LocationLink 数组格式化为紧凑 JSON
fn format_locations(result: Value) -> Value {
    let locs = match result {
        Value::Array(arr) => arr,
        Value::Object(_) => vec![result], // 单个 Location
        _ => return json!([]),
    };
    let items: Vec<Value> = locs.iter().filter_map(|loc| {
        let uri = loc.get("uri")?.as_str()?;
        let range = loc.get("range")?;
        let start = range.get("start")?;
        Some(json!({
            "file": client::uri_to_path(uri),
            "line": start.get("line")?.as_u64()? + 1,   // 1-based for display
            "character": start.get("character")?.as_u64()? + 1,
        }))
    }).collect();
    json!(items)
}

fn format_hover(result: Value) -> Value {
    if result.is_null() { return json!({ "content": null }); }
    let content = result.get("contents")
        .and_then(|c| match c {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("value").and_then(|v| v.as_str()).map(|s| s.to_string()),
            Value::Array(arr) => {
                let parts: Vec<&str> = arr.iter()
                    .filter_map(|item| match item {
                        Value::String(s) => Some(s.as_str()),
                        Value::Object(o) => o.get("value").and_then(|v| v.as_str()),
                        _ => None,
                    }).collect();
                Some(parts.join("\n"))
            }
            _ => None,
        })
        .unwrap_or_else(|| "(no hover info)".to_string());
    json!({ "content": content })
}

fn format_symbols(result: Value, file_path: &str) -> Value {
    let syms = match result { Value::Array(a) => a, _ => return json!([]) };
    let items: Vec<Value> = syms.iter().filter_map(|s| {
        let name = s.get("name")?.as_str()?;
        let kind = symbol_kind_name(s.get("kind")?.as_u64().unwrap_or(0));
        // DocumentSymbol has "range"; SymbolInformation has "location"
        let (line, file) = if let Some(loc) = s.get("location") {
            let uri = loc.get("uri")?.as_str()?;
            let l = loc.get("range")?.get("start")?.get("line")?.as_u64()? + 1;
            (l, client::uri_to_path(uri))
        } else {
            let l = s.get("range")?.get("start")?.get("line")?.as_u64()? + 1;
            (l, file_path.to_string())
        };
        Some(json!({ "name": name, "kind": kind, "file": file, "line": line }))
    }).collect();
    json!(items)
}

fn format_workspace_symbols(result: Value) -> Value {
    let syms = match result { Value::Array(a) => a, _ => return json!([]) };
    let items: Vec<Value> = syms.iter().filter_map(|s| {
        let name = s.get("name")?.as_str()?;
        let kind = symbol_kind_name(s.get("kind")?.as_u64().unwrap_or(0));
        let loc = s.get("location")?;
        let uri = loc.get("uri")?.as_str()?;
        let line = loc.get("range")?.get("start")?.get("line")?.as_u64()? + 1;
        Some(json!({ "name": name, "kind": kind, "file": client::uri_to_path(uri), "line": line }))
    }).collect();
    json!(items)
}

fn format_diagnostics(result: Value, file_path: &str) -> Value {
    let items = result.get("items").cloned().unwrap_or(Value::Array(vec![]));
    let diags = match items { Value::Array(a) => a, _ => return json!([]) };
    let formatted: Vec<Value> = diags.iter().filter_map(|d| {
        let message = d.get("message")?.as_str()?;
        let severity = match d.get("severity").and_then(|s| s.as_u64()) {
            Some(1) => "error", Some(2) => "warning",
            Some(3) => "info",  _ => "hint",
        };
        let line = d.get("range")?.get("start")?.get("line")?.as_u64()? + 1;
        Some(json!({ "file": file_path, "line": line, "severity": severity, "message": message }))
    }).collect();
    json!(formatted)
}

/// 将 CallHierarchyIncomingCall / OutgoingCall 数组格式化
///
/// - `direction = "from"` → incomingCalls（上游调用者）
/// - `direction = "to"` → outgoingCalls（下游被调用者）
fn format_call_hierarchy(result: Value, direction: &str) -> Value {
    let items = match result { Value::Array(a) => a, _ => return json!([]) };
    let formatted: Vec<Value> = items.iter().filter_map(|item| {
        let call_item = item.get(direction)?;
        let name = call_item.get("name")?.as_str()?;
        let uri  = call_item.get("uri")?.as_str()?;
        let start = call_item.get("range")?.get("start")?;
        let line  = start.get("line")?.as_u64()? + 1;
        Some(json!({
            "name": name,
            "file": client::uri_to_path(uri),
            "line": line,
        }))
    }).collect();
    json!(formatted)
}

fn symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "File", 2 => "Module", 3 => "Namespace", 4 => "Package",
        5 => "Class", 6 => "Method", 7 => "Property", 8 => "Field",
        9 => "Constructor", 10 => "Enum", 11 => "Interface", 12 => "Function",
        13 => "Variable", 14 => "Constant", 15 => "String", 16 => "Number",
        17 => "Boolean", 18 => "Array", 19 => "Object", 20 => "Key",
        21 => "Null", 22 => "EnumMember", 23 => "Struct", 24 => "Event",
        25 => "Operator", 26 => "TypeParameter", _ => "Unknown",
    }
}
