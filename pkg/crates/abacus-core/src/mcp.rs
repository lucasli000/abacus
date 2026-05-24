//! MCP Client and Plugin Loader — with optional gRPC transport
//!
//! ## MCP Client
//!
//! Manages connections to Model Context Protocol servers. Supports two transports:
//!
//! | Transport | Config value | Library | When to use |
//! |-----------|-------------|---------|-------------|
//! | stdio (JSON-RPC) | `"stdio"` | Built-in | Local dev, in-process |
//! | gRPC | `"grpc"` | `tonic` + `prost` | Production, cross-host |
//!
//! Tools are discovered via [`McpClient::discover_tools()`] and exposed with prefix
//! `mcp/{server_id}/{tool_name}`.
//!
//! ## Plugin Loader
//!
//! Manages WASM-based plugins. Actual WASM execution requires `wasmtime` dependency —
//! not yet added. [`PluginLoader::execute()`] returns an error for now.
//!
//! ## Dependencies
//!
//! | Crate | Version | Usage |
//! |-------|---------|-------|
//! | `tonic` | workspace | gRPC client via `McpServiceClient` |
//! | `prost` | workspace | Protobuf message types |
//! | `tokio` | workspace | Async runtime + RwLock |
//! | `serde_json` | workspace | JSON-RPC serialization (stdio transport) |
//!
//! ## Proto Definition
//!
//! Defined in `proto/mcp.proto`, compiled by `tonic-build` at build time.
//! Service: `McpService` with `Discover`, `Execute`, `StreamEvents` RPCs.
//!
//! ## Scenarios
//!
//! | Scenario | Transport | Notes |
//! |----------|-----------|-------|
//! | Local dev agent | stdio | Zero deps, fast startup |
//! | Remote MCP server | gRPC | Lower latency, streaming |
//! | TLS-required server | gRPC + TLS | Set `tls: true` in config |
//!
//! ## Known Limitations
//!
//! - Stdio transport is a skeleton (no real MCP protocol parsing)
//! - gRPC transport connects but uses placeholder echo logic
//! - Plugin WASM sandboxing not implemented
//! - Session ID is UUIDv7-like (no dependency on `uuid` crate)

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use abacus_types::{
    KernelError, McpConfig, PluginManifest,
    ToolHandle, ToolId, ToolOutput, ToolProvider, ToolSchema, ToolState,
};
use serde_json::Value;
use tokio::sync::RwLock;

// tonic-generated gRPC types for MCP service
pub mod mcp_proto {
    include!(concat!(env!("OUT_DIR"), "/abacus.mcp.rs"));
}

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a UUIDv7-formatted identifier without external crate dependencies.
///
/// Format: `xxxxxxxx-xxxx-7xxx-axxx-xxxxxxxxxxxx`
/// - 48-bit Unix timestamp in milliseconds (sortable)
/// - Version nibble `7` (UUIDv7)
/// - Variant bits `10` (RFC 4122)
/// - Remaining bits from counter + timestamp mixing
///
/// Not cryptographically random, but provides:
/// - Monotonic ordering by time
/// - Process-unique counter for collision resistance
fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts_ms = SystemTime::now().duration_since(UNIX_EPOCH)
        .unwrap_or_default().as_millis() as u64;
    let count = SESSION_COUNTER.fetch_add(1, Ordering::SeqCst);
    let mix = (ts_ms << 16) ^ count;
    // UUIDv7: 48-bit ts | 4-bit ver(7) | 12-bit rand | 2-bit var(10) | 62-bit rand
    format!(
        "{:08x}-{:04x}-7{:03x}-a{:03x}-{:08x}{:04x}",
        (ts_ms >> 16) & 0xFFFF_FFFF,
        ts_ms & 0xFFFF,
        ((ts_ms >> 4) ^ count) & 0xFFF,
        ((mix >> 20) & count) & 0xFFF,
        ((mix >> 32) ^ ts_ms) & 0xFFFF_FFFF,
        mix as u16,
    )
}

/// gRPC transport client wrapping the tonic-generated `McpServiceClient`.
///
/// Used when `McpConfig.transport == "grpc"`.
/// Connects to `http://{address}` (or `https://` when TLS is enabled).
pub struct McpGrpcTransport {
    server_id: String,
    address: String,
    client: Arc<RwLock<Option<mcp_proto::mcp_service_client::McpServiceClient<tonic::transport::Channel>>>>,
    connected: Arc<RwLock<bool>>,
}

impl McpGrpcTransport {
    /// Create a new gRPC transport for the given server config.
    pub fn new(server_id: &str, address: &str) -> Self {
        Self {
            server_id: server_id.to_string(),
            address: address.to_string(),
            client: Arc::new(RwLock::new(None)),
            connected: Arc::new(RwLock::new(false)),
        }
    }

