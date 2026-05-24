//! User input intent classification
//!
//! Provides [`TaskAnalyzer`] which classifies user input into 12 task categories
//! using keyword + regex heuristics with zero external dependencies.
//!
//! ## Usage
//!
//! ```
//! use abacus_core::core::task_analyzer::{TaskAnalyzer, TaskKind};
//! let result = TaskAnalyzer::classify("implement a parse function");
//! assert_eq!(result.kind, TaskKind::CodeWriting);
//! ```
//!
//! ## Classification Approach
//!
//! Each task kind has a list of trigger keywords and a base confidence score.
//! The classifier selects the kind with the highest confidence after applying
//! a keyword match ratio bonus (capped at 0.98).
//!
//! ## Output
//!
//! - [`TaskClassification`]: kind + confidence + domains
//! - [`TaskAnalyzer::classify_with_domains`]: convenience for (kind, domains) tuple
//!
//! ## Known Limitations
//!
//! - Keyword-based: ambiguous inputs ("search the codebase for bugs") may match
//!   multiple kinds — the highest confidence wins
//! - No semantic understanding: "review" in "review the code" vs "review the
//!   literature" both match Review kind

use abacus_types::progressive::{ComplexityDimensions, ComplexityProfile};
use serde::{Deserialize, Serialize};

/// Task category for user intent.
///
/// Maps to Expert binding, abacusbr sub-scene matching, and tool selection hints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TaskKind {
    /// Reading/understanding existing code
    CodeReading,
    /// Writing new code or features
    CodeWriting,
    /// Searching the web or external sources
    WebSearch,
    /// Editing files or directories
    FileEdit,
    /// Data analysis and statistics
    DataAnalysis,
    /// Mathematical calculations
    Mathematics,
    /// Language translation and grammar
    Linguistics,
    /// General conversation
    GeneralChat,
    /// Debugging and fixing issues
    Debugging,
    /// Architecture and design decisions
    Architecture,
    /// Code review and quality checks
    Review,
    /// Knowledge queries and definitions
    KnowledgeQuery,
}

impl TaskKind {
    /// Return the string label (snake_case) for this kind.
    pub fn label(&self) -> &'static str {
        match self {
            TaskKind::CodeReading => "code_reading",
            TaskKind::CodeWriting => "code_writing",
            TaskKind::WebSearch => "web_search",
            TaskKind::FileEdit => "file_edit",
            TaskKind::DataAnalysis => "data_analysis",
            TaskKind::Mathematics => "mathematics",
            TaskKind::Linguistics => "linguistics",
            TaskKind::GeneralChat => "general_chat",
            TaskKind::Debugging => "debugging",
            TaskKind::Architecture => "architecture",
            TaskKind::Review => "review",
            TaskKind::KnowledgeQuery => "knowledge_query",
        }
    }
}

/// Result of task classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskClassification {
    /// The detected task kind
    pub kind: TaskKind,
    /// Confidence score (0.0 - 0.98)
    pub confidence: f64,
    /// Domain tags for this classification
    pub domains: Vec<String>,
}

/// Keyword-based heuristic classifier for user intent.
///
/// Uses a priority-ordered rule set. Each rule defines a base confidence and
/// trigger keywords. The rule with the highest final confidence wins.
pub struct TaskAnalyzer;

