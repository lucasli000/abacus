//! Phase 1: 模型能力 catalog（内置已知模型表，Phase 3 增加 YAML 覆盖路径）。
//!
//! ## 职责边界
//! - **回答**：「model X 的 thinking 能力是什么？context window 多大？」
//! - **不回答**：「model X 应该路由到哪个 provider？」（这是 ProviderGroup 的职责）
//!
//! ## 引用关系
//! - 创建：`engine_init.rs` / `server.rs` 启动时调用 `ModelCatalog::builtin()`，Phase 3 起再
//!   `.merge_yaml(~/.abacus/models.yaml)`
//! - 注入：`CoreLoop::new()` 接收 `Arc<ModelCatalog>`
//! - 消费：pipeline 调用 `core.model_catalog.lookup(&effective_model)` 获取
//!   `Arc<ModelSpec>`，再用 `spec.thinking_capabilities` 决定 resolver 分支
//!
//! ## 生命周期
//! - 创建：CoreLoop 启动时一次（merge YAML 一次性）
//! - 销毁：进程退出（Arc::drop）
//!
//! ## 内置 vs YAML
//! - 内置（本文件 `builtin()`）：维护 Abacus 知道的主流模型。版本升级随 release 滚动。
//! - YAML 覆盖（Phase 3）：用户自有 endpoint / 第三方代理可覆盖任意模型 spec，无需改源码。

use abacus_types::{
    EffortLevel, LatencyTier, ModelId, ModelSpec, ModelThinkingConfig, MultiTurnReplay,
    RateLimits, SchemaFormat, ThinkingCapabilities, ThinkingEffort, ThinkingModeKind,
};
use std::collections::HashMap;
use std::sync::Arc;

/// 模型能力 catalog。线程安全（Arc 持有），lookup 返回 Arc<ModelSpec> 零拷贝共享。
#[derive(Debug, Clone)]
pub struct ModelCatalog {
    specs: HashMap<ModelId, Arc<ModelSpec>>,
}

impl ModelCatalog {
    /// 空 catalog——主要用于测试场景注入「无任何已知模型」状态
    pub fn empty() -> Self {
        Self { specs: HashMap::new() }
    }

