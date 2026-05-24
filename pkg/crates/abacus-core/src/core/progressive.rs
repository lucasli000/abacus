//! 渐进输出协议 — 状态机运行时
//!
//! ## 场景
//! 管理 LLM 输出的生命周期：分析 → 清单 → 确认 → 续写 → 完成。
//! 每个 Session 持有一个 ProgressiveController 实例。
//!
//! ## 依赖
//! - `progressive_gate.rs`: 策略决策
//! - `progressive_inject.rs`: Prompt 注入
//! - `interaction.rs`: Checkpoint 记录
//!
//! ## 引用关系
//! - 被 CoreLoop 持有（SessionState 字段）
//! - 被 L4 渠道层读取（状态查询 → UI 渲染）
//! - 发出 ProgressiveEvent 供 EventBus 广播

use abacus_types::progressive::*;
use super::progressive_gate::ProgressiveGate;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// 渐进输出控制器
///
/// ## 生命周期
/// 每次用户输入触发 LLM 调用时激活。
/// 随对话推进在状态间流转。
/// 输出完成或用户中止后终结，下次输入重新激活。
///
/// ## 与 CoreLoop 的集成点
/// 1. pre_llm_request → controller.begin()
/// 2. on_llm_response → controller.on_output_chunk()
/// 3. on_user_confirm → controller.on_confirmation()
/// 4. post_complete → controller.finalize()
pub struct ProgressiveController {
    state: ProgressiveState,
    gate: ProgressiveGate,
    autonomy: AutonomyLevel,
    scope: GateScope,
    strategy: Option<OutputStrategy>,
    event_tx: Option<mpsc::Sender<ProgressiveEvent>>,
    /// 累积的 LLM 输出（用于 checklist JSON 检测）
    output_buffer: String,
    /// 清单解析重试计数
    retry_count: u32,
}

impl ProgressiveController {
    /// 创建控制器
    pub fn new(
        gate: ProgressiveGate,
        autonomy: AutonomyLevel,
        event_tx: Option<mpsc::Sender<ProgressiveEvent>>,
    ) -> Self {
        Self {
            state: ProgressiveState::Analyzing,
            gate,
            autonomy,
            scope: GateScope::SingleAgent,
            strategy: None,
            event_tx,
            output_buffer: String::new(),
            retry_count: 0,
        }
    }

    /// 设置作用域（Team 场景下由编排层设置）
    pub fn set_scope(&mut self, scope: GateScope) {
        self.scope = scope;
    }

    // ─── 生命周期方法 ─────────────────────────────────────────

    /// 阶段 1：分析任务，决定策略
    ///
    /// ## 调用时机
    /// CoreLoop 收到用户输入后、构建 LLM Request 前。
    ///
    /// ## 返回
    /// 决定的策略引用（CoreLoop 据此决定是否注入渐进 prompt）
    pub fn begin(&mut self, profile: &ComplexityProfile) -> &OutputStrategy {
        let strategy = self.gate.decide(profile, self.autonomy, self.scope);
        self.state = ProgressiveState::StrategyDecided {
            strategy: strategy.clone(),
        };
        self.strategy = Some(strategy);
        self.output_buffer.clear();
        self.retry_count = 0;

        self.emit(ProgressiveEvent::StrategyDecided(
            self.strategy.clone().unwrap_or(OutputStrategy::PassThrough),
        ));

        self.strategy.as_ref().unwrap_or(&OutputStrategy::PassThrough)
    }

    /// 阶段 1b：带任务类型的决策（强制规则生效）
    pub fn begin_with_task_type(
        &mut self,
        profile: &ComplexityProfile,
        task_type: &str,
    ) -> &OutputStrategy {
        let strategy = self.gate.decide_with_task_type(
            profile, self.autonomy, self.scope, task_type,
        );
        self.state = ProgressiveState::StrategyDecided {
            strategy: strategy.clone(),
        };
        self.strategy = Some(strategy);
        self.output_buffer.clear();
        self.retry_count = 0;

        self.emit(ProgressiveEvent::StrategyDecided(
            self.strategy.clone().unwrap_or(OutputStrategy::PassThrough),
        ));

        self.strategy.as_ref().unwrap_or(&OutputStrategy::PassThrough)
    }