impl TaskAnalyzer {
    /// Classify the user input into a single task kind with confidence.
    ///
    /// Returns [`TaskKind::GeneralChat`] with 0.60 confidence if no rules match.
    pub fn classify(input: &str) -> TaskClassification {
        let lower = input.to_lowercase();

        let rules: Vec<(TaskKind, f64, Vec<&str>)> = vec![
            (TaskKind::CodeWriting, 0.85, vec!["implement", "write", "create", "add feature", "new function", "cargo new", "fn "]),
            (TaskKind::CodeReading, 0.80, vec!["read", "show me", "what does", "how does", "explain code", "understand"]),
            (TaskKind::WebSearch, 0.90, vec!["search", "find online", "look up", "google", "browse", "fetch url"]),
            (TaskKind::FileEdit, 0.85, vec!["edit", "modify", "change", "update file", "rename", "move file"]),
            (TaskKind::DataAnalysis, 0.85, vec!["analyze", "statistics", "chart", "plot", "data", "trend", "distribution"]),
            (TaskKind::Mathematics, 0.90, vec!["calculate", "compute", "formula", "equation", "derivative", "integral"]),
            (TaskKind::Linguistics, 0.80, vec!["translate", "rewrite", "grammar", "spelling", "language"]),
            (TaskKind::Debugging, 0.85, vec!["bug", "error", "crash", "fix", "debug", "issue", "broken"]),
            (TaskKind::Architecture, 0.80, vec!["architecture", "design", "structure", "module", "component"]),
            (TaskKind::Review, 0.85, vec!["review", "audit", "check", "inspect", "quality"]),
            (TaskKind::KnowledgeQuery, 0.80, vec!["what is", "define", "explain", "how to", "tutorial", "guide"]),
        ];

        let mut best: Option<TaskClassification> = None;

        for (kind, base_conf, keywords) in &rules {
            let matched: usize = keywords.iter().filter(|k| lower.contains(*k)).count();
            if matched == 0 { continue; }
            let ratio = matched as f64 / keywords.len() as f64;
            let confidence = (base_conf * (1.0 + ratio)).min(0.98);
            let domains = domain_for_kind(kind);
            let candidate = TaskClassification { kind: kind.clone(), confidence, domains };
            best = match best {
                None => Some(candidate),
                Some(ref prev) if candidate.confidence > prev.confidence => Some(candidate),
                _ => best,
            };
        }

        best.unwrap_or(TaskClassification {
            kind: TaskKind::GeneralChat,
            confidence: 0.60,
            domains: vec!["general".into()],
        })
    }

    /// Classify and return just the (kind, domains) tuple for convenience.
    pub fn classify_with_domains(input: &str) -> (TaskKind, Vec<String>) {
        let result = Self::classify(input);
        (result.kind, result.domains)
    }

    /// 计算完整复杂度剖面（7 维信号 + 合成评分）
    ///
    /// ## 场景
    /// ProgressiveGate 在每次 LLM 请求前调用，决定输出策略。
    ///
    /// ## 流程
    /// 1. classify() 获取 TaskKind
    /// 2. 7 个信号检测函数各自评分 [0, 1]
    /// 3. 加权合成 + 非线性放大器
    pub fn analyze_complexity(input: &str) -> ComplexityProfile {
        let classification = Self::classify(input);
        let lower = input.to_lowercase();
        let char_count = input.chars().count();
        let eff_len = effective_length(input);

        let d1 = length_signal(eff_len);
        let d2 = structural_signal(&lower);
        let domains_hit = count_domain_hits(&lower);
        let d3 = domain_crossing_signal(domains_hit);
        let d4 = decision_signal(&lower);
        let (d5, estimated_chars) = output_scale_signal(&classification, &lower, char_count);
        let d6 = external_dependency_signal(&lower);
        let d7 = precision_signal(&classification, &lower);

        let dimensions = ComplexityDimensions {
            input_length: d1,
            structural: d2,
            domain_crossing: d3,
            decision_density: d4,
            output_scale: d5,
            external_dependency: d6,
            precision_requirement: d7,
        };

        let score = composite_score(&dimensions);

        let signals_active = [d1, d2, d3, d4, d5, d6, d7]
            .iter()
            .filter(|&&s| s > 0.05)
            .count();

        ComplexityProfile {
            score,
            dimensions,
            estimated_output_chars: estimated_chars,
            has_decisions: d4 > 0.2,
            needs_external_info: d6 > 0.3,
            domain_count: domains_hit as u32,
            assessment_confidence: signals_active as f64 / 7.0,
        }
    }
}

