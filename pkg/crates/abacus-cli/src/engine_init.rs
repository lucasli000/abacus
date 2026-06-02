//! Shared engine initialization for CLI commands and TUI.
//!
//! Creates a ready-to-use CoreLoop with:
//! - ToolRegistry (built-in tools registered)
//! - SkillEngine (empty, skills loaded on demand)
//! - CapabilityHub (empty, providers registered on demand)
//! - ContextManager (in-memory session store)
//! - Provider registration via ConfigManager (env vars + YAML config)
//!
//! ## Config priority
//! 1. Environment variables (ABACUS_*)
//! 2. ~/.abacus/config.yaml (TUI setup wizard output)
//! 3. Built-in defaults

use std::sync::Arc;
use tokio::sync::RwLock;

use abacus_core::config::{ConfigManager, default_config};
use abacus_core::core::{CoreConfig, CoreLoop, SessionState};
use abacus_core::core::context::{ContextManager, SessionSnapshot, SessionStore};
use abacus_core::tool::ToolRegistry;
use abacus_core::skill::SkillEngine;
use abacus_core::capability::CapabilityHub;
use abacus_types::{KernelError, ModelId};

// V44: 精简 Kernel prompt — 语义等价，信息密度提升 ~45%
// 旧: ~280 tokens | 新: ~150 tokens
// 原则: 结构化 bullet ≤15 tok/条; 删除 LLM 已知的常识; 保留所有行为约束
const DEFAULT_SYSTEM_PROMPT: &str = "\
You are Abacus — autonomous agent kernel in user's local environment. Not Claude/GPT/any specific LLM.

# Rules
- Act through tools. Verify via tools, never assume.
- Multi-step: decompose → execute → verify each → synthesize.
- Language: follow user (default Chinese conversation, English code).
- Output: answer-first, no intro/filler/emoji/apology. Concise and direct.
- Code: include file path. No placeholder (TODO/pass/...).
- Errors: actual error + diagnosis. Retry with fix (max 2), then report.
- Identity: \"I am Abacus, an autonomous agent kernel.\" Never mention underlying LLM.
- Safety: destructive ops (rm -rf/DROP/force-push) require explicit confirmation.
- Never fabricate tool names or API endpoints — verify existence first.";

/// In-memory session store for CLI (no persistence needed for single session).
struct CliSessionStore;

#[async_trait::async_trait]
impl SessionStore for CliSessionStore {
    async fn save(&self, _snapshot: SessionSnapshot) -> Result<(), KernelError> { Ok(()) }
    async fn load_recent(&self, _limit: usize) -> Result<Vec<SessionSnapshot>, KernelError> { Ok(Vec::new()) }
    async fn search(&self, _query: &str) -> Result<Vec<SessionSnapshot>, KernelError> { Ok(Vec::new()) }
}

