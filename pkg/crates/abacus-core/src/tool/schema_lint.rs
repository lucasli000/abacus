//! ToolSchema Lint —— 注册时静态规则检查
//!
//! ## 设计目标
//! 1. **写时拦截**：新工具 register 时立刻发现 description/parameters/cost 等问题
//! 2. **稳定 rule_id**：白名单/统计/PR 注释依赖 ID 字符串而非数字
//! 3. **白名单豁免**：现有工具违规 → YAML config 显式 allow，不阻断启动
//! 4. **dev panic + 旁路**：debug build 下 Warn 也 panic，env `ABACUS_LINT_PANIC=0` 可关
//!
//! ## 集成点
//! - [`super::ToolRegistry::register`] —— 注册时调 [`LintRuleSet::lint`]
//! - [`super::ToolRegistry::lint_audit`] —— 返回累积 issues 给 Layer 5 audit
//! - YAML config loader（Phase 3 加）—— 把 `(tool_id, rule_id)` 写入 allowed
//!
//! ## 引用关系
//! - 无外部依赖（除 ToolSchema/ToolId 本身）
//! - LintRule trait 可由用户扩展（registry.add_lint_rule）
//!
//! ## 生命周期
//! - LintRuleSet 随 ToolRegistry 创建/销毁
//! - 规则集本身不可变（构造后 Vec 锁定）；allowed list 可运行时增加但不删除

use abacus_types::{ToolId, ToolSchema};
use serde::{Deserialize, Serialize};

/// YAML config 反序列化结构
///
/// ```yaml
/// lint:
///   allowed:
///     - tool: "fs_grep"
///       rules: ["params_too_many_props"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LintOverrides {
    #[serde(default)]
    pub allowed: Vec<LintAllowEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintAllowEntry {
    pub tool: String,
    pub rules: Vec<String>,
}

/// Lint 严重等级
///
/// - `Info`：观察用，写 debug 日志，audit 计数
/// - `Warn`：写 warn 日志；debug build 下默认 panic（除非在 allowed 中或 ABACUS_LINT_PANIC=0）
/// - `Error`：所有 build 都 panic（除非在 allowed 中）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LintSeverity {
    Info,
    Warn,
    Error,
}

/// 单条 lint 检测结果
#[derive(Debug, Clone)]
pub struct LintIssue {
    /// 稳定 ID（如 "desc_too_long"）—— 用于白名单引用 + 统计
    pub rule: &'static str,
    pub severity: LintSeverity,
    /// 字段路径（如 "description" / "parameters.foo"）
    pub field: &'static str,
    pub tool_id: ToolId,
    /// 详细原因（动态消息，可含字节数等）
    pub reason: String,
    /// 建议的修复方向（可选）
    pub suggestion: Option<String>,
}

/// Lint 规则 trait —— 用户可注入自定义规则
pub trait LintRule: Send + Sync {
    /// 稳定 ID，应使用 snake_case，如 "desc_too_long"
    fn id(&self) -> &'static str;
    /// 默认严重度（用户可通过 LintRuleSet::override_severity 覆盖）
    fn severity(&self) -> LintSeverity;
    /// 检测——返回 None 表示通过
    fn check(&self, schema: &ToolSchema, tool_id: &ToolId) -> Option<LintIssue>;
}

/// 规则集合
///
/// 引用关系：被 ToolRegistry 持有；register 时调 lint。
/// 生命周期：随 registry 创建/销毁；allowed 运行时只增不减。
pub struct LintRuleSet {
    rules: Vec<Box<dyn LintRule>>,
    /// 白名单：(tool_id, rule_id) → 豁免
    allowed: std::collections::HashSet<(ToolId, &'static str)>,
}

impl LintRuleSet {
    /// 默认规则集（14 条）—— 基于 Abacus 实测瓶颈
    pub fn default_rules() -> Self {
        let rules: Vec<Box<dyn LintRule>> = vec![
            // ─── Description ───
            Box::new(DescEmpty),
            Box::new(DescTooLong { warn_at: 150, error_at: 400 }),
            Box::new(DescHasMarkdown),
            Box::new(DescDoubleNewline),
            // ─── Name ───
            Box::new(NameInvalid),
            Box::new(NameTooLong { max: 60 }),
            // ─── Parameters ───
            Box::new(ParamsNotObject),
            Box::new(ParamsTooDeep { max_depth: 4 }),
            Box::new(ParamsTooManyProps { warn: 12, error: 30 }),
            Box::new(ParamsMissingPropDesc),
            // ─── Cost ───
            Box::new(CostMissingForExpensive),
            Box::new(CostInvalidLatency),
            // ─── Idempotent ───
            Box::new(IdempotentSemanticMismatch),
            // ─── Provenance ───
            Box::new(NoProvenanceInBuiltin),
            // ─── Description Quality (A5) ───
            Box::new(DescVague),
        ];
        Self {
            rules,
            allowed: std::collections::HashSet::new(),
        }
    }

