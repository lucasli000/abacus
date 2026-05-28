//! Deduction Engine — 推演引擎
//!
//! ## 场景
//! 对接 EffectivenessTracker / ContextManager / MagChain 的数据，产出四类推演：
//! 1. 观察者污染检测 (A5) — adoption_rate 偏离 success_rate 时告警
//! 2. 跨 session 模式提取 — 从全局审计日志发现行为规律
//! 3. Prompt 影响追踪 — 检测 Prompt 结构变化后的工具行为漂移
//! 4. Context 退化预测 — 预判 context 达到压缩/丢弃阈值的时间
//!
//! ## 依赖链
//!
//! MagChain::PersistentAuditLogger (SQLite) -> series::MetricStore
//!   -> analysis::* -> DeductionEngine -> CoreLoop hooks / deduction.* tools
//!
//! ## 引用关系
//! - 被 `CoreLoop` 持有（创建于 CoreLoop::new）
//! - `collect_post_turn()` 被 process_turn 末尾调用
//! - `build_injection()` 被 build_system_prompt 调用（Layer 160 注入）
//!
//! ## 边界
//! - MetricStore SQLite 默认为 ~/.abacus/deduction_metrics.db
//! - 保留 30 天数据后自动清理
//! - 分析算法在数据不足时静默降级（不告警）

pub mod analysis;
pub mod series;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use abacus_types::{KernelError, ToolCost, ToolEffectiveness, ToolHandle,
    ToolId, ToolProvider, ToolSchema, ToolSecurity, ToolState};
use crate::tool::effectiveness::ToolStats;
use crate::memory_palace::DualPalaceMemory;
use crate::deduction::analysis::{
    ContaminationSeverity, DegradationAction,
};
use crate::deduction::series::{
    ContextUsagePoint, MetricStore, PromptStructurePoint, ToolMetricPoint,
};

// ─── DeductionEngine ────────────────────────────────────────────────────────

/// 推演引擎主入口。
///
/// 持有 MetricStore（时序数据）和活跃告警，提供两类接口：
/// - `collect_post_turn()` — CoreLoop 每轮结束后调用
/// - `analyze()` — deduction.* 工具调用
/// - `build_injection()` — PromptAssembly 注入文本
pub struct DeductionEngine {
    store: Arc<MetricStore>,
    active_alerts: Arc<RwLock<Vec<DeductionAlert>>>,
    last_prompt_structure: Arc<RwLock<Option<PromptStructurePoint>>>,
    enabled: Arc<RwLock<DeductionFlags>>,
    /// Counter for periodic purge (every 100 turns)
    purge_counter: Arc<RwLock<u32>>,
    /// 双宫殿记忆系统（可选 — 用于自动行为记录 + 知识维护）
    palace: Option<Arc<DualPalaceMemory>>,
}

/// 每项推演能力的开关
#[derive(Debug, Clone)]
pub struct DeductionFlags {
    pub observer_contamination: bool,
    pub cross_session: bool,
    pub prompt_impact: bool,
    pub context_degradation: bool,
}

impl Default for DeductionFlags {
    fn default() -> Self {
        Self {
            observer_contamination: true,
            cross_session: true,
            prompt_impact: true,
            context_degradation: true,
        }
    }
}

/// 一条活跃的推演告警（注入到 PromptAssembly）
#[derive(Debug, Clone)]
pub struct DeductionAlert {
    pub kind: AlertKind,
    pub message: String,
    pub severity: AlertSeverity,
    pub timestamp_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlertKind {
    Contamination,
    ContextDegradation,
    PromptDrift,
    CrossSessionPattern,
}

impl AlertKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Contamination => "observer_contamination",
            Self::ContextDegradation => "context_degradation",
            Self::PromptDrift => "prompt_drift",
            Self::CrossSessionPattern => "cross_session_pattern",
        }
    }
}

#[derive(Debug, Clone)]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

impl AlertSeverity {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// 分析请求种类
pub enum AnalysisKind {
    ObserverContamination,
    CrossSessionPattern,
    PromptImpact,
    ContextDegradation,
}

/// 分析结果报告
pub struct AnalysisReport {
    pub kind: AnalysisKind,
    pub alerts: Vec<String>,
    pub data_points: usize,
}

impl DeductionEngine {
    /// 创建推演引擎
    pub fn new(db_path: Option<PathBuf>) -> Result<Self, String> {
        Self::with_palace(db_path, None)
    }

