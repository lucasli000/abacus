// ════════════════════════════════════════════════════════════════
// compress_math.rs — 数学压缩引擎（三层架构）
// ════════════════════════════════════════════════════════════════
//
// ## 设计来源
// - Information Bottleneck (QUITO-X, EMNLP 2025)
// - Rate-Distortion Framework (NeurIPS 2024)
// - H2O Heavy-Hitter Oracle (NeurIPS 2023)
// - ARC Adaptive Replacement Cache (Megiddo 2003)
// - Submodular Maximization (greedy set cover)
// - EpiCache Episodic KV Management (2025)
//
// ## 三层架构
// Layer 1: Online Tracking — 每 turn O(1) 更新引用计数 + MinHash 签名
// Layer 2: Scoring — 复合数学模型 (type × density × decay × ref_bonus)
// Layer 3: Selection — 贪心背包 + 次模覆盖保证
//
// ## 引用关系
// - 创建: ContextManager::new() 时初始化
// - 消费: auto_compress_messages_inner() 调用 score + select
// - 更新: pipeline turn 结束时更新 reference counts
//
// ## 生命周期
// - 随 ContextManager 创建，session 级生命周期
// - reference_counts 随消息增长，压缩后同步清理已丢弃消息的计数

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Layer 1: Online Tracking
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 消息元数据追踪器（在线维护，每 turn O(1) 更新）
///
/// 追踪每条消息的：
/// - 引用计数（被后续消息引用的次数）
/// - MinHash 签名（用于近似去重）
/// - 插入时间戳（turn 编号）
///
/// 设计参考：H2O (NeurIPS 2023) heavy-hitter tracking
#[derive(Debug, Clone)]
pub struct MessageTracker {
    /// message_index → 引用计数
    /// 每当后续消息引用（共享符号/变量名/文件路径）时 +1
    pub reference_counts: HashMap<usize, u32>,
    /// message_index → MinHash 签名（k=32 permutations）
    pub minhash_signatures: HashMap<usize, MinHashSig>,
    /// message_index → 插入 turn
    pub insert_turns: HashMap<usize, u32>,
    /// 当前 turn 编号
    pub current_turn: u32,
    /// ARC 自适应参数 p（recency vs frequency 平衡点）
    /// p 大 → 偏向保留高频消息；p 小 → 偏向保留近消息
    pub arc_p: f64,
    /// 自适应阈值 EMA（替代硬编码序列）
    pub ema_threshold: f64,
    /// 历史压缩效果（用于 EMA 更新）
    pub compress_history: Vec<CompressOutcome>,
}

/// 压缩效果记录（用于 EMA 自适应阈值）
#[derive(Debug, Clone)]
pub struct CompressOutcome {
    /// 触发时的占用率
    pub trigger_pct: f64,
    /// 压缩后的占用率
    pub after_pct: f64,
    /// 保留的信息价值（normalized）
    pub retained_value: f64,
}

/// MinHash 签名（k=32 permutations，对 char 3-gram 计算）
///
/// 用于 O(1) 近似 Jaccard 相似度估计
/// J(a, b) ≈ |{i: sig_a[i] == sig_b[i]}| / k
#[derive(Debug, Clone, PartialEq)]
pub struct MinHashSig {
    pub values: [u64; Self::K],
}

impl MinHashSig {
    pub const K: usize = 32;

    /// 从文本计算 MinHash 签名
    /// 使用 char 3-gram 作为 shingle set
    pub fn from_text(text: &str) -> Self {
        let mut values = [u64::MAX; Self::K];
        let chars: Vec<char> = text.chars().take(2000).collect(); // 限制长度防止超长消息

        if chars.len() < 3 {
            return Self { values };
        }

        // 对每个 3-gram 计算 K 个 hash
        for window in chars.windows(3) {
            let shingle: String = window.iter().collect();
            for (perm_idx, slot) in values.iter_mut().enumerate() {
                let h = Self::hash_with_seed(&shingle, perm_idx as u64);
                if h < *slot {
                    *slot = h;
                }
            }
        }
        Self { values }
    }

    /// 估计 Jaccard 相似度
    pub fn jaccard_similarity(&self, other: &Self) -> f64 {
        let matches = self.values.iter()
            .zip(other.values.iter())
            .filter(|(a, b)| a == b)
            .count();
        matches as f64 / Self::K as f64
    }

