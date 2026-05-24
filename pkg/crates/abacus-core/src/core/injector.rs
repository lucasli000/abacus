//! Dynamic knowledge injection into system prompts
//!
//! Provides [`DynamicInjector`] which evaluates incoming user input and context
//! against registered [`InjectionSource`]s, producing [`PromptSegment`]s that
//! are merged into the system prompt via BTreeMap priority ordering.
//!
//! ## Built-in Sources
//!
//! - **TopicInjector**: Detects programming language/topic keywords and injects
//!   relevant best-practice guidance
//! - **ToolResultInjector**: Injects a reminder when tool results are available
//!   in the current turn
//!
//! ## Priority Ordering
//!
//! Segments are ordered by [`SegmentKind::priority()`] (higher = closer to prompt start):
//! - Kernel: 255 (always first)
//! - abacusbr: 230
//! - Guide/Strategy: 200
//! - AntiPattern: 190
//! - ToolGuide/Knowledge: 180
//! - ContextProtocol: 170
//! - SkillPrompt: 90
//! - GeneralGuide: 100
//! - InteractionMap: 20
//! - ToolSchema: 10 (always last)
//!
//! ## Known Limitations
//!
//! - [`InjectionSource`] uses `Box<dyn Fn>` closures, making it neither `Clone`
//!   nor `Serialize`. This was chosen deliberately to avoid requiring `Clone` on
//!   all closure captures.
//! - Topic detection is limited to 11 hardcoded keywords. A production system
//!   should load topics from a configuration file.

use serde_json::Value;

/// Maximum token budget for injected knowledge segments.
/// If total injected text exceeds this (estimated as chars / 2 for CJK-heavy text),
/// only the most recently refreshed entries (highest TTL) are kept.
const MAX_KNOWLEDGE_TOKENS: usize = 600;

/// Default TTL for expert role entries: stays alive for 5 turns of non-matching input.
const DEFAULT_TTL: u32 = 5;

/// Consecutive trigger count threshold before upgrading from brief to full text.
const FULL_UPGRADE_THRESHOLD: u32 = 3;

/// Knowledge segment with TTL for auto-unload.
///
/// ## Lifecycle
/// - Created: when `detect_expert_role()` matches a keyword in user input
/// - Active: TTL > 0; decremented each turn if no keyword re-match
/// - Destroyed: TTL reaches 0 → removed from `active_knowledge`
/// - Upgrade: `consecutive_hits >= FULL_UPGRADE_THRESHOLD` → `is_brief` flips to false
///
/// ## References
/// - Producer: `DynamicInjector::inject()` (expert_role source)
/// - Consumer: `DynamicInjector::active_knowledge()` → `PromptAssembly` (Layer 180)
#[derive(Debug, Clone)]
struct KnowledgeEntry {
    segment: PromptSegment,
    /// Role ID for matching (e.g., "trading_strategist")
    role_id: String,
    /// Turns remaining before auto-unload. Decremented each turn if no keyword match.
    ttl: u32,
    /// Whether this is the brief version (true) or full version (false)
    is_brief: bool,
    /// Consecutive turns this role has been triggered (for brief→full upgrade)
    consecutive_hits: u32,
}

/// Priority tier for a prompt segment.
///
/// Higher priority values place the segment closer to the beginning of the
/// system prompt (255 = kernel/fixed, 10 = tool schemas/last).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SegmentKind {
    /// Kernel-level instructions (always first)
    Kernel,
    /// abacusbr behavioral rules
    Abacusbr,
    /// High-level strategy guidance
    GuideStrategy,
    /// Anti-pattern warnings
    AntiPattern,
    /// Tool usage guidance
    ToolGuide,
    /// Context window protocol
    ContextProtocol,
    /// Injected domain knowledge
    Knowledge,
    /// Active skill prompts
    SkillPrompt,
    /// General instructions
    GeneralGuide,
    /// Tool schema definitions
    ToolSchema,
    /// Interaction map status
    InteractionMap,
    /// ICL examples injected near the end of the prompt (recency position).
    /// Filled by pipeline setup() when complexity.score > 0.5 and active_knowledge exists.
    /// Consumers: PromptAssembly (priority 15, below InteractionMap)
    IclPrimer,
}

impl SegmentKind {
    /// Return the priority value for this segment kind.
    /// Higher values are placed earlier in the assembled prompt.
    pub fn priority(&self) -> u8 {
        match self {
            SegmentKind::Kernel => 255,
            SegmentKind::Abacusbr => 230,
            SegmentKind::GuideStrategy => 200,
            SegmentKind::AntiPattern => 190,
            SegmentKind::ToolGuide => 180,
            SegmentKind::ContextProtocol => 170,
            SegmentKind::Knowledge => 180,
            SegmentKind::SkillPrompt => 90,
            SegmentKind::GeneralGuide => 100,
            SegmentKind::ToolSchema => 10,
            SegmentKind::InteractionMap => 20,
            SegmentKind::IclPrimer => 15,
        }
    }
}

/// A single segment of text tagged with its priority kind.
///
/// Used by [`DynamicInjector`] to produce ordered prompt fragments.
#[derive(Debug, Clone)]
pub struct PromptSegment {
    /// The priority kind of this segment
    pub kind: SegmentKind,
    /// The text content to inject
    pub text: String,
}

/// A registered injection rule.
///
/// Contains two closures:
/// - `should_inject`: evaluates whether this source should fire for the given input/context
/// - `inject`: produces the [`PromptSegment`] if firing
///
/// Uses `Box<dyn Fn>` instead of a trait object to avoid `Clone` requirements
/// on closure types. This means InjectionSource is neither `Clone` nor `Serialize`.
pub struct InjectionSource {
    /// Unique identifier for this source
    pub source_id: String,
    /// Predicate: should this source inject for the given input and context?
    pub should_inject: Box<dyn Fn(&str, &Value) -> bool + Send + Sync>,
    /// Producer: return a prompt segment if conditions are met
    pub inject: Box<dyn Fn(&str, &Value) -> Option<PromptSegment> + Send + Sync>,
}

/// Evaluates input and context against registered sources, producing
/// priority-ordered [`PromptSegment`]s for system prompt injection.
///
/// Maintains cross-turn `active_knowledge` for segments tagged as [`SegmentKind::Knowledge`],
/// so topic context persists across multiple turns with TTL-based auto-unload.
///
/// ## State Lifecycle
/// - `knowledge_entries`: TTL-managed expert role entries (created on keyword match, destroyed on TTL=0)
/// - `non_role_knowledge`: non-expert Knowledge segments (topic/tool_result), managed by byte-dedup
/// - `cached_segments`: materialized view of active knowledge, rebuilt on mutation
pub struct DynamicInjector {
    sources: Vec<InjectionSource>,
    /// TTL-managed expert role entries.
    /// Created: keyword match in inject(). Destroyed: TTL countdown reaches 0.
    knowledge_entries: Vec<KnowledgeEntry>,
    /// Non-role Knowledge segments (topic injector, tool_result injector).
    /// Lifecycle: replaced each turn via byte-dedup comparison.
    non_role_knowledge: Vec<PromptSegment>,
    /// Cached materialized segments for backward-compatible `active_knowledge()` getter.
    /// Rebuilt whenever knowledge_entries or non_role_knowledge mutates.
    cached_segments: Vec<PromptSegment>,
}

