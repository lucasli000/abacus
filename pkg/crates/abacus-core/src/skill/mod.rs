use std::collections::HashMap;

use abacus_types::{SkillDef, SkillExecutionRecord, SkillExperience, SkillId, Sm2State, ToolHandle, ToolId, ToolProvider, ToolSchema};
use regex::Regex;

use crate::tool::ToolRegistry;

#[derive(Debug, Clone)]
pub struct SkillCandidate {
    pub id: SkillId,
    pub confidence: f64,
}

/// Pluggable embedding provider for semantic skill matching.
/// When `None`, falls back to character n-gram Jaccard similarity (zero external deps).
pub trait EmbeddingProvider: Send + Sync {
    /// Compute a similarity score [0, 1] between query text and a skill's semantic corpus.
    fn similarity(&self, query: &str, skill_corpus: &str) -> f64;
}

/// Default fallback: multi-granularity n-gram Jaccard similarity.
/// Combines unigrams (chars) and bigrams for both Chinese and English.
/// Pure char-based, no tokenizer dependency.
pub struct NgramMatcher;

impl NgramMatcher {
    /// Generate all substrings of length 1..=n from a text's character array.
    fn multi_grams(text: &str) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        let mut grams = Vec::new();
        // Unigrams (individual chars)
        for &c in &chars {
            grams.push(c.to_string());
        }
        // Bigrams
        if chars.len() >= 2 {
            for w in chars.windows(2) {
                grams.push(w.iter().collect());
            }
        }
        // Trigrams (weighted lower but still useful for English)
        if chars.len() >= 3 {
            for w in chars.windows(3) {
                grams.push(w.iter().collect());
            }
        }
        grams
    }
}

impl Default for NgramMatcher {
    fn default() -> Self { Self }
}

impl EmbeddingProvider for NgramMatcher {
    fn similarity(&self, query: &str, skill_corpus: &str) -> f64 {
        let q_grams: std::collections::HashSet<String> =
            Self::multi_grams(query).into_iter().collect();
        let s_grams: std::collections::HashSet<String> =
            Self::multi_grams(skill_corpus).into_iter().collect();

        if q_grams.is_empty() && s_grams.is_empty() {
            return 0.0;
        }

        let intersection = q_grams.intersection(&s_grams).count();
        let union = q_grams.union(&s_grams).count();
        if union == 0 { 0.0 } else { intersection as f64 / union as f64 }
    }
}

pub struct TriggerMatcher {
    keywords: Vec<(String, SkillId)>,
    regex: Vec<(Regex, SkillId)>,
    domain_index: HashMap<String, Vec<SkillId>>,
    /// Semantic corpus per skill (combined from triggers.prompt + keywords)
    semantic_corpus: HashMap<SkillId, String>,
    /// Optional external embedding provider; falls back to NgramMatcher
    embedder: Box<dyn EmbeddingProvider>,
    /// Weight for semantic score vs keyword/regex scores
    semantic_weight: f64,
}

impl Default for TriggerMatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl TriggerMatcher {
    pub fn new() -> Self {
        Self {
            keywords: Vec::new(), regex: Vec::new(), domain_index: HashMap::new(),
            semantic_corpus: HashMap::new(),
            embedder: Box::new(NgramMatcher),
            semantic_weight: 0.3,
        }
    }

    /// Replace the default n-gram embedder with a custom one.
    pub fn set_embedder(&mut self, embedder: Box<dyn EmbeddingProvider>) {
        self.embedder = embedder;
    }

    /// Set the weight of semantic scores (0.0 = pure keyword/regex, 1.0 = pure semantic).
    pub fn set_semantic_weight(&mut self, weight: f64) {
        self.semantic_weight = weight.clamp(0.0, 1.0);
    }

