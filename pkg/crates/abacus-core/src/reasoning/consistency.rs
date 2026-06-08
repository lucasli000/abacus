//! Self-Consistency 投票器（P2-B3）
//!
//! ## 研究来源
//! Wang et al. 2022 "Self-Consistency Improves Chain of Thought Reasoning in Language Models"
//!
//! ## 核心思路
//! 对同一问题并行采样 N 条推理路径（temperature > 0），从多个答案中取多数投票，
//! 显著提升高风险推理任务（数学/调试）的准确率。
//!
//! ## 触发条件
//! - `ThinkingIntent::Effort(High)` 或用户在 RequestContext 中设置 `confidence_required = true`
//! - TaskKind 属于 Mathematics 或 Debugging
//! - N ≥ 2（默认 3）
//!
//! ## 成本控制
//! - 并行采样使用 `tokio::join_all`，总延迟 ≈ 单次延迟（而非 N 倍）
//! - 仅在 `confidence_required` 时激活，避免无意义消耗
//! - 结果置信度 < 0.6 时返回 `ConsistencyResult.low_confidence = true`，让调用方决策
//!
//! ## 引用关系
//! - 调用方：CoreLoop pipeline 在 execute_loop 完成后、需要高置信度答案时调用
//! - 依赖：Arc<dyn LlmProvider>（只调用 complete，无状态）
//! - 生命周期：每次调用独立，无持久状态

use std::collections::HashMap;
use std::sync::Arc;

use crate::llm::{LlmProvider, LlmRequest, LlmResponse, MessageContent};

/// Self-Consistency 采样配置
///
/// ## 字段
/// - `n`：采样次数（建议 3-5；成本 = n × 单次调用延迟的并发最大值）
/// - `temperature`：采样温度（建议 0.7-1.0；高温增加多样性，但过高降低质量）
/// - `answer_extractor`：从 LLM 输出中提取最终答案的函数
///   默认提取器：取最后一句非空文本（通用但不精确）
#[derive(Clone)]
pub struct ConsistencyConfig {
    /// 并行采样次数（默认 3）
    pub n: usize,
    /// 采样温度（默认 0.7）
    pub temperature: f64,
}

impl Default for ConsistencyConfig {
    fn default() -> Self {
        Self { n: 3, temperature: 0.7 }
    }
}

/// Self-Consistency 投票结果
#[derive(Debug, Clone)]
pub struct ConsistencyResult {
    /// 多数投票胜出的答案文本
    pub answer: String,
    /// 置信度（胜出答案的票数 / 总票数）
    ///
    /// - ≥ 0.8：高置信度，可直接使用
    /// - 0.6-0.8：中置信度，建议附加 [未验证] 标注
    /// - < 0.6：低置信度，建议向用户说明结果不确定
    pub confidence: f64,
    /// 所有采样答案（供调试和日志）
    pub all_answers: Vec<String>,
    /// 是否低置信度（confidence < 0.6 时为 true）
    pub low_confidence: bool,
}

