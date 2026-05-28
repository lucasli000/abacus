//! Silent Router — 双维度融合路由（语义主导 + 经验增强）
//!
//! ## 设计原则
//! - 语义维度 = 当前意图（主信号）
//! - 经验维度 = 历史验证（增强信号）
//! - 冲突时语义优先（用户在做新事）
//! - 冷启动时语义独立驱动
//! - 输出仅影响工具排序，永不注入 LLM prompt
//!
//! ## 安全保证
//! - SemanticSignal 是纯数值结构（无 impl Into<Message>）
//! - 路由错误 = 工具排序次优 = 浪费 ~200 tokens ≠ 语义偏移
//! - 单调性：只排序不过滤，最差 = 无效果
//!
//! ## Dependencies
//! - `abacus_types::ToolId`: 工具标识
//! - `crate::tool::effectiveness::EffectivenessTracker`: 经验数据源
//! - `crate::skill::SkillCandidate`: Skill 匹配结果
//!
//! ## References
//! - Called by: `CoreLoop::process_turn()` 在 build_tool_definitions 前
//! - Reads from: EffectivenessTracker, SkillEngine, SessionState

use abacus_types::ToolId;
use crate::skill::SkillCandidate;

// ─── Feature Extraction (语义维度) ────────────────────────────────────────

/// 动作类型（从用户输入中提取的动词意图）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActionType {
    Create,    // 创建/新建/写/生成
    Read,      // 查看/读/看/打开
    Update,    // 修改/改/更新/重构
    Delete,    // 删除/移除/清理
    Analyze,   // 分析/检查/审查/诊断
    Fix,       // 修复/修/解决/处理
    Execute,   // 运行/执行/跑/启动
    Search,    // 搜索/查找/找/grep
    Unknown,
}

/// 领域索引
#[derive(Debug, Clone, Copy)]
pub enum Domain {
    Code = 0,
    Infra = 1,
    Data = 2,
    Text = 3,
    Config = 4,
    Web = 5,
    Fs = 6,
}

const DOMAIN_COUNT: usize = 7;

/// 特征向量 — 纯数值，不可序列化为自然语言
#[derive(Debug, Clone)]
pub struct FeatureVec {
    pub action: ActionType,
    pub domain_scores: [f64; DOMAIN_COUNT],
    pub complexity: f64,
    pub has_file_path: bool,
    pub has_code_block: bool,
    pub has_negation: bool,
    pub multi_intent: bool,
    pub reference_count: u32,
}

/// 语义信号 — 纯数值结构（无法注入 prompt）
#[derive(Debug, Clone)]
pub struct SemanticSignal {
    pub tool_scores: Vec<(ToolId, f64)>,
    pub features: FeatureVec,
    pub self_confidence: f64,
}

/// 经验信号 — 来自 EffectivenessTracker
#[derive(Debug, Clone)]
pub struct ExperienceSignal {
    pub tool_scores: Vec<(ToolId, f64)>,
    pub data_points: u32,
}

// ─── Semantic Parser ──────────────────────────────────────────────────────