    /// 内置已知模型表。版本随 Abacus release 维护。
    ///
    /// ## 模型范围
    /// - Anthropic：Opus 4.7 / Opus 4.6 / Sonnet 4.6 / Haiku 4 / Sonnet 4.5（旧）
    /// - DeepSeek：V4 Flash / V4 Pro / V3.2 / Reasoner
    /// - OpenAI：GPT-5 / GPT-5 Mini / o3 / o3-mini
    /// - Gemini：2.5 Pro / 2.5 Flash / 2.5 Flash-Lite（仅 capability，provider 实装在 Phase 5）
    pub fn builtin() -> Self {
        let mut specs: HashMap<ModelId, Arc<ModelSpec>> = HashMap::new();

        // ─── Anthropic ─────────────────────────────────────────────────────
        // Opus 4.7：唯一支持 AdaptiveEffort，旧 ExtendedBudget 直接拒收返 400
        specs.insert(
            ModelId("claude-opus-4-7".into()),
            Arc::new(make_spec_anthropic_adaptive_only(
                vec![EffortLevel::Low, EffortLevel::Medium, EffortLevel::High,
                     EffortLevel::Max, EffortLevel::XHigh],
                200_000, 64_000,
            )),
        );

        // Opus 4.6 / Sonnet 4.6：双支持（Adaptive 推荐，ExtendedBudget 兼容）
        for model_id in ["claude-opus-4-6", "claude-sonnet-4-6"] {
            specs.insert(
                ModelId(model_id.into()),
                Arc::new(make_spec_anthropic_dual(
                    vec![EffortLevel::Low, EffortLevel::Medium, EffortLevel::High, EffortLevel::Max],
                    (1024, 64000),
                    200_000, 32_000,
                )),
            );
        }

        // Sonnet 4.5 / Opus 4.5 / Haiku 4：仅 ExtendedBudget
        for model_id in ["claude-sonnet-4-5", "claude-opus-4-5", "claude-haiku-4"] {
            specs.insert(
                ModelId(model_id.into()),
                Arc::new(make_spec_anthropic_extended_only(
                    (1024, 64000),
                    200_000, 16_384,
                )),
            );
        }

        // ─── DeepSeek ─────────────────────────────────────────────────────
        // V4 / V3.x / Reasoner：OpenAI 格式仅 EnabledToggle；多轮强制 ReasoningContent 回传
        for model_id in [
            "deepseek-v4-flash", "deepseek-v4-pro", "deepseek-v4",
            "deepseek-v3.2", "deepseek-v3.1", "deepseek-chat", "deepseek-reasoner",
        ] {
            specs.insert(
                ModelId(model_id.into()),
                Arc::new(make_spec_deepseek(model_id)),
            );
        }

        // ─── OpenAI ───────────────────────────────────────────────────────
        // GPT-5 系列：AdaptiveEffort + minimal 档位
        for model_id in ["gpt-5", "gpt-5-mini", "gpt-5-nano"] {
            specs.insert(
                ModelId(model_id.into()),
                Arc::new(make_spec_openai_with_minimal(
                    vec![EffortLevel::Minimal, EffortLevel::Low, EffortLevel::Medium, EffortLevel::High],
                    400_000, 128_000,
                )),
            );
        }
        // o3 / o3-mini / o1：无 minimal 档位
        for model_id in ["o3", "o3-mini", "o1", "o4-mini"] {
            specs.insert(
                ModelId(model_id.into()),
                Arc::new(make_spec_openai_classic_reasoning(
                    vec![EffortLevel::Low, EffortLevel::Medium, EffortLevel::High],
                    200_000, 100_000,
                )),
            );
        }

        // ─── Gemini（capability 占位，provider 实装在 Phase 5）───────────────
        specs.insert(
            ModelId("gemini-2.5-pro".into()),
            Arc::new(make_spec_gemini((128, 32_768), 1_000_000, 8_192)),
        );
        specs.insert(
            ModelId("gemini-2.5-flash".into()),
            Arc::new(make_spec_gemini((1, 24_576), 1_000_000, 8_192)),
        );
        specs.insert(
            ModelId("gemini-2.5-flash-lite".into()),
            Arc::new(make_spec_gemini((512, 24_576), 1_000_000, 8_192)),
        );

        // ─── 智谱 GLM（OpenAI-compatible，thinking 走 EnabledToggle）──────────
        // 参考: https://docs.z.ai/guides/llm/glm-5.1
        // GLM-5.1: 200K context, 16K output, 旗舰（SWE-bench Pro 开源第一）
        // GLM-5: 200K context, 16K output, 编程 SOTA
        // GLM-5-Turbo: 200K context, 16K output, Agent 专用（高频工具调用）
        // GLM-5.1-highspeed: 200K context, 16K output, 400 tok/s 高速版
        for model_id in ["glm-5.1", "glm-5.1-highspeed", "glm-5", "glm-5-turbo"] {
            specs.insert(
                ModelId(model_id.into()),
                Arc::new(make_spec_glm_thinking(200_000, 16_384)),
            );
        }
        // GLM-4.7 / GLM-4.6: 128K, thinking
        for model_id in ["glm-4.7", "glm-4.6", "glm-4-plus", "glm-4-long"] {
            specs.insert(
                ModelId(model_id.into()),
                Arc::new(make_spec_glm_thinking(128_000, 8_192)),
            );
        }
        // GLM-4-Flash 系列: 128K, 无 thinking
        for model_id in ["glm-4-flash", "glm-4-flashx", "glm-4-air"] {
            specs.insert(
                ModelId(model_id.into()),
                Arc::new(base_spec(128_000, 4_096)),
            );
        }

        // ─── Kimi/Moonshot（OpenAI-compatible）────────────────────────────────
        // 参考: https://platform.kimi.ai/docs/guide/kimi-k2-6-quickstart
        // Kimi K2.6: 262K context, 1T MoE (32B active), 旗舰（多模态+Agent）
        // Kimi K2 Thinking: 256K context, 深度推理版（200-300 tool calls 稳定）
        // Kimi K2: 256K context, 初代 1T MoE
        specs.insert(
            ModelId("kimi-k2.6".into()),
            Arc::new(make_spec_kimi_thinking(262_144, 16_384)),
        );
        specs.insert(
            ModelId("kimi-k2-thinking".into()),
            Arc::new(make_spec_kimi_thinking(256_000, 16_384)),
        );
        for model_id in ["kimi-k2", "kimi-latest"] {
            specs.insert(
                ModelId(model_id.into()),
                Arc::new(make_spec_kimi_thinking(256_000, 8_192)),
            );
        }
        // Moonshot 旧系列: 无 thinking
        for model_id in ["moonshot-v1-128k", "moonshot-v1-32k", "moonshot-v1-8k"] {
            let cw = if model_id.contains("128k") { 128_000 }
                else if model_id.contains("32k") { 32_000 }
                else { 8_000 };
            specs.insert(
                ModelId(model_id.into()),
                Arc::new(base_spec(cw, 4_096)),
            );
        }

        Self { specs }
    }

