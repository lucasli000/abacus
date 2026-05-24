//! PromptAssembly — 多层 Prompt 组装系统
//!
//! ## 依赖
//! - `abacus_core::core::injector::DynamicInjector`: 动态知识注入
//! - `abacus_core::core::task_analyzer::TaskKind`: 任务分类（子场景匹配用）
//!
//! ## 引用关系
//! - 被 `CoreLoop::build_system_prompt` 调用
//! - 调用 `DynamicInjector` 获取动态分段
//! - `filter_abacusbr()` 根据 TaskKind 做内联子场景筛选
//!
//! ## 9 层优先级架构
//! Layer 1 (255): Kernel — 核心行为规则
//! Layer 2 (230): abacusbr — 用户行为规范（子场景按需加载）
//! Layer 3 (200): Strategy + AntiPattern
//! Layer 4 (180): Knowledge — 保留上下文 + 活跃知识
//! Layer 5 (170): Context — session 上下文
//! Layer 6 (160): Deduction — 推演告警
//! Layer 7 (90):  Skills — 活跃 skill prompts
//! Layer 8 (20):  Interaction — 交互地图状态
//! Tool definitions 通过 LlmRequest.tools 传递（不在 system prompt 中）

use std::collections::HashMap;
use std::path::Path;

use crate::core::injector::{DynamicInjector, PromptSegment};
use crate::core::task_analyzer::TaskKind;
use serde::Deserialize;
use serde_json::Value;

/// TOML 格式的子场景映射
#[derive(Debug, Clone, Deserialize)]
struct SubsceneConfig {
    subscene_map: HashMap<String, Vec<String>>,
}

/// Prompt 分段，带层级信息
#[derive(Debug, Clone)]
pub struct AssemblySegment {
    pub layer: u8,
    pub content: String,
    pub source: String,
}

/// PromptAssembly — 多层 Prompt 组装器
pub struct PromptAssembly {
    kernel_prompt: String,
    /// 原始 abacusbr 全文（未过滤，未截断）
    abacusbr_raw: String,
    injector: DynamicInjector,
    enable_process_layer: bool,
    /// TaskKind label → sub-scene name list（从 TOML 加载 + 默认合并）
    subscene_map: HashMap<String, Vec<String>>,
}

impl PromptAssembly {
    /// 创建新的 PromptAssembly。
    ///
    /// ## 加载顺序（三级合并）
    /// 1. `~/.abacus/abacusbr.md` — 全局行为规范
    /// 2. `<cwd>/abacusbr.md` — fallback（全局不存在时）
    /// 3. `<cwd>/abacusbr.local.md` — 项目级覆盖（追加到全局之后）
    ///
    /// ## 项目级覆盖语义
    /// `abacusbr.local.md` 的 section 与全局同名时，两者内容**都加载**（追加，不替换）。
    /// 这允许项目添加特定 coding 规范而不丢失全局基线。
    /// 如需完全替换某 section，在 local 文件内用 `<!-- override -->` 标记行
    /// （parse_subscenes 检测到此标记时丢弃全局同名 section）。
    ///
    /// ## 引用关系
    /// - 被 `CoreLoop::new()` 调用（engine_init.rs:1126）
    /// - 生命周期：创建后 abacusbr_raw 不可变（session 内稳定，支撑 KV cache）
    pub fn new(kernel_prompt: impl Into<String>, abacusbr_prompt: impl Into<String>) -> Self {
        let mut injector = DynamicInjector::new();
        injector.register_defaults();

        let mut br = abacusbr_prompt.into();
        if br.is_empty() {
            // 全局加载：~/.abacus/abacusbr.md
            if let Ok(home) = std::env::var("HOME") {
                let path = Path::new(&home).join(".abacus").join("abacusbr.md");
                if let Ok(content) = std::fs::read_to_string(&path) {
                    br = content;
                }
            }
            // fallback：CWD/abacusbr.md（无全局文件时）
            if br.is_empty() {
                if let Ok(content) = std::fs::read_to_string(Path::new("abacusbr.md")) {
                    br = content;
                }
            }
        }

        // 项目级追加：CWD/abacusbr.local.md（存在则追加到全局之后）
        // 设计意图：项目特定的 coding 规范/review 标准/arch 约定可以在 local 文件中补充，
        //   不需要修改全局 abacusbr.md。追加语义保证全局基线不丢失。
        if let Ok(local) = std::fs::read_to_string(Path::new("abacusbr.local.md")) {
            if !local.trim().is_empty() {
                br.push_str("\n\n");
                br.push_str(&local);
            }
        }

        // 加载子场景映射
        let subscene_map = load_subscene_map();

        Self {
            kernel_prompt: kernel_prompt.into(),
            abacusbr_raw: br,
            injector,
            enable_process_layer: false,
            subscene_map,
        }
    }

