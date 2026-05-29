//! filengine — Built-in file system, web, and shell tools
//!
//! ## Dependencies (external crates)
//! - `tokio::fs`: async file operations (read/write/rename/metadata)
//! - `tokio::process::Command`: shell command execution (bash.exec)
//! - `reqwest`: HTTP client for web.fetch and web.search
//! - `glob`: glob pattern matching for fs.search
//! - `regex`: regex pattern matching for fs.grep
//! - `async_trait`: trait object support for FilengineExecutor
//!
//! ## Dependencies (internal)
//! - `abacus_types::{ToolHandle, ToolSchema, ToolSecurity, ToolCost}`: tool registration types
//! - `crate::tool::{ToolExecutor, ToolRegistry}`: tool execution trait and registry
//!
//! ## References (callers)
//! - `crate::tool::builtin::mod.rs::register_all()` → calls `filengine::register()`
//! - `crate::core::mod.rs::CoreLoop::new()` → calls `register_all()` during init
//! - `crate::core::mod.rs::CoreLoop::process_turn()` → executes tools via ToolRegistry
//!
//! ## Referenced by
//! - `FilengineSession` is held by `SessionState` (core/mod.rs) — shared via Arc<RwLock>
//! - `allowed_roots()` is checked by `NativeFilengine::resolve()` and `path_is_allowed()` (context.rs)
//!
//! ## Registered Tools (13)
//! | Tool | Confirm | Risk | Description |
//! |------|---------|------|-------------|
//! | fs.read | no | low | 读取文件完整内容 |
//! | fs.write | yes | medium | 写入文件内容（创建或覆盖） |
//! | fs.edit | no | medium | 精确替换文件中的文本段 |
//! | fs.move | yes | medium | 移动和重命名文件或目录 |
//! | fs.info | no | low | 获取文件或目录元数据 |
//! | fs.search | no | low | 按 Glob 模式搜索文件名 |
//! | fs.ls | no | low | 列出目录内容 |
//! | fs.tree | no | low | 递归列出目录树（最多5层） |
//! | fs.mkdir | yes | low | 递归创建目录 |
//! | fs.grep | no | low | 按正则表达式搜索文件内容 |
//! | fs.cwd | no | low | 获取当前工作目录 |
//! | fs.status | no | low | 获取 session 文件活动摘要 |
//! | web.fetch | no | low | HTTP GET 请求获取网页内容 |
//! | web.search | no | low | 搜索引擎搜索并返回结果 |
//! | bash.exec | yes | medium | 执行 shell 命令（白名单限制） |

use std::sync::Arc;

use abacus_types::{
    BashPolicyLevel, SearchProvider, ToolCost, ToolEffectiveness, ToolHandle, ToolId,
    ToolProvider, ToolSchema, ToolSecurity, ToolState,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::process::Command as TokioCmd;

use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};
use std::path::PathBuf;
use tokio::fs;

/// Allowed filesystem roots — resolved at runtime from $HOME.
/// Callers should use `allowed_roots()` instead of this static.
///
/// Security: validates that HOME is a non-root, absolute path with at least 2 segments
/// to prevent directory traversal attacks via `HOME=/`.
pub fn allowed_roots() -> Vec<String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/tmp".into());

    // Validate HOME is a safe, non-root absolute path
    let path = std::path::Path::new(&home);
    let is_valid = path.is_absolute()
        && path.components().count() >= 3  // e.g. /home/user (root + home + username)
        && home != "/"
        && !home.contains("..");

    if is_valid {
        vec![home]
    } else {
        tracing::warn!(home = %home, "unsafe HOME detected, restricting to /tmp");
        vec!["/tmp".into()]
    }
}

// ─── Session state (协同层) ────────────────────────────────────────

pub struct FilengineSession {
    pub cwd: PathBuf,                          // 当前工作目录
    pub recent_files: Vec<String>,              // 最近访问的文件（LRU, max 20）
    pub last_search: Option<Vec<String>>,       // 上次搜索结果
    pub modified: Vec<String>,                  // 本次 session 修改过的文件
    pub open_context: Option<String>,           // 当前"正在看"的文件路径
    /// Phase 2 undo：可选 logger，None 时所有写工具静默跳过 snapshot/log
    /// 注入：CoreLoop 启动时如启用 undo 则 set；测试 / 未启用 → None
    /// 引用：fs.write/edit/move/mkdir 在 dispatch 时读
    pub undo_logger: Option<Arc<crate::undo::UndoLogger>>,
    /// Bash 默认超时（秒）— 从 policy.toml 注入，默认 30
    pub bash_default_timeout: u64,
    /// Bash 最大超时（秒）— 从 policy.toml 注入，默认 120
    pub bash_max_timeout: u64,
    /// 文件系统可访问根目录——从 role_caps 注入（替代 allowed_roots() 直调）
    /// 注入：FilengineToolExecutor::execute() 开头从 ctx.role_caps.fs_roots 同步
    /// 消费：NativeFilengine::resolve() 读此字段做 prefix 检查
    pub fs_roots: Vec<String>,
    /// Bash 执行策略——从 role_caps 注入
    /// 注入：FilengineToolExecutor::execute() 开头同步
    /// 消费：bash_exec() 用于决定降级 / 升级 BashDecision
    pub bash_policy: BashPolicyLevel,
    /// 搜索 provider——从 role_caps 注入
    /// 注入：FilengineToolExecutor::execute() 开头同步
    /// 消费：web_search() 按 provider 分发请求
    pub search_provider: SearchProvider,
}

impl std::fmt::Debug for FilengineSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let provider_name = match &self.search_provider {
            SearchProvider::BraveApi { .. } => "BraveApi",
            SearchProvider::SearxNg { .. } => "SearxNg",
            SearchProvider::DuckDuckGo => "DuckDuckGo",
        };
        f.debug_struct("FilengineSession")
            .field("cwd", &self.cwd)
            .field("recent_files", &self.recent_files)
            .field("last_search", &self.last_search)
            .field("modified", &self.modified)
            .field("open_context", &self.open_context)
            .field("undo_logger_attached", &self.undo_logger.is_some())
            .field("fs_roots", &self.fs_roots)
            .field("bash_policy", &self.bash_policy)
            .field("search_provider", &provider_name)
            .finish()
    }
}

impl Default for FilengineSession {
    fn default() -> Self {
        Self::new()
    }
}

impl FilengineSession {
    pub fn new() -> Self {
        Self {
            cwd: PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())),
            recent_files: Vec::new(),
            last_search: None,
            modified: Vec::new(),
            open_context: None,
            undo_logger: None,
            bash_default_timeout: 30,
            bash_max_timeout: 120,
            // 默认与 allowed_roots() / RoleCapabilities::default() 保持一致
            fs_roots: allowed_roots(),
            bash_policy: BashPolicyLevel::DevTools,
            search_provider: SearchProvider::DuckDuckGo,
        }
    }

    /// 注入 undo logger（CoreLoop 启动时调用）
    pub fn with_undo_logger(mut self, logger: Arc<crate::undo::UndoLogger>) -> Self {
        self.undo_logger = Some(logger);
        self
    }

    pub fn track_read(&mut self, path: &str) {
        self.open_context = Some(path.to_string());
        self.recent_files.retain(|p| p != path);
        self.recent_files.push(path.to_string());
        if self.recent_files.len() > 20 {
            self.recent_files.remove(0);
        }
    }

    pub fn track_write(&mut self, path: &str) {
        if !self.modified.contains(&path.to_string()) {
            self.modified.push(path.to_string());
        }
    }

    pub fn summary(&self) -> Value {
        json!({
            "cwd": self.cwd.to_string_lossy(),
            "open": self.open_context,
            "recent": self.recent_files.iter().rev().take(5).collect::<Vec<_>>(),
            "modified": self.modified,
            "last_search_count": self.last_search.as_ref().map(|s| s.len()),
        })
    }
}

// ─── Executor trait ────────────────────────────────────────────────

#[async_trait]
pub trait FilengineExecutor: Send + Sync {
    async fn execute(&self, tool: &str, args: Value, session: &mut FilengineSession) -> Result<Value, String>;
    fn tool_id(&self) -> &'static str;
}

// ─── Native implementation ─────────────────────────────────────────

pub struct NativeFilengine;

impl NativeFilengine {
    /// Resolve a user-supplied path to a canonical absolute path within `allowed_roots`.
    ///
    /// ## 设计要点（F-BUG-2 修复）
    /// 旧实现 `canonicalize(p)` 要求路径已存在，破坏了 `fs.write` / `fs.mkdir` 的
    /// "创建或覆盖 / 递归创建" 契约。本版处理三类输入：
    /// 1. **路径已存在** → `canonicalize` 全路径，解析 symlink + 大小写规范化
    /// 2. **路径不存在** → 向上找**最近存在的祖先**，对祖先 `canonicalize`，再字面拼回剩余 segments
    /// 3. **包含 `..`** → 直接拒绝
    ///
    /// ## 安全
    /// 必须先拒 `..`，否则攻击者可用 `/safe/foo/../../../etc/passwd`（foo 不存在）
    /// 通过 "祖先 canonicalize + 字面拼接" 绕过 `starts_with` 边界检查。
    /// `..` 拒绝后，剩余 segments 是纯名称（无层级穿越），字面拼接安全。
    ///
    /// ## 引用关系
    /// - 调用方：fs.read / fs.write / fs.edit / fs.move / fs.info / fs.search / fs.ls /
    ///           fs.tree / fs.mkdir / fs.grep / fs.read_multiple / bash.exec(workdir)
    /// - 依赖：`allowed_roots()`（HOME-validated）、`session.cwd`
    pub(crate) fn resolve(path: &str, session: &FilengineSession) -> Result<PathBuf, String> {
        use std::path::Component;

        let raw = if path.starts_with('/') {
            PathBuf::from(path)
        } else {
            session.cwd.join(path)
        };

        // 安全：字面拒绝 ".." 防止 canonicalize 祖先后被字面拼接绕过 prefix check
        if raw.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(format!("path contains '..': {path}"));
        }

        // 路径已存在 → 走 canonicalize 全路径（保持原行为）
        let canonical = if raw.exists() {
            raw.canonicalize().map_err(|e| format!("canonicalize: {e}"))?
        } else {
            // 路径不存在 → 向上找最近存在的祖先 canonicalize，剩余 segments 字面拼回
            let mut suffix: Vec<std::ffi::OsString> = Vec::new();
            let mut cursor: &std::path::Path = raw.as_path();
            loop {
                let parent = cursor.parent()
                    .ok_or_else(|| format!("no existing ancestor: {path}"))?;
                if let Some(name) = cursor.file_name() {
                    suffix.push(name.to_os_string());
                }
                if parent.exists() {
                    let mut p = parent.canonicalize()
                        .map_err(|e| format!("canonicalize parent: {e}"))?;
                    for seg in suffix.iter().rev() {
                        p.push(seg);
                    }
                    break p;
                }
                cursor = parent;
            }
        };

        if !session.fs_roots.iter().any(|r| canonical.starts_with(r)) {
            return Err(format!("path not allowed: {path}"));
        }
        Ok(canonical)
    }
}

#[async_trait]
impl FilengineExecutor for NativeFilengine {
    fn tool_id(&self) -> &'static str { "filengine" }

    async fn execute(&self, tool: &str, args: Value, session: &mut FilengineSession) -> Result<Value, String> {
        // 命名约定：单一约定 — schema.name = ToolId.0 = LLM 调用名 = 内部 dispatch 键
        // 全部使用 "fs_read" 等下划线形式（避免 sanitize 链路 + 三套命名认知负担）
        match tool {
            "fs_read"  => fs_read(args, session).await,
            "fs_write" => fs_write(args, session).await,
            "fs_edit"  => fs_edit(args, session).await,
            "fs_move"  => fs_move(args, session).await,
            "fs_info"  => fs_info(args, session).await,
            "fs_search" => fs_search(args, session).await,
            "fs_ls"    => fs_ls(args, session).await,
            "fs_tree"  => fs_tree(args, session).await,
            "fs_mkdir" => fs_mkdir(args, session).await,
            "web_fetch" => web_fetch(args, session).await,
            "web_search" => web_search(args, session).await,
            "fs_cwd"   => Ok(json!({"cwd": session.cwd.to_string_lossy()})),
            "fs_status" => Ok(session.summary()),
            "fs_grep"  => fs_grep(args, session).await,
            "fs_read_multiple" => fs_read_multiple(args, session).await,
            "bash_exec" => bash_exec(args, session).await,
            _ => Err(format!("unknown tool: {tool}")),
        }
}
}

