//! LSP JSON-RPC 传输层与生命周期管理
//!
//! ## 协议
//! Language Server Protocol 2.0 via stdio:
//!   Content-Length: <n>\r\n\r\n<json body>
//!
//! ## 生命周期
//! new() → initialize() → work（多次 request）→ drop（shutdown + exit）
//!
//! ## 引用关系
//! - 被 `LspManager` 持有（per-workspace per-language 实例）
//! - 被 `LspToolExecutor` 调用以执行 goto_definition 等操作
//!
//! ## 故障处理
//! - 请求超时（默认 10s）→ 返回 Err
//! - Server 进程退出 → 后续请求返回 Err，LspManager 负责重启

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, Mutex};
use tokio::time::{timeout, Duration};

/// 文件同步状态——供 `did_open_or_change_file` 决定发送哪种通知
pub enum FileStateUpdate {
    /// 首次打开，需发送 `textDocument/didOpen`
    FirstOpen,
    /// 内容已变更，需发送 `textDocument/didChange`，附当前版本号
    Changed { version: u32 },
    /// 内容未变化，跳过通知
    Unchanged,
}

fn content_hash(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

const REQUEST_TIMEOUT_SECS: u64 = 10;

/// 检查语言服务器是否可用（在 PATH 中存在）
/// 用于注册前的可用性验证，避免注册无法执行的工具
pub async fn is_server_available(command: &str) -> bool {
    tokio::process::Command::new(command)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .map(|mut child| {
            tokio::spawn(async move { let _ = child.wait().await; });
            true
        })
        .unwrap_or(false)
}

/// 单个 LSP 语言服务器连接
pub struct LspClient {
    /// JSON-RPC 请求 id 递增器
    next_id: AtomicU64,
    /// stdin 写入端（发送请求）
    stdin: Arc<Mutex<ChildStdin>>,
    /// 待处理请求：id → 响应 channel
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    /// 子进程句柄（保持进程存活）
    _child: Arc<Mutex<Child>>,
    /// 是否已完成 initialize 握手
    initialized: Arc<std::sync::atomic::AtomicBool>,
    /// 已打开文件的版本号和内容 hash
    ///
    /// ## 设计
    /// key = file_path，value = (version, content_hash)
    /// - 版本号从 1 开始，每次 `textDocument/didChange` 单调递增
    /// - content_hash 用于检测文件是否变化（变化才发 didChange）
    /// - 首次打开：插入 (1, hash)，发 `textDocument/didOpen`
    /// - 再次访问且 hash 不同：version+1，发 `textDocument/didChange`
    /// - 再次访问且 hash 相同：跳过通知
    file_states: Mutex<HashMap<String, (u32, u64)>>,
    /// 服务器推送的诊断缓存（textDocument/publishDiagnostics 通知）
    ///
    /// ## 生命周期
    /// - 后台 reader task 写入（每次推送覆盖）
    /// - `get_push_diagnostics()` 读取（LspManager.diagnostics() 优先检查）
    push_diagnostics: Arc<Mutex<HashMap<String, Vec<Value>>>>,
}

impl LspClient {
    /// 启动语言服务器并完成 LSP initialize 握手
    ///
    /// ## 参数
    /// - `command`: 服务器可执行文件（如 "rust-analyzer", "typescript-language-server"）
    /// - `args`: 启动参数（如 `["--stdio"]`）
    /// - `workspace_root`: 工作区根路径（作为 rootUri 传给服务器）
    pub async fn start(
        command: &str,
        args: &[&str],
        workspace_root: &str,
    ) -> Result<Self, String> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()) // 丢弃 stderr 噪音
            .spawn()
            .map_err(|e| format!("failed to start {command}: {e}"))?;

        let stdin = child.stdin.take()
            .ok_or("failed to get stdin")?;
        let stdout = child.stdout.take()
            .ok_or("failed to get stdout")?;

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let push_diagnostics: Arc<Mutex<HashMap<String, Vec<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // 启动后台读取任务：解析响应并分发到 pending channels。
        // 同时处理 publishDiagnostics 推送通知——缓存到 push_diagnostics。
        {
            let pending_clone = pending.clone();
            let push_diags_clone = push_diagnostics.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                while let Ok(msg) = read_message(&mut reader).await {
                        // ↓ 原 match 分支保留，仅去掉 outer match 包装让 clippy::while_let_loop 通过
                        {
                            if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                                // 请求响应：分发到对应 channel
                                let mut p = pending_clone.lock().await;
                                if let Some(tx) = p.remove(&id) {
                                    let _ = tx.send(msg);
                                }
                            } else if let Some(method) = msg.get("method").and_then(|v| v.as_str()) {
                                // 服务器推送通知 — 目前处理 publishDiagnostics
                                if method == "textDocument/publishDiagnostics" {
                                    if let Some(params) = msg.get("params") {
                                        let uri = params.get("uri")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string());
                                        let diags = params.get("diagnostics")
                                            .and_then(|v| v.as_array())
                                            .cloned()
                                            .unwrap_or_default();
                                        if let Some(uri) = uri {
                                            let file_path = crate::lsp::client::uri_to_path(&uri);
                                            push_diags_clone.lock().await
                                                .insert(file_path, diags);
                                        }
                                    }
                                }
                                // 其他通知（如 window/logMessage）静默忽略
                            }
                        }
                }
                // 走出 while-let 即说明 read_message 返回 Err（服务器退出）
            });
        }

        let client = Self {
            next_id: AtomicU64::new(1),
            stdin,
            pending,
            _child: Arc::new(Mutex::new(child)),
            initialized,
            file_states: Mutex::new(HashMap::new()),
            push_diagnostics,
        };

        // LSP 初始化握手
        client.initialize(workspace_root).await?;
        Ok(client)
    }

    /// 检查并更新文件同步状态。
    ///
    /// 调用方必须传入文件的当前内容，此方法会根据内容变化返回同步策略：
    /// - `FirstOpen`：首次访问该文件，调用方发 `textDocument/didOpen`
    /// - `Changed { version }`：内容已变，调用方发 `textDocument/didChange`（带版本号）
    /// - `Unchanged`：内容未变，调用方跳过通知
    pub async fn check_file_state(&self, file_path: &str, content: &str) -> FileStateUpdate {
        let hash = content_hash(content);
        let mut states = self.file_states.lock().await;
        match states.get_mut(file_path) {
            None => {
                states.insert(file_path.to_string(), (1, hash));
                FileStateUpdate::FirstOpen
            }
            Some((version, old_hash)) => {
                if *old_hash == hash {
                    FileStateUpdate::Unchanged
                } else {
                    *version += 1;
                    *old_hash = hash;
                    FileStateUpdate::Changed { version: *version }
                }
            }
        }
    }

    /// 从文件状态表中移除文件（用于 didOpen 发送失败后回滚）
    ///
    /// 保证下次请求时仍能重试发送 `textDocument/didOpen`。
    pub async fn mark_closed(&self, file_path: &str) {
        self.file_states.lock().await.remove(file_path);
    }

    /// 获取指定文件的推送诊断缓存。
    ///
    /// 服务器通过 `textDocument/publishDiagnostics` 通知嵌入的诊断信息。
    /// 返回空数组表示该文件尚未收到诊断推送。
    pub async fn get_push_diagnostics(&self, file_path: &str) -> Vec<Value> {
        self.push_diagnostics.lock().await
            .get(file_path)
            .cloned()
            .unwrap_or_default()
    }

    /// 发送 JSON-RPC 请求，等待响应
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().await;
            p.insert(id, tx);
        }

        // 发送请求
        self.send_raw(&msg).await?;

        // 等待响应（超时保护）
        match timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS), rx).await {
            Ok(Ok(resp)) => {
                if let Some(err) = resp.get("error") {
                    Err(format!("LSP error: {err}"))
                } else {
                    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
                }
            }
            Ok(Err(_)) => Err("response channel dropped".into()),
            Err(_) => {
                // 超时：清理 pending
                self.pending.lock().await.remove(&id);
                Err(format!("LSP request '{method}' timed out after {REQUEST_TIMEOUT_SECS}s"))
            }
        }
    }

    /// 发送通知（无需响应）
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send_raw(&msg).await
    }

    /// LSP initialize → initialized 握手
    async fn initialize(&self, workspace_root: &str) -> Result<(), String> {
        let root_uri = path_to_uri(workspace_root);
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "abacus-lsp-client", "version": "0.1" },
            "rootUri": root_uri,
            "workspaceFolders": [{"uri": root_uri, "name": "workspace"}],
            "capabilities": {
                "textDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "didSave": false,
                        "willSave": false
                    },
                    "definition": { "dynamicRegistration": false },
                    "references": { "dynamicRegistration": false },
                    "hover": {
                        "dynamicRegistration": false,
                        "contentFormat": ["plaintext"]
                    },
                    "documentSymbol": { "dynamicRegistration": false },
                    "publishDiagnostics": {
                        "dynamicRegistration": false,
                        "relatedInformation": false
                    }
                },
                "workspace": {
                    "symbol": { "dynamicRegistration": false }
                }
            }
        });
        self.request("initialize", params).await?;
        self.notify("initialized", json!({})).await?;
        self.initialized.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// 写入 Content-Length 帧
    async fn send_raw(&self, msg: &Value) -> Result<(), String> {
        let body = serde_json::to_string(msg)
            .map_err(|e| format!("serialize error: {e}"))?;
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(frame.as_bytes()).await
            .map_err(|e| format!("write error: {e}"))?;
        stdin.flush().await
            .map_err(|e| format!("flush error: {e}"))
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // 发送 shutdown（best-effort，不等待响应）
        let stdin = self.stdin.clone();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            let shutdown = json!({"jsonrpc":"2.0","id":id,"method":"shutdown","params":null});
            let exit = json!({"jsonrpc":"2.0","method":"exit"});
            let mut w = stdin.lock().await;
            for msg in [&shutdown, &exit] {
                let body = serde_json::to_string(msg).unwrap_or_default();
                let _ = w.write_all(
                    format!("Content-Length: {}\r\n\r\n{}", body.len(), body).as_bytes()
                ).await;
            }
            let _ = w.flush().await;
        });
    }
}

