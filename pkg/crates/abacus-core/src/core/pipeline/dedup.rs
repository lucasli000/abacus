//! W2 (Task #100): Tool result dedup over warm tier
//!
//! 在 `idempotent=true` 工具的调度路径前后插入查询/写入，短 TTL 内同一
//! `(tool_id, canonical_args_hash)` 直接复用上次结果，避免重复 IO/LLM token。
//!
//! ## 设计要点
//! - **canonical hashing**：`canonical_json` 递归排序 object key，确保
//!   `{a:1,b:2}` 与 `{b:2,a:1}` 哈希一致；不依赖 LLM 给出的字段顺序。
//! - **仅 idempotent**：非幂等工具（写操作、外部 API 等）一律绕过，缓存正确性靠 schema 声明保证。
//! - **以 ToolOutput.success 为锚**：失败结果不缓存（失败原因可能是临时——cooldown/auth），
//!   下一次重新调度可恢复。
//!
//! ## 引用关系
//! - 上游：`pipeline::execute_loop` dispatch 前调 `lookup`、dispatch 后调 `record`
//! - 下游：`cache::warm::WarmTier`（基础设施）
//! - 启用：`CoreConfig.tool_result_dedup_enabled`（默认 false）
//! - 审计：`CoreLoop::audit_report` / `session_cache_report` 通过 `stats()` 读取命中率

use std::sync::Arc;

use abacus_types::{ToolId, ToolOutput};
use serde_json::Value;

use crate::cache::{WarmCacheable, WarmStats, WarmTier};

/// 唯一键：工具 id + 规范化后的参数哈希。
///
/// `args_hash` 用 64-bit FxHash（std DefaultHasher）足够避免会话内碰撞。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DedupKey {
    pub tool_id: ToolId,
    pub args_hash: u64,
}

impl DedupKey {
    pub fn new(tool_id: &ToolId, args: &Value) -> Self {
        Self {
            tool_id: tool_id.clone(),
            args_hash: canonical_hash(args),
        }
    }
}

/// Warm 层载荷：原 ToolOutput 复用 + 序列化字节缓存（用于 size_hint 加权）。
pub struct CachedToolResult {
    pub output: ToolOutput,
    /// 提前算好的 serialized size，避免 size_hint 每次重新 serialize
    weight: usize,
}

impl CachedToolResult {
    pub fn new(output: ToolOutput) -> Self {
        let weight = serde_json::to_string(&output.output)
            .map(|s| s.len())
            .unwrap_or(0)
            .max(1);
        Self { output, weight }
    }
}

impl WarmCacheable for CachedToolResult {
    fn size_hint(&self) -> usize {
        self.weight
    }
    // on_promote / on_demote 默认空实现：纯数据缓存无副作用。
}

/// 持有 warm tier + 启用开关的薄壳。
pub struct ToolResultDedup {
    inner: Arc<WarmTier<DedupKey, CachedToolResult>>,
}

impl ToolResultDedup {
    pub fn new(capacity_bytes: usize, ttl_secs: u64) -> Self {
        Self {
            inner: Arc::new(WarmTier::new(
                capacity_bytes,
                std::time::Duration::from_secs(ttl_secs),
            )),
        }
    }

    /// dispatch 前查询；命中则直接返回 cached output（已 clone）。
    pub fn lookup(&self, tool_id: &ToolId, args: &Value) -> Option<ToolOutput> {
        let key = DedupKey::new(tool_id, args);
        self.inner.get(&key).map(|arc| arc.output.clone())
    }

    /// dispatch 后写入。仅在 idempotent && success 路径调用。
    pub fn record(&self, tool_id: &ToolId, args: &Value, output: &ToolOutput) {
        let key = DedupKey::new(tool_id, args);
        self.record_with_key(key, output);
    }

    /// 已知 key 的写入路径——pipeline 复用 lookup 阶段算好的 key，避免 params 被 move 后重算。
    pub fn record_with_key(&self, key: DedupKey, output: &ToolOutput) {
        self.inner
            .promote(key, Arc::new(CachedToolResult::new(output.clone())));
    }

    pub fn stats(&self) -> WarmStats {
        self.inner.stats()
    }
}

// ─── canonical JSON hashing ───────────────────────────────────────────────

