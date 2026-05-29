//! MT-GRPO 轨迹收集器（P3-B4）
//!
//! ## 研究来源
//! Zeng et al. 2024 "Multi-Turn-RL-Agent" — MT-GRPO 算法
//!
//! ## 核心概念
//! - `completion_mask`: 区分模型生成（true）vs 工具输出/环境响应（false）
//!   仅对 mask=true 的 token 做策略优化
//! - `turn_signals`: 每步工具调用的质量信号（turn-level reward）
//! - `outcome_signal`: 最终结果质量（outcome-level reward）
//! - `result_positions`: `<result>` tag（工具输出）在序列中的位置
//!   用于 position-aware advantage 分配
//!
//! ## 设计意图
//! 当前只做轨迹收集（在线）。离线训练通过导出 JSONL 供 MT-GRPO 脚本消费。
//! 未来可扩展到在线 PPO 训练（需要梯度计算基础设施）。
//!
//! ## 引用关系
//! - 写入方：TurnPipeline::post_process()（每轮末尾）
//! - 存储：MagChain PersistentAuditLogger（SQLite）
//! - 导出：AutoEngine pipeline 定期导出 JSONL
//! - 生命周期：轨迹随 session 累积，导出后标记为 exported

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::llm::Message;

/// 单步工具调用的 Turn-level 信号（对应 MT-GRPO turn reward）
///
/// ## 字段含义
/// - `tool_success`: 工具调用是否成功（EffectivenessTracker.success_rate 原始信号）
/// - `knowledge_hit`: kb.search/query 是否返回有效结果（ZeroHit → false）
/// - `tool_id`: 工具标识（用于 position-aware advantage 中定位 result 位置）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSignal {
    /// 工具 ID
    pub tool_id: String,
    /// 工具调用是否成功
    pub tool_success: bool,
    /// 知识检索是否命中（仅 kb.* 工具有意义；其他工具默认 true）
    pub knowledge_hit: bool,
    /// 工具执行延迟（毫秒）
    pub latency_ms: u64,
}

/// 单条完整轨迹（一次多轮对话的完整记录）
///
/// ## 对应关系
/// - `messages`: 完整对话历史（含 system/user/assistant/tool）
/// - `completion_mask`: 与 messages 等长，true = 模型生成（参与策略优化）
/// - `turn_signals`: 按工具调用顺序的 Turn-level 信号
/// - `outcome_signal`: 最终结果质量（来自 EffectivenessTracker 或用户反馈）
/// - `result_positions`: 每个工具输出（<result>）在 messages 中的索引
///
/// ## MT-GRPO position-aware advantage 设计
/// 在 result_position 之前的 token：优势 = outcome_adv + turn_adv_coef × turn_adv
/// 在 result_position 之后的 token：优势 = outcome_adv
/// 这使得模型在工具执行前强化"有效搜索/调用"行为，执行后优化"最终结论"质量
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trajectory {
    /// 轨迹唯一 ID
    pub id: String,
    /// Session ID（一个 session 可包含多条轨迹）
    pub session_id: String,
    /// Turn 编号
    pub turn_number: u32,
    /// 完整对话消息列表
    pub messages: Vec<TrajectoryMessage>,
    /// 与 messages 等长的掩码（true=模型生成，false=工具输出/环境响应）
    pub completion_mask: Vec<bool>,
    /// Turn-level 信号（按工具调用顺序）
    pub turn_signals: Vec<TurnSignal>,
    /// Outcome-level 信号（最终结果质量，0.0-1.0）
    ///
    /// 来源优先级：用户反馈（最高）> EffectivenessTracker.composite_score > 默认 0.5
    pub outcome_signal: f32,
    /// 工具输出在 messages 列表中的位置索引（用于 position-aware advantage）
    pub result_positions: Vec<usize>,
    /// 任务分类
    pub task_kind: String,
    /// 轨迹收集时间戳（Unix 毫秒）
    pub collected_at: i64,
    /// 是否已导出为训练数据
    pub exported: bool,
    /// 额外元数据（模型名、温度等）
    pub metadata: HashMap<String, String>,
}

/// 轻量化消息记录（避免重复存储完整 Message 的所有字段）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryMessage {
    /// 消息角色（"system" | "user" | "assistant" | "tool"）
    pub role: String,
    /// 消息内容摘要（前 500 字符，避免大量重复存储）
    pub content_preview: String,
    /// 是否为工具输出（role=tool）
    pub is_tool_output: bool,
    /// 工具 ID（仅 is_tool_output=true 时有值）
    pub tool_id: Option<String>,
}

impl TrajectoryMessage {
    /// 从 llm::Message 构建 TrajectoryMessage
    pub fn from_message(msg: &Message) -> Self {
        use crate::llm::{MessageContent, MessageRole};

        let role = match msg.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        }.to_string();

        let content_str = match &msg.content {
            Some(MessageContent::Text(t)) => t.chars().take(500).collect::<String>(),
            _ => String::new(),
        };

        let is_tool = msg.role == MessageRole::Tool;
        let tool_id = msg.tool_call_id.clone();

        Self {
            role,
            content_preview: content_str,
            is_tool_output: is_tool,
            tool_id,
        }
    }
}