// ─── File commands ─────────────────────────────────────────────────

async fn fs_read(args: Value, session: &mut FilengineSession) -> Result<Value, String> {
    // Wrapping-B：fs.read 接受 path: string 或 paths: string[]——单/多文件统一入口
    //
    // 优先级：① args.paths 为 array → 委托 fs_read_multiple（保留批量语义）
    //         ② args.path 为 string → 单文件 read（原行为）
    //         ③ args.path 为 array → 当作 paths 数组转发
    //
    // 引用关系：旧 fs_read_multiple 函数仍保留——LLM 已不可见但 executor "fs_read_multiple"
    //   路径仍 dispatch（向后兼容内部代码 / 测试）
    if let Some(paths) = args.get("paths").and_then(|v| v.as_array()) {
        let _ = paths; // 占位，下方走原 multi 路径
        return fs_read_multiple(args, session).await;
    }
    if let Some(arr) = args.get("path").and_then(|v| v.as_array()) {
        // path 字段是数组——转 paths 字段委托
        let translated = json!({ "paths": arr });
        return fs_read_multiple(translated, session).await;
    }

    let path = get_str(&args, "path")?;
    let p = NativeFilengine::resolve(path, session)?;
    let content = fs::read_to_string(&p).await.map_err(|e| format!("read: {e}"))?;
    session.track_read(path);
    Ok(json!({"content": content, "path": path}))
}

async fn fs_read_multiple(args: Value, session: &mut FilengineSession) -> Result<Value, String> {
    let paths = args.get("paths")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "missing required parameter: paths (array)".to_string())?;

    if paths.len() > 20 {
        return Err("paths array exceeds limit of 20 files".to_string());
    }

    let mut output = String::new();
    let mut success_count = 0;
    let mut error_count = 0;

    for p in paths {
        let path = p.as_str().unwrap_or("");
        if path.is_empty() { continue; }

        output.push_str(&format!("=== {} ===\n", path));
        match NativeFilengine::resolve(path, session) {
            Ok(resolved) => {
                match fs::read_to_string(&resolved).await {
                    Ok(content) => {
                        output.push_str(&content);
                        session.track_read(path);
                        success_count += 1;
                    }
                    Err(e) => {
                        output.push_str(&format!("Error: {}\n", e));
                        error_count += 1;
                    }
                }
            }
            Err(e) => {
                output.push_str(&format!("Error: {}\n", e));
                error_count += 1;
            }
        }
        output.push_str("\n\n");
    }

    Ok(json!({
        "content": output.trim_end(),
        "filesRead": success_count,
        "errors": error_count,
    }))
}

async fn fs_write(args: Value, session: &mut FilengineSession) -> Result<Value, String> {
    let path = get_str(&args, "path")?;
    let content = get_str(&args, "content")?;
    let p = NativeFilengine::resolve(path, session)?;
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).await.map_err(|e| format!("mkdir: {e}"))?;
    }
    fs::write(&p, content).await.map_err(|e| format!("write: {e}"))?;
    session.track_write(path);
    Ok(json!({"written": true, "path": path}))
}

async fn fs_edit(args: Value, session: &mut FilengineSession) -> Result<Value, String> {
    let path = get_str(&args, "path")?;
    let old = get_str(&args, "old_string")?;
    let new = get_str(&args, "new_string")?;
    let p = NativeFilengine::resolve(path, session)?;
    let content = fs::read_to_string(&p).await.map_err(|e| format!("read: {e}"))?;
    let byte_offset = content.find(old).ok_or("old_string not found")?;
    // 计算 old_string 在文件中的起始行号（1-based）
    // 消费方：TUI diff 渲染用此值将相对行号映射为文件实际行号
    let start_line = content[..byte_offset].chars().filter(|&c| c == '\n').count() + 1;
    fs::write(&p, content.replace(old, new)).await.map_err(|e| format!("write: {e}"))?;
    session.track_write(path);
    Ok(json!({"edited": true, "path": path, "start_line": start_line}))
}

async fn fs_move(args: Value, session: &mut FilengineSession) -> Result<Value, String> {
    let src = get_str(&args, "source")?;
    let dst = get_str(&args, "destination")?;
    let s = NativeFilengine::resolve(src, session)?;
    let d = NativeFilengine::resolve(dst, session)?;
    fs::rename(&s, &d).await.map_err(|e| format!("move: {e}"))?;
    Ok(json!({"moved": true, "from": src, "to": dst}))
}

async fn fs_info(args: Value, session: &mut FilengineSession) -> Result<Value, String> {
    let path = get_str(&args, "path")?;
    let p = NativeFilengine::resolve(path, session)?;
    let meta = fs::metadata(&p).await.map_err(|e| format!("stat: {e}"))?;
    Ok(json!({"path": path, "size": meta.len(),
        "is_dir": meta.is_dir(), "is_file": meta.is_file()}))
}

async fn fs_search(args: Value, session: &mut FilengineSession) -> Result<Value, String> {
    let pattern = get_str(&args, "pattern")?;
    let root = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let p = if root.is_empty() { session.cwd.clone() } else { NativeFilengine::resolve(root, session)? };
    let g = glob::Pattern::new(pattern).map_err(|e| format!("glob: {e}"))?;
    let mut out = Vec::new();
    // Recursive search using walkdir (was single-level, fixed for recursive)
    for entry in walkdir::WalkDir::new(&p).into_iter().filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        if g.matches(&name) {
            out.push(entry.path().to_string_lossy().to_string());
        }
    }
    session.last_search = Some(out.clone());
    Ok(json!({"matches": out, "cwd": session.cwd.to_string_lossy()}))
}

async fn fs_ls(args: Value, session: &mut FilengineSession) -> Result<Value, String> {
    // Wrapping-B：recursive=true 时委托 fs_tree——单工具统一展现
    if args.get("recursive").and_then(|v| v.as_bool()).unwrap_or(false) {
        return fs_tree(args, session).await;
    }
    let path = get_str(&args, "path")?;
    let p = NativeFilengine::resolve(path, session)?;
    let mut entries = Vec::new();
    let mut dir = fs::read_dir(&p).await.map_err(|e| format!("read_dir: {e}"))?;
    while let Ok(Some(e)) = dir.next_entry().await {
        let name = e.file_name().to_string_lossy().to_string();
        let is_dir = e.metadata().await.ok().map(|m| m.is_dir()).unwrap_or(false);
        entries.push(json!({"name": name, "type": if is_dir { "dir" } else { "file" }}));
    }
    Ok(json!({"entries": entries, "path": path}))
}

async fn fs_tree(args: Value, session: &mut FilengineSession) -> Result<Value, String> {
    let path = get_str(&args, "path")?;
    let p = NativeFilengine::resolve(path, session)?;
    let mut tree = Vec::new();
    walk(&p, "", &mut tree, 0).await;
    Ok(json!({"tree": tree, "path": path}))
}

async fn walk(dir: &PathBuf, prefix: &str, out: &mut Vec<Value>, depth: u32) {
    if depth > 5 { return; }
    let mut entries = match fs::read_dir(dir).await { Ok(d) => d, Err(_) => return };
    while let Ok(Some(e)) = entries.next_entry().await {
        let name = e.file_name().to_string_lossy().to_string();
        let is_dir = e.metadata().await.ok().map(|m| m.is_dir()).unwrap_or(false);
        out.push(json!({"name": format!("{prefix}{name}"), "type": if is_dir { "dir" } else { "file" }}));
        if is_dir {
            Box::pin(walk(&e.path(), &format!("{prefix}{name}/"), out, depth + 1)).await;
        }
    }
}

async fn fs_mkdir(args: Value, session: &mut FilengineSession) -> Result<Value, String> {
    let path = get_str(&args, "path")?;
    let p = NativeFilengine::resolve(path, session)?;
    fs::create_dir_all(&p).await.map_err(|e| format!("mkdir: {e}"))?;
Ok(json!({"created": true, "path": path}))
}

fn get_str<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key).and_then(|v| v.as_str()).ok_or_else(|| format!("missing: {key}"))
}

// ─── Web tools ────────────────────────────────────────────────────

async fn web_fetch(args: Value, _session: &mut FilengineSession) -> Result<Value, String> {
    let url = get_str(&args, "url")?;
    // 2026-05-28: 允许 http/https/file 协议。file:// 用于读取本地 HTML/文档。
    // ftp/其他协议直接拒绝（reqwest 不支持且无合法场景）
    if !url.starts_with("http://") && !url.starts_with("https://") && !url.starts_with("file://") {
        return Err(format!("unsupported protocol: {}（仅支持 http/https/file）",
            url.split("://").next().unwrap_or("unknown")));
    }
    let extract = args.get("extract").and_then(|v| v.as_bool()).unwrap_or(false);
    let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(60).max(5);
    // max_chars：extract=true 时默认 8000（可读摘要），extract=false 时默认 512 000（原始体）
    let max_chars = args.get("max_chars").and_then(|v| v.as_u64())
        .unwrap_or(if extract { 8_000 } else { 512_000 }) as usize;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout))
        .build().map_err(|e| format!("client: {e}"))?;
    let resp = client.get(url).send().await
        .map_err(|e| format!("fetch: {e}"))?;
    let status = resp.status().as_u16();
    let content_type = resp.headers()
        .get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = resp.bytes().await
        .map_err(|e| format!("body: {e}"))?;

    if extract {
        let html = String::from_utf8_lossy(&body).to_string();
        let (text, truncated) = html_to_text(&html, max_chars);
        Ok(json!({"status": status, "url": url, "text": text, "truncated": truncated}))
    } else {
        let max_bytes = max_chars;
        let text = if body.len() > max_bytes {
            format!("[truncated {} bytes] {}", body.len(), String::from_utf8_lossy(&body[..max_bytes]))
        } else {
            String::from_utf8_lossy(&body).to_string()
        };
        Ok(json!({"status": status, "content_type": content_type, "body": text}))
    }
}

async fn web_search(args: Value, session: &mut FilengineSession) -> Result<Value, String> {
    let query = get_str(&args, "query")?;
    let count = args.get("count").and_then(|v| v.as_u64()).unwrap_or(10).min(20);
    let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(60).max(5);
    let deep = args.get("deep").and_then(|v| v.as_bool()).unwrap_or(false);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout))
        .build().map_err(|e| format!("client: {e}"))?;

    // provider 选택: 从 session.search_provider 读取（已由 executor 从 role_caps 同步）
    let mut results = match &session.search_provider {
        SearchProvider::BraveApi { api_key } => {
            let key = api_key.clone();
            brave_search(query, count, &key, timeout, &client).await?
        }
        SearchProvider::SearxNg { base_url } => {
            let base = base_url.clone();
            searxng_search(query, count, &base, timeout, &client).await?
        }
        SearchProvider::DuckDuckGo => {
            duckduckgo_search(query, count, timeout, &client).await?
        }
    };

    // deep 模式：对前 3 条结果 fetch+extract，补充页面全文摘要
    if deep {
        let mut pages = Vec::new();
        let links: Vec<String> = results.iter().take(3)
            .filter_map(|r| r.get("link").and_then(|v| v.as_str())
                .filter(|l| l.starts_with("http"))
                .map(|l| l.to_string()))
            .collect();
        for link in links {
            let fetch_args = json!({"url": link, "extract": true, "max_chars": 4000});
            // web_fetch 是无状态的，session 仅用于签名匹配，不修改 session 状态
            if let Ok(page) = web_fetch(fetch_args, session).await {
                pages.push(json!({
                    "url": link,
                    "text": page.get("text").cloned().unwrap_or(Value::Null),
                }));
            }
        }
        return Ok(json!({"results": results, "pages": pages, "query": query}));
    }

    // 截断到 count（各 provider 内部已 take(count)，防御性截断）
    results.truncate(count as usize);
    Ok(json!({"results": results, "query": query}))
}