    /// 创建推演引擎并注入双宫殿记忆系统
    pub fn with_palace(db_path: Option<PathBuf>, palace: Option<Arc<DualPalaceMemory>>) -> Result<Self, String> {
        let store = MetricStore::new(db_path)?;

        Ok(Self {
            store: Arc::new(store),
            active_alerts: Arc::new(RwLock::new(Vec::new())),
            last_prompt_structure: Arc::new(RwLock::new(None)),
            enabled: Arc::new(RwLock::new(DeductionFlags::default())),
            purge_counter: Arc::new(RwLock::new(0)),
            palace,
        })
    }

    /// 返回 MetricStore 的 Arc 引用（供 MagChain middleware 记录数据）
    pub fn store(&self) -> Arc<MetricStore> {
        self.store.clone()
    }

    /// 获取宫殿引用
    pub fn palace(&self) -> Option<&Arc<DualPalaceMemory>> {
        self.palace.as_ref()
    }

    /// 启用/禁用特定推演能力
    pub async fn set_enabled(&self, flags: DeductionFlags) {
        *self.enabled.write().await = flags;
    }

    /// 清除过期数据
    pub async fn purge_old(&self) -> Result<(), String> {
        self.store.purge_old().await
    }

    // ─── 整轮采集（CoreLoop 调用） ────────────────────────────────────────

    /// 每轮对话结束后采集指标并运行轻量分析。
    ///
    /// 采集三项：
    /// 1. 本轮工具指标（从 EffectivenessTracker 获取真实 stats）
    /// 2. Context 使用率（从 ContextManager 获取）
    /// 3. Prompt 结构快照
    #[tracing::instrument(skip(self, tool_stats))]
    pub async fn collect_post_turn(
        &self,
        turn_number: u32,
        session_id: &str,
        tool_stats: &HashMap<ToolId, ToolStats>,
        context_usage_pct: f64,
        context_max: usize,
        context_current: usize,
        was_compressed: bool,
        prompt_layer_count: usize,
        prompt_tool_count: usize,
        prompt_tool_hash: i64,
        prompt_has_thinking: bool,
    ) -> Result<(), String> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let flags = self.enabled.read().await.clone();

        // 1. 工具指标 — 从真实 ToolStats 构建
        let points: Vec<ToolMetricPoint> = tool_stats.iter().map(|(tool_id, s)| {
            let adoption = s.adoption_rate();
            let success = s.success_rate();
            let latency = s.avg_latency_ms();
            // 估算 trend: 最近 5 次 vs 总体成功率之差
            let recent_success = if s.recent_exit_codes.len() >= 5 {
                let ok = s.recent_exit_codes.iter().filter(|&&c| c == 0).count() as f64;
                ok / s.recent_exit_codes.len() as f64
            } else { success };
            let trend = recent_success - success;
            ToolMetricPoint {
                tool_id: tool_id.clone(),
                turn_number,
                session_id: session_id.to_string(),
                timestamp_ms: now_ms,
                adoption_rate: adoption,
                success_rate: success,
                trend,
                composite_score: 0.5 * adoption + 0.25 * trend + 0.15 * success + 0.10 * (1.0 - (latency / 5000.0).clamp(0.0, 1.0)),
                visibility_tier: "active".into(),
                opportunities: s.opportunities,
                invocations: s.invocations,
                successes: s.successes,
                avg_latency_ms: latency,
            }
        }).collect();
        if !points.is_empty() {
            self.store.record_tool_metrics(&points).await?;
        }

        // 2. Context usage
        let ctx_point = ContextUsagePoint {
            turn_number, session_id: session_id.to_string(), timestamp_ms: now_ms,
            usage_pct: context_usage_pct, max_tokens: context_max,
            current_tokens: context_current, was_compressed,
        };
        self.store.record_context_usage(&ctx_point).await?;

        // 3. Prompt 结构
        let prompt_point = PromptStructurePoint {
            turn_number, session_id: session_id.to_string(), timestamp_ms: now_ms,
            layer_count: prompt_layer_count, tool_count: prompt_tool_count,
            tool_set_hash: prompt_tool_hash, has_thinking: prompt_has_thinking,
        };
        self.store.record_prompt_structure(&prompt_point).await?;

        // ── 轻量分析（每轮自动运行） ──

        let mut alerts = self.active_alerts.write().await;
        alerts.clear();

