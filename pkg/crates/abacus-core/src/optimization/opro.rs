//! OPRO 风格 Prompt 优化器（P2-B1）
//!
//! ## 研究来源
//! Yang et al. 2023 "Large Language Models as Optimizers" (Google DeepMind)
//!
//! ## 核心思路
//! 用 LLM 作为黑盒优化器来优化 Skill/System prompt：
//! 1. 收集历史 (instruction, score) 对（来自 DeductionEngine MetricStore）
//! 2. 构建 meta-prompt：历史记录 + 优化指导
//! 3. LLM 生成 N 个候选 prompt 变体
//! 4. 评估候选（小样本测试）
//! 5. 最优候选写入"待评审"状态，等待人工确认后上线
//!
//! ## 关键设计
//! - 候选不自动上线（写入 pending_candidates 状态）
//! - 评分来源：直接复用 EffectivenessTracker.composite_score
//! - SHA256 去重：相同 prompt 内容不重复生成
//! - 接入 CronScheduler：每 N 小时后台自动运行
//!
//! ## 引用关系
//! - 调用方：AutoEngine cron 触发 / 用户命令 `/optimize-prompt`
//! - 依赖：Arc<dyn LlmProvider>, Arc<DeductionEngine>
//! - 生命周期：每次优化轮次独立，候选持久化到 optimization.db

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Sha256, Digest};

/// 一条历史 (instruction, score) 记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionRecord {
    /// 指令/Prompt 文本
    pub instruction: String,
    /// 综合评分（来自 EffectivenessTracker.composite_score，范围 0.0-1.0）
    pub score: f64,
    /// 来源标识（如 skill_id 或 "system_prompt"）
    pub source: String,
    /// 记录时间戳（Unix 毫秒）
    pub timestamp_ms: i64,
}

/// 优化候选（等待评审）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizationCandidate {
    /// 候选 Prompt 文本
    pub instruction: String,
    /// SHA256 前 16 字节 hex（去重用）
    pub hash: String,
    /// 来源标识（优化的目标是哪个 Skill / prompt）
    pub source: String,
    /// 生成时间戳
    pub generated_at: i64,
    /// 状态："pending_review" | "approved" | "rejected"
    pub status: String,
    /// 候选得分预测（基于 meta-prompt 推断，非真实评测）
    pub predicted_score: Option<f64>,
}

/// OPRO 优化配置
#[derive(Clone)]
pub struct OproConfig {
    /// 每轮生成的候选数（默认 8）
    pub candidates_per_round: usize,
    /// meta-prompt 中历史记录的最大条数（默认 20，按 score 降序取 top）
    pub max_history_in_prompt: usize,
    /// 候选去重：score 差异 < threshold 的近似重复视为相同
    pub dedup_score_threshold: f64,
    /// 优化 prompt 语言
    pub lang: &'static str,
}

impl Default for OproConfig {
    fn default() -> Self {
        Self {
            candidates_per_round: 8,
            max_history_in_prompt: 20,
            dedup_score_threshold: 0.02,
            lang: "zh",
        }
    }
}

// ─── Meta-prompt 构建 ────────────────────────────────────────────────────────

/// 构建 meta-prompt（P2-B1 核心）
///
/// ## 格式（来自 OPRO 论文）
/// ```
/// 以下是历史指令及其评分，分数越高越好（0-1分）：
///
/// 指令: "..."   分数: 0.85
/// 指令: "..."   分数: 0.72
/// ...
///
/// 请生成 N 个新的、更好的指令变体，尝试提升分数。
/// 要求：
/// - 每个指令独立成行，用 --- 分隔
/// - 保持原有目标不变，只改写表达方式
/// - 尝试更具体、更清晰的描述
/// ```
pub fn build_meta_prompt(
    history: &[InstructionRecord],
    source: &str,
    n_candidates: usize,
    config: &OproConfig,
) -> String {
    // 按 score 降序取 top-N 历史
    let mut sorted = history.iter().collect::<Vec<_>>();
    sorted.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let top = &sorted[..sorted.len().min(config.max_history_in_prompt)];

    if config.lang == "zh" {
        let history_text = top.iter()
            .map(|r| format!("指令: \"{}\"   分数: {:.2}", r.instruction, r.score))
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            "以下是针对「{source}」的历史指令及其效果评分（分数越高越好，满分 1.0）：\n\n\
             {history_text}\n\n\
             请基于以上历史，生成 {n_candidates} 个新的、可能效果更好的指令变体。\n\
             要求：\n\
             - 每个指令独立成行，用 --- 分隔\n\
             - 保持原有任务目标不变，只改写表达方式\n\
             - 尝试更具体、更清晰、更行动导向的描述\n\
             - 避免与现有高分指令完全重复\n\n\
             请直接输出 {n_candidates} 个候选指令："
        )
    } else {
        let history_text = top.iter()
            .map(|r| format!("Instruction: \"{}\"   Score: {:.2}", r.instruction, r.score))
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            "Below are historical instructions for \"{source}\" with their effectiveness scores \
             (higher is better, max 1.0):\n\n\
             {history_text}\n\n\
             Please generate {n_candidates} new instruction variants that may achieve higher scores.\n\
             Requirements:\n\
             - Each instruction on its own line, separated by ---\n\
             - Keep the same task objective, only rewrite the expression\n\
             - Try to be more specific, clearer, and more action-oriented\n\
             - Avoid exact duplicates of high-scoring existing instructions\n\n\
             Output {n_candidates} candidate instructions:"
        )
    }
}