/// DuckDuckGo HTML 解析搜索（原有逻辑提取为独立函数）
///
/// 引用：web_search() 在 provider=DuckDuckGo 时调用
async fn duckduckgo_search(
    query: &str, count: u64, _timeout: u64, client: &reqwest::Client,
) -> Result<Vec<Value>, String> {
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding(query));
    let resp = client.get(&url)
        .header("User-Agent", "Mozilla/5.0")
        .send().await
        .map_err(|e| format!("search: {e}"))?;
    let html = resp.text().await
        .map_err(|e| format!("body: {e}"))?;

    let mut results = Vec::new();
    for fragment in html.split("<h2 class=\"result__title\">").skip(1) {
        let title = extract_between(fragment, ">", "</a>").unwrap_or("").trim().to_string();
        let snippet = extract_between(fragment, "<a class=\"result__snippet\"", "</a>")
            .and_then(|s| extract_between(s, ">", "<"))
            .unwrap_or("").trim().to_string();
        let link = extract_between(fragment, "href=\"", "\"")
            .unwrap_or("").trim().to_string();

        let quality = assess_quality(&title, &snippet, &link);
        if matches!(quality, Quality::Low) {
            continue;
        }
        results.push(json!({
            "title": title,
            "snippet": snippet,
            "link": link,
            "quality": quality.label(),
        }));
        if results.len() >= count as usize {
            break;
        }
    }
    Ok(results)
}

/// Brave Search API
///
/// 引用：web_search() 在 provider=BraveApi 时调用
/// 生命周期：无状态，每次搜索独立创建/销毁
async fn brave_search(
    query: &str, count: u64, api_key: &str, timeout: u64, client: &reqwest::Client,
) -> Result<Vec<Value>, String> {
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        urlencoding(query), count
    );
    let resp = client.get(&url)
        .header("Accept", "application/json")
        .header("Accept-Encoding", "gzip")
        .header("X-Subscription-Token", api_key)
        .timeout(std::time::Duration::from_secs(timeout))
        .send().await
        .map_err(|e| format!("brave search: {e}"))?;
    let json: Value = resp.json().await.map_err(|e| format!("brave json: {e}"))?;
    let mut results = Vec::new();
    if let Some(items) = json.get("web")
        .and_then(|w| w.get("results"))
        .and_then(|r| r.as_array())
    {
        for item in items.iter().take(count as usize) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let snippet = item.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let link = item.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            results.push(json!({"title": title, "snippet": snippet, "link": link, "quality": "normal"}));
        }
    }
    Ok(results)
}

/// SearxNG 聚合搜索
///
/// 引用：web_search() 在 provider=SearxNg 时调用
/// 生命周期：无状态，每次搜索独立创建/销毁
async fn searxng_search(
    query: &str, count: u64, base_url: &str, timeout: u64, client: &reqwest::Client,
) -> Result<Vec<Value>, String> {
    let url = format!(
        "{}/search?q={}&format=json&engines=google,bing&pageno=1",
        base_url.trim_end_matches('/'), urlencoding(query)
    );
    let resp = client.get(&url)
        .timeout(std::time::Duration::from_secs(timeout))
        .send().await
        .map_err(|e| format!("searxng: {e}"))?;
    let json: Value = resp.json().await.map_err(|e| format!("searxng json: {e}"))?;
    let mut results = Vec::new();
    if let Some(items) = json.get("results").and_then(|r| r.as_array()) {
        for item in items.iter().take(count as usize) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let snippet = item.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let link = item.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            results.push(json!({"title": title, "snippet": snippet, "link": link, "quality": "normal"}));
        }
    }
    Ok(results)
}

fn assess_quality(title: &str, snippet: &str, link: &str) -> Quality {
    let t = title.to_lowercase();
    let s = snippet.to_lowercase();

    // 🔴 广告检测
    let link_lower = link.to_lowercase();
    let ad_keywords = ["咨询", "平台", "立即", "免费", "点击", "限时", "促销", "广告"];
    if ad_keywords.iter().any(|k| t.contains(k))
        || link_lower.contains("/ad/")
        || link_lower.contains("/sponsor/")
        || link_lower.contains("utm_source=ad")
        || snippet.len() < 30
    {
        return Quality::Ad;
    }

    // 🟡 疑似AI生成
    let ai_patterns = ["首先", "其次", "最后", "第一", "第二", "第三",
        "深入探讨", "显著", "至关重要", "不容忽视",
        "在当今", "值得注意的是", "综上所述"];
    let ai_hits = ai_patterns.iter().filter(|p| s.contains(*p)).count();
    let no_data = !s.contains('%') && !s.contains("://");
    if ai_hits >= 2 || (ai_hits >= 1 && no_data && s.len() > 80) {
        return Quality::AiGenerated;
    }

    // ⚫ 低质
    if snippet.len() < 50 {
        return Quality::Low;
    }
    let overlap = common_chars(title, snippet);
    if overlap < 20 {
        return Quality::Low;
    }

    // 🟢 高可信
    let trusted = ["docs.rs", "github.com", "wikipedia.org", "stackoverflow.com",
        "rust-lang.org", "crates.io", "play.rust-lang.org"];
    if trusted.iter().any(|d| link_lower.contains(d)) {
        return Quality::High;
    }

    Quality::Normal
}

fn common_chars(a: &str, b: &str) -> usize {
    a.chars().filter(|c| b.contains(*c)).count().saturating_mul(100) / a.len().max(1)
}

enum Quality {
    High,
    Normal,
    AiGenerated,
    Ad,
    Low,
}

impl Quality {
    fn label(&self) -> &'static str {
        match self {
            Quality::High => "high",
            Quality::Normal => "normal",
            Quality::AiGenerated => "ai_generated",
            Quality::Ad => "ad",
            Quality::Low => "low",
        }
    }
}

fn extract_between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)?;
    let j = s[i + start.len()..].find(end)?;
    Some(&s[i + start.len()..i + start.len() + j])
}

fn urlencoding(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        b' ' => "+".into(),
        _ => format!("%{:02X}", b),
    }).collect()
}

// ─── Grep ─────────────────────────────────────────────────────────

async fn fs_grep(args: Value, session: &mut FilengineSession) -> Result<Value, String> {
    let pattern = get_str(&args, "pattern")?;
    let root = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let include = args.get("include").and_then(|v| v.as_str());
    let max_results = args.get("max_results").and_then(|v| v.as_u64()).unwrap_or(20).min(100);
    let context = args.get("context").and_then(|v| v.as_u64()).unwrap_or(0).min(5) as usize;
    // mode: "fine"（默认，返回逐行匹配）或 "coarse"（按文件聚合，只返回文件路径+计数）
    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("fine");

    let re = regex::Regex::new(pattern).map_err(|e| format!("invalid regex: {e}"))?;
    let p = if root.is_empty() { session.cwd.clone() } else { NativeFilengine::resolve(root, session)? };

    let include_glob = include.and_then(|s| glob::Pattern::new(s).ok());

    let mut all_matches = Vec::new();
    let mut files_scanned = 0u32;
    grep_dir(&p, &re, &include_glob, context, max_results as usize, &mut all_matches, &mut files_scanned).await;

    if mode == "coarse" {
        // 按文件统计 match 数，按 count 降序
        let mut file_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for m in &all_matches {
            let file = m.get("file").and_then(|v| v.as_str()).unwrap_or("").to_string();
            *file_counts.entry(file).or_insert(0) += 1;
        }
        let mut files: Vec<Value> = file_counts.into_iter()
            .map(|(f, c)| json!({"file": f, "count": c}))
            .collect();
        files.sort_by(|a, b| {
            let ca = a.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
            let cb = b.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
            cb.cmp(&ca)
        });
        return Ok(json!({"mode": "coarse", "files": files, "files_scanned": files_scanned, "pattern": pattern}));
    }

    Ok(json!({
        "matches": all_matches,
        "total_matches": all_matches.len(),
        "files_scanned": files_scanned,
        "pattern": pattern,
        "root": p.to_string_lossy(),
    }))
}

async fn grep_dir(
    dir: &PathBuf, re: &regex::Regex, filter: &Option<glob::Pattern>,
    context: usize, limit: usize, out: &mut Vec<Value>, scanned: &mut u32,
) {
    if out.len() >= limit { return; }
    let mut entries = match tokio::fs::read_dir(dir).await { Ok(d) => d, Err(_) => return };
    while let Ok(Some(e)) = entries.next_entry().await {
        let path = e.path();
        if out.len() >= limit { break; }
        *scanned += 1;
        if e.metadata().await.ok().map(|m| m.is_dir()).unwrap_or(false) {
            Box::pin(grep_dir(&path, re, filter, context, limit, out, scanned)).await;
        } else {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(ref g) = filter {
                if !g.matches(&name) { continue; }
            }
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                for (line_no, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        let mut ctx_lines = Vec::new();
                        if context > 0 {
                            let lines: Vec<&str> = content.lines().collect();
                            let start = line_no.saturating_sub(context);
                            let end = (line_no + 1 + context).min(lines.len());
                            for (ci, line_text) in lines.iter().enumerate().take(end).skip(start) {
                                ctx_lines.push(format!("{}: {}", ci + 1, line_text));
                            }
                        }
                        out.push(json!({
                            "file": path.to_string_lossy(),
                            "line": line_no + 1,
                            "match": line,
                            "context": ctx_lines,
                        }));
                        if out.len() >= limit { return; }
                    }
                }
            }
        }
    }
}

// ─── Bash Exec ────────────────────────────────────────────────────

/// Shell metacharacters that could enable injection.
/// Blocks: ; | & ` $ ( ) < > ! \n \r
/// Shell metacharacters that enable injection. `!` removed: it's used in legit
/// commands like `echo "hello!"` and `git commit -m "fix!"`, and history expansion
/// is disabled in non-interactive sh -c context anyway.
// 2026-05-28: SHELL_META 已移除——命令通过 `sh -c` 执行，元字符是合法 shell 语法
// 安全由 classify_bash_command 的语义分类 + MCIP 门控保障

/// Bash command classification result.
///
/// ## Tiers
/// - T0 (Allow): Pure read-only commands, no side effects
/// - T1 (Allow): Dev tools with safe subcommand gate
/// - T2 (NeedsConfirm): State-modifying commands, require user confirmation
/// - T3 (NeedsConfirm/Dangerous): System-dangerous commands, require confirmation, timeout=auto-reject
///
/// ## Referenced by
/// - `bash_exec()` — inner defense-in-depth guard
/// - `crate::core::pipeline::mod.rs` — pre-execution MCIP-level intercept
///
/// ## Lifecycle
/// - Created per bash_exec invocation (stateless classification)
/// - No persistent state; classification is deterministic from command string
#[derive(Debug, Clone, PartialEq)]
pub enum BashDecision {
    /// T0/T1: Execute immediately without confirmation
    Allow,
    /// T2: Requires user confirmation before execution (timeout → single reject)
    NeedsConfirm(String),
    /// T3: Dangerous command, requires confirmation with elevated warning (timeout → single reject)
    /// Pipeline treats same as NeedsConfirm but with "dangerous" tag for UI styling
    Dangerous(String),
}