    /// 空规则集（仅测试 / 渐进迁移）
    pub fn empty() -> Self {
        Self {
            rules: Vec::new(),
            allowed: std::collections::HashSet::new(),
        }
    }

    /// 注入自定义规则
    pub fn add_rule(&mut self, rule: Box<dyn LintRule>) {
        self.rules.push(rule);
    }

    /// 白名单某 (tool, rule)
    pub fn allow(&mut self, tool_id: ToolId, rule_id: &'static str) {
        self.allowed.insert((tool_id, rule_id));
    }

    /// 检查 (tool_id, rule_id) 是否在白名单
    pub fn is_allowed(&self, tool_id: &ToolId, rule_id: &str) -> bool {
        self.allowed
            .iter()
            .any(|(tid, rid)| tid == tool_id && *rid == rule_id)
    }

    /// Phase 3：从 YAML 加载白名单
    ///
    /// 格式：
    /// ```yaml
    /// lint:
    ///   allowed:
    ///     - tool: "fs_grep"
    ///       rules: ["params_too_many_props"]
    ///     - tool: "lsp_find_references"
    ///       rules: ["desc_too_long"]
    /// ```
    ///
    /// 引用关系：调用方负责把 yaml 文件读为 LintOverrides struct，
    /// 此方法把 (tool, rule) 二元组写入 allowed set。
    /// rule_id 必须是已注册规则之一（通过 self.rules 校验）；不在的规则警告但仍接受
    /// （兼容用户自定义规则的场景）。
    pub fn load_overrides(&mut self, overrides: LintOverrides) {
        let known_rules: std::collections::HashSet<&'static str> =
            self.rules.iter().map(|r| r.id()).collect();
        for entry in overrides.allowed {
            for rule_id in entry.rules {
                // 转 &'static str —— 通过 leak 把 String 升级（启动期一次性，量小）
                let rule_static: &'static str = Box::leak(rule_id.into_boxed_str());
                if !known_rules.contains(&rule_static) {
                    tracing::warn!(
                        rule = rule_static, tool = %entry.tool,
                        "lint override references unknown rule (ok if user-defined)"
                    );
                }
                self.allowed.insert((ToolId(entry.tool.clone()), rule_static));
            }
        }
    }

    /// 跑全部规则；过滤 allowed 后返回 issues
    pub fn lint(&self, schema: &ToolSchema, tool_id: &ToolId) -> Vec<LintIssue> {
        self.rules
            .iter()
            .filter_map(|r| r.check(schema, tool_id))
            .filter(|issue| !self.is_allowed(tool_id, issue.rule))
            .collect()
    }
}

impl Default for LintRuleSet {
    fn default() -> Self {
        Self::default_rules()
    }
}

// ─── Description Rules ──────────────────────────────────────────────────

pub struct DescEmpty;
impl LintRule for DescEmpty {
    fn id(&self) -> &'static str { "desc_empty" }
    fn severity(&self) -> LintSeverity { LintSeverity::Error }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        if s.description.trim().is_empty() {
            Some(LintIssue {
                rule: self.id(),
                severity: self.severity(),
                field: "description",
                tool_id: tid.clone(),
                reason: "description is empty".into(),
                suggestion: Some("provide a one-line description of what the tool does".into()),
            })
        } else { None }
    }
}