    pub fn register(&mut self, id: &SkillId, triggers: &abacus_types::SkillTriggers, prompt: &str) {
        for kw in &triggers.keywords {
            self.keywords.push((kw.clone(), id.clone()));
        }
        for pattern in &triggers.regex {
            if let Ok(re) = Regex::new(pattern) {
                self.regex.push((re, id.clone()));
            }
        }
        for domain in &triggers.domain {
            self.domain_index.entry(domain.clone()).or_default().push(id.clone());
        }
        // Build semantic corpus from prompt + keywords
        let corpus = if prompt.is_empty() {
            triggers.keywords.join(" ")
        } else {
            format!("{} {}", prompt, triggers.keywords.join(" "))
        };
        self.semantic_corpus.insert(id.clone(), corpus);
    }

    pub fn evaluate(&self, input: &str, task_kind: Option<&str>) -> Vec<SkillCandidate> {
        let mut scores: HashMap<&SkillId, f64> = HashMap::new();
        let lower = input.to_lowercase();

        // 1. Keyword match (exact substring, case-insensitive)
        for (kw, id) in &self.keywords {
            if lower.contains(&kw.to_lowercase()) {
                *scores.entry(id).or_insert(0.0) += 0.6;
            }
        }
        // 2. Regex match
        for (re, id) in &self.regex {
            if re.is_match(input) {
                *scores.entry(id).or_insert(0.0) += 0.8;
            }
        }
        // 3. Domain match
        if let Some(kind) = task_kind {
            let kl = kind.to_lowercase();
            for (domain, ids) in &self.domain_index {
                if kl.contains(&domain.to_lowercase()) || domain.to_lowercase().contains(&kl) {
                    for id in ids {
                        *scores.entry(id).or_insert(0.0) += 0.5;
                    }
                }
            }
        }
        // 4. Semantic match (n-gram Jaccard)
        let semantic_weight = self.semantic_weight;
        let fallback_weight = 1.0 - semantic_weight;
        for (id, corpus) in &self.semantic_corpus {
            let semantic_score = self.embedder.similarity(input, corpus);
            if semantic_score > 0.15 {
                let entry = scores.entry(id).or_insert(0.0);
                *entry = *entry * fallback_weight + semantic_score * semantic_weight;
            }
        }

        let mut candidates: Vec<SkillCandidate> = scores
            .into_iter()
            .map(|(id, score)| SkillCandidate { id: id.clone(), confidence: score.min(1.0) })
            .collect();
        candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
        candidates
    }
}

pub struct SkillEngine {
    skills: HashMap<SkillId, SkillDef>,
    /// Track loaded state separately — fine-grained lock allows evaluate() during load().
    loaded: std::sync::RwLock<HashMap<SkillId, bool>>,
    matcher: TriggerMatcher,
    experiences: std::sync::RwLock<HashMap<SkillId, SkillExperience>>,
    max_candidates: usize,
    /// 反查表：sanitized ToolId.0 → (SkillId, step_id raw)。
    /// load() 时填充；execute 时 O(1) 查询，避免 sanitize 后无法 split 的问题。
    name_map: std::sync::RwLock<HashMap<String, (SkillId, String)>>,
    /// BehaviorPalace 弱引用，用于 evaluate_with_palace 时叠加历史置信度
    ///
    /// 引用: DualPalaceMemory（CoreLoop 持有 strong Arc，SkillEngine 持有 Weak 避免循环）
    /// 生命周期: CoreLoop 构造 SkillEngine 时注入；升级失败时退化为普通 evaluate()
    palace: std::sync::Weak<crate::memory_palace::DualPalaceMemory>,
}