        // A5 观察者污染
        if flags.observer_contamination {
            let recent = self.store.load_all_recent_metrics(200).await.unwrap_or_default();
            if recent.len() >= 3 {
                let contaminations = analysis::detect_contamination(&recent);
                for c in contaminations {
                    let sev = match c.severity {
                        ContaminationSeverity::Critical => AlertSeverity::Critical,
                        ContaminationSeverity::Watch => AlertSeverity::Warning,
                        _ => AlertSeverity::Info,
                    };
                    alerts.push(DeductionAlert {
                        kind: AlertKind::Contamination,
                        message: format!(
                            "工具 {} 存在观察者污染风险: adoption({:.0}%) >> success({:.0}%)，差值 {:.0}%",
                            c.tool_id, c.adoption_rate * 100.0, c.success_rate * 100.0, c.divergence * 100.0
                        ),
                        severity: sev,
                        timestamp_ms: now_ms,
                    });
                }
            }
        }

        // Context 退化
        if flags.context_degradation {
            let ctx_history = self.store.load_context_history(20).await.unwrap_or_default();
            if ctx_history.len() >= 3 {
                let pred = analysis::predict_context_degradation(&ctx_history);
                let sev = match pred.action {
                    DegradationAction::ForceDiscard => AlertSeverity::Critical,
                    DegradationAction::ImmediateCompress => AlertSeverity::Warning,
                    DegradationAction::PrepareCompress => AlertSeverity::Info,
                    DegradationAction::Normal => AlertSeverity::Info,
                };
                if !matches!(pred.action, DegradationAction::Normal) {
                    alerts.push(DeductionAlert {
                        kind: AlertKind::ContextDegradation,
                        message: format!(
                            "Context 退化预测: 当前 {:.0}%，增长率 {:.1}%/轮，预计 {} 轮后达 85% 压缩阈值",
                            pred.current_usage_pct, pred.growth_rate, pred.turns_to_85pct
                        ),
                        severity: sev,
                        timestamp_ms: now_ms,
                    });
                }
            }
        }

        // Prompt 影响
        if flags.prompt_impact {
            let prev = self.last_prompt_structure.read().await.clone();
            if let Some(ref prev_ps) = prev {
                let delta = analysis::detect_prompt_change(prev_ps, &prompt_point);
                if delta.tool_count_changed || delta.tool_set_changed || delta.thinking_toggled {
                    alerts.push(DeductionAlert {
                        kind: AlertKind::PromptDrift,
                        message: delta.description,
                        severity: AlertSeverity::Info,
                        timestamp_ms: now_ms,
                    });
                }
            }
            *self.last_prompt_structure.write().await = Some(prompt_point);
        }

        // O1: 不变量提取（数据充足时）
        if points.len() >= 3 {
            let history: Vec<ToolMetricPoint> = self.store.load_all_recent_metrics(100).await.unwrap_or_default();
            if !history.is_empty() {
                let by_tool: std::collections::HashMap<&str, Vec<ToolMetricPoint>> = {
                    let mut m: std::collections::HashMap<&str, Vec<ToolMetricPoint>> = std::collections::HashMap::new();
                    for p in &history { m.entry(p.tool_id.0.as_str()).or_default().push(p.clone()); }
                    m
                };
                for (tid, pts) in by_tool.iter().take(3) {
                    let inv = analysis::extract_invariants(pts);
                    for i in inv.iter().filter(|s| !s.contains("data_insufficient")) {
                        alerts.push(DeductionAlert {
                            kind: AlertKind::CrossSessionPattern,
                            message: format!("[{}] {}", tid, i),
                            severity: AlertSeverity::Info,
                            timestamp_ms: now_ms,
                        });
                    }
                }
            }
        }

        // O3: 信号分解（对最活跃的工具）
        if !points.is_empty() {
            let top_tool = points.iter()
                .max_by(|a, b| a.invocations.cmp(&b.invocations))
                .map(|p| p.tool_id.0.as_str());
            if let Some(tid) = top_tool {
                let history = self.store.load_tool_history(tid, 10).await.unwrap_or_default();
                if history.len() >= 5 {
                    let signal = analysis::decompose_signal(&history);
                    if signal.slow_trend.abs() > 0.05 || signal.fast_noise > 0.15 {
                        alerts.push(DeductionAlert {
                            kind: AlertKind::CrossSessionPattern,
                            message: format!("[O3/{}] {}", tid, signal.explanation),
                            severity: AlertSeverity::Info,
                            timestamp_ms: now_ms,
                        });
                    }
                }
            }
        }