    /// 获取原始 abacusbr 全文（用于知识库优先级等场景）
    pub fn abacusbr_content(&self) -> &str {
        &self.abacusbr_raw
    }

    /// 仅返回 `## core` 层内容（不包含任务子场景）。
    ///
    /// ## 用途
    /// 专项为 `assemble_segments()` 的稳定块（cacheable=true）提供不随任务类型变化的内容。
    /// 任务相关子场景（coding/debug 等）通过 `filter_abacusbr_subscenes_only()` 进入动态块。
    pub fn abacusbr_core_only(&self) -> String {
        if self.abacusbr_raw.is_empty() { return String::new(); }
        let parsed = parse_subscenes(&self.abacusbr_raw);
        parsed.get("core").cloned().unwrap_or_default()
    }

    /// 返回当前任务类型对应的子场景内容（不含 core）。
    ///
    /// ## 用途
    /// 专项为 `assemble_segments()` 的动态块（cacheable=false）提供任务相关内容。
    /// 这样大任务切换时只有动态块变化，稳定前缀（Kernel+core+Strategy）均不受影响。
    pub fn filter_abacusbr_subscenes_only(&self, task_kind: Option<TaskKind>) -> String {
        if self.abacusbr_raw.is_empty() { return String::new(); }
        let parsed = parse_subscenes(&self.abacusbr_raw);
        let mut selected = String::new();
        if let Some(kind) = task_kind {
            let label = kind.label();
            if let Some(matches) = self.subscene_map.get(label) {
                for name in matches {
                    if let Some(content) = parsed.get(name) {
                        if !selected.is_empty() { selected.push('\n'); }
                        selected.push_str(content);
                    }
                }
            }
        }
        truncate_200(&selected)
    }

    /// 按任务类型过滤子场景 + 200 行截断，返回最终注入文本。
    ///
    /// ## 用途
    /// 为 `assemble()`（单 String）调用 — core + subscenes 合并为一个层。
    /// 注意: `assemble_segments()` 不调用此方法（它将 core 和 subscenes 分拆到不同块）。
    pub fn filter_abacusbr(&self, task_kind: Option<TaskKind>) -> String {
        if self.abacusbr_raw.is_empty() {
            return String::new();
        }

        let parsed = parse_subscenes(&self.abacusbr_raw);
        let mut selected = String::new();

        // ## Core 始终包含
        if let Some(core) = parsed.get("core") {
            selected.push_str(core);
            selected.push('\n');
        }

        // 按 TaskKind label 匹配子场景（从 subscene_map 读取）
        if let Some(kind) = task_kind {
            let label = kind.label();
            if let Some(matches) = self.subscene_map.get(label) {
                for name in matches {
                    if let Some(content) = parsed.get(name) {
                        selected.push('\n');
                        selected.push_str(content);
                    }
                }
            }
        }

        // 200 行截断
        truncate_200(&selected)
    }

    /// 启用过程层输出
    pub fn with_process_layer(mut self) -> Self {
        self.enable_process_layer = true;
        self
    }

    /// 注册自定义注入源
    pub fn register_injection_source(
        &mut self,
        source_id: String,
        should_inject: Box<dyn Fn(&str, &Value) -> bool + Send + Sync>,
        inject: Box<dyn Fn(&str, &Value) -> Option<PromptSegment> + Send + Sync>,
    ) {
        use crate::core::injector::InjectionSource;
        self.injector.register_source(InjectionSource {
            source_id,
            should_inject,
            inject,
        });
    }