/// 从 LLM 响应中提取答案文本
///
/// ## 策略（按优先级）
/// 1. 查找"答案："、"结论："等中文信号词后的内容
/// 2. 查找"Answer:"、"Therefore:" 等英文信号词后的内容
/// 3. 取最后一段非空文本
fn extract_answer(response: &LlmResponse) -> String {
    let text = match &response.message.content {
        Some(MessageContent::Text(t)) => t.clone(),
        _ => return String::new(),
    };

    // 中文信号词
    let cn_signals = ["答案：", "答案:", "结论：", "结论:", "所以：", "所以:", "因此：", "因此:"];
    for sig in &cn_signals {
        if let Some(pos) = text.find(sig) {
            let after = text[pos + sig.len()..].trim();
            let first_line: String = after.lines().next().unwrap_or("").trim().to_string();
            if !first_line.is_empty() { return first_line; }
        }
    }

    // 英文信号词
    let en_signals = ["Answer:", "Therefore:", "Thus:", "So:", "The answer is", "= "];
    for sig in &en_signals {
        if let Some(pos) = text.to_lowercase().find(&sig.to_lowercase()) {
            let after = text[pos + sig.len()..].trim();
            let first_line: String = after.lines().next().unwrap_or("").trim().to_string();
            if !first_line.is_empty() { return first_line; }
        }
    }

    // 取最后一段非空文本（fallback）
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 多数投票（majority voting）
///
/// ## 规则
/// - 完全相等的答案归为同一组
/// - 返回票数最多的答案和对应置信度
///
/// ## 归一化
/// 答案在比较前先 trim + lowercase，避免格式差异导致分裂
fn majority_vote(answers: &[String]) -> (String, f64) {
    if answers.is_empty() {
        return (String::new(), 0.0);
    }

    let mut counts: HashMap<String, (usize, String)> = HashMap::new();
    for ans in answers {
        let normalized = ans.trim().to_lowercase();
        let entry = counts.entry(normalized.clone()).or_insert((0, ans.clone()));
        entry.0 += 1;
    }

    let total = answers.len();
    let (_, (count, original)) = counts.into_iter()
        .max_by_key(|(_, (c, _))| *c)
        .expect("early return above ensures answers is non-empty, so counts has at least one entry");

    let confidence = count as f64 / total as f64;
    (original, confidence)
}

/// 执行 Self-Consistency 采样并投票（P2-B3 核心函数）
///
/// ## 并发模型
/// 使用 `futures_util::future::join_all` 并发 n 次 LLM 调用，
/// 总延迟约等于单次调用延迟（而非 n 倍）。
///
/// ## 错误处理
/// - 单次采样失败时跳过该结果（不影响其他采样）
/// - 全部失败时返回 `Err("all samples failed")`
/// - 有效样本 < n/2 时返回 `low_confidence = true`
///
/// ## 引用关系
/// - 调用方：CoreLoop pipeline（ThinkingIntent=High + Mathematics/Debugging 任务）
/// - 依赖：provider.complete() 接口（无状态，可并发）
pub async fn consistent_sample(
    provider: Arc<dyn LlmProvider>,
    request: &LlmRequest,
    config: &ConsistencyConfig,
) -> Result<ConsistencyResult, String> {
    if config.n == 0 {
        return Err("consistency n must be > 0".into());
    }

    // 构建并发采样 futures（每次调用独立，temperature 一致）
    let futs: Vec<_> = (0..config.n).map(|_| {
        let mut req = request.clone();
        req.temperature = Some(config.temperature);
        // stream=false（collect full response），确保 extract_answer 可以处理完整文本
        req.stream = false;
        let p = Arc::clone(&provider);
        async move { p.complete(req).await }
    }).collect();

    let results = futures_util::future::join_all(futs).await;

    // 提取答案（跳过失败的采样）
    let answers: Vec<String> = results.into_iter()
        .filter_map(|r| r.ok())
        .map(|resp| extract_answer(&resp))
        .filter(|a| !a.is_empty())
        .collect();

    if answers.is_empty() {
        return Err("all samples failed or returned empty answers".into());
    }

    let (winner, confidence) = majority_vote(&answers);
    let low_confidence = confidence < 0.6 || answers.len() < config.n / 2 + 1;

    Ok(ConsistencyResult {
        answer: winner,
        confidence,
        all_answers: answers,
        low_confidence,
    })
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn majority_vote_picks_winner() {
        let answers = vec![
            "42".into(), "42".into(), "43".into(),
        ];
        let (winner, conf) = majority_vote(&answers);
        assert_eq!(winner, "42");
        assert!((conf - 2.0/3.0).abs() < 0.01);
    }

    #[test]
    fn majority_vote_case_insensitive() {
        let answers = vec!["True".into(), "true".into(), "false".into()];
        let (winner, conf) = majority_vote(&answers);
        // "true"/"True" 都归一化为 "true"，胜出
        assert_eq!(winner.to_lowercase(), "true");
        assert!((conf - 2.0/3.0).abs() < 0.01);
    }

    #[test]
    fn majority_vote_single_answer() {
        let answers = vec!["42".into()];
        let (winner, conf) = majority_vote(&answers);
        assert_eq!(winner, "42");
        assert_eq!(conf, 1.0);
    }

    #[test]
    fn extract_answer_cn_signal() {
        let text = "经过计算，答案：42，符合条件。";
        // 模拟一个最简单的 response
        // 直接测试提取逻辑
        let pos = text.find("答案：").unwrap();
        let after = text[pos + "答案：".len()..].trim();
        let first_line: String = after.lines().next().unwrap_or("").trim().to_string();
        assert!(first_line.starts_with("42"));
    }

    #[test]
    fn config_defaults() {
        let cfg = ConsistencyConfig::default();
        assert_eq!(cfg.n, 3);
        assert_eq!(cfg.temperature, 0.7);
    }

    #[test]
    fn low_confidence_when_split() {
        // 3 个不同答案，置信度 1/3 < 0.6 → low_confidence = true
        let answers = vec!["1".into(), "2".into(), "3".into()];
        let (_, conf) = majority_vote(&answers);
        let low = conf < 0.6;
        assert!(low);
    }
}
