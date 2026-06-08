//! Tool Manifest — single-source-of-truth for built-in tool metadata.
//!
//! ## Why
//! Previously, 6 subsystems each hardcoded the same tool metadata independently:
//! - `ClusterRegistry::builtin()`  — cluster grouping
//! - `scene_active_prefixes()`     — scene→prefix mapping
//! - `cost_suffix_for()`           — token/latency/risk display
//! - `short_description` injection — short-mode descriptions
//! - `SilentRouter` tool→domain    — domain/action mappings
//! - `generate_catalog()`          — provider grouping
//!
//! Every new tool or metadata change required editing *all 6 locations*. This module
//! replaces that with a single TOML file (`tools.toml`) loaded at startup, from which
//! all consumers derive their data automatically.
//!
//! ## Usage
//! ```ignore
//! let m = ToolManifest::load();
//! let cost = m.cost("fs_read");       // → ToolCost { tokens:64, latency:"10ms", risk:"low" }
//! let cluster = m.cluster("fs_read");  // → Some("fs_read_discover")
//! ```

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use serde::Deserialize;
use abacus_types::{ToolCost, ToolSchema, ToolSecurity};

/// Lazily-initialized global manifest index. Call `manifest::index()` from anywhere.
pub fn index() -> &'static ToolManifestIndex {
    static MANIFEST: OnceLock<ToolManifestIndex> = OnceLock::new();
    MANIFEST.get_or_init(|| ToolManifest::load().index())
}

/// 运行时 overlay — 外部工具 (MCP / Plugin / Custom) 的元数据层
///
/// ## 设计意图
/// 静态 `MANIFEST` 由 `tools.toml` 加载 (OnceLock 不可变), 外部工具元数据需要
/// 在运行时合并. 本 overlay 提供：
/// - 读路径: `index().get(name)` 等方法先查 overlay 再 fallback 到 base
/// - 写路径: `merge_external(entries)` / `unmerge_external(names)`
/// - 线程安全: `std::sync::RwLock` (同步、读路径无需 await, HashMap 操作微秒级)
///
/// ## 不阻塞保证
/// - `merge_external` / `unmerge_external` 是微秒级 HashMap 操作, 不会跨 await
/// - 在 async 上下文 (`enable_mcp`) 调用安全 — lock 不会持有跨 .await
/// - 读路径 `index().get()` 等也微秒级, 不阻塞主循环
///
/// ## V42-B 接入
/// 外部工具 ingest 完成后调 `merge_external`:
/// ```ignore
/// let ingested = ingest_sync(&spec);
/// manifest::merge_external(vec![ingested.entry]);
/// let _ = palace_register_async(palace.clone(), &ingested);
/// ```
pub fn overlay() -> &'static RwLock<HashMap<String, ToolEntry>> {
    static OVERLAY: OnceLock<RwLock<HashMap<String, ToolEntry>>> = OnceLock::new();
    OVERLAY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 把外部工具元数据合并到运行时 overlay
///
/// ## 调用约束
/// - 可在 async 上下文调用 (HashMap 写锁微秒级, 不跨 await)
/// - 重复 name 会覆盖 (新 entry 替换旧 entry)
/// - 不会触碰静态 `MANIFEST` (base 层不可变)
pub fn merge_external(entries: Vec<ToolEntry>) {
    if entries.is_empty() {
        return;
    }
    let mut guard = overlay().write().expect("manifest overlay poisoned");
    for e in entries {
        guard.insert(e.name.clone(), e);
    }
}

/// 从 overlay 移除外部工具 (MCP 断开 / Plugin 卸载时调用)
///
/// 不存在的 name 静默忽略 (幂等).
pub fn unmerge_external(names: &[&str]) {
    if names.is_empty() {
        return;
    }
    let mut guard = overlay().write().expect("manifest overlay poisoned");
    for n in names {
        guard.remove(*n);
    }
}

/// 列出当前 overlay 的所有工具名 (用于诊断 / 测试)
pub fn list_external() -> Vec<String> {
    let guard = overlay().read().expect("manifest overlay poisoned");
    guard.keys().cloned().collect()
}

/// Top-level manifest, deserialized from `tools.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolManifest {
    pub manifest_version: String,
    /// cluster id → purpose
    pub cluster: HashMap<String, String>,
    pub scene: HashMap<String, Vec<String>>,
    pub tool: Vec<ToolEntry>,
}

/// Per-tool entry in the manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolEntry {
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
    pub cluster: Option<String>,
    pub differentiator: Option<String>,
    pub tokens: u32,
    pub latency: String,
    pub risk: String,
    pub confirm: bool,
    pub idempotent: bool,
    pub domains: Option<Vec<usize>>,
    pub actions: Option<Vec<String>>,
    pub scenes: Option<Vec<String>>,
}

