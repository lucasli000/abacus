//! OPRO Prompt 优化调度器
//!
//! ## 职责
//! 将 `optimization::opro` 接入 pipeline：
//! 1. Phase 5 检测 effectiveness 骤降 → 触发 OPRO 轮次
//! 2. Phase 2 注入 approved candidate 替换 system prompt 段
//! 3. 周期性 Cron 触发（通过 AutoEngine）
//!
//! ## 引用关系
//! - 被 `TurnPipeline::post_process()` 调用 should_trigger()
//! - 被 `PromptAssembly` 调用 get_approved_for_source()
//! - 消费 `crate::optimization::opro::{build_meta_prompt, process_round_output, ...}`
//!
//! ## 状态管理
//! OproState 在 CoreLoop 中作为 Arc<RwLock<OproState>> 存在。
//! 候选池 + 历史记录均为内存状态，可选持久化到 SQLite。

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::llm::{LlmProvider, LlmRequest, Message, MessageContent, MessageRole};
use crate::optimization::opro::{
    self, InstructionRecord, OptimizationCandidate, OproConfig,
};

use abacus_types::ModelId;

// ─── OproState ─────────────────────────────────────────────────────────────

/// OPRO 优化状态管理器
///
/// ## 生命周期
/// - 创建：CoreLoop::new()
/// - 读取：PromptAssembly Phase 2（注入 approved candidate）
/// - 写入：post_process Phase 5（检测触发 + 执行优化轮次）
/// - 持久化：可选 flush 到 SQLite（当前为内存模式）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OproState {
    /// 候选池（所有生成过的候选）
    pub candidates: Vec<OptimizationCandidate>,
    /// 历史评分记录（输入给 meta-prompt）
    pub history: Vec<InstructionRecord>,
    /// 上次运行 OPRO 的时间戳（Unix 秒）
    pub last_run_at: i64,
    /// 运行间隔（秒，默认 21600 = 6h）
    pub interval_secs: u64,
    /// effectiveness 骤降阈值（默认 0.15）
    /// 当 previous - current > threshold 时触发
    pub drop_threshold: f64,
    /// OPRO 配置
    pub config: OproConfig,
    /// 上一轮 effectiveness 值（用于骤降检测）
    pub previous_effectiveness: f64,
}

impl Default for OproState {
    fn default() -> Self {
        Self {
            candidates: Vec::new(),
            history: Vec::new(),
            last_run_at: 0,
            interval_secs: 21600, // 6 hours
            drop_threshold: 0.15,
            config: OproConfig {
                candidates_per_round: 8,
                max_history_in_prompt: 20,
                dedup_score_threshold: 0.02,
                lang: "en".into(),
            },
            previous_effectiveness: 0.5,
        }
    }
}

impl OproState {
    /// 检查是否应触发 OPRO 优化轮次
    ///
    /// ## 触发条件（OR）
    /// 1. 时间触发：距上次运行超过 interval_secs
    /// 2. 骤降触发：effectiveness 下降超过 drop_threshold
    /// 3. 历史充足：至少有 5 条历史记录（冷启动保护）
    pub fn should_trigger(&self, current_effectiveness: f64) -> bool {
        // 冷启动保护：历史不足时不触发
        if self.history.len() < 5 {
            return false;
        }

        let now = chrono::Utc::now().timestamp();
        let time_trigger = (now - self.last_run_at) > self.interval_secs as i64;
        let drop_trigger = (self.previous_effectiveness - current_effectiveness) > self.drop_threshold;

        time_trigger || drop_trigger
    }

    /// 记录本轮 effectiveness（用于骤降检测）
    pub fn record_effectiveness(&mut self, score: f64) {
        self.previous_effectiveness = score;
    }

    /// 添加历史记录
    pub fn add_history(&mut self, instruction: String, score: f64, source: String) {
        self.history.push(InstructionRecord {
            instruction,
            score,
            source,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        });

        // 保留最近 max_history_in_prompt 条
        if self.history.len() > self.config.max_history_in_prompt * 2 {
            let keep = self.config.max_history_in_prompt;
            self.history = self.history.split_off(self.history.len() - keep);
        }
    }

