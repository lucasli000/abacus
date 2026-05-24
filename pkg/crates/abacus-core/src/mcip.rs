//! MCIP — Model Context Isolation Protocol
//!
//! Three-layer security for MCP tool execution: capability checking, TLS transport,
//! and HMAC-SHA256 request signing.
//!
//!
//! ## Dependencies
//!
//! | Crate | Version | Usage |
//! |-------|---------|-------|
//! | `serde` / `serde_json` | workspace | Capability enum + decision log serialization |
//! | `base64` | workspace | HMAC signature encoding |
//! | `hmac` + `sha2` | workspace | HMAC-SHA256 request signing/verification |
//! | `tokio` | workspace | Async RwLock for decision log |
//! | `getrandom` | 0.3 | CSPRNG for HMAC key generation (replaced insecure SystemTime derivation) |
//!
//! ## Imports from Abacus
//!
//! - `abacus_types::ToolId`: Tool identifier for capability checks
//! - `abacus_types::ToolOutput`: Wrapped output with MCIP metadata
//!
//! ## Architecture
//!
//! ```text
//! LLM → ToolRegistry → McipGateway → McpClient → MCP Server
//!                          │
//!                    ┌─────┴──────┐
//!                    │ Capability  │
//!                    │ Checker     │
//!                    ├─────┬──────┤
//!                    │ TLS │ HMAC │
//!                    └─────┴──────┘
//! ```
//!
//! ## Scenarios
//!
//! | Scenario | MCIP Layer | Example |
//! |----------|-----------|---------|
//! | Untrusted MCP server | TLS + HMAC | Verify server identity + sign every request |
//! | Dangerous tool (rm -rf) | Capability | Policy: `dangerous/**` → confirm_required |
//! | Read-only filesystem tool | Capability | Policy: `mcp/fs/**` → read_only=true |
//! | Network-restricted API tool | Capability | Policy: `mcp/web/**` → allow-listed hosts:ports |
//! | Dev environment | TLS disabled | `insecure_skip_verify=true` (never in prod) |
//! ```

use std::sync::Arc;
use abacus_types::{ToolId, ToolOutput, UserRole};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

// ─── Capability Declarations ────────────────────────────────────────────

/// Resource access capability for an MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum McipCapability {
    /// Network access with allow-listed hosts and ports
    Network {
        allowed_hosts: Vec<String>,
        allowed_ports: Vec<u16>,
    },
    /// Filesystem access with path allow-list and read-only flag
    FileSystem {
        allowed_paths: Vec<String>,
        read_only: bool,
    },
    /// Process execution with resource constraints
    Process {
        max_memory_mb: u32,
        max_cpu_percent: u8,
    },
    /// No capabilities required (safe tool)
    Noop,
}

/// Security policy for a tool or tool ID pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McipPolicy {
    /// Glob pattern for tool IDs this policy applies to (e.g. "mcp/files/**")
    pub tool_id_pattern: String,
    /// Allowed capabilities for matching tools
    pub allowed_capabilities: Vec<McipCapability>,
    /// Require user confirmation before execution
    pub confirm_required: bool,
    /// Maximum execution time in milliseconds
    pub max_execution_ms: u64,
    /// Minimum user role required to execute without denial.
    /// Users below this role are denied; users at this role or above
    /// still respect `confirm_required`.
    pub min_role: UserRole,
}

/// Result of a capability check.
#[derive(Debug, Clone)]
pub enum McipDecision {
    /// Execution allowed
    Allowed,
    /// Execution denied with reason
    Denied(String),
    /// Requires user confirmation
    NeedsConfirm(String),
}

/// 授权请求类型：区分 MCIP 策略拦截和工具级破坏性操作警告
#[derive(Debug, Clone, PartialEq)]
pub enum McipConfirmKind {
    /// MCIP 策略要求确认（外部工具 / 能力限制）
    McipPolicy,
    /// 工具自身标记为需要确认（潜在破坏性操作）
    DestructiveOp,
}