// ─── 候选解析与去重 ─────────────────────────────────────────────────────────

/// 从 LLM 输出解析候选列表
pub fn parse_candidates(output: &str) -> Vec<String> {
    // 尝试 --- 分隔
    let by_sep: Vec<String> = output.split("---")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() > 5) // 过滤过短的（可能是空格/标点）
        .collect();

    if by_sep.len() >= 2 {
        return by_sep;
    }

    // 数字列表
    output.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let t = l.trim();
            // 去掉 "1. " "1) " "- " 前缀
            t.trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start_matches(['.', ')', '-', ' '].as_ref())
                .trim()
                .to_string()
        })
        .filter(|s| s.len() > 5)
        .collect()
}

/// SHA256 前 16 字节 hex（用于去重，16 hex chars = 64 bit，碰撞概率极低）
pub fn content_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let result = hasher.finalize();
    // 用标准库替代 hex crate：每字节格式化为 2 位小写 hex
    result[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// 去重：移除 hash 已见过的候选
pub fn dedup_candidates(
    candidates: Vec<String>,
    existing_hashes: &HashSet<String>,
) -> Vec<String> {
    let mut seen = existing_hashes.clone();
    candidates.into_iter()
        .filter(|c| {
            let h = content_hash(c);
            seen.insert(h)
        })
        .collect()
}

// ─── OptimizationRound ──────────────────────────────────────────────────────

/// 一轮优化的输入输出
#[derive(Debug, Clone)]
pub struct OptimizationRound {
    /// 目标来源（skill_id 或 "system_prompt"）
    pub source: String,
    /// meta-prompt（已构建，供记录）
    pub meta_prompt: String,
    /// LLM 原始输出
    pub raw_output: String,
    /// 解析并去重后的候选列表
    pub candidates: Vec<OptimizationCandidate>,
}

/// 执行一轮 OPRO 优化（纯数据处理，不直接调用 LLM）
///
/// ## 使用方式
/// 1. 调用方先获取 LLM 输出（`provider.complete(build_meta_prompt(...))`）
/// 2. 将 LLM 输出传入本函数进行解析、去重、构建候选
/// 3. 候选写入 OptimizationStore 等待评审
///
/// ## 设计意图
/// 分离 LLM 调用与候选处理，便于测试和错误隔离
pub fn process_round_output(
    source: &str,
    meta_prompt: &str,
    llm_output: &str,
    existing_hashes: &HashSet<String>,
    config: &OproConfig,
) -> OptimizationRound {
    let raw_candidates = parse_candidates(llm_output);
    let deduped = dedup_candidates(raw_candidates, existing_hashes);
    let now_ms = chrono::Utc::now().timestamp_millis();

    let candidates: Vec<OptimizationCandidate> = deduped.into_iter()
        .take(config.candidates_per_round)
        .map(|instr| OptimizationCandidate {
            hash: content_hash(&instr),
            instruction: instr,
            source: source.to_string(),
            generated_at: now_ms,
            status: "pending_review".to_string(),
            predicted_score: None,
        })
        .collect();

    OptimizationRound {
        source: source.to_string(),
        meta_prompt: meta_prompt.to_string(),
        raw_output: llm_output.to_string(),
        candidates,
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_history() -> Vec<InstructionRecord> {
        vec![
            InstructionRecord {
                instruction: "分析代码质量".into(),
                score: 0.72,
                source: "code_review".into(),
                timestamp_ms: 0,
            },
            InstructionRecord {
                instruction: "逐行检查代码逻辑错误，给出改进建议".into(),
                score: 0.85,
                source: "code_review".into(),
                timestamp_ms: 1,
            },
        ]
    }

    #[test]
    fn meta_prompt_contains_history() {
        let history = sample_history();
        let config = OproConfig::default();
        let prompt = build_meta_prompt(&history, "code_review", 3, &config);
        assert!(prompt.contains("0.85"));
        assert!(prompt.contains("0.72"));
        assert!(prompt.contains("code_review"));
    }

    #[test]
    fn parse_candidates_separator() {
        let output = "候选A：详细分析\n---\n候选B：快速检查\n---\n候选C：全面审查";
        let candidates = parse_candidates(output);
        assert!(candidates.len() >= 3);
    }

    #[test]
    fn parse_candidates_numbered() {
        let output = "1. 详细分析代码逻辑\n2. 快速检查性能问题\n3. 全面审查安全漏洞";
        let candidates = parse_candidates(output);
        assert_eq!(candidates.len(), 3);
    }

    #[test]
    fn content_hash_deterministic() {
        let h1 = content_hash("test prompt");
        let h2 = content_hash("test prompt");
        assert_eq!(h1, h2);
        let h3 = content_hash("different prompt");
        assert_ne!(h1, h3);
    }

    #[test]
    fn dedup_filters_existing() {
        let candidates = vec!["A".into(), "B".into(), "A".into()];
        let mut existing = HashSet::new();
        existing.insert(content_hash("A"));
        let result = dedup_candidates(candidates, &existing);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "B");
    }

    #[test]
    fn process_round_builds_candidates() {
        let config = OproConfig::default();
        let output = "候选一：详细分析\n---\n候选二：快速检查";
        let round = process_round_output("test", "meta", output, &HashSet::new(), &config);
        assert!(!round.candidates.is_empty());
        assert_eq!(round.candidates[0].status, "pending_review");
    }
}