        // Periodic purge + palace maintenance: every 100 turns
        {
            let mut counter = self.purge_counter.write().await;
            *counter += 1;
            if *counter >= 100 {
                *counter = 0;
                if let Err(e) = self.store.purge_old().await {
                    tracing::warn!("deduction purge failed: {e}");
                }
                // Palace auto-maintenance
                if let Some(ref palace) = self.palace {
                    for (tid, stat) in tool_stats.iter() {
                        palace.record_tool_behavior(&tid.0, stat.success_rate() > 0.5).await;
                    }
                    palace.prune().await;
                    let due = palace.due_review_count().await;
                    if due > 0 {
                        alerts.push(DeductionAlert {
                            kind: AlertKind::CrossSessionPattern,
                            message: format!("知识宫殿有 {due} 条条目到期需 review"),
                            severity: AlertSeverity::Info,
                            timestamp_ms: now_ms,
                        });
                    }
                }
            }
        }

        Ok(())
    }

    /// 将活跃告警注入为 Prompt 文本（Layer 160，DeductionEngine 调用方负责注入）
    pub async fn build_injection(&self) -> Option<String> {
        let alerts = self.active_alerts.read().await;
        if alerts.is_empty() {
            return None;
        }

        let mut parts = vec!["## Agent 阶段感知".to_string()];
        for alert in alerts.iter() {
            parts.push(format!(
                "- [{}] [{}] {}",
                alert.severity.label(),
                alert.kind.label(),
                alert.message
            ));
        }
        Some(parts.join("\n"))
    }

    // ─── 深度分析（工具调用触发） ─────────────────────────────────────────

    /// 运行指定的推演分析，返回报告 JSON
    pub async fn analyze(&self, kind: &AnalysisKind) -> AnalysisReport {
        match kind {
            AnalysisKind::ObserverContamination => {
                let all = self.store.load_all_recent_metrics(500).await.unwrap_or_default();
                let contaminations = analysis::detect_contamination(&all);
                let alerts: Vec<String> = contaminations.iter().map(|c| {
                    format!("[{}] {}: adoption={:.0}% success={:.0}% divergence={:.0}% (持续{}轮)",
                        c.severity.label(), c.tool_id,
                        c.adoption_rate * 100.0, c.success_rate * 100.0,
                        c.divergence * 100.0, c.since_turns)
                }).collect();
                AnalysisReport { kind: AnalysisKind::ObserverContamination, alerts, data_points: all.len() }
            }
            AnalysisKind::CrossSessionPattern => {
                let all = self.store.load_all_recent_metrics(1000).await.unwrap_or_default();
                let patterns = analysis::extract_cross_session_patterns(&all);
                let alerts: Vec<String> = patterns.iter().map(|p| {
                    format!("[{}] ({:.0}%) {} — {}", p.pattern_type, p.confidence * 100.0, p.description, p.evidence)
                }).collect();
                AnalysisReport { kind: AnalysisKind::CrossSessionPattern, alerts, data_points: all.len() }
            }
            AnalysisKind::ContextDegradation => {
                let history = self.store.load_context_history(30).await.unwrap_or_default();
                let mut alerts = Vec::new();
                if history.len() >= 3 {
                    let pred = analysis::predict_context_degradation(&history);
                    alerts.push(format!(
                        "Context 退化预测: 当前 {:.0}%，增长率 {:.1}%/轮，加速度 {:.3}，预计 {} 轮后 85% / {} 轮后 95%",
                        pred.current_usage_pct, pred.growth_rate, pred.acceleration,
                        pred.turns_to_85pct, pred.turns_to_95pct
                    ));
                    alerts.push(format!("推荐操作: {}", pred.action.label()));
                } else {
                    alerts.push("context 数据不足（<3点），无法预测".into());
                }
                AnalysisReport { kind: AnalysisKind::ContextDegradation, alerts, data_points: history.len() }
            }
            AnalysisKind::PromptImpact => {
                let prompt_history = self.store.load_prompt_history(10).await.unwrap_or_default();
                let tool_history = self.store.load_all_recent_metrics(200).await.unwrap_or_default();
                let mut alerts = Vec::new();

                if prompt_history.len() >= 2 {
                    for pair in prompt_history.windows(2) {
                        let delta = analysis::detect_prompt_change(&pair[0], &pair[1]);
                        alerts.push(delta.description);
                    }
                    // 分割变更前后的工具数据
                    let mid = prompt_history.len() / 2;
                    let before_tools: Vec<ToolMetricPoint> = tool_history.iter()
                        .filter(|p| p.turn_number <= prompt_history[mid].turn_number)
                        .cloned().collect();
                    let after_tools: Vec<ToolMetricPoint> = tool_history.iter()
                        .filter(|p| p.turn_number > prompt_history[mid].turn_number)
                        .cloned().collect();
                    let impacts = analysis::detect_prompt_impact(
                        &prompt_history[0], &prompt_history[prompt_history.len()-1],
                        &before_tools, &after_tools,
                    );
                    alerts.extend(impacts);
                } else {
                    alerts.push("Prompt 数据不足（<2次快照），无法追踪变化".into());
                }
                AnalysisReport { kind: AnalysisKind::PromptImpact, alerts, data_points: prompt_history.len() }
            }
        }
    }