    /// 精确查询。未命中返回 None。
    pub fn lookup(&self, id: &ModelId) -> Option<Arc<ModelSpec>> {
        self.specs.get(id).cloned()
    }

    /// 查询；未命中返回保守默认（不支持 thinking）。
    /// 用于运行时未声明的实验性模型——不阻塞调用，但 thinking 自动降级。
    pub fn lookup_or_default(&self, id: &ModelId) -> Arc<ModelSpec> {
        self.lookup(id).unwrap_or_else(|| Arc::new(ModelSpec::default()))
    }

    /// 注入/覆盖单条 spec（YAML merge 路径或测试构造）
    pub fn insert(&mut self, id: ModelId, spec: ModelSpec) {
        self.specs.insert(id, Arc::new(spec));
    }

    /// Phase 3：从 YAML 文件合并 per-model 覆盖。
    ///
    /// ## YAML schema
    /// ```yaml
    /// models:
    ///   claude-opus-4-7:
    ///     context_window: 200000          # 可选；缺省走内置
    ///     max_output_tokens: 64000        # 可选
    ///     thinking_capabilities:          # 可选；整段覆盖（不是 patch）
    ///       supported_modes: ["adaptive_effort"]
    ///       default_mode: "adaptive_effort"
    ///       effort_levels: ["low", "medium", "high", "max", "xhigh"]
    ///       budget_range: null
    ///       multi_turn_replay: "none"
    ///   custom-local-model:               # 用户自有模型
    ///     context_window: 32000
    ///     thinking_capabilities:
    ///       supported_modes: ["enabled_toggle"]
    ///       default_mode: "enabled_toggle"
    ///       multi_turn_replay: "reasoning_content"
    /// ```
    ///
    /// ## 合并策略
    /// - 文件不存在 → Ok(()) 静默（用户没配置）
    /// - 文件存在但 `models` 段缺省 → Ok(()) 静默
    /// - YAML 解析失败 → Err（不让错误的 YAML 静默吃掉）
    /// - per-model：YAML 中存在 → **整条 ModelSpec 覆盖**（含 thinking_capabilities）
    /// - per-model：YAML 中字段缺省 → 用内置 spec 的对应字段填充
    ///
    /// ## 返回值
    /// 合并后被覆盖/新增的模型数（用于 startup 日志）
    pub fn merge_yaml(&mut self, path: &std::path::Path) -> std::io::Result<usize> {
        if !path.exists() {
            return Ok(0);
        }

        let raw = std::fs::read_to_string(path)?;
        let parsed: serde_yaml::Value = serde_yaml::from_str(&raw)
            .map_err(|e| std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("YAML parse error in {}: {}", path.display(), e),
            ))?;

        let Some(models_section) = parsed.get("models").and_then(|v| v.as_mapping()) else {
            return Ok(0);
        };

        let mut count = 0usize;
        for (k, v) in models_section {
            let Some(name) = k.as_str() else { continue };
            let id = ModelId(name.to_string());
            // 起点：内置 spec（若有）或默认空 spec
            let mut spec = self.lookup(&id)
                .map(|arc| (*arc).clone())
                .unwrap_or_default();

            // 合并字段
            if let Some(cw) = v.get("context_window").and_then(|x| x.as_u64()) {
                spec.context_window = cw as usize;
            }
            if let Some(mt) = v.get("max_output_tokens").and_then(|x| x.as_u64()) {
                spec.max_output_tokens = mt as u32;
            }
            if let Some(caps_v) = v.get("thinking_capabilities") {
                if let Some(caps) = parse_thinking_capabilities_yaml(caps_v) {
                    spec.thinking_capabilities = caps;
                }
            }

            self.specs.insert(id, Arc::new(spec));
            count += 1;
        }