/// Indexed view: fast lookup by tool name, built once from ToolManifest.
#[derive(Debug, Clone)]
pub struct ToolManifestIndex {
    pub version: String,
    /// tool name → ToolEntry
    pub by_name: HashMap<String, ToolEntry>,
    /// scene name → prefix list (derived from manifest.scene)
    pub scene_prefixes: HashMap<String, Vec<String>>,
    /// cluster id → purpose
    pub cluster_purposes: HashMap<String, String>,
}

impl ToolManifest {
    /// Load and parse the manifest from the embedded TOML file.
    pub fn load() -> Self {
        let source = include_str!("../../tools.toml");
        toml::from_str(source).expect("tools.toml: invalid format")
    }

    /// Build an indexed view for O(1) lookups.
    pub fn index(self) -> ToolManifestIndex {
        let total = self.tool.len();
        let by_name: HashMap<String, ToolEntry> =
            self.tool.into_iter().map(|t| (t.name.clone(), t)).collect();
        assert_eq!(
            by_name.len(), total,
            "tools.toml: duplicate tool name (entry silently overwrote another); \
             every [[tool]] must have a unique `name`"
        );
        ToolManifestIndex {
            version: self.manifest_version,
            by_name,
            scene_prefixes: self.scene,
            cluster_purposes: self.cluster,
        }
    }
}

impl ToolManifestIndex {
    /// Look up a tool entry by name. **Overlay-first** — runtime external tools
    /// take precedence over the static `tools.toml` base.
    ///
    /// ## 双层查找语义
    /// 1. 先查 `OVERLAY` (外部工具 ingest 写入)
    /// 2. Fallback 到 `by_name` (tools.toml 静态)
    /// 3. 都 miss → `None`
    ///
    /// ## 不阻塞
    /// `std::sync::RwLock::read` 同步微秒级, 不跨 await, 主循环安全。
    pub fn get(&self, name: &str) -> Option<ToolEntry> {
        if let Some(guard) = overlay().read().ok() {
            if let Some(e) = guard.get(name) {
                return Some(e.clone());
            }
        }
        self.by_name.get(name).cloned()
    }

    /// Get cluster ID for a tool. **Overlay-first** (与 `get` 对齐).
    pub fn cluster(&self, name: &str) -> Option<String> {
        self.get(name)?.cluster
    }

    /// Get cost info for a tool. **Overlay-first**.
    pub fn cost(&self, name: &str) -> Option<(u32, String, String)> {
        let e = self.get(name)?;
        Some((e.tokens, e.latency.clone(), e.risk.clone()))
    }

    /// Get short_description for a tool. **Overlay-first**.
    pub fn short_description(&self, name: &str) -> Option<String> {
        self.get(name)?.short_description
    }