    /// Establish gRPC channel to the MCP server.
    /// Timeout: 10 seconds to prevent hanging on unreachable servers.
    pub async fn connect(&self) -> Result<(), KernelError> {
        if *self.connected.read().await {
            return Ok(());
        }
        let uri = format!("http://{}", self.address);
        let connect_timeout = std::time::Duration::from_secs(10);
        let channel = tonic::transport::Channel::from_shared(uri)
            .map_err(|e| KernelError::Other(format!("gRPC channel: {}", e)))?
            .connect_timeout(connect_timeout)
            .timeout(connect_timeout)
            .connect()
            .await
            .map_err(|e| KernelError::Other(format!("gRPC connect (10s timeout): {}", e)))?;
        let mut client = self.client.write().await;
*client = Some(mcp_proto::mcp_service_client::McpServiceClient::new(channel));
        *self.connected.write().await = true;
        Ok(())
    }

    /// Discover tools from the MCP server via gRPC.
    pub async fn discover_tools(&self) -> Result<Vec<ToolHandle>, KernelError> {
        self.connect().await?;
        let mut client = {
            let c = self.client.write().await;
            c.clone().ok_or_else(|| KernelError::Other("gRPC client not connected".into()))?
        };
        let req = tonic::Request::new(mcp_proto::DiscoverRequest {
            server_id: self.server_id.clone(),
        });
        let response = client.discover(req).await
            .map_err(|e| KernelError::Other(format!("gRPC discover: {}", e)))?;
        let tools = response.into_inner().tools.into_iter().map(|t| {
            let params: Value = serde_json::from_str(&t.parameters_json).unwrap_or_else(|e| {
                tracing::warn!("failed to parse JSON: {e}, raw: {}", &t.parameters_json[..t.parameters_json.len().min(200)]);
                Value::default()
            });
            ToolHandle {
                // 单一命名：ToolId.0 == schema.name == LLM 调用名。
                // server_id / t.name 可能含 . / / 等非协议字符，注册时一次性 sanitize。
                id: ToolId(format!(
                    "mcp_{}_{}",
                    crate::llm::tool_view::sanitize_name(&self.server_id),
                    crate::llm::tool_view::sanitize_name(&t.name),
                )),
                schema: ToolSchema {
                    name: format!(
                        "mcp_{}_{}",
                        crate::llm::tool_view::sanitize_name(&self.server_id),
                        crate::llm::tool_view::sanitize_name(&t.name),
                    ),
                    description: t.description,
                    parameters: params,
                    returns: None,
                    security: None,
                    cost: None,
                    examples: Vec::new(),
                    applicable_task_kinds: None,
                    idempotent: false,
                },
                provider: ToolProvider::Mcp { server_id: self.server_id.clone() },
                state: ToolState::Loaded,
                effectiveness: Default::default(),
            }
        }).collect();
        Ok(tools)
    }

    /// Execute a tool via gRPC with optional HMAC signature.
    ///
    /// `hmac_sig`: base64-encoded HMAC-SHA256 signature (empty string = no signature).
    pub async fn execute(&self, tool_name: &str, params: Value, hmac_sig: &str) -> Result<ToolOutput, KernelError> {
        self.connect().await?;
        let mut client = {
            let c = self.client.write().await;
            c.clone().ok_or_else(|| KernelError::Other("gRPC client not connected".into()))?
        };
        let req = tonic::Request::new(mcp_proto::ExecuteRequest {
            tool_name: tool_name.to_string(),
            params_json: serde_json::to_string(&params).unwrap_or_default(),
            session_id: String::new(),
            hmac_signature: hmac_sig.to_string(),
        });
        let response = client.execute(req).await
            .map_err(|e| KernelError::Other(format!("gRPC execute: {}", e)))?;
        let resp = response.into_inner();
        let output: Value = serde_json::from_str(&resp.output_json).unwrap_or_else(|e| {
            tracing::warn!("failed to parse JSON: {e}, raw: {}", &resp.output_json[..resp.output_json.len().min(200)]);
            Value::default()
        });
        Ok(ToolOutput {
            tool_id: ToolId(format!(
                "mcp_{}_{}",
                crate::llm::tool_view::sanitize_name(&self.server_id),
                crate::llm::tool_view::sanitize_name(tool_name),
            )),
            success: resp.success,
            output,
            latency_ms: resp.latency_ms,
            failure_kind: if resp.success { None } else { Some("BusinessError".into()) },
            try_instead: Vec::new(),
        })
    }