        Ok(count)
    }

    /// 从 ProviderEntry 的 ModelEntry 合并模型参数到 catalog
    ///
    /// ## 引用关系
    /// - 调用方: engine_init.rs 注册 provider 时
    /// - 读取: per-model 参数覆盖 ModelCatalog 内置默认值
    ///
    /// ## 参数
    /// - `entry`: 模型配置条目
    /// - `_provider_id`: 所属 provider ID（预留，用于未来按 provider 分组查询）
    pub fn merge_model_entry(&mut self, entry: &abacus_types::ModelEntry, _provider_id: Option<&str>) {
        let id = ModelId(entry.name.clone());
        let mut spec = self.lookup(&id)
            .map(|arc| (*arc).clone())
            .unwrap_or_default();

        if let Some(cw) = entry.context_window {
            spec.context_window = cw as usize;
        }
        if let Some(mt) = entry.max_tokens {
            spec.max_output_tokens = mt;
        }
        // thinking: 将字符串转为 ThinkingCapabilities
        // off → 清空 supported_modes; adaptive/low/medium/high/max → 设置对应模式
        if let Some(ref thinking_str) = entry.thinking {
            use abacus_types::{ThinkingModeKind, ThinkingCapabilities, MultiTurnReplay, EffortLevel};
            match thinking_str.as_str() {
                "off" => {
                    spec.thinking_capabilities = ThinkingCapabilities::default();
                }
                "adaptive" => {
                    spec.thinking_capabilities.supported_modes = vec![ThinkingModeKind::AdaptiveEffort];
                }
                level @ ("low" | "medium" | "high" | "max" | "xhigh") => {
                    spec.thinking_capabilities.supported_modes = vec![ThinkingModeKind::ExtendedBudget];
                    let eff = match level {
                        "low" => EffortLevel::Low,
                        "medium" => EffortLevel::Medium,
                        "high" => EffortLevel::High,
                        _ => EffortLevel::XHigh,
                    };
                    // 保持 Low → Medium → High → XHigh 顺序，去重
                    let mut levels = vec![EffortLevel::Low, EffortLevel::Medium, EffortLevel::High];
                    if !levels.contains(&eff) { levels.push(eff); }
                    spec.thinking_capabilities.effort_levels = levels;
                }
                _ => {} // 未知值忽略
            }
        }
        self.specs.insert(id, Arc::new(spec));
    }

    /// 已注册模型数（不区分内置 vs 覆盖）
    pub fn len(&self) -> usize { self.specs.len() }
    pub fn is_empty(&self) -> bool { self.specs.is_empty() }

    /// 列出所有已知 ModelId（顺序不保证）
    pub fn ids(&self) -> impl Iterator<Item = &ModelId> { self.specs.keys() }
}

/// 解析 YAML 中的 `thinking_capabilities` 子节
fn parse_thinking_capabilities_yaml(v: &serde_yaml::Value) -> Option<ThinkingCapabilities> {
    let mapping = v.as_mapping()?;

    let supported_modes: Vec<ThinkingModeKind> = mapping.get("supported_modes")
        .and_then(|x| x.as_sequence())
        .map(|seq| seq.iter()
            .filter_map(|s| s.as_str().and_then(parse_mode_kind))
            .collect())
        .unwrap_or_default();

    let default_mode = mapping.get("default_mode")
        .and_then(|x| x.as_str())
        .and_then(parse_mode_kind);

    let effort_levels: Vec<EffortLevel> = mapping.get("effort_levels")
        .and_then(|x| x.as_sequence())
        .map(|seq| seq.iter()
            .filter_map(|s| s.as_str().and_then(parse_effort_level))
            .collect())
        .unwrap_or_default();

    let budget_range = mapping.get("budget_range").and_then(|br| {
        let seq = br.as_sequence()?;
        if seq.len() == 2 {
            let min = seq[0].as_u64()? as u32;
            let max = seq[1].as_u64()? as u32;
            Some((min, max))
        } else {
            None
        }
    });

    let multi_turn_replay = mapping.get("multi_turn_replay")
        .and_then(|x| x.as_str())
        .map(|s| match s {
            "reasoning_content" => MultiTurnReplay::ReasoningContent,
            "signature" => MultiTurnReplay::Signature,
            _ => MultiTurnReplay::None,
        })
        .unwrap_or(MultiTurnReplay::None);

    Some(ThinkingCapabilities {
        supported_modes,
        default_mode,
        effort_levels,
        budget_range,
        multi_turn_replay,
    })
}

fn parse_mode_kind(s: &str) -> Option<ThinkingModeKind> {
    match s {
        "enabled_toggle" => Some(ThinkingModeKind::EnabledToggle),
        "extended_budget" => Some(ThinkingModeKind::ExtendedBudget),
        "adaptive_effort" => Some(ThinkingModeKind::AdaptiveEffort),
        "budget_int" => Some(ThinkingModeKind::BudgetInt),
        _ => None,
    }
}

fn parse_effort_level(s: &str) -> Option<EffortLevel> {
    match s {
        "minimal" => Some(EffortLevel::Minimal),
        "low" => Some(EffortLevel::Low),
        "medium" => Some(EffortLevel::Medium),
        "high" => Some(EffortLevel::High),
        "max" => Some(EffortLevel::Max),
        "xhigh" | "x-high" => Some(EffortLevel::XHigh),
        _ => None,
    }
}

impl Default for ModelCatalog {
    fn default() -> Self { Self::builtin() }
}