impl Default for SkillEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillEngine {
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
            loaded: std::sync::RwLock::new(HashMap::new()),
            matcher: TriggerMatcher::new(),
            experiences: std::sync::RwLock::new(HashMap::new()),
            max_candidates: 3,
            name_map: std::sync::RwLock::new(HashMap::new()),
            palace: std::sync::Weak::new(),
        }
    }

    /// 注入 BehaviorPalace 弱引用（CoreLoop wire-up 阶段调用）
    ///
    /// 引用: DualPalaceMemory（Arc 弱化避免 CoreLoop ↔ SkillEngine 循环）
    /// 生命周期: 注入后随 SkillEngine 存活；升级失败时 evaluate_with_palace 退化为 evaluate()
    pub fn set_palace(&mut self, palace: std::sync::Weak<crate::memory_palace::DualPalaceMemory>) {
        self.palace = palace;
    }

    /// 反查 sanitized ToolId → (SkillId, step_id)
    pub fn lookup_skill_step(&self, sanitized_id: &str) -> Option<(SkillId, String)> {
        self.name_map.read().unwrap().get(sanitized_id).cloned()
    }

    /// Replace the default n-gram embedder with a custom one for semantic matching.
    pub fn set_embedder(&mut self, embedder: Box<dyn EmbeddingProvider>) {
        self.matcher.set_embedder(embedder);
    }

    /// Set the semantic vs keyword weight (0.0 = pure keyword, 1.0 = pure semantic).
    pub fn set_semantic_weight(&mut self, weight: f64) {
        self.matcher.set_semantic_weight(weight);
    }

    pub fn register_skill(&mut self, def: SkillDef) {
        let id = def.id.clone();
        self.matcher.register(&id, &def.triggers, &def.prompt);
        self.skills.insert(id.clone(), def);
        self.loaded.write().unwrap().insert(id, false);
    }

    pub fn evaluate(&self, input: &str, task_kind: Option<&str>) -> Vec<SkillCandidate> {
        let mut candidates = self.matcher.evaluate(input, task_kind);
        let experiences = self.experiences.read().unwrap();
        for c in &mut candidates {
            if let Some(exp) = experiences.get(&c.id) {
                c.confidence *= experience_multiplier(exp);
            }
        }
        candidates.sort_by(|a, b| {
            b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.into_iter().take(self.max_candidates).collect()
    }

    /// evaluate() 的 async 增强版：叠加行为宫殿历史置信度
    ///
    /// final_score = base_score + 0.2 * palace_confidence（上限 1.0）
    /// 若 palace 弱引用升级失败（palace 已 drop），退化为普通 evaluate()。
    ///
    /// ## 引用关系
    /// - 调用: BehaviorPalace.search (memory_palace.rs)
    /// - 被调用: SkillEngine 持有者（CoreLoop / orchestration 层）在工具选择前
    ///
    /// ## 生命周期
    /// - 每次请求调用一次，异步读取 palace 不阻塞主流程
    /// - palace 升级失败时静默退化，不返回错误
    pub async fn evaluate_with_palace(&self, input: &str, task_kind: Option<&str>) -> Vec<SkillCandidate> {
        let mut candidates = self.evaluate(input, task_kind);

        if let Some(palace) = self.palace.upgrade() {
            // 查询行为宫殿中 skill:xxx 的历史置信度
            let skill_memories = palace.behavior.search(&[
                "skill".to_string(),
                "success".to_string(),
            ]).await;
            for memory in &skill_memories {
                // memory.pattern 形如 "skill:search_file"
                if let Some(skill_id_str) = memory.pattern.strip_prefix("skill:") {
                    let skill_id = SkillId(skill_id_str.to_string());
                    if let Some(candidate) = candidates.iter_mut().find(|c| c.id == skill_id) {
                        // 叠加置信度（max 1.0）
                        candidate.confidence = (candidate.confidence + 0.2 * memory.confidence).min(1.0);
                    }
                }
            }
            // 重排
            candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
        }

        candidates
    }

    /// 加载 skill：把 workflow 每个 step 注册为虚拟 ToolHandle 到 registry。
    ///
    /// ## Phase 2 变更
    /// 现在还需要 `executor: Arc<dyn ToolExecutor>` 参数（一般是单例 SkillExecutor）。
    /// 双轨注册：每个 step 同时注册 schema (register) 和 executor (register_executor)。
    /// 之前只注册 schema 导致 LLM 调用时 `no executor for tool` 兜底——现已修复。
    pub async fn load(
        &mut self,
        id: &SkillId,
        registry: &ToolRegistry,
        executor: std::sync::Arc<dyn crate::tool::ToolExecutor>,
    ) -> Result<(), String> {
        if self.loaded.read().unwrap().get(id) == Some(&true) {
            return Ok(());
        }
        let def = self.skills.get(id).ok_or_else(|| format!("skill not found: {id}"))?;

        for step in &def.workflow {
            // 单一命名：ToolId.0 == schema.name == sanitized 形态。
            // skill_id / step.tool / step.id 均可能含 . _，sanitize 一次保证 LLM 协议合规。
            let sanitized = format!(
                "skill_{}_{}_step_{}",
                crate::llm::tool_view::sanitize_name(&id.0),
                crate::llm::tool_view::sanitize_name(&step.tool),
                crate::llm::tool_view::sanitize_name(&step.id),
            );
            // 同步登记反查（execute 时 O(1) 还原 raw skill_id + step_id）
            self.name_map.write().unwrap().insert(
                sanitized.clone(),
                (id.clone(), step.id.clone()),
            );
            let tool_id = ToolId(sanitized);
            let handle = ToolHandle {
                id: tool_id.clone(),
                schema: ToolSchema {
                    name: tool_id.0.clone(),
                    description: step.description.clone(),
                    parameters: step.params.clone(),
                    returns: None,
                    security: None,
                    cost: None,
                    examples: Vec::new(),
                    applicable_task_kinds: None,
                    idempotent: false,
                },
                provider: ToolProvider::Skill { skill_id: id.0.clone() },
                state: abacus_types::ToolState::Loaded,
                effectiveness: abacus_types::ToolEffectiveness::default(),
            };
            registry.register(handle).await;
            registry.register_executor(tool_id, executor.clone()).await;
        }

        self.loaded.write().unwrap().insert(id.clone(), true);
        Ok(())
    }

    pub fn record_execution(&mut self, record: SkillExecutionRecord) {
        let mut exp_map = self.experiences.write().unwrap();
        let key = record.skill_id.clone();
        let exp = exp_map.entry(key).or_insert_with(|| SkillExperience {
            skill_id: record.skill_id.clone(),
            invoke_count: 0,
            success_rate: 0.0,
            avg_latency_ms: 0.0,
            last_invoked: None,
            best_scenario: None,
            sm2: Sm2State::default(),
            trend: "stable".into(),
        });

        exp.invoke_count += 1;
        let success = record.exit_code == 0;
        exp.success_rate = ((exp.success_rate * (exp.invoke_count as f64 - 1.0))
            + if success { 1.0 } else { 0.0 })
            / exp.invoke_count as f64;

        let alpha = 0.3;
        exp.avg_latency_ms =
            alpha * record.total_latency_ms as f64 + (1.0 - alpha) * exp.avg_latency_ms;
        exp.last_invoked = Some(record.timestamp);

        // SM-2 update
        let q = if success { 3 } else { 0 };
        let sm2 = &mut exp.sm2;
        if q >= 3 {
            if sm2.repetition == 0 {
                sm2.interval_days = 1.0;
            } else if sm2.repetition == 1 {
                sm2.interval_days = 6.0;
            } else {
                sm2.interval_days *= sm2.easiness;
            }
            sm2.repetition += 1;
        } else {
            sm2.repetition = 0;
            sm2.interval_days = 1.0;
        }
        sm2.easiness = (sm2.easiness + 0.1 - (5.0 - q as f64) * (0.08 + (5.0 - q as f64) * 0.02))
            .max(1.3);
    }

    /// List all registered skill definitions.
    pub fn list_skills(&self) -> Vec<SkillDef> {
        self.skills.values().cloned().collect()
    }
}

// ─── SkillExecutor (Phase 2: workflow 真执行) ───────────────────────────
//
// 把 skill workflow 的 step 调用转发到底层真实工具。
// LLM 调 `skill/{id}/{step.tool}/step/{step_id}` → SkillExecutor.execute()
//   → 反查 SkillDef + 该 step → 取 step.tool 作为底层工具 ID
//   → 通过 ToolRegistry.execute() 调用底层工具
//
// ## 引用关系
// - 创建：`SkillEngine::install_executor()` 在 CoreLoop wire-up 阶段一次性创建
// - 持有：`Weak<ToolRegistry>` + `Weak<RwLock<SkillEngine>>` 避免引用循环
// - 消费：`ToolRegistry::execute()` HashMap dispatch
// - 销毁：随 ToolRegistry 销毁；Weak 升级失败时返回错误而非 panic
//
// ## 为什么用 Weak
// SkillExecutor 持有 registry，registry.executors 又持有 SkillExecutor → 强引用循环。
// 用 Weak 让 ToolRegistry drop 时引用计数干净归零，CoreLoop 退出无内存泄漏。
pub struct SkillExecutor {
    /// 反查 SkillDef → step.tool 用
    engine: std::sync::Weak<tokio::sync::RwLock<SkillEngine>>,
    /// dispatch 到底层真实工具用
    registry: std::sync::Weak<crate::tool::ToolRegistry>,
    /// BehaviorPalace 弱引用，用于 Skill 执行结果写入
    ///
    /// 引用: DualPalaceMemory.behavior（Arc 弱化避免循环）
    /// 生命周期: CoreLoop 持有 strong Arc; Executor 持有 Weak，升级失败时静默跳过写入
    palace: std::sync::Weak<crate::memory_palace::DualPalaceMemory>,
}

impl SkillExecutor {
    pub fn new(
        engine: std::sync::Weak<tokio::sync::RwLock<SkillEngine>>,
        registry: std::sync::Weak<crate::tool::ToolRegistry>,
        palace: std::sync::Weak<crate::memory_palace::DualPalaceMemory>,
    ) -> Self {
        Self { engine, registry, palace }
    }

    // parse_skill_tool_id 已废弃：
    // 改为通过 SkillEngine.name_map O(1) 反查。
    // 旧实现依赖 ToolId 中保留 `/` `.` 分隔符可逆 split——sanitize 后这些都成 `_`，
    // 无法可靠拆分（skill_id / step_id 自身可能含 _）。
}

#[async_trait::async_trait]
impl crate::tool::ToolExecutor for SkillExecutor {
    async fn execute(
        &self,
        tool_id: &ToolId,
        params: serde_json::Value,
        ctx: &crate::tool::ExecutionContext,
    ) -> abacus_types::Result<serde_json::Value> {
        use abacus_types::KernelError;

        // 升级 Weak → Arc；失败说明 CoreLoop 已销毁
        let engine_arc = self.engine.upgrade()
            .ok_or_else(|| KernelError::Other("SkillEngine dropped".into()))?;
        let registry_arc = self.registry.upgrade()
            .ok_or_else(|| KernelError::Other("ToolRegistry dropped".into()))?;

        // 反查 sanitized ToolId → (skill_id, step_id) → step.tool（底层真实工具 ID）
        let (skill_id, step_id, real_tool_id) = {
            let engine = engine_arc.read().await;
            let (skill_id, step_id) = engine.lookup_skill_step(&tool_id.0)
                .ok_or_else(|| KernelError::Other(format!(
                    "SkillExecutor: tool_id '{}' not registered (skill not loaded?)",
                    tool_id.0
                )))?;
            let def = engine.skills.get(&skill_id)
                .ok_or_else(|| KernelError::Other(format!("skill not found: {}", skill_id.0)))?;
            let step = def.workflow.iter()
                .find(|s| s.id == step_id)
                .ok_or_else(|| KernelError::Other(format!(
                    "step '{}' not found in skill '{}'", step_id, skill_id.0
                )))?;
            (skill_id, step_id, ToolId(step.tool.clone()))
        };

        // 防递归：禁止 skill step 嵌套 skill 工具（避免循环依赖）
        // 单一命名约定：skill ToolId 以 "skill_" 开头
        if real_tool_id.0.starts_with("skill_") {
            return Err(KernelError::Other(format!(
                "SkillExecutor: nested skill calls not allowed (step.tool='{}')",
                real_tool_id.0
            )));
        }

        // 调用底层真实工具——复用现有 ExecutionContext
        let output = registry_arc.execute(&real_tool_id, params, ctx).await?;
        if !output.success {
            // Skill ↔ 行为宫殿协同：记录失败执行结果
            // 引用: BehaviorPalace.record_interaction / record_tool_behavior (memory_palace.rs)
            // 生命周期: 单次异步写入，不阻塞错误返回
            if let Some(palace) = self.palace.upgrade() {
                let skill_tag = skill_id.0.clone();
                tokio::spawn(async move {
                    let tags: Vec<String> = vec![
                        skill_tag.clone(),
                        "skill".into(),
                        "fail".into(),
                    ];
                    palace.record_interaction(&format!("skill:{}", skill_tag), &tags).await;
                    palace.record_tool_behavior(&format!("skill:{}", skill_tag), false).await;
                });
            }
            // 透传底层错误
            return Err(KernelError::Other(format!(
                "skill step '{}/{}' underlying tool '{}' failed: {}",
                skill_id.0, step_id, real_tool_id.0, output.output
            )));
        }

        // Skill ↔ 行为宫殿协同：记录成功执行结果
        // 引用: BehaviorPalace.record_interaction / record_tool_behavior (memory_palace.rs)
        // 生命周期: 单次异步写入，不阻塞结果返回
        if let Some(palace) = self.palace.upgrade() {
            let skill_tag = skill_id.0.clone();
            tokio::spawn(async move {
                let tags: Vec<String> = vec![
                    skill_tag.clone(),
                    "skill".into(),
                    "success".into(),
                ];
                palace.record_interaction(&format!("skill:{}", skill_tag), &tags).await;
                palace.record_tool_behavior(&format!("skill:{}", skill_tag), true).await;
            });
        }

        Ok(output.output)
    }
}

fn experience_multiplier(exp: &SkillExperience) -> f64 {
    // R4: 归一化经验乘数，确保输出稳定 [0, 1.5]
    // - easiness: SM-2 最小值 1.3，理论上界无界，cap 到 5.0 归一化
    // - success_rate: 天然 [0, 1]
    // - avg_latency_ms: 以 5000ms 为参考基线，S 曲线 / (1 + ratio) 确保 [0, 1)
    let easiness_norm = (exp.sm2.easiness.min(5.0) - 1.3) / 3.7; // [0, 1]
    let success_term = exp.success_rate.min(1.0);
    let latency_ratio = (exp.avg_latency_ms / 5000.0).min(10.0);
    let latency_term = 1.0 / (1.0 + latency_ratio); // S 曲线 [0.09, 1.0]

    0.5 * easiness_norm + 0.3 * success_term + 0.2 * latency_term
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_types::SkillTriggers;

    fn make_def(id: &str) -> SkillDef {
        SkillDef {
            id: SkillId(id.into()),
            version: "1.0".into(),
            triggers: SkillTriggers {
                keywords: vec![id.into()],
                regex: vec![],
                domain: vec![],
            },
            workflow: vec![],
            prompt: String::new(),
            knowledge_refs: vec![],
        }
    }

    #[test]
    fn test_trigger_match() {
        let mut matcher = TriggerMatcher::new();
        let id = SkillId("test-skill".into());
        matcher.register(&id, &SkillTriggers {
            keywords: vec!["hello".into(), "test".into()],
            regex: vec![],
            domain: vec![],
        }, "");

        let result = matcher.evaluate("hello world", None);
        assert!(!result.is_empty());
        assert_eq!(result[0].id.0, "test-skill");
    }

    #[test]
    fn test_semantic_match_english() {
        let mut matcher = TriggerMatcher::new();
        matcher.set_semantic_weight(0.8);

        let id = SkillId("fileops".into());
        matcher.register(&id, &SkillTriggers {
            keywords: vec!["file".into()],
            regex: vec![],
            domain: vec![],
        }, "Read, write, edit, and manage files");

        let result = matcher.evaluate("please edit this file", None);
        assert!(!result.is_empty(), "edit + file should match fileops");
        assert_eq!(result[0].id.0, "fileops");
    }

    #[test]
    fn test_semantic_match_filesys() {
        let mut matcher = TriggerMatcher::new();
        matcher.set_semantic_weight(0.8);

        let id = SkillId("filesys".into());
        matcher.register(&id, &SkillTriggers {
            keywords: vec!["filesys".into()],
            regex: vec![],
            domain: vec![],
        }, "filesystem read write and management operations");

        let result = matcher.evaluate("edit this file please", None);
        assert!(!result.is_empty(), "file-related query should match");
    }

    #[test]
    fn test_semantic_chinese_no_match() {
        let mut matcher = TriggerMatcher::new();
        matcher.set_semantic_weight(0.5);

        let id = SkillId("sysadmin".into());
        matcher.register(&id, &SkillTriggers {
            keywords: vec![],
            regex: vec![],
            domain: vec![],
        }, "System administration operations");

        let result = matcher.evaluate("write me a poem about ai", None);
        assert!(result.is_empty(), "poetry should not match sysadmin");
    }

    #[test]
    fn test_semantic_only_no_keywords() {
        let mut matcher = TriggerMatcher::new();
        matcher.set_semantic_weight(1.0);

        let id = SkillId("textops".into());
        matcher.register(&id, &SkillTriggers {
            keywords: vec![],
            regex: vec![],
            domain: vec![],
        }, "Text editing and formatting operations");

        let result = matcher.evaluate("edit and format text content", None);
        assert!(!result.is_empty(), "text processing should match");
    }

    #[tokio::test]
    async fn test_record_execution() {
        let mut engine = SkillEngine::new();
        engine.register_skill(make_def("my-skill"));

        engine.record_execution(SkillExecutionRecord {
            skill_id: SkillId("my-skill".into()),
            input: "test".into(),
            matched_triggers: vec![],
            steps_executed: 2,
            total_steps: 2,
            total_latency_ms: 1000,
            exit_code: 0,
            user_feedback: None,
            timestamp: 12345,
        });

        let experiences = engine.experiences.read().unwrap();
        let exp = experiences.get(&SkillId("my-skill".into())).unwrap();
        assert_eq!(exp.invoke_count, 1);
        assert_eq!(exp.success_rate, 1.0);
        assert!(exp.avg_latency_ms > 0.0);
    }

    // ─── Phase 2: SkillExecutor 测试 ─────────────────────────────────────

    // 旧 parse_skill_tool_id 测试已删除：
    // 命名约定改为单一形态（schema.name == ToolId == LLM 协议合规字符），
    // ToolId 中所有分隔符均 sanitize 为 _，无法可靠 split 还原 skill_id/step_id。
    // 反查通过 SkillEngine.name_map（O(1) HashMap）实现，需要先 load skill 才能验证；
    // 端到端覆盖在 skill_executor_blocks_nested_skill 等下方测试中。

    /// SkillExecutor 应禁止嵌套 skill 调用（防递归）
    #[tokio::test]
    async fn skill_executor_blocks_nested_skill() {
        use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};
        use std::sync::Arc;
        use tokio::sync::RwLock as TokioRwLock;

        let mut engine = SkillEngine::new();
        let mut def = make_def("outer");
        // 故意让 step.tool 指向另一个 skill ToolId（防递归测试关键点）
        def.workflow = vec![abacus_types::SkillStep {
            id: "step1".into(),
            description: "calls inner skill".into(),
            // step.tool 形态是底层 ToolId.0（也是 _ 命名），直接用 nested skill 形态
            tool: "skill_inner_bar_step_y".into(),
            params: serde_json::json!({}),
            depends_on: None,
            condition: None,
            fallback: None,
        }];
        engine.register_skill(def);
        let engine_arc = Arc::new(TokioRwLock::new(engine));
        let registry_arc = Arc::new(ToolRegistry::new());

        // 触发 load → 填充 name_map（execute 时反查需要）
        let dummy_executor: Arc<dyn ToolExecutor> = {
            struct Noop;
            #[async_trait::async_trait]
            impl ToolExecutor for Noop {
                async fn execute(&self, _: &ToolId, _: serde_json::Value, _: &ExecutionContext)
                    -> abacus_types::Result<serde_json::Value> { Ok(serde_json::json!({})) }
            }
            Arc::new(Noop)
        };
        engine_arc.write().await.load(
            &SkillId("outer".into()), &registry_arc, dummy_executor,
        ).await.unwrap();

        let executor = SkillExecutor::new(
            Arc::downgrade(&engine_arc),
            Arc::downgrade(&registry_arc),
            std::sync::Weak::new(),
        );

        let ctx = ExecutionContext::noop("test");
        // sanitized ToolId（与 load 内 format!("skill_{}_{}_step_{}") 一致）
        let result = executor.execute(
            &ToolId("skill_outer_skill_inner_bar_step_y_step_step1".into()),
            serde_json::json!({}),
            &ctx,
        ).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("nested skill"),
            "expected nested skill block, got: {msg}");
    }

    /// SkillExecutor 反查并执行底层真实工具（end-to-end）
    #[tokio::test]
    async fn skill_executor_dispatches_to_real_tool() {
        use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};
        use std::sync::Arc;
        use tokio::sync::RwLock as TokioRwLock;

        // mock 一个底层 echo 工具
        struct EchoTool;
        #[async_trait::async_trait]
        impl ToolExecutor for EchoTool {
            async fn execute(
                &self,
                _tool_id: &ToolId,
                params: serde_json::Value,
                _ctx: &ExecutionContext,
            ) -> abacus_types::Result<serde_json::Value> {
                Ok(serde_json::json!({"echoed": params}))
            }
        }

        let mut engine = SkillEngine::new();
        let mut def = make_def("review");
        def.workflow = vec![abacus_types::SkillStep {
            id: "do_echo".into(),
            description: "echo step".into(),
            tool: "echo".into(),
            params: serde_json::json!({}),
            depends_on: None,
            condition: None,
            fallback: None,
        }];
        engine.register_skill(def);

        let engine_arc = Arc::new(TokioRwLock::new(engine));
        let registry_arc = Arc::new(ToolRegistry::new());

        // 注册底层 echo 工具到 registry
        registry_arc.register(ToolHandle {
            id: ToolId("echo".into()),
            schema: ToolSchema {
                name: "echo".into(),
                description: "echo".into(),
                parameters: serde_json::json!({}),
                returns: None, security: None, cost: None,
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: false,
            },
            provider: ToolProvider::BuiltIn,
            state: abacus_types::ToolState::Loaded,
            effectiveness: Default::default(),
        }).await;
        registry_arc.register_executor(ToolId("echo".into()), Arc::new(EchoTool)).await;

        let executor = SkillExecutor::new(
            Arc::downgrade(&engine_arc),
            Arc::downgrade(&registry_arc),
            std::sync::Weak::new(),
        );

        let ctx = ExecutionContext::noop("test");
        // 触发 load → 填充 name_map（execute 时反查需要）
        engine_arc.write().await.load(
            &SkillId("review".into()), &registry_arc,
            Arc::new(EchoTool) as Arc<dyn ToolExecutor>,
        ).await.unwrap();

        let result = executor.execute(
            &ToolId("skill_review_echo_step_do_echo".into()),
            serde_json::json!({"hello": "world"}),
            &ctx,
        ).await;
        assert!(result.is_ok(), "execute should succeed: {:?}", result.err());
        let val = result.unwrap();
        assert_eq!(val["echoed"]["hello"], "world",
            "echo should round-trip params");
    }
}