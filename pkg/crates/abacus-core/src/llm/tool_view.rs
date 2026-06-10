//! ToolSchema → ToolFunctionSpec 转换 —— LLM 可见视图
//!
//! ## 设计目标（Layer 2）
//! 把 ToolSchema 暴露给 LLM 时只输出 LLM 真正需要的字段：
//! - `name` (直接复用 schema.name —— 已是 [a-zA-Z0-9_-] 形态)
//! - `description` (含 provenance prefix + cost suffix + cooling suffix)
//! - `parameters` (JSON Schema)
//!
//! ## 自动剔除（不发给 LLM）
//! - `security` —— 后端 SafetyGuard 用，LLM 不需要看（曾占 23.2% schema 字节）
//! - `returns` —— 大多为 None，序列化空字段浪费 token
//! - `examples` —— 当前未启用，预留字段
//! - `applicable_task_kinds` —— 路由用，LLM 不需感知
//! - `idempotent` —— effectiveness 内部用
//! - `cost` —— 转成紧凑 suffix 拼到 description（避免完整 cost 对象）
//!
//! ## 命名约定（去 sanitize 化）
//! schema.name == ToolId.0 == LLM 调用名 == 内部 dispatch 键，全部使用
//! `[a-zA-Z0-9_-]` 字符集（OpenAI/DeepSeek 工具名协议要求）。注册 builtin 时
//! 直接用下划线（`fs_read`），MCP/Plugin/Skill 在 ingest 时一次性
//! sanitize（仅一次）。下游 dispatch 不再做 O(N) 反查。
//!
//! ## 引用关系
//! - 被 [`crate::core::CoreLoop::build_tool_definitions_for`] 调用
//! - 不持有状态——纯函数转换
//!
//! ## 生命周期
//! 每次 LLM 请求构造时调用一次；输出 ToolFunctionSpec 短命周期
//! （随 LlmRequest 移交 provider 后立即可释放）。

use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::{LazyLock, RwLock};

use abacus_types::{ToolHandle, ToolProvider, ToolSchema, ToolState};
use lru::LruCache;

use crate::llm::provider::ToolFunctionSpec;

// ─── Tool Schema Cache ───────────────────────────────────────────────────────

/// 工具 schema 缓存键——hash(name | description | parameters)
#[derive(Clone)]
struct ToolSpecKey {
    full_hash: u64,
}

impl ToolSpecKey {
    fn from_spec(name: &str, desc: &str, params: &serde_json::Value) -> Self {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut h);
        desc.hash(&mut h);
        params.hash(&mut h);
        Self { full_hash: h.finish() }
    }
}

/// 工具 schema 缓存——LRU，128 cap，全 provider 共享
struct ToolSpecCache {
    inner: RwLock<LruCache<u64, ToolFunctionSpec>>,
}

impl ToolSpecCache {
    fn new(cap: usize) -> Self {
        Self { inner: RwLock::new(LruCache::new(NonZeroUsize::new(cap).expect("cap > 0"))) }
    }

    fn get(&self, key: &ToolSpecKey) -> Option<ToolFunctionSpec> {
        self.inner.write().ok().and_then(|mut cache| cache.get(&key.full_hash).cloned())
    }

    fn insert(&self, key: ToolSpecKey, spec: ToolFunctionSpec) {
        if let Ok(mut cache) = self.inner.write() {
            cache.put(key.full_hash, spec);
        }
    }
}

static TOOL_SPEC_CACHE: LazyLock<ToolSpecCache> = LazyLock::new(|| ToolSpecCache::new(128));

/// 把 ToolHandle 转为 LLM 可见的 ToolFunctionSpec
///
/// ## 参数
/// `handle` 提供 schema + provider + state（cooling 状态需要 state）
///
/// ## 输出 description 拼装顺序
/// `[provenance_prefix][raw_description][cost_suffix][cooling_suffix]`
///
/// 各 suffix byte-stable —— 不破 KV cache 前缀。
///
/// ## 缓存
/// Cache hit 时跳过 format! 拼装——省 ~3000 tok/轮 schema 重建。
pub fn tool_handle_to_llm_spec(handle: &ToolHandle) -> ToolFunctionSpec {
    let key = ToolSpecKey::from_spec(
        &handle.schema.name,
        handle.schema.description.as_str(),
        &handle.schema.parameters,
    );

    if let Some(cached) = TOOL_SPEC_CACHE.get(&key) {
        return cached;
    }

    // cache miss: 构建逻辑
    let provenance_prefix = provenance_prefix_for(&handle.provider);
    let cost_suffix = cost_suffix_for(&handle.schema);
    let cooling_suffix = cooling_suffix_for(handle);

    let description = format!(
        "{}{}{}{}",
        provenance_prefix,
        handle.schema.description,
        cost_suffix,
        cooling_suffix,
    );

    // schema.name 已是 LLM 协议合规形态（注册时一次性保证），直接 clone 不再 sanitize。
    let spec = ToolFunctionSpec {
        name: handle.schema.name.clone(),
        description: Some(description),
        parameters: handle.schema.parameters.clone(),
        strict: None,
    };

    TOOL_SPEC_CACHE.insert(key, spec.clone());
    spec
}