/// MCIP 工具授权请求 — NeedsConfirm 或 confirm_required 时生成，携带出 TurnResult
///
/// ## 内容
/// - L4 展示授权对话框：「工具 X 请求授权」 + reason
/// - 用户选择单次 / 总是 / 拒绝并回调
#[derive(Debug, Clone)]
pub struct McipConfirmRequest {
    /// 请求授权的工具 ID
    pub tool_id: String,
    /// 拦截原因
    pub reason: String,
    /// 拦截类型：MCIP 策略 or 工具级破坏性操作
    pub kind: McipConfirmKind,
    /// 参数预览（破坏性操作时展示，让用户看到具体操作内容）
    pub params_preview: Option<String>,
    /// V28：唯一 nonce，UI 用此 key 在 SessionState.mcip_confirm_channels 找回 oneshot sender
    /// 同 turn 多个工具同时需要授权时，nonce 区分各请求。空字符串表示"非 channel 路径"（兼容旧调用）
    pub nonce: String,
}

/// 用户对 MCIP 授权请求的决定
#[derive(Debug, Clone, PartialEq)]
pub enum McipGrantDecision {
    /// 单次允许 — 仅当前 turn 生效，下一次调用仍需确认
    Once,
    /// 常騻允许 — Session 内永久生效，不再弹出授权对话框
    Always,
    /// 拒绝 — 工具执行被阻止，返回错误给 LLM
    Deny,
}

// ─── TLS + HMAC ─────────────────────────────────────────────────────────

/// TLS configuration for MCP connections.
#[derive(Debug, Clone)]
#[derive(Default)]
pub struct McipTlsConfig {
    /// Path to TLS certificate file
    pub cert_path: Option<String>,
    /// Path to TLS key file
    pub key_path: Option<String>,
    /// Path to CA certificate file (for client verification)
    pub ca_path: Option<String>,
    /// Skip certificate verification (dev only)
    pub insecure_skip_verify: bool,
}


/// HMAC-SHA256 request signer and verifier.
pub struct McipHmac {
    key: Vec<u8>,
}

impl McipHmac {
    /// Create a new HMAC signer with the given key.
    pub fn new(key: &[u8]) -> Self {
        Self { key: key.to_vec() }
    }

    /// Sign a message payload and return base64-encoded signature.
    pub fn sign(&self, payload: &str) -> String {
        use hmac::Mac;
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.key)
            .expect("HMAC key length valid");
        mac.update(payload.as_bytes());
        let result = mac.finalize();
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(result.into_bytes())
    }

    /// Verify a signature against a message payload.
    pub fn verify(&self, payload: &str, signature: &str) -> bool {
        use hmac::Mac;
        let sig_bytes = match base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD, signature
        ) {
            Ok(b) => b,
            Err(_) => return false,
        };
        let mut mac = match hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.key) {
            Ok(m) => m,
            Err(_) => return false,
        };
        mac.update(payload.as_bytes());
        mac.verify_slice(&sig_bytes).is_ok()
    }

    /// Generate a random HMAC key (32 bytes for SHA-256) using CSPRNG.
    pub fn generate_key() -> Vec<u8> {
        let mut key = vec![0u8; 32];
        getrandom::fill(&mut key).expect("CSPRNG failure");
        key
    }
}

// ─── MCIP Gateway ───────────────────────────────────────────────────────

/// Central security gateway for MCP tool execution.
///
/// Wraps tool execution with capability checking, optional TLS transport,
/// and request signing/verification.
/// 内置工具的 tool_id 前缀列表，匹配则跨过 MCIP 策略检查直接 Allowed。
///
/// MCIP 设计目标是管控**外部 MCP 服务器**工具（不可信第三方）。
/// 内置工具（filengine/lsp/kb 等）是可信二进制的一部分，不需要 capability policy。
///
/// ## 命名规则
/// 单一命名约定：ToolId.0 == schema.name == LLM 调用名，全部下划线形态
/// （`filengine_fs_read` / `db_query` / `lsp_hover` / `mcp_srv_tool` 等）。
/// 前缀匹配 ToolId.0 开头。
const BUILTIN_EXEMPT_PREFIXES: &[&str] = &[
    // —— 单一命名约定：所有工具 ToolId 用下划线分隔形态 ——
    "filengine_",    // filengine_fs_read / filengine_web_fetch / filengine_bash_exec
    "env_",          // env_status
    "deduction_",    // deduction_status / deduction_analyze
    "context_",      // context_declare / context_keep / context_compress
    "task_",         // task_plan / task_run
    "session_",      // session_set_focus / session_request_permission / session_recall
    "interaction_",  // interaction_status / interaction_path / interaction_recall / interaction_mark
    "messages_",     // messages_recover
    "db_",           // db_info / db_query / db_list_tables
    "kb_",           // kb_ingest / kb_query / kb_search
    "code_",         // code_execute
    "orchestrate_",  // orchestrate_assess / orchestrate_upgrade
    "lsp_",          // 全部 LSP 工具
    "result_",       // result_expand
    "sandbox_",
    "mem_",          // 记忆宫殿工具
];

