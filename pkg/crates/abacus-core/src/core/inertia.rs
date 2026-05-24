//! LLM 惰性对抗系统
//!
//! ## 场景
//! 检测 LLM 在输出中表现出的惰性行为（跳过工具、单次失败放弃、
//! 任务未完成、回避不确定性），并触发干预（自动重试/催促/目标复述）。
//!
//! ## 依赖
//! - `abacus_types::ToolId`: 工具标识
//! - `crate::core::context::estimate_tokens`: token 估算
//!
//! ## 引用关系
//! - 被 CoreLoop::process_turn() 在返回前调用
//! - 被 MagChain.after() 检测工具失败后是否重试
//! - 输出 InertiaSignal 供 InterventionPolicy 决策
//!
//! ## 边界
//! - 检测是纯规则引擎（零 LLM 调用，<1ms）
//! - 干预重试最多 2 次（防止无限循环）
//! - 代码类输出豁免大部分检测（代码不需要调工具验证）

use serde::{Deserialize, Serialize};

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 惰性信号
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 检测到的惰性信号
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InertiaSignal {
    /// 该调工具没调：输出含事实断言但本轮无工具调用
    ToolAvoidance {
        /// 输出中检测到的事实断言数
        assertion_count: u32,
        /// 本轮实际工具调用数
        tools_called: u32,
    },

    /// 单次失败放弃：工具报错后未重试就声明无法完成
    PrematureGiveUp {
        /// 失败的工具
        failed_tool: String,
        /// 重试次数（0 = 没重试）
        retry_count: u32,
    },

    /// 任务未完成：多步任务只做了部分就停止
    IncompleteTask {
        /// 检测到的总步骤数（从输入推断）
        expected_steps: u32,
        /// 实际完成的步骤
        completed_steps: u32,
    },

    /// 回避不确定性：说"不确定/不清楚"但没尝试查证
    UncertaintyAvoidance {
        /// 触发的不确定性短语
        phrase: String,
        /// 是否尝试了工具验证
        verification_attempted: bool,
    },

    /// 模糊应付：输出过短或全是套话，无实质内容
    ShallowResponse {
        /// 输出字符数
        response_chars: usize,
        /// 输入期望的最低输出量
        expected_min_chars: usize,
    },
}

impl InertiaSignal {
    /// 信号严重程度 [0, 1]
    pub fn severity(&self) -> f64 {
        match self {
            InertiaSignal::ToolAvoidance { assertion_count, .. } => {
                (*assertion_count as f64 / 3.0).min(1.0)
            }
            InertiaSignal::PrematureGiveUp { retry_count, .. } => {
                if *retry_count == 0 { 0.9 } else { 0.4 }
            }
            InertiaSignal::IncompleteTask { expected_steps, completed_steps } => {
                1.0 - (*completed_steps as f64 / *expected_steps as f64)
            }
            InertiaSignal::UncertaintyAvoidance { verification_attempted, .. } => {
                if *verification_attempted { 0.2 } else { 0.8 }
            }
            InertiaSignal::ShallowResponse { response_chars, expected_min_chars } => {
                1.0 - (*response_chars as f64 / *expected_min_chars as f64).min(1.0)
            }
        }
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 干预策略
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 惰性干预动作
#[derive(Debug, Clone)]
pub enum InertiaIntervention {
    /// 无干预（信号弱或豁免场景）
    None,

    /// 追加催促 prompt 后自动重跑（对用户透明）
    RetryWithNudge {
        nudge_prompt: String,
        /// 当前已重试次数
        attempt: u32,
    },

    /// 标记输出为低质量，TurnResult 携带警告
    FlagWarning {
        signal: InertiaSignal,
        suggestion: String,
    },
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 检测器
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 惰性配置
#[derive(Debug, Clone)]
pub struct InertiaConfig {
    /// 启用惰性检测
    pub enabled: bool,
    /// 最大自动重试次数
    pub max_retries: u32,
    /// 触发干预的最低严重度
    pub intervention_threshold: f64,
    /// 豁免的 TaskKind（代码类通常豁免）
    pub exempt_task_types: Vec<String>,
}

impl Default for InertiaConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 2,
            intervention_threshold: 0.6,
            exempt_task_types: vec![
                "code_writing".into(),
                "code_reading".into(),
                "file_edit".into(),
                "mathematics".into(),
            ],
        }
    }
}