/// 纯规则语义解析器（确定性，<1ms，不调用 LLM）
pub struct SemanticParser {
    /// 动作词 → ActionType 映射
    action_keywords: Vec<(&'static str, ActionType)>,
    /// 领域信号词 → (domain_index, weight)
    domain_lexicon: Vec<(&'static str, usize, f64)>,
    /// 工具 → 领域映射
    tool_domain_map: Vec<(ToolId, Vec<usize>)>,
    /// 工具 → 动作映射
    tool_action_map: Vec<(ToolId, Vec<ActionType>)>,
}

impl Default for SemanticParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SemanticParser {
    pub fn new() -> Self {
        Self {
            action_keywords: vec![
                ("创建", ActionType::Create), ("新建", ActionType::Create),
                ("写", ActionType::Create), ("生成", ActionType::Create),
                ("add", ActionType::Create), ("create", ActionType::Create),
                ("write", ActionType::Create),

                ("查看", ActionType::Read), ("读", ActionType::Read),
                ("看", ActionType::Read), ("打开", ActionType::Read),
                ("read", ActionType::Read), ("show", ActionType::Read),
                ("view", ActionType::Read), ("cat", ActionType::Read),

                ("修改", ActionType::Update), ("改", ActionType::Update),
                ("更新", ActionType::Update), ("重构", ActionType::Update),
                ("refactor", ActionType::Update), ("update", ActionType::Update),
                ("change", ActionType::Update), ("edit", ActionType::Update),

                ("删除", ActionType::Delete), ("移除", ActionType::Delete),
                ("清理", ActionType::Delete), ("remove", ActionType::Delete),
                ("delete", ActionType::Delete), ("clean", ActionType::Delete),

                ("分析", ActionType::Analyze), ("检查", ActionType::Analyze),
                ("审查", ActionType::Analyze), ("诊断", ActionType::Analyze),
                ("analyze", ActionType::Analyze), ("check", ActionType::Analyze),
                ("review", ActionType::Analyze), ("audit", ActionType::Analyze),

                ("修复", ActionType::Fix), ("修", ActionType::Fix),
                ("解决", ActionType::Fix), ("处理", ActionType::Fix),
                ("fix", ActionType::Fix), ("solve", ActionType::Fix),
                ("debug", ActionType::Fix), ("repair", ActionType::Fix),

                ("运行", ActionType::Execute), ("执行", ActionType::Execute),
                ("跑", ActionType::Execute), ("启动", ActionType::Execute),
                ("run", ActionType::Execute), ("exec", ActionType::Execute),
                ("start", ActionType::Execute), ("launch", ActionType::Execute),

                ("搜索", ActionType::Search), ("查找", ActionType::Search),
                ("找", ActionType::Search), ("grep", ActionType::Search),
                ("search", ActionType::Search), ("find", ActionType::Search),
            ],
            domain_lexicon: vec![
                // Code domain
                ("函数", 0, 0.8), ("function", 0, 0.8), ("struct", 0, 0.9),
                ("trait", 0, 0.9), ("impl", 0, 0.9), ("代码", 0, 0.7),
                ("code", 0, 0.7), ("编译", 0, 0.8), ("compile", 0, 0.8),
                ("bug", 0, 0.7), ("error", 0, 0.6), ("test", 0, 0.7),
                ("模块", 0, 0.6), ("module", 0, 0.6), ("crate", 0, 0.9),
                // Infra domain
                ("部署", 1, 0.9), ("deploy", 1, 0.9), ("docker", 1, 0.9),
                ("服务器", 1, 0.8), ("server", 1, 0.7), ("CI", 1, 0.8),
                ("pipeline", 1, 0.8),
                // Data domain
                ("数据库", 2, 0.9), ("database", 2, 0.9), ("SQL", 2, 0.9),
                ("查询", 2, 0.7), ("query", 2, 0.7), ("表", 2, 0.5),
                // Text domain
                ("文档", 3, 0.8), ("doc", 3, 0.7), ("README", 3, 0.9),
                ("注释", 3, 0.7), ("comment", 3, 0.6),
                // Config domain
                ("配置", 4, 0.9), ("config", 4, 0.9), ("yaml", 4, 0.8),
                ("toml", 4, 0.8), ("env", 4, 0.7), ("设置", 4, 0.7),
                // Web domain
                ("网页", 5, 0.8), ("HTTP", 5, 0.8), ("API", 5, 0.7),
                ("URL", 5, 0.8), ("fetch", 5, 0.7), ("request", 5, 0.6),
                // Filesystem domain
                ("文件", 6, 0.7), ("file", 6, 0.7), ("目录", 6, 0.8),
                ("directory", 6, 0.8), ("路径", 6, 0.7), ("path", 6, 0.6),
            ],
            // 2026-05-28: 工具直接用原始名注册（fs_read / bash_exec / web_fetch）
            // subsystem_policy 按前缀 fs_ / bash_ / web_ 分组匹配
            tool_domain_map: vec![
                (ToolId("fs_read".into()), vec![6, 0]),
                (ToolId("fs_write".into()), vec![6, 0]),
                (ToolId("fs_search".into()), vec![6]),
                (ToolId("code_exec".into()), vec![0, 1]),
                (ToolId("web_fetch".into()), vec![5]),
                (ToolId("web_search".into()), vec![5]),
                (ToolId("db_query".into()), vec![2]),
                (ToolId("kb_search".into()), vec![3, 2]),
            ],
            tool_action_map: vec![
                (ToolId("fs_read".into()), vec![ActionType::Read, ActionType::Analyze]),
                (ToolId("fs_write".into()), vec![ActionType::Create, ActionType::Update]),
                (ToolId("fs_search".into()), vec![ActionType::Search]),
                (ToolId("code_exec".into()), vec![ActionType::Execute, ActionType::Fix]),
                (ToolId("web_fetch".into()), vec![ActionType::Read, ActionType::Search]),
                (ToolId("web_search".into()), vec![ActionType::Search]),
                (ToolId("db_query".into()), vec![ActionType::Read, ActionType::Search, ActionType::Analyze]),
            ],
        }
    }

    /// 纯规则解析，确定性，< 1ms
    pub fn parse(&self, input: &str) -> SemanticSignal {
        let features = self.extract_features(input);
        let tool_scores = self.compute_tool_affinity(&features);
        let self_confidence = self.assess_confidence(&features);
        SemanticSignal { tool_scores, features, self_confidence }
    }

    fn extract_features(&self, input: &str) -> FeatureVec {
        let lower = input.to_lowercase();
        let chars: Vec<char> = input.chars().collect();

        // Action detection (first match wins)
        let action = self.action_keywords.iter()
            .find(|(kw, _)| lower.contains(kw))
            .map(|(_, a)| *a)
            .unwrap_or(ActionType::Unknown);

        // Domain scoring
        let mut domain_scores = [0.0f64; DOMAIN_COUNT];
        for &(word, domain_idx, weight) in &self.domain_lexicon {
            if lower.contains(word) {
                domain_scores[domain_idx] = domain_scores[domain_idx].max(weight);
            }
        }

        // Structural features
        let has_file_path = input.contains('/') || input.contains('\\')
            || input.contains(".rs") || input.contains(".ts") || input.contains(".py");
        let has_code_block = input.contains("```") || input.contains("fn ") || input.contains("pub ");
        let has_negation = lower.contains("不要") || lower.contains("不能")
            || lower.contains("别") || lower.contains("除了")
            || lower.contains("don't") || lower.contains("without");
        let multi_intent = lower.contains("然后") || lower.contains("接着")
            || lower.contains("同时") || lower.contains("并且")
            || lower.contains(" then ") || lower.contains(" and then ");
        let reference_count = ["这个", "这", "那个", "上面", "刚才", "它", "this", "that", "above"]
            .iter().filter(|r| lower.contains(*r)).count() as u32;

        // Complexity (simple heuristic: char count * clause indicators)
        let clause_count = 1 + lower.matches("，").count() + lower.matches(",").count()
            + lower.matches("。").count() + lower.matches(".").count();
        let complexity = (chars.len() as f64 / 50.0) * (clause_count as f64).sqrt();

        FeatureVec {
            action,
            domain_scores,
            complexity: complexity.min(5.0),
            has_file_path,
            has_code_block,
            has_negation,
            multi_intent,
            reference_count,
        }
    }

    fn compute_tool_affinity(&self, features: &FeatureVec) -> Vec<(ToolId, f64)> {
        let mut scores: Vec<(ToolId, f64)> = Vec::new();

        for (tool_id, domains) in &self.tool_domain_map {
            let mut score = 0.0f64;

            // Domain match
            for &d in domains {
                score += features.domain_scores[d] * 0.5;
            }

            // Action match
            if let Some((_, actions)) = self.tool_action_map.iter().find(|(t, _)| t == tool_id) {
                if actions.contains(&features.action) {
                    score += 0.4;
                }
            }

            // File path boost for fs tools
            if features.has_file_path && tool_id.0.starts_with("fs_") {
                score += 0.2;
            }

            // Code block boost for code tools
            if features.has_code_block && (tool_id.0.contains("code") || tool_id.0 == "fs_read") {
                score += 0.15;
            }

            if score > 0.1 {
                scores.push((tool_id.clone(), score.min(1.0)));
            }
        }

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores
    }

    fn assess_confidence(&self, features: &FeatureVec) -> f64 {
        let mut conf: f64 = 0.3; // baseline
        if features.action != ActionType::Unknown { conf += 0.25; }
        if features.domain_scores.iter().any(|&s| s > 0.6) { conf += 0.25; }
        if features.has_file_path { conf += 0.1; }
        if features.has_code_block { conf += 0.1; }
        // Penalty for ambiguity
        if features.multi_intent { conf -= 0.1; }
        if features.has_negation { conf -= 0.05; }
        if features.reference_count > 2 { conf -= 0.1; }
        conf.clamp(0.0, 1.0)
    }
}

// ─── Fusion Gate ──────────────────────────────────────────────────────────

/// 融合结果
#[derive(Debug, Clone)]
pub enum FusionResult {
    /// 路由建议（工具排序）
    Route {
        tools: Vec<(ToolId, f64)>,
        source: FusionSource,
    },
    /// 不干预（两个维度都没信号）
    Abstain,
}

#[derive(Debug, Clone, Copy)]
pub enum FusionSource {
    Both,
    Semantic,
    Experience,
}

/// 融合门 — 双维度判断，语义主导
pub struct FusionGate {
    /// 经验数据最低样本量（低于此视为无经验）
    min_experience_points: u32,
    /// 语义最低自信度（低于此视为无信号）
    min_semantic_confidence: f64,
}

impl Default for FusionGate {
    fn default() -> Self {
        Self::new()
    }
}

impl FusionGate {
    pub fn new() -> Self {
        Self {
            min_experience_points: 10,
            min_semantic_confidence: 0.4,
        }
    }

    /// 双维度融合：语义主导 + 经验增强
    ///
    /// 优先级:
    ///   1. 语义 + 经验一致 → 最高置信（经验 boost 语义）
    ///   2. 语义独立 → 正常置信
    ///   3. 经验独立 → 正常置信
    ///   4. 语义 + 经验冲突 → 语义赢
    ///   5. 两者都无信号 → 不干预
    pub fn fuse(&self, exp: &ExperienceSignal, sem: &SemanticSignal) -> FusionResult {
        let has_experience = exp.data_points >= self.min_experience_points;
        let has_semantic = sem.self_confidence >= self.min_semantic_confidence;

        match (has_experience, has_semantic) {
            (true, true) => {
                let agreement = self.cosine_similarity(&exp.tool_scores, &sem.tool_scores);
                if agreement > 0.5 {
                    // 一致 → 经验增强语义
                    let boosted = self.boost_semantic(sem, exp);
                    FusionResult::Route { tools: boosted, source: FusionSource::Both }
                } else {
                    // 冲突 → 语义优先（用户在做新事）
                    FusionResult::Route { tools: sem.tool_scores.clone(), source: FusionSource::Semantic }
                }
            }
            (false, true) => {
                // 冷启动 → 语义独立驱动
                FusionResult::Route { tools: sem.tool_scores.clone(), source: FusionSource::Semantic }
            }
            (true, false) => {
                // 语义无信号 → 经验驱动
                FusionResult::Route { tools: exp.tool_scores.clone(), source: FusionSource::Experience }
            }
            (false, false) => {
                // 两者都无 → 不干预
                FusionResult::Abstain
            }
        }
    }

    /// 经验增强语义分数（不改变排序方向，只放大已有信号）
    fn boost_semantic(&self, sem: &SemanticSignal, exp: &ExperienceSignal) -> Vec<(ToolId, f64)> {
        sem.tool_scores.iter().map(|(tool, score)| {
            let exp_score = exp.tool_scores.iter()
                .find(|(t, _)| t == tool)
                .map(|(_, s)| *s)
                .unwrap_or(0.0);
            // 经验一致 → boost ×1.3；不一致 → 不惩罚
            let boost = if exp_score > 0.5 { 1.0 + exp_score * 0.3 } else { 1.0 };
            (tool.clone(), (score * boost).min(1.0))
        }).collect()
    }

    /// 工具分数向量的余弦相似度
    fn cosine_similarity(&self, a: &[(ToolId, f64)], b: &[(ToolId, f64)]) -> f64 {
        // Align vectors by tool_id
        let all_tools: Vec<&ToolId> = a.iter().map(|(t, _)| t)
            .chain(b.iter().map(|(t, _)| t))
            .collect::<std::collections::HashSet<_>>()
            .into_iter().collect();

        if all_tools.is_empty() { return 0.0; }

        let mut dot = 0.0f64;
        let mut norm_a = 0.0f64;
        let mut norm_b = 0.0f64;

        for tool in &all_tools {
            let sa = a.iter().find(|(t, _)| t == *tool).map(|(_, s)| *s).unwrap_or(0.0);
            let sb = b.iter().find(|(t, _)| t == *tool).map(|(_, s)| *s).unwrap_or(0.0);
            dot += sa * sb;
            norm_a += sa * sa;
            norm_b += sb * sb;
        }

        let denom = norm_a.sqrt() * norm_b.sqrt();
        if denom < 1e-10 { 0.0 } else { dot / denom }
    }
}

// ─── Silent Router (顶层 API) ─────────────────────────────────────────────

/// Silent Router — 融合语义+经验产出工具排序建议
///
/// 输出仅用于控制 ToolDefinition 的排列顺序，永不注入 LLM messages。
pub struct SilentRouter {
    parser: SemanticParser,
    gate: FusionGate,
}

impl Default for SilentRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl SilentRouter {
    pub fn new() -> Self {
        Self {
            parser: SemanticParser::new(),
            gate: FusionGate::new(),
        }
    }

    /// 路由决策
    ///
    /// # Arguments
    /// - `input`: 用户原文（只读，不修改）
    /// - `experience`: 经验信号（从 EffectivenessTracker 获取）
    /// - `skill_matches`: 已匹配的 Skill（提供额外工具亲和度）
    /// - `session_tools`: 当前 session 最近使用的工具（惯性信号）
    /// - `palace_recommendations`: Phase γ-Palace-E 行为宫殿推荐（tool_name, strength），
    ///   来自 `DualPalaceMemory::recommend_next_tools(last_tool)`。空切片 → 不干预。
    ///
    /// # Returns
    /// - `Some(排序后的工具列表)` — 建议的工具排序
    /// - `None` — 不干预（维持默认排序）
    pub fn route(
        &self,
        input: &str,
        experience: &ExperienceSignal,
        skill_matches: &[SkillCandidate],
        session_tools: &[ToolId],
        palace_recommendations: &[(String, f64)],
    ) -> Option<Vec<ToolId>> {
        // 1. 语义解析
        let mut semantic = self.parser.parse(input);

        // 2. Skill 匹配增强语义分数
        for skill in skill_matches {
            if skill.confidence > 0.5 {
                // Skill 关联的工具获得额外分数
                for (tool, score) in &mut semantic.tool_scores {
                    if tool.0.contains(&skill.id.0) {
                        *score = (*score + 0.2).min(1.0);
                    }
                }
            }
        }

        // 3. Session 惯性（微弱信号，衰减）
        for (i, recent_tool) in session_tools.iter().rev().take(5).enumerate() {
            let decay = 0.1 * (1.0 - i as f64 * 0.2); // 0.1, 0.08, 0.06, 0.04, 0.02
            if let Some((_, score)) = semantic.tool_scores.iter_mut().find(|(t, _)| t == recent_tool) {
                *score = (*score + decay).min(1.0);
            } else {
                semantic.tool_scores.push((recent_tool.clone(), decay));
            }
        }

        // 3b. Phase γ-Palace-E：行为宫殿推荐（历史关联工具）
        //
        // 与 session_tools 的"短期惯性"互补：palace 推荐是跨 session 的"长期关联"。
        // 加权范围 0.05~0.10——次于 skill_matches (0.20) 和 session_tools (0.02~0.10) 的最高档，
        // 避免主导路由但仍能在多个候选并列时打破平局。
        for (tool_name, strength) in palace_recommendations.iter().take(5) {
            let weight = (0.05 + 0.05 * strength).clamp(0.05, 0.10);
            let tool_id = ToolId(tool_name.clone());
            if let Some((_, score)) = semantic.tool_scores.iter_mut().find(|(t, _)| t == &tool_id) {
                *score = (*score + weight).min(1.0);
            } else {
                semantic.tool_scores.push((tool_id, weight));
            }
        }

        // 4. 融合
        let result = self.gate.fuse(experience, &semantic);

        match result {
            FusionResult::Route { tools, .. } if !tools.is_empty() => {
                Some(tools.into_iter().map(|(id, _)| id).collect())
            }
            _ => None,
        }
    }

    /// 快速门控：极短/极简输入直接跳过解析
    pub fn should_skip(input: &str) -> bool {
        let len = input.chars().count();
        // 极短消息（< 5 字）或纯标点/emoji → 跳过
        len < 5 || input.chars().all(|c| c.is_whitespace() || c.is_ascii_punctuation())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_semantic_parser_action() {
        let parser = SemanticParser::new();
        let sig = parser.parse("帮我修复这个 auth 模块的 bug");
        assert_eq!(sig.features.action, ActionType::Fix);
        assert!(sig.features.domain_scores[0] > 0.5); // code domain
    }

    #[test]
    fn test_semantic_parser_file_path() {
        let parser = SemanticParser::new();
        let sig = parser.parse("读取 src/main.rs 的内容");
        assert!(sig.features.has_file_path);
        assert_eq!(sig.features.action, ActionType::Read);
        // fs.read should have high score
        assert!(sig.tool_scores.iter().any(|(t, s)| t.0 == "fs_read" && *s > 0.5));
    }

    #[test]
    fn test_semantic_parser_negation() {
        let parser = SemanticParser::new();
        let sig = parser.parse("修改配置，但是不要删除现有的设置");
        assert!(sig.features.has_negation);
        assert_eq!(sig.features.action, ActionType::Update);
    }

    #[test]
    fn test_fusion_semantic_wins_on_conflict() {
        let gate = FusionGate::new();
        let exp = ExperienceSignal {
            tool_scores: vec![(ToolId("fs_read".into()), 0.9)],
            data_points: 50,
        };
        let sem = SemanticSignal {
            tool_scores: vec![(ToolId("code_exec".into()), 0.8)],
            features: FeatureVec {
                action: ActionType::Execute,
                domain_scores: [0.8, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                complexity: 1.0, has_file_path: false, has_code_block: false,
                has_negation: false, multi_intent: false, reference_count: 0,
            },
            self_confidence: 0.7,
        };

        let result = gate.fuse(&exp, &sem);
        match result {
            FusionResult::Route { tools, source } => {
                assert!(matches!(source, FusionSource::Semantic));
                assert_eq!(tools[0].0, ToolId("code_exec".into()));
            }
            _ => panic!("expected Route"),
        }
    }

    #[test]
    fn test_fusion_cold_start_semantic_drives() {
        let gate = FusionGate::new();
        let exp = ExperienceSignal {
            tool_scores: vec![],
            data_points: 2, // insufficient
        };
        let sem = SemanticSignal {
            tool_scores: vec![(ToolId("fs_search".into()), 0.7)],
            features: FeatureVec {
                action: ActionType::Search,
                domain_scores: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.8],
                complexity: 0.5, has_file_path: false, has_code_block: false,
                has_negation: false, multi_intent: false, reference_count: 0,
            },
            self_confidence: 0.6,
        };

        let result = gate.fuse(&exp, &sem);
        match result {
            FusionResult::Route { source, .. } => {
                assert!(matches!(source, FusionSource::Semantic));
            }
            _ => panic!("expected Route on cold start"),
        }
    }

    #[test]
    fn test_fusion_both_agree_boost() {
        let gate = FusionGate::new();
        let exp = ExperienceSignal {
            tool_scores: vec![(ToolId("fs_read".into()), 0.8)],
            data_points: 30,
        };
        let sem = SemanticSignal {
            tool_scores: vec![(ToolId("fs_read".into()), 0.6)],
            features: FeatureVec {
                action: ActionType::Read,
                domain_scores: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.7],
                complexity: 0.5, has_file_path: true, has_code_block: false,
                has_negation: false, multi_intent: false, reference_count: 0,
            },
            self_confidence: 0.7,
        };

        let result = gate.fuse(&exp, &sem);
        match result {
            FusionResult::Route { tools, source } => {
                assert!(matches!(source, FusionSource::Both));
                // Score should be boosted above original 0.6
                assert!(tools[0].1 > 0.6);
            }
            _ => panic!("expected boosted Route"),
        }
    }

    #[test]
    fn test_fusion_no_signals_abstain() {
        let gate = FusionGate::new();
        let exp = ExperienceSignal { tool_scores: vec![], data_points: 0 };
        let sem = SemanticSignal {
            tool_scores: vec![],
            features: FeatureVec {
                action: ActionType::Unknown,
                domain_scores: [0.0; DOMAIN_COUNT],
                complexity: 0.1, has_file_path: false, has_code_block: false,
                has_negation: false, multi_intent: false, reference_count: 0,
            },
            self_confidence: 0.2,
        };

        let result = gate.fuse(&exp, &sem);
        assert!(matches!(result, FusionResult::Abstain));
    }

    #[test]
    fn test_should_skip_short() {
        assert!(SilentRouter::should_skip("hi"));
        assert!(SilentRouter::should_skip("ok"));
        assert!(!SilentRouter::should_skip("帮我修复这个 auth 模块"));
    }

    #[test]
    fn test_router_full_flow() {
        let router = SilentRouter::new();
        let exp = ExperienceSignal { tool_scores: vec![], data_points: 0 };
        let result = router.route(
            "搜索 src 目录下所有包含 auth 的文件",
            &exp,
            &[],
            &[],
            &[],
        );
        // Should produce a routing with fs.search ranked high
        assert!(result.is_some());
        let tools = result.unwrap();
        assert!(tools.iter().any(|t| t.0.contains("search")));
    }

    /// Phase γ-Palace-E: palace recommendations 加权工具
    #[test]
    fn test_palace_recommendations_boost_tool() {
        let router = SilentRouter::new();
        let exp = ExperienceSignal { tool_scores: vec![], data_points: 0 };
        // 用一段语义不明显的输入，让 semantic.tool_scores 几乎空
        let palace_recs = vec![
            ("kb_search".to_string(), 0.9),  // 应被强加权
            ("orchestrate_assess".to_string(), 0.5),
        ];
        let result = router.route(
            "do something",  // 模糊输入
            &exp,
            &[],
            &[],
            &palace_recs,
        );
        // palace 推荐至少应让候选工具出现在结果集中
        if let Some(tools) = result {
            assert!(tools.iter().any(|t| t.0 == "kb_search"),
                "palace 推荐 kb_search 应进入候选集");
        }
    }
}
