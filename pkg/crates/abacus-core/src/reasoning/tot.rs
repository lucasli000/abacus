//! Tree of Thoughts（ToT）推理子系统（P2-B2）
//!
//! ## 研究来源
//! Yao et al. 2023 "Tree of Thoughts: Deliberate Problem Solving with Large Language Models"
//!
//! ## 核心思路
//! 将线性推理链变为树形搜索：每步生成多个候选思维节点，LLM 自评
//! sure/maybe/impossible，配合 BFS/DFS/Beam Search 剪枝。
//!
//! ## 触发条件
//! - TaskKind::Architecture（规划/设计类任务）
//! - ThinkingIntent::Effort(High)
//! - 通过 `workflow_engine.rs` 检测到"规划"类任务时切入 ToT 路径
//!
//! ## 与 Self-Consistency 的区别
//! - Self-Consistency：并行采样同一问题 → 投票取最一致答案
//! - ToT：分步展开 → 每步评估 → 剪枝 → 深度优先/广度优先探索
//!
//! ## 引用关系
//! - 调用方：workflow_engine.rs / CoreLoop（Architecture 任务 + thinking=High）
//! - 依赖：Arc<dyn LlmProvider>（complete 接口）
//! - 生命周期：每次规划任务独立，无持久状态

use std::sync::Arc;

use crate::llm::{LlmProvider, LlmRequest, Message, MessageContent, MessageRole};

/// ToT 搜索策略
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SearchStrategy {
    /// 广度优先搜索（BFS）：逐层展开所有候选，适合短树
    Bfs,
    /// 深度优先搜索（DFS）：沿最优路径深入，发现不可行则回溯
    Dfs,
    /// Beam Search：每层保留 top-K 候选，平衡质量与效率（默认）
    Beam { k: usize },
}

impl Default for SearchStrategy {
    fn default() -> Self { Self::Beam { k: 3 } }
}

/// 节点状态（LLM 自评结果）
#[derive(Debug, Clone, PartialEq)]
pub enum NodeStatus {
    /// 确定可行（LLM 评估为 "sure"）—— 保留且优先
    Sure,
    /// 可能可行（LLM 评估为 "maybe"）—— 保留继续探索
    Maybe,
    /// 不可行（LLM 评估为 "impossible"）—— 剪枝
    Impossible,
    /// 未评估（初始状态）
    Pending,
}

/// ToT 思维节点
#[derive(Debug, Clone)]
pub struct ThoughtNode {
    /// 本步思维内容
    pub content: String,
    /// LLM 自评状态
    pub status: NodeStatus,
    /// 从根节点到本节点的完整路径（用于构建最终计划）
    pub path: Vec<String>,
    /// 评估理由（来自 LLM 的解释）
    pub reason: String,
}

/// ToT 配置
#[derive(Clone)]
pub struct ToTConfig {
    /// 最大搜索深度（默认 3）
    pub max_depth: u32,
    /// 每步生成的候选思维数（默认 5）
    pub branch_factor: u32,
    /// 搜索策略（默认 Beam(3)）
    pub strategy: SearchStrategy,
    /// 评估 prompt 语言（"zh" 或 "en"）
    pub lang: &'static str,
}

impl Default for ToTConfig {
    fn default() -> Self {
        Self {
            max_depth: 3,
            branch_factor: 5,
            strategy: SearchStrategy::Beam { k: 3 },
            lang: "zh",
        }
    }
}

/// ToT 搜索结果
#[derive(Debug, Clone)]
pub struct ToTResult {
    /// 最终规划路径（每步一条思维）
    pub plan: Vec<String>,
    /// 搜索过程中探索的总节点数（用于性能分析）
    pub nodes_explored: usize,
    /// 是否在最大深度前找到 Sure 路径
    pub found_sure_path: bool,
}

// ─── Prompts ────────────────────────────────────────────────────────────────

fn thought_generation_prompt(task: &str, path_so_far: &[String], branch_factor: u32, lang: &str) -> String {
    if lang == "zh" {
        let path_text = if path_so_far.is_empty() {
            "（第一步）".to_string()
        } else {
            format!("已有步骤:\n{}", path_so_far.iter().enumerate()
                .map(|(i, s)| format!("{}. {}", i + 1, s))
                .collect::<Vec<_>>().join("\n"))
        };
        format!(
            "任务：{task}\n\n{path_text}\n\n\
             请生成 {branch_factor} 个不同的下一步思路，每个思路单独一行，用 --- 分隔。\
             每个思路应该具体、可执行、不同角度切入。"
        )
    } else {
        let path_text = if path_so_far.is_empty() {
            "(first step)".to_string()
        } else {
            format!("Steps so far:\n{}", path_so_far.iter().enumerate()
                .map(|(i, s)| format!("{}. {}", i + 1, s))
                .collect::<Vec<_>>().join("\n"))
        };
        format!(
            "Task: {task}\n\n{path_text}\n\n\
             Generate {branch_factor} different next steps, each on a new line separated by ---. \
             Each step should be specific, actionable, and from a different angle."
        )
    }
}

