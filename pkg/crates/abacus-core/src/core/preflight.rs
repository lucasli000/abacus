//! Preflight — LLM 行动前静默自审
//!
//! ## 场景
//! 无论 Mode 1/2/3，LLM 收到用户请求后、执行任何工具前，
//! 先静默执行一次结构化自审：
//!
//! 1. 意图确认 — 用户真正想要什么？
//! 2. 依赖检查 — 需要哪些前置信息？是否已有？
//! 3. 风险识别 — 有没有破坏性操作、越权风险？
//! 4. 执行方案 — 用什么工具、什么顺序？
//! 5. 验收标准 — 怎么判断做完了？
//!
//! ## 输出
//! PreflightReport — 不展示给用户，注入 system prompt 引导执行。

use serde::{Deserialize, Serialize};

/// 自审报告（注入 system prompt 中段，引导 LLM 执行）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightReport {
    /// 确认的意图
    pub confirmed_intent: String,
    /// 识别到的依赖/前置条件
    pub dependencies: Vec<String>,
    /// 风险项
    pub risks: Vec<String>,
    /// 建议的执行方案摘要
    pub execution_plan: Vec<String>,
    /// 验收标准
    pub acceptance_criteria: Vec<String>,
    /// 自审置信度
    pub confidence: f64,
    /// SelfReview 关注点（由 ComplexityProfile 信号驱动）
    /// 消费方: to_prompt_block() → preflight_block → PromptAssembly Layer 155
    /// 生命周期: setup() 调用 check(complexity=Some) 时填充 → assemble() 消费
    pub self_review_points: Vec<String>,
}

impl Default for PreflightReport {
    fn default() -> Self {
        Self {
            confirmed_intent: String::new(),
            dependencies: Vec::new(),
            risks: Vec::new(),
            execution_plan: Vec::new(),
            acceptance_criteria: Vec::new(),
            confidence: 0.5,
            self_review_points: Vec::new(),
        }
    }
}

impl PreflightReport {
    pub fn is_safe(&self) -> bool {
        self.risks.is_empty() && self.confidence > 0.5
    }

    /// 合并规则自审 + LLM 自审报告。LLM 报告字段优先，风险取并集。
    pub fn merge(rule: PreflightReport, llm: PreflightReport) -> PreflightReport {
        let has_risk = !rule.risks.is_empty() || !llm.risks.is_empty();
        let mut risks = rule.risks;
        for r in llm.risks {
            if !risks.contains(&r) { risks.push(r); }
        }
        let mut dependencies = rule.dependencies;
        for d in llm.dependencies {
            if !dependencies.contains(&d) { dependencies.push(d); }
        }
        let mut review_points = rule.self_review_points;
        for p in llm.self_review_points {
            if !review_points.contains(&p) { review_points.push(p); }
        }
        PreflightReport {
            confirmed_intent: if llm.confirmed_intent.len() > rule.confirmed_intent.len() { llm.confirmed_intent } else { rule.confirmed_intent },
            dependencies,
            risks,
            execution_plan: if llm.execution_plan.is_empty() { rule.execution_plan } else { llm.execution_plan },
            acceptance_criteria: if llm.acceptance_criteria.is_empty() { rule.acceptance_criteria } else { llm.acceptance_criteria },
            confidence: if has_risk { rule.confidence.min(llm.confidence) } else { rule.confidence.max(llm.confidence) },
            self_review_points: review_points,
        }
    }