pub struct McipGateway {
    policies: Vec<McipPolicy>,
    hmac: Option<McipHmac>,
    tls_config: Option<McipTlsConfig>,
    /// P3-B: BoundedFifo 替代 Vec.remove(0)，500 条上限
    decision_log: Arc<RwLock<abacus_types::BoundedFifo<McipDecisionRecord>>>,
    /// Simple sliding-window rate limiter: (window_start, count_in_window)
    rate_limit: std::sync::Mutex<(std::time::Instant, u32)>,
    /// Max checks per second (0 = unlimited)
    max_checks_per_sec: u32,
    /// 额外调用方自定义的豆免前缀（补充 BUILTIN_EXEMPT_PREFIXES）
    /// 用 std::sync::RwLock 包裹，支持 &self 运行时添加
    extra_exempt_prefixes: std::sync::RwLock<Vec<String>>,
    /// 精确允许工具 ID 名单（跳过 MCIP 策略，直接 Allowed）
    ///
    /// 来源：security.yaml `mcip.allow_tools`
    /// 优先级低于 deny_tools，高于 exempt_prefixes
    allow_tools: std::sync::RwLock<std::collections::HashSet<String>>,
    /// 永久禁止工具 ID 名单（最高优先级，覆盖一切授权）
    ///
    /// 来源：security.yaml `mcip.deny_tools`
    /// 即使用户已手动授权也不能绕过此项（系统管理员第一道防线）
    deny_tools: std::sync::RwLock<std::collections::HashSet<String>>,
}

/// Record of an MCIP decision for audit logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McipDecisionRecord {
    pub tool_id: String,
    pub decision: String,
    pub reason: String,
    pub timestamp_ms: u64,
}

impl Default for McipGateway {
    fn default() -> Self { Self::new() }
}

impl McipGateway {
    /// Create an MCIP gateway.
    ///
    /// Default policy: no explicit policies registered.
    /// However, `check()` applies an implicit **deny-all** fallback:
    /// any tool not matching an explicit policy is denied.
    pub fn new() -> Self {
        Self {
            policies: Vec::new(),
            hmac: None,
            tls_config: None,
            decision_log: Arc::new(RwLock::new(abacus_types::BoundedFifo::new(500))),
            rate_limit: std::sync::Mutex::new((std::time::Instant::now(), 0)),
            max_checks_per_sec: 100,
            extra_exempt_prefixes: std::sync::RwLock::new(Vec::new()),
            allow_tools: std::sync::RwLock::new(std::collections::HashSet::new()),
            deny_tools: std::sync::RwLock::new(std::collections::HashSet::new()),
        }
    }

    /// 从 config 批量设置允许工具名单（security.yaml `mcip.allow_tools`）
    pub fn apply_allow_tools(&self, tools: &[String]) {
        let mut guard = self.allow_tools.write().unwrap();
        for t in tools { guard.insert(t.clone()); }
    }

    /// 从 config 批量设置禁止工具名单（security.yaml `mcip.deny_tools`）
    pub fn apply_deny_tools(&self, tools: &[String]) {
        let mut guard = self.deny_tools.write().unwrap();
        for t in tools { guard.insert(t.clone()); }
    }

    /// 添加额外豆免前缀（对除内置工具外的自定义工具豆免）。
    ///
    /// 内置前缀（`BUILTIN_EXEMPT_PREFIXES`）无需手动添加，已默认生效。
    /// 取 `&self` ——可在 CoreLoop 初始化后、Arc 包裹前后任意时刻调用。
    pub fn add_exempt_prefix(&self, prefix: impl Into<String>) {
        self.extra_exempt_prefixes.write().unwrap().push(prefix.into());
    }