// ─── 内部构造 helpers（保持 Phase 1 范围内职责单一）───────────────────────

fn base_spec(context_window: usize, max_output_tokens: u32) -> ModelSpec {
    ModelSpec {
        max_temperature: 1.0,
        min_temperature: 0.0,
        max_top_p: 1.0,
        max_n: 1,
        max_stop_sequences: 4,
        max_tools: 64,
        supported_schemas: vec![SchemaFormat::Text, SchemaFormat::JsonObject, SchemaFormat::JsonSchema],
        rate_limits: RateLimits::default(),
        latency_tier: LatencyTier::Interactive,
        context_window,
        max_output_tokens,
        thinking_config: ModelThinkingConfig::default(),
        thinking_capabilities: ThinkingCapabilities::none(),
    }
}

/// Anthropic Opus 4.7 流派：仅 AdaptiveEffort（旧 ExtendedBudget 拒收）
fn make_spec_anthropic_adaptive_only(
    levels: Vec<EffortLevel>,
    context_window: usize,
    max_output_tokens: u32,
) -> ModelSpec {
    let mut s = base_spec(context_window, max_output_tokens);
    s.thinking_capabilities = ThinkingCapabilities {
        supported_modes: vec![ThinkingModeKind::AdaptiveEffort],
        default_mode: Some(ThinkingModeKind::AdaptiveEffort),
        effort_levels: levels,
        budget_range: None,
        multi_turn_replay: MultiTurnReplay::None,
    };
    s
}

/// Anthropic Opus 4.6 / Sonnet 4.6 流派：双支持（Adaptive 优先）
fn make_spec_anthropic_dual(
    levels: Vec<EffortLevel>,
    budget_range: (u32, u32),
    context_window: usize,
    max_output_tokens: u32,
) -> ModelSpec {
    let mut s = base_spec(context_window, max_output_tokens);
    s.thinking_capabilities = ThinkingCapabilities {
        supported_modes: vec![
            ThinkingModeKind::AdaptiveEffort,
            ThinkingModeKind::ExtendedBudget,
        ],
        default_mode: Some(ThinkingModeKind::AdaptiveEffort),
        effort_levels: levels,
        budget_range: Some(budget_range),
        multi_turn_replay: MultiTurnReplay::None,
    };
    s
}

/// Anthropic Sonnet 4.5 / Opus 4.5 / Haiku 4：仅 ExtendedBudget
fn make_spec_anthropic_extended_only(
    budget_range: (u32, u32),
    context_window: usize,
    max_output_tokens: u32,
) -> ModelSpec {
    let mut s = base_spec(context_window, max_output_tokens);
    s.thinking_capabilities = ThinkingCapabilities {
        supported_modes: vec![ThinkingModeKind::ExtendedBudget],
        default_mode: Some(ThinkingModeKind::ExtendedBudget),
        effort_levels: Vec::new(),
        budget_range: Some(budget_range),
        multi_turn_replay: MultiTurnReplay::None,
    };
    s
}

/// DeepSeek 流派：OpenAI 格式只接受 enabled/disabled；多轮要求 ReasoningContent 回传
fn make_spec_deepseek(model_id: &str) -> ModelSpec {
    let mut s = base_spec(1_000_000, 8_192);
    // ThinkingEffort 兼容字段（旧路径）
    s.thinking_config = ModelThinkingConfig {
        enabled: false,
        effort: Some(ThinkingEffort::High),
        preserve_thinking: matches!(model_id, "deepseek-reasoner"),
    };
    // DeepSeek V4 thinking capability（参考官方文档 https://api-docs.deepseek.com/guides/thinking_mode）：
    // - supported_modes：除 EnabledToggle 外也接受 effort 控制（reasoning_effort 字段）
    // - effort_levels：服务端真实差异化档位仅 high / max；
    //   官方说明 low/medium 会被 server-side aliased 到 high，xhigh 被 aliased 到 max。
    //   仍把全档位列在这里——resolver 不要因此降级；client 侧的 clamp 在 deepseek.rs 完成
    //   （让 wire 上发的 effort 字符串与 server 真实执行档位一致，避免监控/日志失真）。
    // - multi_turn_replay：含 tool_call 的多轮强制回传 reasoning_content（缺失则 400）
    s.thinking_capabilities = ThinkingCapabilities {
        supported_modes: vec![
            ThinkingModeKind::EnabledToggle,
            ThinkingModeKind::AdaptiveEffort,
        ],
        default_mode: Some(ThinkingModeKind::EnabledToggle),
        effort_levels: vec![
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Max,
            EffortLevel::XHigh,
        ],
        budget_range: None,
        multi_turn_replay: MultiTurnReplay::ReasoningContent,
    };
    s
}