    /// 执行一轮 OPRO 优化
    ///
    /// ## 流程
    /// 1. 构建 meta-prompt（历史记录 + 优化指导）
    /// 2. 调用 LLM 生成 N 个候选
    /// 3. 解析 + 去重
    /// 4. 写入 candidates 池（status = "pending_review"）
    ///
    /// ## 返回
    /// 本轮生成的新候选数量
    pub async fn run_round(
        &mut self,
        provider: Arc<dyn LlmProvider>,
        source: &str,
    ) -> usize {
        let meta_prompt = opro::build_meta_prompt(
            &self.history,
            source,
            self.config.candidates_per_round,
            &self.config,
        );

        // 构造 LLM 请求
        let request = LlmRequest {
            model: ModelId("default".into()),
            messages: vec![Message {
                role: MessageRole::User,
                content: Some(MessageContent::Text(meta_prompt.clone())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
                prefix: false,
            }],
            system: None,
            system_segments: Vec::new(),
            tools: Vec::new(),
            temperature: Some(0.9), // 高温增加多样性
            max_tokens: Some(4000),
            top_p: None,
            stop: Vec::new(),
            stream: false,
            thinking_intent: None,
            cache_config: None,
            extra_body: Default::default(),
            user_message_preamble: None,
        };

        // 调用 LLM
        let output = match provider.complete(request).await {
            Ok(response) => {
                match &response.message.content {
                    Some(MessageContent::Text(t)) => t.clone(),
                    _ => String::new(),
                }
            }
            Err(_) => return 0,
        };

        // 解析候选 + 去重
        let existing_hashes: HashSet<String> = self.candidates.iter()
            .map(|c| c.hash.clone())
            .collect();

        let round = opro::process_round_output(
            source,
            &meta_prompt,
            &output,
            &existing_hashes,
            &self.config,
        );

        let new_count = round.candidates.len();
        self.candidates.extend(round.candidates);
        self.last_run_at = chrono::Utc::now().timestamp();

        new_count
    }

    /// 获取某 source 的最佳 approved candidate
    ///
    /// ## 调用时机
    /// PromptAssembly Phase 2——如果有 approved candidate，
    /// 用它替换当前 system prompt 对应段。
    pub fn get_approved_for_source(&self, source: &str) -> Option<&str> {
        self.candidates.iter()
            .filter(|c| c.source == source && c.status == "approved")
            .max_by(|a, b| {
                a.predicted_score
                    .partial_cmp(&b.predicted_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|c| c.instruction.as_str())
    }

    /// 手动批准某个候选
    pub fn approve_candidate(&mut self, hash: &str) -> bool {
        if let Some(candidate) = self.candidates.iter_mut().find(|c| c.hash == hash) {
            candidate.status = "approved".into();
            true
        } else {
            false
        }
    }

    /// 手动拒绝某个候选
    pub fn reject_candidate(&mut self, hash: &str) -> bool {
        if let Some(candidate) = self.candidates.iter_mut().find(|c| c.hash == hash) {
            candidate.status = "rejected".into();
            true
        } else {
            false
        }
    }

    /// 获取所有 pending_review 候选
    pub fn pending_candidates(&self) -> Vec<&OptimizationCandidate> {
        self.candidates.iter()
            .filter(|c| c.status == "pending_review")
            .collect()
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_trigger_cold_start_protection() {
        let state = OproState::default();
        // 历史 < 5 条，不触发
        assert!(!state.should_trigger(0.3));
    }

    #[test]
    fn test_should_trigger_time_based() {
        let mut state = OproState::default();
        // 填充历史
        for i in 0..10 {
            state.add_history(format!("prompt_{i}"), 0.5, "test".into());
        }
        state.last_run_at = 0; // 很久以前
        assert!(state.should_trigger(0.5));
    }

    #[test]
    fn test_should_trigger_effectiveness_drop() {
        let mut state = OproState::default();
        for i in 0..10 {
            state.add_history(format!("prompt_{i}"), 0.5, "test".into());
        }
        state.last_run_at = chrono::Utc::now().timestamp(); // 刚刚运行过
        state.previous_effectiveness = 0.8;

        // 骤降 0.3 > threshold 0.15
        assert!(state.should_trigger(0.5));
        // 小幅波动不触发
        assert!(!state.should_trigger(0.7));
    }

    #[test]
    fn test_approve_reject_candidate() {
        let mut state = OproState::default();
        state.candidates.push(OptimizationCandidate {
            instruction: "test prompt".into(),
            hash: "abc123".into(),
            source: "system_prompt".into(),
            generated_at: 0,
            status: "pending_review".into(),
            predicted_score: Some(0.85),
        });

        assert!(state.approve_candidate("abc123"));
        assert_eq!(state.get_approved_for_source("system_prompt"), Some("test prompt"));

        // 拒绝不存在的
        assert!(!state.reject_candidate("nonexistent"));
    }

    #[test]
    fn test_history_truncation() {
        let mut state = OproState::default();
        // max_history_in_prompt = 20, 截断阈值 = 20*2 = 40
        // 加到 41 条时触发截断（保留最近 20 条），之后继续加 9 条 = 29
        for i in 0..50 {
            state.add_history(format!("prompt_{i}"), 0.5, "test".into());
        }
        // 截断只在超过 2x 时触发一次，所以最终 = 20 + (50-41) = 29
        assert!(state.history.len() <= 40, "should never exceed 2x threshold");
        assert!(state.history.len() > 20, "only truncates at 2x, not every insert");
        // 最后一条应该是 prompt_49
        assert_eq!(state.history.last().unwrap().instruction, "prompt_49");
    }
}