/// LLM 惰性检测器
///
/// ## 场景
/// process_turn() 返回前调用，分析本轮输入/输出/工具调用的匹配度。
///
/// ## 设计原理
/// 纯规则引擎，不调 LLM。检测是确定性的（<1ms），保证不增加延迟。
pub struct InertiaDetector {
    config: InertiaConfig,
}

impl InertiaDetector {
    pub fn new(config: InertiaConfig) -> Self {
        Self { config }
    }

    /// 检测本轮是否有惰性信号
    ///
    /// ## 参数
    /// - input: 用户输入
    /// - response: LLM 输出文本
    /// - tools_called: 本轮调用的工具数
    /// - tools_failed: 本轮失败的工具数
    /// - tools_retried: 失败后重试的次数
    /// - task_type: 任务类型（用于豁免判断）
    /// - failed_tool_names: 失败的工具名列表
    /// - is_progressive_paused: 是否处于 Progressive 主动暂停状态
    pub fn detect(
        &self,
        input: &str,
        response: &str,
        tools_called: u32,
        tools_failed: u32,
        tools_retried: u32,
        task_type: &str,
        failed_tool_names: &[String],
        is_progressive_paused: bool,
    ) -> Vec<InertiaSignal> {
        if !self.config.enabled {
            return Vec::new();
        }

        // 豁免场景
        if self.config.exempt_task_types.contains(&task_type.to_string()) {
            return Vec::new();
        }

        let mut signals = Vec::new();

        // ─── 检测 1: 工具回避 ────────────────────────────
        if let Some(signal) = self.detect_tool_avoidance(input, response, tools_called) {
            signals.push(signal);
        }

        // ─── 检测 2: 过早放弃 ────────────────────────────
        if let Some(signal) = self.detect_premature_giveup(response, tools_failed, tools_retried, failed_tool_names) {
            signals.push(signal);
        }

        // ─── 检测 3: 任务未完成（Progressive 暂停时豁免）────
        if !is_progressive_paused {
            if let Some(signal) = self.detect_incomplete_task(input, response) {
                signals.push(signal);
            }
        }

        // ─── 检测 4: 不确定性回避 ────────────────────────
        if let Some(signal) = self.detect_uncertainty_avoidance(response, tools_called) {
            signals.push(signal);
        }

        // ─── 检测 5: 浅层应付 ────────────────────────────
        if let Some(signal) = self.detect_shallow_response(input, response) {
            signals.push(signal);
        }

        signals
    }

    /// 检测 1: 输出含事实断言但未调工具
    fn detect_tool_avoidance(&self, input: &str, response: &str, tools_called: u32) -> Option<InertiaSignal> {
        if tools_called > 0 {
            return None;
        }

        // 输入要求查证的信号
        let query_signals = [
            "最新", "当前", "现在", "多少", "是什么", "查一下", "看看",
            "latest", "current", "how many", "what is", "check", "look up",
        ];
        let input_needs_tool = query_signals.iter().any(|s| input.contains(s));

        if !input_needs_tool {
            return None;
        }

        // 输出含事实性断言的信号（精确模式，减少中文假阳性）
        // 强模式：明确的量化/估计表达
        let strong_assertion_patterns = [
            "达到", "约为", "大约有", "大概是", "通常是", "一般为",
            "总计", "总共有", "平均为", "最多有", "最少有", "通常在",
            "approximately ", "currently at ", "typically around ",
        ];
        // 弱模式：含数字的上下文断言（"约 7" 算，"约定" 不算）
        let numeric_assertion_count = {
            let re_patterns = ["约 ", "共 ", "有 ", "是 ", "为 "];
            re_patterns.iter().filter(|p| {
                response.find(*p).map(|pos| {
                    let after = &response[pos + p.len()..];
                    after.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
                }).unwrap_or(false)
            }).count() as u32
        };
        // 数值范围模式（如 "7.0-7.5"、"3~5"）
        let has_numeric_range = response.chars().any(|c| c == '-' || c == '~') && {
            let bytes = response.as_bytes();
            bytes.windows(5).any(|w| {
                w[0].is_ascii_digit() && (w[1] == b'.' || w[1] == b'-' || w[1] == b'~' || (w[1].is_ascii_digit() && (w[2] == b'-' || w[2] == b'~')))
            })
        };
        let strong_count = strong_assertion_patterns.iter()
            .filter(|m| response.contains(*m))
            .count() as u32;
        let assertion_count = strong_count + numeric_assertion_count + if has_numeric_range { 1 } else { 0 };

        if assertion_count >= 2 {
            Some(InertiaSignal::ToolAvoidance {
                assertion_count,
                tools_called,
            })
        } else {
            None
        }
    }