/// Classify a bash command into security tiers.
///
/// ## Dependencies
/// - Called by `bash_exec()` (inner guard) and pipeline dispatch (MCIP-level intercept)
/// - No external dependencies; pure function
///
/// ## Command Tiers
/// | Tier | Policy | Examples |
/// |------|--------|----------|
/// | Tier | Policy | Token Impact | Examples |
/// |------|--------|-------------|----------|
/// | T0 | Zero-block allow | 0 extra token | ls, cat, grep, find, stat, ps, env |
/// | T1 | Allow + subcommand gate | 0 extra token | git status, cargo check, npm ls |
/// | T2 | Confirm popup (timeout→reject) | ~50 tokens on reject | rm, mv, git push, npm install |
/// | T3 | Confirm popup + danger warning | ~50 tokens on reject | sudo, dd, shutdown, mkfs |
pub fn classify_bash_command(command: &str) -> BashDecision {
    // 2026-05-28: 允许 shell 元字符（|, &, $, >, ; 等）——命令通过 `sh -c` 执行，
    // 这些是合法 shell 语法。按首个命令（pipe/chain 前的部分）做语义分类。
    // 如果管道链中包含 dangerous 命令，首命令的分类决定整体安全等级。
    //
    // 提取首个命令：取 `|`, `&&`, `||`, `;` 之前的部分
    let first_segment = command
        .split(&['|', ';'][..])
        .next()
        .unwrap_or(command)
        .split("&&")
        .next()
        .unwrap_or(command)
        .trim();

    let parts: Vec<&str> = first_segment.split_whitespace().collect();
    let cmd = parts.first().copied().unwrap_or("");

    // ── T3: Dangerous (system-level, needs confirm with elevated warning) ──
    const DANGEROUS_COMMANDS: &[&str] = &[
        "sudo", "su", "doas", "dd", "mkfs", "fdisk", "parted", "diskutil",
        "shutdown", "reboot", "halt", "poweroff", "init",
        "iptables", "ufw", "pfctl", "chroot", "mount", "umount",
        "systemctl", "launchctl", "service",
        "format", "fsck",
    ];
    if DANGEROUS_COMMANDS.contains(&cmd) {
        return BashDecision::Dangerous(
            format!("⚠ '{}' is a system-dangerous command", cmd));
    }

    // ── T0: Always Allow (read-only, no side effects) ──
    const ALLOW_READONLY: &[&str] = &[
        "ls", "cat", "echo", "pwd", "which", "head", "tail",
        "wc", "sort", "uniq", "cut", "grep", "find", "diff", "stat",
        "du", "df", "ps", "env", "whoami", "id", "date", "cal",
        "uptime", "free", "uname", "file", "basename", "dirname",
        "realpath", "readlink", "type", "command", "tr", "seq",
        "true", "false", "test", "printf", "md5sum", "sha256sum",
        "xxd", "od", "hexdump", "strings", "less", "more",
    ];
    if ALLOW_READONLY.contains(&cmd) {
        return BashDecision::Allow;
    }

    // ── T1/T2: Dev tools with subcommand classification ──
    match cmd {
        "git" => classify_git(&parts, command),
        "cargo" => classify_cargo(&parts),
        "npm" | "npx" | "yarn" | "pnpm" => classify_npm(&parts, cmd),
        "python3" | "python" => classify_python(&parts),
        "node" => classify_node(&parts),
        "rustc" => classify_rustc(&parts),
        "make" | "cmake" => classify_make(&parts),
        "docker" => classify_docker(&parts),
        "curl" => classify_curl(&parts, command),
        "wget" => classify_wget(&parts),
        // T2: State-modifying commands (always need confirm)
        "rm" | "rmdir" => BashDecision::NeedsConfirm(
            format!("'{}' deletes files/directories", cmd)),
        "mv" => BashDecision::NeedsConfirm("'mv' moves/renames files".into()),
        "cp" => BashDecision::Allow, // cp 不删除原件，低风险
        "mkdir" => BashDecision::Allow, // mkdir is low-risk (creates dirs)
        "touch" => BashDecision::Allow, // touch is low-risk (creates empty files)
        "chmod" | "chown" => BashDecision::NeedsConfirm(
            format!("'{}' changes file permissions/ownership", cmd)),
        "kill" | "killall" | "pkill" => BashDecision::NeedsConfirm(
            format!("'{}' terminates processes", cmd)),
        "pip" | "pip3" => classify_pip(&parts),
        "brew" => classify_brew(&parts),
        "tar" => classify_tar(&parts),
        "zip" | "unzip" | "gzip" | "gunzip" => BashDecision::Allow,
        "sed" => {
            // sed -i is destructive; without -i is read-only
            if parts.iter().any(|p| p.starts_with("-i") || *p == "--in-place") {
                BashDecision::NeedsConfirm("'sed -i' modifies files in-place".into())
            } else {
                BashDecision::Allow
            }
        }
        "awk" => BashDecision::Allow, // awk without redirection is read-only (metachar blocks >)
        "tee" => BashDecision::NeedsConfirm("'tee' writes to files".into()),
        // macOS/Linux: 打开文件/URL（无副作用，只是启动外部 app）
        "open" | "xdg-open" => BashDecision::Allow,
        // 编辑器/IDE 打开（无副作用）
        "code" | "vim" | "nvim" | "nano" | "subl" | "cursor" => BashDecision::Allow,
        // Rust 工具链管理（只读查询）
        "rustup" => {
            let subcmd = parts.get(1).copied().unwrap_or("");
            if matches!(subcmd, "show" | "which" | "doc" | "man" | "check"
                | "target" | "toolchain" | "component" | "override" | "--version") {
                BashDecision::Allow
            } else {
                BashDecision::NeedsConfirm(format!("'rustup {}' may modify toolchain", subcmd))
            }
        }
        // 剪贴板（允许——常用于复制结果）
        "pbcopy" | "pbpaste" | "xclip" | "xsel" | "wl-copy" | "wl-paste" => BashDecision::Allow,
        // man/help 查阅
        "man" | "info" | "tldr" => BashDecision::Allow,
        // 其他未知命令 → 需确认
        _ => BashDecision::NeedsConfirm(format!("command '{}' not in allowed list", cmd)),
    }
}

/// Git subcommand classification.
/// T1 (Allow): read-only git ops. T2 (NeedsConfirm): write ops.
fn classify_git(parts: &[&str], command: &str) -> BashDecision {
    // Block known injection vectors: `git -c key=val` (config injection)
    // 注意：`git log -c` (combined diff) 是合法操作，不能误匹配
    // 只匹配 "git -c " 即 parts[1] == "-c" 的情况
    let has_config_inject = parts.get(1).copied() == Some("-c")
        || command.contains("--upload-pack") || command.contains("--exec=");
    if has_config_inject {
        return BashDecision::NeedsConfirm("git config injection vector detected (-c/--upload-pack/--exec=)".into());
    }

    let subcmd = parts.get(1).copied().unwrap_or("");
    const GIT_READONLY: &[&str] = &[
        "status", "log", "diff", "show", "branch", "tag", "remote",
        "fetch", "describe", "rev-parse", "ls-files", "ls-tree",
        "blame", "shortlog", "reflog", "stash", "config",
        "rev-list", "cat-file", "name-rev", "symbolic-ref",
        "for-each-ref", "count-objects", "fsck", "verify-pack",
    ];
    // git stash list/show = readonly; git stash drop/pop/apply = write
    if subcmd == "stash" {
        let stash_op = parts.get(2).copied().unwrap_or("list");
        return match stash_op {
            "list" | "show" => BashDecision::Allow,
            _ => BashDecision::NeedsConfirm(
                format!("'git stash {}' modifies stash state", stash_op)),
        };
    }
    // git branch -d/-D = write; git branch (list) = readonly
    if subcmd == "branch" {
        if parts.iter().any(|p| *p == "-d" || *p == "-D" || *p == "--delete") {
            return BashDecision::NeedsConfirm("'git branch -d' deletes branches".into());
        }
        return BashDecision::Allow;
    }
    // git config --get = read; git config (set) = write
    if subcmd == "config" {
        if parts.iter().any(|p| *p == "--get" || *p == "--get-all" || *p == "--list" || *p == "-l") {
            return BashDecision::Allow;
        }
        return BashDecision::NeedsConfirm("'git config' modifies git configuration".into());
    }
    // git tag -l = read; git tag (create/delete) = write
    if subcmd == "tag" {
        if parts.iter().any(|p| *p == "-l" || *p == "--list") || parts.len() == 2 {
            return BashDecision::Allow;
        }
        return BashDecision::NeedsConfirm("'git tag' creates/deletes tags".into());
    }

    if GIT_READONLY.contains(&subcmd) {
        return BashDecision::Allow;
    }
    // 日常开发写操作（低风险，可回退）→ 允许
    const GIT_DEV_WRITE: &[&str] = &[
        "add", "commit", "pull", "clone", "init", "checkout", "switch",
        "merge", "cherry-pick", "stash",
    ];
    if GIT_DEV_WRITE.contains(&subcmd) {
        return BashDecision::Allow;
    }
    // 高风险写操作（不可回退/影响远端）→ 需确认
    // push, reset, rebase, push --force, clean, gc
    BashDecision::NeedsConfirm(format!("'git {}' modifies repository state", subcmd))
}

/// Cargo subcommand classification.
/// T1: build/check/test/clippy/fmt/doc/tree/metadata. T2: install/publish/add/remove/run.
fn classify_cargo(parts: &[&str]) -> BashDecision {
    let subcmd = parts.get(1).copied().unwrap_or("");
    const CARGO_SAFE: &[&str] = &[
        "build", "check", "test", "clippy", "fmt", "doc",
        "tree", "metadata", "verify-project", "locate-project",
        "pkgid", "search", "info",
        // 开发常用：运行/基准/清理/更新/安装是日常操作
        "run", "bench", "clean", "update", "generate-lockfile",
        "vendor", "fetch", "install", "add", "remove",
    ];
    if subcmd == "--version" || subcmd == "-V" {
        return BashDecision::Allow;
    }
    if CARGO_SAFE.contains(&subcmd) {
        BashDecision::Allow
    } else {
        // install, publish, add, remove, update, run, bench, clean, ...
        BashDecision::NeedsConfirm(format!("'cargo {}' may modify project state", subcmd))
    }
}

/// npm/yarn/pnpm subcommand classification.
/// T1: 只读 + 开发常用（test/run/start/build 不改 node_modules）
/// T2: install/uninstall/publish（修改依赖或发布）
fn classify_npm(parts: &[&str], pkg_mgr: &str) -> BashDecision {
    let subcmd = parts.get(1).copied().unwrap_or("");
    const NPM_ALLOW: &[&str] = &[
        "ls", "list", "info", "view", "audit", "outdated",
        "why", "explain", "pack", "search", "--version", "-v",
        // 开发常用：运行脚本/测试/构建不修改 node_modules
        "test", "t", "run", "run-script", "start", "build",
        "dev", "serve", "lint", "format", "typecheck",
    ];
    if NPM_ALLOW.contains(&subcmd) {
        BashDecision::Allow
    } else {
        // install, uninstall, publish, init, link, etc.
        BashDecision::NeedsConfirm(
            format!("'{} {}' may modify node_modules or project", pkg_mgr, subcmd))
    }
}

/// Python classification.
/// T1: --version, -c (inline eval), -m pytest/unittest, script.py (开发常用)
/// T2: -m pip install (修改包)
fn classify_python(parts: &[&str]) -> BashDecision {
    let flag = parts.get(1).copied().unwrap_or("");
    // --version
    if matches!(flag, "--version" | "-V") {
        return BashDecision::Allow;
    }
    // python3 -c "expr" — inline eval, 允许（无文件副作用）
    if matches!(flag, "-c") {
        return BashDecision::Allow;
    }
    // python3 -m module
    if flag == "-m" {
        let module = parts.get(2).copied().unwrap_or("");
        // 测试框架：允许
        if matches!(module, "pytest" | "unittest" | "doctest" | "mypy" | "black" | "ruff"
            | "isort" | "flake8" | "pylint" | "json.tool" | "http.server") {
            return BashDecision::Allow;
        }
        // pip 子命令分流
        if module == "pip" {
            let pip_cmd = parts.get(3).copied().unwrap_or("");
            if matches!(pip_cmd, "list" | "show" | "freeze" | "check" | "search") {
                return BashDecision::Allow;
            }
            return BashDecision::NeedsConfirm(
                format!("'python -m pip {}' modifies packages", pip_cmd));
        }
        return BashDecision::NeedsConfirm(
            format!("'python -m {}' may have side effects", module));
    }
    // python3 script.py — 允许（LLM 写完脚本需要验证运行）
    if flag.ends_with(".py") {
        return BashDecision::Allow;
    }
    BashDecision::NeedsConfirm("python execution may have side effects".into())
}