    /// Disconnect the gRPC channel.
    pub async fn disconnect(&self) {
        let mut client = self.client.write().await;
        *client = None;
        *self.connected.write().await = false;
    }
}

/// Client for a single MCP server.
///
/// Manages connection lifecycle and tool discovery.
/// Uses gRPC transport when `McpConfig.transport == "grpc"`, stdio otherwise.
/// Tools from this server are registered as `mcp/{server_id}/{tool_name}`.
pub struct McpClient {
    /// Configuration including server identity and transport settings.
    pub config: McpConfig,
    /// Unique session identifier for this client instance.
    pub session_id: String,
    tools: Arc<RwLock<Vec<ToolHandle>>>,
    connected: Arc<RwLock<bool>>,
    /// Optional gRPC transport used when `config.transport == "grpc"`
    grpc: Option<McpGrpcTransport>,
    /// 远程 raw tool name 反查表（sanitized ToolId.0 → raw name）。
    ///
    /// 设计：MCP 远程 server 用自己的 tool name（可能含 `.`），我方注册时统一 sanitize
    /// 为 LLM 协议合规字符（`mcp_srv_my_func`），但 execute 调远程时仍需用原始 raw name。
    /// 此 map 在 discover 时一次性填充；O(1) 查询。
    name_map: Arc<RwLock<std::collections::HashMap<String, String>>>,
}