    /// 渲染为 system prompt 注入文本（不可被 context 压缩）
    ///
    /// ## Phase 3 KV cache 修复：减频
    ///
    /// **门控规则**：仅在以下情况注入（其余返回空字符串，pipeline 跳过注入）：
    ///   - 检测到 `risks` 非空（高风险操作需要警示）
    ///   - 或 `self_review_points` 非空（复杂任务需要 review checklist）
    ///
    /// **删除字段**：`confirmed_intent`/`dependencies`/`execution_plan`/`acceptance_criteria` 不再注入：
    ///   - 它们是 LLM 自己能从 input 推断的信息（重复）
    ///   - 每轮变化破坏 Layer 155 缓存前缀（input-driven）
    ///   - 真正有价值的 `risks`（破坏性操作警示）和 `self_review_points` 保留
    ///
    /// 多数普通 query（无破坏性、低复杂度）→ 返回空 → 跳过注入 → 不破 cache
    pub fn to_prompt_block(&self) -> String {
        // 减频门控：无风险且无 review points → 完全不注入
        if self.risks.is_empty() && self.self_review_points.is_empty() {
            return String::new();
        }

        let mut parts = vec!["## Preflight Analysis (静默自审)".to_string()];

        if !self.risks.is_empty() {
            parts.push(format!("⚠️ 风险: {}", self.risks.join("; ")));
        }

        if !self.self_review_points.is_empty() {
            parts.push("\n## Self-Review Checklist".into());
            parts.push("完成初步回答后，以「挑剔专家」身份逐项检查（有问题则修正并标注 [已修正]，无问题输出 [PASS]）：".into());
            for point in &self.self_review_points {
                parts.push(format!("- {}", point));
            }
        }

        parts.join("\n")
    }
}

/// 轻量自审器（无 LLM 调用，基于规则 + TaskAnalyzer）
///
/// ## 场景
/// 快速检查：意图是否明确、是否有已知风险、工具可用性。
/// 不阻塞执行，仅标记关注点供 LLM 参考。
pub struct PreflightChecker;

impl PreflightChecker {
    /// 执行静默自审，返回自审报告
    /// 执行静默自审，返回自审报告。
    ///
    /// `complexity` 可选：传入时注入 SelfReview 关注点；传 `None` 时行为与旧接口完全一致。
    pub fn check(
        input: &str,
        classification: &crate::core::task_analyzer::TaskKind,
        complexity: Option<&abacus_types::progressive::ComplexityProfile>,
    ) -> PreflightReport {
        let lower = input.to_lowercase();

        // 1. 意图确认
        let confirmed_intent = format!("{:?} — {}", classification, input.chars().take(80).collect::<String>());

        // 2. 依赖检查
        let mut dependencies = Vec::new();
        if !input.contains("path") && !input.contains("file") && !input.contains("路径") && !input.contains("文件")
            && matches!(classification, crate::core::task_analyzer::TaskKind::FileEdit) {
                dependencies.push("未指定文件路径".into());
            }

        // 3. 风险识别（仅标记真正破坏性操作；edit/write/修改 是正常操作不标 risk）
        // 原设计 "修改" 触发 risk → llm_self_review → 每次代码任务多 1-3s + 1024 tokens
        // 收紧：只有不可逆删除/覆盖/清空才标记
        let mut risks = Vec::new();
        let destructive_patterns = ["delete all", "drop table", "truncate", "rm -rf", "format disk",
            "覆盖全部", "清空", "删除所有"];
        if destructive_patterns.iter().any(|p| lower.contains(p)) {
            risks.push("请求包含不可逆破坏性操作".into());
        }

        // 4. 执行方案
        let mut execution_plan = Vec::new();
        execution_plan.push(format!("任务分类: {:?}", classification));
        if matches!(classification, crate::core::task_analyzer::TaskKind::Debugging) {
            execution_plan.push("步骤: 复现→最小化→隔离→修复→验证".into());
        }
        if matches!(classification, crate::core::task_analyzer::TaskKind::CodeWriting) {
            execution_plan.push("步骤: 理解需求→接口设计→实现→测试→审查".into());
        }
        if matches!(classification, crate::core::task_analyzer::TaskKind::Review) {
            execution_plan.push("步骤: 理解变更→逐块审查→汇总发现→评级".into());
        }

        // 5. 验收标准
        let mut acceptance_criteria = Vec::new();
        if matches!(classification, crate::core::task_analyzer::TaskKind::Debugging) {
            acceptance_criteria.push("问题已修复且无回归".into());
        }
        if matches!(classification, crate::core::task_analyzer::TaskKind::CodeWriting) {
            acceptance_criteria.push("编译通过 + 测试通过".into());
        }
        if matches!(classification, crate::core::task_analyzer::TaskKind::Review) {
            acceptance_criteria.push("所有 P0/P1 已标注修复路径".into());
        }

        // 置信度
        let confidence = if risks.is_empty() && dependencies.is_empty() {
            0.85
        } else if risks.len() <= 1 {
            0.65
        } else {
            0.40
        };

        let self_review_points = complexity
            .map(|c| build_self_review_points(c, classification))
            .unwrap_or_default();

        PreflightReport {
            confirmed_intent,
            dependencies,
            risks,
            execution_plan,
            acceptance_criteria,
            confidence,
            self_review_points,
        }
    }
}