fn domain_for_kind(kind: &TaskKind) -> Vec<String> {
    match kind {
        TaskKind::CodeReading | TaskKind::CodeWriting => vec!["development".into(), "coding".into()],
        TaskKind::WebSearch => vec!["web".into()],
        TaskKind::FileEdit => vec!["development".into()],
        TaskKind::DataAnalysis => vec!["data".into(), "analysis".into()],
        TaskKind::Mathematics => vec!["mathematics".into()],
        TaskKind::Linguistics => vec!["linguistics".into()],
        TaskKind::GeneralChat => vec!["general".into()],
        TaskKind::Debugging => vec!["development".into(), "debugging".into()],
        TaskKind::Architecture => vec!["development".into(), "architecture".into()],
        TaskKind::Review => vec!["development".into(), "review".into()],
        TaskKind::KnowledgeQuery => vec!["knowledge".into(), "general".into()],
    }
}

// ─── 复杂度信号检测函数 ─────────────────────────────────────────────────

/// D1: 输入长度信号（信息密度感知）
///
/// 中文字符信息密度约为英文的 2-3 倍（1 中文字 ≈ 2-3 英文词）。
/// 使用"等效信息单元"：CJK 字符 ×2.5 权重，ASCII 字符 ×1.0 权重。
fn length_signal(char_count: usize) -> f64 {
    // 简化：如果没有传入原始文本，用 char_count 做保守估计
    // 实际调用时可用 effective_length() 替代
    let effective = char_count; // 被 analyze_complexity 中的 effective_length 覆盖
    match effective {
        0..=29 => 0.0,
        30..=99 => (effective - 30) as f64 / 70.0 * 0.3,
        100..=299 => 0.3 + (effective - 100) as f64 / 200.0 * 0.3,
        300..=999 => 0.6 + (effective - 300) as f64 / 700.0 * 0.3,
        _ => 0.95,
    }
}

/// 计算等效信息单元长度（CJK 字符权重 2.5，ASCII 权重 1.0）
fn effective_length(input: &str) -> usize {
    let mut score: f64 = 0.0;
    for c in input.chars() {
        if c > '\u{2E80}' {
            // CJK 范围（含中日韩统一表意字符）
            score += 2.5;
        } else if c.is_alphanumeric() {
            score += 1.0;
        } else {
            score += 0.5; // 标点/空格
        }
    }
    score as usize
}

/// D2: 结构复杂度（多步骤/条件/组合）
fn structural_signal(input: &str) -> f64 {
    let step_markers = [
        "第一", "第二", "第三", "第四", "首先", "然后", "最后", "接着",
        "先", "再", "之后", "完成后", "下一步", "紧接着",
        "first", "second", "third", "then", "finally", "next",
        "step 1", "step 2", "1.", "2.", "3.", "4.", "after that", "once done",
        "一方面", "另一方面",
    ];
    let branch_markers = [
        "如果", "否则", "或者", "要么", "取决于", "视情况", "假如", "万一",
        "if ", "else", "either", "depending", "when ", "whether",
        "条件是", "前提是",
    ];
    let compound_markers = [
        "并且", "同时", "另外", "还需要", "以及", "包括", "涵盖",
        "此外", "除此之外", "不仅", "而且",
        "and also", "additionally", "furthermore", "as well as", "plus",
    ];

    let step_hits = step_markers.iter().filter(|m| input.contains(*m)).count();
    let branch_hits = branch_markers.iter().filter(|m| input.contains(*m)).count();
    let compound_hits = compound_markers.iter().filter(|m| input.contains(*m)).count();

    let step_score = (step_hits as f64 / 3.0).min(1.0);
    let branch_score = (branch_hits as f64 / 2.0).min(1.0);
    let compound_score = (compound_hits as f64 / 2.0).min(1.0);

    (step_score * 0.4 + branch_score * 0.3 + compound_score * 0.3).min(1.0)
}