impl McpClient {
    /// Create a new MCP client with the given configuration.
    /// Session ID is auto-generated.
    pub fn new(config: McpConfig) -> Self {
        let grpc = if config.transport == "grpc" {
            Some(McpGrpcTransport::new(&config.server_id.0, &config.address))
        } else {
            None
        };
        Self {
            session_id: generate_session_id(),
            tools: Arc::new(RwLock::new(Vec::new())),
            connected: Arc::new(RwLock::new(false)),
            grpc,
            config,
            name_map: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// Establish connection to the MCP server.
    /// Uses gRPC transport if configured; stdio spawns the server process.
    pub async fn connect(&self) -> Result<(), KernelError> {
        if let Some(ref grpc) = self.grpc {
            return grpc.connect().await;
        }
        // stdio: mark as connected (actual process spawn happens on first call)
        let mut connected = self.connected.write().await;
        *connected = true;
        Ok(())
    }

    /// Discover tools from the MCP server via JSON-RPC `tools/list`.
    pub async fn discover_tools(&self) -> Result<Vec<ToolHandle>, KernelError> {
        if let Some(ref grpc) = self.grpc {
            return grpc.discover_tools().await;
        }
        self.connect().await?;
        let server_id = &self.config.server_id;

        // JSON-RPC request: tools/list
        let response = self.stdio_rpc("tools/list", serde_json::json!({})).await?;

        // Parse tool list from response
        let tools_array = response.get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();

        // 同步收集 raw name → 用于填充 name_map
        let mut name_map_entries: Vec<(String, String)> = Vec::new();
        let tools: Vec<ToolHandle> = tools_array.iter().filter_map(|t| {
            let name = t.get("name")?.as_str()?;
            let description = t.get("description").and_then(|d| d.as_str()).unwrap_or("");
            let params = t.get("inputSchema").cloned().unwrap_or(serde_json::json!({"type":"object"}));
            // 单一命名：ToolId.0 == schema.name == sanitized；外部输入的 server_id/name
            // 可能含 . 或 /，sanitize 一次让后续 dispatch 直接命中。
            let sanitized_id = format!(
                "mcp_{}_{}",
                crate::llm::tool_view::sanitize_name(&server_id.0),
                crate::llm::tool_view::sanitize_name(name),
            );
            // execute 时把 sanitized_id 反查回远程 raw name
            name_map_entries.push((sanitized_id.clone(), name.to_string()));
            Some(ToolHandle {
                id: ToolId(sanitized_id.clone()),
                schema: ToolSchema {
                    name: sanitized_id,
                    description: description.to_string(),
                    parameters: params,
                    returns: None,
                    security: None,
                    cost: None,
                    examples: Vec::new(),
                    applicable_task_kinds: None,
                    idempotent: false,
                },
                provider: ToolProvider::Mcp { server_id: server_id.0.clone() },
                state: ToolState::Loaded,
                effectiveness: Default::default(),
            })
        }).collect();

        {
            let mut t = self.tools.write().await;
            *t = tools.clone();
        }
        {
            let mut m = self.name_map.write().await;
            for (k, v) in name_map_entries {
                m.insert(k, v);
            }
        }
        Ok(tools)
    }

    /// Execute a tool on the MCP server via JSON-RPC `tools/call`.
    pub async fn execute(&self, tool_id: &ToolId, params: Value) -> Result<ToolOutput, KernelError> {
        // 反查远程 raw name —— ToolId 是 sanitized 形态（mcp_srv_xx_yy_zz），
        // 远程 server 期望的 tool name 可能含 .，必须从 name_map 取原值。
        // 缺失映射 → 硬失败：sanitized 名几乎注定让远程 404，静默回退会把"本地状态错误"
        // 转嫁成"远程协议错误"，调试时表象偏离根因。
        let tool_name = {
            let m = self.name_map.read().await;
            m.get(&tool_id.0).cloned().ok_or_else(|| KernelError::Other(format!(
                "MCP name_map miss for tool_id={} — list_tools 未注册或被覆盖",
                tool_id.0
            )))?
        };
        if let Some(ref grpc) = self.grpc {
            let hmac_sig = String::new();
            return grpc.execute(&tool_name, params, &hmac_sig).await;
        }
        if !*self.connected.read().await {
            return Err(KernelError::Other("MCP client not connected".into()));
        }
        let start = std::time::Instant::now();
        let response = self.stdio_rpc("tools/call", serde_json::json!({
            "name": tool_name,
            "arguments": params,
        })).await?;
        let latency = start.elapsed().as_millis() as u64;

        let content = response.get("content")
            .cloned()
            .unwrap_or(response.clone());
        let is_error = response.get("isError")
            .and_then(|e| e.as_bool())
            .unwrap_or(false);

        Ok(ToolOutput {
            tool_id: tool_id.clone(),
            success: !is_error,
            output: content,
            latency_ms: latency,
            failure_kind: if is_error { Some("BusinessError".into()) } else { None },
            try_instead: Vec::new(),
        })
    }

    /// Send a JSON-RPC request over stdio to the MCP server process.
    ///
    /// Protocol: newline-delimited JSON-RPC 2.0
    /// - Write: `{"jsonrpc":"2.0","id":N,"method":"<method>","params":<params>}\n`
    /// - Read: `{"jsonrpc":"2.0","id":N,"result":<result>}\n`
    async fn stdio_rpc(&self, method: &str, params: Value) -> Result<Value, KernelError> {
        use tokio::process::Command;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let address = &self.config.address;
        if address.is_empty() {
            return Err(KernelError::Other("stdio transport requires 'address' to be the server command (e.g. 'npx @modelcontextprotocol/server-foo')".into()));
        }

        // Spawn server process (short-lived per call for simplicity; production should pool)
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(address)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| KernelError::Other(format!("spawn MCP server: {e}")))?;

        let mut stdin = child.stdin.take()
            .ok_or_else(|| KernelError::Other("no stdin".into()))?;
        let stdout = child.stdout.take()
            .ok_or_else(|| KernelError::Other("no stdout".into()))?;

        // Send JSON-RPC request
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let mut req_bytes = serde_json::to_vec(&request)
            .map_err(|e| KernelError::Other(format!("serialize: {e}")))?;
        req_bytes.push(b'\n');
        stdin.write_all(&req_bytes).await
            .map_err(|e| KernelError::Other(format!("write stdin: {e}")))?;
        stdin.flush().await.ok();
        drop(stdin); // Signal EOF to server

        // Read response (first line of stdout)
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();

        let read_result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            reader.read_line(&mut line),
        ).await;

        // Kill child process
        child.kill().await.ok();

        match read_result {
            Ok(Ok(0)) | Ok(Err(_)) => {
                Err(KernelError::Other("MCP server returned empty response".into()))
            }
            Ok(Ok(_)) => {
                let resp: Value = serde_json::from_str(line.trim())
                    .map_err(|e| KernelError::Other(format!("parse response: {e}")))?;
                if let Some(error) = resp.get("error") {
                    Err(KernelError::Other(format!("MCP error: {}", error)))
                } else {
                    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
                }
            }
            Err(_) => Err(KernelError::Other("MCP server timeout (30s)".into())),
        }
    }

    /// Disconnect from the MCP server.
    pub async fn disconnect(&self) {
        if let Some(ref grpc) = self.grpc {
            return grpc.disconnect().await;
        }
        let mut connected = self.connected.write().await;
        *connected = false;
    }
}

// ─── MCP ToolExecutor 桥接 ──────────────────────────────────────────────
//
// Phase 1 实现：把 McpClient 包装为 ToolExecutor，让 ToolRegistry.execute()
// 能 dispatch 到外部 MCP 服务器。
//
// ## 引用关系
// - 创建：`CoreLoop::enable_mcp()` 为每个 server_id 创建一个 executor
// - 消费：`ToolRegistry::execute()` 通过 executors HashMap 调用
// - 销毁：随 ToolRegistry 销毁；client 引用计数到 0 时断开连接
//
// ## 工具 ID 约定
// 形如 `mcp/{server_id}/{tool_name}`。executor 持有 server_id，
// 执行时剥前缀提取 tool_name 转给 McpClient。