/// Build SelfReview focus points driven by ComplexityProfile signals.
///
/// ## 引用
/// 被 `PreflightChecker::check()` 调用（complexity=Some 时）
/// 输出注入 `PreflightReport.self_review_points` → `to_prompt_block()` → Layer 155
///
/// ## 触发条件
/// precision_requirement > 0.5 OR has_decisions OR domain_count ≥ 3
/// OR Architecture / Review / DataAnalysis 任务类型
fn build_self_review_points(
    c: &abacus_types::progressive::ComplexityProfile,
    kind: &crate::core::task_analyzer::TaskKind,
) -> Vec<String> {
    let trigger = c.dimensions.precision_requirement > 0.5
        || c.has_decisions
        || c.domain_count >= 3
        || matches!(
            kind,
            crate::core::task_analyzer::TaskKind::Architecture
                | crate::core::task_analyzer::TaskKind::Review
                | crate::core::task_analyzer::TaskKind::DataAnalysis
        );
    if !trigger {
        return Vec::new();
    }
    let mut points = Vec::new();
    if c.dimensions.precision_requirement > 0.5 {
        points.push("数值/版本/状态断言是否已工具验证，未验证的标注 [training_snapshot]".into());
    }
    if c.has_decisions {
        points.push("推荐方案的核心前提是否成立；如不成立，结论会变成什么".into());
    }
    if c.domain_count >= 3 {
        points.push("跨域衔接处是否存在术语混用或隐含假设".into());
    }
    if c.dimensions.structural > 0.6 {
        points.push("推理链有无跳步（前提→结论缺中间桥接）".into());
    }
    if matches!(
        kind,
        crate::core::task_analyzer::TaskKind::Review
            | crate::core::task_analyzer::TaskKind::Architecture
    ) {
        points.push("有没有遗漏的边界条件或异常路径".into());
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::task_analyzer::{TaskAnalyzer, TaskKind};

    #[test]
    fn test_safe_input() {
        let input = "show me the code in main.rs";
        let classification = TaskAnalyzer::classify(input).kind;
        let report = PreflightChecker::check(input, &classification, None);
        assert!(report.is_safe());
        assert!(report.risks.is_empty());
    }

    #[test]
    fn test_risky_input() {
        let input = "delete all files in /tmp";
        let classification = TaskAnalyzer::classify(input).kind;
        let report = PreflightChecker::check(input, &classification, None);
        assert!(!report.is_safe());
        assert!(!report.risks.is_empty());
    }

    #[test]
    fn test_prompt_block() {
        // Phase 3: 无 risks 且无 self_review_points → 门控为空（不破 cache）
        let safe_input = "fix the compilation error in parser.rs";
        let safe_kind = TaskAnalyzer::classify(safe_input).kind;
        let safe_report = PreflightChecker::check(safe_input, &safe_kind, None);
        assert!(safe_report.to_prompt_block().is_empty(),
            "无风险/无 review_points 时应当返回空块以保护 prefix cache");

        // 有 risks → 渲染 Preflight Analysis 块 + 风险条目
        let risky_input = "delete all files in /tmp";
        let risky_kind = TaskAnalyzer::classify(risky_input).kind;
        let risky_report = PreflightChecker::check(risky_input, &risky_kind, None);
        let block = risky_report.to_prompt_block();
        assert!(block.contains("Preflight Analysis"));
        assert!(block.contains("破坏性操作"));
    }

    #[test]
    fn test_dependency_check() {
        let input = "modify the content";
        let classification = TaskAnalyzer::classify(input).kind;
        let report = PreflightChecker::check(input, &classification, None);
        // FileEdit with no path should flag dependency
        assert_eq!(classification, TaskKind::FileEdit);
        assert!(!report.dependencies.is_empty());
    }
}