    /// Get scene prefix list for a task kind (falling back to `_default`).
    pub fn scene_prefixes_for(&self, task_kind: &str) -> &[String] {
        self.scene_prefixes
            .get(task_kind)
            .or_else(|| self.scene_prefixes.get("_default"))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Get domain IDs for SilentRouter mapping. **Overlay-first**.
    pub fn domains(&self, name: &str) -> Vec<usize> {
        self.get(name)
            .and_then(|t| t.domains)
            .unwrap_or_default()
    }

    /// Get action tags for SilentRouter mapping. **Overlay-first**.
    pub fn actions(&self, name: &str) -> Vec<String> {
        self.get(name)
            .and_then(|t| t.actions)
            .unwrap_or_default()
    }

    /// Get purpose string for a cluster.
    pub fn cluster_purpose(&self, cluster_id: &str) -> Option<&str> {
        self.cluster_purposes.get(cluster_id).map(|s| s.as_str())
    }

    /// Grouped cluster members: cluster_id → Vec<(tool_name, differentiator, purpose)>.
    ///
    /// **双层遍历**：base + overlay 合并. Overlay 同名 entry 覆盖 base.
    /// 返回 owned `String` 因为 overlay guard 不能跨函数边界持有。
    pub fn cluster_members_grouped(&self) -> Vec<(String, String, Vec<(String, String)>)> {
        let mut groups: HashMap<String, Vec<(String, String)>> = HashMap::new();
        let mut seen_order: Vec<String> = Vec::new();
        // base 层
        for (name, entry) in &self.by_name {
            if let Some(cid) = entry.cluster.as_deref() {
                if !groups.contains_key(cid) {
                    seen_order.push(cid.to_string());
                }
                groups.entry(cid.to_string()).or_default()
                    .push((name.clone(), entry.differentiator.clone().unwrap_or_default()));
            }
        }
        // overlay 层 (覆盖同名 base)
        if let Ok(guard) = overlay().read() {
            for (name, entry) in guard.iter() {
                if let Some(cid) = entry.cluster.as_deref() {
                    if !groups.contains_key(cid) {
                        seen_order.push(cid.to_string());
                    }
                    // 移除 base 同名 entry, 加 overlay
                    if let Some(v) = groups.get_mut(cid) {
                        v.retain(|(n, _)| n != name);
                    }
                    groups.entry(cid.to_string()).or_default()
                        .push((name.clone(), entry.differentiator.clone().unwrap_or_default()));
                }
            }
        }
        seen_order.into_iter().filter_map(|cid| {
            let purpose = self.cluster_purposes.get(cid.as_str()).map(|s| s.to_string()).unwrap_or_default();
            let members = groups.remove(&cid)?;
            if members.len() < 2 { return None; } // singletons don't need a cluster
            Some((cid, purpose, members))
        }).collect()
    }

    /// Collect all (tool_name, cluster, differentiator) triples for ClusterRegistry.
    /// **双层遍历**: base + overlay. Overlay 同名 entry 覆盖 base.
    /// 返回 owned `String` 因为 overlay guard 不能跨函数边界持有。
    pub fn all_cluster_members(&self) -> Vec<(String, String, String)> {
        let mut out: Vec<(String, String, String)> = self
            .by_name
            .iter()
            .filter_map(|(name, entry)| {
                let cluster = entry.cluster.as_deref()?;
                let diff = entry.differentiator.as_deref()?;
                Some((name.clone(), cluster.to_string(), diff.to_string()))
            })
            .collect();
        if let Ok(guard) = overlay().read() {
            // 移除 base 中被 overlay 覆盖的 name
            let overlay_names: std::collections::HashSet<String> = guard.keys().cloned().collect();
            out.retain(|(n, _, _)| !overlay_names.contains(n));
            // 追加 overlay 条目
            for (name, entry) in guard.iter() {
                if let (Some(cid), Some(diff)) = (entry.cluster.as_deref(), entry.differentiator.as_deref()) {
                    out.push((name.clone(), cid.to_string(), diff.to_string()));
                }
            }
        }
        out
    }

    /// Collect all tool entries for generating schemas. **双层遍历**.
    pub fn all_entries(&self) -> Vec<ToolEntry> {
        let mut out: Vec<ToolEntry> = self.by_name.values().cloned().collect();
        if let Ok(guard) = overlay().read() {
            // 移除 base 中被 overlay 覆盖的 name
            out.retain(|e| !guard.contains_key(&e.name));
            // 追加 overlay 条目
            out.extend(guard.values().cloned());
        }
        out
    }
}

impl ToolEntry {
    /// Build a ToolSchema from manifest data. Caller sets parameters/returns/examples.
    pub fn to_schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            short_description: self.short_description.clone(),
            cost: Some(ToolCost {
                tokens: self.tokens,
                latency: self.latency.clone(),
                risk: self.risk.clone(),
            }),
            security: Some(ToolSecurity {
                allowed_paths: None,
                max_size_mb: None,
                confirm_required: self.confirm,
                needs_sandbox: false,
            }),
            idempotent: self.idempotent,
            schema_stable: true,
            parameters: serde_json::Value::Null,
            returns: None,
            examples: Vec::new(),
            applicable_task_kinds: self.scenes.clone(),
        }
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 双层 manifest 测试 (V42-B F-1 修复)
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 全局测试锁: `static OVERLAY` 跨测试共享, 必须串行化避免污染
    /// `cargo test` 默认多线程并发, 没有这个锁测试会相互干扰
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn make_entry(name: &str, cluster: Option<&str>, desc: &str) -> ToolEntry {
        ToolEntry {
            name: name.to_string(),
            description: desc.to_string(),
            short_description: Some(format!("{name} short")),
            cluster: cluster.map(String::from),
            differentiator: Some("test diff".to_string()),
            tokens: 64,
            latency: "10ms".to_string(),
            risk: "low".to_string(),
            confirm: false,
            idempotent: true,
            domains: Some(vec![0]),
            actions: Some(vec!["Read".to_string()]),
            scenes: Some(vec!["plan".to_string()]),
        }
    }

    /// 测试辅助: 清空 overlay (不依赖任何 base 内容)
    fn clear_overlay() {
        let names: Vec<String> = list_external();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        unmerge_external(&refs);
    }

