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
//! 2. ~/.abacus/config.toml + provider.toml (TUI setup wizard output)
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

const DEFAULT_SYSTEM_PROMPT: &str = "\
You are Abacus, an autonomous agent kernel in the user's local environment.

Execute through tools — tool-verified facts beat memory and speculation.
Language: follow the user's. Default Chinese for conversation, English for code.

Output: direct and concise. No filler, no emoji (unless requested). Lead with the answer.
Code: include file path. Complete implementations only — no placeholders.
Errors: report the actual error and your diagnosis.

Safety: no destructive ops without confirmation. No fabricated paths/APIs.";

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

    // ─── ConfigManager: env vars + ~/.abacus/config.toml ───────────────
    let mut cfg_mgr = ConfigManager::new(default_config());
    cfg_mgr.load_env("ABACUS_");

    // 配置加载顺序：默认内置层 < models.toml < config.toml < security.toml < provider.toml < conf.d/*.toml < 环境变量
    // 路径走 abacus_core::paths，遵循 ABACUS_HOME 覆盖。
    use abacus_core::paths;
    let _ = cfg_mgr.load_file(paths::models_toml());      // models.toml — 模型能力 catalog 覆盖
    let _ = cfg_mgr.load_file(paths::config_toml());      // config.toml — Abacus 行为配置
    let _ = cfg_mgr.load_file(paths::security_toml());    // security.toml — safety / MCIP 安全配置
    let _ = cfg_mgr.load_provider_file(paths::provider_toml()); // provider.toml — 供应商配置（唯一真相源）
    cfg_mgr.load_dir(paths::conf_d_dir());                // conf.d/ — 自定义扩展配置

    // Validate config before using — warn on out-of-range values
    let validation_warnings = cfg_mgr.validate();
    for w in &validation_warnings {
        tracing::warn!("config validation: {}", w);
    }

    // 检测废弃的 llm.* 配置键，提示迁移到 provider.toml
    let deprecated_warnings = cfg_mgr.detect_deprecated_keys();
    if !deprecated_warnings.is_empty() && !cfg_mgr.has_provider_entries() {
        tracing::warn!(
            "检测到旧版 llm.* 配置但无 provider.toml 配置。\
             请将配置迁移到 ~/.abacus/provider.toml 以获得多 provider 支持。"
        );
    }

    // Model — prefer config over parameter
    let resolved_model = cfg_mgr.get_str("core.default_model").unwrap_or(model);

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
            cfg_intent.unwrap_or(abacus_types::ThinkingIntent::Off)
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

    // Phase 3：模型能力 catalog——builtin + paths::models_toml() 覆盖
    let mut catalog = abacus_core::llm::ModelCatalog::builtin();
    let toml_path = paths::models_toml();
    match catalog.merge_toml(&toml_path) {
        Ok(0) => {}
        Ok(n) => tracing::info!("Loaded {} model spec override(s) from {}", n, toml_path.display()),
        Err(e) => tracing::warn!("Failed to merge {}: {}", toml_path.display(), e),
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
        // Reasoning 增强（ToT / Self-Consistency）
        reasoning_config: abacus_core::core::reasoning_integration::ReasoningConfig::default(),
        // 内容分类引擎
        triage: abacus_core::core::triage::TriageConfig {
            enabled: cfg_mgr.get_bool("triage.enabled").unwrap_or(true),
            audit_only: cfg_mgr.get_bool("triage.audit_only").unwrap_or(true),
            keep_count: cfg_mgr.get_number("triage.keep_count").map(|n| n as usize).unwrap_or(5),
            early_keep: cfg_mgr.get_number("triage.early_keep").map(|n| n as usize).unwrap_or(2),
            inject_threshold: cfg_mgr.get_number("triage.inject_threshold").unwrap_or(0.65),
            standby_threshold: cfg_mgr.get_number("triage.standby_threshold").unwrap_or(0.40),
            cold_threshold: cfg_mgr.get_number("triage.cold_threshold").unwrap_or(0.20),
            hysteresis_deadband: cfg_mgr.get_number("triage.hysteresis_deadband").unwrap_or(0.15),
            sticky_turns: cfg_mgr.get_number("triage.sticky_turns").map(|n| n as u32).unwrap_or(3),
            cooldown_turns: cfg_mgr.get_number("triage.cooldown_turns").map(|n| n as u32).unwrap_or(10),
            max_compress_depth: cfg_mgr.get_number("triage.max_compress_depth").map(|n| n as u32).unwrap_or(3),
                standby_capacity: cfg_mgr.get_number("triage.standby_capacity").map(|n| n as usize).unwrap_or(200),
                cold_batch_cap: cfg_mgr.get_number("triage.cold_batch_cap").map(|n| n as usize).unwrap_or(20),
                skip_below_msg_count: cfg_mgr.get_number("triage.skip_below_msg_count").map(|n| n as usize).unwrap_or(8),
            },
    };

    let mut core = CoreLoop::new(registry, skill_engine, cap_hub, ctx_mgr, config).await;

    // ─── 资源感知（治本：从 config.toml [llm_budget] 真读取并 reconfigure）────────
    // 用户在 `config.toml` 显式启用 `[llm_budget]` 段后才会有限额；
    // 默认 0 = 不限（opt-in 语义），保持向后兼容。
    let budget_cfg = cfg_mgr.llm_budget_config();
    if budget_cfg.max_cost_usd > 0.0 || budget_cfg.max_total_tokens > 0 {
        tracing::info!(
            "llm_budget enabled: max_cost=${:.2} max_tokens={} \
             soft={:.2} hard={:.2} reject={:.2}",
            budget_cfg.max_cost_usd, budget_cfg.max_total_tokens,
            budget_cfg.soft_threshold, budget_cfg.hard_threshold, budget_cfg.reject_threshold
        );
    } else {
        tracing::debug!("llm_budget not enabled (max_cost_usd=0 max_total_tokens=0); resources unlimited");
    }
    core.reconfigure_llm_budget(budget_cfg);

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

    // ─── 本地 Embedding/Reranker 服务注入 ───────────────────────────────
    // V42-B: 自动发现 + 手动配置合并 + health check + reranker 实际注入
    let mut local_health = abacus_core::local_provider::LocalModelHealth::default();

    // 1. 自动发现本地服务（Ollama / vLLM / Generic）
    let detector = abacus_core::local_provider::LocalProviderDetector::new();
    let discovered = detector.discover().await;
    if !discovered.is_empty() {
        tracing::info!(count = discovered.len(), endpoints = ?discovered.iter().map(|e| (&e.provider, &e.base_url)).collect::<Vec<_>>(), "本地模型服务自动发现结果");
    }

    // 2. 从 config 读取手动配置
    let cfg_embed_url = cfg_mgr.get_str("local.embedding_url");
    let cfg_embed_model = cfg_mgr.get_str("local.embedding_model");
    let cfg_embed_dim = cfg_mgr.get_number("local.embedding_dim").map(|n| n as usize);
    let cfg_rerank_url = cfg_mgr.get_str("local.reranker_url");
    let cfg_rerank_model = cfg_mgr.get_str("local.reranker_model");

    // 3. 合并：手动配置优先，否则用自动发现推断
    // V42-B FIX: cfg_mgr.get_str() 返回 Option<&str>，guess_*_config 返回 Option<(String, ...)>，
    // 必须统一类型（手动配置也转为 String）。
    let embed_config: Option<(String, String, usize)> = match (cfg_embed_url, cfg_embed_model, cfg_embed_dim) {
        (Some(u), Some(m), Some(d)) => {
            tracing::info!("本地 embedding 使用手动配置");
            Some((u.to_string(), m.to_string(), d))
        }
        _ => {
            let guessed = abacus_core::local_provider::LocalProviderDetector::guess_embedding_config(&discovered);
            if guessed.is_some() {
                tracing::info!("本地 embedding 使用自动发现配置");
            }
            guessed
        }
    };

    let rerank_config: Option<(String, String)> = match (cfg_rerank_url, cfg_rerank_model) {
        (Some(u), Some(m)) => {
            tracing::info!("本地 reranker 使用手动配置");
            Some((u.to_string(), m.to_string()))
        }
        _ => {
            let guessed = abacus_core::local_provider::LocalProviderDetector::guess_reranker_config(&discovered);
            if guessed.is_some() {
                tracing::info!("本地 reranker 使用自动发现配置");
            }
            guessed
        }
    };

    // 4. 创建 embedder + health check + 注入
    let _vllm_embedder: Option<Arc<abacus_core::vllm_embedder::VllmEmbedder>> = if let Some((url, model, dim)) = embed_config {
        let embedder = abacus_core::vllm_embedder::create_embedder_from_config(&url, &model, dim);
        let healthy = embedder.health_check().await;
        local_health.embedding_running = healthy;
        local_health.embedding_model = model.clone();
        if healthy {
            kb_store.set_embedder(embedder.clone() as Arc<dyn abacus_core::memory_palace::MemoryEmbedder>);
            palace.read().await.set_embedder(embedder.clone() as Arc<dyn abacus_core::memory_palace::MemoryEmbedder>).await;
            tracing::info!(embed_url = %url, embed_model = %model, embed_dim = dim, "本地 embedding 服务已连接并注入");
            Some(embedder)
        } else {
            tracing::warn!(embed_url = %url, "本地 embedding 服务配置/发现成功但 health check 失败，降级为关键词匹配");
            None
        }
    } else {
        tracing::info!("本地 embedding 服务未配置且未自动发现，语义搜索降级为关键词匹配");
        None
    };

    // 5. 记录 provider 类型（用于 TUI 展示）
    if let Some(ep) = discovered.first() {
        local_health.provider_type = ep.provider.label().to_string();
    }

    // 注册 kb.* 工具执行器（schema 已由 register_all() 在 CoreLoop::new() 内注册）
    // registry 已 move 进 CoreLoop，通过 tool_registry_ref() 取回 Arc 引用
    abacus_core::tool::builtin::kb::register_executors(core.tool_registry_ref(), kb_store.clone(), palace.clone()).await;

    // V42: 注入 memory palace 到 CoreLoop（面板数据拉取 + pipeline 主动读写 + PalaceAbsorbHook）
    // 引用关系：core.memory_palace() 被 TUI run.rs 读取 → state.palace_data
    // 必须在 register_executors 之后调用（with_memory 内部重注册 result.expand executor）
    let palace_clone = palace.clone();
    core = core.with_memory(kb_store.clone(), palace).await;

    // 6. 创建 reranker + health check + 注入 triage_engine
    // V42-B: 之前此处仅打印日志，未实际注入。reranker 注入必须在 with_memory 之后
    //（triage_engine 在 with_memory 中才被完整初始化）。
    if let Some((url, model)) = rerank_config {
        let reranker = abacus_core::vllm_embedder::create_reranker_from_config(&url, &model);
        let healthy = reranker.health_check().await;
        local_health.reranker_running = healthy;
        local_health.reranker_model = model.clone();
        if healthy {
            core.inject_reranker(reranker).await;
            tracing::info!(reranker_url = %url, reranker_model = %model, "本地 reranker 服务已连接并注入 triage");
        } else {
            tracing::warn!(reranker_url = %url, "本地 reranker 配置/发现成功但 health check 失败，跳过注入");
        }
    }

    // 7. 同步本地模型健康状态到 CoreLoop（TUI 通过 core.local_model_health() 读取）
    core.set_local_model_health(local_health);

    // ─── 知识库种子数据 ─────────────────────────────────────────────
    // 首次启动时注入基础编码知识，提升 LLM 代码生成质量
    // 后续通过 kb.ingest 工具或用户手动添加更多知识
    {
        let seeds = knowledge_seed_data();
        for (path, content) in seeds {
            if let Err(e) = kb_store.ingest_text(path, content).await {
                tracing::debug!("知识库种子注入跳过 {}: {}", path, e);
            }
        }
    }

    // ─── 记忆宫殿种子数据 ───────────────────────────────────────────
    // 首次启动时注入行为模式和领域知识到双宫殿记忆系统
    {
        let palace_guard = palace_clone.read().await;
        let (behaviors, knowledge) = palace_seed_data();
        for (pattern, tags) in behaviors {
            let string_tags: Vec<String> = tags.iter().map(|s| s.to_string()).collect();
            palace_guard.record_interaction(pattern, &string_tags).await;
        }
        for (id, title, content, domain) in knowledge {
            let entry = abacus_core::memory_palace::KnowledgeEntry::new(id, title, content, domain);
            palace_guard.store_knowledge(entry).await;
        }
    }

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
    // 2026-05-28: 优先使用 provider.toml `[[providers]]` 数组（多供应商多模型）
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
                        api_key, models.first().cloned().unwrap_or_else(|| ModelId(resolved_model.to_string())),
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
                        base, None, None, None,  // auth_prefix: default "Bearer "
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
            clean_base, None, None, None,  // auth_prefix: default "Bearer "
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

    // ─── MCIP 权限配置（来自 security.toml）───────────────────────
    // 内置工具前缀（fs_/lsp./kb_ 等）已硬编码豁免，无需配置。
    core.configure_mcip_permissions(
        &cfg_mgr.get_list("mcip.exempt_prefixes").unwrap_or_default(),
        &cfg_mgr.get_list("mcip.allow_tools").unwrap_or_default(),
        &cfg_mgr.get_list("mcip.deny_tools").unwrap_or_default(),
    );

    // ─── LSP 支持（按需激活）──────────────────────────────────────────
    // 语言服务器是 lazy start：首次调用工具时才实际启动。
    // 可用 `lsp.enabled = false` 在 ~/.abacus/config.toml 中禁用。
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

    // ─── 脚本钩子加载（magchain.hooks）───────────────────────────────
    // 从 config.toml 读取 magchain.hooks 列表，注册为 PipelineHook。
    // 支持 rhai:// / sh:// / py:// 三种运行时。
    if let Some(hook_configs) = cfg_mgr.get_typed::<Vec<abacus_core::script_hook::HookConfig>>("magchain.hooks") {
        for cfg in hook_configs {
            match abacus_core::script_hook::ScriptHook::from_config(cfg) {
                Ok(hook) => {
                    let priority = hook.priority();
                    core.add_pipeline_hook(priority, std::sync::Arc::new(hook)).await;
                }
                Err(e) => tracing::warn!(error = %e, "script hook registration failed, skipping"),
            }
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

/// 知识库种子数据 — 首次启动时注入的 Abacus 架构知识
///
/// 返回 (virtual_path, content) 列表。
/// virtual_path 用于 FTS5 索引和去重；content 为 Markdown 格式知识内容。
///
/// ## 设计意图
/// - 基于 Abacus 实际架构模式提炼
/// - 覆盖分层架构、Pipeline、Provider、Memory、Skill、Tool 等核心系统
/// - 帮助 LLM 理解项目结构和编码约定
fn knowledge_seed_data() -> Vec<(&'static str, &'static str)> {
    vec![
        ("seed://abacus-architecture", r#"# Abacus 分层架构

## 依赖层级（严格 L0→L4）
```
abacus-types (L0)     — 纯数据类型，无运行时依赖
    ↓
abacus-core (L1+L2)   — CoreLoop, ToolRegistry, LLM providers, Config, Memory
    ↓
abacus-orchestrator (L3) — Team, SubAgent, Plan, Meeting 编排
    ↓
abacus-ui-kit (L3.5)   — 共享 UI 原语 (CardStream, Theme)
    ↓
abacus-cli / abacus-server (L4) — TUI 和 HTTP 入口
```

## 核心模块
- `CoreLoop` — 主事件循环，管理 provider、tool、skill、memory
- `TurnPipeline` — 多阶段执行管线（setup→analyze→build→execute→post）
- `MagChain` — 优先级中间件链（CircuitBreaker→RateLimiter→EpistemicGuard→PiiRedactor→AuditLogger）
- `DualPalaceMemory` — 双宫殿记忆系统（行为宫+知识宫+记忆桥）
- `SkillEngine` — 技能引擎（多策略匹配+宫殿增强评估）
- `ToolRegistry` — 工具注册表（懒加载+可见性分层+panic 隔离）

## 关键设计原则
- **SSOT**: 每个数据概念有唯一所有者
- **优雅降级**: 永不崩溃，失败时降级到更简单的行为
- **Write-through**: 内存 + SQLite 双写
- **有界 FIFO**: 防止无限增长（BehaviorPalace 2000, KnowledgePalace 5000, MemoryBridge 30000）
- **OnceLock**: 进程级单例（共享 HTTP 客户端、废弃警告）
"#),

        ("seed://abacus-pipeline", r#"# Abacus TurnPipeline 执行管线

## 五个阶段
```
Phase 1: setup()           — 任务分类、技能匹配、系统提示词组装
Phase 2: analyze_complexity() — 思考意图解析
Phase 3: build LlmRequest  — Provider 选择、工具定义构建
Phase 4: execute_loop()    — LLM 调用 → 工具分派 → 迭代
Phase 5: post-processing   — 渐进门控、记忆更新、快照
```

## TurnContext 累积状态
```rust
struct TurnContext {
    turn_number: u32,
    total_tool_calls: u32,
    all_tool_outputs: Vec<ToolOutput>,
    provider: Arc<dyn LlmProvider>,
    classification: TaskClassification,
    complexity_thinking: Option<ThinkingIntent>,
    // ... 20+ 字段
}
```

## 关键路径
- 模型选择优先级: req_ctx.model > model_override > escalated > default
- 工具可见性: S/A/B/C/D 分层，低于阈值的工具自动隐藏
- 思考模式: ThinkingIntent::Off | Adaptive | Effort(level) | Budget(tokens)
- 上下文管理: warm → cold → compressed → discarded 四层衰减
"#),

        ("seed://abacus-provider", r#"# Abacus Provider 抽象

## LlmProvider Trait
```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse>;
    async fn complete_streaming(&self, request: &LlmRequest) -> Result<StreamHandle>;
    fn supported_models(&self) -> Vec<String>;
}
```

## Provider 注册
```rust
// 注册 provider 组（模型列表 + provider 实例）
core.register_provider_group("deepseek", models, provider).await;
// 注册 fallback provider
provider_registry.set_fallback(provider).await;
```

## 模型路由
```
QualifiedModelId { provider: Option<ProviderId>, model: ModelId }
    ↓
ProviderRegistry.resolve()
    ↓
1. Qualified: 精确 provider + model 匹配
2. Unqualified: 反向索引 → 最高优先级 provider
3. Fallback: 已注册的 fallback provider
```

## FallbackProvider
包装两个 provider，自动协议降级（Anthropic 主 → OpenAI-compatible 备）

## 共享 HTTP 客户端
```rust
static SHARED_HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
// 进程级连接池复用
```
"#),

        ("seed://abacus-memory", r#"# Abacus 双宫殿记忆系统

## 架构
```
DualPalaceMemory
├── BehaviorPalace   — 交互模式（用户习惯、工具使用、纠正历史）
│   └── 最大 2000 条，LFU 淘汰
├── KnowledgePalace  — 领域知识（技术最佳实践、项目结构）
│   └── 最大 5000 条，SM-2 间隔重复
├── MemoryBridge     — 跨宫殿关系（16 种关系类型）
│   └── 最大 30000 条，FIFO 淘汰 + O1 去重索引
└── SqlitePalaceStore — 可选持久化（write-through）
```

## SM-2 间隔重复
```rust
pub fn sm2_update(&mut self, quality: f64) {
    let quality = quality.clamp(0.0, 5.0);
    if quality >= 3.0 {
        self.sm2_repetitions += 1;
        self.sm2_interval_days *= self.sm2_ease;
        self.sm2_ease += 0.1 - (5.0 - quality) * (0.08 + (5.0 - quality) * 0.02);
    } else {
        self.sm2_repetitions = 0;
        self.sm2_interval_days = 1.0;
    }
}
```

## 关系类型（5 维度）
- 结构: ParentChild, SummaryDetail
- 引用: SeeAlso, DependsOn
- 比较: Similar, Exclusive, Replaces
- 组合: ComposesTo, TagAggregator
- 演化: DraftOf, RevisionOf, Supersedes

## 智能检索（3 阶段）
1. 知识宫缓存探测（SM-2 热条目）
2. 行为宫 → 推荐最佳 Skill
3. 返回缓存命中或需要执行

## 自动吸收
- `PalaceAbsorbHook` 监听 `TurnPostFanOut` 事件
- 当 context tiers 将 SessionSnapshot 从 warm 降级到 cold 时
- 自动将 key_decisions 吸收到 KnowledgePalace
"#),

        ("seed://abacus-skill", r#"# Abacus 技能系统

## 技能定义
```rust
pub struct SkillDef {
    pub id: SkillId,
    pub triggers: SkillTriggers,  // keywords, regex, domain
    pub prompt: String,
    pub steps: Vec<SkillStep>,
}
```

## 多策略匹配（4 层）
1. **关键词匹配** — 精确子串，权重 0.6
2. **正则匹配** — 权重 0.8
3. **领域匹配** — task_kind 对齐，权重 0.5
4. **语义匹配** — n-gram Jaccard 或自定义 embedder，权重 0.3

## 宫殿增强评估
`evaluate_with_palace()` 用行为宫历史增强技能匹配：
- 成功信号: 7 天半衰期衰减 × log 频率权重
- 失败惩罚: 24 小时指数衰减
- 领域匹配奖励

## 内置技能（7 个）
| 技能 | 用途 | 触发词 |
|------|------|--------|
| search_file | 文件搜索 | 找文件, 搜索文件 |
| search_code | 代码符号定位 | 找函数, 找类 |
| web_research | 网络研究 | 搜索网络, research |
| knowledge | KB 语义检索 | 知识库, knowledge |
| data_query | 结构化数据查询 | 查数据库, sql |
| diagnose | 系统/错误诊断 | 诊断, 排查 |
| config_find | 配置文件查找 | 找配置, config |
"#),

        ("seed://abacus-tool", r#"# Abacus 工具系统

## 工具注册
```rust
pub struct ToolRegistry {
    tools: RwLock<HashMap<ToolId, ToolHandle>>,
    executors: RwLock<HashMap<ToolId, Arc<dyn ToolExecutor>>>,
    lazy_loaders: Vec<Box<dyn LazyToolLoader>>,
    tools_cache: RwLock<Option<Vec<ToolHandle>>>,
    lint_rules: RwLock<LintRuleSet>,
}
```

## 可见性分层
工具按效果排名 S/A/B/C/D，Pipeline 只注入阈值以上的工具：
```rust
pub enum VisibilityTier { S, A, B, C, D }
```

## Panic 隔离
```rust
let handle = tokio::spawn(async move { exe.execute(...).await });
let abort_handle = handle.abort_handle();
let result = tokio::time::timeout(Duration::from_secs(timeout), handle).await;
// Panic → ToolOutput { success: false, failure_kind: "Panic" }
// Timeout → abort + ToolOutput { success: false, failure_kind: "Timeout" }
```

## 懒加载
```rust
#[async_trait]
pub trait LazyToolLoader: Send + Sync {
    async fn prepare(&self, tool_id: &ToolId) -> LazyLoadResult;
}
// Ready | Blocked { reason } | NotFound
```

## 工具分类（>50 工具时启用）
```rust
pub fn categories_for_task(task_kind: &str) -> Vec<ToolCategory> {
    match task_kind {
        "code_review" => vec![FileSystem, CodeExec, Lsp],
        "data_analysis" => vec![Network, Knowledge, CodeExec, Json],
        // ...
    }
}
```
"#),

        ("seed://abacus-error", r#"# Abacus 错误处理模式

## 统一错误枚举
```rust
#[derive(Error, Debug)]
pub enum KernelError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("API error: {status} {body}")]
    ApiError { status: u16, body: String },
    #[error("rate limited: retry after {retry_after}s")]
    RateLimited { retry_after: u64 },
    #[error("context overflow: {current} > {limit}")]
    ContextOverflow { current: usize, limit: usize },
    #[error("needs human review: {0}")]
    NeedsHumanReview(String),
    // ... 15 个变体
}
```

## 用户安全消息
每个 KernelError 有 `user_message()` 方法返回本地化、不泄露内部信息的消息：
```rust
impl KernelError {
    pub fn user_message(&self) -> String {
        match self {
            Self::Provider(_) => "服务提供商暂时不可用，请稍后重试".into(),
            Self::RateLimited { retry_after } => format!("请求限流，请 {}s 后重试", retry_after),
            // ...
        }
    }
}
```

## 优雅降级模式
```rust
// 磁盘失败降级到内存模式
let kb_store = KnowledgeStore::new(&kb_db_path)
    .unwrap_or_else(|e| {
        tracing::warn!("KnowledgeStore 磁盘初始化失败，降级内存模式: {e}");
        KnowledgeStore::in_memory().expect("in-memory KnowledgeStore must succeed")
    });
```

## 工具错误隔离
工具返回 `ToolOutput { success: false }` 而非 `Err`，防止一个工具失败破坏多轮循环。
"#),

        ("seed://abacus-config", r#"# Abacus 配置系统

## 多源配置优先级（从高到低）
1. CLI 参数 (`--key value`)
2. 环境变量 (`ABACUS_*` 前缀)
3. TOML 文件 (`~/.abacus/config.toml`, `provider.toml`, `security.toml`, `models.toml`, `conf.d/*.toml`)
4. 内置默认值

## ConfigManager
```rust
pub struct ConfigManager {
    merged: HashMap<String, TaggedValue>,  // 每个值标记来源
    provider_entries: Vec<ProviderEntry>,
}

pub struct TaggedValue {
    pub value: ConfigValue,
    pub source: ConfigSource,  // Default | File(path) | Env(var) | Cli(arg)
    pub key: String,
}
```

## Provider 配置
```toml
[[providers]]
id = "deepseek"
type = "deepseek"
api_key = "env:DEEPSEEK_API_KEY"  # 运行时环境变量查找
base_url = "https://api.deepseek.com"
```

## 配置验证
```rust
pub fn validate(&mut self) -> Vec<String> {
    let range_rules: &[(&str, f64, f64, f64)] = &[
        ("core.max_tokens", 100.0, 1_000_000.0, 8192.0),
        ("core.temperature", 0.0, 2.0, 0.7),
        // ...
    ];
    // 超范围值自动修正为默认值，返回警告
}
```

## 废弃键检测
```rust
pub fn detect_deprecated_keys(&self) -> Vec<String> {
    // 检测旧 llm.* 键，建议迁移到 provider.toml
    // 使用 OnceLock 保证每进程仅警告一次
}
```
"#),

        ("seed://abacus-state", r#"# Abacus TUI 状态管理

## 中央化 AppState
```rust
pub struct AppState {
    pub theme: Theme,
    pub mode: AbacusMode,        // Clarify | Plan | Team | Meeting
    pub messages: VecDeque<Message>,
    pub trace_events: Vec<TraceEvent>,
    pub input: String,
    pub input_state: InputState, // Ready | Typing | Thinking | Executing | Outputting | Editor
    pub focus: Focus,            // Input | Panel | CommandHint
    pub panel_tab: PanelTab,     // Timeline | Quant | Custom
    pub scroll: usize,
    // ... 100+ 字段
}
```

## RefCell 内部可变性
TUI 使用单线程 crossterm 事件循环，AppState 使用 RefCell 实现内部可变性：
```rust
// 安全不变量：RefCell borrow_mut() 作用域内不能有 .await 表达式
```

## 模式状态机
```
Clarify ←→ Meeting
Clarify → Plan → Team
```
模式间通过 ModeArtifact 传递数据（ClarifyBrief, MeetingConclusion）

## ScrollAction SSOT
```rust
pub enum ScrollAction {
    ToBottom, Up(usize), Down(usize), Absolute(usize),
    AnchorAdjust { after_rows: usize, before_rows: usize },
    Restore(usize),
}
```
单一入口 `set_scroll(ScrollAction)` 消除分散的 scroll 修改。

## Timeline Entry 模式
流式事件统一为线性 timeline：
```rust
pub enum TimelineEntry {
    Thinking { summary: String },
    Tool { name: String, status: StreamingToolStatus, ... },
    Text { start: usize, end: usize },
    Iteration { number: u32 },
}
```
"#),

        // ─── 代码审查（清洗自 Claude 知识库/code-review-comprehensive.md）───
        ("seed://code-review", r#"# 代码审查核心检查点

## 安全审查（OWASP Top 10 代码层面）
| 风险 | 检查点 |
|------|--------|
| 注入 | SQL/NoSQL/OS/命令注入，参数化查询 |
| XSS | innerHTML/dangerouslySetInnerHTML，上下文编码 |
| SSRF | 用户可控 URL 发起服务端请求，白名单校验 |
| 权限 | 每个入口的权限校验，IDOR 模式 |
| 密钥 | 硬编码密钥/Token，明文传输 |

## 竞态条件（TOCTOU）
- 文件系统：先检查存在再操作 → 用原子操作
- 金额/库存：先检查再扣减 → 用事务+行级锁
- 并发请求：多请求同改一资源 → 用锁或乐观锁

## 代码质量检查
- 函数长度 ≤ 50 行
- 圈复杂度 ≤ 10
- 嵌套深度 ≤ 4 层
- 错误处理：无 unwrap/expect（生产代码）
- 测试覆盖：新功能必有测试，bug 修复必有回归

## 常见反模式
- 过度工程：简单问题复杂化
- 重复逻辑：DRY 原则
- 全局状态：难以测试和推理
- 隐式依赖：依赖注入优于全局查找
"#),

        // ─── Rust 陷阱（清洗自 Claude 知识库/programming/rust/rust-pitfalls.md）───
        ("seed://rust-pitfalls", r#"# Rust 实战陷阱

## 1. `?` 在循环中的语义
`?` 在 for 循环内会从**外层函数** return，不是跳过迭代。
```rust
// 错误：一个 entry 失败 → 整个函数返回 None
for entry in fs::read_dir(dir).ok()? {
    let meta = entry.metadata()?;  // ← 这里 ? 会让函数返回
}
// 正确：用 match/continue 跳过失败项
for entry in fs::read_dir(dir).ok()?.flatten() {
    let meta = match entry.metadata() {
        Ok(m) => m,
        Err(_) => continue,
    };
}
```

## 2. 数据源优先级
权威源不应被推断源覆盖。用 `Option` + 显式优先级：
```rust
let value = authoritative_source
    .or(inferred_source)
    .unwrap_or(default);
```

## 3. OnceLock 使用
进程级单例，初始化后不可变：
```rust
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| reqwest::Client::new())
}
```

## 4. RefCell 安全
- `borrow()` / `borrow_mut()` 在作用域内持有锁
- 嵌套 borrow 会 panic
- 异步代码中用 `tokio::sync::RwLock` 替代

## 5. 生命周期省略
- `fn foo(&self) -> &str` → 返回值生命周期 = self
- 多个引用时必须显式标注
- `'static` 不是万能解药
"#),

        // ─── TypeScript 模式（清洗自 Claude 知识库/programming/typescript/ts-mastery.md）───
        ("seed://typescript-patterns", r#"# TypeScript 核心模式

## 类型安全工具函数
```typescript
// 穷尽性检查
function assertNever(x: never): never {
  throw new Error(`Unexpected value: ${x}`);
}

// 类型安全的 Object.keys
function typedKeys<T extends object>(obj: T): (keyof T)[] {
  return Object.keys(obj) as (keyof T)[];
}

// 非空断言（运行时检查）
function nonNull<T>(value: T | null | undefined, msg?: string): T {
  if (value == null) throw new Error(msg ?? "Expected non-null");
  return value;
}
```

## Discriminated Union（Result 模式）
```typescript
type Result<T, E = Error> =
  | { ok: true; value: T }
  | { ok: false; error: E };

async function fetchUser(id: string): Promise<Result<User, ApiError>> {
  try {
    const resp = await fetch(`/api/users/${id}`);
    if (!resp.ok) return err({ code: resp.status, message: resp.statusText });
    return ok(await resp.json());
  } catch (e) {
    return err({ code: 0, message: String(e) });
  }
}
```

## 类型窄化
```typescript
function process(value: string | number) {
  if (typeof value === "string") {
    // 这里 value 类型是 string
    return value.toUpperCase();
  }
  // 这里 value 类型是 number
  return value.toFixed(2);
}
```

## 常见陷阱
- `any` 是类型系统的逃生舱，尽量避免
- `as` 断言绕过类型检查，优先用类型窄化
- `!` 非空断言可能掩盖 null 错误
- 泛型约束 `extends` 优于 `any`
"#),

        // ─── API 设计（清洗自 Claude 知识库/api-design-comprehensive.md）───
        ("seed://api-design", r#"# API 设计规范

## RESTful 基础
- 资源用名词复数：`GET /api/v1/users`
- HTTP 方法表达语义：GET(读) / POST(创建) / PUT(全量更新) / PATCH(部分更新) / DELETE(删除)
- 层级关系用路径：`GET /api/v1/users/{id}/orders`
- 过滤/排序/分页用查询参数：`?status=active&page=1&limit=20`

## HTTP 状态码
- 200 成功 / 201 创建成功 / 204 删除成功
- 400 参数错误 / 401 未认证 / 403 无权限 / 404 不存在
- 409 冲突 / 422 语义验证失败 / 429 限流 / 500 服务端错误

## 错误响应标准化
```json
{
  "error": {
    "code": "INVALID_PARAMETER",
    "message": "User email is required",
    "details": [{"field": "email", "reason": "required"}],
    "requestId": "req_abc123"
  }
}
```

## 分页策略
- 数据 < 10K：offset/limit 可用
- 数据 >= 10K：cursor-based 分页（常量查询性能）

## 安全
- 始终使用 HTTPS
- 认证：OAuth 2.1 + JWT
- 输入校验 + Schema 强制
- 限流 + Quota 管理
- CORS 白名单
"#),

        // ─── 前端规范（清洗自 Claude 知识库/code-standards.md）───
        ("seed://frontend-standards", r#"# 前端代码规范

## HTML 语义化
- 用 `<header>` / `<nav>` / `<main>` / `<article>` / `<aside>` / `<footer>` 替代 div
- 图片必须有 alt（装饰图 alt=""）
- 表单 label 关联 input
- 按钮必须有文字或 aria-label

## CSS 规范
- BEM 命名：`.block__element--modifier`
- 优先用 Flexbox/Grid 布局
- 避免 !important
- 用 CSS 变量管理主题色
- 响应式：mobile-first，断点统一

## JavaScript/TypeScript
- 用 `const` 优先，需要重赋值用 `let`，禁用 `var`
- 用 `===` 严格相等
- async/await 优于 .then() 链
- 错误处理：try/catch + 具体错误类型
- 避免 `any`，用 `unknown` + 类型窄化

## 性能
- 图片懒加载
- 代码分割（dynamic import）
- 虚拟列表（大数据量）
- 防抖/节流（用户输入/滚动）
- 缓存策略（HTTP Cache / Service Worker）

## 可访问性 (A11y)
- 键盘导航支持
- 屏幕阅读器兼容
- 颜色对比度 ≥ 4.5:1
- 焦点指示器可见
"#),
    ]
}

/// 记忆宫殿种子数据 — 首次启动时注入的行为模式和领域知识
///
/// 返回 (behaviors, knowledge) 两个列表：
/// - behaviors: (pattern, tags) 列表，注入到行为宫
/// - knowledge: (id, title, content, domain) 列表，注入到知识宫
///
/// ## 设计意图
/// - 行为宫：注入用户偏好和工作流模式
/// - 知识宫：注入架构知识和最佳实践
/// - 基于 Claude MEMORY.md 清洗后的内容
fn palace_seed_data() -> (
    Vec<(&'static str, Vec<&'static str>)>,
    Vec<(&'static str, &'static str, &'static str, &'static str)>
) {
    let behaviors = vec![
        // 用户偏好
        ("用户偏好简洁回答，结论先行", vec!["preference", "style", "core"]),
        ("用户偏好中文交互，技术术语保留英文", vec!["preference", "language", "core"]),
        ("修改代码前先声明影响范围", vec!["preference", "workflow", "coding"]),
        ("破坏性操作前需用户确认", vec!["preference", "safety", "core"]),
        ("优先编辑现有文件而非新建", vec!["preference", "workflow", "coding"]),
        ("避免过度设计，最小复杂度", vec!["preference", "style", "coding"]),
        // 工具使用模式
        ("优先使用专用工具（grep > bash grep）", vec!["pattern", "tool", "core"]),
        ("批量操作前先小范围验证", vec!["pattern", "safety", "core"]),
        ("长命令加 timeout 避免阻塞", vec!["pattern", "tool", "core"]),
        // 工作流模式
        ("代码修改遵循：声明→执行→验证三步", vec!["workflow", "coding", "core"]),
        ("测试先行：新功能必有测试，bug 修复必有回归", vec!["workflow", "testing", "coding"]),
        ("小步重构：每步可编译可测试", vec!["workflow", "refactor", "coding"]),
    ];

    let knowledge = vec![
        // 用户画像
        ("user-profile", "用户画像", r#"## 基本信息
- Workspace: macOS (darwin)
- Shell: zsh
- 语言偏好: 中文交互，技术术语英文
- 风格偏好: 简洁、结论先行、避免冗余

## 工作偏好
- 代码修改前声明影响范围
- 破坏性操作前确认
- 优先编辑现有文件
- 避免过度设计
- 测试先行
"#, "user"),
        // 记忆生命周期
        ("memory-lifecycle", "记忆生命周期", r#"## 三层记忆模型
- **Hot**: 配置文件、当前会话上下文
- **Warm**: 项目知识、用户偏好、工作流模式
- **Cold**: 历史归档、旧会话记录

## 降温规则
- criticality:HIGH 不自动降温
- 场景专用内容 → 归档候选
- MEMORY.md > 150 行 → 合并/降温

## 可信度评估（5 维）
1. 时效性: last-validated + decay-rate
2. 来源可信度: 用户确认 > 工具验证 > 模型推断
3. 上下文相关性: domain match + 语义距离
4. 结构依赖: graph 被引用次数/centrality
5. 使用频率: hit count 作为加载优先级信号
"#, "system"),
        // 工作流模式
        ("workflow-patterns", "工作流模式", r#"## 代码修改流程
1. 理解上下文（读 imports，看调用链）
2. 声明修改内容、影响范围、验证方式
3. 执行修改（小步、单一职责）
4. 验证（测试通过、编译无错）
5. 交付清单（修改内容、影响范围、审查结果）

## 调试流程
1. 复现问题（最小输入）
2. 定位根因（二分法、日志、断点）
3. 修复验证（测试通过）
4. 回归防护（新增测试用例）

## 重构流程
1. 确保测试通过
2. 小步重构（每步可编译可测试）
3. 不改变外部行为
4. 运行完整测试套件
"#, "workflow"),
        // 安全规则
        ("safety-rules", "安全规则", r#"## 安全红线
- 不在代码中硬编码密钥/Token/密码
- 不在日志中输出敏感信息
- 不在错误信息中暴露内部路径
- 不绕过权限检查

## D-HIL（人在回路）
涉及以下领域时必须确认：
- financial: 支付、资金、订单
- infra: CI/CD、部署、防火墙
- auth: 认证、凭据、token

## 强验证场景
- 破坏性操作（删除、清空、重置）
- 凭据相关（API Key、Token）
- 规则变更（配置、权限、策略）
- 生产环境操作（部署、迁移）
"#, "security"),
        // 性能优化
        ("performance-patterns", "性能优化模式", r#"## 优化原则
- 先测量，后优化（避免过早优化）
- 关注热路径（80/20 法则）
- 基准测试验证改进效果

## 资源管理
- 及时释放资源（RAII 模式）
- 避免不必要的内存分配
- 使用流式处理大数据
- 设置超时防止无限等待

## 并发模式
- 使用 tokio 异步运行时
- RwLock 读写锁（非 Mutex）
- OnceLock 进程级单例
- AtomicU64 无锁计数器
"#, "performance"),
    ];

    (behaviors, knowledge)
}