/// D3: 领域交叉度 — 命中领域数
fn count_domain_hits(input: &str) -> usize {
    let domain_lexicons: &[(&str, &[&str])] = &[
        ("finance", &["财务", "报表", "审批", "预算", "成本", "ROI", "revenue", "finance", "利润", "营收", "现金流", "损益"]),
        ("legal", &["合规", "法律", "条款", "协议", "隐私", "GDPR", "license", "compliance", "监管", "合同", "法规"]),
        ("tech", &["API", "数据库", "服务器", "部署", "微服务", "接口", "架构", "代码", "SDK", "后端", "前端", "中间件"]),
        ("product", &["需求", "PRD", "用户故事", "MVP", "迭代", "功能", "feature", "产品", "用例", "验收标准"]),
        ("data", &["数据", "指标", "报告", "分析", "统计", "趋势", "dashboard", "可视化", "ETL", "数仓", "BI"]),
        ("operations", &["运营", "流程", "SOP", "效率", "自动化", "workflow", "流转", "工单", "审批流"]),
        ("security", &["安全", "加密", "认证", "授权", "审计", "漏洞", "渗透", "防护", "RBAC", "鉴权"]),
        ("design", &["设计", "UI", "UX", "交互", "原型", "线框", "视觉", "组件", "布局", "配色"]),
        ("infra", &["运维", "CI/CD", "容器", "K8s", "Docker", "监控", "告警", "日志", "SRE", "可观测性"]),
        ("trading", &["交易", "下单", "持仓", "风控", "行情", "K线", "撮合", "清算", "结算"]),
    ];

    domain_lexicons.iter()
        .filter(|(_, keywords)| keywords.iter().any(|k| input.contains(k)))
        .count()
}

/// D3 辅助：领域数 → 交叉度分数
fn domain_crossing_signal(domains_hit: usize) -> f64 {
    match domains_hit {
        0 | 1 => 0.0,
        2 => 0.3,
        3 => 0.6,
        4 => 0.8,
        _ => 0.95,
    }
}

/// D4: 决策密度
fn decision_signal(input: &str) -> f64 {
    let markers = [
        "选择", "方案", "对比", "比较", "权衡", "取舍",
        "choose", "compare", "trade-off", "option",
        "应该", "建议", "推荐", "最好", "最优", "还是",
        "should", "recommend", "best", " or ",
        "策略", "路径", "方向", "优先", "先做",
        "strategy", "approach", "priority",
    ];

    let hits = markers.iter().filter(|m| input.contains(*m)).count();
    match hits {
        0 => 0.0,
        1 => 0.2,
        2 => 0.4,
        3 => 0.6,
        4 => 0.8,
        _ => 0.95,
    }
}

/// D5: 输出规模预估 — 返回 (归一化分数, 预估字符数)
///
/// 使用 effective_length 来感知中文输入的信息密度
fn output_scale_signal(classification: &TaskClassification, input: &str, _char_count: usize) -> (f64, u32) {
    let base_chars: u32 = match classification.kind {
        TaskKind::GeneralChat => 400,
        TaskKind::CodeReading => 800,
        TaskKind::KnowledgeQuery => 1000,
        TaskKind::Debugging => 1200,
        TaskKind::CodeWriting => 1600,
        TaskKind::FileEdit => 800,
        TaskKind::WebSearch => 600,
        TaskKind::DataAnalysis => 2000,
        TaskKind::Mathematics => 1200,
        TaskKind::Linguistics => 1000,
        TaskKind::Architecture => 3000,
        TaskKind::Review => 2400,
    };

    // 文档类标记（强信号：输出量翻倍）
    let doc_markers = [
        "文档", "报告", "PRD", "设计文档", "方案", "规范", "SOP",
        "specification", "document", "report", "write up",
        "手册", "指南", "白皮书", "分析报告",
    ];
    let doc_mult = if doc_markers.iter().any(|m| input.contains(m)) { 2.5 } else { 1.0 };

    // 范围词（中信号：输出量增加）
    let scope_markers = [
        "全面", "完整", "详细", "所有", "全部", "深入", "系统性",
        "comprehensive", "complete", "full", "detailed", "thorough",
    ];
    let scope_mult = if scope_markers.iter().any(|m| input.contains(m)) { 1.8 } else { 1.0 };

    // 列举词（要求多个输出项）
    let list_markers = ["列出", "列举", "枚举", "哪些", "有哪些", "所有的", "list", "enumerate"];
    let list_mult = if list_markers.iter().any(|m| input.contains(m)) { 1.5 } else { 1.0 };

    // 输入长度修正（用 effective_length 而非 raw char_count）
    let eff_len = effective_length(input);
    let length_factor = 1.0 + (eff_len as f64 / 300.0).min(2.0);

    let estimated = (base_chars as f64 * doc_mult * scope_mult * list_mult * length_factor) as u32;

    let score = match estimated {
        0..=999 => 0.1,
        1000..=2999 => 0.1 + (estimated - 1000) as f64 / 2000.0 * 0.4,
        3000..=7999 => 0.5 + (estimated - 3000) as f64 / 5000.0 * 0.4,
        _ => 0.95,
    };

    (score, estimated)
}