pub struct DescTooLong { pub warn_at: usize, pub error_at: usize }
impl LintRule for DescTooLong {
    fn id(&self) -> &'static str { "desc_too_long" }
    fn severity(&self) -> LintSeverity { LintSeverity::Warn }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        let len = s.description.len();
        if len > self.error_at {
            Some(LintIssue {
                rule: self.id(),
                severity: LintSeverity::Error,
                field: "description",
                tool_id: tid.clone(),
                reason: format!("description {} bytes > error threshold {}", len, self.error_at),
                suggestion: Some("trim to a single sentence; move details to internal docs".into()),
            })
        } else if len > self.warn_at {
            Some(LintIssue {
                rule: self.id(),
                severity: LintSeverity::Warn,
                field: "description",
                tool_id: tid.clone(),
                reason: format!("description {} bytes > warn threshold {}", len, self.warn_at),
                suggestion: Some("aim for ≤80 bytes (p95 of existing tools)".into()),
            })
        } else { None }
    }
}

pub struct DescHasMarkdown;
impl LintRule for DescHasMarkdown {
    fn id(&self) -> &'static str { "desc_has_markdown" }
    fn severity(&self) -> LintSeverity { LintSeverity::Warn }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        let d = &s.description;
        let has = d.contains("##") || d.contains("###") || d.contains("```");
        if has {
            Some(LintIssue {
                rule: self.id(),
                severity: self.severity(),
                field: "description",
                tool_id: tid.clone(),
                reason: "description contains markdown markers (## / ### / ```)".to_string(),
                suggestion: Some("strip headers and code blocks; description should be inline".into()),
            })
        } else { None }
    }
}

pub struct DescDoubleNewline;
impl LintRule for DescDoubleNewline {
    fn id(&self) -> &'static str { "desc_double_newline" }
    fn severity(&self) -> LintSeverity { LintSeverity::Warn }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        if s.description.contains("\n\n") {
            Some(LintIssue {
                rule: self.id(),
                severity: self.severity(),
                field: "description",
                tool_id: tid.clone(),
                reason: "description has paragraph break (\\n\\n)".into(),
                suggestion: Some("collapse to single line".into()),
            })
        } else { None }
    }
}

// ─── Name Rules ─────────────────────────────────────────────────────────

pub struct NameInvalid;
impl LintRule for NameInvalid {
    fn id(&self) -> &'static str { "name_invalid" }
    fn severity(&self) -> LintSeverity { LintSeverity::Error }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        let valid = s.name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.');
        if !valid {
            Some(LintIssue {
                rule: self.id(),
                severity: self.severity(),
                field: "name",
                tool_id: tid.clone(),
                reason: format!("name '{}' contains chars outside [a-zA-Z0-9_.-]", s.name),
                suggestion: Some("OpenAI/DeepSeek require ^[a-zA-Z0-9_-]+$; '.' will be sanitized to '_' at request time".into()),
            })
        } else { None }
    }
}

pub struct NameTooLong { pub max: usize }
impl LintRule for NameTooLong {
    fn id(&self) -> &'static str { "name_too_long" }
    fn severity(&self) -> LintSeverity { LintSeverity::Error }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        if s.name.len() > self.max {
            Some(LintIssue {
                rule: self.id(),
                severity: self.severity(),
                field: "name",
                tool_id: tid.clone(),
                reason: format!("name length {} > max {}", s.name.len(), self.max),
                suggestion: Some(format!("OpenAI tool name limit is {} bytes", self.max)),
            })
        } else { None }
    }
}

// ─── Parameters Rules ───────────────────────────────────────────────────

pub struct ParamsNotObject;
impl LintRule for ParamsNotObject {
    fn id(&self) -> &'static str { "params_not_object" }
    fn severity(&self) -> LintSeverity { LintSeverity::Error }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        let typ = s.parameters.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if typ != "object" {
            Some(LintIssue {
                rule: self.id(),
                severity: self.severity(),
                field: "parameters.type",
                tool_id: tid.clone(),
                reason: format!("parameters.type must be 'object', got '{}'", typ),
                suggestion: Some("LLM tool calling spec requires top-level type:object".into()),
            })
        } else { None }
    }
}

pub struct ParamsTooDeep { pub max_depth: usize }
impl LintRule for ParamsTooDeep {
    fn id(&self) -> &'static str { "params_too_deep" }
    fn severity(&self) -> LintSeverity { LintSeverity::Warn }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        let depth = json_depth(&s.parameters);
        if depth > self.max_depth {
            Some(LintIssue {
                rule: self.id(),
                severity: self.severity(),
                field: "parameters",
                tool_id: tid.clone(),
                reason: format!("parameters JSON Schema depth {} > max {}", depth, self.max_depth),
                suggestion: Some("flatten nested objects; LLM accuracy drops with deep schemas".into()),
            })
        } else { None }
    }
}