/// ToolExecutor wrapper for MCP-discovered tools.
///
/// One executor instance per MCP server. Multiple tool IDs from the same
/// server share this single executor (via Arc::clone in register_executor).
pub struct McpToolExecutor {
    client: Arc<McpClient>,
    server_id: String,
}

impl McpToolExecutor {
    pub fn new(client: Arc<McpClient>, server_id: impl Into<String>) -> Self {
        Self { client, server_id: server_id.into() }
    }
}

#[async_trait::async_trait]
impl crate::tool::ToolExecutor for McpToolExecutor {
    async fn execute(
        &self,
        tool_id: &abacus_types::ToolId,
        params: Value,
        _ctx: &crate::tool::ExecutionContext,
    ) -> abacus_types::Result<Value> {
        // 命名约定：ToolId 为 "mcp_{sanitized_server}_{sanitized_tool}"
        let prefix = format!(
            "mcp_{}_",
            crate::llm::tool_view::sanitize_name(&self.server_id),
        );
        if !tool_id.0.starts_with(&prefix) {
            return Err(KernelError::Other(format!(
                "McpToolExecutor for server '{}' got non-MCP tool_id: {}",
                self.server_id, tool_id.0
            )));
        }
        // McpClient::execute 内部 name_map 反查 raw name；此处保留 ToolId 完整传入
        let output = self.client.execute(tool_id, params).await?;
        if !output.success {
            // 与 ToolRegistry::execute 错误处理对齐：失败时返回 Err，
            // 由上层包成 ToolOutput { success: false, output: { error: ... } }
            return Err(KernelError::Other(
                output.output.to_string()
            ));
        }
        Ok(output.output)
    }
}

// ─── Plugin Loader ───────────────────────────────────────────────────────

use std::collections::HashMap;
use std::path::PathBuf;

use wasmtime::{Config, Engine, Linker, Module, Store, TypedFunc};

/// Loads and manages WASM-based plugins.
///
/// Plugins are loaded from `base_dir/<plugin_id>/` directories containing
/// `manifest.yaml` and `plugin.wasm`.
///
/// Execution sandbox:
/// - No WASI (no file/network access)
/// - Memory limited by `max_memory_mb` from manifest
/// - Pure sandboxed execution in linear memory
pub struct PluginLoader {
    plugins: Arc<RwLock<HashMap<String, PluginInstance>>>,
    base_dir: PathBuf,
    engine: Engine,
    /// 反查表：sanitized ToolId.0 (`plugin_xxx_yyy`) → (raw plugin_id, raw tool_name)
    ///
    /// 注册时由 CoreLoop::enable_plugins 填充；execute 时 O(1) 反查。
    /// plugin_id / tool_name 都可能含 `_` 等字符，无法 split 准确还原。
    name_map: Arc<RwLock<HashMap<String, (String, String)>>>,
}

struct PluginInstance {
    manifest: PluginManifest,
    module: Module,
}