    /// 串行化 overlay 测试 + 清理 — 必须在每个测试函数开头调
    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_overlay();
        g
    }

    #[test]
    fn overlay_merge_then_get() {
        let _g = setup();
        // 合并
        merge_external(vec![make_entry("external.test_a", Some("web_research"), "external A")]);
        // 查询
        let entry = index().get("external.test_a");
        assert!(entry.is_some(), "overlay entry should be findable");
        let entry = entry.unwrap();
        assert_eq!(entry.description, "external A");
        assert_eq!(entry.cluster, Some("web_research".to_string()));
    }

    #[test]
    fn overlay_overrides_base() {
        let _g = setup();
        // 假定 base 中已有 fs_read (从 tools.toml 加载)
        // 合并一个同名 entry, 应当覆盖
        merge_external(vec![make_entry("fs_read", Some("fs_read_discover"), "OVERRIDDEN")]);
        let entry = index().get("fs_read").unwrap();
        assert_eq!(entry.description, "OVERRIDDEN", "overlay must win over base");
    }

    #[test]
    fn unmerge_removes_from_overlay() {
        let _g = setup();
        merge_external(vec![make_entry("external.test_b", None, "B")]);
        assert!(index().get("external.test_b").is_some());
        unmerge_external(&["external.test_b"]);
        assert!(index().get("external.test_b").is_none());
    }

    #[test]
    fn unmerge_unknown_is_noop() {
        let _g = setup();
        unmerge_external(&["does.not.exist"]); // 不 panic 即可
    }

    #[test]
    fn all_entries_includes_overlay() {
        let _g = setup();
        let before: Vec<String> = index().all_entries().iter().map(|e| e.name.clone()).collect();
        assert!(!before.is_empty(), "base should have entries from tools.toml");
        merge_external(vec![make_entry("external.test_c", None, "C")]);
        let after: Vec<String> = index().all_entries().iter().map(|e| e.name.clone()).collect();
        assert!(after.contains(&"external.test_c".to_string()));
        // base entry 数量不变
        assert!(after.iter().any(|n| n == "fs_read"), "base entries preserved");
    }

    #[test]
    fn cluster_lookup_via_overlay() {
        let _g = setup();
        merge_external(vec![make_entry("external.test_d", Some("web_research"), "D")]);
        let cluster = index().cluster("external.test_d");
        assert_eq!(cluster, Some("web_research".to_string()));
    }

    #[test]
    fn cost_lookup_via_overlay() {
        let _g = setup();
        merge_external(vec![make_entry("external.test_e", None, "E")]);
        let cost = index().cost("external.test_e");
        assert_eq!(cost, Some((64, "10ms".to_string(), "low".to_string())));
    }

    #[test]
    fn domains_actions_via_overlay() {
        let _g = setup();
        merge_external(vec![make_entry("external.test_f", None, "F")]);
        let domains = index().domains("external.test_f");
        assert_eq!(domains, vec![0]);
        let actions = index().actions("external.test_f");
        assert_eq!(actions, vec!["Read".to_string()]);
    }

    #[test]
    fn list_external_returns_overlay_names() {
        let _g = setup();
        merge_external(vec![
            make_entry("external.test_g1", None, "G1"),
            make_entry("external.test_g2", None, "G2"),
        ]);
        let names = list_external();
        assert!(names.contains(&"external.test_g1".to_string()));
        assert!(names.contains(&"external.test_g2".to_string()));
    }

    #[test]
    fn cluster_members_grouped_includes_overlay() {
        let _g = setup();
        merge_external(vec![
            make_entry("external.test_h1", Some("web_research"), "H1"),
            make_entry("external.test_h2", Some("web_research"), "H2"),
        ]);
        let groups = index().cluster_members_grouped();
        let web_group = groups.iter().find(|(cid, _, _)| cid == "web_research");
        assert!(web_group.is_some(), "web_research cluster should exist");
        let (_, _, members) = web_group.unwrap();
        let member_names: Vec<&str> = members.iter().map(|(n, _)| n.as_str()).collect();
        assert!(member_names.contains(&"external.test_h1"));
        assert!(member_names.contains(&"external.test_h2"));
    }

    #[test]
    fn empty_merge_is_noop() {
        let _g = setup();
        let before = list_external().len();
        merge_external(vec![]);
        let after = list_external().len();
        assert_eq!(before, after, "empty merge should not change overlay");
    }

    #[test]
    fn merge_then_unmerge_keeps_other_entries() {
        let _g = setup();
        merge_external(vec![
            make_entry("external.test_i1", None, "I1"),
            make_entry("external.test_i2", None, "I2"),
        ]);
        unmerge_external(&["external.test_i1"]);
        assert!(index().get("external.test_i1").is_none());
        assert!(index().get("external.test_i2").is_some(), "i2 should survive");
    }
}
