//! 外部工具预处理流水线 (External Tool Ingest Pipeline)
//!
//! ## 用途
//!
//! 当用户安装外部工具 (MCP / Plugin / Custom) 时, 工具元数据从外部源到达,
//! 但 abacus 内部需要 6 套元数据 (cluster/scene/cost/short_description/...
//! + Memory Palace 联动). 本模块把"外部工具 spec"通过 6 步转换为完整的
//! [`IngestedTool`], 供 manifest 合并 + Palace 注册使用.
//!
//! ## 6 步流水线
//!
//! 1. **Domain 检测**: 多信号融合 (`extract_tool_domain` + 关键词扫描 + 参数类型启发)
//!    输出: `Domain` 枚举 (0-7)
//! 2. **Cost 估算**: 基于描述关键词 (I/O / compute / network) 分配 token + latency
//!    输出: `tokens: u32`, `latency: String`
//! 3. **Cluster 分配**: 基于 domain + actions 把工具分配到现有 cluster 之一
//!    输出: `cluster: String` (与 `tools.toml` 的 cluster 名一致)
//! 4. **Short description 生成**: 截取 description 第一句或前 60 字符
//!    输出: `short_description: String`
//! 5. **Scene prefix 映射**: 基于工具名匹配 scene (clarify/plan/team/meeting)
//!    输出: `scenes: Vec<String>`
//! 6. **Palace 注册**: 返回 `tokio::task::JoinHandle<()>`, 调用方决定 spawn
//!    **不阻塞**: 此步 fire-and-forget, 不在 ingest 主路径上加 .await
//!
//! ## 不阻塞保证 (顶层约束)
//!
//! - `ingest_sync` (步骤 1-5): 纯字符串操作, 无锁, 无 await, 同步极速 (微秒级)
//! - `palace_register_async` (步骤 6): 接收 `&DualPalaceMemory` 句柄, 内部
//!   `tokio::spawn` 包装后返回 `JoinHandle`. 调用方可以 `.await` 等待完成,
//!   也可以 detach 让其在后台跑. **绝不在关键路径上同步阻塞**.
//! - 不修改 `static MANIFEST` (OnceLock 不可变, 设计上 read-only).
//!   合并由调用方负责 (返回 `ToolEntry` 让调用方 merge).
//!
//! ## 使用示例
//!
//! ```ignore
//! use crate::tool::ingest::{ingest_sync, ExternalToolSpec, ExternalSource};
//!
//! let spec = ExternalToolSpec {
//!     name: "github.list_issues".into(),
//!     description: "List issues in a GitHub repository".into(),
//!     parameters: vec![ParamSpec { name: "repo".into(), kind: ParamKind::String, required: true }],
//!     entry_point: "https://api.github.com/...".into(),
//!     source_kind: ExternalSource::Mcp,
//! };
//!
//! // 步骤 1-5 同步极速 (无锁无 await)
//! let ingested = ingest_sync(&spec);
//!
//! // 步骤 6 fire-and-forget (后台 Palace 注册, 不阻塞当前流程)
//! if let Some(palace) = core.memory_palace.as_ref() {
//!     let _ = palace_register_async(palace.clone(), &ingested);
//! }
//! ```

use crate::core::silent_router::Domain;
use crate::memory_palace::{extract_tool_domain, DualPalaceMemory, KnowledgeEntry};
use crate::tool::manifest::ToolEntry;

use abacus_types::{PluginToolSpec, ToolHandle};
use serde_json::Value;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 外部输入定义
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 外部工具来源类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalSource {
    /// MCP (Model Context Protocol) server
    Mcp,
    /// 用户安装的 Plugin (二进制/脚本)
    Plugin,
    /// 用户自定义脚本 (Rhai / Python sandbox)
    Custom,
    /// 外部 Agent
    Agent,
}

impl ExternalSource {
    /// 用于 Palace tag 分类 (`["external", "mcp"]` / `["external", "plugin"]` / `["external", "custom"]`)
    pub fn tag(self) -> &'static str {
        match self {
            ExternalSource::Mcp => "mcp",
            ExternalSource::Plugin => "plugin",
            ExternalSource::Custom => "custom",
            ExternalSource::Agent => "agent",
        }
    }
}