fn evaluation_prompt(task: &str, path: &[String], lang: &str) -> String {
    let path_text = path.iter().enumerate()
        .map(|(i, s)| format!("{}. {}", i + 1, s))
        .collect::<Vec<_>>().join("\n");

    if lang == "zh" {
        format!(
            "任务：{task}\n\n规划路径：\n{path_text}\n\n\
             评估此规划路径是否可行。回答格式：\n\
             状态: sure/maybe/impossible\n\
             理由: <一句话解释>\n\n\
             - sure：路径明确、完整、可执行\n\
             - maybe：路径合理但有不确定因素\n\
             - impossible：路径有根本性缺陷、无法完成任务"
        )
    } else {
        format!(
            "Task: {task}\n\nPlanning path:\n{path_text}\n\n\
             Evaluate if this planning path is viable. Format:\n\
             Status: sure/maybe/impossible\n\
             Reason: <one sentence>\n\n\
             - sure: clear, complete, actionable path\n\
             - maybe: reasonable but has uncertainties\n\
             - impossible: fundamental flaw, cannot complete the task"
        )
    }
}

// ─── Core Functions ─────────────────────────────────────────────────────────

/// 解析 LLM 输出的多个候选思维
fn parse_thoughts(output: &str, expected: u32) -> Vec<String> {
    // 先尝试 --- 分隔
    let by_separator: Vec<String> = output.split("---")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if by_separator.len() >= 2 {
        return by_separator.into_iter().take(expected as usize).collect();
    }

    // 尝试数字列表：1. ... 2. ... 或 1) ... 2) ...
    let lines: Vec<String> = output.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let trimmed = l.trim();
            // 去掉 "1. " "1) " "- " 等前缀
            let stripped = trimmed.trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start_matches(['.', ')', '-', ' '].as_ref());
            stripped.trim().to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();

    if !lines.is_empty() {
        return lines.into_iter().take(expected as usize).collect();
    }

    // fallback：整个输出作为单个思维
    if !output.trim().is_empty() {
        vec![output.trim().to_string()]
    } else {
        vec![]
    }
}

/// 解析 LLM 的评估输出 → NodeStatus + reason
fn parse_evaluation(output: &str) -> (NodeStatus, String) {
    let lower = output.to_lowercase();

    let status = if lower.contains("sure") && !lower.contains("not sure") {
        NodeStatus::Sure
    } else if lower.contains("impossible") || lower.contains("不可行") || lower.contains("无法") {
        NodeStatus::Impossible
    } else {
        NodeStatus::Maybe
    };

    // 提取理由
    let reason = output.lines()
        .find(|l| {
            let ll = l.to_lowercase();
            ll.contains("reason") || ll.contains("理由") || ll.contains("因为")
        })
        .map(|l| {
            l.splitn(2, ':')
                .nth(1).unwrap_or(l)
                .trim().to_string()
        })
        .unwrap_or_else(|| output.lines().last().unwrap_or("").trim().to_string());

    (status, reason)
}

/// 调用 LLM 生成候选思维
async fn generate_thoughts(
    provider: &dyn LlmProvider,
    base_request: &LlmRequest,
    task: &str,
    path: &[String],
    config: &ToTConfig,
) -> Vec<String> {
    let prompt = thought_generation_prompt(task, path, config.branch_factor, config.lang);
    let mut req = base_request.clone();
    req.messages = vec![Message {
        role: MessageRole::User,
        content: Some(MessageContent::Text(prompt)),
        name: None, tool_calls: None, tool_call_id: None,
        reasoning_content: None, prefix: false,
    }];
    req.stream = false;
    req.temperature = Some(0.8); // 高温增加多样性

    match provider.complete(req).await {
        Ok(resp) => {
            let text = match &resp.message.content {
                Some(MessageContent::Text(t)) => t.clone(),
                _ => return vec![],
            };
            parse_thoughts(&text, config.branch_factor)
        }
        Err(_) => vec![],
    }
}

/// 调用 LLM 评估路径可行性
async fn evaluate_path(
    provider: &dyn LlmProvider,
    base_request: &LlmRequest,
    task: &str,
    path: &[String],
    config: &ToTConfig,
) -> (NodeStatus, String) {
    let prompt = evaluation_prompt(task, path, config.lang);
    let mut req = base_request.clone();
    req.messages = vec![Message {
        role: MessageRole::User,
        content: Some(MessageContent::Text(prompt)),
        name: None, tool_calls: None, tool_call_id: None,
        reasoning_content: None, prefix: false,
    }];
    req.stream = false;
    req.temperature = Some(0.0); // 确定性评估

    match provider.complete(req).await {
        Ok(resp) => {
            let text = match &resp.message.content {
                Some(MessageContent::Text(t)) => t.clone(),
                _ => return (NodeStatus::Maybe, "evaluation failed".into()),
            };
            parse_evaluation(&text)
        }
        Err(_) => (NodeStatus::Maybe, "provider error, defaulting to maybe".into()),
    }
}