/// D6: 外部依赖度
fn external_dependency_signal(input: &str) -> f64 {
    let markers = [
        "最新", "当前", "现在", "目前", "latest", "current", "today",
        "具体", "你的", "贵公司", "实际", "your", "specific", "actual",
        "线上", "生产", "数据库", "第三方", "production", "database", "third-party",
        "假设", "假定", "如果是", "假如",
    ];

    let hits = markers.iter().filter(|m| input.contains(*m)).count();
    (hits as f64 / 3.0).min(1.0)
}

/// D7: 精确度要求
fn precision_signal(classification: &TaskClassification, input: &str) -> f64 {
    let domain_base = match classification.kind {
        TaskKind::Mathematics => 0.8,
        TaskKind::DataAnalysis => 0.5,
        _ => 0.0,
    };

    let precision_domains = [
        "金融", "财税", "合规", "法律", "审计", "交易",
        "finance", "tax", "compliance", "legal", "audit", "trading",
        "医疗", "药品", "安全", "加密",
    ];
    let domain_hit = if precision_domains.iter().any(|m| input.contains(m)) { 0.6 } else { 0.0 };

    let precision_markers = ["精确", "准确", "不能有误", "零容错", "必须正确", "exact", "accurate", "precise"];
    let explicit_hit = if precision_markers.iter().any(|m| input.contains(m)) { 0.3 } else { 0.0 };

    let total: f64 = domain_base + domain_hit + explicit_hit;
    total.min(1.0)
}