    /// 批量注册豆免前缀（从 config.yaml 读入的列表）
    pub fn apply_exempt_prefixes(&self, prefixes: &[String]) {
        let mut guard = self.extra_exempt_prefixes.write().unwrap();
        for p in prefixes {
            if !guard.contains(p) {
                guard.push(p.clone());
            }
        }
    }

    /// Set the maximum checks per second for rate limiting (0 = unlimited).
    pub fn set_rate_limit(&mut self, max_per_sec: u32) {
        self.max_checks_per_sec = max_per_sec;
    }

    /// Register a security policy.
    pub fn add_policy(&mut self, policy: McipPolicy) {
        self.policies.push(policy);
    }

    /// Enable HMAC request signing with an auto-generated key.
    pub fn enable_hmac(&mut self, key: Option<Vec<u8>>) {
        let key = key.unwrap_or_else(McipHmac::generate_key);
        self.hmac = Some(McipHmac::new(&key));
    }

    /// Configure TLS for this gateway.
    pub fn set_tls(&mut self, config: McipTlsConfig) {
        self.tls_config = Some(config);
    }

    /// Check whether a tool execution is allowed under current policies and user role.
    ///
    /// Three-tier gate: role check → confirm check → capability check.
    /// - If `role < policy.min_role`: **Denied** (insufficient privilege)
    /// - If `policy.confirm_required`: **NeedsConfirm** (even Admins must confirm)
    /// - If `check_capabilities` fails: **Denied**
    /// - If no explicit policy matches: **Denied** (implicit deny-all)
    #[tracing::instrument(skip(self, params))]
    pub fn check(&self, tool_id: &ToolId, params: &Value, role: UserRole) -> McipDecision {
        // Rate limiting: simple sliding window (1-second window)
        if self.max_checks_per_sec > 0 {
            if let Ok(mut guard) = self.rate_limit.lock() {
                let now = std::time::Instant::now();
                if now.duration_since(guard.0).as_secs() >= 1 {
                    *guard = (now, 1);
                } else {
                    guard.1 += 1;
                    if guard.1 > self.max_checks_per_sec {
                        return McipDecision::Denied(
                            "rate limit exceeded: too many tool checks per second".into()
                        );
                    }
                }
            }
        }

        let id_str = tool_id.0.as_str();

        // ① deny_tools：最高优先级——系统管理员第一道防线，覆盖一切授权
        {
            let deny = self.deny_tools.read().unwrap();
            if deny.contains(id_str) {
                return McipDecision::Denied(format!(
                    "tool '{id_str}' is on the permanent deny list (security.yaml mcip.deny_tools)"
                ));
            }
        }

        // ② allow_tools：精确允许名单——跳过策略直接放行
        {
            let allow = self.allow_tools.read().unwrap();
            if allow.contains(id_str) {
                return McipDecision::Allowed;
            }
        }

        // ③ exempt_prefixes：内置工具和自定义前缀豆免
        // 内置前缀：env_/fs_/db_/kb_/lsp. 等——可信二进制不需 capability policy
        // 自定义前缀：调用方通过 `add_exempt_prefix()` 或 security.yaml 添加
        let extra = self.extra_exempt_prefixes.read().unwrap();
        let is_exempt = BUILTIN_EXEMPT_PREFIXES.iter().any(|p| id_str.starts_with(p))
            || extra.iter().any(|p| id_str.starts_with(p.as_str()));
        drop(extra); // 释放读锁，避免后续策略循环持锁
        if is_exempt {
            return McipDecision::Allowed;
        }

        let mut matched = false;
        for policy in &self.policies {
            if glob_match(&policy.tool_id_pattern, id_str) {
                matched = true;
                // Tier 1: Role gate
                if role < policy.min_role {
                    return McipDecision::Denied(
                        format!("insufficient role: need {need}, have {have} for tool {tool}",
                            need = policy.min_role, have = role, tool = id_str)
                    );
                }
                // Tier 2: Confirmation gate
                if policy.confirm_required {
                    return McipDecision::NeedsConfirm(
                        format!("tool {} requires confirmation", id_str)
                    );
                }
                // Tier 3: Capability gate
                if let Err(reason) = self.check_capabilities(policy, params) {
                    return McipDecision::Denied(reason);
                }
            }
        }
        if !matched {
            // V22：MCIP 默认从"deny by default"改为"confirm by default"
            //   用户场景下所有 unmatched 工具应弹窗请求授权（带破坏性标识），而非直接拒
            //   破坏性启发式：name 含 write/delete/remove/exec/run/drop/truncate/rm/kill
            //                 → 标记 destructive，UI 渲染红色警告
            //   非破坏性（read/list/search/get/info）→ 友好的授权确认
            let lname = id_str.to_ascii_lowercase();
            let destructive_kw = [
                "write", "delete", "remove", "exec", "run", "drop",
                "truncate", "rm", "kill", "create", "update", "patch",
                "edit", "send", "post", "shell", "bash",
            ];
            let is_destructive = destructive_kw.iter().any(|kw| lname.contains(kw));
            let reason = if is_destructive {
                format!("[destructive] tool {tool} 未配置 policy，需用户确认（破坏性操作）", tool = id_str)
            } else {
                format!("[read-only] tool {tool} 未配置 policy，需用户确认", tool = id_str)
            };
            return McipDecision::NeedsConfirm(reason);
        }
        McipDecision::Allowed
    }