pub struct ParamsTooManyProps { pub warn: usize, pub error: usize }
impl LintRule for ParamsTooManyProps {
    fn id(&self) -> &'static str { "params_too_many_props" }
    fn severity(&self) -> LintSeverity { LintSeverity::Warn }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        let props = s.parameters.get("properties").and_then(|v| v.as_object());
        let count = props.map(|m| m.len()).unwrap_or(0);
        if count > self.error {
            Some(LintIssue {
                rule: self.id(),
                severity: LintSeverity::Error,
                field: "parameters.properties",
                tool_id: tid.clone(),
                reason: format!("{} properties > error threshold {}", count, self.error),
                suggestion: Some("split into multiple tools or use a single 'options: object'".into()),
            })
        } else if count > self.warn {
            Some(LintIssue {
                rule: self.id(),
                severity: LintSeverity::Warn,
                field: "parameters.properties",
                tool_id: tid.clone(),
                reason: format!("{} properties > warn threshold {}", count, self.warn),
                suggestion: Some("LLM accuracy drops with many params; consider grouping".into()),
            })
        } else { None }
    }
}

pub struct ParamsMissingPropDesc;
impl LintRule for ParamsMissingPropDesc {
    fn id(&self) -> &'static str { "params_missing_prop_desc" }
    fn severity(&self) -> LintSeverity { LintSeverity::Info }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        let props = s.parameters.get("properties").and_then(|v| v.as_object())?;
        let missing: Vec<&String> = props.iter()
            .filter(|(_, v)| v.get("description").is_none())
            .map(|(k, _)| k)
            .collect();
        if !missing.is_empty() {
            Some(LintIssue {
                rule: self.id(),
                severity: self.severity(),
                field: "parameters.properties",
                tool_id: tid.clone(),
                reason: format!("{} prop(s) without description: {:?}",
                    missing.len(), missing.iter().take(3).collect::<Vec<_>>()),
                suggestion: Some("each property should have a description for LLM accuracy".into()),
            })
        } else { None }
    }
}

// ─── Cost Rules ─────────────────────────────────────────────────────────

pub struct CostMissingForExpensive;
impl LintRule for CostMissingForExpensive {
    fn id(&self) -> &'static str { "cost_missing_for_expensive" }
    fn severity(&self) -> LintSeverity { LintSeverity::Warn }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        if s.cost.is_some() { return None; }
        let n = s.name.to_ascii_lowercase();
        let expensive = ["search", "grep", "query", "llm", "fetch", "crawl"];
        for kw in &expensive {
            if n.contains(kw) {
                return Some(LintIssue {
                    rule: self.id(),
                    severity: self.severity(),
                    field: "cost",
                    tool_id: tid.clone(),
                    reason: format!("name contains '{}' but cost field is None", kw),
                    suggestion: Some("set ToolCost {tokens, latency, risk} so LLM can self-select cheaper paths".into()),
                });
            }
        }
        None
    }
}

pub struct CostInvalidLatency;
impl LintRule for CostInvalidLatency {
    fn id(&self) -> &'static str { "cost_invalid_latency" }
    fn severity(&self) -> LintSeverity { LintSeverity::Error }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        let cost = s.cost.as_ref()?;
        let lat = &cost.latency;
        // 简化检查：必须以 ms 或 s 结尾，前面是数字
        let ends_ok = lat.ends_with("ms") || lat.ends_with("s");
        let prefix = lat.trim_end_matches("ms").trim_end_matches('s');
        let prefix_ok = !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit() || c == '.');
        if !(ends_ok && prefix_ok) {
            Some(LintIssue {
                rule: self.id(),
                severity: self.severity(),
                field: "cost.latency",
                tool_id: tid.clone(),
                reason: format!("cost.latency '{}' must match \\d+(ms|s)", lat),
                suggestion: Some("use formats like '500ms' or '2s'".into()),
            })
        } else { None }
    }
}

// ─── Idempotent Rules ──────────────────────────────────────────────────