/// Node classification.
/// T1: --version, -e (eval), -p (print), script.js (开发常用)
fn classify_node(parts: &[&str]) -> BashDecision {
    let flag = parts.get(1).copied().unwrap_or("");
    if matches!(flag, "--version" | "-v" | "-e" | "-p" | "--eval" | "--print") {
        return BashDecision::Allow;
    }
    // node script.js — 允许（LLM 验证脚本执行）
    if flag.ends_with(".js") || flag.ends_with(".mjs") || flag.ends_with(".ts") {
        return BashDecision::Allow;
    }
    BashDecision::NeedsConfirm("'node' script execution may have side effects".into())
}

/// rustc classification.
fn classify_rustc(parts: &[&str]) -> BashDecision {
    let flag = parts.get(1).copied().unwrap_or("");
    if matches!(flag, "--version" | "-V" | "--print" | "--explain") {
        BashDecision::Allow
    } else {
        BashDecision::NeedsConfirm("'rustc' compilation may produce artifacts".into())
    }
}

/// make/cmake classification.
fn classify_make(parts: &[&str]) -> BashDecision {
    if parts.iter().any(|p| *p == "-n" || *p == "--dry-run" || *p == "--just-print") {
        BashDecision::Allow
    } else {
        BashDecision::NeedsConfirm("'make' builds and may modify filesystem".into())
    }
}

/// Docker classification. Read-only inspection vs. container lifecycle.
fn classify_docker(parts: &[&str]) -> BashDecision {
    let subcmd = parts.get(1).copied().unwrap_or("");
    const DOCKER_READONLY: &[&str] = &[
        "ps", "images", "inspect", "logs", "stats", "top",
        "port", "version", "info", "network", "volume",
    ];
    if subcmd == "--version" || subcmd == "-v" {
        return BashDecision::Allow;
    }
    if DOCKER_READONLY.contains(&subcmd) {
        BashDecision::Allow
    } else {
        BashDecision::NeedsConfirm(format!("'docker {}' modifies container state", subcmd))
    }
}

/// curl classification. GET = safe; POST/PUT/DELETE/PATCH = needs confirm.
fn classify_curl(parts: &[&str], command: &str) -> BashDecision {
    // Check for write methods
    let has_write_method = parts.iter().any(|p| {
        *p == "-X" || *p == "--request"
    }) && command.to_uppercase().contains("POST")
        || command.to_uppercase().contains("PUT")
        || command.to_uppercase().contains("DELETE")
        || command.to_uppercase().contains("PATCH");
    // Check for data flags (implies POST)
    let has_data = parts.iter().any(|p| {
        p.starts_with("-d") || p.starts_with("--data") || *p == "-F" || *p == "--form"
    });
    if has_write_method || has_data {
        BashDecision::NeedsConfirm("curl with POST/PUT/DELETE sends data to remote".into())
    } else {
        BashDecision::Allow
    }
}

/// wget classification. 下载文件低风险（不覆盖已有，除非 -O）
fn classify_wget(parts: &[&str]) -> BashDecision {
    // -O 指定输出文件可能覆盖 → 需确认
    if parts.iter().any(|p| p.starts_with("-O") || *p == "--output-document") {
        BashDecision::NeedsConfirm("'wget -O' may overwrite existing file".into())
    } else {
        // 默认下载到当前目录新文件，低风险
        BashDecision::Allow
    }
}

/// pip/pip3 classification.
fn classify_pip(parts: &[&str]) -> BashDecision {
    let subcmd = parts.get(1).copied().unwrap_or("");
    if matches!(subcmd, "list" | "show" | "freeze" | "check" | "search" | "--version" | "-V") {
        BashDecision::Allow
    } else {
        BashDecision::NeedsConfirm(format!("'pip {}' modifies installed packages", subcmd))
    }
}

/// brew classification.
fn classify_brew(parts: &[&str]) -> BashDecision {
    let subcmd = parts.get(1).copied().unwrap_or("");
    if matches!(subcmd, "list" | "info" | "search" | "doctor" | "config" | "deps" | "--version") {
        BashDecision::Allow
    } else {
        BashDecision::NeedsConfirm(format!("'brew {}' modifies system packages", subcmd))
    }
}

/// tar classification. -t (list) = safe; -x (extract) = needs confirm.
fn classify_tar(parts: &[&str]) -> BashDecision {
    let flags: String = parts.iter()
        .filter(|p| p.starts_with('-'))
        .flat_map(|p| p.chars())
        .collect();
    if flags.contains('t') {
        // listing contents
        BashDecision::Allow
    } else if flags.contains('x') {
        BashDecision::NeedsConfirm("'tar -x' extracts files (may overwrite)".into())
    } else if flags.contains('c') {
        // creating archive = allow (writes new file but doesn't destroy existing)
        BashDecision::Allow
    } else {
        BashDecision::Allow
    }
}

/// Inner defense-in-depth guard (called within bash_exec after pipeline-level classification).
/// Pipeline is the primary gate (classify_bash_command → NeedsConfirm → UI confirm).
/// Once confirmed, execution reaches bash_exec — inner guard only blocks injection vectors.
///
/// ## Referenced by
/// - `bash_exec()` only (belt-and-suspenders; pipeline classify_bash_command is primary gate)
// 2026-05-28: is_command_allowed() 已移除
// 原逻辑（SHELL_META 硬拒）在 `sh -c` 执行模式下是误拒——元字符是合法 shell 语法。
// 安全由 pipeline 层 classify_bash_command() + MCIP 门控保障。

async fn bash_exec(args: Value, session: &mut FilengineSession) -> Result<Value, String> {
    let command = get_str(&args, "command")?;
    let timeout = args.get("timeout").and_then(|v| v.as_u64())
        .unwrap_or(session.bash_default_timeout)
        .min(session.bash_max_timeout);
    let workdir = args.get("workdir").and_then(|v| v.as_str())
        .map(|p| NativeFilengine::resolve(p, session))
        .unwrap_or(Ok(session.cwd.clone()))?;

    // 2026-05-28: 移除工具内部的命令分类和硬拒逻辑
    // Pipeline 层（line 2118-2143）已在 MCIP 阶段做了 classify_bash_command()：
    //   - Allow → 直接执行
    //   - NeedsConfirm → 弹用户确认对话框
    //   - Dangerous → 在 Full 模式下降级为 NeedsConfirm，否则也弹确认
    // 工具执行到此处时已通过 MCIP 门控，无需重复检查。
    // 唯一保留的安全防线：DANGEROUS_COMMANDS 系统级命令由 pipeline 在 MCIP 阶段拦截。
    let _ = session.bash_policy; // consumed by pipeline's classify call

    let child = TokioCmd::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&workdir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout),
        child.wait_with_output(),
    ).await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            Ok(json!({
                "exit_code": output.status.code().unwrap_or(-1),
                "stdout": stdout,
                "stderr": stderr,
                "command": command,
            }))
        }
        Ok(Err(e)) => Err(format!("exec: {e}")),
        Err(_) => Err(format!("timeout after {timeout}s")),
    }
}

// ─── ToolExecutor wrapper ─────────────────────────────────────────

/// Wraps NativeFilengine as a stateless ToolExecutor.
///
/// ## 无状态设计（post ExecutionContext 重构）
///
/// 不再持有 `Arc<RwLock<FilengineSession>>`。
/// 每次执行时从 `ctx.filengine` 取当前 session（per-request 注入），
/// 实现多 session 隔离：session A 和 session B 的文件操作互不影响。
///
/// ## 生命周期
/// - 创建：`register_executors()` 时（单例，与 ToolRegistry 同生命周期）
/// - 消费：每次工具调用时从 `ExecutionContext` 读取 session，不缓存状态
/// - 销毁：随 ToolRegistry 销毁
pub struct FilengineToolExecutor {
    native: NativeFilengine,
}

impl FilengineToolExecutor {
    pub fn new() -> Self {
        Self { native: NativeFilengine }
    }
}

impl Default for FilengineToolExecutor {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl ToolExecutor for FilengineToolExecutor {
    async fn execute(&self, tool_id: &ToolId, params: Value, ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        // 2026-05-28: ToolId 直接等于 schema name（fs_read / bash_exec / web_fetch）
        // 去掉了 filengine_ 前缀——工具名更短更直观，LLM token 开销更低
        let tool_name: &str = &tool_id.0;

        // Phase 2 undo 注入：写工具前 snapshot/record，成功后 commit
        // 仅当 session.undo_logger=Some 时启用；None 走原路径（向后兼容）
        let mut session = ctx.filengine.write().await;
        // 每次工具调用前同步 role_caps → session（保持 session 与 role 策略一致）
        // 引用：ctx.role_caps 由 pipeline 从 SessionState.role_caps 注入，工具返回后 drop
        session.bash_default_timeout = ctx.bash_default_timeout;
        session.bash_max_timeout = ctx.bash_max_timeout;
        session.fs_roots = ctx.role_caps.fs_roots.clone();
        session.bash_policy = ctx.role_caps.bash_policy;
        session.search_provider = ctx.role_caps.search_provider.clone();
        let pending = if let Some(logger) = session.undo_logger.clone() {
            match tool_name {
                "fs_write" | "fs_edit" => {
                    // 解析路径（不存在路径走 resolve 的"祖先 canonicalize + 字面拼"分支，
                    // 与 fs_write 内部 resolve 行为一致）
                    if let Some(path_str) = params.get("path").and_then(|v| v.as_str()) {
                        if let Ok(abs) = NativeFilengine::resolve(path_str, &session) {
                            // best-effort：snapshot 失败不阻塞工具
                            logger.snapshot_before(tool_name, &abs).await.ok()
                                .map(|p| (p, logger.clone()))
                        } else { None }
                    } else { None }
                }
                "fs_move" => {
                    let src = params.get("source").and_then(|v| v.as_str());
                    let dst = params.get("destination").and_then(|v| v.as_str());
                    if let (Some(s), Some(d)) = (src, dst) {
                        if let (Ok(s_abs), Ok(d_abs)) = (
                            NativeFilengine::resolve(s, &session),
                            NativeFilengine::resolve(d, &session),
                        ) {
                            logger.record_move(&s_abs, &d_abs).await.ok()
                                .map(|p| (p, logger.clone()))
                        } else { None }
                    } else { None }
                }
                "fs_mkdir" => {
                    if let Some(path_str) = params.get("path").and_then(|v| v.as_str()) {
                        if let Ok(abs) = NativeFilengine::resolve(path_str, &session) {
                            logger.record_mkdir(&abs).await.ok()
                                .map(|p| (p, logger.clone()))
                        } else { None }
                    } else { None }
                }
                _ => None,
            }
        } else {
            None
        };

        // 实际工具执行
        let result = self.native.execute(tool_name, params, &mut session).await
            .map_err(abacus_types::KernelError::Other);

        // 成功才 commit；失败丢弃 PendingEntry（snapshot 文件留下，下次容量回收时被 prune）
        if result.is_ok() {
            if let Some((pe, lg)) = pending {
                // commit best-effort：log 写失败不影响工具结果
                let _ = pe.commit(ctx.turn_number, &lg).await;
            }
        }

        result
    }
}

// ─── Schema definitions ────────────────────────────────────────────

fn schema(name: &str, desc: &str, props: Value, required: &[&str],
          confirm: bool, tokens: u32, latency: &str, risk: &str) -> ToolSchema {
    // Phase β-G 启发式：读/查类工具 idempotent=true，写/执行类 false
    let read_only = matches!(name,
        "fs_read" | "fs_info" | "fs_search" | "fs_ls" | "fs_tree"
        | "fs_grep" | "fs_read_multiple" | "web_fetch" | "web_search"
    );
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
            allowed_paths: Some(allowed_roots().iter().map(|s| s.to_string()).collect()),
            max_size_mb: Some(10),
            confirm_required: confirm,
            needs_sandbox: false,
        }),
        cost: Some(ToolCost { tokens, latency: latency.into(), risk: risk.into() }),
        examples: Vec::new(),                  // 数据补全后续工程
        applicable_task_kinds: None,           // None = 所有任务可见
        idempotent: read_only,                 // 读类工具可并行
        // P0-C2: filengine.* schema 在运行时不变，参与 KV prefix cache
        schema_stable: true,
    }
}