    /// 阶段 2：处理 LLM 输出
    ///
    /// ## 返回
    /// - Forward: 正常转发给用户
    /// - Buffer: 暂存（还在组装清单）
    /// - Gate: 停止生成，等确认
    pub fn on_output_chunk(&mut self, chunk: &str) -> OutputAction {
        match &self.strategy {
            Some(OutputStrategy::PassThrough) | None => OutputAction::Forward,

            Some(OutputStrategy::Gated { .. }) => {
                match &self.state {
                    ProgressiveState::StrategyDecided { .. } => {
                        // 累积输出，尝试解析 checklist（上限 50KB 防 OOM）
                        if self.output_buffer.len() + chunk.len() > 50_000 {
                            // Buffer 已达上限，停止累积并降级为 PassThrough
                            tracing::warn!(
                                buffer_size = self.output_buffer.len(),
                                "progressive buffer overflow, degrading to passthrough"
                            );
                            return self.on_checklist_parse_failed();
                        }
                        self.output_buffer.push_str(chunk);
                        if let Some(checklist) = try_parse_checklist(&self.output_buffer) {
                            self.transition_to_awaiting(checklist);
                            OutputAction::Gate
                        } else if self.output_buffer.len() > 10000 {
                            // 输出过长仍未解析到 checklist → 降级
                            self.on_checklist_parse_failed()
                        } else {
                            OutputAction::Buffer
                        }
                    },
                    ProgressiveState::Generating { .. } => OutputAction::Forward,
                    _ => OutputAction::Forward,
                }
            },

            Some(OutputStrategy::Staged { .. }) => {
                if chunk.contains("---section-break---") {
                    self.advance_section();
                }
                OutputAction::Forward
            },
        }
    }

    /// 阶段 3：接收用户确认
    pub fn on_confirmation(&mut self, responses: Vec<(u32, UserResponse)>) {
        match &self.state {
            ProgressiveState::AwaitingConfirmation { .. }
            | ProgressiveState::ReconfirmRequested { .. } => {
                self.state = ProgressiveState::Generating {
                    strategy: self.strategy.clone().unwrap_or(OutputStrategy::PassThrough),
                    current_section: Some(0),
                    confirmed_decisions: responses,
                };
                self.output_buffer.clear();
            },
            _ => {} // 忽略非法状态下的确认
        }
    }

    /// 用户要求修改已确认决策（从 Generating 回退）
    pub fn request_reconfirm(&mut self, modification_ids: Vec<u32>) {
        if let ProgressiveState::Generating { confirmed_decisions, .. } = &self.state {
            self.state = ProgressiveState::ReconfirmRequested {
                prior_decisions: confirmed_decisions.clone(),
                modification_ids,
            };
        }
    }

    /// 阶段 4：输出完成
    pub fn finalize(&mut self, total_tokens: u64) {
        let total_sections = match &self.strategy {
            Some(OutputStrategy::Staged { sections }) => sections.len() as u32,
            _ => 1,
        };
        self.state = ProgressiveState::Completed {
            total_sections,
            total_tokens,
        };
        self.emit(ProgressiveEvent::OutputCompleted { total_tokens });
    }

    /// 用户中止
    pub fn abort(&mut self, reason: String) {
        self.state = ProgressiveState::Aborted { reason };
    }

    // ─── 查询方法（L4 层读取）─────────────────────────────────

    pub fn current_state(&self) -> &ProgressiveState {
        &self.state
    }

    pub fn current_strategy(&self) -> Option<&OutputStrategy> {
        self.strategy.as_ref()
    }

    pub fn is_blocking(&self) -> bool {
        matches!(self.state, ProgressiveState::AwaitingConfirmation { .. })
    }

    pub fn is_passthrough(&self) -> bool {
        matches!(self.strategy, Some(OutputStrategy::PassThrough) | None)
    }

    /// 获取已确认的决策（供 Prompt 注入使用）
    pub fn confirmed_decisions(&self) -> Option<&Vec<(u32, UserResponse)>> {
        if let ProgressiveState::Generating { confirmed_decisions, .. } = &self.state {
            Some(confirmed_decisions)
        } else {
            None
        }
    }