    /// 检测 2: 工具失败后未重试就放弃
    fn detect_premature_giveup(
        &self,
        response: &str,
        tools_failed: u32,
        tools_retried: u32,
        failed_tool_names: &[String],
    ) -> Option<InertiaSignal> {
        if tools_failed == 0 {
            return None;
        }

        let giveup_phrases = [
            "无法", "无能为力", "抱歉", "不能完成", "建议你自己",
            "cannot", "unable to", "sorry", "I can't",
            "请手动", "需要你自己",
        ];
        let has_giveup = giveup_phrases.iter().any(|p| response.contains(p));

        if has_giveup && tools_retried == 0 {
            let failed_tool = failed_tool_names.first()
                .cloned()
                .unwrap_or_else(|| "(unknown)".into());
            Some(InertiaSignal::PrematureGiveUp {
                failed_tool,
                retry_count: tools_retried,
            })
        } else {
            None
        }
    }

    /// 检测 3: 多步任务未完成
    fn detect_incomplete_task(&self, input: &str, response: &str) -> Option<InertiaSignal> {
        // 先确认是祈使性输入（真正的任务请求，而非描述性背景）
        let imperative_markers = [
            "请", "帮我", "帮忙", "需要你", "麻烦", "做一下",
            "please", "help me", "I need you to", "could you",
        ];
        let is_imperative = imperative_markers.iter().any(|m| input.contains(m));
        if !is_imperative {
            return None; // 非任务请求，不检测
        }

        // 从输入推断期望步骤数
        let step_indicators = [
            "第一", "第二", "第三", "第四", "第五",
            "1.", "2.", "3.", "4.", "5.",
            "首先", "然后", "接着", "最后",
            "first", "second", "third", "finally",
        ];
        let expected = step_indicators.iter()
            .filter(|s| input.contains(*s))
            .count() as u32;

        if expected < 3 {
            return None; // 非多步任务
        }

        // 从输出检测完成了多少步
        let _completion_markers = [
            "完成", "done", "✓", "✅", "已",
        ];
        let step_output_markers: Vec<&str> = step_indicators.iter()
            .filter(|s| response.contains(*s))
            .copied()
            .collect();

        let completed = step_output_markers.len() as u32;
        let leave_to_user = response.contains("剩余") || response.contains("你可以继续")
            || response.contains("remaining") || response.contains("you can");

        if completed < expected && leave_to_user {
            Some(InertiaSignal::IncompleteTask {
                expected_steps: expected,
                completed_steps: completed,
            })
        } else {
            None
        }
    }

    /// 检测 4: 说"不确定"但没查证
    ///
    /// 精度优化：区分三种情况
    /// - 调了工具，工具返回"无结果" → 合理的不确定（不报）
    /// - 调了工具，结论仍用"可能是" → 轻微信号（severity 低）
    /// - 没调工具就说"不确定" → 强信号
    fn detect_uncertainty_avoidance(&self, response: &str, tools_called: u32) -> Option<InertiaSignal> {
        // 强不确定性短语（明确表达"不知道"）
        let strong_uncertainty = [
            "不确定", "不太清楚", "我不知道", "无法确认",
            "not sure", "I don't know", "cannot confirm",
        ];
        // 弱不确定性短语（猜测性表述）
        let weak_uncertainty = [
            "可能是", "大概是", "也许", "或许",
            "might be", "possibly", "perhaps", "maybe",
        ];

        // 如果输出含"根据查询结果"/"工具返回"等标记 → 说明引用了工具结果，豁免
        let evidence_markers = [
            "根据", "查询结果", "搜索结果", "显示", "返回",
            "according to", "results show", "found that",
        ];
        let has_evidence = evidence_markers.iter().any(|m| response.contains(m));
        if has_evidence {
            return None; // 有引用证据，不算回避
        }

        for phrase in &strong_uncertainty {
            if response.contains(phrase) {
                return Some(InertiaSignal::UncertaintyAvoidance {
                    phrase: phrase.to_string(),
                    verification_attempted: tools_called > 0,
                });
            }
        }

        // 弱不确定性：只有在完全没调工具时才报
        if tools_called == 0 {
            for phrase in &weak_uncertainty {
                if response.contains(phrase) {
                    return Some(InertiaSignal::UncertaintyAvoidance {
                        phrase: phrase.to_string(),
                        verification_attempted: false,
                    });
                }
            }
        }

        None
    }