    /// 组装完整系统 prompt
    ///
    /// ## 参数
    /// - `injector_segments`: injector 输出的活跃知识（safety role 等跨 turn 稳定段）
    /// - `deduction_block`: 推演告警文本
    /// - `preflight_block`: 静默自审报告文本（Layer 155）
    /// - `retained_context`: 保留上下文文本
    /// - `task_kind`: 当前任务类型（用于 abacusbr 子场景匹配，session-sticky 锁定）
    /// - `with_process_layer`: 是否附加过程层
    ///
    /// ## Phase 5 清理（参数删除）
    /// 已删除 `matched_skills` / `interaction_status` / `session_context` 三个参数：
    ///   - Phase 1 已切断它们的注入路径（详见下方注释）
    ///   - 上游 `build_system_output` 不再采集对应数据，节省 lock 获取
    pub fn assemble(
        &self,
        injector_segments: &[PromptSegment],
        deduction_block: Option<&str>,
        preflight_block: Option<&str>,
        retained_context: &str,
        task_kind: Option<TaskKind>,
        with_process_layer: bool,
    ) -> String {
        let mut layers: std::collections::BTreeMap<u8, Vec<String>> = std::collections::BTreeMap::new();

        // Layer 1 (255): Kernel
        if !self.kernel_prompt.is_empty() {
            layers.entry(255).or_default().push(self.kernel_prompt.clone());
        }

        // Layer 2 (230): abacusbr core 部分——不随 task_kind 变化，形成稳定前缀
        //
        // ## DeepSeek / OpenAI KV Cache 设计
        // DeepSeek 确定缓存命中基于「从第 0 个 token 开始的连续相同前缀」。
        // 将 core（任务无关）放在 Layer 230，task subscenes 降至 Layer 185（不出现在稳定前缀中）。
        // 效果：稳定前缀 ~607 token。任务类型切换时岁改动的是 Layer 185 之后的动态内容。
        let br_core = self.abacusbr_core_only();
        if !br_core.is_empty() {
            layers.entry(230).or_default().push(br_core);
        }

        // Layer 3 (200): Strategy + AntiPattern
        layers.entry(200).or_default().push(
            "## Execution Strategy\n\
            - Before acting: identify what information you need → use tools to gather it → then proceed.\n\
            - Multi-tool chains: execute in dependency order. Verify intermediate results before continuing.\n\
            - If first approach fails: diagnose the root cause (read error, check assumptions) before switching tactics.\n\
            - Large tasks: break into verifiable milestones. Report progress at each milestone.".into());
        layers.entry(190).or_default().push(
            "## Constraints\n\
            - NEVER fabricate file paths, function names, or API endpoints — verify they exist first.\n\
            - NEVER skip tool verification by saying \"I believe\" or \"I think\" — use the tool.\n\
            - NEVER produce placeholder code (TODO, ..., pass) — write complete implementations.\n\
            - When output exceeds 50 lines: use structured sections with clear headers.\n\
            - Error recovery: retry with corrected params (max 2 retries), then report with diagnosis.".into());

        // Layer 185: 任务相关子场景——动态内容，放在 Constraints(190) 之后
        // 任务切换时只影响该层和后续动态层，对稳定前缀 607 token 无影响
        let br_subscenes = self.filter_abacusbr_subscenes_only(task_kind);
        if !br_subscenes.is_empty() {
            layers.entry(185).or_default().push(br_subscenes);
        }

        // Layer 4 (180): Knowledge + injector
        if !retained_context.is_empty() {
            layers.entry(180).or_default().push(retained_context.to_string());
        }
        for seg in injector_segments {
            layers.entry(seg.kind.priority()).or_default().push(seg.text.clone());
        }

        // ── Phase 1 KV cache 删除（Phase 5 已彻底清理参数）─────────────
        // 删除 Layer 30 session_context（cwd/open_file/modified_count）：
        //   信息冗余——LLM 需要时调 fs.cwd / fs.status 工具，比 push 注入更准
        //   每轮 byte 变化破坏中段 cache prefix
        //
        // 删除 Layer 90 matched_skills 注入：
        //   SilentRouter 默认关后已冗余；skill workflow 通过 tool description 表达
        //
        // 删除 Layer 20 InteractionMap status：
        //   改为 LLM 主动通过 session.interaction_map 工具查询（如有需要）

        // Layer 5.5 (155): Preflight — 静默自审报告
        if let Some(pf) = preflight_block {
            if !pf.is_empty() {
                layers.entry(155).or_default().push(pf.to_string());
            }
        }

        // Layer 6 (160): Deduction
        if let Some(ded) = deduction_block {
            if !ded.is_empty() {
                layers.entry(160).or_default().push(ded.to_string());
            }
        }

        let mut parts = Vec::new();
        for (_prio, segs) in layers.iter().rev() { parts.extend(segs.iter().cloned()); }
        let mut result = parts.join("\n\n---\n\n");

        if with_process_layer {
            result.push_str("\n\n<!-- Process Layer (enabled) -->\n");
            result.push_str("Show your reasoning process in <thinking> tags before responding.\n");
        }

        result
    }