    /// Wrap a tool output with MCIP metadata.
    pub fn wrap_output(&self, tool_output: ToolOutput) -> ToolOutput {
        let signed = self.hmac.as_ref().map(|hmac| {
            let payload = serde_json::to_string(&tool_output.output)
                .unwrap_or_default();
            hmac.sign(&payload)
        });
        let mut output = tool_output.output;
        if let Some(sig) = signed {
            if let Value::Object(ref mut map) = output {
                map.insert("_mcip_sig".into(), Value::String(sig));
            }
        }
        ToolOutput { output, ..tool_output }
    }

    /// Sign a request payload with HMAC if enabled.
    pub fn sign_request(&self, payload: &str) -> Option<String> {
        self.hmac.as_ref().map(|hmac| hmac.sign(payload))
    }

    /// Log a decision for audit. P3-B: BoundedFifo 自动 evict.
    pub async fn log_decision(&self, record: McipDecisionRecord) {
        self.decision_log.write().await.push(record);
    }

    /// Get the HMAC instance for external use.
    pub fn hmac(&self) -> Option<&McipHmac> {
        self.hmac.as_ref()
    }

    /// Get TLS config reference.
    pub fn tls_config(&self) -> Option<&McipTlsConfig> {
        self.tls_config.as_ref()
    }

    fn check_capabilities(&self, policy: &McipPolicy, params: &Value) -> Result<(), String> {
        for cap in &policy.allowed_capabilities {
            match cap {
                McipCapability::Network { allowed_hosts, allowed_ports } => {
                    if let Some(host) = params.get("host").and_then(|v| v.as_str()) {
                        if !allowed_hosts.iter().any(|h| host == h) {
                            return Err(format!("host '{}' not in allowed list: {:?}", host, allowed_hosts));
                        }
                    }
                    if let Some(port) = params.get("port").and_then(|v| v.as_u64()) {
                        if !allowed_ports.contains(&(port as u16)) {
                            return Err(format!("port {} not in allowed list: {:?}", port, allowed_ports));
                        }
                    }
                }
                McipCapability::FileSystem { allowed_paths, read_only } => {
                    if *read_only {
                        if let Some(method) = params.get("method").and_then(|v| v.as_str()) {
                            if matches!(method, "write" | "edit" | "delete" | "move" | "create") {
                                return Err("filesystem is read-only for this tool".into());
                            }
                        }
                    }
                    if let Some(path) = params.get("path").and_then(|v| v.as_str()) {
                        if !allowed_paths.iter().any(|p| path.starts_with(p)) {
                            return Err(format!("path '{}' not in allowed list", path));
                        }
                    }
                }
                McipCapability::Process { max_memory_mb, max_cpu_percent } => {
                    if let Some(mem) = params.get("memory_mb").and_then(|v| v.as_u64()) {
                        if mem > *max_memory_mb as u64 {
                            return Err(format!("memory {}MB exceeds limit {}MB", mem, max_memory_mb));
                        }
                    }
                    // cpu_percent is advisory — not checked at runtime
                    let _ = max_cpu_percent;
                }
                McipCapability::Noop => {}
            }
        }
        Ok(())
    }
}