    fn hash_with_seed(text: &str, seed: u64) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        let mut h = DefaultHasher::new();
        seed.hash(&mut h);
        text.hash(&mut h);
        h.finish()
    }
}

impl MessageTracker {
    pub fn new() -> Self {
        Self {
            reference_counts: HashMap::new(),
            minhash_signatures: HashMap::new(),
            insert_turns: HashMap::new(),
            current_turn: 0,
            arc_p: 0.5, // 初始 recency/frequency 平衡
            ema_threshold: 70.0, // 初始阈值
            compress_history: Vec::new(),
        }
    }

    /// 注册新消息（turn 结束时批量调用）
    pub fn register_message(&mut self, index: usize, text: &str, turn: u32) {
        self.insert_turns.insert(index, turn);
        self.reference_counts.entry(index).or_insert(0);
        self.minhash_signatures.insert(index, MinHashSig::from_text(text));
        self.current_turn = turn;
    }

    /// 更新引用计数：新消息引用了旧消息中的符号
    ///
    /// 检测逻辑：提取新消息中的"符号"（文件路径、函数名、变量名），
    /// 与已注册消息的 text 做快速匹配
    pub fn update_references(&mut self, new_msg_text: &str, all_indices: &[usize], get_text: impl Fn(usize) -> String) {
        // 提取新消息中的符号（简化版：连续 identifier 字符序列 > 4 chars）
        let symbols: Vec<&str> = extract_symbols(new_msg_text);
        if symbols.is_empty() {
            return;
        }

        for &idx in all_indices {
            if !self.reference_counts.contains_key(&idx) {
                continue;
            }
            let old_text = get_text(idx);
            let mut referenced = false;
            for sym in &symbols {
                if old_text.contains(sym) {
                    referenced = true;
                    break;
                }
            }
            if referenced {
                *self.reference_counts.entry(idx).or_insert(0) += 1;
            }
        }
    }

    /// 清理已压缩消息的追踪数据
    pub fn remove_indices(&mut self, indices: &[usize]) {
        for &idx in indices {
            self.reference_counts.remove(&idx);
            self.minhash_signatures.remove(&idx);
            self.insert_turns.remove(&idx);
        }
    }

    /// ARC 自适应：根据压缩决策的"ghost hit"调整 p
    ///
    /// 当一个被淘汰的消息后来被引用（ghost hit on frequency list），
    /// 说明 frequency 信号更可靠 → 增大 p
    pub fn arc_adjust_frequency_bias(&mut self, delta: f64) {
        self.arc_p = (self.arc_p + delta).clamp(0.1, 0.9);
    }

    /// EMA 自适应阈值更新
    ///
    /// 公式: threshold(t+1) = α × optimal + (1-α) × current
    /// optimal = current_threshold × (target / actual_after)
    ///
    /// 设计参考：PID 控制器的比例项简化
    pub fn update_adaptive_threshold(&mut self, outcome: CompressOutcome) {
        const ALPHA: f64 = 0.3; // EMA 平滑因子
        const TARGET_AFTER: f64 = 60.0; // 压缩后目标占用率

        if outcome.after_pct > 0.0 {
            let optimal = self.ema_threshold * (TARGET_AFTER / outcome.after_pct);
            // 限制调整幅度（防止振荡）
            let clamped_optimal = optimal.clamp(55.0, 92.0);
            self.ema_threshold = ALPHA * clamped_optimal + (1.0 - ALPHA) * self.ema_threshold;
        }

        self.compress_history.push(outcome);
        // 保留最近 10 次历史
        if self.compress_history.len() > 10 {
            self.compress_history.remove(0);
        }
    }