/// 从完整消息列表构建 completion_mask
///
/// ## 规则
/// - role=tool（工具输出）→ false（环境响应，不参与策略优化）
/// - role=system → false（系统 prompt，固定不优化）
/// - role=user（非工具输出的 user）→ false（用户输入，不优化）
/// - role=assistant → true（模型生成，参与策略优化）
pub fn build_completion_mask(messages: &[Message]) -> Vec<bool> {
    use crate::llm::MessageRole;
    messages.iter().map(|m| m.role == MessageRole::Assistant).collect()
}

/// 从消息列表提取工具输出位置（result_positions）
///
/// ## 逻辑
/// 找出所有 role=tool 的消息索引，这些位置对应 MT-GRPO 中的 <result> 标签位置
pub fn extract_result_positions(messages: &[Message]) -> Vec<usize> {
    use crate::llm::MessageRole;
    messages.iter().enumerate()
        .filter(|(_, m)| m.role == MessageRole::Tool)
        .map(|(i, _)| i)
        .collect()
}

/// 轨迹构建器（收集一次 turn 的所有信号）
///
/// ## 使用方式
/// ```rust,ignore
/// let mut builder = TrajectoryBuilder::new(session_id, turn_number, task_kind);
/// // 工具执行后
/// builder.add_turn_signal(TurnSignal { ... });
/// // turn 结束后
/// let traj = builder.build(messages, outcome_signal);
/// ```
///
/// ## 引用关系
/// - 创建：TurnPipeline::post_process() 开始时
/// - 更新：每次工具调用完成后
/// - 完成：post_process() 结束时调用 build()
#[derive(Default)]
pub struct TrajectoryBuilder {
    session_id: String,
    turn_number: u32,
    task_kind: String,
    turn_signals: Vec<TurnSignal>,
    metadata: HashMap<String, String>,
}

impl TrajectoryBuilder {
    pub fn new(session_id: impl Into<String>, turn_number: u32, task_kind: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            turn_number,
            task_kind: task_kind.into(),
            turn_signals: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// 添加工具调用 Turn-level 信号
    pub fn add_turn_signal(&mut self, signal: TurnSignal) {
        self.turn_signals.push(signal);
    }

    /// 添加元数据（模型名、温度等）
    pub fn add_metadata(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.metadata.insert(key.into(), value.into());
    }

    /// 构建最终轨迹
    ///
    /// ## 参数
    /// - `messages`: 本 turn 的完整消息历史
    /// - `outcome_signal`: 最终结果质量（0.0-1.0）
    pub fn build(self, messages: &[Message], outcome_signal: f32) -> Trajectory {
        let completion_mask = build_completion_mask(messages);
        let result_positions = extract_result_positions(messages);
        let traj_messages: Vec<TrajectoryMessage> = messages.iter()
            .map(TrajectoryMessage::from_message)
            .collect();

        Trajectory {
            id: format!("traj_{}_{}", self.session_id, self.turn_number),
            session_id: self.session_id,
            turn_number: self.turn_number,
            messages: traj_messages,
            completion_mask,
            turn_signals: self.turn_signals,
            outcome_signal,
            result_positions,
            task_kind: self.task_kind,
            collected_at: chrono::Utc::now().timestamp_millis(),
            exported: false,
            metadata: self.metadata,
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Message, MessageContent, MessageRole};

    fn make_msg(role: MessageRole, content: &str) -> Message {
        Message {
            role,
            content: Some(MessageContent::Text(content.into())),
            name: None, tool_calls: None,
            tool_call_id: None, reasoning_content: None, prefix: false,
        }
    }

    #[test]
    fn completion_mask_marks_assistant() {
        let msgs = vec![
            make_msg(MessageRole::System, "system"),
            make_msg(MessageRole::User, "user"),
            make_msg(MessageRole::Assistant, "assistant"),
            make_msg(MessageRole::Tool, "tool"),
        ];
        let mask = build_completion_mask(&msgs);
        assert_eq!(mask, vec![false, false, true, false]);
    }

    #[test]
    fn result_positions_finds_tool_msgs() {
        let msgs = vec![
            make_msg(MessageRole::User, "q"),
            make_msg(MessageRole::Assistant, "a"),
            make_msg(MessageRole::Tool, "result"),
            make_msg(MessageRole::Assistant, "a2"),
        ];
        let positions = extract_result_positions(&msgs);
        assert_eq!(positions, vec![2]);
    }

    #[test]
    fn builder_creates_trajectory() {
        let mut builder = TrajectoryBuilder::new("s1", 1, "code_reading");
        builder.add_turn_signal(TurnSignal {
            tool_id: "kb_query".into(),
            tool_success: true,
            knowledge_hit: true,
            latency_ms: 50,
        });

        let msgs = vec![
            make_msg(MessageRole::User, "question"),
            make_msg(MessageRole::Assistant, "answer"),
        ];
        let traj = builder.build(&msgs, 0.8);

        assert_eq!(traj.session_id, "s1");
        assert_eq!(traj.turn_number, 1);
        assert_eq!(traj.turn_signals.len(), 1);
        assert_eq!(traj.outcome_signal, 0.8);
        assert!(!traj.exported);
    }
}