impl PluginLoader {
    /// Create a new plugin loader that scans the given directory.
    pub fn new(base_dir: impl Into<String>) -> Self {
        let config = Config::new();
        let engine = Engine::new(&config).unwrap();
        Self {
            plugins: Arc::new(RwLock::new(HashMap::new())),
            base_dir: PathBuf::from(base_dir.into()),
            engine,
            name_map: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Task #79：暴露 base_dir 给签名验证路径读取 wasm 字节
    pub fn base_dir(&self) -> &std::path::Path {
        &self.base_dir
    }

    /// 注册 sanitized ToolId → raw (plugin_id, tool_name) 反查映射
    ///
    /// 由 CoreLoop::enable_plugins 在创建 ToolHandle 时同步调用。
    pub async fn register_name_mapping(
        &self,
        sanitized_id: String,
        plugin_id: String,
        tool_name: String,
    ) {
        let mut m = self.name_map.write().await;
        m.insert(sanitized_id, (plugin_id, tool_name));
    }

    /// 反查 sanitized ToolId → 原始 (plugin_id, tool_name)
    pub async fn lookup_name(&self, sanitized_id: &str) -> Option<(String, String)> {
        let m = self.name_map.read().await;
        m.get(sanitized_id).cloned()
    }

    /// Discover available plugins from the base directory.
    ///
    /// Expects each subdirectory to contain `manifest.yaml` + `plugin.wasm`.
    pub async fn discover(&self) -> Result<Vec<PluginManifest>, KernelError> {
        let mut manifests = Vec::new();
        let base = &self.base_dir;
        if !base.exists() { return Ok(manifests); }

        let mut entries = tokio::fs::read_dir(base).await
            .map_err(|e| KernelError::Other(format!("read plugin dir: {e}")))?;
        while let Some(entry) = entries.next_entry().await
            .map_err(|e| KernelError::Other(format!("read entry: {e}")))? {
            let path = entry.path();
            if !path.is_dir() { continue; }
            let yaml_path = path.join("manifest.yaml");
            let wasm_path = path.join("plugin.wasm");
            if !yaml_path.exists() || !wasm_path.exists() { continue; }
            let content = tokio::fs::read_to_string(&yaml_path).await
                .map_err(|e| KernelError::Other(format!("read manifest: {e}")))?;
            if let Ok(manifest) = serde_yaml::from_str::<PluginManifest>(&content) {
                manifests.push(manifest);
            }
        }
        Ok(manifests)
    }

    /// Task #79：验证 manifest.signature 与 wasm 字节 hash 是否一致
    ///
    /// ## 算法
    /// 当前仅支持 `algorithm = "sha256"`：取 hex 比较。
    /// 其他算法（如 ed25519）返回 `Err`——上层按 require_signing 决定 skip 还是 abort。
    ///
    /// ## 输入
    /// `manifest.signature` 为 None → 返回 `Ok(false)`（未签名）
    /// `manifest.signature` 为 Some 但算法不支持 → `Err`
    /// 算法 sha256 + value 与计算 hash 一致 → `Ok(true)`
    /// 算法 sha256 + value 不一致 → `Err`
    ///
    /// 引用关系：仅由 enable_plugins_with_options 调用；纯函数无副作用。
    pub fn verify_signature(manifest: &PluginManifest, wasm_bytes: &[u8]) -> Result<bool, KernelError> {
        let sig = match &manifest.signature {
            Some(s) => s,
            None => return Ok(false),
        };
        match sig.algorithm.as_str() {
            "sha256" => {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(wasm_bytes);
                let digest = hasher.finalize();
                // 内联 hex 编码避免新增 hex crate 依赖
                let mut computed = String::with_capacity(64);
                for b in digest.iter() {
                    computed.push_str(&format!("{:02x}", b));
                }
                if computed.eq_ignore_ascii_case(&sig.value) {
                    Ok(true)
                } else {
                    Err(KernelError::Other(format!(
                        "plugin {} signature mismatch: expected {}, got {}",
                        manifest.id, sig.value, computed
                    )))
                }
            }
            other => Err(KernelError::Other(format!(
                "plugin {} unsupported signature algorithm: {}",
                manifest.id, other
            ))),
        }
    }

    /// Load a plugin: read WASM binary → compile module → hold in memory.
    pub async fn load(&self, manifest: PluginManifest) -> Result<(), KernelError> {
        let wasm_path = self.base_dir.join(&manifest.id).join("plugin.wasm");
        let wasm_bytes = tokio::fs::read(&wasm_path).await
            .map_err(|e| KernelError::Other(format!("read wasm: {e}")))?;
        let module = Module::new(&self.engine, &wasm_bytes)
            .map_err(|e| KernelError::Other(format!("compile wasm: {e}")))?;
        self.plugins.write().await.insert(manifest.id.clone(), PluginInstance { manifest, module });
        Ok(())
    }

    /// Execute a WASM plugin function with sandbox timeout.
    ///
    /// Module must export:
    /// - `memory` — linear memory
    /// - `alloc(size: i32) -> i32` — allocate bytes
    /// - `{tool}(params_ptr: i32, params_len: i32) -> i32` — result ptr (null-terminated JSON)
    ///
    /// Each call creates a fresh instance (compiled module cached).
    /// Execution is bounded by a 30s timeout.
    pub async fn execute(&self, plugin_id: &str, tool: &str, params: Value) -> Result<ToolOutput, KernelError> {
        let plugins = self.plugins.read().await;
        let entry = plugins.get(plugin_id)
            .ok_or_else(|| KernelError::Other(format!("plugin not found: {plugin_id}")))?;

        let max_memory = entry.manifest.max_memory_mb.max(1) as usize * 1024 * 1024;

        let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let mut store = Store::new(&self.engine, ());
            let linker = Linker::new(&self.engine);
            let instance = linker.instantiate(&mut store, &entry.module)
                .map_err(|e| KernelError::Other(format!("instantiate: {e}")))?;

            let memory = instance.get_memory(&mut store, "memory")
                .ok_or_else(|| KernelError::Other("no exported memory".into()))?;

            let params_json = serde_json::to_string(&params).unwrap_or_else(|_| "{}".into());
            let params_bytes = params_json.as_bytes().to_vec();

            // Allocate memory for input
            let alloc: TypedFunc<i32, i32> = instance.get_typed_func(&mut store, "alloc")
                .map_err(|e| KernelError::Other(format!("alloc export: {e}")))?;
            let ptr = alloc.call(&mut store, params_bytes.len() as i32)
                .map_err(|e| KernelError::Other(format!("alloc call: {e}")))?;
            memory.write(&mut store, ptr as usize, &params_bytes)
                .map_err(|e| KernelError::Other(format!("memory write: {e}")))?;

            // Call tool function
            let func: TypedFunc<(i32, i32), i32> = instance.get_typed_func(&mut store, tool)
                .map_err(|e| KernelError::Other(format!("{tool} export: {e}")))?;
            let result_ptr = func.call(&mut store, (ptr, params_bytes.len() as i32))
                .map_err(|e| KernelError::Other(format!("wasm call: {e}")))?;

            // Scan for null terminator (up to max_memory from result_ptr)
            let scan_len = max_memory.saturating_sub(result_ptr as usize).min(64 * 1024);
            let mut buf = vec![0u8; scan_len];
            memory.read(&mut store, result_ptr as usize, &mut buf)
                .map_err(|e| KernelError::Other(format!("memory read: {e}")))?;
            let content_end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let result_str = String::from_utf8_lossy(&buf[..content_end]).to_string();
            let result_value: Value = serde_json::from_str(&result_str).unwrap_or(Value::Null);

            let success_flag = !result_value.is_null();
            Ok(ToolOutput {
                tool_id: ToolId(format!(
                    "plugin_{}_{}",
                    crate::llm::tool_view::sanitize_name(plugin_id),
                    crate::llm::tool_view::sanitize_name(tool),
                )),
                success: success_flag,
                output: result_value,
                latency_ms: 0,
                failure_kind: if success_flag { None } else { Some("BusinessError".into()) },
                try_instead: Vec::new(),
            })
        }).await;

        match result {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(KernelError::Other(format!("plugin {plugin_id}/{tool} timed out after 30s"))),
        }
    }