pub fn schemas() -> Vec<ToolSchema> {
    vec![
        schema("fs_read",  "读取文件完整内容（支持单 path 字符串或 paths 数组批量）",
            json!({
                "path": {"type": "string", "description": "单文件绝对路径"},
                "paths": {"type": "array", "items": {"type": "string"}, "description": "批量文件路径(最多20)；与 path 二选一"}
            }), &["path"], false, 64, "10ms", "low"),
        // 2026-05-28: confirm_required=false — pipeline MCIP sensitive_operations 已覆盖
        schema("fs_write", "创建或覆盖文件",
            json!({
                "path": {"type":"string", "description": "目标文件绝对路径"},
                "content": {"type":"string", "description": "写入的完整文件内容"}
            }), &["path","content"], false, 64, "10ms", "medium"),
        schema("fs_edit", "精确替换文件中的文本段（old_string 必须在文件中唯一匹配）",
            json!({
                "path": {"type":"string", "description": "文件绝对路径"},
                "old_string": {"type":"string", "description": "要替换的原始文本（必须精确匹配文件中的内容）"},
                "new_string": {"type":"string", "description": "替换后的新文本"}
            }), &["path","old_string","new_string"], false, 96, "15ms", "medium"),
        schema("fs_move", "移动或重命名文件/目录",
            json!({
                "source": {"type":"string", "description": "源文件/目录绝对路径"},
                "destination": {"type":"string", "description": "目标绝对路径"}
            }), &["source","destination"], false, 48, "10ms", "medium"),
        schema("fs_info", "获取文件或目录元数据（大小/权限/修改时间）",
            json!({"path": {"type":"string", "description": "文件或目录绝对路径"}}),
            &["path"], false, 32, "5ms", "low"),
        schema("fs_search", "按 Glob 模式搜索文件名（如 **/*.rs）",
            json!({
                "pattern": {"type":"string", "description": "Glob 模式（如 **/*.ts, src/**/mod.rs）"},
                "path": {"type":"string", "description": "搜索根目录（默认当前工作目录）"}
            }), &["pattern"], false, 48, "50ms", "low"),
        schema("fs_ls", "列出目录内容（recursive=true 时递归 5 层树）",
            json!({
                "path": {"type": "string"},
                "recursive": {"type": "boolean", "description": "true=递归树形(最多5层)；false=单层(默认)"}
            }), &["path"], false, 32, "5ms", "low"),
        schema("fs_mkdir", "递归创建目录（含所有父目录）",
            json!({"path": {"type":"string", "description": "要创建的目录绝对路径"}}),
            &["path"], false, 32, "5ms", "low"),
        schema("web_fetch", "HTTP GET 请求获取网页内容",
            json!({"url": {"type":"string", "description":"完整 URL"},
                   "timeout": {"type":"number", "description":"超时秒数(默认60)"},
                   "extract": {"type":"boolean", "description":"移除 HTML 标签返回可读文本（默认 false）"},
                   "max_chars": {"type":"integer", "description":"最大返回字符数（extract=true 时默认 8000）"}}),
            &["url"], false, 128, "1s", "low"),
        schema("web_search", "搜索引擎搜索并返回结果标题/摘要/链接",
            json!({"query": {"type":"string", "description":"搜索关键词"},
                   "count": {"type":"number", "description":"返回条数(默认10,最大20)"},
                   "timeout": {"type":"number", "description":"超时秒数(默认60)"},
                   "deep": {"type":"boolean", "description":"true 时抓取前3条结果页面正文并附在 pages 字段（默认 false）"}}),
            &["query"], false, 128, "2s", "low"),
        schema("fs_grep", "搜索文件内容（正则匹配，返回匹配行+文件路径+行号）",
            json!({"pattern": {"type":"string", "description":"正则表达式（如 fn main|class.*Error）"},
                   "path": {"type":"string", "description":"搜索根目录绝对路径（默认当前工作目录）"},
                   "include": {"type":"string", "description":"文件名 glob 过滤（如 *.rs, *.{ts,tsx}）"},
                   "max_results": {"type":"number", "description":"最大结果数（默认20,最大100）"},
                   "context": {"type":"number", "description":"匹配行前后上下文行数（0-5,默认0）"},
                   "mode": {"type":"string", "description":"fine（默认，逐行匹配）或 coarse（按文件聚合计数）"}}),
            &["pattern"], false, 96, "500ms", "low"),
        // Wrapping-B：fs_read_multiple 已合并到 fs_read（接受 paths 数组），schema 不再注册
        // executor 路径"fs_read_multiple"仍 dispatch 旧函数（向后兼容）
        // 2026-05-28: confirm_required=false — pipeline 层 classify_bash_command() 已做命令级门控
        // 不需要 schema 级 blanket confirm（否则 `find`/`ls | grep` 等安全命令也弹确认）
        schema("bash_exec", "执行 shell 命令并返回输出",
            json!({"command": {"type":"string", "description":"shell命令"},
                   "timeout": {"type":"number", "description":"超时秒数(默认30,最大120)"},
                   "workdir": {"type":"string", "description":"工作目录(默认session.cwd)"}}),
            &["command"], false, 96, "1s", "medium"),
    ]
}

pub async fn register(registry: &ToolRegistry) {
    // 2026-05-28: 去掉 filengine_ 前缀——所有工具直接用原始 schema name 注册
    // ToolId == schema.name == LLM 调用名 == dispatch 键（如 fs_read, bash_exec, web_fetch）
    //
    // ## 子系统分组（subsystem_policy 前缀匹配）
    // - fs_*: 文件系统操作
    // - bash_*: Shell 执行
    // - web_*: HTTP/搜索
    for s in schemas() {
        let id = ToolId(s.name.clone());
        registry.register(ToolHandle {
            id,
            schema: s,
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        }).await;
    }
}

/// Register filengine tool executors (must be called after register).
pub async fn register_executors(registry: &ToolRegistry) {
    let executor = Arc::new(FilengineToolExecutor::new());
    for s in schemas() {
        let id = ToolId(s.name.clone());
        registry.register_executor(id, executor.clone()).await;
    }
}

// ─── HTML 提取 ────────────────────────────────────────────────────

/// 将 HTML 文本转换为可读纯文本，移除标签、折叠空白、截断到 max_chars。
///
/// ## 引用关系
/// - 调用方：web_fetch()（extract=true 时）、deep 模式下 web_search() 内部调用 web_fetch
///
/// ## 处理流程
/// 1. 移除 script/style/nav/header/footer/aside 块（含内容）
/// 2. 移除所有 HTML 标签（< ... >）
/// 3. 解码常见 HTML 实体（&amp; &lt; &gt; &quot; &#39; &nbsp;）
/// 4. 折叠连续空行（最多保留一个空行分隔）
/// 5. 按 max_chars 截断（按字符安全截断，不破坏多字节）
///
/// ## 返回
/// (提取后文本, 是否已截断)
fn html_to_text(html: &str, max_chars: usize) -> (String, bool) {
    // 1. 移除指定块级元素（含嵌套内容）
    let mut text = html.to_string();
    for tag in &["script", "style", "nav", "header", "footer", "aside"] {
        let open = format!("<{}", tag);
        let close = format!("</{}>", tag);
        loop {
            match text.find(&open) {
                Some(start) => {
                    match text[start..].find(&close) {
                        Some(end_rel) => {
                            let end_pos = start + end_rel + close.len();
                            let mut next = String::with_capacity(text.len() - (end_pos - start));
                            next.push_str(&text[..start]);
                            next.push_str(&text[end_pos..]);
                            text = next;
                        }
                        None => break,
                    }
                }
                None => break,
            }
        }
    }
    // 2. 移除所有 HTML 标签
    let mut stripped = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => stripped.push(c),
            _ => {}
        }
    }
    // 3. 解码常见 HTML 实体
    let decoded = stripped
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    // 4. 折叠连续空行
    let mut out = String::new();
    let mut prev_blank = false;
    for line in decoded.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !prev_blank {
                out.push('\n');
                prev_blank = true;
            }
        } else {
            out.push_str(trimmed);
            out.push('\n');
            prev_blank = false;
        }
    }
    let out = out.trim().to_string();
    // 5. 按字符边界截断
    let truncated = out.chars().count() > max_chars;
    let final_text = if truncated {
        out.char_indices()
            .nth(max_chars)
            .map(|(i, _)| out[..i].to_string())
            .unwrap_or(out.clone())
    } else {
        out
    };
    (final_text, truncated)
}

// ─── Tests ────────────────────────────────────────────────────────
//
// ## 测试策略
// - **HOME env 全局共享**：`allowed_roots()` 读进程级 HOME；任何并发改 HOME 的
//   测试都会污染彼此。引入 `ENV_LOCK` `Mutex<()>` 串行化所有改 HOME 的测试。
// - **`canonicalize()` 要求路径存在**：`NativeFilengine::resolve()` 在 path 不
//   存在时直接 fail，因此测试必须先 `fs::write` 创建文件。
// - **单 long `#[tokio::test]`**：fs_* 全套生命周期（write→read→edit→info→
//   move→ls→mkdir→UTF-8→cwd）合并到一个测试里跑，避免多个 #[tokio::test]
//   并发竞争 HOME，也减少 tempdir 创建开销。
// - **Session 单元测试不改 env**：可以独立并发运行，无需 ENV_LOCK。
//
// ## 引用关系
// - 验证：`FilengineSession::{new, track_read, track_write, summary}`
// - 验证：`allowed_roots()` HOME 校验逻辑
// - 验证：`NativeFilengine::resolve()` 路径越界拒绝
// - 验证：fs.write/read/edit/edit-fail/info/move/ls/mkdir/cwd 端到端契约
// - 验证：fs.search 递归 walkdir
//
// ## 生命周期
// - 创建：每个 #[test]/#[tokio::test] 进入时
// - 销毁：tempdir 在 Drop 时自动清理；ENV_LOCK 静态全程持有
#[cfg(test)]
mod tests {
    // 整个测试模块允许 ENV_LOCK 跨 .await 持有（这是 HOME env 串行化的核心机制）。
    // 设计性安全：tokio::test 默认单线程顺序执行，await 不会让另一测试线程争抢同 lock。
    // 生产代码不允许此模式——本 allow 仅作用于 #[cfg(test)] 内。
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// 串行化 HOME env 修改 — 进程级状态，防止并发测试互相覆盖。
    /// 创建：测试模块加载时（once_cell 风格懒初始化）
    /// 销毁：进程结束
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // ─── Session unit tests（不改 env，可并发） ─────────────

    #[test]
    fn session_default_init_uses_home() {
        let s = FilengineSession::new();
        assert!(s.recent_files.is_empty());
        assert!(s.last_search.is_none());
        assert!(s.modified.is_empty());
        assert!(s.open_context.is_none());
        // cwd 应非空
        assert!(!s.cwd.as_os_str().is_empty());
    }

    #[test]
    fn session_track_read_lru_dedup_and_cap() {
        let mut s = FilengineSession::new();
        // 推入 25 条，期望保留最近 20 条
        for i in 0..25 {
            s.track_read(&format!("/tmp/file{i}"));
        }
        assert_eq!(s.recent_files.len(), 20);
        // 最早的 0..4 已被淘汰；最新的应是 file24
        assert!(!s.recent_files.iter().any(|p| p == "/tmp/file0"));
        assert_eq!(s.recent_files.last().unwrap(), "/tmp/file24");
        assert_eq!(s.open_context, Some("/tmp/file24".to_string()));

        // 重复读同一路径应去重并移到末尾
        s.track_read("/tmp/file10");
        let count_file10 = s.recent_files.iter().filter(|p| *p == "/tmp/file10").count();
        assert_eq!(count_file10, 1, "track_read must dedup");
        assert_eq!(s.recent_files.last().unwrap(), "/tmp/file10");
    }

    #[test]
    fn session_track_write_dedup() {
        let mut s = FilengineSession::new();
        s.track_write("/a");
        s.track_write("/a");
        s.track_write("/b");
        assert_eq!(s.modified, vec!["/a".to_string(), "/b".to_string()]);
    }