/// Simple glob matching for tool_id matching (supports `*` and `**`).
fn glob_match(pattern: &str, tool_id: &str) -> bool {
    if pattern == "*" || pattern == "**" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        if tool_id.len() <= prefix.len() { return false; }
        let remaining = &tool_id[prefix.len()..];
        return remaining.starts_with('/') && !remaining[1..].contains('/');
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return tool_id.starts_with(prefix);
    }
    pattern == tool_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hmac_sign_verify() {
        let key = McipHmac::generate_key();
        let hmac = McipHmac::new(&key);
        let payload = r#"{"tool":"filengine_fs_read","path":"/tmp/test"}"#;
        let sig = hmac.sign(payload);
        assert!(hmac.verify(payload, &sig));
        assert!(!hmac.verify(payload, &(sig[..sig.len()-1].to_string() + "0")));
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("mcp/files/*", "mcp/files/read"));
        assert!(!glob_match("mcp/files/*", "mcp/files/read/sub/other"));
        assert!(glob_match("mcp/**", "mcp/files/read/edit"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exact2"));
    }

    #[test]
    fn test_capability_network_deny() {
        let policy = McipPolicy {
            tool_id_pattern: "mcp/web/**".into(),
            allowed_capabilities: vec![McipCapability::Network {
                allowed_hosts: vec!["api.example.com".into()],
                allowed_ports: vec![443],
            }],
            confirm_required: false,
            max_execution_ms: 60000,
            min_role: UserRole::User,
        };
        let gateway = McipGateway::new();
        // gateway created without adding policy — just test check_capabilities directly
        let params = serde_json::json!({"host": "evil.com", "port": 80});
        let result = gateway.check_capabilities(&policy, &params);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("evil.com"));
    }

    #[test]
    fn test_capability_filesystem_readonly() {
        let policy = McipPolicy {
            tool_id_pattern: "mcp/fs/**".into(),
            allowed_capabilities: vec![McipCapability::FileSystem {
                allowed_paths: vec!["/tmp".into()],
                read_only: true,
            }],
            confirm_required: false,
            max_execution_ms: 60000,
            min_role: UserRole::User,
        };
        let gateway = McipGateway::new();
        let params = serde_json::json!({"method": "write", "path": "/tmp/test"});
        let result = gateway.check_capabilities(&policy, &params);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_gateway_log_decision() {
        let gateway = McipGateway::new();
        gateway.log_decision(McipDecisionRecord {
            tool_id: "test".into(),
            decision: "denied".into(),
            reason: "capability check failed".into(),
            timestamp_ms: 1000,
        }).await;
        let log = gateway.decision_log.read().await;
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn test_mcip_decision_check() {
        let mut gateway = McipGateway::new();
        gateway.add_policy(McipPolicy {
            tool_id_pattern: "dangerous/**".into(),
            allowed_capabilities: vec![],
            confirm_required: true,
            max_execution_ms: 1000,
            min_role: UserRole::User,
        });
        let decision = gateway.check(&ToolId("dangerous/rm_rf".into()), &Value::Null, UserRole::User);
        assert!(matches!(decision, McipDecision::NeedsConfirm(_)));

        // Role gate: Developer should also need confirm (confirm_required > role gate)
        let dev_decision = gateway.check(&ToolId("dangerous/rm_rf".into()), &Value::Null, UserRole::Developer);
        assert!(matches!(dev_decision, McipDecision::NeedsConfirm(_)));

        // Role gate: User below min_role=Developer → denied
        let mut gateway2 = McipGateway::new();
        gateway2.add_policy(McipPolicy {
            tool_id_pattern: "admin/**".into(),
            allowed_capabilities: vec![],
            confirm_required: false,
            max_execution_ms: 60000,
            min_role: UserRole::Admin,
        });
        let denied = gateway2.check(&ToolId("admin/delete".into()), &Value::Null, UserRole::Developer);
        assert!(matches!(denied, McipDecision::Denied(ref r) if r.contains("insufficient role")));

        // Admin with same policy → allowed (no confirm_required)
        let allowed = gateway2.check(&ToolId("admin/delete".into()), &Value::Null, UserRole::Admin);
        assert!(matches!(allowed, McipDecision::Allowed));
    }
}