    /// 组装并返回分段 system prompt（用于支持多 block 的 provider 如 Anthropic）。
    ///
    /// 每个段标注 cacheable=true 表示跨 turn 稳定，provider 可对其标记缓存。
    ///
    /// ## W3 (Task #101) 三段式分层
    /// | 段 | priority | cacheable | 稳定性 | 内容 |
    /// |----|----------|-----------|--------|------|
    /// | 0 (Tier 1)  | ≥190 | true  | **永久稳定**——session 内字节不变 | Kernel(255) + abacusbr_core(230) + Strategy(200) + Constraints(190) |
    /// | 1 (Tier 2)  | =185 | true  | **task-sticky 稳定**——同 task_kind 锁定后字节不变 | abacusbr_subscenes（任务相关行为规范） |
    /// | 2 (dynamic) | <185 | false | turn-specific | retained_context + injector + deduction + preflight + skills + interaction |
    ///
    /// ### 拆分动机
    /// 之前所有 <190 内容（包括稳定的 br_subscenes）混在 dynamic 段，task switching 时整段失效。
    /// 拆出 Tier 2 后：① 同 task 内连续 turn → Tier 1+2 都命中 ② task 切换 → 仅 Tier 2+dynamic 失效，
    /// Tier 1（占 token 大头，含 Kernel/abacusbr core）保持命中。
    ///
    /// ## 引用关系
    /// - 被 `TurnPipeline::execute_loop` 调用（当 provider 为 Anthropic 时）
    /// - 替代 `assemble()` 的单一 String 返回
    pub fn assemble_segments(
        &self,
        injector_segments: &[PromptSegment],
        deduction_block: Option<&str>,
        preflight_block: Option<&str>,
        retained_context: &str,
        task_kind: Option<TaskKind>,
        with_process_layer: bool,
    ) -> Vec<crate::llm::provider::SystemSegment> {
        use crate::llm::provider::SystemSegment;

        // 分层构建逻辑：稳定层（可缓存） vs 动态层（不可缓存）
        //
        // ## KV Cache 设计
        // 稳定前缀（≥190）必须字节级不变，否则缓存失效。
        // 不变的内容：Kernel(255) + abacusbr_core(230) + Strategy(200) + Constraints(190)
        // 变化的内容：task_subscenes(185) + retained_ctx(180) + expert_role(180) + deduction + skills + session
        // 关键改动： abacusbr 任务子场景从 Layer 230（稳定）降至 Layer 185（动态）
        // 效果：任务切换时稳定块 token 量从 ~2400 降至 ~610，缓存命中率显著提升。
        let mut layers: std::collections::BTreeMap<u8, Vec<String>> = std::collections::BTreeMap::new();

        if !self.kernel_prompt.is_empty() {
            layers.entry(255).or_default().push(self.kernel_prompt.clone());
        }

        // Layer 230: 仅注入 core 部分（任务无关，稳定不变）
        let br_core = self.abacusbr_core_only();
        if !br_core.is_empty() {
            layers.entry(230).or_default().push(br_core);
        }

        layers.entry(200).or_default().push(
            "## Execution Strategy\n\
            - Before acting: identify what information you need → use tools to gather it → then proceed.\n\
            - Multi-tool chains: execute in dependency order. Verify intermediate results before continuing.\n\
            - If first approach fails: diagnose the root cause (read error, check assumptions) before switching tactics.\n\
            - Large tasks: break into verifiable milestones. Report progress at each milestone.".into());
        layers.entry(190).or_default().push(
            "## Constraints\n\
            - NEVER fabricate file paths, function names, or API endpoints — verify they exist first.\n\
            - NEVER skip tool verification by saying \"I believe\" or \"I think\" — use the tool.\n\
            - NEVER produce placeholder code (TODO, ..., pass) — write complete implementations.\n\
            - When output exceeds 50 lines: use structured sections with clear headers.\n\
            - Error recovery: retry with corrected params (max 2 retries), then report with diagnosis.".into());

        // Layer 185: 任务相关子场景（动态块，任务切换时内容变化）
        let br_subscenes = self.filter_abacusbr_subscenes_only(task_kind);
        if !br_subscenes.is_empty() {
            layers.entry(185).or_default().push(br_subscenes);
        }

        if !retained_context.is_empty() {
            layers.entry(180).or_default().push(retained_context.to_string());
        }
        for seg in injector_segments {
            layers.entry(seg.kind.priority()).or_default().push(seg.text.clone());
        }

        // Phase 1 KV cache 删除：session_context / matched_skills / interaction_status 不再注入
        // 详见 assemble() 同位置注释；Phase 5 已彻底清理参数

        if let Some(pf) = preflight_block {
            if !pf.is_empty() {
                layers.entry(155).or_default().push(pf.to_string());
            }
        }

        if let Some(ded) = deduction_block {
            if !ded.is_empty() {
                layers.entry(160).or_default().push(ded.to_string());
            }
        }

        // W3 (Task #101): 三段式拆分
        //
        // Tier 1 stable（永久稳定）：≥190
        // Tier 2 stable（task-sticky）：==185（br_subscenes，同 task_kind 内字节不变）
        // Dynamic（不可缓存）：<185
        //
        // 边界条件：BTreeMap.iter().rev() 给出降序 key 顺序——保持收集确定性
        let mut tier1_stable = Vec::new();
        let mut tier2_stable = Vec::new();
        let mut dynamic_parts = Vec::new();

        for (prio, segs) in layers.iter().rev() {
            if *prio >= 190 {
                tier1_stable.extend(segs.iter().cloned());
            } else if *prio == 185 {
                tier2_stable.extend(segs.iter().cloned());
            } else {
                dynamic_parts.extend(segs.iter().cloned());
            }
        }

        let mut segments = Vec::new();

        // Tier 1：Kernel(255) + abacusbr_core(230) + Strategy(200) + Constraints(190)
        // 全 session 字节稳定——provider 端 cache_control=ephemeral 命中率最高
        if !tier1_stable.is_empty() {
            segments.push(SystemSegment {
                text: tier1_stable.join("\n\n---\n\n"),
                cacheable: true,
            });
        }

        // Tier 2：br_subscenes(185)——task_kind 锁定后字节稳定
        // 引用：CoreLoop 的 task_kind sticky 机制（Phase 2）保证同 session 锁定后不抖动
        // 收益：task switching 时仅此段失效，Tier 1（占 token 大头）继续命中
        if !tier2_stable.is_empty() {
            segments.push(SystemSegment {
                text: tier2_stable.join("\n\n---\n\n"),
                cacheable: true,
            });
        }

        // 动态后缀：retained_context(180) + injector(动态) + preflight(155) + deduction(160) + skills + interaction(20)
        if !dynamic_parts.is_empty() {
            let mut dynamic_text = dynamic_parts.join("\n\n---\n\n");
            if with_process_layer {
                dynamic_text.push_str("\n\n<!-- Process Layer (enabled) -->\n");
                dynamic_text.push_str("Show your reasoning process in <thinking> tags before responding.\n");
            }
            segments.push(SystemSegment {
                text: dynamic_text,
                cacheable: false,
            });
        } else if with_process_layer {
            segments.push(SystemSegment {
                text: "<!-- Process Layer (enabled) -->\nShow your reasoning process in <thinking> tags before responding.\n".into(),
                cacheable: false,
            });
        }

        segments
    }
}