    // ─── 内部方法 ─────────────────────────────────────────────

    fn transition_to_awaiting(&mut self, checklist: Checklist) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        self.emit(ProgressiveEvent::ChecklistReady(checklist.clone()));

        let timeout = checklist.timeout
            .map(|d| d.as_secs())
            .unwrap_or(300);
        self.emit(ProgressiveEvent::AwaitingInput { timeout_secs: timeout });

        self.state = ProgressiveState::AwaitingConfirmation {
            checklist,
            emitted_at_epoch_ms: now,
        };
    }

    fn advance_section(&mut self) {
        let completed_id = if let ProgressiveState::Generating { current_section: Some(s), .. } = &mut self.state {
            *s += 1;
            Some(*s - 1)
        } else {
            None
        };
        if let Some(id) = completed_id {
            self.emit(ProgressiveEvent::SectionCompleted { section_id: id });
        }
    }

    /// 降级处理：checklist 解析失败
    fn on_checklist_parse_failed(&mut self) -> OutputAction {
        self.retry_count += 1;

        if self.retry_count >= 2 {
            // 两次失败 → 降级为 Staged，直接转发已缓冲内容
            self.strategy = Some(OutputStrategy::Staged { sections: Vec::new() });
            self.state = ProgressiveState::Generating {
                strategy: OutputStrategy::Staged { sections: Vec::new() },
                current_section: Some(0),
                confirmed_decisions: Vec::new(),
            };
            self.emit(ProgressiveEvent::DegradedToStaged {
                reason: "checklist parse failed twice".into(),
            });
            OutputAction::Forward
        } else {
            // 首次：尝试启发式提取
            if let Some(checklist) = heuristic_extract_checklist(&self.output_buffer) {
                self.transition_to_awaiting(checklist);
                OutputAction::Gate
            } else {
                OutputAction::Buffer // 继续等待
            }
        }
    }

    fn emit(&self, event: ProgressiveEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.try_send(event);
        }
    }
}

// ─── 解析辅助 ─────────────────────────────────────────────────────────

/// 尝试从 LLM 输出中解析 Checklist JSON
///
/// 查找 {"type":"checklist"...} 格式的 JSON 块
fn try_parse_checklist(output: &str) -> Option<Checklist> {
    // 查找 JSON 块（可能在 ```json ... ``` 中）
    let json_str = extract_json_block(output)?;

    // 尝试解析
    let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;

    // 验证 type 字段
    if parsed.get("type")?.as_str()? != "checklist" {
        return None;
    }

    // 解析 info_items
    let info_items = parsed.get("info_items")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().enumerate().filter_map(|(i, item)| {
                let category = match item.get("category")?.as_str()? {
                    "info_acquired" => ChecklistCategory::InfoAcquired,
                    "needs_verification" => ChecklistCategory::NeedsVerification,
                    "risk_notice" => ChecklistCategory::RiskNotice,
                    _ => return None,
                };
                Some(ChecklistItem {
                    id: i as u32,
                    category,
                    label: item.get("label")?.as_str()?.to_string(),
                    detail: item.get("detail").and_then(|v| v.as_str()).map(String::from),
                    source: item.get("source").and_then(|v| v.as_str()).map(String::from),
                    response: None,
                })
            }).collect()
        })
        .unwrap_or_default();

    // 解析 decisions
    let decisions = parsed.get("decisions")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().filter_map(|item| {
                let options = item.get("options")
                    .and_then(|v| v.as_array())
                    .map(|opts| {
                        opts.iter().filter_map(|o| {
                            Some(DecisionOption {
                                id: o.get("id")?.as_str()?.to_string(),
                                label: o.get("label")?.as_str()?.to_string(),
                                summary: o.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                rationale: o.get("rationale").and_then(|v| v.as_str()).map(String::from),
                                pros: extract_string_array(o.get("pros")),
                                cons: extract_string_array(o.get("cons")),
                                confidence: o.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.5),
                            })
                        }).collect()
                    })
                    .unwrap_or_default();

                Some(DecisionBlock {
                    id: item.get("id")?.as_u64()? as u32,
                    question: item.get("question")?.as_str()?.to_string(),
                    context: item.get("context").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    options,
                    recommended: item.get("recommended").and_then(|v| v.as_str()).map(String::from),
                    recommendation_reason: item.get("recommendation_reason").and_then(|v| v.as_str()).map(String::from),
                    response: None,
                })
            }).collect()
        })
        .unwrap_or_default();

    let context_digest = parsed.get("context_digest")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let checklist = Checklist {
        info_items,
        decisions,
        context_digest,
        timeout: Some(std::time::Duration::from_secs(300)),
        blocking: true,
    };

    if checklist.is_valid() {
        Some(checklist)
    } else {
        None
    }
}