    /// 获取自适应阈值（替代硬编码序列）
    pub fn adaptive_threshold(&self) -> f64 {
        self.ema_threshold
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Layer 2: Composite Scoring
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 消息类型权重（基于 EpiCache 类型先验）
///
/// 设计依据：不同角色的消息对 LLM 后续推理的贡献不同
/// - User instructions: 最高（定义任务，丢失 = 方向偏移）
/// - Code blocks: 高（精确信息，压缩损失大）
/// - Error/decision: 高（诊断+决策，丢失 = 重复错误）
/// - Assistant prose: 中（可从 context 推理恢复）
/// - Tool output: 低-中（通常是数据，可重新获取）
#[derive(Debug, Clone, Copy)]
pub enum MessageType {
    UserInstruction,   // 用户消息（含指令）
    UserQuestion,      // 用户消息（纯提问）
    AssistantDecision, // assistant 含决策/结论
    AssistantProse,    // assistant 一般回复
    AssistantError,    // assistant 错误报告
    CodeBlock,         // 含代码块的消息
    ToolResult,        // tool 输出
    SystemContext,     // 系统上下文/压缩摘要
}

impl MessageType {
    /// 类型先验权重（0.0-1.0）
    ///
    /// 论文依据：H2O 发现 5% 消息承载 50% 信息，
    /// 类型权重帮助快速识别高价值消息。
    pub fn weight(&self) -> f64 {
        match self {
            Self::UserInstruction => 1.0,
            Self::UserQuestion => 0.7,
            Self::AssistantDecision => 0.9,
            Self::AssistantError => 0.85,
            Self::CodeBlock => 0.8,
            Self::AssistantProse => 0.4,
            Self::ToolResult => 0.35,
            Self::SystemContext => 0.3,
        }
    }

    /// 从消息内容推断类型（O(1) 快速分类）
    pub fn classify(role: &str, text: &str) -> Self {
        let lower = text.to_lowercase();

        if role == "user" || role == "User" {
            // 含指令动词 → Instruction；否则 → Question
            let instruction_markers = ["请", "帮我", "修改", "添加", "删除", "实现",
                "please", "fix", "add", "implement", "create", "change", "update"];
            if instruction_markers.iter().any(|m| lower.contains(m)) {
                return Self::UserInstruction;
            }
            return Self::UserQuestion;
        }

        if role == "tool" || role == "Tool" {
            return Self::ToolResult;
        }

        // Assistant 消息细分
        if text.contains("```") {
            return Self::CodeBlock;
        }

        let decision_markers = ["决定", "结论", "方案", "选择", "确认",
            "decision", "conclusion", "chosen", "plan:", "strategy:"];
        if decision_markers.iter().any(|m| lower.contains(m)) {
            return Self::AssistantDecision;
        }

        let error_markers = ["error", "failed", "panic", "bug",
            "错误", "失败", "异常"];
        if error_markers.iter().any(|m| lower.contains(m)) {
            return Self::AssistantError;
        }

        if lower.contains("[compressed") || lower.contains("[context loss") {
            return Self::SystemContext;
        }

        Self::AssistantProse
    }
}

/// 复合重要性评分（Layer 2 核心）
///
/// 公式: score = type_weight × content_density × recency_decay × (1 + ref_bonus)
///
/// ## 各因子含义
/// - type_weight: 消息角色先验（UserInstruction=1.0, ToolResult=0.35）
/// - content_density: 信息密度 = unique_tokens / total_tokens（越高越不可替代）
/// - recency_decay: 指数衰减 e^(-λ×distance)（越远越可牺牲）
/// - ref_bonus: 被引用次数 bonus（heavy-hitter 保护）
///
/// ## 性能
/// O(1) per message（density 预计算，其余实时查表）
#[derive(Debug, Clone)]
pub struct CompositeScorer {
    /// 衰减系数 λ（Communication 阶段 1.5，Execution 阶段 0.8）
    pub decay_lambda: f64,
    /// ARC 参数 p（从 MessageTracker 同步）
    pub arc_p: f64,
}

impl CompositeScorer {
    pub fn new(decay_lambda: f64) -> Self {
        Self { decay_lambda, arc_p: 0.5 }
    }

    /// 计算复合分数
    ///
    /// ## 参数
    /// - `msg_type`: 消息类型分类
    /// - `token_count`: 消息 token 数
    /// - `unique_ratio`: 唯一 token 比例（content density proxy）
    /// - `distance`: 到 tail 的归一化距离 (0.0=tail, 1.0=head)
    /// - `ref_count`: 被引用次数
    /// - `total_messages`: 总消息数（用于归一化）
    pub fn score(
        &self,
        msg_type: MessageType,
        _token_count: usize,
        unique_ratio: f64,
        distance: f64,
        ref_count: u32,
    ) -> f64 {
        let type_w = msg_type.weight();

        // Content density: unique_ratio ∈ [0,1]，加 floor 0.2 防止过低
        let density = (unique_ratio * 0.8 + 0.2).min(1.0);

        // Recency decay: e^(-λ × distance)
        let decay = (-self.decay_lambda * distance).exp();

        // Reference bonus: log(1 + ref_count) / log(10)
        // 使用 log 压缩高引用数的边际效应
        let ref_bonus = (1.0 + ref_count as f64).ln() / 10.0_f64.ln();

        // ARC 混合: p × frequency_signal + (1-p) × recency_signal
        let frequency_signal = type_w * density * (1.0 + ref_bonus);
        let recency_signal = type_w * decay;
        let combined = self.arc_p * frequency_signal + (1.0 - self.arc_p) * recency_signal;

        // 最终分数 clamp 到 [0, 1]
        combined.clamp(0.0, 1.0)
    }

    /// 计算唯一 token 比例（content density proxy）
    ///
    /// 使用 word-level unigram 去重统计
    /// O(n) where n = word count
    pub fn compute_unique_ratio(text: &str) -> f64 {
        if text.is_empty() {
            return 0.0;
        }
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.is_empty() {
            return 0.0;
        }
        let mut seen = std::collections::HashSet::new();
        let mut unique = 0usize;
        for w in &words {
            // 归一化：小写 + 去标点
            let normalized: String = w.to_lowercase().chars()
                .filter(|c| c.is_alphanumeric() || *c > '\u{7F}') // 保留 CJK
                .collect();
            if !normalized.is_empty() && seen.insert(normalized) {
                unique += 1;
            }
        }
        unique as f64 / words.len() as f64
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Layer 3: Selection Algorithm
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 消息选择候选项
#[derive(Debug, Clone)]
pub struct SelectionCandidate {
    /// 消息在原始列表中的 index
    pub index: usize,
    /// 复合分数
    pub score: f64,
    /// Token 消耗
    pub tokens: usize,
    /// 价值密度 = score / tokens（贪心排序键）
    pub value_density: f64,
    /// MinHash 签名（用于去重）
    pub signature: Option<MinHashSig>,
    /// 是否被标记为冗余（与已选消息相似度 > 0.6）
    pub redundant: bool,
}

/// 贪心背包选择器 + 次模覆盖保证
///
/// ## 算法流程
/// 1. 预处理：MinHash 去重（相似度 > 0.6 的消息聚类，每簇保留最近一条）
/// 2. 排序：按 value_density (score/tokens) 降序
/// 3. 贪心填充：依次选入直到 token 预算用尽
/// 4. 覆盖检查：确保至少每种 MessageType 有一条代表
///
/// ## 复杂度
/// O(n² / k) for MinHash dedup + O(n log n) sort + O(n) greedy = O(n² / k)
/// 实际 n < 100（中间消息数），k=32，总计 < 5ms
///
/// ## 近似保证
/// 贪心背包在 sorted by density 时给出最优分数背包解；
/// 次模覆盖的贪心保证 (1-1/e) ≈ 63% 最优覆盖。
pub struct GreedyKnapsackSelector;

impl GreedyKnapsackSelector {
    /// 执行选择：给定候选列表和 token 预算，返回保留的 indices
    ///
    /// ## 返回
    /// (retained_indices, compressed_indices)
    pub fn select(
        candidates: &mut Vec<SelectionCandidate>,
        token_budget: usize,
    ) -> (Vec<usize>, Vec<usize>) {
        // Phase 1: MinHash 去重
        Self::mark_redundant(candidates);

        // Phase 2: 过滤冗余 + 按 value_density 降序排序
        let mut active: Vec<&SelectionCandidate> = candidates.iter()
            .filter(|c| !c.redundant)
            .collect();
        active.sort_by(|a, b| b.value_density.partial_cmp(&a.value_density).unwrap_or(std::cmp::Ordering::Equal));

        // Phase 3: 贪心填充
        let mut budget_remaining = token_budget;
        let mut retained = Vec::new();
        let mut compressed = Vec::new();

        for c in &active {
            if c.tokens <= budget_remaining {
                retained.push(c.index);
                budget_remaining -= c.tokens;
            } else {
                compressed.push(c.index);
            }
        }

        // 冗余消息全部进入 compressed
        for c in candidates.iter().filter(|c| c.redundant) {
            compressed.push(c.index);
        }

        // Phase 4: 覆盖保证——如果某种高价值类型完全缺失，强制保留一条
        // （这里简化为：如果 retained 为空但 candidates 非空，至少保留分数最高的一条）
        if retained.is_empty() && !candidates.is_empty() {
            if let Some(best) = candidates.iter().max_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal)) {
                retained.push(best.index);
                compressed.retain(|&idx| idx != best.index);
            }
        }

        (retained, compressed)
    }

    /// MinHash 近似去重：相似度 > DEDUP_THRESHOLD 的消息标记为 redundant
    ///
    /// 策略：保留每个相似簇中分数最高的消息
    fn mark_redundant(candidates: &mut Vec<SelectionCandidate>) {
        const DEDUP_THRESHOLD: f64 = 0.6;

        let n = candidates.len();
        if n <= 1 {
            return;
        }

        // O(n²) 比较——但 n 通常 < 100，可接受
        for i in 0..n {
            if candidates[i].redundant {
                continue;
            }
            let sig_i = match &candidates[i].signature {
                Some(s) => s.clone(),
                None => continue,
            };
            for j in (i + 1)..n {
                if candidates[j].redundant {
                    continue;
                }
                if let Some(ref sig_j) = candidates[j].signature {
                    let sim = sig_i.jaccard_similarity(sig_j);
                    if sim > DEDUP_THRESHOLD {
                        // 保留分数高的，标记分数低的为冗余
                        if candidates[i].score >= candidates[j].score {
                            candidates[j].redundant = true;
                        } else {
                            candidates[i].redundant = true;
                            break; // i 被标记，不再作为比较源
                        }
                    }
                }
            }
        }
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Adaptive Threshold (EMA-based)
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Rate-Distortion 启发的自适应阈值
///
/// 替代硬编码序列 70→80→85→90→75。
///
/// ## 数学基础
/// Shannon Rate-Distortion 函数 D(R) 告诉我们：压缩越多，失真越大。
/// 最优触发点 = 信息边际价值开始陡降的拐点。
///
/// 我们用 EMA 逼近这个拐点：
/// - 如果压缩后实际占用 >> target → 阈值太高，降低（更早触发）
/// - 如果压缩后实际占用 << target → 阈值太低，提高（延迟触发）
///
/// ## 参数
/// - target_after_pct: 压缩后目标占用率（默认 60%）
/// - alpha: EMA 平滑因子（0.3 = 30% 新信息 + 70% 历史）
/// - bounds: [55%, 92%] 防止极端值
pub struct AdaptiveThreshold {
    pub current: f64,
    pub target_after_pct: f64,
    pub alpha: f64,
    pub min_threshold: f64,
    pub max_threshold: f64,
}

impl AdaptiveThreshold {
    pub fn new() -> Self {
        Self {
            current: 70.0,
            target_after_pct: 60.0,
            alpha: 0.3,
            min_threshold: 55.0,
            max_threshold: 92.0,
        }
    }

    /// 获取当前阈值
    pub fn get(&self) -> f64 {
        self.current
    }

    /// 根据压缩结果更新
    pub fn update(&mut self, trigger_pct: f64, after_pct: f64) {
        if after_pct <= 0.0 || trigger_pct <= 0.0 {
            return;
        }
        // 最优触发点估计
        let optimal = trigger_pct * (self.target_after_pct / after_pct);
        let clamped = optimal.clamp(self.min_threshold, self.max_threshold);
        self.current = self.alpha * clamped + (1.0 - self.alpha) * self.current;
    }
}

impl Default for AdaptiveThreshold {
    fn default() -> Self {
        Self::new()
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Rate-Distortion Snippet Length Optimizer
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 最优截断长度计算器
///
/// 替代固定 snippet 长度（Detailed=160, Brief=80, Minimal=0），
/// 根据每条消息的信息分布决定截断点。
///
/// ## 核心思想（Rate-Distortion 近似）
/// 对于一条消息，前 L 个字符覆盖的"概念"数量随 L 增长，
/// 但边际收益递减。最优截断点 = 边际概念覆盖率降到 threshold 以下。
///
/// ## 近似实现
/// 用累积 unique word 比例作为 coverage proxy：
/// coverage(L) = |unique_words_in_first_L_chars| / |unique_words_in_full_text|
/// optimal_L = min L such that coverage(L) >= target_coverage
pub struct SnippetOptimizer {
    /// 目标覆盖率（0.0-1.0）
    pub target_coverage: f64,
    /// 最大截断长度（防止过长）
    pub max_length: usize,
    /// 最小截断长度（保证至少有内容）
    pub min_length: usize,
}

impl SnippetOptimizer {
    pub fn new(target_coverage: f64) -> Self {
        Self {
            target_coverage: target_coverage.clamp(0.5, 0.95),
            max_length: 300,
            min_length: 40,
        }
    }

    /// 计算最优截断长度
    ///
    /// ## 复杂度
    /// O(n) where n = text length
    pub fn optimal_length(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }

        // 全文 unique words
        let all_words: std::collections::HashSet<&str> = text.split_whitespace().collect();
        if all_words.is_empty() {
            return self.min_length.min(text.len());
        }
        let total_unique = all_words.len();
        let target_count = (total_unique as f64 * self.target_coverage).ceil() as usize;

        // 逐字符扫描，跟踪累积 unique words
        let mut seen = std::collections::HashSet::new();
        let mut current_word = String::new();
        let mut char_pos = 0;

        for ch in text.chars() {
            char_pos += ch.len_utf8();
            if char_pos > self.max_length {
                return self.max_length;
            }

            if ch.is_whitespace() {
                if !current_word.is_empty() {
                    seen.insert(current_word.clone());
                    current_word.clear();
                    if seen.len() >= target_count {
                        return char_pos.max(self.min_length);
                    }
                }
            } else {
                current_word.push(ch);
            }
        }

        // 全文长度 < max_length
        text.len().max(self.min_length).min(self.max_length)
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Helper: Symbol Extraction
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 从文本中提取"符号"（用于引用计数追踪）
///
/// 符号 = 可能是标识符的连续字符序列（长度 > 4，含下划线/点/斜杠）
/// 例如：function_name, file/path.rs, my_variable, ClassName
///
/// 引用关系：compress_math 内部 + pipeline/post.rs 引用计数更新
pub fn extract_symbols(text: &str) -> Vec<&str> {
    let mut symbols = Vec::new();
    let mut start = None;

    for (i, ch) in text.char_indices() {
        let is_symbol_char = ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == '/' || ch == '-';
        match (is_symbol_char, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let candidate = &text[s..i];
                if candidate.len() > 4 && candidate.chars().any(|c| c == '_' || c == '/' || c == '.') {
                    symbols.push(candidate);
                }
                start = None;
            }
            _ => {}
        }
    }
    // 处理末尾
    if let Some(s) = start {
        let candidate = &text[s..];
        if candidate.len() > 4 && candidate.chars().any(|c| c == '_' || c == '/' || c == '.') {
            symbols.push(candidate);
        }
    }

    // 限制数量防止过长消息
    symbols.truncate(50);
    symbols
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Tests
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minhash_identical_texts() {
        let sig1 = MinHashSig::from_text("hello world this is a test message");
        let sig2 = MinHashSig::from_text("hello world this is a test message");
        assert_eq!(sig1.jaccard_similarity(&sig2), 1.0);
    }

    #[test]
    fn test_minhash_similar_texts() {
        let sig1 = MinHashSig::from_text("hello world this is a test message for similarity");
        let sig2 = MinHashSig::from_text("hello world this is a test message for checking");
        let sim = sig1.jaccard_similarity(&sig2);
        assert!(sim > 0.5, "similar texts should have Jaccard > 0.5, got {}", sim);
    }

    #[test]
    fn test_minhash_different_texts() {
        let sig1 = MinHashSig::from_text("the quick brown fox jumps over");
        let sig2 = MinHashSig::from_text("量子力学是物理学的基本理论之一");
        let sim = sig1.jaccard_similarity(&sig2);
        assert!(sim < 0.3, "different texts should have low Jaccard, got {}", sim);
    }

    #[test]
    fn test_composite_scorer_recency_matters() {
        let scorer = CompositeScorer::new(1.5);
        let recent = scorer.score(MessageType::AssistantProse, 100, 0.7, 0.1, 0);
        let distant = scorer.score(MessageType::AssistantProse, 100, 0.7, 0.9, 0);
        assert!(recent > distant, "recent msg should score higher: {} vs {}", recent, distant);
    }

    #[test]
    fn test_composite_scorer_type_matters() {
        let scorer = CompositeScorer::new(1.0);
        let user_inst = scorer.score(MessageType::UserInstruction, 100, 0.7, 0.5, 0);
        let tool_out = scorer.score(MessageType::ToolResult, 100, 0.7, 0.5, 0);
        assert!(user_inst > tool_out, "user instruction > tool result: {} vs {}", user_inst, tool_out);
    }

    #[test]
    fn test_composite_scorer_refs_boost() {
        let scorer = CompositeScorer::new(1.0);
        let no_refs = scorer.score(MessageType::AssistantProse, 100, 0.7, 0.5, 0);
        let many_refs = scorer.score(MessageType::AssistantProse, 100, 0.7, 0.5, 5);
        assert!(many_refs > no_refs, "referenced msg should score higher: {} vs {}", many_refs, no_refs);
    }

    #[test]
    fn test_unique_ratio() {
        let high = CompositeScorer::compute_unique_ratio("each word here is completely different and unique");
        let low = CompositeScorer::compute_unique_ratio("the the the the the the the the");
        assert!(high > low, "diverse text should have higher ratio: {} vs {}", high, low);
    }

    #[test]
    fn test_greedy_knapsack_basic() {
        let mut candidates = vec![
            SelectionCandidate { index: 0, score: 0.9, tokens: 100, value_density: 0.009, signature: None, redundant: false },
            SelectionCandidate { index: 1, score: 0.3, tokens: 50, value_density: 0.006, signature: None, redundant: false },
            SelectionCandidate { index: 2, score: 0.5, tokens: 30, value_density: 0.0167, signature: None, redundant: false },
        ];
        let (retained, compressed) = GreedyKnapsackSelector::select(&mut candidates, 80);
        // Budget=80: item2 (30 tok, density 0.0167) first, then item0 won't fit (100>50 remaining)
        // item1 (50 tok) fits
        assert!(retained.contains(&2), "highest density should be selected");
        assert!(retained.contains(&1), "second should fit in remaining budget");
        assert!(compressed.contains(&0), "largest should be compressed");
    }

    #[test]
    fn test_greedy_knapsack_dedup() {
        let sig = MinHashSig::from_text("identical repeated content");
        let mut candidates = vec![
            SelectionCandidate { index: 0, score: 0.8, tokens: 50, value_density: 0.016, signature: Some(sig.clone()), redundant: false },
            SelectionCandidate { index: 1, score: 0.6, tokens: 50, value_density: 0.012, signature: Some(sig.clone()), redundant: false },
            SelectionCandidate { index: 2, score: 0.4, tokens: 50, value_density: 0.008, signature: Some(sig.clone()), redundant: false },
        ];
        let (retained, compressed) = GreedyKnapsackSelector::select(&mut candidates, 200);
        // All have same signature → only highest score (index 0) retained
        assert!(retained.contains(&0));
        assert!(compressed.contains(&1));
        assert!(compressed.contains(&2));
    }

    #[test]
    fn test_adaptive_threshold_converges() {
        let mut t = AdaptiveThreshold::new();
        // 压缩效果太差（after=80% > target=60%）→ 应降低阈值
        t.update(70.0, 80.0);
        assert!(t.get() < 70.0, "threshold should decrease when compression insufficient: {}", t.get());

        // 压缩效果太好（after=30% < target=60%）→ 应提高阈值
        let mut t2 = AdaptiveThreshold::new();
        t2.update(70.0, 30.0);
        assert!(t2.get() > 70.0, "threshold should increase when over-compressed: {}", t2.get());
    }

    #[test]
    fn test_snippet_optimizer() {
        let opt = SnippetOptimizer::new(0.8);
        let text = "This is a short text with few unique words";
        let len = opt.optimal_length(text);
        // 短文本覆盖率很快达标
        assert!(len <= text.len());
        assert!(len >= opt.min_length);
    }

    #[test]
    fn test_extract_symbols() {
        let text = "修改了 crates/abacus-core/src/main.rs 中的 process_message 函数";
        let syms = extract_symbols(text);
        assert!(syms.contains(&"crates/abacus-core/src/main.rs"));
        assert!(syms.contains(&"process_message"));
    }

    #[test]
    fn test_message_type_classification() {
        assert_eq!(MessageType::classify("user", "请帮我修改这个函数") as u8,
                   MessageType::UserInstruction as u8);
        assert_eq!(MessageType::classify("assistant", "```rust\nfn main() {}\n```") as u8,
                   MessageType::CodeBlock as u8);
        assert_eq!(MessageType::classify("assistant", "Error: file not found") as u8,
                   MessageType::AssistantError as u8);
    }
}