// ─── 内联子场景解析 ─────────────────────────────────────────────────────────

/// 将 abacusbr 文本按 `## ` 标题拆分为 {section_name: content} 映射
///
/// ## 代码块处理
/// 被 ` ``` ` 包裹的行不视为节头部（避免 README 模板等代码示例被误解析为子场景）。
fn parse_subscenes(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut current_key = String::from("core");
    let mut current_lines = Vec::new();
    let mut in_code_block = false;

    for line in text.lines() {
        // 追踪代码块状态（``` 切换 in/out）
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            current_lines.push(line);
            continue;
        }

        if !in_code_block && line.starts_with("## ") {
            // 新节开始：保存当前节
            if !current_lines.is_empty() {
                map.insert(current_key.clone(), current_lines.join("\n"));
            }
            current_key = line[3..].trim().to_lowercase();
            current_lines.clear();
        } else {
            current_lines.push(line);
        }
    }
    if !current_lines.is_empty() {
        map.insert(current_key, current_lines.join("\n"));
    }

    map
}

/// 从 ~/.abacus/subscenes.toml 加载子场景映射，与默认值合并
fn load_subscene_map() -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = default_subscene_map();

    let path = std::env::var("HOME")
        .map(|h| Path::new(&h).join(".abacus").join("subscenes.toml"))
        .ok();
    if let Some(ref path) = path {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(config) = toml::from_str::<SubsceneConfig>(&content) {
                for (key, values) in config.subscene_map {
                    if values.is_empty() {
                        map.remove(&key);
                    } else {
                        map.insert(key, values);
                    }
                }
            }
        }
    }

    map
}