/// 从输出中提取 JSON 块
fn extract_json_block(output: &str) -> Option<&str> {
    // 先尝试 ```json ... ``` 格式
    if let Some(start) = output.find("```json") {
        let content_start = start + 7; // skip ```json
        if let Some(end) = output[content_start..].find("```") {
            return Some(output[content_start..content_start + end].trim());
        }
    }

    // 再尝试裸 JSON（找 {"type":"checklist"...}）
    if let Some(start) = output.find("{\"type\":\"checklist\"") {
        // 找匹配的闭合大括号
        let mut depth = 0;
        for (i, c) in output[start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&output[start..start + i + 1]);
                    }
                },
                _ => {},
            }
        }
    }

    None
}

/// 启发式提取：从非结构化输出中尝试识别决策点
fn heuristic_extract_checklist(output: &str) -> Option<Checklist> {
    // 简单启发式：查找 numbered list 模式
    let lines: Vec<&str> = output.lines().collect();
    let mut info_items = Vec::new();
    let mut id = 0u32;

    for line in &lines {
        let trimmed = line.trim();
        // 检测 "1." "2." "-" "•" 开头的列表项
        if trimmed.starts_with(|c: char| c.is_ascii_digit()) || trimmed.starts_with('-') || trimmed.starts_with('•') {
            id += 1;
            info_items.push(ChecklistItem {
                id,
                category: ChecklistCategory::NeedsVerification,
                label: trimmed.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == '-' || c == '•' || c == ' ').to_string(),
                detail: None,
                source: Some("heuristic_extract".into()),
                response: None,
            });
        }
    }

    if info_items.len() >= 2 {
        Some(Checklist {
            info_items,
            decisions: Vec::new(),
            context_digest: "Extracted via heuristic (LLM did not follow JSON format)".into(),
            timeout: Some(std::time::Duration::from_secs(300)),
            blocking: true,
        })
    } else {
        None
    }
}