impl Default for DynamicInjector {
    fn default() -> Self { Self::new() }
}

impl DynamicInjector {
    /// Create an empty injector with no registered sources.
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
            knowledge_entries: Vec::new(),
            non_role_knowledge: Vec::new(),
            cached_segments: Vec::new(),
        }
    }

    /// Register a new injection source.
    pub fn register_source(&mut self, source: InjectionSource) {
        self.sources.push(source);
    }

    /// Evaluate all registered sources against the current input and context.
    ///
    /// Returns segments sorted by priority (highest first).
    ///
    /// ## TTL Logic (expert role entries)
    /// 1. If a role keyword matches this turn: reset TTL to DEFAULT_TTL, increment consecutive_hits
    /// 2. If no role keyword matches: decrement all TTL by 1
    /// 3. If TTL reaches 0: remove that entry
    /// 4. First injection uses brief_text; upgrades to full_text after FULL_UPGRADE_THRESHOLD consecutive turns
    ///
    /// ## Budget Cap
    /// After TTL processing, if total token estimate exceeds MAX_KNOWLEDGE_TOKENS,
    /// entries are sorted by TTL desc and trimmed from the tail.
    pub fn inject(&mut self, input: &str, context: &Value) -> Vec<PromptSegment> {
        let mut segments = Vec::new();
        for source in &self.sources {
            if (source.should_inject)(input, context) {
                if let Some(seg) = (source.inject)(input, context) {
                    segments.push(seg);
                }
            }
        }
        segments.sort_by_key(|s| std::cmp::Reverse(s.kind.priority()));

        // Separate expert_role segments from other Knowledge segments.
        // Expert role TTL is managed via detect_expert_role_split() below;
        // here we only collect non-role Knowledge segments for byte-dedup.
        let new_non_role_knowledge: Vec<PromptSegment> = segments.iter()
            .filter(|s| s.kind == SegmentKind::Knowledge)
            .filter(|s| !s.text.starts_with("[ExpertRole:") && !s.text.contains("[ExpertRole:"))
            .cloned()
            .collect();

        // ─── TTL management for expert role entries ───────────────────────────
        let mut mutated = false;

        // Detect which roles matched this turn via detect_expert_role
        let lower = input.to_lowercase();
        let detected = detect_expert_role_split(&lower);

        if let Some((role_id, brief_text, full_text)) = detected {
            // Role keyword matched: find existing entry or create new
            if let Some(entry) = self.knowledge_entries.iter_mut().find(|e| e.role_id == role_id) {
                // Reset TTL and increment hit counter
                if entry.ttl != DEFAULT_TTL || entry.consecutive_hits < FULL_UPGRADE_THRESHOLD {
                    entry.ttl = DEFAULT_TTL;
                    entry.consecutive_hits += 1;
                    // Upgrade brief→full if threshold reached
                    if entry.is_brief && entry.consecutive_hits >= FULL_UPGRADE_THRESHOLD {
                        entry.is_brief = false;
                        entry.segment = PromptSegment {
                            kind: SegmentKind::Knowledge,
                            text: full_text,
                        };
                    }
                    mutated = true;
                } else {
                    // TTL already at max and already upgraded — no mutation needed (KV stability)
                    entry.consecutive_hits += 1;
                }
            } else {
                // New role: inject brief version with TTL
                self.knowledge_entries.push(KnowledgeEntry {
                    segment: PromptSegment {
                        kind: SegmentKind::Knowledge,
                        text: brief_text,
                    },
                    role_id,
                    ttl: DEFAULT_TTL,
                    is_brief: true,
                    consecutive_hits: 1,
                });
                mutated = true;
            }
        } else {
            // No role matched: decrement TTL on all entries
            for entry in &mut self.knowledge_entries {
                if entry.ttl > 0 {
                    entry.ttl -= 1;
                    mutated = true;
                }
            }
            // Remove expired entries (TTL = 0)
            let before_len = self.knowledge_entries.len();
            self.knowledge_entries.retain(|e| e.ttl > 0);
            if self.knowledge_entries.len() != before_len {
                mutated = true;
            }
        }

        // ─── Budget cap: enforce MAX_KNOWLEDGE_TOKENS ─────────────────────────
        if !self.knowledge_entries.is_empty() {
            let total_tokens: usize = self.knowledge_entries.iter()
                .map(|e| estimate_tokens(&e.segment.text))
                .sum();
            if total_tokens > MAX_KNOWLEDGE_TOKENS {
                // Sort by TTL desc (keep highest TTL = most recently refreshed)
                self.knowledge_entries.sort_by(|a, b| b.ttl.cmp(&a.ttl));
                let mut budget = 0usize;
                let mut keep_count = 0;
                for entry in &self.knowledge_entries {
                    let cost = estimate_tokens(&entry.segment.text);
                    if budget + cost > MAX_KNOWLEDGE_TOKENS {
                        break;
                    }
                    budget += cost;
                    keep_count += 1;
                }
                if keep_count < self.knowledge_entries.len() {
                    self.knowledge_entries.truncate(keep_count);
                    mutated = true;
                }
            }
        }

        // ─── Non-role knowledge: byte-dedup (topic/tool_result segments) ──────
        if !new_non_role_knowledge.is_empty() {
            let current_text: String = self.non_role_knowledge
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join("\x00");
            let new_text: String = new_non_role_knowledge
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join("\x00");
            if current_text != new_text {
                self.non_role_knowledge = new_non_role_knowledge;
                mutated = true;
            }
        } else if !self.non_role_knowledge.is_empty() {
            // No new non-role knowledge this turn but we had some — keep existing (sticky)
            // (topic context persists; only replaced when new topic detected)
        }

        // ─── Rebuild cached_segments if any mutation occurred ─────────────────
        if mutated {
            self.rebuild_cache();
        }

        segments
    }

    /// Get the cross-turn knowledge segments from previous injections.
    ///
    /// Returns a slice of `PromptSegment` for backward compatibility.
    /// Internally extracts segments from TTL-managed `KnowledgeEntry` + non-role knowledge.
    pub fn active_knowledge(&self) -> &[PromptSegment] {
        &self.cached_segments
    }

    /// Clear all accumulated active knowledge (both expert role entries and non-role knowledge).
    pub fn clear_active_knowledge(&mut self) {
        self.knowledge_entries.clear();
        self.non_role_knowledge.clear();
        self.cached_segments.clear();
    }

    /// Rebuild the cached_segments vector from current state.
    /// Called after any mutation to knowledge_entries or non_role_knowledge.
    fn rebuild_cache(&mut self) {
        self.cached_segments.clear();
        for entry in &self.knowledge_entries {
            self.cached_segments.push(entry.segment.clone());
        }
        self.cached_segments.extend(self.non_role_knowledge.clone());
    }

    /// 注册专家角色注入源（10 个领域角色，Layer 180）。
    ///
    /// 检测用户输入中的领域关键词，匹配时注入对应角色的思维框架和行为约束。
    /// 使用 `SegmentKind::Knowledge`，跨 turn 持久化（角色上下文在会话中保持稳定）。
    ///
    /// ## 支持的角色
    /// | 角色 ID          | 中文触发词                              | 英文触发词 |
    /// |------------------|-----------------------------------------|------------|
    /// | financial        | 金融/财务/估值/利率/财报/量化           | finance/valuation/DCF |
    /// | security         | 安全/漏洞/攻击/渗透/加密/认证           | security/vulnerability/pentest |
    /// | data_science     | 数据分析/机器学习/特征/模型/预测        | ML/dataset/feature/training |
    /// | product          | 需求/用户故事/PRD/产品/MVP/用户体验     | PRD/user story/MVP |
    /// | devops           | 部署/Docker/K8s/CI/CD/监控/运维         | deploy/kubernetes/pipeline |
    /// | trading          | 策略/回测/alpha/信号/仓位/风控/做多/做空 | backtest/strategy/alpha/position |
    /// | architecture     | 分布式/微服务/DDD/高可用/架构设计       | distributed/microservice/DDD |
    /// | code_review      | 代码审查/重构/代码质量/性能优化/可读性  | code review/refactor/clean code |
    /// | legal_compliance | 合规/法律/条款/隐私/GDPR/许可证/版权   | compliance/GDPR/license |
    /// | tech_writing     | 文档/README/API文档/注释/规范/说明书    | documentation/README/API spec |
    pub fn register_expert_role_injector(&mut self) {
        self.register_source(InjectionSource {
            source_id: "expert_role".into(),
            should_inject: Box::new(|input, _ctx| {
                let lower = input.to_lowercase();
                // 中英文领域触发词检测
                const FINANCIAL: &[&str] = &["金融", "财务", "估值", "利率", "财报", "量化", "dcf", "irr", "ebitda", "valuation", "finance"];
                const SECURITY:  &[&str] = &["安全", "漏洞", "攻击", "渗透", "加密", "认证", "xss", "注入", "vulnerability", "pentest", "owasp"];
                const DATASCIENCE: &[&str] = &["机器学习", "特征工程", "模型训练", "数据集", "过拟合", "gradient", "embedding", "neural", "dataset"];
                const PRODUCT:   &[&str] = &["用户故事", "prd", "需求文档", "用户体验", "mvp", "迭代计划", "user story", "product requirement"];
                const DEVOPS:    &[&str] = &["docker", "kubernetes", "k8s", "ci/cd", "流水线", "helm", "terraform", "ansible", "部署流程", "pipeline"];
                const TRADING:   &[&str] = &["回测", "alpha", "做多", "做空", "仓位", "开仓", "止损", "backtest", "long/short", "position sizing", "sharpe"];
                const ARCH:      &[&str] = &["分布式系统", "微服务", "ddd", "事件溯源", "cqrs", "saga", "高可用", "容灾", "microservice", "event sourcing"];
                const REVIEW:    &[&str] = &["代码审查", "代码质量", "重构方案", "技术债", "可读性", "clean code", "code smell", "solid原则", "solid principle"];
                const LEGAL:     &[&str] = &["合规性", "隐私政策", "gdpr", "开源许可", "版权", "法律条款", "数据主权", "license", "compliance", "regulation"];
                const TECHWRITE: &[&str] = &["api文档", "readme", "技术规范", "接口文档", "文档注释", "changelog", "openapi", "swagger", "api spec"];
                [FINANCIAL, SECURITY, DATASCIENCE, PRODUCT, DEVOPS, TRADING, ARCH, REVIEW, LEGAL, TECHWRITE]
                    .iter().any(|kws| kws.iter().any(|kw| lower.contains(kw)))
            }),
            inject: Box::new(|input, _ctx| {
                let lower = input.to_lowercase();
                let role = detect_expert_role(&lower)?;
                Some(PromptSegment {
                    kind: SegmentKind::Knowledge,
                    text: role.to_string(),
                })
            }),
        });
    }

    /// Register the default built-in sources:
    /// - Topic-based knowledge injector (11 languages/topics)
    /// - Tool result availability injector
    /// - Expert role injector (10 domain roles, Layer 180)
    pub fn register_defaults(&mut self) {
        self.register_expert_role_injector();
        self.register_source(InjectionSource {
            source_id: "topic".into(),
            should_inject: Box::new(|input, _ctx| {
                let topics = ["rust", "typescript", "python", "react", "api", "database"];
                topics.iter().any(|t| input.to_lowercase().contains(t))
            }),
            inject: Box::new(|input, _ctx| {
                let lower = input.to_lowercase();
                let topic = if lower.contains("rust") { "Rust" }
                    else if lower.contains("typescript") { "TypeScript" }
                    else if lower.contains("python") { "Python" }
                    else if lower.contains("react") { "React" }
                    else if lower.contains("api") { "API Design" }
                    else if lower.contains("database") { "Database" }
                    else { return None };
                Some(PromptSegment {
                    kind: SegmentKind::Knowledge,
                    text: format!("[Injector] Topic context: user discussing {topic}. Use relevant best practices and conventions."),
                })
            }),
        });

        self.register_source(InjectionSource {
            source_id: "tool_result".into(),
            should_inject: Box::new(|_input, ctx| {
                ctx.get("tool_results")
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
            }),
            // KV cache 修复：text 不嵌入 count（之前 `{} tool results` 让此 Knowledge 段
            //   每轮 byte 变化 → dedup 触发 active_knowledge 替换 → Layer 180 之后所有
            //   层 cache miss）。LLM 能直接从 messages 数组看到实际 tool 结果数，count 是冗余信号。
            //   稳定文本让相同情境下 dedup（line 181）短路、active_knowledge byte-identical。
            inject: Box::new(|_input, _ctx| {
                Some(PromptSegment {
                    kind: SegmentKind::Knowledge,
                    text: "[Injector] Tool results available in current turn. Review them before responding.".to_string(),
                })
            }),
        });
    }
}