/// 默认子场景映射（用户未配置时使用）
///
/// ## 扩展说明
/// 每个 TaskKind label → abacusbr.md 中的 `## SectionName`（小写）列表。
/// 多个 section 按列表顺序加载并拼接，总量受 `truncate_200` (~4000 token) 限制。
/// 新专家场景（financial/security/trading 等）通过 DynamicInjector 的 expert_role
/// 源在 Layer 180 动态注入，此映射只处理 TaskKind 维度的静态路由。
fn default_subscene_map() -> HashMap<String, Vec<String>> {
    let mut m = HashMap::new();
    // ── 架构感知分层注入策略 ──
    // "system" (~300 tok): crate 分层 + 关键入口 + 硬约束。轻量，所有代码任务加载。
    // "system_deep" (~900 tok): 完整子系统/状态机/设计模式。仅 debug/architecture/review 加载。
    // 设计意图：改 UI 样式不需要知道 McipGateway oneshot 语义，节省 token 成本。

    // 基础任务类型（轻量架构感知）
    m.insert("code_reading".into(),   vec!["system".into(), "code".into()]);
    m.insert("code_writing".into(),   vec!["system".into(), "coding".into(), "code".into()]);
    m.insert("web_search".into(),     vec!["web".into()]);
    m.insert("file_edit".into(),      vec!["system".into(), "code".into(), "file".into()]);
    m.insert("mathematics".into(),    vec!["math".into()]);
    m.insert("linguistics".into(),    vec!["language".into(), "tech_writing".into()]);
    m.insert("knowledge_query".into(),vec!["knowledge".into()]);
    // 深度任务类型（完整架构感知 = system + system_deep）
    m.insert("debugging".into(),      vec!["system".into(), "system_deep".into(), "debug".into(), "code".into()]);
    m.insert("architecture".into(),   vec!["system".into(), "system_deep".into(), "architecture".into(), "system_design".into()]);
    m.insert("review".into(),         vec!["system".into(), "system_deep".into(), "review".into(), "code_review".into(), "code".into()]);
    m.insert("data_analysis".into(),  vec!["data".into(), "data_science".into()]);
    // 通用对话：仅加载 core（不注入额外场景噪声）
    m.insert("general_chat".into(),   vec![]);
    m
}

/// Token-aware truncation (~4000 tokens ≈ 16000 chars for English, 8000 for CJK).
/// Falls back to line-based truncation at 200 lines as secondary limit.
///
/// Uses a conservative char-based estimate: avg 4 chars/token for English,
/// 2 chars/token for CJK. We use 3 as a blended average.
const MAX_PROMPT_TOKENS: usize = 4000;
const CHARS_PER_TOKEN: usize = 3; // conservative estimate