/// 把外部来源（MCP/Plugin/Skill）的工具名规范化到 LLM 协议要求的字符集。
///
/// ## 适用场景
/// 仅在**注册时一次性**调用——MCP server 返回的 tool.name 可能含 `.` `/`，
/// 此处统一转 `_`，确保 ToolId.0 与 schema.name 一开始就合规。
///
/// ## 不变量
/// 注册后所有运行时路径（dispatch / 路由 / safety / undo）使用规范化后的名字，
/// 不再做反向查找。
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// Provenance prefix —— 让 LLM 通过 description 感知工具来源
fn provenance_prefix_for(provider: &ToolProvider) -> String {
    match provider {
        ToolProvider::BuiltIn => String::new(),
        ToolProvider::Mcp { server_id } => format!("[External MCP server: {server_id}] "),
        ToolProvider::Plugin { plugin_id } => format!("[WASM plugin: {plugin_id}] "),
        ToolProvider::Skill { skill_id } => format!("[Skill workflow step from '{skill_id}'] "),
    }
}

/// Cost 紧凑 suffix —— ` [~64t/500ms/low]`
///
/// Task #86：从原 ` [cost: ~64tok, 500ms, low-risk]` (33B) 缩到 17B，省 16B/工具
fn cost_suffix_for(schema: &ToolSchema) -> String {
    schema.cost.as_ref()
        .map(|c| format!(" [~{}t/{}/{}]", c.tokens, c.latency, c.risk))
        .unwrap_or_default()
}

/// Cooling suffix —— 状态为 Cooling 时附加 turn 数提示
fn cooling_suffix_for(handle: &ToolHandle) -> String {
    if handle.state == ToolState::Cooling {
        format!(" [cooling: {} turns remaining]", handle.effectiveness.cooldown_remaining)
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_types::{ToolCost, ToolEffectiveness, ToolId, VisibilityTier};

    fn mk_handle(name: &str, desc: &str, cost: Option<ToolCost>, provider: ToolProvider, state: ToolState) -> ToolHandle {
        ToolHandle {
            id: ToolId(name.into()),
            schema: ToolSchema { short_description: None,
                name: name.into(),
                description: desc.into(),
                parameters: serde_json::json!({"type": "object"}),
                returns: None, security: None, cost,
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: false,
                                schema_stable: false,            },
            provider,
            state,
            effectiveness: ToolEffectiveness {
                tool_id: ToolId(name.into()),
                composite_score: 0.5,
                tier: VisibilityTier::C,
                cooldown_remaining: 3,
                blocked_by_env: false,
                insufficient_data: false,
            },
        }
    }

    #[test]
    fn builtin_no_prefix() {
        let h = mk_handle("fs_read", "read file", None, ToolProvider::BuiltIn, ToolState::Loaded);
        let spec = tool_handle_to_llm_spec(&h);
        assert_eq!(spec.description.as_deref(), Some("read file"));
    }

    #[test]
    fn mcp_has_prefix() {
        let h = mk_handle("foo", "bar", None, ToolProvider::Mcp { server_id: "srv1".into() }, ToolState::Loaded);
        let spec = tool_handle_to_llm_spec(&h);
        assert!(spec.description.as_ref().unwrap().contains("[External MCP server: srv1]"));
    }

    #[test]
    fn cost_compact_format() {
        let cost = ToolCost { tokens: 64, latency: "500ms".into(), risk: "low".into() };
        let h = mk_handle("t", "do x", Some(cost), ToolProvider::BuiltIn, ToolState::Loaded);
        let spec = tool_handle_to_llm_spec(&h);
        assert!(spec.description.as_ref().unwrap().contains("[~64t/500ms/low]"));
    }

    #[test]
    fn cooling_suffix_when_cooling() {
        let h = mk_handle("t", "x", None, ToolProvider::BuiltIn, ToolState::Cooling);
        let spec = tool_handle_to_llm_spec(&h);
        assert!(spec.description.as_ref().unwrap().contains("[cooling: 3 turns"));
    }

    #[test]
    fn llm_spec_name_passthrough() {
        // schema.name 已是合规形态 → 直接传递，不再消毒。
        let h = mk_handle("fs_read", "x", None, ToolProvider::BuiltIn, ToolState::Loaded);
        let spec = tool_handle_to_llm_spec(&h);
        assert_eq!(spec.name, "fs_read");
    }

    #[test]
    fn sanitize_name_replaces_special_chars() {
        // sanitize_name 只在注册时（MCP/Plugin/Skill ingest 时）一次性调用。
        assert_eq!(sanitize_name("foo.bar"), "foo_bar");
        assert_eq!(sanitize_name("mcp/srv/tool"), "mcp_srv_tool");
        assert_eq!(sanitize_name("kept-dash_underscore09"), "kept-dash_underscore09");
    }
}