/// Initialize CoreLoop with configuration-driven initialization.
///
/// `thinking_level` maps to ThinkingEffort: "off" | "low" | "medium" | "high"
pub async fn create_engine(
    model: &str,
    system_prompt: Option<&str>,
    thinking_level: &str,
) -> color_eyre::eyre::Result<(Arc<CoreLoop>, Arc<RwLock<SessionState>>)> {
    let registry = Arc::new(ToolRegistry::new());
    let skill_engine = Arc::new(RwLock::new(SkillEngine::new()));
    let cap_hub = Arc::new(CapabilityHub::new());
    let session_store: Arc<dyn SessionStore> = Arc::new(CliSessionStore);
    let ctx_mgr = Arc::new(ContextManager::new(session_store));

    // ─── ConfigManager: env vars + ~/.abacus/config.yaml ───────────────
    let mut cfg_mgr = ConfigManager::new(default_config());
    cfg_mgr.load_env("ABACUS_");

    // 配置加载顺序：默认内置层 < models.yaml < config.yaml < security.yaml < conf.d/*.yaml < 环境变量
    // 路径走 abacus_core::paths，遵循 ABACUS_HOME 覆盖。
    use abacus_core::paths;
    let _ = cfg_mgr.load_file(paths::models_yaml());      // models.yaml — LLM 模型能力声明
    let _ = cfg_mgr.load_file(paths::config_yaml());      // config.yaml — Abacus 行为配置
    let _ = cfg_mgr.load_file(paths::security_yaml());    // security.yaml — safety / MCIP 安全配置
    cfg_mgr.load_dir(paths::conf_d_dir());                // conf.d/ — 自定义扩展配置
    // V43.7: providers.json 优先——JSON 格式的 provider/LLM 配置（覆盖 config.yaml 中的 providers）
    if let Err(e) = cfg_mgr.load_providers_json(paths::providers_json()) {
        tracing::warn!("providers.json load failed: {e}");
    }

    // Validate config before using — warn on out-of-range values
    let validation_warnings = cfg_mgr.validate();
    for w in &validation_warnings {
        tracing::warn!("config validation: {}", w);
    }

    // Model — 优先级链: config.yaml core.default_model > providers.json 第一个 model > CLI 参数
    // V44: 不再硬编码默认模型——从 providers.json 自动推导
    let auto_model: Option<String> = cfg_mgr.parse_providers()
        .first()
        .and_then(|entry| entry.models.first())
        .map(|m| m.name.clone());
    let resolved_model = cfg_mgr.get_str("core.default_model")
        .unwrap_or_else(|| auto_model.as_deref().unwrap_or(model));

    let max_turns = cfg_mgr.get_number("core.max_turns").map(|n| n as u32).unwrap_or(200);
    // V29.14 (Risk 3): max_turns 过高时记录 tracing::warn, 提示 token 费用风险
    //   阈值 60 = 默认 25 的 2.4x, 是合理"上限警戒线"; <60 静默
    //   仅记日志, 不阻塞启动 (用户主动调高通常有意图, 不该硬限制)
    //   引用关系: tui.log / RUST_LOG=warn 时可见; 不进 TUI toast (启动期没有 state)
    if max_turns > 60 {
        tracing::warn!(
            max_turns,
            "core.max_turns={} 偏高 (默认 25). pro/reasoner 模式下单 turn 多 tool 调用会显著放大 token 消耗. \
             如果撞过 max turns 建议先看 timeline panel 是否 LLM 进了循环, 而非盲目调高",
            max_turns
        );
    }
    let max_tool_calls = cfg_mgr.get_number("core.max_tool_calls").map(|n| n as u32).unwrap_or(100);
    let temperature = cfg_mgr.get_number("core.temperature").unwrap_or(0.6);
    // V40: 默认 64000 — 对齐 Claude Code/OpenCode 的单轮输出上限
    // DeepSeek v4-flash thinking 模式支持最高 64K+ output
    let max_tokens = cfg_mgr.get_number("core.max_tokens").map(|n| n as u32).unwrap_or(64000);
    let context_window = cfg_mgr.get_number("core.context_window").map(|n| n as usize).unwrap_or(1_000_000);
    // 可用窗口比例：用户配置占模型最大上下文的比例（0.1-1.0）
    // 默认 1.0 = 全用（setup 向导的空填写语义；旧配置文件中若显式写了 0.5 则沿用旧值）
    let context_window_ratio = cfg_mgr.get_number("core.context_window_ratio").unwrap_or(1.0);
    let silent_router = cfg_mgr.get_bool("core.silent_router_enabled").unwrap_or(true);

    // Phase 3：统一 thinking 入口。
    //   优先级 1：CLI 参数 thinking_level（运行时显式覆盖）
    //   优先级 2：ConfigManager.get_thinking_intent()（合并新旧 key + deprecation warn）
    //   优先级 3：默认 Off（用户未设置任何 key）
    //
    // 修复 deprecation gap（2026-05-24）：原实现 if 分支命中即跳过 cfg_mgr 调用，
    // 由于所有 CLI 调用方（chat/exec/turnkey/model/meeting/team）都传非空 thinking_level，
    // 旧 key 的 deprecation warn 永远不可达。改为 always 调用 get_thinking_intent() 作
    // side-effect 触发 warn（OnceLock 守护进程级单次），不影响优先级链最终结果。
    //
    // 引用关系：
    // - 写入：本函数把 intent 传入 CoreConfig.thinking_intent
    // - 读取：cfg_mgr.get_thinking_intent() 内部 tracing::warn! 命中 stderr 一次
    let cfg_intent = cfg_mgr.get_thinking_intent();
    let intent: abacus_types::ThinkingIntent =
        if !thinking_level.is_empty() && thinking_level != "default" {
            abacus_types::ThinkingIntent::from_str_loose(thinking_level)
                .unwrap_or(abacus_types::ThinkingIntent::Off)
        } else {
            cfg_intent.unwrap_or(abacus_types::ThinkingIntent::Effort(abacus_types::EffortLevel::Medium))
        };

    // L1 后：thinking_intent 直接传给 CoreConfig，无需兼容外壳。
    // ModelSpec.thinking_config 仍是旧 ModelThinkingConfig 形态，保留以维持 spec 表达
    // （主要供 spec.default_budget_tokens / spec.preserve_thinking 等下游字段使用）。
    let legacy_effort: Option<abacus_types::ThinkingEffort> = match &intent {
        abacus_types::ThinkingIntent::Off => None,
        abacus_types::ThinkingIntent::Adaptive => Some(abacus_types::ThinkingEffort::High),
        abacus_types::ThinkingIntent::Effort(level) => Some(match level {
            abacus_types::EffortLevel::Minimal | abacus_types::EffortLevel::Low => abacus_types::ThinkingEffort::Low,
            abacus_types::EffortLevel::Medium => abacus_types::ThinkingEffort::Medium,
            abacus_types::EffortLevel::High | abacus_types::EffortLevel::Max | abacus_types::EffortLevel::XHigh => abacus_types::ThinkingEffort::High,
        }),
        abacus_types::ThinkingIntent::Budget(_) => Some(abacus_types::ThinkingEffort::High),
    };
    let thinking_intent = match &intent {
        abacus_types::ThinkingIntent::Off => None,
        _ => Some(intent.clone()),
    };

    let model_spec = Some(abacus_types::ModelSpec {
        context_window,
        max_output_tokens: max_tokens,
        thinking_config: abacus_types::ModelThinkingConfig {
            enabled: intent.is_enabled(),
            effort: legacy_effort,
            preserve_thinking: false,
        },
        ..Default::default()
    });

    // Phase 3：模型能力 catalog——builtin + paths::models_yaml() 覆盖
    let mut catalog = abacus_core::llm::ModelCatalog::builtin();
    let yaml_path = paths::models_yaml();
    match catalog.merge_yaml(&yaml_path) {
        Ok(0) => {}
        Ok(n) => tracing::info!("Loaded {} model spec override(s) from {}", n, yaml_path.display()),
        Err(e) => tracing::warn!("Failed to merge {}: {}", yaml_path.display(), e),
    }
    // 2026-05-28: 从 providers[].models[] per-model 参数合并到 catalog
    // 2026-05-30 PR2: 传入 provider_id 写入 qualified_specs（provider-aware 索引）
    let provider_entries_for_catalog = cfg_mgr.parse_providers();
    for entry in &provider_entries_for_catalog {
        for model_entry in &entry.models {
            catalog.merge_model_entry(model_entry, Some(&entry.id));
        }
    }
    let model_catalog = Some(std::sync::Arc::new(catalog));

    let config = CoreConfig {
        max_turns_per_request: max_turns,
        max_tool_calls_per_turn: max_tool_calls,
        default_model: ModelId(resolved_model.to_string()),
        default_temperature: temperature,
        default_max_tokens: max_tokens,
        context_window_ratio,
        system_prompt: system_prompt.unwrap_or(DEFAULT_SYSTEM_PROMPT).to_string(),
        model_spec,
        thinking_intent,
        silent_router_enabled: silent_router,
        model_catalog,
        tool_visibility_threshold: abacus_types::VisibilityTier::D,
        // Task #84/#87：按任务类型路由工具（减少 LLM 上下文噪声 1k-3k tokens/turn）
        task_kind_routing_enabled: cfg_mgr.get_bool("core.task_kind_routing").unwrap_or(true),
        // 频率剪枝：N turn 未调用的工具隐藏（None = 关闭）
        tool_frequency_pruning_turns: cfg_mgr.get_number("core.tool_frequency_pruning_turns")
            .map(|n| n as u64)
            .or(Some(20)),
        // 记忆宫殿同步频率：每 N 轮写一次（0 或缺省 = 关闭）
        palace_sync_interval_turns: cfg_mgr.get_number("palace.sync_interval_turns")
            .map(|n| n as u32)
            .filter(|&n| n > 0)
            .or(Some(5)),
        // V28.7: schema 演化补漏——CoreConfig 新增字段，与 abacus-core 默认值对齐
        default_compress_level: abacus_core::core::context::CompressLevel::Brief,
        // Phase 3 (lint)：从 cfg_mgr 读 lint 配置；缺省 None
        lint_overrides: cfg_mgr.get_typed::<abacus_core::tool::schema_lint::LintOverrides>("lint"),
        // Task #96：单 session 模型升级预算
        max_escalations: cfg_mgr.get_number("core.max_escalations").map(|n| n as u32).unwrap_or(10),
        // 模型升级目标：从 config 读取；空字符串或缺省视为 None（禁用升级）
        escalation_model: cfg_mgr.get_str("pipeline.escalation_target_model")
            .filter(|s| !s.is_empty())
            .map(|s| abacus_types::ModelId(s.to_string())),
        // Tool result dedup：相同幂等工具调用短 TTL 内复用结果
        tool_result_dedup_enabled: cfg_mgr.get_bool("core.dedup.enabled").unwrap_or(true),
        tool_result_dedup_ttl_secs: cfg_mgr.get_number("core.dedup.ttl_secs").map(|n| n as u64).unwrap_or(60),
        tool_result_dedup_capacity_kb: cfg_mgr.get_number("core.dedup.capacity_kb").map(|n| n as usize).unwrap_or(2048),
        adaptive_d_tier_hide: cfg_mgr.get_bool("core.adaptive_d_tier_hide").unwrap_or(true),
        // cross-session: 默认开启 jsonl 事件流写入
        event_sink_enabled: cfg_mgr.get_bool("core.event_sink_enabled").unwrap_or(true),
        scene_tool_loading_enabled: cfg_mgr.get_bool("core.scene_tool_loading").unwrap_or(true),
        policy: std::sync::Arc::new(abacus_core::core::policy::PolicyConfig::load()),
        thresholds: abacus_core::core::ThresholdConfig::default(),
        prompt_roles_path: dirs::home_dir().map(|h| h.join(".abacus/prompt_roles.toml")),
        subscenes_path: dirs::home_dir().map(|h| h.join(".abacus/subscenes.toml")),
        // Deduction engine capabilities（默认全开）
        deduction_observer_contamination: cfg_mgr.get_bool("deduction.observer_contamination").unwrap_or(true),
        deduction_cross_session: cfg_mgr.get_bool("deduction.cross_session").unwrap_or(true),
        deduction_context_degradation: cfg_mgr.get_bool("deduction.context_degradation").unwrap_or(true),
        deduction_prompt_impact: cfg_mgr.get_bool("deduction.prompt_impact").unwrap_or(true),
        // 认识论约束：连续违规 N 次后强制 LLM 显式声明不确定性
        epistemic_threshold: cfg_mgr.get_number("epistemic.threshold").map(|n| n as u32).unwrap_or(3),
        // 记忆宫殿：palace hints + 到期复习提醒
        palace_enabled: cfg_mgr.get_bool("palace.enabled").unwrap_or(true),
    };

    let mut core = CoreLoop::new(registry, skill_engine, cap_hub, ctx_mgr, config).await;

    // ─── 知识库 + 记忆宫殿 wire-up ───────────────────────────────────────
    // KnowledgeStore 和 DualPalaceMemory 均未集成进 CoreLoop::new()，需在此显式初始化。
    // 两者均持久化到 ~/.abacus/（磁盘失败时静默降级为内存模式，不中断启动）。
    //
    // ## 生命周期
    // - 创建：此处（CoreLoop::new() 后）
    // - 执行时：由 KbToolExecutor 通过 Arc 共享，永不释放
    // - 销毁：随进程销毁
    let kb_db_path = paths::knowledge_db();
    let palace_db_path = paths::palace_db();

    let kb_store = Arc::new(
        abacus_core::knowledge_store::KnowledgeStore::new(&kb_db_path)
            .unwrap_or_else(|e| {
                tracing::warn!("KnowledgeStore 磁盘初始化失败，降级内存模式: {e}");
                abacus_core::knowledge_store::KnowledgeStore::in_memory()
                    .expect("in-memory KnowledgeStore must succeed")
            })
    );

    let palace_sqlite = abacus_core::memory_palace::SqlitePalaceStore::new(&palace_db_path)
        .ok().map(Arc::new);

    let palace = Arc::new(RwLock::new(
        abacus_core::memory_palace::DualPalaceMemory::with_store(palace_sqlite.clone())
    ));

    // 预热：从 SQLite 恢复记忆。warmup() 内部用 write lock on palace.knowledge.entries 等，
    // 与外层 RwLock<DualPalaceMemory> 的读锁无竞态（两层 RwLock 相互独立）。
    if let Some(ref store) = palace_sqlite {
        if let Err(e) = store.warmup(&*palace.read().await).await {
            tracing::warn!("记忆宫殿 warmup 失败（本次 session 从空宫殿启动）: {e}");
        }
    }

    // 注册 kb.* 工具执行器（schema 已由 register_all() 在 CoreLoop::new() 内注册）
    // registry 已 move 进 CoreLoop，通过 tool_registry_ref() 取回 Arc 引用
    abacus_core::tool::builtin::kb::register_executors(core.tool_registry_ref(), kb_store.clone(), palace.clone()).await;

    // V42: 注入 memory palace 到 CoreLoop（面板数据拉取 + pipeline 主动读写 + PalaceAbsorbHook）
    // 引用关系：core.memory_palace() 被 TUI run.rs 读取 → state.palace_data
    // 必须在 register_executors 之后调用（with_memory 内部重注册 result.expand executor）
    core = core.with_memory(kb_store, palace).await;

    // ─── MagChain 中间件注册 ─────────────────────────────────────────
    // 执行顺序由 priority 决定（lower = earlier）。
    // 注册窗口：CoreLoop::new() 之后、Arc::new(core) 之前（mag_chain_mut 需 &mut self）。
    {
        use abacus_core::mag_chain::{AuditLogger, CircuitBreaker, PiiRedactor, RateLimiter};
        use std::time::Duration;

        // P10: 熔断 — 连续 10 次失败后熔断，30s 自动恢复
        core.add_middleware(10, Arc::new(CircuitBreaker::new(10, Duration::from_secs(30)))).await;
        // P20: 限流 — 每工具每分钟最多 200 次调用（滑动窗口）
        core.add_middleware(20, Arc::new(RateLimiter::new(200, Duration::from_secs(60)))).await;
        // P50: 认识论约束 — 与 CoreLoop.epistemic_guard 共享同一 Arc 实例
        //       EpistemicGuard 的内部状态（violation_count/zero_hit_streak）通过此共享引用
        //       在 pipeline setup() 和 post_process() 中跨 turn 累积
        core.add_middleware(50, Arc::clone(core.epistemic_guard()) as Arc<dyn abacus_core::mag_chain::Middleware>).await;
        // P70: PII 脱敏 — 递归清洗 output 中的信用卡/Email/SSN
        core.add_middleware(70, Arc::new(PiiRedactor::new())).await;
        // P100: 审计 — 最后执行，记录经上游中间件处理后的最终 output
        core.add_middleware(100, Arc::new(AuditLogger::new(1000))).await;
    }

    // ─── Provider registration ─────────────────────────────────────────
    // 2026-05-28: 优先使用 config.yaml `providers` 数组（多供应商多模型）
    // fallback: 无 `providers` 时走旧 llm.* 逻辑（向后兼容）
    use abacus_core::llm::fallback_provider::FallbackProvider;
    use abacus_core::LlmProvider;

    let provider_entries = cfg_mgr.parse_providers();
    if !provider_entries.is_empty() {
        // ── 新路径：多供应商配置 ──
        use abacus_types::ProviderType;
        let mut registered_ids: Vec<String> = Vec::new();

        for entry in &provider_entries {
            let api_key = entry.api_key.clone().unwrap_or_default();
            // models 为空时用 default_model 作占位——discover_models() 会在后台填充实际列表
            let models: Vec<ModelId> = if entry.models.is_empty() {
                vec![ModelId(resolved_model.to_string())]
            } else {
                entry.models.iter().map(|m| ModelId(m.name.clone())).collect()
            };

            match entry.provider_type {
                ProviderType::Anthropic => {
                    use abacus_core::llm::providers::anthropic::AnthropicProvider;
                    let p = Arc::new(AnthropicProvider::new(
                        api_key, models.first().cloned().unwrap_or(ModelId("claude-sonnet-4".into())),
                        entry.base_url.clone(), None,
                    ));
                    core.register_provider_group(&entry.id, models, p).await;
                }
                ProviderType::Deepseek => {
                    use abacus_core::llm::providers::deepseek::DeepSeekProvider;
                    let base = entry.base_url.clone()
                        .map(|u| u.trim_end_matches("/v1").trim_end_matches("/v2")
                            .trim_end_matches("/v3").trim_end_matches("/v4").to_string());
                    let default_ds_model = models.first().cloned()
                        .unwrap_or_else(|| ModelId(abacus_types::ModelId::AUTO.into()));
                    let p = Arc::new(DeepSeekProvider::with_config(
                        api_key, default_ds_model,
                        base, None, None,
                    ));
                    core.register_provider_group(&entry.id, models, p).await;
                }
                ProviderType::OpenaiCompatible | ProviderType::Gemini => {
                    use abacus_core::llm::providers::openai_compatible::OpenAICompatibleProvider;
                    let user_configured_url = entry.base_url.is_some();
                    let base = entry.base_url.clone()
                        .unwrap_or_else(|| "https://api.openai.com/v1".into());
                    let default_oai_model = models.first().cloned()
                        .unwrap_or_else(|| ModelId(abacus_types::ModelId::AUTO.into()));
                    let mut provider = OpenAICompatibleProvider::new(
                        api_key, default_oai_model,
                        base, None, None, None,
                    );
                    // 只从用户配置的 URL 检测模型；未配置时用静态列表
                    if !user_configured_url {
                        provider.set_discover_enabled(false);
                    }
                    let p = Arc::new(provider);
                    core.register_provider_group(&entry.id, models, p).await;
                }
            }
            registered_ids.push(entry.id.clone());
            tracing::info!(provider = %entry.id, models = ?entry.models, "registered provider group");
        }

        // 构建 "primary" — fallback chain 按配置顺序（或 fallback_chain 字段覆盖）
        let chain_ids = cfg_mgr.get_list("fallback_chain")
            .unwrap_or(registered_ids.clone());
        if chain_ids.len() == 1 {
            if let Some(p) = core.get_provider(&chain_ids[0]).await {
                core.register_provider("primary", p).await;
                core.set_adapter("primary", &chain_ids[0]).await;
            }
        } else if chain_ids.len() >= 2 {
            let pri = core.get_provider(&chain_ids[0]).await;
            let fb = core.get_provider(&chain_ids[1]).await;
            if let (Some(pri), Some(fb)) = (pri, fb) {
                let fallback = Arc::new(FallbackProvider::new(
                    pri, fb, &chain_ids[0], &chain_ids[1]
                ));
                core.register_provider("primary", fallback).await;
                core.set_adapter("primary", &chain_ids[0]).await;
            }
        }
    } else {
    // ── 旧路径：llm.* 单键配置（向后兼容）──

    let anthropic_key = cfg_mgr.get_str("llm.anthropic_api_key")
        .map(|s| s.to_string())
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok());
    let openai_base = cfg_mgr.get_str("llm.openai_base_url")
        .or_else(|| cfg_mgr.get_str("llm.base_url"))
        .map(|s| s.to_string())
        .or_else(|| std::env::var("ABACUS_OPENAI_BASE_URL").ok());
    let openai_key = cfg_mgr.get_str("llm.openai_api_key")
        .or_else(|| cfg_mgr.get_str("llm.api_key"))
        .map(|s| s.to_string())
        .or_else(|| std::env::var("ABACUS_OPENAI_API_KEY").ok())
        .or_else(|| std::env::var("ABACUS_API_KEY").ok());
    let mut deepseek_key = std::env::var("ABACUS_API_KEY")
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY")).ok();

    // V19b：base_url 是 deepseek 时，把用户配置在 cfg `llm.api_key`/`llm.openai_api_key` 的
    //   key 借给 deepseek_key——典型场景是用户用 OpenAI 兼容 SDK 思路填 config，但 base_url
    //   实际指 DeepSeek。V19 跳过了 OpenAI 注册后，deepseek_key 必须能拿到这个 key。
    if deepseek_key.is_none()
        && openai_base.as_ref().is_some_and(|u| u.contains("deepseek.com"))
    {
        if let Some(ref k) = openai_key {
            deepseek_key = Some(k.clone());
        }
    }

    let mut primary: Option<Arc<dyn LlmProvider>> = None;
    let mut fallback: Option<Arc<dyn LlmProvider>> = None;
    // 跟踪 primary provider 的底层协议，用于 FallbackProvider 绑定正确 PromptAdapter
    let mut primary_adapter_id: &str = "neutral";

    if let Some(api_key) = anthropic_key {
        use abacus_core::llm::providers::anthropic::AnthropicProvider;
        let base_url = cfg_mgr.get_str("llm.anthropic_base_url").map(|s| s.to_string());
        let p = Arc::new(AnthropicProvider::new(
            api_key, ModelId(resolved_model.to_string()), base_url, None,
        ));
        core.register_provider("anthropic", p.clone()).await;
        primary = Some(p);
        primary_adapter_id = "anthropic";
    }

    // V19 修复：base_url 是 DeepSeek 时跳过 OpenAI 协议适配
    //   原因：OpenAI 协议没有 reasoning_content 字段；assistant 消息的 reasoning_content
    //   会在 build_request 时被丢弃；下一轮 DeepSeek 服务端发现缺失就 400
    //   "thinking mode must be passed back"。让 DeepSeekProvider 接管即可。
    let openai_base_is_deepseek = openai_base.as_ref()
        .is_some_and(|u| u.contains("deepseek.com"));
    if !openai_base_is_deepseek {
        if let (Some(base_url), Some(api_key)) = (openai_base, openai_key) {
            use abacus_core::llm::providers::openai_compatible::OpenAICompatibleProvider;
            let p = Arc::new(OpenAICompatibleProvider::new(
                api_key, ModelId(resolved_model.to_string()),
                base_url, None, None, None,
            ));
            core.register_provider("openai-compatible", p.clone()).await;
            if primary.is_none() {
                primary = Some(p);
                primary_adapter_id = "openai-compatible";
            } else {
                fallback = Some(p);
            }
        }
    }

    if let Some(api_key) = deepseek_key {
        use abacus_core::llm::providers::deepseek::DeepSeekProvider;
        let clean_base = cfg_mgr.get_str("llm.base_url")
            .map(|s| {
                let s = s.trim_end_matches("/v1").trim_end_matches("/v2")
                    .trim_end_matches("/v3").trim_end_matches("/v4").trim();
                if s.is_empty() { None } else { Some(s.to_string()) }
            })
            .flatten();
        let p = Arc::new(DeepSeekProvider::with_config(
            api_key, ModelId(resolved_model.to_string()),
            clean_base, None, None,
        ));
        core.register_provider("deepseek", p.clone()).await;
        if primary.is_none() {
            primary = Some(p);
            primary_adapter_id = "deepseek";
        } else {
            fallback = Some(p);
        }
    }

    match (primary, fallback) {
        (Some(pri), Some(fb)) => {
            let fb = Arc::new(FallbackProvider::new(pri, fb, "primary", "fallback"));
            core.register_provider("primary", fb).await;
            // FallbackProvider id="primary" 自动获得 NeutralAdapter，需显式绑定主协议 adapter
            core.set_adapter("primary", primary_adapter_id).await;
        }
        (Some(p), None) | (None, Some(p)) => {
            core.register_provider("primary", p).await;
            // 单 provider 时 register_provider 已正确注册 adapter（基于 primary_adapter_id）
        }
        (None, None) => {
            core.register_provider("no-api-key", Arc::new(abacus_core::NoApiKeyProvider)).await;
        }
    }
    } // end of else block (旧路径)

    // ─── PR4: Load ModelPreference + apply last_selected ─────────────────
    // After all providers registered, load user preference from disk.
    // If the user previously selected a model via `/model`, auto-restore that choice.
    //
    // ## 引用关系
    // - 写入: core.model_preference() (Arc<RwLock<ModelPreference>>)
    // - 写入: core.set_model_override() (如果 last_selected 存在)
    // - 读取: resolve_provider() 在每次 turn 时消费 preference
    //
    // ## 生命周期
    // - 创建: 此处（provider 注册后）
    // - 销毁: 随 core Arc drop（进程结束）
    {
        let pref_path = abacus_types::preference_file_path();
        let preference = abacus_types::load_model_preference(&pref_path).unwrap_or_default();
        // Step 10: If last_selected is set, auto-restore model_override so the
        // user's previous `/model` selection takes effect immediately.
        if let Some(ref last) = preference.last_selected {
            core.set_model_override(&last.model.0).await;
        }
        *core.model_preference().write().await = preference;
    }

    // ─── MCIP 权限配置（来自 security.yaml）───────────────────────
    // 内置工具前缀（fs_/lsp./kb_ 等）已硬编码豁免，无需配置。
    core.configure_mcip_permissions(
        &cfg_mgr.get_list("mcip.exempt_prefixes").unwrap_or_default(),
        &cfg_mgr.get_list("mcip.allow_tools").unwrap_or_default(),
        &cfg_mgr.get_list("mcip.deny_tools").unwrap_or_default(),
    );

    // ─── LSP 支持（按需激活）──────────────────────────────────────────
    // 语言服务器是 lazy start：首次调用工具时才实际启动。
    // 可用 `lsp.enabled = false` 在 ~/.abacus/config.yaml 中禁用。
    if cfg_mgr.get_bool("lsp.enabled").unwrap_or(true) {
        let workspace = std::env::current_dir()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".into());
        core.enable_lsp(workspace).await;
    }

    // ─── CodeGraph 代码知识图谱（默认启用）──────────────────────────────
    // tree-sitter 多语言代码索引 + FTS5 符号搜索 + 调用图/依赖图遍历。
    // 共享 knowledge.db（WAL 模式并发安全）。schemas 已在 register_all() 注册，
    // enable_code_graph 仅绑定 executor。可用 `code_graph.enabled = false` 禁用。
    if cfg_mgr.get_bool("code_graph.enabled").unwrap_or(true) {
        let workspace = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        core.enable_code_graph(&workspace).await;
    }

    // ─── Skill workflow executor（默认禁用）──────────────────────────
    // 启用后 SkillDef.workflow 步骤注册为虚拟 ToolHandle，由 LLM 工具调用触发。
    // 引用关系：core.skill_workflow_executor 持 Weak<ToolRegistry/SkillEngine>，
    // 不形成循环；销毁随 core 自然回收。
    if cfg_mgr.get_bool("core.skill_workflow_enabled").unwrap_or(false) {
        core.enable_skill_workflow_executor().await;
    }

    // ─── AutoEngine 持久化（默认禁用）──────────────────────────────
    // 配置 auto.persist_path 路径即启用 SQLite 写入；失败 warn 不阻断启动。
    if let Some(path) = cfg_mgr.get_str("auto.persist_path") {
        match abacus_core::auto::AutoStore::new(path) {
            Ok(store) => {
                core.enable_auto_store(std::sync::Arc::new(store)).await;
                tracing::info!(path = path, "AutoEngine SQLite persistence enabled");
            }
            Err(e) => tracing::warn!(error = %e, "AutoStore init failed; running in-memory"),
        }
    }

    // ─── WASM Plugins（默认禁用，启用要求签名）──────────────────────
    // 配置 core.plugins.base_dir 启用；signing_required 默认 true。
    if let Some(base_dir) = cfg_mgr.get_str("core.plugins.base_dir") {
        let require_signing = cfg_mgr.get_bool("core.plugins.signing_required").unwrap_or(true);
        match core.enable_plugins_with_options(base_dir.to_string(), require_signing).await {
            Ok(n) => tracing::info!(tools = n, signing = require_signing, "WASM plugins enabled"),
            Err(e) => tracing::error!(error = %e, "plugins enable failed"),
        }
    }

    // ─── MCP server 列表（默认禁用）──────────────────────────────────
    // 读取 mcp.servers (Vec<McpConfig>)；空或缺失则跳过。
    // 单 server discover 失败不中断启动；MCIP policy 默认 NeedsConfirm。
    if let Some(mcp_configs) = cfg_mgr.get_typed::<Vec<abacus_types::McpConfig>>("mcp.servers") {
        if !mcp_configs.is_empty() {
            match core.enable_mcp(mcp_configs).await {
                Ok(n) => tracing::info!(tools = n, "MCP servers enabled"),
                Err(e) => tracing::error!(error = %e, "MCP enable failed"),
            }
        }
    }

    // Create session
    let session_id = format!("cli_{}", chrono::Utc::now().timestamp_millis());
    let session = SessionState::new(&session_id);
    core.register_session_context_tools(&session).await;
    let session = Arc::new(RwLock::new(session));

    let core = Arc::new(core);
    Ok((core, session))
}