/// 加权合成最终复杂度分数
///
/// ## 权重设计
/// D2(0.22) + D5(0.22) + D4(0.20) = 0.64（三强信号占主导）
/// D1(0.08) + D3(0.12) + D6(0.08) + D7(0.08) = 0.36（补充信号）
///
/// ## 非线性放大器
/// 3+ 维度同时 > 0.5 时乘性放大，防止单维度假阳性。
fn composite_score(d: &ComplexityDimensions) -> f64 {
    const W1: f64 = 0.08;  // input_length
    const W2: f64 = 0.22;  // structural
    const W3: f64 = 0.12;  // domain_crossing
    const W4: f64 = 0.20;  // decision_density
    const W5: f64 = 0.22;  // output_scale
    const W6: f64 = 0.08;  // external_dependency
    const W7: f64 = 0.08;  // precision_requirement

    let base = W1 * d.input_length
             + W2 * d.structural
             + W3 * d.domain_crossing
             + W4 * d.decision_density
             + W5 * d.output_scale
             + W6 * d.external_dependency
             + W7 * d.precision_requirement;

    let high_dims = [
        d.input_length, d.structural, d.domain_crossing,
        d.decision_density, d.output_scale, d.external_dependency,
        d.precision_requirement,
    ].iter().filter(|&&v| v > 0.5).count();

    let amplifier = match high_dims {
        0..=1 => 1.0,
        2 => 1.05,
        3 => 1.15,
        4 => 1.25,
        _ => 1.35,
    };

    (base * amplifier).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_code_reading() {
        let r = TaskAnalyzer::classify("show me the code in main.rs");
        assert_eq!(r.kind, TaskKind::CodeReading);
        assert!(r.confidence > 0.7);
    }

    #[test]
    fn test_code_writing() {
        let r = TaskAnalyzer::classify("implement a new function to parse JSON");
        assert_eq!(r.kind, TaskKind::CodeWriting);
    }

    #[test]
    fn test_web_search() {
        let r = TaskAnalyzer::classify("search for rust async tutorial online");
        assert_eq!(r.kind, TaskKind::WebSearch);
    }

    #[test]
    fn test_debugging() {
        let r = TaskAnalyzer::classify("fix the compilation error in parser.rs");
        assert_eq!(r.kind, TaskKind::Debugging);
    }

    #[test]
    fn test_general_chat() {
        let r = TaskAnalyzer::classify("hello, how are you?");
        assert_eq!(r.kind, TaskKind::GeneralChat);
    }

    #[test]
    fn test_data_analysis() {
        let r = TaskAnalyzer::classify("analyze the sales data trend");
        assert_eq!(r.kind, TaskKind::DataAnalysis);
    }

    #[test]
    fn test_domains_coding() {
        let (kind, domains) = TaskAnalyzer::classify_with_domains("write a rust function");
        assert_eq!(kind, TaskKind::CodeWriting);
        assert!(domains.contains(&"development".to_string()));
    }

    // ─── 复杂度评分测试 ────────────────────────────────────

    #[test]
    fn test_simple_input_low_complexity() {
        let profile = TaskAnalyzer::analyze_complexity("hello");
        assert!(profile.score < 0.15, "simple greeting should be low complexity, got {}", profile.score);
        assert!(!profile.has_decisions);
    }

    #[test]
    fn test_prd_input_high_complexity() {
        let input = "为内部审批系统写一份完整的PRD文档，需要对比自建和外购方案，包括需求分析和架构设计";
        let profile = TaskAnalyzer::analyze_complexity(input);
        // 输入较短(~40字)，score 偏低是正确行为。但关键信号应被检出：
        assert!(profile.score > 0.2, "PRD task should have some complexity, got {}", profile.score);
        assert!(profile.has_decisions, "should detect decision signals");
        // output_scale 受 length_factor 影响，短输入仍能通过 doc_mult 和 scope_mult 提升
        assert!(profile.dimensions.output_scale > 0.1, "should detect doc output scale, got {}", profile.dimensions.output_scale);
    }

    #[test]
    fn test_decision_detection() {
        let input = "应该选择方案A还是方案B？请对比两个策略的优先级";
        let profile = TaskAnalyzer::analyze_complexity(input);
        assert!(profile.has_decisions);
        assert!(profile.dimensions.decision_density >= 0.4);
    }

    #[test]
    fn test_multi_domain_crossing() {
        let input = "设计一个合规审计系统，需要API接口、数据分析dashboard和安全加密";
        let profile = TaskAnalyzer::analyze_complexity(input);
        assert!(profile.domain_count >= 3, "should cross 3+ domains, got {}", profile.domain_count);
        assert!(profile.dimensions.domain_crossing >= 0.6);
    }

    #[test]
    fn test_structural_complexity() {
        let input = "首先分析需求，然后设计架构，接着实现代码，最后写测试";
        let profile = TaskAnalyzer::analyze_complexity(input);
        assert!(profile.dimensions.structural >= 0.4, "multi-step should trigger structural, got {}", profile.dimensions.structural);
    }

    #[test]
    fn test_composite_amplifier() {
        // Longer input that hits multiple dimensions > 0.5
        // 需要足够长度才能触发 input_length 信号和多个领域交叉
        let input = "请写一份完整详细的金融合规审计报告文档，全面对比三种不同安全策略方案的优劣，\
                     需要精确的统计数据分析和可视化dashboard，同时确保符合法律合规要求，\
                     首先分析现状，然后对比方案，最后给出推荐建议和实施路径";
        let profile = TaskAnalyzer::analyze_complexity(input);
        // Multi-dim: finance + legal + data + security → domain crossing
        // + structural (首先/然后/最后) + decision (对比/推荐) + output_scale (完整/详细/文档)
        assert!(profile.score > 0.4, "multi-dim high should amplify, got {}", profile.score);
        assert!(profile.domain_count >= 3, "should cross 3+ domains, got {}", profile.domain_count);
    }
}