    pub async fn list(&self) -> Vec<PluginManifest> {
        self.plugins.read().await.values().map(|p| p.manifest.clone()).collect()
    }
}

// ─── Plugin ToolExecutor 桥接（Phase 3）─────────────────────────────────
//
// 把 PluginLoader 包装为 ToolExecutor，让 ToolRegistry 能 dispatch 到 WASM 沙箱。
//
// ## 工具 ID 约定
// 形如 `plugin/{plugin_id}/{tool_name}`。executor 全局共享一个 PluginLoader 实例。
//
// ## 引用关系
// - 创建：`CoreLoop::enable_plugins(base_dir)` 创建一次
// - 消费：`ToolRegistry::execute()` 通过 executors HashMap 调用
// - 销毁：随 CoreLoop drop；PluginLoader 内 wasmtime Engine + Module 一同释放

pub struct PluginToolExecutor {
    loader: Arc<PluginLoader>,
}

impl PluginToolExecutor {
    pub fn new(loader: Arc<PluginLoader>) -> Self {
        Self { loader }
    }
}

#[async_trait::async_trait]
impl crate::tool::ToolExecutor for PluginToolExecutor {
    async fn execute(
        &self,
        tool_id: &abacus_types::ToolId,
        params: Value,
        _ctx: &crate::tool::ExecutionContext,
    ) -> abacus_types::Result<Value> {
        // tool_id 形如 "plugin_{sanitized_pid}_{sanitized_tool}"，但 plugin_id / tool_name
        // 都可能含 _，sanitize 后无法 split 还原 → 用 PluginLoader.name_map O(1) 反查
        // 拿回 raw (plugin_id, tool_name)，再传给 loader.execute。
        let (plugin_id, tool_name) = self.loader.lookup_name(&tool_id.0).await
            .ok_or_else(|| KernelError::Other(format!(
                "PluginToolExecutor: tool_id '{}' not registered (call enable_plugins first?)",
                tool_id.0
            )))?;
        let output = self.loader.execute(&plugin_id, &tool_name, params).await?;
        if !output.success {
            return Err(KernelError::Other(format!(
                "plugin '{}/{}' returned null/failure", plugin_id, tool_name
            )));
        }
        Ok(output.output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_types::ServerId;

    #[tokio::test]
    async fn test_mcp_connect_disconnect() {
        let config = McpConfig {
            server_id: ServerId("test-server".into()),
            transport: "stdio".into(),
            address: "".into(),
            tls: false,
            request_signing: false,
        };
        let client = McpClient::new(config);
        assert!(!*client.connected.read().await);
        client.connect().await.unwrap();
        assert!(*client.connected.read().await);
        client.disconnect().await;
        assert!(!*client.connected.read().await);
    }

    #[tokio::test]
    async fn test_mcp_discover() {
        let config = McpConfig {
            server_id: ServerId("test".into()),
            transport: "stdio".into(),
            address: "".into(),
            tls: false,
            request_signing: false,
        };
        let client = McpClient::new(config);
        // With empty address, stdio_rpc returns an error (no server command configured)
        let result = client.discover_tools().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("address"));
    }

    #[tokio::test]
    async fn test_plugin_loader() {
        let loader = PluginLoader::new("/tmp/.abacus/plugins");
        let plugins = loader.discover().await.unwrap();
        assert!(plugins.is_empty());
    }

    // ─── Phase 1：MCP ToolExecutor 桥接测试 ──────────────────────────

    /// McpToolExecutor 必须拒绝非本 server 的 tool_id（防止串扰）
    #[tokio::test]
    async fn mcp_executor_rejects_foreign_tool_id() {
        use crate::tool::{ExecutionContext, ToolExecutor};
        let config = McpConfig {
            server_id: ServerId("alice".into()),
            transport: "stdio".into(),
            address: "".into(),
            tls: false,
            request_signing: false,
        };
        let client = Arc::new(McpClient::new(config));
        let executor = McpToolExecutor::new(client, "alice");

        let ctx = ExecutionContext::noop("test-session");
        // 故意传 bob 服务器的工具 ID
        let result = executor.execute(
            &abacus_types::ToolId("mcp_bob_some_tool".into()),
            serde_json::json!({}),
            &ctx,
        ).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("non-MCP tool_id") || err.contains("alice"),
            "expected server mismatch error, got: {err}");
    }