pub struct IdempotentSemanticMismatch;
impl LintRule for IdempotentSemanticMismatch {
    fn id(&self) -> &'static str { "idempotent_semantic_mismatch" }
    fn severity(&self) -> LintSeverity { LintSeverity::Warn }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        let n = s.name.to_ascii_lowercase();
        let mutating = ["write", "delete", "create", "update", "insert", "remove"];
        let is_mutating = mutating.iter().any(|kw| n.contains(kw));
        if is_mutating && s.idempotent {
            Some(LintIssue {
                rule: self.id(),
                severity: self.severity(),
                field: "idempotent",
                tool_id: tid.clone(),
                reason: format!("name '{}' suggests mutation but idempotent=true", s.name),
                suggestion: Some("only true if 2nd call has no additional effect (e.g. mkdir -p)".into()),
            })
        } else { None }
    }
}

// ─── Description Quality Rules ─────────────────────────────────────────

/// A5: 模糊词检测 —— description 中出现 ≥2 个模糊量词时告警
///
/// ## 动机
/// Prompting Tips 研究：具体性 > 模糊描述。
/// Tool description 是 LLM 选择工具的主要依据，模糊词会降低 LLM 选择准确率。
///
/// ## 规则
/// 匹配中英文常见模糊量词，≥2 次命中才触发（单次出现可能是合理用法）
///
/// ## 引用关系
/// - 被 LintRuleSet::default_rules() 加入规则集
/// - 调用方：ToolRegistry::register() → LintRuleSet::lint()
pub struct DescVague;

/// 中英文模糊量词列表（A5）
const VAGUE_PATTERNS: &[&str] = &[
    // 中文
    "可以", "也许", "通常", "一些", "相关", "各种", "某些", "等等",
    "大概", "可能", "一般", "有时", "往往", "适当",
    // English
    "various", "some", "certain", "usually", "typically", "often",
    "maybe", "perhaps", "generally", "sometimes",
];

impl LintRule for DescVague {
    fn id(&self) -> &'static str { "desc_vague" }
    fn severity(&self) -> LintSeverity { LintSeverity::Info }
    fn check(&self, s: &ToolSchema, tid: &ToolId) -> Option<LintIssue> {
        let desc_lower = s.description.to_lowercase();
        let hits: Vec<&str> = VAGUE_PATTERNS.iter()
            .filter(|&&p| desc_lower.contains(p))
            .copied()
            .collect();
        if hits.len() >= 2 {
            Some(LintIssue {
                rule: self.id(),
                severity: self.severity(),
                field: "description",
                tool_id: tid.clone(),
                reason: format!(
                    "description contains {} vague patterns: {:?} (first 3)",
                    hits.len(),
                    hits.iter().take(3).collect::<Vec<_>>()
                ),
                suggestion: Some("use specific, action-oriented language; avoid hedging words".into()),
            })
        } else {
            None
        }
    }
}

// ─── Provenance Rules ──────────────────────────────────────────────────

pub struct NoProvenanceInBuiltin;
impl LintRule for NoProvenanceInBuiltin {
    fn id(&self) -> &'static str { "no_provenance_in_builtin" }
    fn severity(&self) -> LintSeverity { LintSeverity::Warn }
    fn check(&self, s: &ToolSchema, _tid: &ToolId) -> Option<LintIssue> {
        let d = &s.description;
        // 防止开发者在 BuiltIn 工具描述里手动塞 [External MCP] 等标签
        // （这些标签由 build_tool_definitions_for 在运行时统一注入）
        for marker in ["[External", "[WASM plugin", "[Skill workflow"] {
            if d.contains(marker) {
                return Some(LintIssue {
                    rule: self.id(),
                    severity: self.severity(),
                    field: "description",
                    tool_id: _tid.clone(),
                    reason: format!("description contains provenance marker '{}' - injected at runtime, not in source", marker),
                    suggestion: Some("remove marker; ToolProvider determines prefix automatically".into()),
                });
            }
        }
        None
    }
}

// ─── 辅助 ──────────────────────────────────────────────────────────────