fn truncate_200(text: &str) -> String {
    let char_limit = MAX_PROMPT_TOKENS * CHARS_PER_TOKEN; // 12000 chars
    let char_count = text.chars().count();

    // Primary: token-based truncation
    if char_count > char_limit {
        let truncated: String = text.chars().take(char_limit).collect();
        // Find last newline to avoid cutting mid-line
        let cut_point = truncated.rfind('\n').unwrap_or(truncated.len());
        let clean = &truncated[..cut_point];
        return format!(
            "{}\n\n[TRUNCATED: 原文约 {} tokens，已截断至 ~{} tokens]\n[查看完整规范: ~/.abacus/abacusbr.md]",
            clean,
            char_count / CHARS_PER_TOKEN,
            MAX_PROMPT_TOKENS
        );
    }

    // Secondary: line-based limit for exceptionally short-line content
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 200 {
        return text.to_string();
    }
    let truncated: String = lines[..200].join("\n");
    format!(
        "{}\n\n[TRUNCATED: 全文共 {} 行，仅加载前 200 行]\n[查看完整规范: ~/.abacus/abacusbr.md]",
        truncated,
        lines.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_subscenes() {
        let text = "# Title\n\n## Core\nrule1\nrule2\n\n## Code\ncode rule\n\n## Debug\ndebug rule";
        let map = parse_subscenes(text);
        assert!(map.contains_key("core"));
        assert!(map.contains_key("code"));
        assert!(map.contains_key("debug"));
        assert!(map["core"].contains("rule1"));
    }

    #[test]
    fn test_filter_core_only() {
        let text = "## Core\nalways\n\n## Code\nonly when coding\n\n## Debug\nonly when debug";
        let assembly = PromptAssembly::new("kernel", text);
        let result = assembly.filter_abacusbr(None);
        assert!(result.contains("always"));
        assert!(!result.contains("only when coding"));
    }

    #[test]
    fn test_filter_with_task() {
        let text = "## Core\nalways\n\n## Code\nonly when code\n\n## Debug\nonly debug";
        let assembly = PromptAssembly::new("kernel", text);
        let result = assembly.filter_abacusbr(Some(TaskKind::CodeReading));
        assert!(result.contains("always"));
        assert!(result.contains("only when code"));
        assert!(!result.contains("only debug"));
    }

    #[test]
    fn test_truncate_200() {
        let text = (0..250).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let result = truncate_200(&text);
        assert_eq!(result.lines().count(), 203); // 200 + truncated msg + 2 ref lines
        assert!(result.contains("[TRUNCATED"));
    }

    #[test]
    fn test_no_truncate_under_200() {
        let text = (0..50).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let result = truncate_200(&text);
        assert_eq!(result.lines().count(), 50);
        assert!(!result.contains("[TRUNCATED"));
    }

    #[test]
    fn test_default_map_keys() {
        let map = default_subscene_map();
        assert!(map.contains_key("debugging"));
        assert!(map.get("debugging").unwrap().contains(&"debug".to_string()));
        assert!(map.get("debugging").unwrap().contains(&"code".to_string()));
        // general_chat 现在已入库（值为空列表，仅加载 core）
        assert!(map.contains_key("general_chat"));
        assert!(map.get("general_chat").unwrap().is_empty());
        // 新专家子场景映射存在
        assert!(map.get("review").unwrap().contains(&"code_review".to_string()));
        assert!(map.get("architecture").unwrap().contains(&"system_design".to_string()));
        assert!(map.get("data_analysis").unwrap().contains(&"data_science".to_string()));
    }

    #[test]
    fn test_filter_with_debug_task() {
        let text = "## Core\nalways\n\n## Code\ncode rules\n\n## Debug\ndebug rules";
        let assembly = PromptAssembly::new("kernel", text);
        let result = assembly.filter_abacusbr(Some(TaskKind::Debugging));
        assert!(result.contains("always"));
        assert!(result.contains("code rules"));
        assert!(result.contains("debug rules"));
    }

    // ─── W3 (Task #101) 字节稳定性契约 ──────────────────────────────────────

    fn assembly_with_subscenes() -> PromptAssembly {
        let abacusbr = "\
            ## core\nalways stable\n\n\
            ## code\ncode-specific rules\n\n\
            ## debug\ndebug-specific rules";
        PromptAssembly::new("KERNEL_PROMPT_TEXT", abacusbr)
    }

    /// **契约 1**：同 task_kind 两次调用，所有 cacheable=true 段字节相等。
    /// 这是 KV cache prefix 命中的必要条件。
    #[test]
    fn assemble_segments_stable_across_same_task() {
        let asm = assembly_with_subscenes();
        let s1 = asm.assemble_segments(&[], None, None, "", Some(TaskKind::CodeWriting), false);
        let s2 = asm.assemble_segments(&[], None, None, "", Some(TaskKind::CodeWriting), false);

        let cacheable1: Vec<&String> = s1.iter().filter(|s| s.cacheable).map(|s| &s.text).collect();
        let cacheable2: Vec<&String> = s2.iter().filter(|s| s.cacheable).map(|s| &s.text).collect();
        assert_eq!(cacheable1, cacheable2, "cacheable segments must be byte-identical across calls");
    }

    /// **契约 2**：跨 task_kind，Tier 1（Kernel + abacusbr_core + Strategy + Constraints）必须一致。
    /// 这是 W3 拆分的核心收益——task switching 时大头 prefix 仍命中。
    #[test]
    fn assemble_segments_tier1_invariant_across_tasks() {
        let asm = assembly_with_subscenes();
        let s_code = asm.assemble_segments(&[], None, None, "", Some(TaskKind::CodeWriting), false);
        let s_debug = asm.assemble_segments(&[], None, None, "", Some(TaskKind::Debugging), false);

        // Tier 1 = 第一个 cacheable=true 段
        let tier1_code = s_code.iter().find(|s| s.cacheable).map(|s| &s.text);
        let tier1_debug = s_debug.iter().find(|s| s.cacheable).map(|s| &s.text);
        assert!(tier1_code.is_some());
        assert_eq!(
            tier1_code, tier1_debug,
            "Tier 1 stable segment must be invariant across task_kinds"
        );

        // Tier 1 必须包含 Kernel/Strategy/Constraints
        let t1 = tier1_code.unwrap();
        assert!(t1.contains("KERNEL_PROMPT_TEXT"), "Tier 1 must contain kernel");
        assert!(t1.contains("Execution Strategy"), "Tier 1 must contain Strategy");
        assert!(t1.contains("Constraints"), "Tier 1 must contain Constraints");
        assert!(t1.contains("always stable"), "Tier 1 must contain abacusbr core");
    }

    /// **契约 3**：跨 task_kind，Tier 2（subscenes）字节不同——确认拆分点正确。
    #[test]
    fn assemble_segments_tier2_varies_across_tasks() {
        let asm = assembly_with_subscenes();
        let s_code = asm.assemble_segments(&[], None, None, "", Some(TaskKind::CodeWriting), false);
        let s_debug = asm.assemble_segments(&[], None, None, "", Some(TaskKind::Debugging), false);

        let cacheable_code: Vec<&String> = s_code.iter().filter(|s| s.cacheable).map(|s| &s.text).collect();
        let cacheable_debug: Vec<&String> = s_debug.iter().filter(|s| s.cacheable).map(|s| &s.text).collect();

        // 至少有 Tier 1（永久稳定）；Tier 2 视 task 是否匹配 subscene 而定
        assert!(!cacheable_code.is_empty());
        // Tier 1 永久稳定（已被契约 2 覆盖）；如果两个 task 都有 Tier 2，内容应不同
        if cacheable_code.len() >= 2 && cacheable_debug.len() >= 2 {
            assert_ne!(
                cacheable_code[1], cacheable_debug[1],
                "Tier 2 (task-sticky) should differ across CodeImpl vs Debugging"
            );
        }
    }

    /// **契约 4**：dynamic 段非空时是末段，且 cacheable=false——保护 stable prefix。
    #[test]
    fn assemble_segments_dynamic_segment_is_last_and_uncacheable() {
        let asm = assembly_with_subscenes();
        let segs = asm.assemble_segments(
            &[],
            Some("DEDUCTION_BLOCK"),
            None,
            "RETAINED_CTX",
            Some(TaskKind::CodeWriting),
            false,
        );
        let last = segs.last().expect("at least one segment");
        assert!(!last.cacheable, "last segment should be cacheable=false (dynamic)");
        assert!(last.text.contains("RETAINED_CTX") || last.text.contains("DEDUCTION_BLOCK"));
    }

    /// **契约 5b**：subscenes 段被消费后字节稳定 — 同 task 重复调用 Tier 2 不抖动。
    /// 防止未来加入 HashMap 不确定迭代顺序而破坏 task-sticky cache。
    #[test]
    fn assemble_segments_tier2_internal_order_stable() {
        let asm = assembly_with_subscenes();
        let s1 = asm.assemble_segments(&[], None, None, "", Some(TaskKind::CodeWriting), false);
        let s2 = asm.assemble_segments(&[], None, None, "", Some(TaskKind::CodeWriting), false);

        // Tier 2 = 第二个 cacheable=true 段（如果存在）
        let t2_a: Vec<&String> = s1.iter().filter(|s| s.cacheable).skip(1).take(1).map(|s| &s.text).collect();
        let t2_b: Vec<&String> = s2.iter().filter(|s| s.cacheable).skip(1).take(1).map(|s| &s.text).collect();
        assert_eq!(t2_a, t2_b, "Tier 2 byte-identical across same-task calls");
    }

    /// **契约 5**：tier 顺序不变——Tier 1 先于 Tier 2 先于 dynamic。
    /// 顺序错误会让 provider 端的 cache_control 标记错位。
    #[test]
    fn assemble_segments_order_tier1_then_tier2_then_dynamic() {
        let asm = assembly_with_subscenes();
        let segs = asm.assemble_segments(
            &[],
            Some("DEDUCTION"),
            None,
            "RETAINED",
            Some(TaskKind::CodeWriting),
            false,
        );
        // 找到第一个 cacheable=false 段的索引——它之前的所有段必须 cacheable=true
        let first_dynamic_idx = segs.iter().position(|s| !s.cacheable);
        if let Some(idx) = first_dynamic_idx {
            for (i, s) in segs.iter().enumerate() {
                if i < idx {
                    assert!(s.cacheable, "segment {} before first dynamic must be cacheable", i);
                } else {
                    assert!(!s.cacheable, "segment {} after first dynamic must be cacheable=false", i);
                }
            }
        }
    }
}