/// OpenAI GPT-5 流派：AdaptiveEffort + Minimal 档位
fn make_spec_openai_with_minimal(
    levels: Vec<EffortLevel>,
    context_window: usize,
    max_output_tokens: u32,
) -> ModelSpec {
    let mut s = base_spec(context_window, max_output_tokens);
    s.thinking_capabilities = ThinkingCapabilities {
        supported_modes: vec![ThinkingModeKind::AdaptiveEffort],
        default_mode: Some(ThinkingModeKind::AdaptiveEffort),
        effort_levels: levels,
        budget_range: None,
        multi_turn_replay: MultiTurnReplay::None,
    };
    s
}

/// OpenAI o3/o1 经典 reasoning 流派：低/中/高三档
fn make_spec_openai_classic_reasoning(
    levels: Vec<EffortLevel>,
    context_window: usize,
    max_output_tokens: u32,
) -> ModelSpec {
    let mut s = base_spec(context_window, max_output_tokens);
    s.thinking_capabilities = ThinkingCapabilities {
        supported_modes: vec![ThinkingModeKind::AdaptiveEffort],
        default_mode: Some(ThinkingModeKind::AdaptiveEffort),
        effort_levels: levels,
        budget_range: None,
        multi_turn_replay: MultiTurnReplay::None,
    };
    s
}

/// 2026-05-28: 智谱 GLM 流派（带 thinking）
/// OpenAI-compatible API；thinking 走 EnabledToggle；多轮回传 reasoning_content
fn make_spec_glm_thinking(context_window: usize, max_output_tokens: u32) -> ModelSpec {
    let mut s = base_spec(context_window, max_output_tokens);
    s.thinking_capabilities = ThinkingCapabilities {
        supported_modes: vec![ThinkingModeKind::EnabledToggle],
        default_mode: Some(ThinkingModeKind::EnabledToggle),
        effort_levels: vec![EffortLevel::High], // GLM 无显式档位，默认 high
        budget_range: None,
        multi_turn_replay: MultiTurnReplay::ReasoningContent,
    };
    s
}

/// 2026-05-28: Kimi/Moonshot 流派（带 thinking）
/// OpenAI-compatible API；k1/k2 thinking 走 EnabledToggle；reasoning_content 回传
fn make_spec_kimi_thinking(context_window: usize, max_output_tokens: u32) -> ModelSpec {
    let mut s = base_spec(context_window, max_output_tokens);
    s.thinking_capabilities = ThinkingCapabilities {
        supported_modes: vec![ThinkingModeKind::EnabledToggle],
        default_mode: Some(ThinkingModeKind::EnabledToggle),
        effort_levels: vec![EffortLevel::High], // Kimi 无显式档位
        budget_range: None,
        multi_turn_replay: MultiTurnReplay::ReasoningContent,
    };
    s
}