/// 计算 JSON Value 的最大嵌套深度（object/array）
fn json_depth(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Object(o) => 1 + o.values().map(json_depth).max().unwrap_or(0),
        serde_json::Value::Array(a) => 1 + a.iter().map(json_depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// 处理 lint issue —— 写日志 + panic
///
/// ## Panic 行为（V29.14 调整）
/// - **Error**：所有 build 都 panic（除非 ABACUS_LINT_PANIC=0 旁路）
/// - **Warn**：默认**不** panic，仅写 warn 日志
///   严格模式 (`ABACUS_LINT_STRICT=1`) 在 debug build 下启用 panic
///   设计意图：Warn 是"建议级"提醒，不该打死用户的 TUI/CLI 启动
///   原 Phase 3 设计在 debug build 默认 panic，实际造成 schema 微小漂移就启动失败
/// - **Info**：始终只写 debug 日志，不 panic
///
/// ## 严格模式 (V29.14 新增)
/// `ABACUS_LINT_STRICT=1` —— 让 dev build 的 Warn 也 panic
/// 用途：CI/PR 检查时启用，让 schema 漂移立刻暴露
///
/// ## 旁路环境变量
/// `ABACUS_LINT_PANIC=0` —— 禁用所有 panic 路径（含 Error），仅记日志
/// 用于本地紧急修复或迁移期
pub fn handle_issue(issue: &LintIssue) {
    match issue.severity {
        LintSeverity::Info => {
            tracing::debug!(
                rule = issue.rule, tool = %issue.tool_id.0, field = issue.field,
                suggestion = %issue.suggestion.as_deref().unwrap_or(""),
                "{}", issue.reason
            );
        }
        LintSeverity::Warn => {
            tracing::warn!(
                rule = issue.rule, tool = %issue.tool_id.0, field = issue.field,
                suggestion = %issue.suggestion.as_deref().unwrap_or(""),
                "{}", issue.reason
            );
            // V29.14: Warn 不再默认 panic. 仅严格模式 (ABACUS_LINT_STRICT=1) 下 dev build panic
            //   原行为: cfg(debug_assertions) 默认 panic → schema 微小漂移就打死 TUI 启动 (用户报告 lsp.goto_definition 155 字节超阈值)
            //   新行为: 写 warn 日志即可, 用户启动正常; CI 可设 ABACUS_LINT_STRICT=1 强制 panic 暴露
            #[cfg(debug_assertions)]
            {
                if lint_strict_enabled() && !panic_bypassed() {
                    panic!(
                        "schema lint Warn (ABACUS_LINT_STRICT=1 enabled): \
                         tool={} rule={} field={} reason={}",
                        issue.tool_id.0, issue.rule, issue.field, issue.reason
                    );
                }
            }
        }
        LintSeverity::Error => {
            tracing::error!(
                rule = issue.rule, tool = %issue.tool_id.0, field = issue.field,
                suggestion = %issue.suggestion.as_deref().unwrap_or(""),
                "{}", issue.reason
            );
            // 仅 debug build 下 panic（CI 可捕获）；release 不崩溃启动
            // 旧行为: 任何 Error 直接 panic → 微小 schema 漂移崩溃生产启动
            // 新行为: debug panic + release 仅 log（issues 已累积到 lint_issues 供审计）
            #[cfg(debug_assertions)]
            {
                if !panic_bypassed() {
                    panic!(
                        "schema lint Error (set ABACUS_LINT_PANIC=0 to bypass): \
                         tool={} rule={} field={} reason={}",
                        issue.tool_id.0, issue.rule, issue.field, issue.reason
                    );
                }
            }
        }
    }
}

/// V29.14 新增: 检查严格模式是否激活 (Warn level dev panic 的 opt-in)
///
/// 设计意图：让 Warn 默认不 panic（用户友好），CI 可显式 opt-in
///
/// 引用关系：仅由 handle_issue 的 Warn 分支调用（#[cfg(debug_assertions)] 块内）
#[cfg(debug_assertions)]
fn lint_strict_enabled() -> bool {
    if cfg!(test) { return false; } // 测试场景永不启用
    std::env::var("ABACUS_LINT_STRICT").as_deref() == Ok("1")
}

/// 检查 panic 旁路是否激活
///
/// 旁路触发条件：
/// 1. **测试场景**：`cfg!(test)` 自动旁路——避免 mock schema 触发 panic
/// 2. **环境变量**：`ABACUS_LINT_PANIC=0` 显式旁路（用于本地紧急修复）
///
/// 引用关系：仅由 handle_issue 内部调用。每次新 register 都重读环境变量。
#[cfg(debug_assertions)]
fn panic_bypassed() -> bool {
    // 测试场景下自动旁路——cargo test 会让整个 crate 编译为 test 模式
    // 测试中常用最简 schema（如 {"type": "string"}），不该触发 lint Error panic
    if cfg!(test) { return true; }
    std::env::var("ABACUS_LINT_PANIC").as_deref() == Ok("0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_types::{ToolId, ToolSchema};

    fn mk_schema(name: &str, desc: &str, params: serde_json::Value) -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: desc.into(),
            parameters: params,
            returns: None, security: None, cost: None,
            examples: Vec::new(),
            applicable_task_kinds: None,
            idempotent: false,
            schema_stable: false,
            short_description: None,
        }
    }

    #[test]
    fn desc_empty_caught() {
        let s = mk_schema("t", "", serde_json::json!({"type": "object"}));
        let tid = ToolId("t".into());
        let r = DescEmpty;
        assert!(r.check(&s, &tid).is_some());
    }

    #[test]
    fn desc_too_long_warn_only() {
        let s = mk_schema("t", &"x".repeat(200), serde_json::json!({"type": "object"}));
        let tid = ToolId("t".into());
        let r = DescTooLong { warn_at: 150, error_at: 400 };
        let issue = r.check(&s, &tid).unwrap();
        assert_eq!(issue.severity, LintSeverity::Warn);
    }

    #[test]
    fn desc_too_long_error_at_threshold() {
        let s = mk_schema("t", &"x".repeat(500), serde_json::json!({"type": "object"}));
        let tid = ToolId("t".into());
        let r = DescTooLong { warn_at: 150, error_at: 400 };
        let issue = r.check(&s, &tid).unwrap();
        assert_eq!(issue.severity, LintSeverity::Error);
    }

    #[test]
    fn desc_markdown_caught() {
        let s = mk_schema("t", "## Heading\nbody", serde_json::json!({"type": "object"}));
        let tid = ToolId("t".into());
        let r = DescHasMarkdown;
        assert!(r.check(&s, &tid).is_some());
    }

    #[test]
    fn name_invalid_caught() {
        let s = mk_schema("bad name!", "ok", serde_json::json!({"type": "object"}));
        let tid = ToolId("t".into());
        let r = NameInvalid;
        assert!(r.check(&s, &tid).is_some());
    }

    #[test]
    fn params_not_object_caught() {
        let s = mk_schema("t", "ok", serde_json::json!({"type": "string"}));
        let tid = ToolId("t".into());
        let r = ParamsNotObject;
        assert!(r.check(&s, &tid).is_some());
    }

    #[test]
    fn idempotent_mismatch_caught() {
        let mut s = mk_schema("file_delete", "ok", serde_json::json!({"type": "object"}));
        s.idempotent = true;
        let tid = ToolId("file_delete".into());
        let r = IdempotentSemanticMismatch;
        let issue = r.check(&s, &tid).unwrap();
        assert_eq!(issue.severity, LintSeverity::Warn);
    }

    #[test]
    fn allowed_filters_issue() {
        let mut set = LintRuleSet::default_rules();
        let tid = ToolId("ok_tool".into());
        let s = mk_schema("ok_tool", "## md content", serde_json::json!({"type": "object"}));
        // 默认会触发 desc_has_markdown
        assert!(set.lint(&s, &tid).iter().any(|i| i.rule == "desc_has_markdown"));
        // 加 allowed 后不再出
        set.allow(tid.clone(), "desc_has_markdown");
        assert!(!set.lint(&s, &tid).iter().any(|i| i.rule == "desc_has_markdown"));
    }

    #[test]
    fn desc_vague_caught_on_two_hits() {
        let s = mk_schema("t", "通常可以用来处理各种任务", serde_json::json!({"type": "object"}));
        let tid = ToolId("t".into());
        let r = DescVague;
        assert!(r.check(&s, &tid).is_some());
    }

    #[test]
    fn desc_vague_not_caught_on_one_hit() {
        let s = mk_schema("t", "通常用来搜索文件内容", serde_json::json!({"type": "object"}));
        let tid = ToolId("t".into());
        let r = DescVague;
        // 只有 "通常" 一个，不触发
        assert!(r.check(&s, &tid).is_none());
    }

    #[test]
    fn json_depth_basic() {
        assert_eq!(json_depth(&serde_json::json!(1)), 0);
        assert_eq!(json_depth(&serde_json::json!({"a": 1})), 1);
        assert_eq!(json_depth(&serde_json::json!({"a": {"b": 1}})), 2);
        assert_eq!(json_depth(&serde_json::json!({"a": [{"b": [1, 2]}]})), 4);
    }
}