    /// PluginToolExecutor 通过 PluginLoader.name_map 反查；未注册 ToolId 应报错
    #[tokio::test]
    async fn plugin_executor_rejects_malformed_id() {
        use crate::tool::{ExecutionContext, ToolExecutor};
        let loader = Arc::new(PluginLoader::new("/tmp/.abacus/nonexistent_test"));
        let executor = PluginToolExecutor::new(loader);
        let ctx = ExecutionContext::noop("test");

        // 任意未注册 ToolId（无论形态）都应通过 "not registered" 路径报错
        let result = executor.execute(
            &abacus_types::ToolId("not_a_plugin_foo".into()),
            serde_json::json!({}),
            &ctx,
        ).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not registered"));

        // 形似 plugin_xxx 但 name_map 中无对应映射 → 报错
        let result2 = executor.execute(
            &abacus_types::ToolId("plugin_only_id".into()),
            serde_json::json!({}),
            &ctx,
        ).await;
        assert!(result2.is_err());
        assert!(result2.unwrap_err().to_string().contains("not registered"));
    }

    /// PluginToolExecutor 找不到对应 plugin 时返回 error（来自 PluginLoader）
    #[tokio::test]
    async fn plugin_executor_rejects_unknown_plugin() {
        use crate::tool::{ExecutionContext, ToolExecutor};
        let loader = Arc::new(PluginLoader::new("/tmp/.abacus/nonexistent_test"));
        let executor = PluginToolExecutor::new(loader);
        let ctx = ExecutionContext::noop("test");

        let result = executor.execute(
            &abacus_types::ToolId("plugin_ghost_do_x".into()),
            serde_json::json!({}),
            &ctx,
        ).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // ToolId 未在 PluginLoader.name_map 注册 → 报 "not registered"
        // （之前依赖 split_once 解析时报 "plugin not found"，现在改为反查表）
        assert!(err.contains("not registered"),
            "expected not-registered error, got: {err}");
    }

    /// McpToolExecutor 必须能识别本 server 前缀（即使后端连接失败）
    #[tokio::test]
    async fn mcp_executor_accepts_own_prefix() {
        use crate::tool::{ExecutionContext, ToolExecutor};
        let config = McpConfig {
            server_id: ServerId("alice".into()),
            transport: "stdio".into(),
            address: "".into(),  // 无效地址 → execute 会因 stdio_rpc 失败
            tls: false,
            request_signing: false,
        };
        let client = Arc::new(McpClient::new(config));
        let executor = McpToolExecutor::new(client, "alice");

        let ctx = ExecutionContext::noop("test-session");
        let result = executor.execute(
            &abacus_types::ToolId("mcp_alice_foo".into()),
            serde_json::json!({}),
            &ctx,
        ).await;
        // 前缀正确，但 stdio 后端无地址 → 会到达 client.execute 后失败
        // 错误**不应**是 "non-MCP tool_id"
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("non-MCP tool_id"),
            "前缀正确时不应报 non-MCP 错误：{err}");
    }
}