/// ToT 规划搜索（P2-B2 核心函数）
///
/// ## 算法
/// 1. 生成初始候选思维（branch_factor 个）
/// 2. 并发评估所有候选
/// 3. 按策略剪枝（Impossible 丢弃，Sure/Maybe 保留）
/// 4. 递归到下一深度，直到达到 max_depth 或全部确定
///
/// ## 并发安全
/// 每层的 generate/evaluate 调用独立，可并发执行。
///
/// ## 引用关系
/// - 调用方：workflow_engine.rs 在 Architecture 任务时调用
/// - 生命周期：每次任务独立，无持久状态
pub async fn tot_plan(
    provider: Arc<dyn LlmProvider>,
    base_request: &LlmRequest,
    task: &str,
    config: &ToTConfig,
) -> ToTResult {
    let mut nodes_explored = 0usize;
    let mut frontier: Vec<Vec<String>> = vec![vec![]]; // 每个元素是一条路径

    for _depth in 0..config.max_depth {
        let mut next_frontier: Vec<(Vec<String>, NodeStatus)> = Vec::new();

        for path in &frontier {
            // 生成候选思维
            let thoughts = generate_thoughts(&*provider, base_request, task, path, config).await;
            nodes_explored += thoughts.len();

            // 并发评估所有候选
            let eval_futs: Vec<_> = thoughts.iter().map(|thought| {
                let mut new_path = path.clone();
                new_path.push(thought.clone());
                let p = Arc::clone(&provider);
                let req = base_request.clone();
                let t = task.to_string();
                let cfg = config.clone();
                async move {
                    let (status, reason) = evaluate_path(&*p, &req, &t, &new_path, &cfg).await;
                    (new_path, status, reason)
                }
            }).collect();

            let evaluated = futures_util::future::join_all(eval_futs).await;

            for (path, status, _reason) in evaluated {
                if status != NodeStatus::Impossible {
                    next_frontier.push((path, status));
                }
            }
        }

        if next_frontier.is_empty() {
            break;
        }

        // 检查是否有 Sure 路径
        if let Some((sure_path, _)) = next_frontier.iter().find(|(_, s)| *s == NodeStatus::Sure) {
            return ToTResult {
                plan: sure_path.clone(),
                nodes_explored,
                found_sure_path: true,
            };
        }

        // 按搜索策略剪枝
        frontier = match config.strategy {
            SearchStrategy::Bfs => next_frontier.into_iter().map(|(p, _)| p).collect(),
            SearchStrategy::Dfs => {
                next_frontier.into_iter().take(1).map(|(p, _)| p).collect()
            }
            SearchStrategy::Beam { k } => {
                // Sure > Maybe 优先，保留 top-k
                let mut sorted = next_frontier;
                sorted.sort_by(|(_, a), (_, b)| {
                    let a_score = if *a == NodeStatus::Sure { 2 } else { 1 };
                    let b_score = if *b == NodeStatus::Sure { 2 } else { 1 };
                    b_score.cmp(&a_score)
                });
                sorted.into_iter().take(k).map(|(p, _)| p).collect()
            }
        };
    }

    // 达到最大深度，返回最长路径
    let best_path = frontier.into_iter()
        .max_by_key(|p| p.len())
        .unwrap_or_default();

    ToTResult {
        plan: best_path,
        nodes_explored,
        found_sure_path: false,
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_thoughts_separator() {
        let output = "步骤A\n---\n步骤B\n---\n步骤C";
        let thoughts = parse_thoughts(output, 3);
        assert_eq!(thoughts.len(), 3);
        assert_eq!(thoughts[0], "步骤A");
    }

    #[test]
    fn parse_thoughts_numbered() {
        let output = "1. 分析需求\n2. 设计架构\n3. 实现组件";
        let thoughts = parse_thoughts(output, 3);
        assert_eq!(thoughts.len(), 3);
    }

    #[test]
    fn parse_evaluation_sure() {
        let output = "Status: sure\nReason: The path is clear and complete";
        let (status, reason) = parse_evaluation(output);
        assert_eq!(status, NodeStatus::Sure);
        assert!(reason.contains("clear"));
    }

    #[test]
    fn parse_evaluation_impossible() {
        let output = "状态: impossible\n理由: 技术路径不可行";
        let (status, _) = parse_evaluation(output);
        assert_eq!(status, NodeStatus::Impossible);
    }

    #[test]
    fn parse_evaluation_maybe() {
        let output = "Status: maybe\nReason: Needs more investigation";
        let (status, _) = parse_evaluation(output);
        assert_eq!(status, NodeStatus::Maybe);
    }

    #[test]
    fn config_defaults() {
        let cfg = ToTConfig::default();
        assert_eq!(cfg.max_depth, 3);
        assert_eq!(cfg.branch_factor, 5);
        assert!(matches!(cfg.strategy, SearchStrategy::Beam { k: 3 }));
    }
}