fn extract_string_array(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_high_complexity_profile() -> ComplexityProfile {
        ComplexityProfile {
            score: 0.85,
            dimensions: ComplexityDimensions {
                input_length: 0.5, structural: 0.6, domain_crossing: 0.6,
                decision_density: 0.7, output_scale: 0.8,
                external_dependency: 0.3, precision_requirement: 0.4,
            },
            estimated_output_chars: 4000,
            has_decisions: true,
            needs_external_info: true,
            domain_count: 3,
            assessment_confidence: 0.8,
        }
    }

    #[test]
    fn test_begin_gated() {
        let gate = ProgressiveGate::from_autonomy(AutonomyLevel::Medium);
        let mut ctrl = ProgressiveController::new(gate, AutonomyLevel::Medium, None);
        let profile = make_high_complexity_profile();
        let strategy = ctrl.begin(&profile);
        assert!(matches!(strategy, OutputStrategy::Gated { .. }));
        assert!(matches!(ctrl.current_state(), ProgressiveState::StrategyDecided { .. }));
    }

    #[test]
    fn test_passthrough_flow() {
        let gate = ProgressiveGate::from_autonomy(AutonomyLevel::High);
        let mut ctrl = ProgressiveController::new(gate, AutonomyLevel::High, None);
        let profile = ComplexityProfile {
            score: 0.1,
            dimensions: ComplexityDimensions {
                input_length: 0.0, structural: 0.0, domain_crossing: 0.0,
                decision_density: 0.0, output_scale: 0.0,
                external_dependency: 0.0, precision_requirement: 0.0,
            },
            estimated_output_chars: 200,
            has_decisions: false,
            needs_external_info: false,
            domain_count: 0,
            assessment_confidence: 0.1,
        };
        ctrl.begin(&profile);
        assert!(ctrl.is_passthrough());
        assert_eq!(ctrl.on_output_chunk("hello world"), OutputAction::Forward);
    }

    #[test]
    fn test_checklist_parse() {
        let json = r#"```json
{"type":"checklist","info_items":[{"category":"info_acquired","label":"test info"}],"decisions":[{"id":1,"question":"choose?","context":"ctx","options":[{"id":"a","label":"A","summary":"opt A","confidence":0.8}],"recommended":"a","recommendation_reason":"because"}],"context_digest":"digest"}
```"#;
        let checklist = try_parse_checklist(json).unwrap();
        assert_eq!(checklist.info_items.len(), 1);
        assert_eq!(checklist.decisions.len(), 1);
        assert_eq!(checklist.decisions[0].recommended, Some("a".into()));
    }

    #[test]
    fn test_gated_flow_with_checklist() {
        let gate = ProgressiveGate::from_autonomy(AutonomyLevel::Medium);
        let mut ctrl = ProgressiveController::new(gate, AutonomyLevel::Medium, None);
        let profile = make_high_complexity_profile();
        ctrl.begin(&profile);

        // LLM 输出 checklist JSON
        let checklist_json = r#"{"type":"checklist","info_items":[{"category":"info_acquired","label":"got it"}],"decisions":[],"context_digest":"test"}"#;
        let action = ctrl.on_output_chunk(checklist_json);
        assert_eq!(action, OutputAction::Gate);
        assert!(ctrl.is_blocking());

        // 用户确认
        ctrl.on_confirmation(vec![(0, UserResponse::Confirmed)]);
        assert!(matches!(ctrl.current_state(), ProgressiveState::Generating { .. }));
        assert!(!ctrl.is_blocking());
    }

    #[test]
    fn test_degradation_on_parse_failure() {
        let gate = ProgressiveGate::from_autonomy(AutonomyLevel::Medium);
        let mut ctrl = ProgressiveController::new(gate, AutonomyLevel::Medium, None);
        let profile = make_high_complexity_profile();
        ctrl.begin(&profile);

        // LLM 输出垃圾（非 JSON，无列表）
        let garbage = "x".repeat(10001);
        let action = ctrl.on_output_chunk(&garbage);
        // 第一次超长 → retry_count=1, 启发式也失败 → Buffer
        // 但因为 >10000 触发了 on_checklist_parse_failed，retry=1，heuristic 失败 → Buffer
        assert_eq!(action, OutputAction::Buffer);

        // 再次超长输入触发第二次
        ctrl.output_buffer.clear();
        let garbage2 = "y".repeat(10001);
        let action2 = ctrl.on_output_chunk(&garbage2);
        // retry_count=2 → 降级为 Staged → Forward
        assert_eq!(action2, OutputAction::Forward);
        assert!(matches!(ctrl.current_strategy(), Some(OutputStrategy::Staged { .. })));
    }

    #[test]
    fn test_reconfirm_flow() {
        let gate = ProgressiveGate::from_autonomy(AutonomyLevel::Medium);
        let mut ctrl = ProgressiveController::new(gate, AutonomyLevel::Medium, None);
        let profile = make_high_complexity_profile();
        ctrl.begin(&profile);

        let json = r#"{"type":"checklist","info_items":[{"category":"info_acquired","label":"x"}],"decisions":[],"context_digest":"t"}"#;
        ctrl.on_output_chunk(json);
        ctrl.on_confirmation(vec![(0, UserResponse::Confirmed)]);

        // 进入 Generating 后要求修改
        ctrl.request_reconfirm(vec![0]);
        assert!(matches!(ctrl.current_state(), ProgressiveState::ReconfirmRequested { .. }));

        // 重新确认
        ctrl.on_confirmation(vec![(0, UserResponse::Corrected("new value".into()))]);
        assert!(matches!(ctrl.current_state(), ProgressiveState::Generating { .. }));
    }
}