    /// 检测 5: 输出过短（相对于输入复杂度）
    fn detect_shallow_response(&self, input: &str, response: &str) -> Option<InertiaSignal> {
        let input_chars = input.chars().count();
        let response_chars = response.chars().count();

        // 中文字符信息密度高：50 中文字 ≈ 150 英文字
        // 阈值：输入 > 50 字且输出 < 输入/3 → 可能在敷衍
        let expected_min = if input_chars > 100 {
            80
        } else if input_chars > 50 {
            30
        } else {
            return None; // 短输入不检测
        };

        if response_chars < expected_min {
            Some(InertiaSignal::ShallowResponse {
                response_chars,
                expected_min_chars: expected_min,
            })
        } else {
            None
        }
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 干预策略决策
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 干预策略决策器
///
/// ## 场景
/// 接收 InertiaSignal，决定是否干预以及干预方式。
pub struct InterventionPolicy {
    config: InertiaConfig,
}

impl InterventionPolicy {
    pub fn new(config: InertiaConfig) -> Self {
        Self { config }
    }

    /// 根据信号决定干预动作
    pub fn decide(&self, signals: &[InertiaSignal], current_retry: u32) -> InertiaIntervention {
        if signals.is_empty() {
            return InertiaIntervention::None;
        }

        // 找最严重的信号
        let worst = signals.iter()
            .max_by(|a, b| a.severity().partial_cmp(&b.severity()).unwrap_or(std::cmp::Ordering::Equal));

        let worst = match worst {
            Some(s) => s,
            None => return InertiaIntervention::None,
        };

        // 低于阈值不干预
        if worst.severity() < self.config.intervention_threshold {
            return InertiaIntervention::None;
        }

        // 超过最大重试次数 → 只标记警告不重试
        if current_retry >= self.config.max_retries {
            return InertiaIntervention::FlagWarning {
                signal: worst.clone(),
                suggestion: self.suggestion_for(worst),
            };
        }

        // 生成催促 prompt
        let nudge = self.nudge_for(worst);
        InertiaIntervention::RetryWithNudge {
            nudge_prompt: nudge,
            attempt: current_retry + 1,
        }
    }

    /// 为不同信号生成催促 prompt
    fn nudge_for(&self, signal: &InertiaSignal) -> String {
        match signal {
            InertiaSignal::ToolAvoidance { .. } => {
                "你的回答包含事实断言但未使用工具验证。请使用合适的工具（kb.query / web.search / fs.read）查证后再回答。不要凭记忆猜测。".to_string()
            }
            InertiaSignal::PrematureGiveUp { failed_tool, .. } => {
                format!(
                    "工具 {} 调用失败，但你还没有尝试其他方法。请换一组参数重试，或尝试替代工具。至少尝试 2 种不同策略后才能声明无法完成。",
                    failed_tool
                )
            }
            InertiaSignal::IncompleteTask { expected_steps, completed_steps } => {
                format!(
                    "用户要求了 {} 个步骤，你只完成了 {} 个。请继续完成剩余步骤，不要留给用户自行处理。",
                    expected_steps, completed_steps
                )
            }
            InertiaSignal::UncertaintyAvoidance { phrase, .. } => {
                format!(
                    "你说「{}」，但没有尝试验证。请先用工具查证（kb.query / web.search），确认后再给出结论或明确说明查证范围和结果。",
                    phrase
                )
            }
            InertiaSignal::ShallowResponse { expected_min_chars, .. } => {
                format!(
                    "你的回答过于简短（期望至少 {} 字），无法充分回应用户的问题。请展开说明，提供具体细节、数据或步骤。",
                    expected_min_chars
                )
            }
        }
    }

    /// 为警告生成用户可见的建议
    fn suggestion_for(&self, signal: &InertiaSignal) -> String {
        match signal {
            InertiaSignal::ToolAvoidance { .. } => "LLM 未使用工具验证事实断言。建议追问要求引用来源。".into(),
            InertiaSignal::PrematureGiveUp { .. } => "LLM 在工具失败后未充分重试。可尝试重新描述需求。".into(),
            InertiaSignal::IncompleteTask { .. } => "LLM 未完成所有步骤。可提示「请继续」。".into(),
            InertiaSignal::UncertaintyAvoidance { .. } => "LLM 表达不确定但未查证。可要求其先查再答。".into(),
            InertiaSignal::ShallowResponse { .. } => "LLM 回答过于简短。可要求展开。".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_detector() -> InertiaDetector {
        InertiaDetector::new(InertiaConfig::default())
    }

    #[test]
    fn test_tool_avoidance_detected() {
        let det = default_detector();
        let input = "查一下当前的汇率是多少";
        let response = "当前美元对人民币汇率约为 7.2，通常在 7.0-7.5 之间波动。";
        let signals = det.detect(input, response, 0, 0, 0, "general_chat", &[], false);
        assert!(!signals.is_empty());
        assert!(matches!(signals[0], InertiaSignal::ToolAvoidance { .. }));
    }

    #[test]
    fn test_no_avoidance_when_tool_called() {
        let det = default_detector();
        let input = "查一下当前的汇率是多少";
        let response = "根据查询结果，当前汇率是 7.21";
        let signals = det.detect(input, response, 1, 0, 0, "general_chat", &[], false);
        // 调了工具就不算回避
        let avoidance = signals.iter().any(|s| matches!(s, InertiaSignal::ToolAvoidance { .. }));
        assert!(!avoidance);
    }

    #[test]
    fn test_code_exempt() {
        let det = default_detector();
        let input = "查一下当前版本号是多少";
        let response = "当前版本是 1.0.0";
        let signals = det.detect(input, response, 0, 0, 0, "code_writing", &[], false);
        assert!(signals.is_empty()); // 代码类豁免
    }

    #[test]
    fn test_premature_giveup() {
        let det = default_detector();
        let input = "帮我查询数据库中的用户数";
        let response = "抱歉，无法完成此操作，建议你自己查看数据库。";
        let signals = det.detect(input, response, 1, 1, 0, "data_analysis", &["db_query".into()], false);
        let giveup = signals.iter().any(|s| matches!(s, InertiaSignal::PrematureGiveUp { .. }));
        assert!(giveup);
    }

    #[test]
    fn test_no_giveup_when_retried() {
        let det = default_detector();
        let input = "帮我查询数据库中的用户数";
        let response = "抱歉，尝试了多种方式仍无法连接，建议你自己检查连接配置。";
        // retried = 2，虽然放弃了但重试过
        let signals = det.detect(input, response, 3, 1, 2, "data_analysis", &["db_query".into()], false);
        let giveup = signals.iter().any(|s| matches!(s, InertiaSignal::PrematureGiveUp { retry_count, .. } if *retry_count == 0));
        assert!(!giveup);
    }

    #[test]
    fn test_uncertainty_avoidance() {
        let det = default_detector();
        let input = "这个 API 的限流策略是什么";
        let response = "我不确定具体的限流策略，可能是每分钟 60 次。";
        let signals = det.detect(input, response, 0, 0, 0, "knowledge_query", &[], false);
        let avoidance = signals.iter().any(|s| matches!(s, InertiaSignal::UncertaintyAvoidance { verification_attempted: false, .. }));
        assert!(avoidance);
    }

    #[test]
    fn test_shallow_response() {
        let det = default_detector();
        let input = "请详细分析过去三个月的销售数据趋势，对比各渠道表现，给出下季度的策略建议，需要考虑季节性因素和竞品动态。包括具体数据支撑和可执行的行动建议。";
        let response = "好的。";
        let signals = det.detect(input, response, 0, 0, 0, "data_analysis", &[], false);
        let shallow = signals.iter().any(|s| matches!(s, InertiaSignal::ShallowResponse { .. }));
        assert!(shallow);
    }

    #[test]
    fn test_intervention_policy_retry() {
        let policy = InterventionPolicy::new(InertiaConfig::default());
        let signals = vec![InertiaSignal::ToolAvoidance {
            assertion_count: 3,
            tools_called: 0,
        }];
        let intervention = policy.decide(&signals, 0);
        assert!(matches!(intervention, InertiaIntervention::RetryWithNudge { .. }));
    }

    #[test]
    fn test_intervention_policy_max_retries() {
        let policy = InterventionPolicy::new(InertiaConfig::default());
        let signals = vec![InertiaSignal::ToolAvoidance {
            assertion_count: 3,
            tools_called: 0,
        }];
        // 已重试 2 次（达到上限）→ 只警告不重试
        let intervention = policy.decide(&signals, 2);
        assert!(matches!(intervention, InertiaIntervention::FlagWarning { .. }));
    }

    #[test]
    fn test_intervention_policy_below_threshold() {
        let policy = InterventionPolicy::new(InertiaConfig::default());
        let signals = vec![InertiaSignal::UncertaintyAvoidance {
            phrase: "可能是".into(),
            verification_attempted: true, // 已尝试验证 → severity 0.2
        }];
        let intervention = policy.decide(&signals, 0);
        assert!(matches!(intervention, InertiaIntervention::None));
    }
}