// ─── Helper functions ────────────────────────────────────────────────────

/// Estimate token count for a text string.
/// Uses a simple heuristic: CJK characters count as ~1.5 tokens each,
/// ASCII words count as ~1 token per 4 chars. Simplified to chars/2 for mixed text.
fn estimate_tokens(text: &str) -> usize {
    // Rough estimate: 1 token per 2 characters for CJK-heavy text
    (text.len() + 1) / 2
}


// ─── 专家角色内容库 ───────────────────────────────────────────────────

/// 根据输入内容检测并返回对应专家角色的 brief + full 提示词。
///
/// ## 返回格式
/// `Option<(role_id, brief_text, full_text)>`
/// - brief_text: ~50 tokens, 单行角色标题 + 核心约束
/// - full_text: 完整多行格式 `[ExpertRole: <ID>] <角色简述>\n<思维框架>\n<行为约束>\n<工具使用偏好>`
///
/// ## 与 abacusbr.md 子场景的分工
/// - 此函数：轻量角色激活提示（Layer 180，跨 turn 持久，TTL 管理）
/// - abacusbr.md 子场景：完整领域行为规范（Layer 230，按 TaskKind 写入）
fn detect_expert_role_split(lower: &str) -> Option<(String, String, String)> {
    detect_expert_role_inner(lower)
}