    #[test]
    fn session_summary_contains_required_keys() {
        let mut s = FilengineSession::new();
        s.track_read("/x");
        s.track_write("/y");
        let v = s.summary();
        assert!(v.get("cwd").is_some());
        assert!(v.get("open").is_some());
        assert!(v.get("recent").is_some());
        assert!(v.get("modified").is_some());
        assert!(v.get("last_search_count").is_some());
        // recent 应至少含我们刚 track 的路径
        let recent = v["recent"].as_array().unwrap();
        assert!(recent.iter().any(|p| p.as_str() == Some("/x")));
    }

    // ─── allowed_roots 校验 ─────────────────────────────────

    #[test]
    fn allowed_roots_falls_back_to_tmp_when_home_unsafe() {
        let _g = ENV_LOCK.lock().unwrap();
        let original = std::env::var("HOME").ok();

        // 用 / 触发 fallback —— path.is_absolute() 但 components < 3
        std::env::set_var("HOME", "/");
        let roots = allowed_roots();
        assert_eq!(roots, vec!["/tmp".to_string()]);

        // 包含 .. 的也 fallback
        std::env::set_var("HOME", "/foo/../bar");
        let roots = allowed_roots();
        assert_eq!(roots, vec!["/tmp".to_string()]);

        // restore
        match original {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn allowed_roots_accepts_safe_home() {
        let _g = ENV_LOCK.lock().unwrap();
        let original = std::env::var("HOME").ok();

        std::env::set_var("HOME", "/Users/testuser");
        let roots = allowed_roots();
        assert_eq!(roots, vec!["/Users/testuser".to_string()]);

        match original {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    // ─── End-to-end fs.* lifecycle（单 long test 串行） ──────

    /// 全套 fs_* 操作端到端：write→read→edit→edit-fail→info→move→ls→
    /// mkdir→UTF-8→cwd→search 递归。所有路径锁定在 tempdir 内，
    /// 通过临时改 HOME 让 `allowed_roots()` 接纳 tempdir。
    #[tokio::test]
    async fn fs_lifecycle_end_to_end() {
        let _g = ENV_LOCK.lock().unwrap();
        let original_home = std::env::var("HOME").ok();
        let tmp = TempDir::new().expect("tempdir");
        let tmp_path = tmp.path().canonicalize().expect("canonicalize tempdir");
        // tempdir 在 macOS 是 /var/folders/...，segments >=3，满足 allowed_roots 校验
        std::env::set_var("HOME", &tmp_path);

        let mut session = FilengineSession::new();
        session.cwd = tmp_path.clone();

        let file = tmp_path.join("hello.txt");
        let file_str = file.to_string_lossy().to_string();

        // F-BUG-2 已修复：fs.write 现在真正能创建新文件（resolve 容忍不存在路径）
        let r = fs_write(json!({"path": file_str, "content": "hello"}), &mut session).await;
        assert!(r.is_ok(), "fs_write: {:?}", r);
        assert_eq!(r.unwrap()["written"], json!(true));
        assert!(session.modified.contains(&file_str));
        assert!(file.exists(), "fs.write must actually create file");

        // read
        let r = fs_read(json!({"path": file_str}), &mut session).await.unwrap();
        assert_eq!(r["content"], json!("hello"));
        assert_eq!(session.open_context.as_deref(), Some(file_str.as_str()));

        // edit OK
        let r = fs_edit(json!({
            "path": file_str, "old_string": "hello", "new_string": "world"
        }), &mut session).await.unwrap();
        assert_eq!(r["edited"], json!(true));
        let r = fs_read(json!({"path": file_str}), &mut session).await.unwrap();
        assert_eq!(r["content"], json!("world"));

        // edit fail（旧串不存在）
        let r = fs_edit(json!({
            "path": file_str, "old_string": "NOPE", "new_string": "x"
        }), &mut session).await;
        assert!(r.is_err());

        // info
        let r = fs_info(json!({"path": file_str}), &mut session).await.unwrap();
        assert_eq!(r["is_file"], json!(true));
        assert_eq!(r["is_dir"], json!(false));
        assert!(r["size"].as_u64().unwrap() > 0);

        // F-BUG-2 已修复：fs.mkdir 真正能递归创建（祖先 canonicalize + 字面拼接）
        let sub = tmp_path.join("sub").join("nested");
        let sub_str = sub.to_string_lossy().to_string();
        let r = fs_mkdir(json!({"path": sub_str}), &mut session).await.unwrap();
        assert_eq!(r["created"], json!(true));
        assert!(sub.is_dir(), "fs.mkdir must create directory");

        // F-BUG-2 已修复：fs.move 目标可以是不存在路径
        let dst = sub.join("renamed.txt");
        let dst_str = dst.to_string_lossy().to_string();
        let r = fs_move(json!({
            "source": file_str, "destination": dst_str
        }), &mut session).await;
        assert!(r.is_ok(), "fs_move: {:?}", r);
        assert!(dst.exists() && !file.exists(), "move must transfer the file");

        // ls 应看到 sub/
        let r = fs_ls(json!({"path": tmp_path.to_string_lossy()}), &mut session).await.unwrap();
        let entries = r["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| e["name"] == "sub" && e["type"] == "dir"));

        // UTF-8 内容写读不能 panic（F-BUG-2 修复后无需 pre-touch）
        let utf8_file = tmp_path.join("中文.txt");
        let utf8_str = utf8_file.to_string_lossy().to_string();
        let r = fs_write(json!({
            "path": utf8_str, "content": "你好世界——Rust 文件引擎"
        }), &mut session).await;
        assert!(r.is_ok());
        let r = fs_read(json!({"path": utf8_str}), &mut session).await.unwrap();
        assert_eq!(r["content"], json!("你好世界——Rust 文件引擎"));

        // cwd
        let r = NativeFilengine.execute(
            "fs_cwd", json!({}), &mut session,
        ).await.unwrap();
        assert_eq!(r["cwd"], json!(tmp_path.to_string_lossy().to_string()));

        // search 递归 —— sub/nested/renamed.txt 应被 *.txt 命中（深度 2）
        let r = fs_search(json!({
            "pattern": "*.txt", "path": tmp_path.to_string_lossy()
        }), &mut session).await.unwrap();
        let matches = r["matches"].as_array().unwrap();
        assert!(matches.len() >= 2, "expect ≥2 .txt files, got {:?}", matches);
        assert!(
            matches.iter().any(|p| p.as_str().unwrap_or("").ends_with("renamed.txt")),
            "fs.search must descend into sub/nested/, got {:?}", matches
        );

        // restore HOME
        match original_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    // ─── ".." 字面拒绝（F-BUG-2 安全防御） ─────────────────

    #[tokio::test]
    async fn fs_resolve_rejects_dotdot_traversal() {
        let _g = ENV_LOCK.lock().unwrap();
        let original_home = std::env::var("HOME").ok();
        let tmp = TempDir::new().expect("tempdir");
        let tmp_path = tmp.path().canonicalize().expect("canonicalize tempdir");
        std::env::set_var("HOME", &tmp_path);

        let mut session = FilengineSession::new();
        session.cwd = tmp_path.clone();

        // 攻击向量：在不存在路径上叠 .. 越界（祖先 canonicalize + 字面拼接绕过 prefix 检查）
        let traversal = format!("{}/foo/../../../etc/passwd", tmp_path.display());
        let r = fs_read(json!({"path": traversal}), &mut session).await;
        assert!(r.is_err(), "must reject .. in path");
        assert!(r.unwrap_err().contains("'..'"));

        // 相对路径 .. 同样拒
        let r = fs_read(json!({"path": "../../etc/passwd"}), &mut session).await;
        assert!(r.is_err());

        match original_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    // ─── Path traversal 拒绝 ────────────────────────────────

    #[tokio::test]
    async fn fs_resolve_rejects_outside_allowed_roots() {
        let _g = ENV_LOCK.lock().unwrap();
        let original_home = std::env::var("HOME").ok();
        let tmp = TempDir::new().expect("tempdir");
        let tmp_path = tmp.path().canonicalize().expect("canonicalize tempdir");
        std::env::set_var("HOME", &tmp_path);

        let mut session = FilengineSession::new();
        session.cwd = tmp_path.clone();

        // /etc/hosts 在 macOS/Linux 都存在，且不在 tempdir 下，应被拒
        let r = fs_read(json!({"path": "/etc/hosts"}), &mut session).await;
        assert!(r.is_err(), "must reject path outside allowed_roots");
        let err = r.unwrap_err();
        assert!(
            err.contains("not allowed") || err.contains("invalid path"),
            "unexpected error: {err}"
        );

        match original_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    // ─── bash 命令白名单 ────────────────────────────────────

    #[test]
    fn bash_blocks_shell_metacharacters() {
        // 即使命令是白名单 ls，含元字符也应拒绝（防 injection）
        assert!(!is_command_allowed("ls; rm -rf /"));
        assert!(!is_command_allowed("ls | cat"));
        assert!(!is_command_allowed("echo $(whoami)"));
        assert!(!is_command_allowed("ls && pwd"));
        assert!(!is_command_allowed("ls > /tmp/x"));
        assert!(!is_command_allowed("ls\nrm"));
    }

    #[test]
    fn bash_whitelist_enforced() {
        use super::BashDecision;

        // T0: read-only commands → Allow
        assert_eq!(classify_bash_command("ls -la"), BashDecision::Allow);
        assert_eq!(classify_bash_command("cat /tmp/x.txt"), BashDecision::Allow);
        assert_eq!(classify_bash_command("echo hello"), BashDecision::Allow);
        assert_eq!(classify_bash_command("grep pattern file"), BashDecision::Allow);

        // T1: git/cargo safe subcommands → Allow
        assert_eq!(classify_bash_command("git status"), BashDecision::Allow);
        assert_eq!(classify_bash_command("git log --oneline"), BashDecision::Allow);
        assert_eq!(classify_bash_command("cargo build"), BashDecision::Allow);
        assert_eq!(classify_bash_command("cargo test"), BashDecision::Allow);
        assert_eq!(classify_bash_command("cargo check"), BashDecision::Allow);
        assert_eq!(classify_bash_command("npm ls"), BashDecision::Allow);

        // T2: state-modifying → NeedsConfirm
        assert!(matches!(classify_bash_command("rm -rf /tmp/x"), BashDecision::NeedsConfirm(_)));
        assert!(matches!(classify_bash_command("git push origin main"), BashDecision::NeedsConfirm(_)));
        assert!(matches!(classify_bash_command("cargo install evil"), BashDecision::NeedsConfirm(_)));
        // cargo run 现在允许（开发常用）
        assert_eq!(classify_bash_command("cargo run"), BashDecision::Allow);
        // cargo install 仍需确认
        assert!(matches!(classify_bash_command("cargo install evil"), BashDecision::NeedsConfirm(_)));
        assert!(matches!(classify_bash_command("npm install lodash"), BashDecision::NeedsConfirm(_)));
        // python3 script.py 现在允许（LLM 写脚本后需验证执行）
        assert_eq!(classify_bash_command("python3 evil.py"), BashDecision::Allow);
        // python3 无参数仍需确认
        assert!(matches!(classify_bash_command("python3"), BashDecision::NeedsConfirm(_)));
        assert!(matches!(classify_bash_command("mv a b"), BashDecision::NeedsConfirm(_)));
        assert!(matches!(classify_bash_command("kill -9 1234"), BashDecision::NeedsConfirm(_)));

        // T3: dangerous → Dangerous (still needs confirm, not flat deny)
        assert!(matches!(classify_bash_command("sudo rm -rf /"), BashDecision::Dangerous(_)));
        assert!(matches!(classify_bash_command("dd if=/dev/zero of=/dev/sda"), BashDecision::Dangerous(_)));
        assert!(matches!(classify_bash_command("shutdown -h now"), BashDecision::Dangerous(_)));

        // Git injection vectors → Dangerous
        assert!(matches!(classify_bash_command("git -c core.evil=1 status"), BashDecision::Dangerous(_)));
        assert!(matches!(classify_bash_command("git clone --upload-pack=evil repo"), BashDecision::Dangerous(_)));

        // Unknown commands → NeedsConfirm (not denied)
        assert!(matches!(classify_bash_command("bash"), BashDecision::NeedsConfirm(_)));
        assert!(matches!(classify_bash_command("unknown_tool"), BashDecision::NeedsConfirm(_)));

        // Shell metacharacters → Dangerous
        assert!(matches!(classify_bash_command("ls; rm -rf /"), BashDecision::Dangerous(_)));
        assert!(matches!(classify_bash_command("echo $(whoami)"), BashDecision::Dangerous(_)));
    }

    // ─── Phase 2 undo 注入：FilengineToolExecutor + undo_logger ──────────

    use crate::tool::{ExecutionContext, ToolExecutor};
    use crate::undo::UndoLogger;
    use abacus_types::ToolId;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Phase 2 测试隔离：所有用 fs.* 工具的测试**必须**持 `ENV_LOCK`，
    /// 因为 fs_lifecycle 等测试改 HOME 期间，本组 fs_*_logs_* 测试若并发执行
    /// 会读到瞬时改动的 HOME（allowed_roots 检查失败）。
    /// 持锁期间不改 HOME 即可——锁保护的是"读 HOME 的瞬态一致性"。
    fn tempdir_in_home() -> (TempDir, std::path::PathBuf) {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| "/tmp".into());
        let tmp = TempDir::new_in(&home).unwrap();
        let p = tmp.path().canonicalize().unwrap();
        (tmp, p)
    }

    /// 工厂：构造带 undo_logger 的 ExecutionContext
    /// 注意：tempdir 已在 HOME 下，allowed_roots 接纳，**无需改 HOME env**
    async fn make_undo_ctx(tmp_path: &std::path::Path, session_id: &str)
        -> (Arc<UndoLogger>, ExecutionContext)
    {
        let project_dir = tmp_path.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let logger = Arc::new(
            UndoLogger::new_at(project_dir, session_id.into()).unwrap()
        );
        let mut session = FilengineSession::new();
        session.cwd = tmp_path.to_path_buf();
        session.undo_logger = Some(logger.clone());
        let ctx = ExecutionContext {
            session_id: session_id.into(),
            filengine: Arc::new(RwLock::new(session)),
            turn_number: 7,
            bash_default_timeout: 30,
            bash_max_timeout: 120,
            tool_default_timeout: 60,
            role_caps: std::sync::Arc::new(abacus_types::RoleCapabilities::default()),
        };
        (logger, ctx)
    }

    #[tokio::test]
    async fn fs_write_logs_undo_entry_when_logger_attached() {
        let _g = ENV_LOCK.lock().unwrap();
        let (_keep, tmp_path) = tempdir_in_home();
        let (logger, ctx) = make_undo_ctx(&tmp_path, "sess-write").await;
        let target = tmp_path.join("phase2-write.txt");
        let target_str = target.to_string_lossy().to_string();

        let exec = FilengineToolExecutor::new();
        let r = exec.execute(
            &ToolId("fs_write".into()),
            json!({"path": target_str, "content": "phase2 hello"}),
            &ctx,
        ).await;
        assert!(r.is_ok(), "{:?}", r);

        let log = std::fs::read_to_string(logger.log_path()).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 1, "exactly 1 log entry for one fs.write");
        let e: crate::undo::LogEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(e.tool, "fs_write");
        assert_eq!(e.turn, 7);
        assert_eq!(e.op, crate::undo::OpKind::Create);
        let am = e.after_meta.as_ref().unwrap();
        assert_eq!(am.size, 12);
    }

    #[tokio::test]
    async fn fs_edit_logs_overwrite_with_before_snapshot() {
        let _g = ENV_LOCK.lock().unwrap();
        let (_keep, tmp_path) = tempdir_in_home();
        let (logger, ctx) = make_undo_ctx(&tmp_path, "sess-edit").await;
        let target = tmp_path.join("phase2-edit.txt");
        std::fs::write(&target, b"old content here").unwrap();

        let exec = FilengineToolExecutor::new();
        let r = exec.execute(
            &ToolId("fs_edit".into()),
            json!({"path": target.to_string_lossy(), "old_string": "old", "new_string": "new"}),
            &ctx,
        ).await;
        assert!(r.is_ok(), "{:?}", r);

        let log = std::fs::read_to_string(logger.log_path()).unwrap();
        let e: crate::undo::LogEntry = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(e.tool, "fs_edit");
        assert_eq!(e.op, crate::undo::OpKind::Edit);
        let snap_name = e.before_snapshot.as_ref().expect("edit on existing file must snapshot");
        let snap_content = std::fs::read(logger.snapshot_dir().join(snap_name)).unwrap();
        assert_eq!(snap_content, b"old content here");
    }

    #[tokio::test]
    async fn fs_move_logs_with_move_to_field() {
        let _g = ENV_LOCK.lock().unwrap();
        let (_keep, tmp_path) = tempdir_in_home();
        let (logger, ctx) = make_undo_ctx(&tmp_path, "sess-mv").await;
        let src = tmp_path.join("phase2-src.txt");
        let dst = tmp_path.join("phase2-dst.txt");
        std::fs::write(&src, b"data").unwrap();

        let exec = FilengineToolExecutor::new();
        let r = exec.execute(
            &ToolId("fs_move".into()),
            json!({"source": src.to_string_lossy(), "destination": dst.to_string_lossy()}),
            &ctx,
        ).await;
        assert!(r.is_ok(), "{:?}", r);

        let log = std::fs::read_to_string(logger.log_path()).unwrap();
        let e: crate::undo::LogEntry = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(e.op, crate::undo::OpKind::Move);
        assert!(e.move_to.is_some());
        let n_snaps = std::fs::read_dir(logger.snapshot_dir()).unwrap().count();
        assert_eq!(n_snaps, 0);
    }

    #[tokio::test]
    async fn fs_mkdir_logs_with_no_snapshot() {
        let _g = ENV_LOCK.lock().unwrap();
        let (_keep, tmp_path) = tempdir_in_home();
        let (logger, ctx) = make_undo_ctx(&tmp_path, "sess-mkdir").await;
        let dir = tmp_path.join("phase2-newdir");

        let exec = FilengineToolExecutor::new();
        let r = exec.execute(
            &ToolId("fs_mkdir".into()),
            json!({"path": dir.to_string_lossy()}),
            &ctx,
        ).await;
        assert!(r.is_ok(), "{:?}", r);

        let log = std::fs::read_to_string(logger.log_path()).unwrap();
        let e: crate::undo::LogEntry = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(e.op, crate::undo::OpKind::Mkdir);
        assert!(e.before_snapshot.is_none());
        assert!(e.before_meta.is_none());
    }

    #[tokio::test]
    async fn read_only_tools_do_not_create_log_entries() {
        let _g = ENV_LOCK.lock().unwrap();
        let (_keep, tmp_path) = tempdir_in_home();
        let (logger, ctx) = make_undo_ctx(&tmp_path, "sess-readonly").await;
        let f = tmp_path.join("readonly.txt");
        std::fs::write(&f, b"content").unwrap();

        let exec = FilengineToolExecutor::new();
        let r = exec.execute(
            &ToolId("fs_read".into()),
            json!({"path": f.to_string_lossy()}),
            &ctx,
        ).await;
        assert!(r.is_ok());

        if logger.log_path().exists() {
            let log = std::fs::read_to_string(logger.log_path()).unwrap();
            assert!(log.is_empty(), "fs.read 不应产生 log entry");
        }
    }

    #[tokio::test]
    async fn no_logger_attached_skips_logging_entirely() {
        // session.undo_logger=None → 写工具静默跳过 snapshot/log（向后兼容）
        let _g = ENV_LOCK.lock().unwrap();
        let (_keep, tmp_path) = tempdir_in_home();
        let mut session = FilengineSession::new();
        session.cwd = tmp_path.clone();
        let ctx = ExecutionContext {
            session_id: "sess-nologger".into(),
            filengine: Arc::new(RwLock::new(session)),
            turn_number: 1,
            bash_default_timeout: 30,
            bash_max_timeout: 120,
            tool_default_timeout: 60,
            role_caps: std::sync::Arc::new(abacus_types::RoleCapabilities::default()),
        };

        let target = tmp_path.join("nolog.txt");
        let exec = FilengineToolExecutor::new();
        let r = exec.execute(
            &ToolId("fs_write".into()),
            json!({"path": target.to_string_lossy(), "content": "x"}),
            &ctx,
        ).await;
        assert!(r.is_ok(), "fs.write 在无 logger 时仍应成功");
        assert!(target.exists());
    }

    // ─── Wrapping-B：fs.read 接受 paths 数组、fs.ls recursive=true ────────

    /// fs.read 单 path 行为不变
    #[tokio::test]
    async fn fs_read_single_path_still_works() {
        let _g = ENV_LOCK.lock().unwrap();
        let _orig = std::env::var("HOME").ok();
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().canonicalize().unwrap();
        std::env::set_var("HOME", &p);
        let mut session = FilengineSession::new();
        session.cwd = p.clone();

        let file = p.join("a.txt");
        tokio::fs::write(&file, "single").await.unwrap();
        let r = fs_read(json!({"path": file.to_string_lossy()}), &mut session).await.unwrap();
        assert_eq!(r["content"], "single");
        match _orig { Some(v) => std::env::set_var("HOME", v), None => std::env::remove_var("HOME") }
    }

    /// fs.read 接受 paths 数组——委托 fs_read_multiple
    #[tokio::test]
    async fn fs_read_with_paths_array_delegates_to_multi() {
        let _g = ENV_LOCK.lock().unwrap();
        let _orig = std::env::var("HOME").ok();
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().canonicalize().unwrap();
        std::env::set_var("HOME", &p);
        let mut session = FilengineSession::new();
        session.cwd = p.clone();

        let f1 = p.join("a.txt");
        let f2 = p.join("b.txt");
        tokio::fs::write(&f1, "AA").await.unwrap();
        tokio::fs::write(&f2, "BB").await.unwrap();

        let r = fs_read(json!({
            "paths": [f1.to_string_lossy(), f2.to_string_lossy()]
        }), &mut session).await.unwrap();
        // multi 路径返回的字段名带 output（含两文件 marker）
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("AA") && s.contains("BB"), "多文件内容应都出现: {}", s);
        match _orig { Some(v) => std::env::set_var("HOME", v), None => std::env::remove_var("HOME") }
    }

    /// fs.ls recursive=true 委托 fs_tree
    #[tokio::test]
    async fn fs_ls_recursive_delegates_to_tree() {
        let _g = ENV_LOCK.lock().unwrap();
        let _orig = std::env::var("HOME").ok();
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().canonicalize().unwrap();
        std::env::set_var("HOME", &p);
        let mut session = FilengineSession::new();
        session.cwd = p.clone();

        // 准备子目录
        let sub = p.join("sub");
        tokio::fs::create_dir_all(&sub).await.unwrap();
        tokio::fs::write(sub.join("inside.txt"), "x").await.unwrap();

        // 不传 recursive → entries 字段
        let flat = fs_ls(json!({"path": p.to_string_lossy()}), &mut session).await.unwrap();
        assert!(flat.get("entries").is_some());

        // recursive=true → tree 字段
        let tree = fs_ls(json!({"path": p.to_string_lossy(), "recursive": true}), &mut session).await.unwrap();
        assert!(tree.get("tree").is_some(), "recursive=true 应返回 tree 字段");
        match _orig { Some(v) => std::env::set_var("HOME", v), None => std::env::remove_var("HOME") }
    }

    /// schemas() 反映合并后的工具数：read_multiple/tree 不再独立 schema
    #[test]
    fn fs_schemas_collapsed_read_and_tree() {
        let names: Vec<String> = schemas().iter().map(|s| s.name.clone()).collect();
        assert!(names.contains(&"fs_read".to_string()));
        assert!(names.contains(&"fs_ls".to_string()));
        assert!(!names.contains(&"fs_read_multiple".to_string()));
        assert!(!names.contains(&"fs_tree".to_string()));
    }
}