/// 从 stdout 读取一个 JSON-RPC 消息（Content-Length 帧）
async fn read_message(reader: &mut BufReader<ChildStdout>) -> Result<Value, String> {
    // 读取 headers 直到空行
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await
            .map_err(|e| format!("read error: {e}"))?;
        if n == 0 { return Err("EOF".into()); }
        let line = line.trim();
        if line.is_empty() { break; }
        if let Some(val) = line.strip_prefix("Content-Length: ") {
            content_length = val.trim().parse().ok();
        }
    }
    let len = content_length.ok_or("missing Content-Length")?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await
        .map_err(|e| format!("read body error: {e}"))?;
    serde_json::from_slice(&buf).map_err(|e| format!("parse error: {e}"))
}

/// 将文件系统路径转换为 file:// URI
pub fn path_to_uri(path: &str) -> String {
    let abs = if path.starts_with('/') {
        path.to_string()
    } else {
        std::env::current_dir()
            .map(|d| d.join(path).to_string_lossy().to_string())
            .unwrap_or_else(|_| format!("/{path}"))
    };
    format!("file://{abs}")
}

/// 将 file:// URI 还原为文件系统路径
pub fn uri_to_path(uri: &str) -> String {
    uri.strip_prefix("file://").unwrap_or(uri).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_to_uri() {
        assert_eq!(path_to_uri("/foo/bar.rs"), "file:///foo/bar.rs");
    }

    #[test]
    fn test_uri_to_path() {
        assert_eq!(uri_to_path("file:///foo/bar.rs"), "/foo/bar.rs");
        assert_eq!(uri_to_path("/foo/bar.rs"), "/foo/bar.rs");
    }
}