/// 参数规格 (外部工具暴露的输入参数)
#[derive(Debug, Clone)]
pub struct ParamSpec {
    pub name: String,
    pub kind: ParamKind,
    pub required: bool,
    /// 简短说明 (≤ 80 chars)
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    String,
    Number,
    Boolean,
    Object,
    Array,
}

/// 外部工具的原始元数据
#[derive(Debug, Clone)]
pub struct ExternalToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Vec<ParamSpec>,
    pub entry_point: String,
    pub source_kind: ExternalSource,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 处理结果
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 6 步处理后的完整工具元数据
#[derive(Debug, Clone)]
pub struct IngestedTool {
    /// 可直接合并到 manifest 的 `ToolEntry`
    pub entry: ToolEntry,
    /// 推断的 domain (0-7)
    pub domain: Domain,
    /// 推断的 cluster 名
    pub cluster: String,
    /// 推断的场景列表 (clarify/plan/team/meeting)
    pub scenes: Vec<String>,
    /// 用于 Palace 注册的稳定 ID (`external:{source}:{name}`)
    pub palace_id: String,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 步骤 1-5: 同步极速 (不阻塞)
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 步骤 1-5 同步执行 — 纯字符串操作, 无锁, 无 await.
///
/// ## 复杂度
/// O(description.len() + parameters.len()) — 微秒级完成
///
/// ## 调用约束
/// - 可在主循环 / event handler / install 回调中直接调用
/// - 不持有任何全局锁
/// - 不修改 `static MANIFEST` (返回新 `ToolEntry` 让调用方 merge)
pub fn ingest_sync(spec: &ExternalToolSpec) -> IngestedTool {
    let domain = detect_domain(spec);
    let (tokens, latency) = estimate_cost(spec);
    let cluster = assign_cluster(spec, domain);
    let short_description = generate_short_description(&spec.description);
    let scenes = map_scene_prefixes(&spec.name);
    let actions = infer_actions(&spec.name, &spec.description);

    let entry = ToolEntry {
        name: spec.name.clone(),
        description: spec.description.clone(),
        short_description: Some(short_description.clone()),
        cluster: Some(cluster.clone()),
        differentiator: Some(actions.first().cloned().unwrap_or_default()),
        tokens,
        latency: latency.clone(),
        risk: estimate_risk(spec),
        confirm: matches!(domain, Domain::Fs | Domain::Code | Domain::Infra),
        idempotent: estimate_idempotent(&spec.name, &spec.description),
        domains: Some(vec![domain as usize]),
        actions: Some(actions),
        scenes: Some(scenes.clone()),
    };

    IngestedTool {
        entry,
        domain,
        cluster,
        scenes,
        palace_id: format!("external:{}:{}", spec.source_kind.tag(), spec.name),
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 接入辅助: 从 MCP ToolHandle / PluginToolSpec 构造 ExternalToolSpec
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 从 MCP `ToolHandle` 构造 `ExternalToolSpec` 并跑 ingest
///
/// ## 用法 (在 enable_mcp 注册循环里):
/// ```ignore
/// for handle in tools {
///     let ingested = ingest_from_handle(&handle);
///     crate::tool::manifest::merge_external(vec![ingested.entry.clone()]);
///     if let Some(palace) = core.memory_palace.as_ref() {
///         let _ = palace_register_async(palace.clone(), &ingested);
///     }
///     self.registry.register(handle).await;
/// }
/// ```
///
/// ## 不阻塞
/// - `ingest_sync` 微秒级（无锁无 await）
/// - `merge_external` 微秒级（std RwLock 不跨 await）
/// - `palace_register_async` 立即返回 JoinHandle（fire-and-forget）
pub fn ingest_from_handle(handle: &ToolHandle) -> IngestedTool {
    let spec = spec_from_handle(handle);
    ingest_sync(&spec)
}

/// 从 Plugin `PluginToolSpec` 构造 `ExternalToolSpec` 并跑 ingest
///
/// ## 用法 (在 plugin load 后):
/// ```ignore
/// for tool in &manifest.tools {
///     let ingested = ingest_from_plugin_tool(&manifest.id, tool);
///     crate::tool::manifest::merge_external(vec![ingested.entry.clone()]);
/// }
/// ```
pub fn ingest_from_plugin_tool(plugin_id: &str, tool: &PluginToolSpec) -> IngestedTool {
    let spec = spec_from_plugin_tool(plugin_id, tool);
    ingest_sync(&spec)
}

/// 内部: `ToolHandle` → `ExternalToolSpec`
fn spec_from_handle(handle: &ToolHandle) -> ExternalToolSpec {
    // 提取参数 (从 schema.parameters JSON Object 推断 kind)
    let parameters = extract_params_from_json(&handle.schema.parameters);
    // entry_point 从 provider 信息推断 (Mcp 工具的 entry_point 来自 server 地址)
    let entry_point = match &handle.provider {
        abacus_types::ToolProvider::Mcp { server_id } => {
            format!("mcp://{}", server_id)
        }
        other => format!("provider:{:?}", other),
    };
    ExternalToolSpec {
        name: handle.id.0.clone(),
        description: handle.schema.description.clone(),
        parameters,
        entry_point,
        source_kind: ExternalSource::Mcp,
    }
}

/// 内部: `PluginToolSpec` → `ExternalToolSpec`
fn spec_from_plugin_tool(plugin_id: &str, tool: &PluginToolSpec) -> ExternalToolSpec {
    let parameters = extract_params_from_json(&tool.parameters);
    ExternalToolSpec {
        name: format!("{}.{}", plugin_id, tool.name),
        description: tool.description.clone(),
        parameters,
        entry_point: format!("plugin://{}", plugin_id),
        source_kind: ExternalSource::Plugin,
    }
}

/// 从 JSON Schema Value 提取参数列表 (简化版, 顶层 properties)
fn extract_params_from_json(value: &Value) -> Vec<ParamSpec> {
    let mut params = Vec::new();
    let Some(obj) = value.as_object() else { return params; };

    // 支持 { "properties": {...}, "required": [...] } 格式
    let properties = obj.get("properties").and_then(|p| p.as_object());
    let required_list: Vec<String> = obj.get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    if let Some(props) = properties {
        for (name, def) in props {
            let kind = param_kind_from_json(def);
            let required = required_list.contains(name);
            let description = def.get("description")
                .and_then(|d| d.as_str())
                .map(String::from);
            params.push(ParamSpec { name: name.clone(), kind, required, description });
        }
    } else if value.is_object() {
        // 兜底: 整个 value 是个 Object, 取它的 keys 当 params
        for (name, _) in obj {
            params.push(ParamSpec {
                name: name.clone(),
                kind: ParamKind::Object,
                required: false,
                description: None,
            });
        }
    }
    params
}

fn param_kind_from_json(def: &Value) -> ParamKind {
    let Some(ty) = def.get("type").and_then(|t| t.as_str()) else {
        return ParamKind::Object;
    };
    match ty {
        "string" => ParamKind::String,
        "number" | "integer" => ParamKind::Number,
        "boolean" => ParamKind::Boolean,
        "array" => ParamKind::Array,
        _ => ParamKind::Object,
    }
}

/// 批量 ingest 多个 tool 并 merge (用于 Plugin 一次性注册多个 tool)
///
/// ## 不阻塞
/// - ingest 同步极速
/// - `merge_external` 微秒级 (一次性写入所有 entry)
/// - 返回 `Vec<IngestedTool>` 让调用方决定 Palace 注册策略
pub fn ingest_and_merge_batch(specs: Vec<ExternalToolSpec>) -> Vec<IngestedTool> {
    let ingested: Vec<IngestedTool> = specs.iter().map(ingest_sync).collect();
    let entries: Vec<ToolEntry> = ingested.iter().map(|i| i.entry.clone()).collect();
    crate::tool::manifest::merge_external(entries);
    ingested
}

// ─── 步骤 1: Domain 检测 ────────────────────────────────────────

fn detect_domain(spec: &ExternalToolSpec) -> Domain {
    // 信号 1: 工具名 dot/underscore 前缀 (复用 extract_tool_domain)
    let name_domain = extract_tool_domain(&spec.name);
    if let Some(d) = domain_from_str(name_domain) {
        return d;
    }

    // 信号 2: 描述关键词扫描 (兜底)
    let desc_lower = spec.description.to_lowercase();
    for (kw, d) in DOMAIN_KEYWORDS {
        if desc_lower.contains(kw) {
            return *d;
        }
    }

    // 信号 3: entry_point 启发 (URL 包含 "github" "gitlab" → Web)
    if spec.entry_point.contains("://") {
        if let Some(d) = classify_url(&spec.entry_point) {
            return d;
        }
    }

    // 默认: General → Text (最保守的归类, 渲染层会兜底显示)
    Domain::Text
}

const DOMAIN_KEYWORDS: &[(&str, Domain)] = &[
    ("search", Domain::Web),
    ("fetch", Domain::Web),
    ("download", Domain::Web),
    ("http", Domain::Web),
    ("api", Domain::Web),
    ("query", Domain::Data),
    ("database", Domain::Data),
    ("sql", Domain::Data),
    ("table", Domain::Data),
    ("file", Domain::Fs),
    ("directory", Domain::Fs),
    ("path", Domain::Fs),
    ("execute", Domain::Code),
    ("compile", Domain::Code),
    ("run", Domain::Code),
    ("sandbox", Domain::Code),
    ("config", Domain::Config),
    ("setting", Domain::Config),
    ("env", Domain::Config),
    ("session", Domain::Session),
    ("history", Domain::Session),
];

fn domain_from_str(s: &str) -> Option<Domain> {
    match s {
        "code" | "rhai" | "exec" => Some(Domain::Code),
        "infra" | "shell" | "bash" => Some(Domain::Infra),
        "data" | "db" | "sql" => Some(Domain::Data),
        "text" | "doc" | "format" => Some(Domain::Text),
        "config" | "settings" => Some(Domain::Config),
        "web" | "http" | "search" | "fetch" => Some(Domain::Web),
        "fs" | "filengine" | "file" | "path" => Some(Domain::Fs),
        "session" | "history" | "memory" => Some(Domain::Session),
        _ => None,
    }
}

fn classify_url(url: &str) -> Option<Domain> {
    let lower = url.to_lowercase();
    if lower.contains("github.com") || lower.contains("gitlab.com") {
        Some(Domain::Code)
    } else if lower.contains("api.") || lower.contains("http") {
        Some(Domain::Web)
    } else {
        None
    }
}

// ─── 步骤 2: Cost 估算 ─────────────────────────────────────────

fn estimate_cost(spec: &ExternalToolSpec) -> (u32, String) {
    let desc_lower = spec.description.to_lowercase();
    let has_io = ["read", "write", "fetch", "list", "search", "query", "open"]
        .iter()
        .any(|k| desc_lower.contains(k));
    let has_compute = ["execute", "compute", "transform", "process", "compile"]
        .iter()
        .any(|k| desc_lower.contains(k));
    let has_network = ["http", "api", "url", "download", "upload"]
        .iter()
        .any(|k| desc_lower.contains(k) || spec.entry_point.contains(k));

    // 网络类工具: token 估值高 + latency 长
    if has_network {
        return (80, "500ms-5s".to_string());
    }
    // 计算密集: 中等 token + 中等 latency
    if has_compute {
        return (48, "200ms-2s".to_string());
    }
    // I/O 类: 中等 token + 快
    if has_io {
        return (64, "100ms-1s".to_string());
    }
    // 默认
    (48, "100ms".to_string())
}

fn estimate_risk(spec: &ExternalToolSpec) -> String {
    let desc_lower = spec.description.to_lowercase();
    let destructive = ["delete", "remove", "drop", "destroy", "wipe", "truncate"]
        .iter()
        .any(|k| desc_lower.contains(k));
    if destructive {
        "high".to_string()
    } else if spec.parameters.iter().any(|p| matches!(p.kind, ParamKind::Array | ParamKind::Object)) {
        "medium".to_string()
    } else {
        "low".to_string()
    }
}

fn estimate_idempotent(name: &str, description: &str) -> bool {
    let lower = format!("{} {}", name, description).to_lowercase();
    // 创建/删除/递增类操作非幂等
    !["create", "delete", "remove", "add", "increment", "decrement", "set "]
        .iter()
        .any(|k| lower.contains(k))
}

// ─── 步骤 3: Cluster 分配 ──────────────────────────────────────

fn assign_cluster(spec: &ExternalToolSpec, domain: Domain) -> String {
    match domain {
        Domain::Web => "web_research".to_string(),
        Domain::Data => "data_query".to_string(),
        Domain::Code => "code_execute".to_string(),
        Domain::Fs => {
            // 基于 name + description 联合判断读写 (中文 desc 不可靠)
            let lower = format!("{} {}", spec.name, spec.description).to_lowercase();
            let write_signals = ["write", "create", "delete", "remove", "mkdir", "rm ", "touch", "edit", "save"];
            if write_signals.iter().any(|k| lower.contains(k)) {
                "fs_write_mutate".to_string()
            } else {
                "fs_read_discover".to_string()
            }
        }
        Domain::Text => "text_transform".to_string(),
        Domain::Config => "config_admin".to_string(),
        Domain::Infra => "infra_ops".to_string(),
        Domain::Session => "session_meta".to_string(),
    }
}

// ─── 步骤 4: Short description ────────────────────────────────

fn generate_short_description(description: &str) -> String {
    const MAX_CHARS: usize = 60;
    // 优先: 第一句 (中英句号 + 问号 + 感叹号)
    let first_sentence = description
        .split(|c: char| c == '。' || c == '.' || c == '!' || c == '?' || c == '！' || c == '？')
        .next()
        .unwrap_or(description)
        .trim();

    if first_sentence.chars().count() <= MAX_CHARS {
        first_sentence.to_string()
    } else {
        // 截断到 MAX_CHARS 字符 (注意是 char 不是 byte, 避免中文 UTF-8 切碎)
        first_sentence.chars().take(MAX_CHARS).collect::<String>() + "…"
    }
}

// ─── 步骤 5: Scene prefix 映射 ────────────────────────────────

fn map_scene_prefixes(name: &str) -> Vec<String> {
    let lower = name.to_lowercase();
    let mut scenes = Vec::new();

    // 知识检索类 → clarify
    if lower.starts_with("kb_") || lower.starts_with("memory_") || lower.starts_with("search_kb") {
        scenes.push("clarify".to_string());
    }
    // 网络研究类 → team
    if lower.starts_with("web_") || lower.starts_with("http_") || lower.starts_with("search_") {
        scenes.push("team".to_string());
    }
    // 文件 / 代码 / 数据操作类 → plan
    if lower.starts_with("fs_") || lower.starts_with("code_") || lower.starts_with("db_") {
        scenes.push("plan".to_string());
    }

    // 兜底: 至少挂到 plan scene
    if scenes.is_empty() {
        scenes.push("plan".to_string());
    }
    scenes
}

// ─── 辅助: 推断 actions ────────────────────────────────────────

fn infer_actions(name: &str, description: &str) -> Vec<String> {
    let lower = format!("{} {}", name, description).to_lowercase();
    let mut actions = Vec::new();

    if lower.contains("search") || lower.contains("find") {
        actions.push("Search".to_string());
    }
    if lower.contains("read") || lower.contains("get") || lower.contains("fetch") {
        actions.push("Read".to_string());
    }
    if lower.contains("write") || lower.contains("update") || lower.contains("set") {
        actions.push("Update".to_string());
    }
    if lower.contains("create") || lower.contains("add") || lower.contains("new") {
        actions.push("Create".to_string());
    }
    if lower.contains("delete") || lower.contains("remove") {
        actions.push("Delete".to_string());
    }
    if lower.contains("execute") || lower.contains("run") {
        actions.push("Execute".to_string());
    }
    if lower.contains("analyze") || lower.contains("transform") {
        actions.push("Analyze".to_string());
    }

    if actions.is_empty() {
        actions.push("Read".to_string());
    }
    actions
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 步骤 6: Palace 注册 (不阻塞, fire-and-forget)
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 步骤 6: 把 `IngestedTool` 写入 Memory Palace, 用于跨 session 推荐.
///
/// ## 不阻塞保证
/// - 内部 `tokio::spawn` 包装, 立即返回 `JoinHandle<()>`
/// - 调用方可以 `.await` 等完成, 也可以 `drop(handle)` 让其在后台跑
/// - Palace 写入走原有 `tokio::sync::RwLock`, 与主循环并发不冲突
///
/// ## 典型用法 (fire-and-forget)
/// ```ignore
/// if let Some(palace) = core.memory_palace.as_ref() {
///     let _ = palace_register_async(palace.clone(), &ingested);
///     // 立刻继续, 不等 Palace 写完
/// }
/// ```
pub fn palace_register_async(
    palace: std::sync::Arc<tokio::sync::RwLock<DualPalaceMemory>>,
    ingested: &IngestedTool,
) -> tokio::task::JoinHandle<()> {
    let id = ingested.palace_id.clone();
    let title = ingested.entry.name.clone();
    let content = format!(
        "{}\n\nDomain: {:?}\nCluster: {}\nScenes: {}\nTokens: {}, Latency: {}\nRisk: {}\nActions: {:?}",
        ingested.entry.description,
        ingested.domain,
        ingested.cluster,
        ingested.scenes.join(", "),
        ingested.entry.tokens,
        ingested.entry.latency,
        ingested.entry.risk,
        ingested.entry.actions,
    );
    let domain_str = format!("{:?}", ingested.domain).to_lowercase();
    let tags = vec![
        "external".to_string(),
        "tool".to_string(),
        ingested.cluster.clone(),
    ];

    tokio::spawn(async move {
        let mut entry = KnowledgeEntry::new(&id, &title, &content, &domain_str);
        entry.tags = tags;

        // 自动创建工具实体
        entry.entities.push(abacus_core::memory_palace::KnowledgeEntity {
            name: title.clone(),
            entity_type: "tool".into(),
            description: ingested.entry.description.clone(),
        });

        // 创建工具→领域关系
        entry.relations.push(abacus_core::memory_palace::KnowledgeRelation {
            source: title.clone(),
            target: domain_str.clone(),
            relation_type: "belongs_to".into(),
            time: None,
        });

        // 为每个 action 创建实体
        if let Some(ref actions) = ingested.entry.actions {
            for action in actions {
                entry.entities.push(abacus_core::memory_palace::KnowledgeEntity {
                    name: format!("{}::{}", title, action),
                    entity_type: "action".into(),
                    description: format!("{} 的 {} 操作", title, action),
                });
            }
        }

        let p = palace.read().await;
        let _ = p.store_knowledge(entry).await;
    })
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 单元测试
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[cfg(test)]
mod tests {
    use super::*;

    fn mcp_github_spec() -> ExternalToolSpec {
        ExternalToolSpec {
            name: "github.list_issues".to_string(),
            description: "List issues in a GitHub repository, supports filter and sort.".to_string(),
            parameters: vec![
                ParamSpec { name: "repo".into(), kind: ParamKind::String, required: true, description: None },
                ParamSpec { name: "state".into(), kind: ParamKind::String, required: false, description: Some("open/closed/all".into()) },
            ],
            entry_point: "https://api.github.com/repos/{repo}/issues".to_string(),
            source_kind: ExternalSource::Mcp,
        }
    }

    fn mcp_filesystem_spec() -> ExternalToolSpec {
        ExternalToolSpec {
            name: "fs_mkdir".to_string(),
            description: "递归创建目录（含所有父目录）".to_string(),
            parameters: vec![
                ParamSpec { name: "path".into(), kind: ParamKind::String, required: true, description: None },
            ],
            entry_point: "stdio://mcp-fs".to_string(),
            source_kind: ExternalSource::Mcp,
        }
    }

    fn custom_script_spec() -> ExternalToolSpec {
        ExternalToolSpec {
            name: "rhai.transform_csv".to_string(),
            description: "Execute a Rhai script to transform a CSV file in place.".to_string(),
            parameters: vec![
                ParamSpec { name: "script".into(), kind: ParamKind::String, required: true, description: None },
            ],
            entry_point: "sandbox://rhai".to_string(),
            source_kind: ExternalSource::Custom,
        }
    }

    #[test]
    fn ingest_github_lists_issues() {
        let spec = mcp_github_spec();
        let ing = ingest_sync(&spec);
        // 工具名 dot 分隔 → 优先 signal 1
        // 但 github 映射到 None, 兜底到 description 关键词 "list" 不在表里
        // entry_point "https://api.github.com" → classify_url → Code
        assert_eq!(ing.domain, Domain::Code);
        assert_eq!(ing.cluster, "code_execute");
        assert!(ing.scenes.contains(&"plan".to_string()));
        assert!(ing.entry.actions.as_ref().unwrap().contains(&"Read".to_string()));
        // "list" 不在非幂等词表, "fetch" 也不在 → 幂等
        assert!(ing.entry.idempotent);
        assert!(ing.entry.short_description.is_some());
        // 短描述 ≤ 60 chars
        assert!(ing.entry.short_description.as_ref().unwrap().chars().count() <= 60);
        assert_eq!(ing.palace_id, "external:mcp:github.list_issues");
    }

    #[test]
    fn ingest_filesystem_mkdir() {
        let spec = mcp_filesystem_spec();
        let ing = ingest_sync(&spec);
        // fs_mkdir → extract_tool_domain → "fs" → Domain::Fs
        assert_eq!(ing.domain, Domain::Fs);
        // 描述含 "创建" + "目录" → write cluster
        assert_eq!(ing.cluster, "fs_write_mutate");
        assert!(ing.entry.confirm); // Fs domain 默认 confirm
        // scenes: fs_* → plan
        assert!(ing.scenes.contains(&"plan".to_string()));
    }

    #[test]
    fn ingest_rhai_sandbox() {
        let spec = custom_script_spec();
        let ing = ingest_sync(&spec);
        // rhai → extract_tool_domain "rhai" → Domain::Code
        assert_eq!(ing.domain, Domain::Code);
        // "Execute" + "transform" → has_compute → cost estimate
        assert!(ing.entry.tokens > 0);
        assert!(!ing.entry.latency.is_empty());
    }

    #[test]
    fn short_description_chinese_no_utf8_break() {
        // 中文描述：每字符 3 byte, 必须按 char 截, 不能按 byte
        let desc = "这是一个非常长的中文描述用于测试 short_description 截断功能是否正确处理 UTF-8 字符边界";
        let short = generate_short_description(desc);
        // 必须有效 UTF-8 (Rust & str 保证)
        let _ = short.chars().count();
        // 不会 panic, 字符串合法
        assert!(short.chars().count() <= 61); // 60 + "…"
    }

    #[test]
    fn short_description_english_truncate() {
        let desc = "This is a very long English description that should be truncated to sixty characters total";
        let short = generate_short_description(desc);
        assert!(short.ends_with('…') || short.chars().count() <= 60);
    }

    #[test]
    fn risk_high_for_destructive() {
        let spec = ExternalToolSpec {
            name: "fs_delete".to_string(),
            description: "Delete a file or directory permanently".to_string(),
            parameters: vec![],
            entry_point: "stdio://fs".to_string(),
            source_kind: ExternalSource::Mcp,
        };
        let ing = ingest_sync(&spec);
        assert_eq!(ing.entry.risk, "high");
    }

    #[test]
    fn idempotent_false_for_create() {
        let spec = ExternalToolSpec {
            name: "fs_create".to_string(),
            description: "Create a new file".to_string(),
            parameters: vec![],
            entry_point: "stdio://fs".to_string(),
            source_kind: ExternalSource::Mcp,
        };
        let ing = ingest_sync(&spec);
        assert!(!ing.entry.idempotent);
    }

    #[test]
    fn scene_kb_tools_go_to_clarify() {
        let spec = ExternalToolSpec {
            name: "kb_search".to_string(),
            description: "Search the knowledge base".to_string(),
            parameters: vec![],
            entry_point: "memory://".to_string(),
            source_kind: ExternalSource::Plugin,
        };
        let ing = ingest_sync(&spec);
        assert!(ing.scenes.contains(&"clarify".to_string()));
    }

    #[test]
    fn ingest_does_not_block_under_load() {
        // 同步路径下, 1000 个 spec 应该在毫秒级完成 (基准)
        // 不持有任何全局锁, 不 await
        let start = std::time::Instant::now();
        for i in 0..1000 {
            let spec = ExternalToolSpec {
                name: format!("test.tool_{i}"),
                description: format!("A test tool {i} for measuring ingest throughput under load"),
                parameters: vec![],
                entry_point: "stdio://test".to_string(),
                source_kind: ExternalSource::Custom,
            };
            let _ing = ingest_sync(&spec);
        }
        let elapsed = start.elapsed();
        // 1000 次 ≤ 1 秒 (实测通常 < 100ms)
        assert!(elapsed.as_secs() < 1, "ingest too slow: {:?}", elapsed);
    }

    // ─── 接入辅助测试 ──────────────────────────────────────

    #[test]
    fn ingest_from_handle_mcp() {
        use abacus_types::{ToolHandle, ToolId, ToolProvider, ToolSchema, ToolState};
        let handle = ToolHandle {
            id: ToolId("mcp_github_list_issues".into()),
            schema: ToolSchema {
                name: "mcp_github_list_issues".into(),
                description: "List issues in a GitHub repository".into(),
                short_description: None,
                cost: None,
                security: None,
                idempotent: true,
                schema_stable: false,
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string"},
                        "state": {"type": "string"}
                    },
                    "required": ["repo"]
                }),
                returns: None,
                examples: vec![],
                applicable_task_kinds: None,
            },
            provider: ToolProvider::Mcp { server_id: "github".into() },
            state: ToolState::Loaded,
            effectiveness: Default::default(),
        };

        let ing = ingest_from_handle(&handle);
        assert_eq!(ing.entry.name, "mcp_github_list_issues");
        assert!(ing.entry.short_description.is_some());
        // description 含 "List" → I/O 路径 → tokens=64, latency="100ms-1s"
        assert_eq!(ing.entry.tokens, 64);
        assert_eq!(ing.entry.latency, "100ms-1s");
        // domain: entry_point "mcp://github" 不在 domain_from_str 表,
        // 兜底 description 关键词 — "List" 不在表里, 走 Text 兜底
        let _ = ing.domain;
    }

    #[test]
    fn ingest_from_plugin_tool_test() {
        let tool = PluginToolSpec {
            name: "transform_csv".into(),
            description: "Execute a Rhai script to transform CSV data".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "script": {"type": "string"},
                    "input_path": {"type": "string"}
                },
                "required": ["script"]
            }),
        };
        let ing = ingest_from_plugin_tool("csv_tools", &tool);
        // plugin_id.name 命名
        assert_eq!(ing.entry.name, "csv_tools.transform_csv");
        // rhai 域
        assert_eq!(ing.domain, Domain::Code);
    }

    #[test]
    fn ingest_and_merge_batch_merges_all() {
        use crate::tool::manifest;
        // 清空 overlay
        let existing = manifest::list_external();
        let existing_refs: Vec<&str> = existing.iter().map(String::as_str).collect();
        manifest::unmerge_external(&existing_refs);

        let specs = vec![
            ExternalToolSpec {
                name: "batch.test_a".into(),
                description: "A test tool".into(),
                parameters: vec![],
                entry_point: "stdio://batch".into(),
                source_kind: ExternalSource::Custom,
            },
            ExternalToolSpec {
                name: "batch.test_b".into(),
                description: "B test tool".into(),
                parameters: vec![],
                entry_point: "stdio://batch".into(),
                source_kind: ExternalSource::Custom,
            },
        ];

        let ingested = ingest_and_merge_batch(specs);
        assert_eq!(ingested.len(), 2);
        // 应该都合并到 overlay
        assert!(manifest::index().get("batch.test_a").is_some());
        assert!(manifest::index().get("batch.test_b").is_some());
    }
}