/// 把 Value 递归规范化（object key 排序）后哈希。
///
/// 直接哈希 Value 不行：HashMap 字段顺序不确定 → 不稳定。
/// 先 serialize 到 sorted form 字符串再哈希，是最简单且可验证的方式。
pub fn canonical_hash(v: &Value) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let canon = canonicalize(v);
    let s = canon.to_string();
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn canonicalize(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::with_capacity(map.len());
            for k in keys {
                out.insert(k.clone(), canonicalize(&map[k]));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mk_output(tool: &str, body: &str) -> ToolOutput {
        ToolOutput {
            tool_id: ToolId(tool.into()),
            success: true,
            output: json!({ "data": body }),
            latency_ms: 10,
            failure_kind: None,
            try_instead: vec![],
        }
    }

    #[test]
    fn canonical_hash_ignores_field_order() {
        let a = json!({ "x": 1, "y": 2 });
        let b = json!({ "y": 2, "x": 1 });
        assert_eq!(canonical_hash(&a), canonical_hash(&b));
    }

    #[test]
    fn canonical_hash_distinguishes_arrays() {
        let a = json!([1, 2, 3]);
        let b = json!([3, 2, 1]);
        assert_ne!(canonical_hash(&a), canonical_hash(&b));
    }

    #[test]
    fn canonical_hash_recurses_into_nested_objects() {
        let a = json!({ "outer": { "a": 1, "b": 2 } });
        let b = json!({ "outer": { "b": 2, "a": 1 } });
        assert_eq!(canonical_hash(&a), canonical_hash(&b));
    }

    #[test]
    fn dedup_lookup_then_record_roundtrip() {
        let dedup = ToolResultDedup::new(64 * 1024, 60);
        let tool = ToolId("fs_read".into());
        let args = json!({ "path": "/tmp/x" });

        // 初始 miss
        assert!(dedup.lookup(&tool, &args).is_none());

        // 写入后命中
        let out = mk_output("fs_read", "hello");
        dedup.record(&tool, &args, &out);
        let hit = dedup.lookup(&tool, &args).expect("should hit");
        assert_eq!(hit.output, out.output);

        let s = dedup.stats();
        assert_eq!(s.hits, 1);
        assert_eq!(s.misses, 1);
        assert_eq!(s.entries, 1);
    }

    #[test]
    fn dedup_distinguishes_args() {
        let dedup = ToolResultDedup::new(64 * 1024, 60);
        let tool = ToolId("db_read_records".into());
        let out_a = mk_output("db_read_records", "row-a");
        dedup.record(&tool, &json!({ "id": "a" }), &out_a);
        // 不同 args 不该命中
        assert!(dedup.lookup(&tool, &json!({ "id": "b" })).is_none());
        // 相同 args 命中
        assert!(dedup.lookup(&tool, &json!({ "id": "a" })).is_some());
    }

    #[test]
    fn dedup_field_order_independent_hit() {
        let dedup = ToolResultDedup::new(64 * 1024, 60);
        let tool = ToolId("fs_read".into());
        let out = mk_output("fs_read", "x");
        dedup.record(&tool, &json!({ "path": "/p", "limit": 100 }), &out);
        // LLM 第二次给的字段顺序变了，仍应命中
        assert!(dedup.lookup(&tool, &json!({ "limit": 100, "path": "/p" })).is_some());
    }

    #[test]
    fn ttl_expires_entries() {
        let dedup = ToolResultDedup::new(64 * 1024, 0); // ttl=0 → 立即过期
        let tool = ToolId("t".into());
        let out = mk_output("t", "x");
        dedup.record(&tool, &json!({}), &out);
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(dedup.lookup(&tool, &json!({})).is_none());
    }

    // ─── Phase Stability-1：proptest property tests ──────────────────────

    use proptest::prelude::*;

    /// 生成任意 JSON 字典（key/value 都是字母数字，递归深度 1）—— 用于排序无关性测试
    fn arb_flat_json() -> impl Strategy<Value = Value> {
        proptest::collection::vec(
            (
                "[a-z]{1,5}",                      // key
                proptest::sample::select(vec![     // value 取自有限集
                    json!(null),
                    json!(true),
                    json!(false),
                    json!(0),
                    json!(42),
                    json!("hello"),
                    json!([1, 2, 3]),
                ]),
            ),
            0..6,
        )
        .prop_map(|kvs| {
            let mut map = serde_json::Map::new();
            for (k, v) in kvs {
                map.insert(k, v);
            }
            Value::Object(map)
        })
    }

    proptest! {
        /// **属性 1**：canonical_hash 对 object 字段顺序无关——任意打乱字段顺序后哈希相同
        #[test]
        fn canonical_hash_field_order_invariant(j in arb_flat_json()) {
            // 用 BTreeMap 强制字典序，再随机洗牌——洗牌前后哈希必须相等
            let h_original = canonical_hash(&j);

            // 把 object 拆解为 Vec<(K, V)> 反序后重新组装（顺序变了但内容不变）
            if let Value::Object(map) = &j {
                let mut entries: Vec<(String, Value)> = map.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                entries.reverse();
                let mut shuffled = serde_json::Map::new();
                for (k, v) in entries {
                    shuffled.insert(k, v);
                }
                let h_shuffled = canonical_hash(&Value::Object(shuffled));
                prop_assert_eq!(h_original, h_shuffled);
            }
        }

        /// **属性 2**：DedupKey 等价性——`new(t, v1) == new(t, v2)` ⟺ `canonical_hash(v1) == canonical_hash(v2)`
        #[test]
        fn dedup_key_equality_iff_canonical_hash_equal(
            j1 in arb_flat_json(),
            j2 in arb_flat_json(),
        ) {
            let tool = ToolId("t".into());
            let k1 = DedupKey::new(&tool, &j1);
            let k2 = DedupKey::new(&tool, &j2);
            let h_eq = canonical_hash(&j1) == canonical_hash(&j2);
            prop_assert_eq!(k1 == k2, h_eq);
        }

        /// **属性 3**：相同 args 反复 record/lookup 不破坏命中（幂等写入）
        #[test]
        fn dedup_record_is_idempotent(j in arb_flat_json()) {
            let dedup = ToolResultDedup::new(64 * 1024, 60);
            let tool = ToolId("t".into());
            let out = mk_output("t", "x");
            dedup.record(&tool, &j, &out);
            dedup.record(&tool, &j, &out);
            dedup.record(&tool, &j, &out);
            prop_assert!(dedup.lookup(&tool, &j).is_some());
        }
    }
}