/// Legacy wrapper: returns only the full text for backward compatibility with inject closure.
/// Used by the `expert_role` InjectionSource's inject closure.
fn detect_expert_role(lower: &str) -> Option<&'static str> {
    // 优先级：精确多词组先检测，再检测单词

    // 交易策略：高精度领域，无歧义
    if lower.contains("回测") || lower.contains("backtest")
        || lower.contains("alpha") || lower.contains("做多") || lower.contains("做空")
        || lower.contains("仓位管理") || lower.contains("position sizing")
        || lower.contains("开仓") || lower.contains("平仓") || lower.contains("sharpe")
    {
        return Some("[ExpertRole: trading_strategist] 交易策略专家
思维框架: 策略假设副题驗证 → 信号素质评估 → 风险定量 → 执行路径设计。
行为约束:
- 回测必须北废: 分离训练集/测试集，禁止未来数据泄露。
- 信号分析必须包含: 信号强度、宽度、衰减、市场制度四个维度。
- 风险控制先于收益: Kelly 分数/半 Kelly，强调最大回撤和浮动盗上限。
- 波动率调整: Sharpe、Sortino、回撤比这三个指标必须同时呈现。
工具偏好: code.execute（数值计算）、db.query（历史行情）、kb.query（策略知识）。");
    }

    // 安全审计：漏洞和攻击面很具体
    if lower.contains("漏洞") || lower.contains("vulnerability") || lower.contains("pentest")
        || lower.contains("渗透") || lower.contains("exploit") || lower.contains("owasp")
        || lower.contains("xss") || lower.contains("sql注入") || lower.contains("sql injection")
        || lower.contains("安全审查") || lower.contains("security audit")
    {
        return Some("[ExpertRole: security_analyst] 安全审计专家
思维框架: 威胁建模（STRIDE）→ 攻击面识别 → 漏洞验证 → 修复方案优先级。
行为约束:
- 永远提供技术鉴定和防御视角，拒绝指导实际攻击操作。
- 浏览器/输入辽渗点用 OWASP Top 10 进行完整密度检查。
- 加密建议必须指明算法强度和证书链完整性。
- 输出验证方法: PoC 一定要指定环境和限制条件。
工具偏好: fs.grep（模式扫描）、code.execute（沙盒验证）、kb.query（CVE知识库）。");
    }

    // 系统架构
    if lower.contains("分布式系统") || lower.contains("微服务") || lower.contains("microservice")
        || lower.contains("ddd") || lower.contains("事件溯源") || lower.contains("event sourcing")
        || lower.contains("cqrs") || lower.contains("容灾") || lower.contains("高可用")
    {
        return Some("[ExpertRole: system_architect] 系统架构师
思维框架: 业务边界划分 → CAP/PACELC 权衡判断 → 数据流建模 → 故障隔离 → ADR 冒。
行为约束:
- 每个架构决策必须输出 ADR（底层/备选/得失）三要素。
- 识别共享数据边界，不允许跨服务直接访问内部 DB。
- 一致性和延迟必须同时建模：最终一致还是强一致？为什么？
- 容量估算必须附归假设： QPS/DAU/数据增长率。
工具偏好: fs.read（现有架构）、lsp.workspace_symbol（依赖图）、kb.query（模式库）。");
    }

    // 数据科学：多词组选择，避免和数据分析十差错不分
    if lower.contains("机器学习") || lower.contains("machine learning") || lower.contains("deep learning")
        || lower.contains("特征工程") || lower.contains("feature engineering")
        || lower.contains("过拟合") || lower.contains("overfitting")
        || lower.contains("神经网络") || lower.contains("neural network")
        || lower.contains("模型训练") || lower.contains("model training")
    {
        return Some("[ExpertRole: data_scientist] 数据科学家
思维框架: 问题建模 → 数据探索(EDA) → 特征工程 → 基线模型 → 迭代优化 → 可解释性分析。
行为约束:
- 评估指标必须匹配业务目标（精度还是召回率？为什么？）。
- 数据泄露检查: 训练集中禁止包含未来信息。
- 模型复杂度 vs 效果的对比：简单基线模型必须先跑。
- 不确定性量化: 置信区间和检验集表现必须展示。
工具偏好: code.execute（Rhai 计算）、db.query（算法指标）、kb.query（学术方法）。");
    }

    // 产品经理
    if lower.contains("prd") || lower.contains("用户故事") || lower.contains("user story")
        || lower.contains("需求文档") || lower.contains("product requirement")
        || lower.contains("迭代计划") || lower.contains("sprint planning") || lower.contains("mvp")
    {
        return Some("[ExpertRole: product_manager] 产品经理
思维框架: 用户痛点定义 → 问题范围边界 → 方案评估（RICE/ICE）→ 验收标准 → 上线日期。
行为约束:
- 需求必须区分: 功能需求 vs 约束条件，每条采用 MECE 层次写。
- 带出成功标准（定量）和验证方法同时展示。
- 用户故事格式: 作为「角色」，我希望「行为」，以便「价値」。
- 优先级必须伴随依据：数据估算、技术成本、业务影响。
工具偏好: kb.query（竞品分析）、fs.read（现有需求文档）。");
    }

    // DevOps / 基础设施
    if lower.contains("docker") || lower.contains("kubernetes") || lower.contains("k8s")
        || lower.contains("流水线") || lower.contains("pipeline") || lower.contains("helm")
        || lower.contains("terraform") || lower.contains("ansible")
    {
        return Some("[ExpertRole: devops_engineer] DevOps / 基础设施工程师
思维框架: 环境差异分析 → 流水线设计 → 幂等性验证 → 回滚安全网 → 可观测性。
行为约束:
- Dockerfile 必须指定基础镜像版本，使用最小权限 non-root 用户。
- 幂等性验证: 重复执行 apply 必须无副作用。
- 销毁操作（drain/delete）必须附带回滚方案和记录备份验证。
- 资源定频和限制必须同时展示: CPU/内存/并发三个维度。
工具偏好: fs.read（配置文件）、bash.exec（验证命令）、kb.query（最佳实践）。");
    }

    // 代码审查
    if lower.contains("代码审查") || lower.contains("code review") || lower.contains("代码质量")
        || lower.contains("技术债") || lower.contains("technical debt")
        || lower.contains("重构方案") || lower.contains("solid原则") || lower.contains("solid principle")
    {
        return Some("[ExpertRole: code_reviewer] 代码审查専家
思维框架: 正确性 → 延伸性和可读性 → 性能隐患 → 安全风险 → 测试覆盖。
行为约束:
- 审查评论分三级: Blocker（阅考前必修）／Major（建议修）／Nit（风格建议）。
- 必须指出为什么是问题，不能只说“应该改”。
- 建议重构时必须评估: 改动成本 × 风险 ÷ 收益。
- 覆盖测试评估: 单元测试对领域概念的覆盖率是否足够。
工具偏好: lsp.find_references（引用分析）、lsp.call_hierarchy_incoming（调用链）。");
    }

    // 金融分析
    if lower.contains("dcf") || lower.contains("irr") || lower.contains("ebitda")
        || lower.contains("金融建模") || lower.contains("估値模型") || lower.contains("valuation model")
        || lower.contains("财务报表") || lower.contains("利润表") || lower.contains("资产负债表")
    {
        return Some("[ExpertRole: financial_analyst] 金融分析师
思维框架: 财务数据质量检验 → 可比性分析 → 假设设定 → 模型橁鸟 → 敖事性和数字并行。
行为约束:
- 关键指标必须附带行业平均对标。
- 假设必须明确标注并进行敏感性分析。
- 不确定性必须用情景分析（基准/乐观/悲观）表现。
- 标注数据来源时间戳，区分公告数据和估算数据。
工具偏好: code.execute（DCF/估值计算）、db.query（财务数据）。");
    }

    // 法务合规
    if lower.contains("gdpr") || lower.contains("合规性") || lower.contains("数据主权")
        || lower.contains("开源许可") || lower.contains("license") || lower.contains("隐私政策")
        || lower.contains("regulation") || lower.contains("法律验证")
    {
        return Some("[ExpertRole: legal_compliance] 法务合规专家
思维框架: 法规映射 → 应用场景确认 → 差距分析 → 修复方案优先级 → 封项记录。
行为约束:
- 定论必须附属具体法律条款编号和生效日期。
- 法律风险估算必须展示为最小化路径和接受路径两种方案。
- 隐私数据识别: 区分 PII/敏感数据/匹名化数据，说明处理要求。
- 禁止给出属于法律领域范围的具体建议，请务必提示孙证。
工具偏好: kb.query（法规知识库）、fs.read（合同和指南文件）。");
    }

    // 技术写作
    if lower.contains("api文档") || lower.contains("openapi") || lower.contains("swagger")
        || lower.contains("技术规范") || lower.contains("changelog") || lower.contains("接口文档")
        || lower.contains("api spec") || lower.contains("文档注释") || lower.contains("docstring")
    {
        return Some("[ExpertRole: tech_writer] 技术文档専家
思维框架: 读者画像 → 信息层次设计 → 示例第一 → 可发现性 → 小处细节与大局观并行。
行为约束:
- API 文档必须包含: 请求示例、响应示例、错误码表三部分。
- README 结构: 快速开始(一命令) → 核心功能 → 配置项 → FAQ。
- 长度安全处理: 单一概念 ≤ 300 字，超出时分页。
- 每个示例必须是可运行的，不允许出现 `your-value-here` 占位符。
工具偏好: fs.read（现有代码理解接口）、lsp.hover（类型提取）。");
    }

    // 产品/金融 通用金融分析（单词匹配）
    if lower.contains("金融") || lower.contains("财务") || lower.contains("量化")
        || lower.contains("finance") || lower.contains("financial")
    {
        return Some("[ExpertRole: financial_analyst] 金融分析师
思维框架: 定义问题 → 收集制度数据 → 量化模型架构 → 迟饢性验证 → 阈值配置。
行为约束:
- 每个金融结论必须附带数据来源和时间界。
- 关键指标必须对标市场基准/行业均値。
- 不确定性必须用情景分析展现而非单点预测。
- 标注数据时间戳和来源（市场数据还是公序数据）。
工具偏好: code.execute（金融模型计算）、db.query（历史数据）。");
    }

    // 通用安全（单词）
    if lower.contains("安全") || lower.contains("security") || lower.contains("加密")
        || lower.contains("认证") || lower.contains("权限")
    {
        return Some("[ExpertRole: security_analyst] 安全专家
思维框架: 威胁建模 → 攻击面识别 → 防御控制评估 → 深层防御设计。
行为约束:
- 点出安全问题时必须同时给出缓解方案。
- 永远提供防御视角，拒绝指导实际攻击。
- 加密建议必须指明使用场景和密鑰管理方案。
- 输入验证: 必须指出验证层级（客户端还是服务端）和拒绝策略。
工具偏好: fs.grep（模式扫描）、kb.query（安全知识库）。");
    }

    None
}

/// Returns `(role_id, brief_text, full_text)` for the matched expert role.
///
/// - brief_text: single-line, ~50 tokens — role title + core constraints
/// - full_text: multi-line format with full thinking framework and behavioral constraints
///
/// Mirror of `detect_expert_role` logic but returns structured tuple for TTL management.
fn detect_expert_role_inner(lower: &str) -> Option<(String, String, String)> {
    // 交易策略
    if lower.contains("回测") || lower.contains("backtest")
        || lower.contains("alpha") || lower.contains("做多") || lower.contains("做空")
        || lower.contains("仓位管理") || lower.contains("position sizing")
        || lower.contains("开仓") || lower.contains("平仓") || lower.contains("sharpe")
    {
        return Some((
            "trading_strategist".into(),
            "[trading_strategist] 策略假设→信号质素→风险定量→执行路径。禁止未来数据泄露，Kelly分数控仓。".into(),
            "[ExpertRole: trading_strategist] 交易策略专家\n思维框架: 策略假设副题驗证 → 信号素质评估 → 风险定量 → 执行路径设计。\n行为约束:\n- 回测必须北废: 分离训练集/测试集，禁止未来数据泄露。\n- 信号分析必须包含: 信号强度、宽度、衰减、市场制度四个维度。\n- 风险控制先于收益: Kelly 分数/半 Kelly，强调最大回撤和浮动盗上限。\n- 波动率调整: Sharpe、Sortino、回撤比这三个指标必须同时呈现。\n工具偏好: code.execute（数值计算）、db.query（历史行情）、kb.query（策略知识）。".into(),
        ));
    }

    // 安全审计
    if lower.contains("漏洞") || lower.contains("vulnerability") || lower.contains("pentest")
        || lower.contains("渗透") || lower.contains("exploit") || lower.contains("owasp")
        || lower.contains("xss") || lower.contains("sql注入") || lower.contains("sql injection")
        || lower.contains("安全审查") || lower.contains("security audit")
    {
        return Some((
            "security_analyst".into(),
            "[security_analyst] 威胁建模(STRIDE)→攻击面→漏洞验证→修复优先级。拒绝指导攻击，PoC必附环境限制。".into(),
            "[ExpertRole: security_analyst] 安全审计专家\n思维框架: 威胁建模（STRIDE）→ 攻击面识别 → 漏洞验证 → 修复方案优先级。\n行为约束:\n- 永远提供技术鉴定和防御视角，拒绝指导实际攻击操作。\n- 浏览器/输入辽渗点用 OWASP Top 10 进行完整密度检查。\n- 加密建议必须指明算法强度和证书链完整性。\n- 输出验证方法: PoC 一定要指定环境和限制条件。\n工具偏好: fs.grep（模式扫描）、code.execute（沙盒验证）、kb.query（CVE知识库）。".into(),
        ));
    }

    // 系统架构
    if lower.contains("分布式系统") || lower.contains("微服务") || lower.contains("microservice")
        || lower.contains("ddd") || lower.contains("事件溯源") || lower.contains("event sourcing")
        || lower.contains("cqrs") || lower.contains("容灾") || lower.contains("高可用")
    {
        return Some((
            "system_architect".into(),
            "[system_architect] 业务边界→CAP权衡→数据流建模→故障隔离→ADR。禁止跨服务直连DB，容量估算必附假设。".into(),
            "[ExpertRole: system_architect] 系统架构师\n思维框架: 业务边界划分 → CAP/PACELC 权衡判断 → 数据流建模 → 故障隔离 → ADR 冒。\n行为约束:\n- 每个架构决策必须输出 ADR（底层/备选/得失）三要素。\n- 识别共享数据边界，不允许跨服务直接访问内部 DB。\n- 一致性和延迟必须同时建模：最终一致还是强一致？为什么？\n- 容量估算必须附归假设： QPS/DAU/数据增长率。\n工具偏好: fs.read（现有架构）、lsp.workspace_symbol（依赖图）、kb.query（模式库）。".into(),
        ));
    }

    // 数据科学
    if lower.contains("机器学习") || lower.contains("machine learning") || lower.contains("deep learning")
        || lower.contains("特征工程") || lower.contains("feature engineering")
        || lower.contains("过拟合") || lower.contains("overfitting")
        || lower.contains("神经网络") || lower.contains("neural network")
        || lower.contains("模型训练") || lower.contains("model training")
    {
        return Some((
            "data_scientist".into(),
            "[data_scientist] EDA→特征工程→基线模型→迭代优化→可解释性。禁止训练集含未来信息，简单基线先跑。".into(),
            "[ExpertRole: data_scientist] 数据科学家\n思维框架: 问题建模 → 数据探索(EDA) → 特征工程 → 基线模型 → 迭代优化 → 可解释性分析。\n行为约束:\n- 评估指标必须匹配业务目标（精度还是召回率？为什么？）。\n- 数据泄露检查: 训练集中禁止包含未来信息。\n- 模型复杂度 vs 效果的对比：简单基线模型必须先跑。\n- 不确定性量化: 置信区间和检验集表现必须展示。\n工具偏好: code.execute（Rhai 计算）、db.query（算法指标）、kb.query（学术方法）。".into(),
        ));
    }

    // 产品经理
    if lower.contains("prd") || lower.contains("用户故事") || lower.contains("user story")
        || lower.contains("需求文档") || lower.contains("product requirement")
        || lower.contains("迭代计划") || lower.contains("sprint planning") || lower.contains("mvp")
    {
        return Some((
            "product_manager".into(),
            "[product_manager] 痛点定义→范围边界→RICE评估→验收标准。需求MECE分层，用户故事三段式。".into(),
            "[ExpertRole: product_manager] 产品经理\n思维框架: 用户痛点定义 → 问题范围边界 → 方案评估（RICE/ICE）→ 验收标准 → 上线日期。\n行为约束:\n- 需求必须区分: 功能需求 vs 约束条件，每条采用 MECE 层次写。\n- 带出成功标准（定量）和验证方法同时展示。\n- 用户故事格式: 作为「角色」，我希望「行为」，以便「价値」。\n- 优先级必须伴随依据：数据估算、技术成本、业务影响。\n工具偏好: kb.query（竞品分析）、fs.read（现有需求文档）。".into(),
        ));
    }

    // DevOps
    if lower.contains("docker") || lower.contains("kubernetes") || lower.contains("k8s")
        || lower.contains("流水线") || lower.contains("pipeline") || lower.contains("helm")
        || lower.contains("terraform") || lower.contains("ansible")
    {
        return Some((
            "devops_engineer".into(),
            "[devops_engineer] 环境差异→流水线→幂等性→回滚安全网→可观测性。镜像版本锁定，non-root运行。".into(),
            "[ExpertRole: devops_engineer] DevOps / 基础设施工程师\n思维框架: 环境差异分析 → 流水线设计 → 幂等性验证 → 回滚安全网 → 可观测性。\n行为约束:\n- Dockerfile 必须指定基础镜像版本，使用最小权限 non-root 用户。\n- 幂等性验证: 重复执行 apply 必须无副作用。\n- 销毁操作（drain/delete）必须附带回滚方案和记录备份验证。\n- 资源定频和限制必须同时展示: CPU/内存/并发三个维度。\n工具偏好: fs.read（配置文件）、bash.exec（验证命令）、kb.query（最佳实践）。".into(),
        ));
    }

    // 代码审查
    if lower.contains("代码审查") || lower.contains("code review") || lower.contains("代码质量")
        || lower.contains("技术债") || lower.contains("technical debt")
        || lower.contains("重构方案") || lower.contains("solid原则") || lower.contains("solid principle")
    {
        return Some((
            "code_reviewer".into(),
            "[code_reviewer] 正确性→可读性→性能→安全→测试覆盖。评论三级(Blocker/Major/Nit)，重构必评成本收益。".into(),
            "[ExpertRole: code_reviewer] 代码审查専家\n思维框架: 正确性 → 延伸性和可读性 → 性能隐患 → 安全风险 → 测试覆盖。\n行为约束:\n- 审查评论分三级: Blocker（阅考前必修）／Major（建议修）／Nit（风格建议）。\n- 必须指出为什么是问题，不能只说\"应该改\"。\n- 建议重构时必须评估: 改动成本 × 风险 ÷ 收益。\n- 覆盖测试评估: 单元测试对领域概念的覆盖率是否足够。\n工具偏好: lsp.find_references（引用分析）、lsp.call_hierarchy_incoming（调用链）。".into(),
        ));
    }

    // 金融分析（精确）
    if lower.contains("dcf") || lower.contains("irr") || lower.contains("ebitda")
        || lower.contains("金融建模") || lower.contains("估値模型") || lower.contains("valuation model")
        || lower.contains("财务报表") || lower.contains("利润表") || lower.contains("资产负债表")
    {
        return Some((
            "financial_analyst".into(),
            "[financial_analyst] 数据质检→可比分析→假设设定→敏感性分析→情景建模。指标必对标行业，区分公告/估算数据。".into(),
            "[ExpertRole: financial_analyst] 金融分析师\n思维框架: 财务数据质量检验 → 可比性分析 → 假设设定 → 模型橁鸟 → 敖事性和数字并行。\n行为约束:\n- 关键指标必须附带行业平均对标。\n- 假设必须明确标注并进行敏感性分析。\n- 不确定性必须用情景分析（基准/乐观/悲观）表现。\n- 标注数据来源时间戳，区分公告数据和估算数据。\n工具偏好: code.execute（DCF/估值计算）、db.query（财务数据）。".into(),
        ));
    }

    // 法务合规
    if lower.contains("gdpr") || lower.contains("合规性") || lower.contains("数据主权")
        || lower.contains("开源许可") || lower.contains("license") || lower.contains("隐私政策")
        || lower.contains("regulation") || lower.contains("法律验证")
    {
        return Some((
            "legal_compliance".into(),
            "[legal_compliance] 法规映射→场景确认→差距分析→修复优先级。附条款编号+生效日期，禁止具体法律建议。".into(),
            "[ExpertRole: legal_compliance] 法务合规专家\n思维框架: 法规映射 → 应用场景确认 → 差距分析 → 修复方案优先级 → 封项记录。\n行为约束:\n- 定论必须附属具体法律条款编号和生效日期。\n- 法律风险估算必须展示为最小化路径和接受路径两种方案。\n- 隐私数据识别: 区分 PII/敏感数据/匹名化数据，说明处理要求。\n- 禁止给出属于法律领域范围的具体建议，请务必提示孙证。\n工具偏好: kb.query（法规知识库）、fs.read（合同和指南文件）。".into(),
        ));
    }

    // 技术写作
    if lower.contains("api文档") || lower.contains("openapi") || lower.contains("swagger")
        || lower.contains("技术规范") || lower.contains("changelog") || lower.contains("接口文档")
        || lower.contains("api spec") || lower.contains("文档注释") || lower.contains("docstring")
    {
        return Some((
            "tech_writer".into(),
            "[tech_writer] 读者画像→信息层次→示例第一→可发现性。API文档必含请求/响应/错误码，示例必须可运行。".into(),
            "[ExpertRole: tech_writer] 技术文档専家\n思维框架: 读者画像 → 信息层次设计 → 示例第一 → 可发现性 → 小处细节与大局观并行。\n行为约束:\n- API 文档必须包含: 请求示例、响应示例、错误码表三部分。\n- README 结构: 快速开始(一命令) → 核心功能 → 配置项 → FAQ。\n- 长度安全处理: 单一概念 ≤ 300 字，超出时分页。\n- 每个示例必须是可运行的，不允许出现 `your-value-here` 占位符。\n工具偏好: fs.read（现有代码理解接口）、lsp.hover（类型提取）。".into(),
        ));
    }

    // 通用金融分析（单词匹配）
    if lower.contains("金融") || lower.contains("财务") || lower.contains("量化")
        || lower.contains("finance") || lower.contains("financial")
    {
        return Some((
            "financial_analyst".into(),
            "[financial_analyst] 问题定义→数据收集→量化建模→验证→阈值配置。结论附数据来源+时间界，情景分析非单点预测。".into(),
            "[ExpertRole: financial_analyst] 金融分析师\n思维框架: 定义问题 → 收集制度数据 → 量化模型架构 → 迟饢性验证 → 阈值配置。\n行为约束:\n- 每个金融结论必须附带数据来源和时间界。\n- 关键指标必须对标市场基准/行业均値。\n- 不确定性必须用情景分析展现而非单点预测。\n- 标注数据时间戳和来源（市场数据还是公序数据）。\n工具偏好: code.execute（金融模型计算）、db.query（历史数据）。".into(),
        ));
    }

    // 通用安全（单词）
    if lower.contains("安全") || lower.contains("security") || lower.contains("加密")
        || lower.contains("认证") || lower.contains("权限")
    {
        return Some((
            "security_analyst".into(),
            "[security_analyst] 威胁建模→攻击面→防御控制→深层防御。拒绝指导攻击，加密必附密钥管理方案。".into(),
            "[ExpertRole: security_analyst] 安全专家\n思维框架: 威胁建模 → 攻击面识别 → 防御控制评估 → 深层防御设计。\n行为约束:\n- 点出安全问题时必须同时给出缓解方案。\n- 永远提供防御视角，拒绝指导实际攻击。\n- 加密建议必须指明使用场景和密鑰管理方案。\n- 输入验证: 必须指出验证层级（客户端还是服务端）和拒绝策略。\n工具偏好: fs.grep（模式扫描）、kb.query（安全知识库）。".into(),
        ));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── 专家角色注入器测试 ─────────────────────────────────────────────

    #[test]
    fn test_expert_role_trading() {
        let mut injector = DynamicInjector::new();
        injector.register_expert_role_injector();
        let segs = injector.inject("我需要设计一个回测系统，需要注意 sharpe 和仓位管理", &Value::Null);
        assert!(!segs.is_empty(), "应该注入交易策略角色");
        assert!(segs[0].text.contains("trading_strategist"), "应匹配 trading_strategist");
        assert!(segs[0].kind == SegmentKind::Knowledge, "应为 Knowledge 类型（跨 turn 持久）");
    }

    #[test]
    fn test_expert_role_security() {
        let mut injector = DynamicInjector::new();
        injector.register_expert_role_injector();
        let segs = injector.inject("这个API接口有 SQL注入漏洞，请分析安全风险", &Value::Null);
        assert!(!segs.is_empty());
        assert!(segs[0].text.contains("security_analyst"));
    }

    #[test]
    fn test_expert_role_architecture() {
        let mut injector = DynamicInjector::new();
        injector.register_expert_role_injector();
        // 分布式系统触发系统架构师
        let segs = injector.inject("设计一个分布式系统，采用微服务和事件溯源模式", &Value::Null);
        assert!(!segs.is_empty());
        assert!(segs[0].text.contains("system_architect"));
    }

    #[test]
    fn test_expert_role_data_science() {
        let mut injector = DynamicInjector::new();
        injector.register_expert_role_injector();
        let segs = injector.inject("我要训练一个神经网络，需要避免过拟合和特征工程优化", &Value::Null);
        assert!(!segs.is_empty());
        assert!(segs[0].text.contains("data_scientist"));
    }

    #[test]
    fn test_expert_role_code_review() {
        let mut injector = DynamicInjector::new();
        injector.register_expert_role_injector();
        let segs = injector.inject("对这段代码做代码审查，需要检查 SOLID原则和技术债", &Value::Null);
        assert!(!segs.is_empty());
        assert!(segs[0].text.contains("code_reviewer"));
    }

    #[test]
    fn test_expert_role_devops() {
        let mut injector = DynamicInjector::new();
        injector.register_expert_role_injector();
        let segs = injector.inject("如何用 helm 将服务部署到 kubernetes 集群", &Value::Null);
        assert!(!segs.is_empty());
        assert!(segs[0].text.contains("devops_engineer"));
    }

    #[test]
    fn test_expert_role_product() {
        let mut injector = DynamicInjector::new();
        injector.register_expert_role_injector();
        let segs = injector.inject("写一个用户故事：作为进阶用户的 PRD 文档模板", &Value::Null);
        assert!(!segs.is_empty());
        assert!(segs[0].text.contains("product_manager"));
    }

    #[test]
    fn test_expert_role_tech_writing() {
        let mut injector = DynamicInjector::new();
        injector.register_expert_role_injector();
        let segs = injector.inject("为这个 API 生成 openapi 规范的 api文档", &Value::Null);
        assert!(!segs.is_empty());
        assert!(segs[0].text.contains("tech_writer"));
    }

    #[test]
    fn test_expert_role_no_match() {
        let mut injector = DynamicInjector::new();
        injector.register_expert_role_injector();
        // 通用对话不触发任何角色
        let segs = injector.inject("今天天气怎么样？", &Value::Null);
        assert!(segs.is_empty(), "闲聊不应注入专家角色");
    }

    #[test]
    fn test_expert_role_persists_across_turns() {
        // 专家角色使用 Knowledge 类型——跨 turn 持久化验证
        let mut injector = DynamicInjector::new();
        injector.register_expert_role_injector();
        injector.inject("分析这个代码的安全漏洞", &Value::Null);
        // 第二 turn 即使输入无关键词，也能通过 active_knowledge 取到安全角色
        let active = injector.active_knowledge();
        assert!(!active.is_empty(), "安全角色应持久化到 active_knowledge");
        assert!(active[0].text.contains("security_analyst"));
    }

    // ─── 原有内置注入测试 ─────────────────────────────────────────────

    #[test]
    fn test_topic_injection() {
        let mut injector = DynamicInjector::new();
        injector.register_defaults();

        let segments = injector.inject("how do I use async tokio in rust?", &Value::Null);
        assert!(!segments.is_empty(), "expected topic injection for rust");
        assert!(segments[0].text.contains("Rust"));
    }

    #[test]
    fn test_no_injection_for_irrelevant() {
        let mut injector = DynamicInjector::new();
        injector.register_defaults();

        let segments = injector.inject("hello world", &Value::Null);
        assert!(segments.is_empty(), "expected no injection for irrelevant input");
    }

    #[test]
    fn test_tool_result_injection() {
        let mut injector = DynamicInjector::new();
        injector.register_defaults();

        let ctx = serde_json::json!({"tool_results": [{"tool": "filengine_fs_read", "status": "ok"}]});
        let segments = injector.inject("check the file", &ctx);
        assert!(!segments.is_empty());
        // 大小写按 stable 文本来——KV cache 修复后改成 "Tool results available..."（无 count）
        assert!(segments.iter().any(|s| s.text.contains("Tool results available")));
    }

    /// KV cache 回归：tool_result 段在不同 count 下文本必须 byte-identical。
    /// 修复历史：之前 text 用 `format!("{} tool results", count)` 嵌入 count，
    /// 每轮 N 变化 → Layer 180 Knowledge 段 byte 变化 → DeepSeek 前缀 cache 命中
    /// 从 Layer 180 起整段失效（Kernel/abacusbr/Strategy/Constraints/subscenes 之后全部 miss）。
    /// 现在文本固定，跨 turn 同情境下 dedup 短路、active_knowledge 不再 byte-drift。
    #[test]
    fn tool_result_text_stable_across_counts() {
        let mut injector = DynamicInjector::new();
        injector.register_defaults();

        let ctx_a = serde_json::json!({"tool_results": [{"tool": "filengine_fs_read"}]});
        let ctx_b = serde_json::json!({"tool_results": [
            {"tool": "filengine_fs_read"}, {"tool": "filengine_fs_write"}, {"tool": "kb_query"},
            {"tool": "code_execute"}, {"tool": "db_query"}
        ]});

        // 复制器跑两轮（每次新建 injector 避免 active_knowledge 副作用扰动）
        let segs_a = {
            let mut i = DynamicInjector::new();
            i.register_defaults();
            i.inject("debug this", &ctx_a)
        };
        let segs_b = {
            let mut i = DynamicInjector::new();
            i.register_defaults();
            i.inject("debug this", &ctx_b)
        };

        let text_a: Vec<_> = segs_a.iter()
            .filter(|s| s.text.starts_with("[Injector] Tool results"))
            .map(|s| s.text.as_str())
            .collect();
        let text_b: Vec<_> = segs_b.iter()
            .filter(|s| s.text.starts_with("[Injector] Tool results"))
            .map(|s| s.text.as_str())
            .collect();

        assert_eq!(text_a.len(), 1, "应有 1 个 tool_result 段（count=1）");
        assert_eq!(text_b.len(), 1, "应有 1 个 tool_result 段（count=5）");
        assert_eq!(text_a[0], text_b[0],
            "tool_result 段文本必须 byte-identical 跨 count——这是 Layer 180 cache 命中的前提");

        // 二次校验：同一 injector 上两次 inject() 后，active_knowledge 不该被 dedup 触发替换
        let mut injector = DynamicInjector::new();
        injector.register_defaults();
        injector.inject("debug this", &ctx_a);
        let snap_a: String = injector.active_knowledge().iter()
            .map(|s| s.text.clone()).collect::<Vec<_>>().join("|");
        injector.inject("debug this", &ctx_b);
        let snap_b: String = injector.active_knowledge().iter()
            .map(|s| s.text.clone()).collect::<Vec<_>>().join("|");
        assert_eq!(snap_a, snap_b,
            "active_knowledge 跨 turn 必须 byte-identical（输入与情境一致时）—— \
             否则 dedup 触发替换，KV 前缀失效");
    }
}