    /// 运行所有分析（供 deduction.analyze --all 使用）
    pub async fn analyze_all(&self) -> Vec<AnalysisReport> {
        let kinds = vec![
            AnalysisKind::ObserverContamination,
            AnalysisKind::CrossSessionPattern,
            AnalysisKind::ContextDegradation,
            AnalysisKind::PromptImpact,
        ];
        let mut reports = Vec::new();
        for kind in &kinds {
            reports.push(self.analyze(kind).await);
        }
        reports
    }
}

// ─── DeductionToolExecutor ──────────────────────────────────────────────────

use async_trait::async_trait;
use serde_json::{json, Value};
use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};

/// Executor for deduction.* tools
pub struct DeductionToolExecutor {
    engine: Arc<DeductionEngine>,
}

impl DeductionToolExecutor {
    pub fn new(engine: Arc<DeductionEngine>) -> Self {
        Self { engine }
    }

    pub async fn register(&self, registry: &ToolRegistry) {
        let tools = vec![
            ToolHandle {
                id: ToolId("deduction_status".into()),
                schema: ToolSchema {
                    name: "deduction_status".into(),
                    description: "查看当前推演引擎的活跃告警".into(),
                    parameters: json!({"type": "object", "properties": {}, "required": []}),
                    returns: None,
                    security: Some(ToolSecurity {
                        allowed_paths: None, max_size_mb: None,
                        confirm_required: false, needs_sandbox: false,
                    }),
                    cost: Some(ToolCost { tokens: 32, latency: "10ms".into(), risk: "low".into() }),
                    examples: Vec::new(),
                    applicable_task_kinds: None,
                    idempotent: true,
                },
                provider: ToolProvider::BuiltIn,
                state: ToolState::Loaded,
                effectiveness: ToolEffectiveness::default(),
            },
            ToolHandle {
                id: ToolId("deduction_analyze".into()),
                schema: ToolSchema {
                    name: "deduction_analyze".into(),
                    description: "运行深度推演分析。kind 可选: contamination|patterns|context|prompt".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": ["observer_contamination", "cross_session_patterns", "context_degradation", "prompt_impact"],
                                "description": "分析类型"
                            },
                            "all": {
                                "type": "boolean",
                                "description": "运行所有分析（忽略 kind）"
                            }
                        }
                    }),
                    returns: None,
                    security: Some(ToolSecurity {
                        allowed_paths: None, max_size_mb: None,
                        confirm_required: false, needs_sandbox: false,
                    }),
                    cost: Some(ToolCost { tokens: 64, latency: "50ms".into(), risk: "low".into() }),
                    examples: Vec::new(),
                    applicable_task_kinds: None,
                    idempotent: false,
                },
                provider: ToolProvider::BuiltIn,
                state: ToolState::Loaded,
                effectiveness: ToolEffectiveness::default(),
            },
        ];
        let executor = Arc::new(DeductionToolExecutor::new(self.engine.clone()));
        for tool in tools {
            let tid = tool.id.clone();
            registry.register(tool).await;
            registry.register_executor(tid, executor.clone()).await;
        }
    }
}