/// Gemini 流派：BudgetInt（-1=动态、0=禁用、范围内整数）
fn make_spec_gemini(
    budget_range: (u32, u32),
    context_window: usize,
    max_output_tokens: u32,
) -> ModelSpec {
    let mut s = base_spec(context_window, max_output_tokens);
    s.thinking_capabilities = ThinkingCapabilities {
        supported_modes: vec![ThinkingModeKind::BudgetInt],
        default_mode: Some(ThinkingModeKind::BudgetInt),
        effort_levels: Vec::new(),
        budget_range: Some(budget_range),
        multi_turn_replay: MultiTurnReplay::None,
    };
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_contains_known_models() {
        let cat = ModelCatalog::builtin();
        // 抽样验证六家覆盖
        assert!(cat.lookup(&ModelId("claude-opus-4-7".into())).is_some());
        assert!(cat.lookup(&ModelId("deepseek-v4-pro".into())).is_some());
        assert!(cat.lookup(&ModelId("gpt-5".into())).is_some());
        assert!(cat.lookup(&ModelId("gemini-2.5-pro".into())).is_some());
        assert!(cat.lookup(&ModelId("glm-4.7".into())).is_some());
        assert!(cat.lookup(&ModelId("kimi-k2".into())).is_some());
    }

    #[test]
    fn test_glm_5_1_thinking_support() {
        let cat = ModelCatalog::builtin();
        let spec = cat.lookup(&ModelId("glm-5.1".into())).unwrap();
        assert_eq!(spec.context_window, 200_000);
        assert_eq!(spec.max_output_tokens, 16_384);
        assert!(spec.thinking_capabilities.supported_modes.contains(&ThinkingModeKind::EnabledToggle));
        assert_eq!(spec.thinking_capabilities.multi_turn_replay, MultiTurnReplay::ReasoningContent);
    }

    #[test]
    fn test_glm_4_flash_no_thinking() {
        let cat = ModelCatalog::builtin();
        let spec = cat.lookup(&ModelId("glm-4-flash".into())).unwrap();
        assert_eq!(spec.context_window, 128_000);
        assert!(spec.thinking_capabilities.supported_modes.is_empty());
    }

    #[test]
    fn test_kimi_k2_6_thinking_support() {
        let cat = ModelCatalog::builtin();
        let spec = cat.lookup(&ModelId("kimi-k2.6".into())).unwrap();
        assert_eq!(spec.context_window, 262_144);
        assert_eq!(spec.max_output_tokens, 16_384);
        assert!(spec.thinking_capabilities.supported_modes.contains(&ThinkingModeKind::EnabledToggle));
        assert_eq!(spec.thinking_capabilities.multi_turn_replay, MultiTurnReplay::ReasoningContent);
    }

    #[test]
    fn test_kimi_k2_thinking_256k() {
        let cat = ModelCatalog::builtin();
        let spec = cat.lookup(&ModelId("kimi-k2-thinking".into())).unwrap();
        assert_eq!(spec.context_window, 256_000);
    }

    #[test]
    fn test_moonshot_no_thinking() {
        let cat = ModelCatalog::builtin();
        let spec = cat.lookup(&ModelId("moonshot-v1-128k".into())).unwrap();
        assert_eq!(spec.context_window, 128_000);
        assert!(spec.thinking_capabilities.supported_modes.is_empty());
    }

    #[test]
    fn test_opus_4_7_only_supports_adaptive() {
        let cat = ModelCatalog::builtin();
        let spec = cat.lookup(&ModelId("claude-opus-4-7".into())).unwrap();
        let caps = &spec.thinking_capabilities;
        assert!(caps.supports_adaptive());
        // 关键：Opus 4.7 不能走 ExtendedBudget（API 拒收）
        assert!(!caps.supported_modes.contains(&ThinkingModeKind::ExtendedBudget));
        assert!(caps.effort_levels.contains(&EffortLevel::XHigh));
    }

    #[test]
    fn test_sonnet_4_6_dual_support() {
        let cat = ModelCatalog::builtin();
        let spec = cat.lookup(&ModelId("claude-sonnet-4-6".into())).unwrap();
        let caps = &spec.thinking_capabilities;
        assert!(caps.supports_adaptive());
        assert!(caps.supports_budget());
    }

    #[test]
    fn test_sonnet_4_5_extended_only() {
        let cat = ModelCatalog::builtin();
        let spec = cat.lookup(&ModelId("claude-sonnet-4-5".into())).unwrap();
        let caps = &spec.thinking_capabilities;
        assert!(!caps.supports_adaptive());
        assert!(caps.supports_budget());
    }

    #[test]
    fn test_deepseek_requires_reasoning_content_replay() {
        let cat = ModelCatalog::builtin();
        let spec = cat.lookup(&ModelId("deepseek-v4-pro".into())).unwrap();
        assert_eq!(
            spec.thinking_capabilities.multi_turn_replay,
            MultiTurnReplay::ReasoningContent,
            "DeepSeek 多轮必须回传 reasoning_content（V15 hotfix 根因）"
        );
    }

    #[test]
    fn test_gpt_5_has_minimal_effort() {
        let cat = ModelCatalog::builtin();
        let spec = cat.lookup(&ModelId("gpt-5".into())).unwrap();
        assert!(spec.thinking_capabilities.effort_levels.contains(&EffortLevel::Minimal));
    }

    #[test]
    fn test_o3_classic_no_minimal() {
        let cat = ModelCatalog::builtin();
        let spec = cat.lookup(&ModelId("o3".into())).unwrap();
        assert!(!spec.thinking_capabilities.effort_levels.contains(&EffortLevel::Minimal));
    }

    #[test]
    fn test_gemini_uses_budget_int() {
        let cat = ModelCatalog::builtin();
        let spec = cat.lookup(&ModelId("gemini-2.5-pro".into())).unwrap();
        assert!(spec.thinking_capabilities.supported_modes.contains(&ThinkingModeKind::BudgetInt));
        assert!(spec.thinking_capabilities.budget_range.is_some());
    }

    #[test]
    fn test_lookup_unknown_returns_none() {
        let cat = ModelCatalog::builtin();
        assert!(cat.lookup(&ModelId("nonexistent-model-x".into())).is_none());
    }

    #[test]
    fn test_lookup_or_default_unknown_returns_no_thinking() {
        let cat = ModelCatalog::builtin();
        let spec = cat.lookup_or_default(&ModelId("nonexistent-model-x".into()));
        assert!(!spec.thinking_capabilities.is_supported(),
                "未知模型保守默认：不假设支持思考");
    }

    #[test]
    fn test_insert_overrides_builtin() {
        let mut cat = ModelCatalog::builtin();
        let custom = ModelSpec {
            context_window: 12345,
            ..Default::default()
        };
        cat.insert(ModelId("claude-opus-4-7".into()), custom);
        let spec = cat.lookup(&ModelId("claude-opus-4-7".into())).unwrap();
        assert_eq!(spec.context_window, 12345, "insert 应覆盖内置");
    }

    // ── Phase 3：merge_yaml 测试 ──────────────────────────────────────────

    #[test]
    fn test_merge_yaml_missing_file_is_silent() {
        let mut cat = ModelCatalog::builtin();
        let n = cat.merge_yaml(&std::path::PathBuf::from("/nonexistent/path/x.yaml")).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_merge_yaml_overrides_context_window() {
        let yaml = r#"
models:
  claude-opus-4-7:
    context_window: 500000
    max_output_tokens: 100000
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), yaml).unwrap();

        let mut cat = ModelCatalog::builtin();
        let n = cat.merge_yaml(tmp.path()).unwrap();
        assert_eq!(n, 1);

        let spec = cat.lookup(&ModelId("claude-opus-4-7".into())).unwrap();
        assert_eq!(spec.context_window, 500_000);
        assert_eq!(spec.max_output_tokens, 100_000);
        // thinking_capabilities 未在 YAML 中提供 → 保留内置（仍是 adaptive_effort）
        assert!(spec.thinking_capabilities.supports_adaptive());
    }

    #[test]
    fn test_merge_yaml_adds_unknown_model() {
        let yaml = r#"
models:
  custom-local-llama-3:
    context_window: 32000
    max_output_tokens: 4096
    thinking_capabilities:
      supported_modes: ["enabled_toggle"]
      default_mode: "enabled_toggle"
      multi_turn_replay: "reasoning_content"
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), yaml).unwrap();

        let mut cat = ModelCatalog::builtin();
        cat.merge_yaml(tmp.path()).unwrap();

        let spec = cat.lookup(&ModelId("custom-local-llama-3".into())).unwrap();
        assert_eq!(spec.context_window, 32000);
        assert_eq!(
            spec.thinking_capabilities.multi_turn_replay,
            MultiTurnReplay::ReasoningContent
        );
        assert!(spec.thinking_capabilities.supported_modes
            .contains(&ThinkingModeKind::EnabledToggle));
    }

    #[test]
    fn test_merge_yaml_overrides_thinking_capabilities() {
        let yaml = r#"
models:
  claude-sonnet-4-6:
    thinking_capabilities:
      supported_modes: ["adaptive_effort"]
      default_mode: "adaptive_effort"
      effort_levels: ["low", "high", "max"]
      budget_range: null
      multi_turn_replay: "none"
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), yaml).unwrap();

        let mut cat = ModelCatalog::builtin();
        cat.merge_yaml(tmp.path()).unwrap();

        let spec = cat.lookup(&ModelId("claude-sonnet-4-6".into())).unwrap();
        let caps = &spec.thinking_capabilities;
        // 强制移除 ExtendedBudget 支持（仅 adaptive）
        assert!(caps.supports_adaptive());
        assert!(!caps.supports_budget());
        assert_eq!(caps.effort_levels.len(), 3);
    }

    #[test]
    fn test_merge_yaml_invalid_syntax_returns_error() {
        let yaml = "models:\n  unbalanced: [";
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), yaml).unwrap();

        let mut cat = ModelCatalog::builtin();
        let err = cat.merge_yaml(tmp.path());
        assert!(err.is_err(), "坏 YAML 应当报错而非静默");
    }

    #[test]
    fn test_merge_yaml_budget_range_parsing() {
        let yaml = r#"
models:
  custom-budget-model:
    thinking_capabilities:
      supported_modes: ["budget_int"]
      default_mode: "budget_int"
      budget_range: [128, 32768]
      multi_turn_replay: "none"
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), yaml).unwrap();

        let mut cat = ModelCatalog::empty();
        cat.merge_yaml(tmp.path()).unwrap();
        let spec = cat.lookup(&ModelId("custom-budget-model".into())).unwrap();
        assert_eq!(spec.thinking_capabilities.budget_range, Some((128, 32768)));
    }
}