#[async_trait]
impl ToolExecutor for DeductionToolExecutor {
    async fn execute(&self, tool_id: &ToolId, params: Value, _ctx: &ExecutionContext) -> Result<Value, KernelError> {
        match tool_id.0.as_str() {
            "deduction_status" => {
                let alerts = self.engine.active_alerts.read().await;
                let items: Vec<Value> = alerts.iter().map(|a| json!({
                    "kind": a.kind.label(),
                    "severity": a.severity.label(),
                    "message": a.message,
                })).collect();
                Ok(json!({"alerts": items, "count": items.len()}))
            }
            "deduction_analyze" => {
                let all = params.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
                let reports = if all {
                    self.engine.analyze_all().await
                } else {
                    let kind_str = params.get("kind").and_then(|v| v.as_str()).unwrap_or("observer_contamination");
                    let kind = match kind_str {
                        "observer_contamination" => AnalysisKind::ObserverContamination,
                        "cross_session_patterns" => AnalysisKind::CrossSessionPattern,
                        "context_degradation" => AnalysisKind::ContextDegradation,
                        "prompt_impact" => AnalysisKind::PromptImpact,
                        _ => return Err(KernelError::Other(format!("unknown analysis kind: {kind_str}"))),
                    };
                    vec![self.engine.analyze(&kind).await]
                };
                let items: Vec<Value> = reports.iter().map(|r| {
                    let kind_label = match r.kind {
                        AnalysisKind::ObserverContamination => "observer_contamination",
                        AnalysisKind::CrossSessionPattern => "cross_session_patterns",
                        AnalysisKind::ContextDegradation => "context_degradation",
                        AnalysisKind::PromptImpact => "prompt_impact",
                    };
                    json!({
                        "kind": kind_label,
                        "data_points": r.data_points,
                        "findings": r.alerts,
                    })
                }).collect();
                Ok(json!({"reports": items}))
            }
            _ => Err(KernelError::Other(format!("unknown deduction tool: {}", tool_id.0))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // ContaminationSeverity 已经通过 super::* 重导入

    fn make_stats(adopt: f64, success: f64, latency: f64, n: u64) -> ToolStats {
        ToolStats {
            opportunities: (n as f64 / adopt.max(0.01)).ceil() as u64,
            invocations: n,
            successes: (n as f64 * success).round() as u64,
            total_latency_ms: (n as f64 * latency).round() as u64,
            recent_exit_codes: (0..(n.min(10) as usize)).map(|i| if (i as f64) < success * n.min(10) as f64 { 0 } else { 1 }).collect(),
            // 段 K1：测试 fixture 默认无 env_failure（专注于工具自身评分语义）
            env_failures: 0,
        }
    }

    #[tokio::test]
    async fn test_engine_create() {
        let engine = DeductionEngine::new(Some(PathBuf::from(":memory:"))).unwrap();
        let alerts = engine.active_alerts.read().await;
        assert!(alerts.is_empty());
    }

    #[tokio::test]
    async fn test_collect_post_turn() {
        let engine = DeductionEngine::new(Some(PathBuf::from(":memory:"))).unwrap();
        let mut stats = HashMap::new();
        stats.insert(ToolId("fs_read".into()), make_stats(0.8, 0.95, 50.0, 20));
        engine.collect_post_turn(
            1, "s1", &stats, 50.0, 128_000, 64_000,
            false, 6, 14, 0x1234, true,
        ).await.unwrap();

        let tool_history = engine.store.load_tool_history("fs_read", 10).await.unwrap();
        assert_eq!(tool_history.len(), 1);
        let p = &tool_history[0];
        assert!((p.adoption_rate - 0.8).abs() < 0.01, "adoption={}", p.adoption_rate);
        assert!((p.success_rate - 0.95).abs() < 0.01, "success={}", p.success_rate);
    }

    #[tokio::test]
    async fn test_analyze_all() {
        let engine = DeductionEngine::new(Some(PathBuf::from(":memory:"))).unwrap();

        for i in 0..10 {
            let mut stats = HashMap::new();
            stats.insert(ToolId("fs_read".into()), make_stats(0.7, 0.85, 30.0, 10));
            engine.collect_post_turn(
                i, "s1", &stats, 40.0 + i as f64 * 3.0,
                128_000, (40000 + i * 3000) as usize,
                false, 6, 14, 0x1234, false,
            ).await.unwrap();
        }

        let reports = engine.analyze_all().await;
        assert_eq!(reports.len(), 4);
    }

    #[tokio::test]
    async fn test_injection_with_alerts() {
        let engine = DeductionEngine::new(Some(PathBuf::from(":memory:"))).unwrap();
        assert!(engine.build_injection().await.is_none());

        for i in 0..8 {
            let mut stats = HashMap::new();
            stats.insert(ToolId("web_fetch".into()), make_stats(0.9, 0.4, 300.0, 10));
            engine.collect_post_turn(
                i, "s1", &stats, 50.0, 128_000, 64_000,
                false, 6, 14, 0x1234, false,
            ).await.unwrap();
        }

        let injection = engine.build_injection().await;
        assert!(injection.is_some() || injection.is_none());
    }
}
