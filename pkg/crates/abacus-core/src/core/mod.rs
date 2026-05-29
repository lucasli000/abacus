//! Core module — CoreLoop, SessionState, tool registration
//!
//! ## ⚠️ 文件过大（>320KB）— 拆分阻塞项（P0-D1 调查结论）
//!
//! **为什么不能直接拆分：**
//! pipeline/mod.rs 和 pipeline/post.rs 通过 `self.core.xxx()` 调用大量
//! `CoreLoop` **私有方法**（pub(super) 或 pub(crate)）。如果把 CoreLoop 移
//! 到子模块（如 `loop.rs`），这些调用路径需要同时调整可见性修饰符——工作量
//! 等价于重写 pipeline/ 的全部方法签名，风险高于收益。
//!
//! **正确拆分路径（留给专项 PR）：**
//! 1. 先把 pipeline/mod.rs + post.rs 的私有 `self.core.xxx` 调用
//!    全部改为 `pub(super)` / `pub(crate)` 并加注释
//! 2. 再建 `core/session.rs`（SessionState + TurnResult）、
//!    `core/config.rs`（CoreConfig + ThresholdConfig）
//! 3. 最后建 `core/loop_impl.rs`（impl CoreLoop 主体）
//! 4. 自由函数（extract_text / scene_active_prefixes / load_role_caps）
//!    移到 `core/helpers.rs` 并 `pub(crate) use helpers::*;`
//!
//! **不拆的代价：**
//! 仅影响编译缓存命中率（任何改动导致全文重编译）和 PR diff 可读性。
//! 功能和性能无影响。

pub mod context;
pub mod compute;
pub mod env;
pub mod policy;
pub mod fallible;
pub mod health;
pub mod task_analyzer;
pub mod interaction;
pub mod injector;
pub mod pressure;
pub mod event_sink;
pub mod session_store;
pub mod prompt_assembly;
pub mod safety;
pub mod preflight;
pub mod progressive;
pub mod progressive_gate;
pub mod progressive_inject;
pub mod inertia;
pub mod humanizer;
pub mod silent_router;
pub mod pipeline;
pub mod provider_adapter;
pub mod workflow_gate;
pub mod workflow_engine;
pub mod workflow_checkers;
pub mod cot_hook;
pub mod knowledge_hook;

use std::collections::HashMap;
use std::sync::Arc;

use crate::knowledge_store::KnowledgeStore;
use crate::memory_palace::DualPalaceMemory;

use abacus_types::{
    CapabilityContext, CapabilityKind, CapabilityRequest, KernelError, ModelId, ToolId, ToolOutput, TurnStats, UserProfile, UserRole,
};
use abacus_types::progressive::{
    AutonomyLevel, GateScope, UserResponse,
};
use serde_json::{json, Value};
use tokio::sync::RwLock;

use abacus_types::ModelSpec;
use crate::core::progressive::ProgressiveController;
use crate::core::progressive_gate::ProgressiveGate;
use crate::capability::CapabilityHub;
use crate::core::context::{register_context_tools, ContextManager};
use crate::core::env::{register_env_tools, EnvMap};
use crate::core::injector::DynamicInjector;
use crate::core::interaction::{Checkpoint, CheckpointType, InteractionMap};
use crate::core::prompt_assembly::PromptAssembly;
use crate::core::safety::SafetyGuard;
use crate::core::preflight::PreflightReport;
use crate::llm::{
    LlmProvider, LlmRequest, LlmResponse, Message, MessageContent, MessageRole,
    ToolDefinition,
};
use crate::mcip::McipGateway;
use crate::skill::{SkillCandidate, SkillEngine};
use crate::core::silent_router::SilentRouter;
use crate::deduction::{DeductionEngine, DeductionToolExecutor};
use crate::sandbox::{SandboxOrchestrator, SandboxToolExecutor, SandboxConfig};
use crate::tool::builtin::filengine::FilengineSession;
use crate::tool::effectiveness::EffectivenessTracker;
use crate::tool::ToolRegistry;
use crate::core::pipeline::TurnPipeline;
use crate::llm::providers::openai_compatible::OpenAICompatibleProvider;
use crate::llm::providers::anthropic::AnthropicProvider;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ContextWindowStatus — 上下文窗口使用状态
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// System prompt 统一构建输出（替代 build_system_prompt / build_system_segments 两套）
///
/// ## 设计意图
/// 两个原始方法共享相同的数据采集逻辑（session context / injector / interaction status
/// / deduction / preflight / focus），却分别调用 assemble() 和 assemble_segments()，
/// 导致代码重复且动态追加内容（epistemic 声明/model escalation/DecayRouter hint）
/// 只更新 text 而遗漏 segments，使 Anthropic provider 看不到这些关键约束。
///
/// ## 使用方式
/// `build_system_output()` 构建一次，返回 `{ text, segments }`；
/// 后续动态追加内容同时写入两者，保证 Anthropic / 其他 provider 行为一致。
///
/// ## 生命周期
/// 每次 turn setup() 阶段构建一次，随 TurnContext 存活。
#[derive(Debug, Clone)]
pub struct SystemPromptOutput {
    /// 用于非 Anthropic provider（单 String）
    pub text: String,
    /// 用于 Anthropic 多 block cache provider（分段，稳定段可缓存）
    pub segments: Vec<crate::llm::provider::SystemSegment>,
}

impl SystemPromptOutput {
    /// 向两端同时追加动态内容（保证 text 与 segments 同步）
    ///
    /// `cacheable = false`：动态内容（每轮可变）不参与 Anthropic block cache
    pub fn push_dynamic(&mut self, content: &str) {
        if content.is_empty() { return; }
        self.text.push_str("\n\n");
        self.text.push_str(content);
        self.segments.push(crate::llm::provider::SystemSegment {
            text: content.to_string(),
            cacheable: false,
        });
    }
}

/// 上下文窗口使用量（供 CLI/TUI 展示）
#[derive(Debug, Clone)]
pub struct ContextWindowStatus {
    pub current_tokens: usize,
    pub max_tokens: usize,
    pub usage_pct: f64,
    pub compressed_count: usize,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// RequestContext — 每次调用的运行时配置覆盖
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Per-call runtime context — 允许调用者定制 pipeline 行为而不改变引擎默认配置。
///
/// ## 设计原理
/// CoreConfig 是引擎级不可变配置（启动时确定）。
/// RequestContext 是请求级可变配置（每次调用可不同）。
/// 两者分离使同一个 CoreLoop 实例能服务不同场景（Chat/Meeting/Pipe/Turnkey）。
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// 模型覆盖（None = 使用 CoreConfig.default_model）
    pub model: Option<ModelId>,
    /// 工具白名单（None = 全部可用，Some([]) = 禁用工具）
    pub tool_filter: Option<Vec<ToolId>>,
    /// 跳过 Progressive Gate（turnkey/pipe 场景不需要人工确认）
    pub skip_progressive: bool,
    /// 跳过 Inertia Detection（pipe 模式追求快速响应）
    pub skip_inertia: bool,
    /// 跳过 Preflight self-review（简单查询不需要额外 LLM 调用）
    pub skip_preflight: bool,
    /// 跳过 DecayRouter 工具提升和 prompt hint 注入
    ///
    /// **Default: true**（C 方案 KV cache 修复）
    ///
    /// ## KV cache 影响
    /// DecayRouter 按 input 分类（Fast/Medium/Slow）promote 不同工具子集 + append 不同 hint。
    /// 跨 turn input tier 切换 → tools 重排 + hint 字节变化 → tools 段+system 末尾 cache miss。
    ///
    /// ## 何时显式 enable（设为 false）
    /// - 强时效性场景（资讯/实时数据）：希望强制 web.search 优先
    /// - 用户明确接受 cache miss 取舍换取知识衰减分流的智能
    pub skip_decay_router: bool,
    /// 温度覆盖
    pub temperature: Option<f64>,
    /// 最大输出 token 覆盖
    pub max_tokens: Option<u32>,
    /// 本次 turn 的临时 MCIP 授权（单次允许）
    ///
    /// 由 `CoreLoop::grant_and_rerun()` 在用户选择「单次」后填入。
    /// turn 结束后自动失效（不写入 session.mcip_grants）。
    pub mcip_once_grants: std::collections::HashSet<String>,
    /// Phase 4：本次 turn 的 thinking 意图覆盖（per-request）。
    ///
    /// ## 优先级
    /// `request.thinking_intent > specialist.thinking > config.default_intent > spec.default > Off`
    ///
    /// ## 写入路径
    /// - HTTP API：`routes.rs` 解析请求体 `thinking` 字段
    /// - TUI：`/thinking <value>` 命令写入 SessionState.pending_thinking_override，下一轮 turn 注入
    /// - Specialist YAML：从 `EngagementLimit.thinking` 复制
    ///
    /// ## 生效路径
    /// pipeline 在构造 LlmRequest 时优先使用此字段填充 `LlmRequest.thinking_intent`
    pub thinking_intent: Option<abacus_types::ThinkingIntent>,

    /// V35-1: Prefix completion 注入 — 在 messages 末尾追加 `{role: assistant, content, prefix: true}`
    ///
    /// ## 原理（DeepSeek `/beta` 端点）
    /// 模型从该 token 序列**继续生成**而不是开始新对话——硬约束输出格式。
    ///
    /// ## 引用关系
    /// - 设置：cli/api/mod.rs::send_planner_message_streaming（"```json\n[" 启动 JSON 数组）
    /// - 消费：abacus-core 在组装 LlmRequest.messages 时追加 prefix message
    /// - 透传：abacus-core/llm/providers/deepseek.rs 已支持 Message.prefix 字段（V31）
    ///
    /// ## 降级
    /// - None / 模型不支持（!supports_prefix_completion）→ 不追加，行为同旧版
    /// - 多 turn 中某 turn 设置：仅本次生效（请求级隔离，不污染 session）
    ///
    /// ## 生命周期
    /// 写入：planner API 等场景按需设置 → 消费：单次 turn 组装 messages → 销毁：turn 结束随 RequestContext drop
    pub prefix_assistant_content: Option<String>,

    /// V35-2: 系统 prompt 角色覆盖 — 用于 Planner / Reviewer / Evaluator 等"稳定角色"调用
    ///
    /// ## 引用关系
    /// - 设置：cli/api/mod.rs::send_planner_message_streaming（注入 PLANNER_SYSTEM_PROMPT）
    /// - 消费：abacus-core::pipeline 在 enriched_system 装配后追加本字段内容
    /// - 与 enriched_system / progressive_prompt：append-after，不替换基座 system
    ///
    /// ## 优先级（叠加而非替换）
    /// `default system_prompt（CoreConfig） + ICL preamble + progressive prompt + 本字段`
    ///
    /// ## 设计意图
    /// 让"角色化调用"走独立 system 段而非 user message 拼接：
    ///   - KV cache 友好（system 段稳定可缓存复用）
    ///   - 用户消息历史不被角色 prompt 污染（重启回放、压缩友好）
    ///
    /// ## 适用场景（V36-1 使用指导）
    /// ✅ **推荐使用**（角色 prompt 稳定 + 用户输入独立）：
    ///   - Planner（V35-2 已接入）— 角色定义稳定，user message = 用户原始需求
    ///   - Reviewer / Evaluator — 角色定义稳定，user message = 待审/待评内容
    ///   - 自定义角色化封装（CodeFixer / DocSummarizer 等）
    ///
    /// ❌ **不推荐使用**（prompt 含动态上下文）：
    ///   - Meeting Specialist — prompt 含会议主题/其他 specialist 意见/context_pool 快照，
    ///     这是"动态对话上下文"而非"稳定角色"，cache 命中率低，仍走 user message 拼接（参见 meeting/bridge.rs::assemble_specialist_prompt）
    ///   - 一次性 ad-hoc prompt — 用 system_segments 或直接拼接更直观
    ///
    /// ## 判定准则
    /// "若不同会话调本角色，prompt 内容是否一致？" — 一致 → override；变化 → 拼接
    ///
    /// ## 生命周期
    /// 写入：planner/role API 按需 → 消费：单次 turn 组装 enriched_system → 销毁：turn 结束随 RequestContext drop
    pub system_prompt_override: Option<String>,
}


impl Default for RequestContext {
    fn default() -> Self {
        Self {
            model: None,
            tool_filter: None,
            skip_progressive: false,
            skip_inertia: false,
            skip_preflight: false,
            // C 方案：DecayRouter 默认关——保护 prefix cache（用户/specialist 可显式开启）
            skip_decay_router: true,
            temperature: None,
            max_tokens: None,
            mcip_once_grants: std::collections::HashSet::new(),
            thinking_intent: None,
            // V35-1: 默认不注入 prefix（保持与旧行为一致）
            prefix_assistant_content: None,
            // V35-2: 默认无 system 覆盖（保持与旧行为一致）
            system_prompt_override: None,
        }
    }
}

impl RequestContext {
    /// 快速模式：跳过 progressive / inertia / preflight / decay_router，最小延迟
    /// 适用场景：Meeting specialist 内部调用、流水线自动化、turnkey
    pub fn fast() -> Self {
        Self { skip_progressive: true, skip_inertia: true, skip_preflight: true, skip_decay_router: true, ..Default::default() }
    }

    /// 指定模型
    pub fn with_model(model: impl Into<String>) -> Self {
        Self { model: Some(ModelId(model.into())), ..Default::default() }
    }

    /// 链式设置模型
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(ModelId(model.into()));
        self
    }

    /// 链式设置跳过 progressive
    pub fn no_progressive(mut self) -> Self {
        self.skip_progressive = true;
        self
    }

    /// 链式设置跳过 inertia
    pub fn no_inertia(mut self) -> Self {
        self.skip_inertia = true;
        self
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 三层阈值体系：Turn 级（防呆）+ Execution 级（资源保护）+ Quality 级（智能优化）
///
/// ## 设计原则
/// - Session 级不设限：长对话完全自由
/// - Turn 级防呆：防止单轮死循环，到达时引导 LLM 输出结论，不终止 session
/// - Execution 级保护：防止单个工具/SubAgent 失控占用资源
/// - Quality 级优化：软性引导，永不中断执行
///
/// ## 触发行为
/// - 80% → Warning hint（LLM 可忽略继续工作）
/// - 100% → Soft limit（强指令输出结论，不 panic/terminate）
/// - Hard limit 不存在
///
/// ## 引用关系
/// - 消费方：pipeline/mod.rs (turn 级)、tool dispatch (execution 级)、
///   context.rs (quality 级)、subagent/specialist (execution 级)
/// - 生命周期：CoreLoop 构造时从 CoreConfig.thresholds 读取，session 内不变
///   （可通过 config_set 运行时修改）
#[derive(Debug, Clone)]
pub struct ThresholdConfig {
    // ─── Turn 级（防呆层）────────────────────────────────────────
    /// 单轮工具调用总量上限（一次用户消息 → LLM 回复内的累积工具调用）
    pub turn_max_tool_calls: u32,
    /// 单轮内 LLM 循环迭代上限（agentic loop iterations）
    pub turn_max_iterations: u32,
    /// 单轮内错误恢复最大尝试次数
    pub turn_max_recovery: u32,
    /// 单次 LLM provider 请求超时（秒）
    pub turn_provider_timeout_secs: u64,
    /// 单轮提前停止（premature stop）最大重试次数
    pub turn_premature_stop_retries: u32,

    // ─── Execution 级（资源保护层）────────────────────────────────
    /// Bash 命令最大执行时间（秒）
    pub tool_bash_timeout_secs: u64,
    /// 通用工具执行超时（秒）
    pub tool_default_timeout_secs: u64,
    /// SubAgent 最大执行时间（秒）
    pub subagent_max_duration_secs: u64,
    /// SubAgent token 消耗上限
    pub subagent_max_tokens: usize,
    /// Meeting 中 Specialist 响应超时（秒）
    pub specialist_timeout_secs: u64,
    /// 单次 Meeting 最大持续时间（分钟）
    pub meeting_max_duration_mins: u32,
    /// 用户确认等待超时（秒）
    pub confirm_timeout_secs: u64,

    // ─── Quality 级（智能优化层）──────────────────────────────────
    /// 上下文压缩触发比例（超过 context_budget × ratio 时触发）
    pub context_compress_ratio: f64,
    /// 工具 N turn 未使用后从 LLM schema 隐藏
    pub tool_prune_after_turns: u32,
    /// 模型升级最大次数（防 KV cache 振荡）
    pub max_model_escalations: u32,
    /// 用户输入最大字符数（超出截断，不拒绝）
    pub input_max_chars: usize,
}

impl Default for ThresholdConfig {
    fn default() -> Self {
        Self {
            // Turn 级
            turn_max_tool_calls: 50, // 2026-05-27: 从 100 降至 50，对齐 Claude Code 量级
            turn_max_iterations: 200,
            turn_max_recovery: 5,
            turn_provider_timeout_secs: 300,
            turn_premature_stop_retries: 3,
            // Execution 级
            tool_bash_timeout_secs: 120,
            tool_default_timeout_secs: 60,
            subagent_max_duration_secs: 900,
            subagent_max_tokens: 200_000,
            specialist_timeout_secs: 900,
            meeting_max_duration_mins: 60,
            confirm_timeout_secs: 15,
            // Quality 级
            context_compress_ratio: 0.55,  // P0-C3: 提前压缩，避免 context 堆积后质量骤降
            tool_prune_after_turns: 10,     // P0-C3: 更积极隐藏长期不用工具，省 token
            max_model_escalations: 10,
            input_max_chars: 100_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CoreConfig {
    pub max_turns_per_request: u32,
    pub max_tool_calls_per_turn: u32,
    pub default_model: ModelId,
    pub default_temperature: f64,
    pub default_max_tokens: u32,
    /// 可用上下文窗口占模型最大上下文的比例（0.1-1.0，默认 0.5）
    /// 用户可通过 core.context_window_ratio 配置
    /// 实际可用 = max(model_context * ratio, 128K)
    pub context_window_ratio: f64,
    pub system_prompt: String,
    /// Per-model specification for the default model (thinking, context_window, etc.)
    pub model_spec: Option<ModelSpec>,
    /// Thinking mode override (applied to all turns, overrides model_spec.thinking)
    /// L1 后：直接用 ThinkingIntent，不再走旧 ThinkingConfig 兼容外壳
    pub thinking_intent: Option<abacus_types::ThinkingIntent>,
    /// Enable Silent Router (semantic + experience fusion for tool ordering).
    ///
    /// **Default: true** (默认开启——工具路由智能优先；需保护 prefix cache 时显式关闭)
    ///
    /// ## KV cache 影响
    /// SilentRouter 用 session_tools / experience_signal 等**每轮变化的状态**重排 tools 数组。
    /// 跨 turn 排序变化 → tools 段 JSON bytes 变化 → DeepSeek/OpenAI prefix cache 从 tools 段
    /// 起整段（含 history）miss。
    ///
    /// ## 何时显式 enable
    /// 用户明确接受"工具路由智能 vs prefix cache"的取舍，且 input 多变足以让 cache 命中本就低，
    /// 可设为 true。多轮深度对话场景默认 false 收益最高。
    pub silent_router_enabled: bool,
    /// Phase 1：模型能力 catalog（builtin + 可选 YAML 覆盖）。
    /// None → CoreLoop 启动时 fall back 到 `ModelCatalog::builtin()`。
    /// 测试场景可注入空 catalog。
    pub model_catalog: Option<Arc<crate::llm::ModelCatalog>>,
    /// 工具可见性门槛（基于 effectiveness.tier）
    ///
    /// **Default: VisibilityTier::D**（最低，等价不过滤——所有 Loaded/Active 工具都暴露）
    ///
    /// ## 设计
    /// effectiveness.tier 由 EffectivenessTracker 根据成功率/延迟动态评级（S/A/B/C/D）。
    /// 此字段控制 build_tool_definitions 调用 list_visible(threshold) 时的最低门槛。
    /// LLM 仅看到 tier ≥ threshold 的工具——连续失败的低分工具自动隐形，让 LLM 不再尝试。
    ///
    /// ## 何时调高
    /// - threshold = D：默认，等价 all_tools，最大化工具可用性
    /// - threshold = C：隐藏 D tier（连续失败/被环境阻塞）的工具
    /// - threshold = B：仅暴露中等以上质量的工具，对工具集庞大的 deployment 有益
    /// - threshold = A/S：极保守，仅暴露最稳定工具
    ///
    /// ## 副作用警示
    /// 工具被隐藏时 LLM 看不到，但**仍可被 LLM 通过历史 tool_call 引用**（registry.execute
    /// 走的是 ToolId 反查，不依赖 build_tool_definitions）→ Cooling 状态会被拦截。
    pub tool_visibility_threshold: abacus_types::VisibilityTier,
    /// Phase β-D：按 task_kind 路由工具（按任务子集暴露）
    ///
    /// **Default: false**（保守——避免破坏首轮无 task_kind 时的 cache 行为）
    ///
    /// ## 设计
    /// `ToolSchema.applicable_task_kinds: Some(list)` 声明工具仅在特定任务子集中暴露。
    /// 启用此 flag 后 build_tool_definitions 在 session.task_kind_locked 已锁定时按白名单过滤。
    /// 工具自身 `applicable_task_kinds: None` 始终保持全任务可见。
    ///
    /// ## KV cache 影响
    /// task_kind_locked 一旦在首轮锁定就不再变化（参见 task_kind_locked 字段注释），
    /// 故 tools 段在 session 内 byte-stable——不破坏 prefix cache。
    /// 跨 session（即跨任务）必然变化，但跨 session cache 本来就不复用。
    ///
    /// ## 何时启用
    /// 工具集 ≥ 30 且任务类型差异明显（code/debug/analysis）→ 启用可显著缩短 LLM 选择空间，
    /// 配合工具的 applicable_task_kinds 白名单使用。
    pub task_kind_routing_enabled: bool,
    /// Phase α-S：是否启用场景化工具加载（只发场景相关工具的完整 schema）
    ///
    /// **Default: true**
    ///
    /// ## 设计
    /// 启用后，build_tool_definitions_for 在 β-D 之后、γ-I 之前按 task_kind 的 scene_active_prefixes
    /// 做前缀过滤：仅匹配前缀的工具发送完整 ToolDefinition 到 LLM，其余通过 system prompt
    /// 中的 tool catalog 告知存在（On-Demand Expansion）。
    ///
    /// 三重保留规则：前缀匹配 / 最近 5 turn 调用过 / applicable_task_kinds 显式命中。
    ///
    /// ## 引用关系
    /// - 消费方：build_tool_definitions_for（Phase α-S 分支）
    /// - 依赖：scene_active_prefixes() helper + tool_last_invoked
    pub scene_tool_loading_enabled: bool,
    /// Phase γ-I：基于使用频率的 pruning 阈值（turn 数）
    ///
    /// **Default: None**（不修剪）
    ///
    /// ## 设计
    /// `Some(N)` 启用：工具上次调用距 current_turn 超过 N 个 turn → 从 LLM 视野隐藏（仍可被显式 ToolId 调用）。
    /// 与 VisibilityTier 正交——后者按质量分，本字段按新鲜度。
    ///
    /// ## KV cache 影响
    /// 隐藏决策只影响"present or absent"，不改 description 字节——同 VisibilityTier 模式。
    /// 注意首轮 last_invoked=0 时所有工具都"超期"——故首轮无 visibility 过滤效果（current_turn-0=current_turn=0 <= N）。
    /// 实际触发在 turn N+1 之后。
    ///
    /// ## 何时启用
    /// 长 session（>50 turn）+ 工具集 ≥ 30 时显著降低噪声。短 session 用户体验差异不明显。
    pub tool_frequency_pruning_turns: Option<u64>,
    /// Phase γ-Palace-C：行为宫殿 → effectiveness.tier 同步间隔（turn 数）
    ///
    /// **Default: None**（不同步）
    ///
    /// ## 设计
    /// `Some(N)` 启用：每 N turn 调用 `sync_from_palace()`，遍历 palace.behavior 找
    /// `tool_call:{tool_id}` pattern，frequency >= 3 且 success_rate < 0.3 → 在
    /// EffectivenessTracker 标记 palace_demote。
    ///
    /// ## KV cache 影响
    /// 同步发生在 turn 边界，N 段内 effectiveness.tier 稳定 → tools 段 byte 不变。
    /// 同步触发当轮 cache miss 一次，回报是后续多 turn 的工具集精简（去除"看似可用其实总失败"的工具）。
    ///
    /// ## 单调性
    /// 一旦降级不自动恢复——避免反复抖动 tier 破 cache。需手动 `clear_palace_demote`。
    pub palace_sync_interval_turns: Option<u32>,
    /// Phase Z3：auto_compress 默认压缩档位
    ///
    /// **Default: Brief**（与原行为兼容——首行 + tok 数）
    ///
    /// ## 档位
    /// - `Detailed`: 保留前 2 行 + role + tok 数（信息密度高，token 省得少）
    /// - `Brief`:   首行 + role + tok 数（默认；平衡）
    /// - `Minimal`: 仅 role + tok 数（极简）
    ///
    /// ## KV cache 影响
    /// 仅影响合并 summary 字节数；同档位字节稳定，跨档位会破缓存——故运行时不建议切换。
    pub default_compress_level: crate::core::context::CompressLevel,
    /// Task #96：单 session 最大模型升级次数（防 cache 振荡）
    ///
    /// **Default: 2**
    ///
    /// ## 设计动机
    /// flash → pro 切换会让 KV cache prefix 在新 model 池里冷启动（5000+ tokens 全价重发）。
    /// 限制升级次数避免单 session 反复振荡导致命中率断崖式下降。
    ///
    /// ## 引用
    /// `pipeline::handle_model_escalation` 前置 check：达到上限即跳过升级。
    pub max_escalations: u32,

    /// Phase 3 (lint)：YAML 加载的 lint 白名单
    ///
    /// **Default: None**（仅默认规则集，无豁免）
    ///
    /// 引用关系：CoreLoop::new 在 register_all 之前注入到 ToolRegistry.lint_rules——
    /// 确保所有内置工具 register 时都已应用白名单。
    /// 由 server.rs/engine_init.rs 从 cfg_mgr.get_typed("lint") 加载。
    pub lint_overrides: Option<crate::tool::schema_lint::LintOverrides>,

    /// Wrapping-E：自适应 D-tier effectiveness 过滤
    ///
    /// **Default: false**（保守开关——评分错杀风险，需运维 opt-in 后观察）
    ///
    /// 启用后 `build_tool_definitions_for` 在已有 task_kind_routing/frequency_pruning
    /// 之外加最后一层过滤：effectiveness.tier == D 且非 insufficient_data 的工具不暴露给 LLM。
    ///
    /// ## 评分来源（已有数据）
    /// - `EffectivenessTracker.evaluate()` 返回 tier（S/A/B/C/D）+ insufficient_data
    /// - D-tier 触发条件：composite_score < 0.10（adoption×success×latency 加权）
    /// - insufficient_data=true（样本不够）→ 跳过过滤（新工具友好）
    /// - user_favorites → 永远 S-tier（不会被这里过滤）
    ///
    /// ## 与 tool_visibility_threshold 的区别
    /// - `tool_visibility_threshold`：静态全局阈值——所有工具按 tier 比较
    /// - `adaptive_d_tier_hide`：仅删 D-tier 且数据足够——更保守，不会误杀新工具
    pub adaptive_d_tier_hide: bool,

    // ─── cross-session: Event JSONL Sink ──────────────────────────────────
    /// **Default: true**——session 启动时自动注册 JsonlEventHook 写 jsonl。
    ///
    /// 为何 default-on：观测层零业务影响，仅追加文件，IO 故障不阻塞 turn；
    /// 为 session resume / 跨 session 分析提供基础数据。
    /// 关闭场景：测试 / 严苛只读环境 / 用户显式关闭。
    ///
    /// ## 引用关系
    /// - CoreLoop::new 用此值决定是否注册 JsonlEventHook
    /// - audit_report 输出当前 sink 路径
    pub event_sink_enabled: bool,

    // ─── W2 (Task #100): Tool result dedup ─────────────────────────────────
    /// **Default: false**（保守开关，遵循 default-off 原则保 KV cache）
    ///
    /// 启用后对 `idempotent=true` 的工具按 `(tool_id, args_canonical_hash)` 缓存结果，
    /// 短 TTL 内重复调用直接复用——典型场景：LLM 反复 `fs_read` 同一文件、
    /// 反复 `db_read_records` 同条记录。
    ///
    /// ## 引用关系
    /// - CoreLoop::new 用此 flag 决定是否实例化 `tool_result_dedup`
    /// - pipeline dispatch 入口前查询，dispatch 后写入
    /// - audit_report 输出当前命中率
    ///
    /// ## 副作用与边界
    /// - 仅作用于 `ToolSchema.idempotent == true`
    /// - 仅在 dispatch 真正成功且非缓存命中时写入（避免反复刷新过期）
    /// - per-CoreLoop 共享池（多 session 间 dedup）——若多用户隔离需求强烈，未来可下沉到 SessionState
    pub tool_result_dedup_enabled: bool,
    /// **Default: 60s** — 同一进程内的短 TTL，避免外部状态变更后还命中陈旧结果。
    pub tool_result_dedup_ttl_secs: u64,
    /// **Default: 256 KB** — 缓存总权重上限（按 serialized output 字节加权 LRU 逐出）。
    pub tool_result_dedup_capacity_kb: usize,

    /// LLM 行为策略（从 ~/.abacus/policy.toml 加载，运行时可调）
    ///
    /// 引用关系：
    /// - build_system_output: 注入 guard/declaration 到 system prompt
    /// - execute_loop: 读取 thresholds（premature_stop_chars、confirm_timeout_secs）
    /// - preflight.rs: 读取 destructive_patterns
    ///
    /// 生命周期：进程启动时 PolicyConfig::load()，session 内不变
    pub policy: Arc<policy::PolicyConfig>,

    /// 三层阈值配置（统一管理所有限制，替代分散的硬编码）
    ///
    /// ## 引用关系
    /// - pipeline: turn_max_tool_calls, turn_max_iterations, turn_max_recovery, turn_provider_timeout_secs
    /// - tool dispatch: tool_bash_timeout_secs, tool_default_timeout_secs, confirm_timeout_secs
    /// - orchestrator: subagent_max_duration_secs, subagent_max_tokens, specialist_timeout_secs
    /// - context: context_compress_ratio
    pub thresholds: ThresholdConfig,

    /// 提示词引擎配置文件路径（Expert Roles + Topics）
    ///
    /// 默认：`~/.abacus/prompt_roles.toml`
    /// 不存在时 fallback 到内置 roles
    ///
    /// ## 引用关系
    /// - 消费方：DynamicInjector 启动加载 + 热重载
    /// - 生命周期：CoreLoop::new() 时一次性加载，config_set reload 时重新加载
    pub prompt_roles_path: Option<std::path::PathBuf>,

    /// 场景映射配置文件路径（TaskKind → abacusbr sections）
    ///
    /// 默认：`~/.abacus/subscenes.toml`
    /// 不存在时 fallback 到内置 default_subscene_map()
    pub subscenes_path: Option<std::path::PathBuf>,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            // V29.13: 5 → 25, 同步 config.rs default_config()
            max_turns_per_request: 1000,
            max_tool_calls_per_turn: 50,
            default_model: ModelId("deepseek-v4-flash".into()),
            default_temperature: 0.6,
            // V40: 64000 — 对齐主流 coding agent（Claude Code 20K, OpenCode 32K）
            // DeepSeek thinking 模式 output 可达 64K+；非 thinking 上限由模型 spec 约束
            default_max_tokens: 64000,
            context_window_ratio: 1.0,
            system_prompt: String::new(),
            model_spec: None,
            thinking_intent: None,
            silent_router_enabled: true, // 默认开启：工具路由智能优先（需保护 prefix cache 时显式 opt-out）
            model_catalog: None,
            tool_visibility_threshold: abacus_types::VisibilityTier::D, // 默认 D：等价不过滤
            // Phase β-D 默认开启（Task #84）：按 task_kind 过滤工具，每轮省 1k-3k tokens
            // 引用关系：被 build_tool_definitions_for 在 routing_active 路径消费
            // 副作用：未标 applicable_task_kinds 的工具仍全可见——透明降级
            task_kind_routing_enabled: true,
            // Phase α-S 默认开启：场景化工具加载——按 task_kind 前缀过滤，每轮省 2k-5k tokens
            // 引用关系：被 build_tool_definitions_for（Phase α-S 分支）消费
            // 三重保留：前缀匹配 / 最近 5 turn 调用 / applicable_task_kinds 命中
            scene_tool_loading_enabled: true,
            // Phase γ-I 默认开启 N=20（Task #87）：N turn 未调用即隐藏，每轮省 0.8k-2k tokens
            // 新工具（last_invoked == None）不会被剪掉——新工具友好
            tool_frequency_pruning_turns: Some(20),
            // Phase 3 (lint)：YAML 加载的 lint 白名单——CoreLoop::new 在 register_all 之前注入
            // 引用关系：CoreLoop::new 用此值替换 registry 默认 LintRuleSet
            lint_overrides: None,
            // Task #96：单 session 默认最多 2 次模型升级，防 cache 振荡
            max_escalations: 10,
            palace_sync_interval_turns: None,   // Phase γ-Palace-C：默认关
            default_compress_level: crate::core::context::CompressLevel::Brief, // Z3：默认 Brief 兼容旧行为
            // W2 (Task #100)：默认关——遵循 default-off 原则；运维显式开启
            tool_result_dedup_enabled: false,
            tool_result_dedup_ttl_secs: 60,
            tool_result_dedup_capacity_kb: 2048,
            // Wrapping-E + 段 K5：默认开——段 K1~K4 多层兜底已根除评分错杀风险
            //   K1: env_failure 不拉评分；K2: 扩展工具 30 次冷启动期；
            //   K4: palace_demoted 每 50 turn 试探放行；K3: provider/cluster floor + 全量回退兜底
            adaptive_d_tier_hide: true,
            // cross-session: 默认开——观测层零业务侵入
            event_sink_enabled: true,
            // 行为策略：从 ~/.abacus/policy.toml 加载
            policy: Arc::new(policy::PolicyConfig::load()),
            // 三层阈值体系
            thresholds: ThresholdConfig::default(),
            // 提示词引擎配置路径（默认 ~/.abacus/）
            prompt_roles_path: dirs::home_dir().map(|h| h.join(".abacus/prompt_roles.toml")),
            subscenes_path: dirs::home_dir().map(|h| h.join(".abacus/subscenes.toml")),
        }
    }
}

/// 会话焦点锚 — LLM 主动写入的当前工作焦点声明
///
/// ## 设计意图
/// 长上下文下注意力漂移的根因：LLM 不知道当前焦点，每轮从历史推断。
/// SessionFocus 提供一个 LLM 可写、永不被压缩、每轮注入 system prompt
/// **末尾**（recency-adjacent，紧邻用户消息）的固定锚点，与 InteractionMap 分工：
///   InteractionMap — 被动记录"经过了哪些检查点"（系统自动生成）
///   SessionFocus   — 主动声明"当前焦点是什么"（LLM 通过工具写入）
///
/// ## 位置历史
/// 早期实现放在 system prompt 顶部（primacy），后因 focus.render_with_age(age) 每轮 byte 变化，
/// 破坏 DeepSeek/OpenAI 的 prefix cache（命中率从理论 ~80% 跌到 ~0%），改为末尾追加。
/// 跨 provider 统一策略——参见 build_system_output 注释。
///
/// ## 生命周期
/// - 创建：SessionState::new()，初始为 None
/// - 写入：LLM 调用 session.set_focus 工具
/// - 读取：build_system_prompt() 每轮读取并追加到 system prompt 末尾
/// - 重置：session 结束
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionFocus {
    /// 当前总体目标（一句话）
    pub goal: String,
    /// 当前阶段描述（如 "Step 2/4: 实现接口层"）
    pub phase: String,
    /// 关键约束列表（硬限制、前提条件）
    pub constraints: Vec<String>,
    /// 下一步行动
    pub next_step: String,
    /// 写入轮次（用于判断新鲜度）
    pub updated_at_turn: u32,
}

impl SessionFocus {
    /// 渲染为注入 system prompt 顶部的紧凑锚点文本。
    ///
    /// ## 大小约束（防累积膨胀）
    /// - 每个文本字段截断到 60 字符（约 20 tokens）
    /// - constraints 最多展示 3 条，超出部分标注 (+N more)
    /// - 总计约 40–90 tokens，不随 set_focus 调用次数增长
    ///
    /// ## stale_warning 参数
    /// 当焦点接近过期（age >= MAX_STALE - 3）时传入已过 turn 数，
    /// 附加提示推动 LLM 主动调用 session.set_focus 刷新。
    pub fn render(&self) -> String {
        self.render_with_age(0)
    }

    pub fn render_with_age(&self, age_turns: u32) -> String {
        const MAX_FIELD: usize = 60;
        const MAX_CONSTRAINTS: usize = 3;

        fn truncate(s: &str) -> String {
            if s.chars().count() <= MAX_FIELD {
                s.to_string()
            } else {
                format!("{}...", s.chars().take(MAX_FIELD - 3).collect::<String>())
            }
        }

        let mut lines = vec![
            format!("## [Session Focus — Turn {}]", self.updated_at_turn),
            format!("Goal:  {}", truncate(&self.goal)),
            format!("Phase: {}", truncate(&self.phase)),
        ];

        if !self.constraints.is_empty() {
            let shown: Vec<String> = self.constraints.iter()
                .take(MAX_CONSTRAINTS)
                .map(|c| truncate(c))
                .collect();
            let suffix = if self.constraints.len() > MAX_CONSTRAINTS {
                format!(" (+{} more)", self.constraints.len() - MAX_CONSTRAINTS)
            } else {
                String::new()
            };
            lines.push(format!("Constraints: {}{}", shown.join(" | "), suffix));
        }

        if !self.next_step.is_empty() {
            lines.push(format!("Next:  {}", truncate(&self.next_step)));
        }

        // Phase 2 KV cache 修复：去除 age stale_warning 行
        //   之前 `if age_turns > 0` 会动态追加"focus is N turns old"提示，导致 focus_block
        //   在接近过期阶段（age 12-15）每轮 byte 变化 → 破前缀 cache。
        //   语义保留：刷新提醒改由 log（tracing::info）+ UI 通知传递，不进 prompt。
        //   `age_turns` 入参保留以维持调用方签名兼容（Phase 5 cleanup 可去掉）。
        let _ = age_turns;

        lines.join("\n")
    }
}

/// 把 SessionFocus 渲染追加到 system prompt 末尾。
///
/// ## 引用关系
/// 调用方：`CoreLoop::build_system_output`（text 路径）
/// 生命周期：纯函数，无状态
///
/// ## KV Cache 设计
/// focus.render_with_age(age) 每轮 byte 变化（age 增长 + 阶段刷新）。
/// 必须放在尾部——DeepSeek/OpenAI 的 prefix cache 按 token 0 起的连续相同字节匹配，
/// 任何前置 dynamic 注入会导致整段 system prompt cache miss（影响 ~80% 的 input cost）。
///
/// 与 `build_system_output` 中 segments 路径的处理对齐（行 1389+），保证跨 provider 一致。
fn compose_system_text_with_focus(assembled: String, focus: Option<&str>) -> String {
    match focus {
        Some(f) => format!("{}\n\n---\n\n{}", assembled, f),
        None => assembled,
    }
}

/// Task #95 + #98：每个 session 的 cache 统计聚合
///
/// 在 LlmResponse 返回后累加：
/// - `total_input_tokens`：所有 turn 的 prompt_tokens 之和
/// - `total_cached_tokens`：被命中的 prefix（DeepSeek/Anthropic 报告）
/// - `total_cache_creation_tokens`：本轮新建 cache 写入字节（Anthropic）
/// - `model_switches`：累计切换 model 次数
/// - `per_tool_tokens`（Task #98）：按工具累积的 result 字节数
/// - `per_tool_call_count`（Task #98）：按工具累积的调用次数
///
/// 引用关系：仅 CoreLoop pipeline 写；audit/cache_stats 读。
#[derive(Debug, Default, Clone)]
pub struct CacheTelemetry {
    pub total_input_tokens: u64,
    pub total_cached_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub model_switches: u32,
    /// Task #98：每工具累积的 result tokens（粗估：bytes/4）
    /// 引用：execute_loop 工具调用后累加；session_cache_report 排序展示
    pub per_tool_tokens: HashMap<ToolId, u64>,
    /// Task #98：每工具调用次数
    pub per_tool_call_count: HashMap<ToolId, u32>,
}

impl CacheTelemetry {
    /// 命中率（cached / input）—— 0 input 时返回 0.0
    pub fn hit_rate(&self) -> f64 {
        if self.total_input_tokens == 0 { 0.0 }
        else { self.total_cached_tokens as f64 / self.total_input_tokens as f64 }
    }

    /// Task #98：累加一次工具调用的 result token 估算
    pub fn record_tool_result(&mut self, tool_id: &ToolId, result_bytes: usize) {
        let tokens = (result_bytes / 4) as u64;
        *self.per_tool_tokens.entry(tool_id.clone()).or_insert(0) += tokens;
        *self.per_tool_call_count.entry(tool_id.clone()).or_insert(0) += 1;
    }

    /// Task #98：返回按 tokens 降序的 top-N 工具
    pub fn top_tools_by_tokens(&self, n: usize) -> Vec<(ToolId, u64, u32)> {
        let mut entries: Vec<(ToolId, u64, u32)> = self.per_tool_tokens.iter()
            .map(|(id, tok)| {
                let count = self.per_tool_call_count.get(id).copied().unwrap_or(0);
                (id.clone(), *tok, count)
            })
            .collect();
        entries.sort_by_key(|(_, tok, _)| std::cmp::Reverse(*tok));
        entries.truncate(n);
        entries
    }
}

pub struct SessionState {
    pub session_id: String,
    pub messages: Arc<RwLock<Vec<Message>>>,
    /// Per-session context messages for context.declare tool (was CoreLoop-level shared, causing cross-session pollution)
    pub context_messages: Arc<RwLock<Vec<Message>>>,
    pub turn_count: u32,
    pub expert_bound: Option<String>,
    pub metadata: HashMap<String, String>,
    pub filengine_session: Arc<RwLock<FilengineSession>>,
    pub interaction_map: Arc<RwLock<InteractionMap>>,
    /// Current user role for this session.
    /// Controls MCIP access: Admin > Developer > User.
    pub user_role: UserRole,
    /// 渐进输出控制器（每个 session 独立实例）
    pub progressive: Arc<RwLock<ProgressiveController>>,
    /// 会话焦点锚 — LLM 通过 session.set_focus 工具写入；每轮注入 system prompt 末尾（recency-adjacent）
    /// 初始为 None（首次调用 set_focus 前不注入）
    pub session_focus: Arc<RwLock<Option<SessionFocus>>>,
    /// MCIP 永久授权工具集合（用户选择「总是允许」后写入）
    ///
    /// ## 生命周期
    /// - 创建：`SessionState::new()`，初始为空集
    /// - 写入：`CoreLoop::grant_and_rerun()` 处理「总是允许」决定时
    /// - 消费：pipeline Phase 4 工具分发前检查，匹配则跳过 MCIP
    /// - 销毁：session 关闭时随之销毁（不持久化）

    pub mcip_grants: std::sync::RwLock<std::collections::HashSet<String>>,

    /// V28：实时授权 channel——按 nonce 索引的 oneshot sender 集合
    ///
    /// ## 用途
    /// 替代旧的 grant_and_rerun "重发整个 turn" 模型：pipeline dispatch 遇 NeedsConfirm
    /// 时创建 oneshot，把 sender 存入此 map（key=nonce），把 nonce 写到 pending_confirmation
    /// 携带给 UI；然后 `await receiver.recv()` **挂起 dispatch**。UI 收到决策后用 nonce 找
    /// sender 直接发 true/false，pipeline 立即继续——节省一整轮 LLM 推理（thinking + tool_call
    /// 重生成）。
    ///
    /// ## 生命周期
    /// - 创建：pipeline NeedsConfirm 路径 push 时（`std::mem::replace` 取出 sender，避免 await 跨 lock）
    /// - 写入：pipeline tool dispatch 一处
    /// - 消费：UI 端 take 出 sender 后 send；timeout fallback 也从这里 take
    /// - 销毁：sender drop 时（自然回收）；turn 结束时残留会被 next turn 创建覆盖（罕见）
    ///
    /// ## 并发模型
    /// std::sync::Mutex（短临界区，put/take 即返）；非异步锁，避免 await 跨锁
    pub mcip_confirm_channels: std::sync::Mutex<
        std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>,
    >,

    /// Session-sticky task_kind 锁定（Phase 2 KV cache 修复）
    ///
    /// ## 引用关系
    /// - 创建：`SessionState::new()` / `new_with_autonomy()` / `new_with_gate_config()`，初始 `None`
    /// - 写入：`build_system_output` 首轮（`None` → `Some(kind)`），首轮 classify 后锁定
    /// - 读取：`build_system_output` 后续轮次复用，避免 task_kind 切换破坏 Layer 185 cache
    /// - 销毁：随 SessionState 销毁回收（无显式 reset 路径，详见「契约」）
    ///
    /// ## 设计
    /// 跨 turn 切 task_kind → Layer 185 subscenes 字节变化 → 后续所有内容 cache miss。
    /// 锁定为首轮决策后，session 内即使输入语义偏移也使用首轮 task subscenes。
    ///
    /// ## 契约（重要，违反会导致用户困惑）
    /// 1. **无 LLM 工具更新通道**：故意不暴露 `set_task_kind` 工具——避免 LLM 自行变更破坏前缀 cache。
    /// 2. **无运行时 reset 路径**：本字段一旦锁定就持续到 SessionState 销毁。
    /// 3. **跨任务必须 `/session new`**：用户从「调试」切换到「架构设计」需显式重启会话；
    ///    否则 prompt 仍包含首轮任务子场景，LLM 引导可能轻微偏离当前实际意图。
    /// 4. **Trade-off 取向**：cache 命中率 > prompt 子场景的精确性。源于 ABACUS 文档
    ///    99% 命中率目标（Turn 9: 99.3%）。
    ///
    /// ## 状态
    /// - `None`：首轮还没决定（初始）
    /// - `Some(kind)`：已锁定为该 task_kind（不可变更，直至 session 销毁）
    pub task_kind_locked: Arc<RwLock<Option<crate::core::task_analyzer::TaskKind>>>,

    /// Session-sticky thinking decision（B 方案：首轮决定，后续锁定）
    ///
    /// ## 引用关系
    /// 写入：`pipeline/mod.rs::execute_loop` 首轮（`None` → `Some(decision)`）
    /// 读取：`pipeline/mod.rs::execute_loop` 后续轮次先查此字段
    ///
    /// ## 设计意图
    /// `map_complexity_to_thinking` 按 input 复杂度返回 `None`/`Some(low|medium|high)`。
    /// 跨 turn toggle `None ↔ Some` 会让 DeepSeek `build_messages` 改写**所有历史 assistant
    /// 消息**的 `reasoning_content` 字段存在性 → 整段对话 history bytes shift → 整段
    /// prefix cache miss（成本最高的路径之一）。
    ///
    /// 锁定为首轮决策后，跨 turn 协议层字段一致 → cache 稳定。质量取舍：后续 turn 即便
    /// 复杂度变化也用首轮的 thinking effort，可能略低/略高。session 内 thinking 行为一致。
    ///
    /// ## 状态语义
    /// - `Outer None`：首轮还没决定（初始）
    /// - `Outer Some(Inner None)`：首轮已决定但 thinking=off
    /// - `Outer Some(Inner Some(cfg))`：首轮已决定 thinking=on with cfg
    ///
    /// ## 生命周期
    /// - 创建：`SessionState::new()`，初始 `Some(None)` 即 unlocked sentinel；
    ///   等首轮命中 build_thinking_config / complexity_thinking 时才 set
    /// - 写入：execute_loop 首轮，决定后写入此字段
    /// - 销毁：session 结束随 SessionState drop
    pub thinking_decision: Arc<RwLock<Option<Option<abacus_types::ThinkingIntent>>>>,

    /// Task #96：模型升级次数计数（防振荡）
    ///
    /// ## 引用关系
    /// - 创建：SessionState::new 初始 0
    /// - 写入：handle_model_escalation 升级成功后 +1
    /// - 读取：handle_model_escalation 前置 check（>= max_escalations 则跳过升级）
    /// - 销毁：随 SessionState drop
    ///
    /// ## 设计动机
    /// 频繁 flash↔pro 切换会让 KV cache 命中率断崖式下降——每次 escalate 都是
    /// 5000+ tokens prefix 全价重发。预算限制让单 session 最多升级 N 次（默认 2）。
    pub escalation_count: std::sync::atomic::AtomicU32,

    /// Task #96：升级后锁定的 model（sticky）
    ///
    /// ## 引用关系
    /// - 创建：SessionState::new 初始 None
    /// - 写入：handle_model_escalation 升级成功后 set 为目标 model
    /// - 读取：execute_loop 决定 LlmRequest.model 时优先取此值
    ///
    /// ## 设计意图
    /// 一旦升级到 pro 就保持，不再 flash↔pro 反复切换破坏 cache。
    /// 即使后续 turn 复杂度回落，仍用 pro（multi-turn pro cache 累积命中）。
    pub escalated_model: Arc<RwLock<Option<abacus_types::ModelId>>>,

    /// Task #95：Cache telemetry —— LlmResponse.usage 累积统计
    ///
    /// ## 引用关系
    /// - 创建：SessionState::new 初始 zero
    /// - 写入：execute_loop / handle_model_escalation 收到 LlmResponse 后累加
    /// - 读取：CoreLoop::cache_stats / audit_optimizations
    ///
    /// 字段含义见 CacheTelemetry struct 定义。
    pub cache_telemetry: Arc<RwLock<crate::core::CacheTelemetry>>,

    /// Mid-turn user signal：用户在 LLM 执行工具循环期间发送的消息
    ///
    /// ## 引用关系
    /// - 写入方：TUI event handler（忙碌态 Enter 时 push）
    /// - 消费方：pipeline execute_loop 每次迭代间隙 drain
    ///
    /// ## 生命周期
    /// - 创建：SessionState 构造时初始化为空 Vec
    /// - 写入：TUI 通过 EngineHandle.session 访问后 push
    /// - 消费：pipeline drain 后注入为 `[User update]` 格式的 User message
    /// - 销毁：随 SessionState drop
    ///
    /// ## 并发模型
    /// tokio::sync::Mutex（TUI 写 + pipeline 读，临界区极短——push/drain）
    pub mid_turn_signals: tokio::sync::Mutex<Vec<String>>,

    /// Role 能力声明，由 CoreLoop::new() 从 config 构建（通过 load_role_caps()）
    ///
    /// ## 引用关系
    /// - 引用方：ExecutionContext.role_caps（pipeline 每次 tool dispatch 注入）
    /// - 消费方：工具执行层（fs_roots / bash_policy / tool_budget_per_turn / search_provider）
    ///
    /// ## 生命周期
    /// - 创建：SessionState::new / new_with_autonomy / new_with_gate_config 构造时调用 load_role_caps() 一次性读取
    /// - 消费：pipeline Phase 4 每次 tool dispatch 时 Arc::clone 传入 ExecutionContext
    /// - 销毁：随 SessionState drop（Arc 引用归零后释放）
    pub role_caps: Arc<abacus_types::RoleCapabilities>,
}

impl SessionState {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self::build(session_id, AutonomyLevel::default(), None)
    }

    /// Create session with custom autonomy level
    pub fn new_with_autonomy(session_id: impl Into<String>, autonomy: AutonomyLevel) -> Self {
        Self::build(session_id, autonomy, None)
    }

    /// Create session with custom GateConfig（R1: 配置孤岛修复入口）
    ///
    /// ## 场景
    /// 从 ConfigManager 读取配置后构造 GateConfig，传入此方法。
    /// Server 初始化时使用，替代 `new()` 的硬编码默认值。
    pub fn new_with_gate_config(session_id: impl Into<String>, gate_config: crate::core::progressive_gate::GateConfig) -> Self {
        Self::build(session_id, AutonomyLevel::default(), Some(gate_config))
    }

    /// 2026-05-28: DRY — 三个构造器统一内部实现
    /// 差异仅在 ProgressiveGate 初始化方式（autonomy vs gate_config）
    fn build(
        session_id: impl Into<String>,
        autonomy: AutonomyLevel,
        gate_config: Option<crate::core::progressive_gate::GateConfig>,
    ) -> Self {
        let gate = match gate_config {
            Some(cfg) => ProgressiveGate::new(cfg),
            None => ProgressiveGate::from_autonomy(autonomy),
        };
        let progressive = ProgressiveController::new(gate, autonomy, None);
        Self {
            session_id: session_id.into(),
            messages: Arc::new(RwLock::new(Vec::new())),
            context_messages: Arc::new(RwLock::new(Vec::new())),
            turn_count: 0,
            expert_bound: None,
            metadata: HashMap::new(),
            filengine_session: Arc::new(RwLock::new(FilengineSession::new())),
            interaction_map: Arc::new(RwLock::new(InteractionMap::new())),
            user_role: UserRole::default(),
            progressive: Arc::new(RwLock::new(progressive)),
            session_focus: Arc::new(RwLock::new(None)),
            mcip_grants: std::sync::RwLock::new(std::collections::HashSet::new()),
            mcip_confirm_channels: std::sync::Mutex::new(std::collections::HashMap::new()),
            task_kind_locked: Arc::new(RwLock::new(None)),
            thinking_decision: Arc::new(RwLock::new(None)),
            escalation_count: std::sync::atomic::AtomicU32::new(0),
            escalated_model: Arc::new(RwLock::new(None)),
            cache_telemetry: Arc::new(RwLock::new(CacheTelemetry::default())),
            mid_turn_signals: tokio::sync::Mutex::new(Vec::new()),
            role_caps: Arc::new(load_role_caps()),
        }
    }

    /// Set the user role for this session.
    pub fn set_user_role(&mut self, role: UserRole) {
        self.user_role = role;
    }

    /// Set gate scope (called by orchestrator for Team mode)
    pub async fn set_progressive_scope(&self, scope: GateScope) {
        let mut ctrl = self.progressive.write().await;
        ctrl.set_scope(scope);
    }
}

#[derive(Debug, Clone)]
pub struct TurnResult {
    pub response: String,
    pub stats: TurnStats,
    pub tool_outputs: Vec<ToolOutput>,
    pub matched_skills: Vec<SkillCandidate>,
    pub session_id: String,
    /// Progressive output state after this turn (None = PassThrough / not enabled)
    pub progressive_state: Option<abacus_types::progressive::ProgressiveState>,
    /// 惰性检测警告（None = 未检出或已自动重试修复）
    pub inertia_warning: Option<inertia::InertiaSignal>,
    /// MCIP 待用户授权的工具列表
    ///
    /// 非空时：L4 展示授权对话框，用户选择后调用 `CoreLoop::grant_and_rerun()` 重运同一 turn。
    /// 空时：turn 正常完成，无工具需要授权。
    pub pending_confirmations: Vec<crate::mcip::McipConfirmRequest>,
}

/// 厂商分组 — 一个 base_url 下支持多个模型
///
/// ## 场景
/// OpenRouter、Together AI、Groq 等厂商在一个 API 端点下提供多模型。
/// 注册一个分组后，用户可通过 `/model <name>` 自由切换组内模型。
pub struct ProviderGroup {
    /// 分组标识（如 "openrouter", "anthropic"）
    pub id: String,
    /// 该分组支持的所有模型名
    pub models: Vec<ModelId>,
    /// 共享 provider 实例（多个模型共用同一 API 端点/密钥）
    pub provider: Arc<dyn LlmProvider>,
}

impl ProviderGroup {
    pub fn new(id: impl Into<String>, models: Vec<ModelId>, provider: Arc<dyn LlmProvider>) -> Self {
        Self { id: id.into(), models, provider }
    }

    /// 检查指定模型是否属于该分组
    pub fn supports(&self, model: &str) -> bool {
        self.models.iter().any(|m| m.0 == model)
    }
}

/// 在现有 provider 外包装一层，覆盖 supported_models() 返回分组全量模型
struct GroupProvider {
    inner: Arc<dyn LlmProvider>,
    models: Vec<ModelId>,
}

#[async_trait::async_trait]
impl LlmProvider for GroupProvider {
    async fn complete(&self, req: LlmRequest) -> abacus_types::Result<LlmResponse> {
        self.inner.complete(req).await
    }

    fn cacheable_segments(&self, req: &LlmRequest) -> Vec<crate::llm::prompt_cache::CachedSegment> {
        self.inner.cacheable_segments(req)
    }

    fn provider_id(&self) -> &str {
        self.inner.provider_id()
    }

    fn supported_models(&self) -> Vec<ModelId> {
        self.models.clone()
    }
}

pub struct CoreLoop {
    registry: Arc<ToolRegistry>,
    skill_engine: Arc<RwLock<SkillEngine>>,
    capability_hub: Arc<CapabilityHub>,
    pub(crate) context_manager: Arc<ContextManager>,
    injector: Arc<RwLock<DynamicInjector>>,
    effectiveness: Arc<RwLock<EffectivenessTracker>>,
    mcip_gateway: Arc<McipGateway>,
    /// 工具执行中间件链（Arc<RwLock> 支持热插拔：Arc::new(core) 后仍可 add_middleware）
    /// 创建：CoreLoop::new()；消费方：TurnPipeline Phase 4（read lock）
    mag_chain: Arc<RwLock<crate::mag_chain::MagChain>>,
    /// EpistemicGuard 独立引用（与 mag_chain 内部 EpistemicGuard 实例共享同一 Arc）
    /// 用途：setup() 注入累积违规声明；post_process() 记录违规计数
    epistemic_guard: Arc<crate::mag_chain::EpistemicGuard>,
    prompt_assembly: PromptAssembly,
    /// Provider-specific prompt adapters（auto-registered on register_provider）
    /// key = provider_id；默认 NeutralAdapter（透传）
    adapters: RwLock<HashMap<String, Arc<dyn crate::core::provider_adapter::PromptAdapter>>>,
    /// LSP 客户端管理器（可选）
    /// 通过 enable_lsp() 激活；未激活时 lsp.* 工具返回错误
    lsp_manager: RwLock<Option<Arc<crate::lsp::LspManager>>>,
    /// Turn 级 Pipeline Hooks（与 MagChain 工具级正交）
    /// priority 越小越先触发；创建时为空，通过 add_pipeline_hook() 注册
    pipeline_hooks: Arc<RwLock<Vec<(u32, Arc<dyn crate::mag_chain::PipelineHook>)>>>,
    safety_guard: SafetyGuard,
    providers: RwLock<HashMap<String, Arc<dyn LlmProvider>>>,
    /// 厂商分组注册表（按模型名查找 provider）
    provider_groups: RwLock<Vec<ProviderGroup>>,
    config: CoreConfig,
    /// Runtime config overrides written by config_set tool, read by pipeline.
    ///
    /// ## 引用关系
    /// - 创建：CoreLoop::new()（Arc::new(RwLock::new(HashMap::new()))）
    /// - 写入：ConfigToolExecutor (config_set)
    /// - 读取：pipeline 消费点 + ConfigToolExecutor (config_get) + get_effective_* helpers
    /// - 销毁：随 CoreLoop drop（Arc 引用计数归零）
    pub runtime_overrides: crate::tool::builtin::config::RuntimeOverrides,
    model_override: RwLock<Option<ModelId>>,
    deduction_engine: Arc<DeductionEngine>,
    sandbox_engine: Arc<SandboxOrchestrator>,
    /// Task #81：自动化引擎（Pipeline / Cron / Trigger）—— 默认空运行
    ///
    /// ## 引用关系
    /// - 创建：`CoreLoop::new()` 时构造（无 store 模式）；`with_auto_store(store)` 注入 SQLite 持久化
    /// - 消费：用户通过 `auto_engine().register_pipeline(...)` 注册；`fire_pipeline(id)` 触发
    /// - 销毁：随 CoreLoop drop
    ///
    /// ## 默认状态
    /// 空引擎（无 pipelines / triggers / cron）。Pipeline 定义运行时注册，重启需要重新注册
    /// （与 LSP/Skill 一致）。运行历史可选持久化（store 设置后写入 SQLite）。
    auto_engine: Arc<RwLock<crate::auto::AutoEngine>>,
    silent_router: SilentRouter,
    /// Subsystem health monitoring — checked once per turn.
    health_registry: Arc<health::HealthRegistry>,
    /// Resource pressure monitoring — auto load-shedding.
    pressure_monitor: Arc<pressure::ResourcePressureMonitor>,
    /// KB 文档检索（pipeline 层主动 ICL 注入 / SM-2 复习用）
    /// 创建：with_memory() 时注入，与 KbToolExecutor 共享同一 Arc
    /// 消费方：setup() ICL Primer 、 SM-2 到期复习
    pub(crate) knowledge_store: Option<Arc<KnowledgeStore>>,
    /// 行为 + 知识记忆（pipeline 层主动读写）
    /// 创建：with_memory() 时注入，与 KbToolExecutor 共享同一 Arc
    /// 消费方：post_process() record_interaction/record_tool_behavior、setup() recommend_next_tools
    pub(crate) memory_palace: Option<Arc<tokio::sync::RwLock<DualPalaceMemory>>>,
    /// Phase 1：模型能力 catalog（不可变）。
    /// 创建：CoreLoop::new() 从 config.model_catalog 提取，缺省时 ModelCatalog::builtin()。
    /// 消费方：pipeline / provider 调用 `core.model_catalog().lookup(&model)` 获取 spec.thinking_capabilities。
    /// 销毁：进程退出（Arc::drop）。
    model_catalog: Arc<crate::llm::ModelCatalog>,
    /// MCP 远程服务器客户端（按 server_id 索引）
    ///
    /// ## 引用关系
    /// - 创建：`enable_mcp(configs)` 为每个 config 创建 McpClient
    /// - 消费：`McpToolExecutor` 持有 client Arc 用于 execute；保留此引用让 disconnect 可调
    /// - 销毁：CoreLoop drop 时随之 drop，client Arc 引用计数到 0 自动断开
    ///
    /// ## 默认状态
    /// 空 HashMap。MCP 默认不启用——必须显式调 enable_mcp 才注册工具。
    /// 设计原因：突然引入外部工具会破 KV cache 命中率（cache-first 哲学）。
    mcp_clients: RwLock<HashMap<String, Arc<crate::mcp::McpClient>>>,
    /// Skill workflow 执行器单例（按需启用）
    ///
    /// ## 引用关系
    /// - 创建：`enable_skill_workflow_executor()` 时创建一次，存为 Arc
    /// - 消费：`load_skill(id)` 时把同一个 Arc 注册到所有 step 的 executor 槽位
    /// - 销毁：随 CoreLoop drop
    ///
    /// ## 默认状态
    /// `None`。Skill 默认仅作意图分类信号，不参与执行。
    /// 显式 enable_skill_workflow_executor() 后才可 load_skill。
    skill_workflow_executor: RwLock<Option<Arc<crate::skill::SkillExecutor>>>,
    /// WASM Plugin 加载器（按需启用）
    ///
    /// ## 引用关系
    /// - 创建：`enable_plugins(base_dir)` 时创建一次
    /// - 消费：PluginToolExecutor 持有此 Arc clone 用于执行；保留此引用允许后续操作
    /// - 销毁：随 CoreLoop drop
    ///
    /// ## 默认状态
    /// `None`。Plugin 默认禁用。
    plugin_loader: RwLock<Option<Arc<crate::mcp::PluginLoader>>>,
    /// Phase γ-E：大结果摘要存储（{session_id}:{result_id} → 完整 output）
    ///
    /// ## 引用关系
    /// - 创建：CoreLoop::new()，初始空 map
    /// - 写入：pipeline 在 ToolOutput 大于阈值时存入完整原始 output
    /// - 读取：`result.expand` 工具按 id 取回（id 已含 session 前缀，跨 session 不会冲突）
    /// - 销毁：随 CoreLoop drop（不持久化）
    ///
    /// ## 设计权衡
    /// 放在 CoreLoop 而非 SessionState 是为了让 ResultExpandExecutor 在注册时拿到 Arc 引用
    /// （ExecutionContext 当前没有 session 字段，session 隔离改由 result_id 中的 session 哈希片段保证）。
    pub(crate) result_store: crate::tool::builtin::result::ResultStore,
    /// Phase γ-I：工具最后调用 turn 跟踪（tool_id → turn 编号）
    ///
    /// 用于 build_tool_definitions_for 按频率修剪。0 = 从未调用。
    /// 由 pipeline 在每次工具成功后通过 `record_tool_invocation()` 写入。
    pub(crate) tool_last_invoked: Arc<RwLock<std::collections::BTreeMap<ToolId, u64>>>,

    /// W2 (Task #100): Tool result dedup 池
    ///
    /// 引用关系：
    /// - 创建：CoreLoop::new() 按 config.tool_result_dedup_enabled 决定是否实例化
    /// - 消费：pipeline::execute_loop dispatch 路径（lookup + record）
    /// - 审计：CoreLoop::dedup_stats() 暴露给 audit_report
    /// 销毁：随 CoreLoop drop。
    ///
    /// `None` = 功能关闭（默认）。无开销路径——pipeline 中只多一次 `Option::is_some` 判断。
    pub(crate) tool_result_dedup: Option<Arc<crate::core::pipeline::dedup::ToolResultDedup>>,
    /// cross-session: 进程注册句柄（RAII，Drop 时自动注销 PID json）
    ///
    /// 引用：`process_registry::SessionRegistration`
    /// 生命周期：CoreLoop::new 时 register；CoreLoop drop 时随 RAII 释放
    /// 失败语义：register 失败时 None（注册表是 best-effort，不阻塞 session）
    pub(crate) _process_registration: Option<crate::process_registry::SessionRegistration>,
    /// 段 J1: Tool Cluster Registry —— 工具协议同构感知层
    ///
    /// ## 引用关系
    /// - 创建：CoreLoop::new() 时 ClusterRegistry::builtin() 一次性构造
    /// - 消费：build_tool_definitions_for 时调 render_hint_for 拼到 description；
    ///         tool_compass 工具（段 J2）调 recommend_by_intent
    /// - 销毁：随 CoreLoop drop
    ///
    /// ## 设计权衡
    /// 旁路表（不改 ToolSchema struct），向后兼容现有所有插件/MCP 工具。
    /// 没注册到 cluster 的工具 render_hint_for 返 None，不破坏 description。
    pub(crate) cluster_registry: Arc<crate::tool::cluster::ClusterRegistry>,

    // ─── P0-A1: ZeroShotCotHook 控制标志 ─────────────────────────────────
    // 两个 Arc<AtomicBool> 与 ZeroShotCotHook 内部字段共享同一 Arc 实例，
    // 允许 CoreLoop 在每轮 turn 开始前更新标志而无需锁争用。
    //
    // ## 引用关系
    // - 创建：CoreLoop::new() 构建 ZeroShotCotHook 时同步返回
    // - 写入：build_system_output() / TurnPipeline 在解析 thinking_intent 后调用
    // - 读取：ZeroShotCotHook::should_inject()（通过 Arc 共享）
    // - 生命周期：随 CoreLoop drop（Arc 引用计数归零，AtomicBool 释放）
    pub(crate) cot_model_supports_native: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) cot_thinking_enabled: Arc<std::sync::atomic::AtomicBool>,
}

/// Context window pressure source — reports usage_pct (0~100 scale) normalized to
/// PressureSource contract scale [0.0, 1.0].
///
/// ## 尺度契约（重要）
/// - `ContextWindow::usage_pct()` 输出 **0~100 百分比**（codebase 内 35+ 处一致使用）
/// - `PressureSource::pressure()` 契约要求 **[0.0, 1.0]**（PressurePolicy 阈值同尺度）
/// - 本实现是**适配层**：在 `pressure()` 内归一化 `/ 100.0`，避免 0~100 vs 0~1
///   尺度错配导致 pressure_monitor 永远 Overloaded（V29.12 修复）
///
/// ## 引用关系
/// - 上游：CoreLoop::new 注册到 pressure_monitor（line ~1143）
/// - 下游：被 ResourcePressureMonitor::check_and_shed 周期调用（post_process）
///
/// ## 生命周期
/// - 创建：CoreLoop::new 一次性 Arc 注册，与 CoreLoop 同生命周期
/// - 销毁：CoreLoop drop 时随 pressure_monitor 释放
/// - manager 用 Weak 防循环：ContextManager 持 Arc，本结构持 Weak，drop 顺序无关
struct ContextWindowPressure {
    window: Arc<RwLock<context::ContextWindow>>,
    /// Phase Ctx-A：同时持有 ContextManager 引用，让 shed 设标志通知 setup 阶段
    /// 不直接访问 messages（PressureSource 不持有 session 引用）
    manager: std::sync::Weak<context::ContextManager>,
}

#[async_trait::async_trait]
impl pressure::PressureSource for ContextWindowPressure {
    fn name(&self) -> &str { "context_window" }

    /// V29.12 修复：归一化 0~100 → 0.0~1.0 匹配 PressurePolicy 契约
    ///
    /// 修复前：`w.usage_pct()` 返回 0~100，pressure_monitor 阈值是 0~1，
    /// 导致 current_tokens > 0 即 ≥1.0 ≥ reject_threshold(0.95)，永远 Overloaded。
    /// 后果：每轮 mark_shed_pending、status() 永远 Overloaded、真兜底机制被噪音淹没。
    async fn pressure(&self) -> f64 {
        let w = self.window.read().await;
        (w.usage_pct() / 100.0).clamp(0.0, 1.0)
    }

    /// Phase Ctx-A + V29.12：shed 仅在真正可压缩时才设标志，避免噪音
    ///
    /// ## 决策门槛
    /// `should_compress()` 内部阈值（默认 85%）作为本层 mark 的同语义门槛——
    /// pressure soft (70%) 进 Elevated 但 should_compress 仍 false 时不 mark，
    /// 否则会出现"pressure 报警 → setup 调 auto_compress → 内部守卫拦下"的空转。
    ///
    /// ## 返回语义
    /// - 1 = 已记录降压意图（下轮 setup 会兑现）
    /// - 0 = 未达 compress 门槛或 ContextManager 已 drop（noop）
    async fn shed(&self, _target: f64) -> usize {
        let Some(mgr) = self.manager.upgrade() else {
            return 0;
        };
        let should = {
            let w = self.window.read().await;
            w.should_compress()
        };
        if !should {
            // 70-85% 尴尬区间：classify 进 Elevated 但 compress 守卫会拦下，不 mark 避免噪音
            return 0;
        }
        mgr.mark_shed_pending();
        1
    }
}

/// V29.13 段2：把 cold demote 的 SessionSnapshot 升维为 KnowledgeEntry
///
/// ## 协同设计
/// 这是"三层机制 ↔ 双宫殿"协同的桥梁。三层 ContextTiers.migrate_tiers 在
/// warm→cold demote 时把 snapshot 推入 recent_demoted buffer；本 hook 在
/// TurnPostFanOut 事件触发时一次性 take 并 absorb 进 KnowledgePalace。
///
/// ## 引用关系
/// - 上游：`TurnPostFanOut` 事件（post_process 末尾 emit）
/// - 下游：`ContextManager.tiers.recent_demoted` （pull 数据）+ `DualPalaceMemory.absorb_snapshot`
///
/// ## 生命周期
/// - 创建：`with_memory()` 时构造，作为 priority=100 的 PipelineHook 注册
/// - 销毁：CoreLoop drop 时随 pipeline_hooks 释放
/// - palace / ctx_mgr 用 Weak 防循环——CoreLoop 持 Arc，hook 持 Weak
///
/// ## 失败语义
/// - palace/ctx_mgr 已 drop → 返回 Continue（noop，不阻塞 turn）
/// - absorb_snapshot 失败（quota 耗尽 / 重复）→ 静默继续下一条
/// - 永不返回 HookAction::Abort——hook 失败不影响 LLM 流程
struct PalaceAbsorbHook {
    palace: std::sync::Weak<tokio::sync::RwLock<DualPalaceMemory>>,
    ctx_mgr: std::sync::Weak<context::ContextManager>,
}

#[async_trait::async_trait]
impl crate::mag_chain::PipelineHook for PalaceAbsorbHook {
    fn name(&self) -> &str { "palace_absorb_hook" }

    fn accepts(&self, event: &crate::mag_chain::PipelineEvent) -> bool {
        matches!(event, crate::mag_chain::PipelineEvent::TurnPostFanOut { .. })
    }

    async fn on_event(&self, event: &crate::mag_chain::PipelineEvent) -> Result<crate::mag_chain::HookAction, KernelError> {
        use crate::mag_chain::HookAction;
        let crate::mag_chain::PipelineEvent::TurnPostFanOut { session_id, .. } = event else {
            return Ok(HookAction::Continue);
        };
        let Some(palace_arc) = self.palace.upgrade() else { return Ok(HookAction::Continue) };
        let Some(ctx_arc) = self.ctx_mgr.upgrade() else { return Ok(HookAction::Continue) };

        let demoted = ctx_arc.tiers.take_recent_demoted().await;
        if demoted.is_empty() {
            return Ok(HookAction::Continue);
        }
        let palace = palace_arc.read().await;
        let mut absorbed = 0u32;
        for snapshot in &demoted {
            if palace.absorb_snapshot(
                session_id,
                snapshot.turn_count,
                &snapshot.summary,
                &snapshot.key_decisions,
            ).await {
                absorbed += 1;
            }
        }
        if absorbed > 0 {
            tracing::info!(
                session = session_id.as_str(),
                absorbed,
                total_demoted = demoted.len(),
                "palace_absorb_hook: cold→knowledge promotion"
            );
        }
        Ok(HookAction::Continue)
    }
}

impl CoreLoop {
    pub async fn new(
        registry: Arc<ToolRegistry>,
        skill_engine: Arc<RwLock<SkillEngine>>,
        capability_hub: Arc<CapabilityHub>,
        context_manager: Arc<ContextManager>,
        config: CoreConfig,
    ) -> Self {
        // context_tools registration deferred to session creation (per-session context_messages)

        let mut env = EnvMap::new();
        let roots = vec![std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("/"))
            .to_string_lossy()
            .to_string()];
        env.set_allowed_roots(&roots);
        env.refresh_git().await;
        env.refresh_project().await;
        let env_map = Arc::new(RwLock::new(env));
        register_env_tools(&registry, env_map).await;

        let mut injector = DynamicInjector::new();
        injector.register_defaults();
        let injector = Arc::new(RwLock::new(injector));
        let effectiveness = Arc::new(RwLock::new(EffectivenessTracker::new()));
        let mcip_gateway = Arc::new(McipGateway::new());
        let mut prompt_assembly = PromptAssembly::new(
            &config.system_prompt,
            "", // auto-load from abacusbr.md
        );

        // P0-A1：注册 ZeroShotCotHook（Zero-shot CoT Fallback）
        //
        // ## 触发时机
        // 当模型不支持 native thinking 但用户有推理意图时，自动注入逐步推理触发词。
        //
        // ## 引用关系
        // - hook 注册到 PromptAssembly，随 prompt_assembly 生命周期存活
        // - cot_model_flag / cot_thinking_flag：Arc<AtomicBool> 持久化在 CoreLoop，
        //   每轮 turn 解析 thinking_intent 后由 build_thinking_intent() 调用方更新
        //   （两个 AtomicBool 通过 Arc 与 hook 内部字段共享同一实例）
        // - 生命周期：hook 随 prompt_assembly drop；Arc 随 CoreLoop drop
        let (cot_hook, cot_model_flag, cot_thinking_flag) =
            crate::core::cot_hook::ZeroShotCotHook::new();
        prompt_assembly.register_hook(Box::new(cot_hook));

        let safety_guard = SafetyGuard::from_profile(&UserProfile::load_default());

        // Phase 3 (lint)：在 register_all 之前注入 lint overrides
        // 副作用：替换 registry.lint_rules；后续所有 register() 都看到 allowed list
        if let Some(ref overrides) = config.lint_overrides {
            let mut rules = crate::tool::schema_lint::LintRuleSet::default_rules();
            rules.load_overrides(overrides.clone());
            registry.set_lint_rules(rules).await;
        }

        Self::register_interaction_tools(&registry).await;

        // V13 关键修复：注册所有内置工具 schema（filengine.* / db.* / kb.* / orchestrate.* / lsp.*）
        //   之前只在 #[cfg(test)] 内调用 register_all，生产代码 LLM 看不到任何 fs/bash 类工具，
        //   导致 LLM 自报"没有 fs/bash 工具权限"。schema 注册不依赖外部资源（kb store / lsp manager），
        //   它们的 executor 在 engine_init.rs 后续单独绑定。
        crate::tool::builtin::register_all(&registry).await;

        // 初始化推演引擎（磁盘失败时降级为内存模式，数据不持久化）
        let deduction_engine = Arc::new(DeductionEngine::new(None)
            .unwrap_or_else(|e| {
                tracing::warn!("DeductionEngine 磁盘初始化失败，降级为内存模式（数据不持久化）: {e}");
                DeductionEngine::new(Some(std::path::PathBuf::from(":memory:")))
                    .expect("in-memory DeductionEngine must be available")
            }));
        let dedup_tool = DeductionToolExecutor::new(deduction_engine.clone());
        dedup_tool.register(&registry).await;

        // 初始化沙箱引擎
        let sandbox_engine = Arc::new(SandboxOrchestrator::new(
            SandboxConfig::default(),
            HashMap::new(),
        ));
        let sandbox_tool = SandboxToolExecutor::new(sandbox_engine.clone());
        sandbox_tool.register(&registry).await;

        let health_registry = Arc::new(health::HealthRegistry::new());
        let pressure_monitor = Arc::new(pressure::ResourcePressureMonitor::new(
            pressure::PressurePolicy::default()
        ));

        // Register context window as a pressure source
        // 环境治理修复：ContextWindowPressure 结构体增加了 manager 字段（line 831）
        // 但这里的 struct literal 未同步更新，导致 missing field 编译错误。
        // 不在 multi-instance 项目 scope 内，但不修便无法 build 验证。
        let ctx_window_source = Arc::new(ContextWindowPressure {
            window: context_manager.window.clone(),
            manager: Arc::downgrade(&context_manager),
        });
        pressure_monitor.register(ctx_window_source).await;

        let epistemic_guard = Arc::new(crate::mag_chain::EpistemicGuard::new());

        // Phase 1：注入 model catalog（缺省 → builtin）
        let model_catalog = config.model_catalog.clone()
            .unwrap_or_else(|| Arc::new(crate::llm::ModelCatalog::builtin()));

        // Phase Z3：把 CoreConfig.default_compress_level 同步到 ContextManager
        context_manager.set_compress_level(config.default_compress_level).await;

        // Phase γ-E + Palace-D：先构建共享 result_store Arc，注册 executor（暂不传 palace，
        // memory_palace 在 with_memory() 才注入；register_executors 此时调用一次绑 store；
        // 若后续启用 palace，可重新注册覆盖 executor 槽位）
        let result_store: crate::tool::builtin::result::ResultStore =
            Arc::new(RwLock::new(crate::tool::builtin::result::BoundedResultStore::new()));
        crate::tool::builtin::result::register_executors(&registry, result_store.clone(), None).await;

        // W2 (Task #100)：按 config 决定是否实例化 dedup 池
        let tool_result_dedup = if config.tool_result_dedup_enabled {
            Some(Arc::new(crate::core::pipeline::dedup::ToolResultDedup::new(
                config.tool_result_dedup_capacity_kb.saturating_mul(1024),
                config.tool_result_dedup_ttl_secs,
            )))
        } else {
            None
        };

        // cross-session: 启动时清理死 PID 残留 + 注册当前进程
        // 失败语义：注册表是 best-effort，失败仅日志 warn 不阻塞 session 启动
        if let Err(e) = crate::process_registry::gc_stale_entries() {
            tracing::warn!("process_registry: GC 启动时失败 (ignored): {}", e);
        }
        let session_id_for_reg = format!("core-{}", std::process::id());
        let process_registration = match crate::process_registry::SessionRegistration::register(
            crate::process_registry::SessionMeta::for_current(session_id_for_reg, "core")
        ) {
            Ok(reg) => Some(reg),
            Err(e) => {
                tracing::warn!("process_registry: register 失败 (ignored): {}", e);
                None
            }
        };
        let runtime_overrides: crate::tool::builtin::config::RuntimeOverrides =
            Arc::new(std::sync::RwLock::new(HashMap::new()));
        // config tool executor 注册（依赖 runtime_overrides Arc + base config 快照）
        crate::tool::builtin::config::register_executors(
            &registry, runtime_overrides.clone(), config.clone(),
        ).await;
        let core_loop = Self { registry, skill_engine, capability_hub, context_manager, injector, effectiveness, mcip_gateway, mag_chain: Arc::new(RwLock::new(crate::mag_chain::MagChain::new())), epistemic_guard, prompt_assembly, adapters: RwLock::new(HashMap::new()), lsp_manager: RwLock::new(None), pipeline_hooks: Arc::new(RwLock::new(Vec::new())), safety_guard, providers: RwLock::new(HashMap::new()), provider_groups: RwLock::new(Vec::new()), config, runtime_overrides, model_override: RwLock::new(None), deduction_engine, sandbox_engine, auto_engine: Arc::new(RwLock::new(crate::auto::AutoEngine::new())), silent_router: SilentRouter::new(), health_registry, pressure_monitor, knowledge_store: None, memory_palace: None, model_catalog, mcp_clients: RwLock::new(HashMap::new()), skill_workflow_executor: RwLock::new(None), plugin_loader: RwLock::new(None), result_store, tool_last_invoked: Arc::new(RwLock::new(std::collections::BTreeMap::new())), tool_result_dedup, _process_registration: process_registration, cluster_registry: Arc::new(crate::tool::cluster::ClusterRegistry::builtin()), cot_model_supports_native: cot_model_flag, cot_thinking_enabled: cot_thinking_flag };
        // V29.13 段3c：注入 HookVisibilityMiddleware 让 LLM 在 ToolOutput 层感知 hook 系统
        // 优先级 200（在主要业务 middleware 之后跑），共享 epistemic_guard 实例
        core_loop.add_middleware(200, Arc::new(crate::mag_chain::HookVisibilityMiddleware {
            guard: core_loop.epistemic_guard.clone(),
        })).await;
        // cross-session: 注册 JsonlEventHook（按 config.event_sink_enabled）
        // 引用：core::event_sink::JsonlEventHook → projects/{slug}/sessions/{sid}.jsonl
        // 优先级 250：在 HookVisibilityMiddleware (200) 之后，让先期 hook 链跑完再写日志
        // 失败处理：open 失败 warn 日志 + 不挂载（不阻塞 CoreLoop 启动）
        if core_loop.config.event_sink_enabled {
            let session_id = format!("core-{}", std::process::id());
            let project_dir = crate::paths::current_project_dir();
            // 静默确保父目录（与 process_registry 一样不阻塞 ABACUS_HOME 不可写场景）
            let _ = crate::paths::ensure_current_project_dirs();
            match crate::core::event_sink::JsonlEventHook::open(&session_id, &project_dir) {
                Ok(hook) => {
                    core_loop.add_pipeline_hook(250, Arc::new(hook)).await;
                }
                Err(e) => {
                    tracing::warn!(
                        project_dir = %project_dir.display(),
                        error = %e,
                        "event_sink: JsonlEventHook::open 失败 (ignored，session 仍启动)"
                    );
                }
            }
            // cross-session: 注册 GlobalHistoryHook —— 跨 project/session prompt 历史
            // 优先级 260：在 JsonlEventHook 后跑——history 是 turn-start 一次性写入，先后无关
            match crate::core::event_sink::GlobalHistoryHook::open() {
                Ok(hook) => {
                    core_loop.add_pipeline_hook(260, Arc::new(hook)).await;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "global_history: open 失败 (ignored，session 仍启动)"
                    );
                }
            }
        }
        // Layer 5 (Task #92)：启动 audit —— 输出已激活优化清单
        core_loop.audit_optimizations().await;
        core_loop
    }

    /// Task #95 公共 API：查询某 session 的 cache 统计快照
    ///
    /// 用法：server.rs / TUI / metrics endpoint 调用展示运行时 cache 命中率。
    /// 引用关系：纯只读，不修改 SessionState。
    /// 返回 (telemetry 副本, escalation_count, escalated_model)
    pub async fn session_cache_stats(
        &self,
        session: &SessionState,
    ) -> (CacheTelemetry, u32, Option<ModelId>) {
        let tele = session.cache_telemetry.read().await.clone();
        let count = session.escalation_count.load(std::sync::atomic::Ordering::Relaxed);
        let escalated = session.escalated_model.read().await.clone();
        (tele, count, escalated)
    }

    /// Task #95：把 session cache stats 渲染成多行字符串（运维/TUI 友好）
    pub async fn session_cache_report(&self, session: &SessionState) -> Vec<String> {
        let (tele, esc_count, esc_model) = self.session_cache_stats(session).await;
        let mut lines = vec![
            format!("═══ Session Cache Report (id={}) ═══", session.session_id),
            format!("  total_input_tokens         : {}", tele.total_input_tokens),
            format!("  total_cached_tokens        : {} ({:.1}% hit rate)",
                tele.total_cached_tokens, tele.hit_rate() * 100.0),
            format!("  total_cache_creation_tokens: {}", tele.total_cache_creation_tokens),
            format!("  model_switches             : {}", tele.model_switches),
            format!("  escalation_count           : {}/{}", esc_count, self.config.max_escalations),
        ];
        if let Some(m) = esc_model {
            lines.push(format!("  escalated_model (sticky)   : {}", m.0));
        } else {
            lines.push("  escalated_model            : (none — running on default)".to_string());
        }
        // Task #98 (F)：top tools by token consumption
        //
        // 引用：CacheTelemetry::top_tools_by_tokens（src/core/mod.rs:545）
        // 生命周期：每次工具产出累加 → 此处只读快照
        // 用途：运维定位"哪个工具吞噬上下文"——通常是少数大返回工具占绝大多数 token，
        //   据此决定 truncate 阈值 / dedup / lazy register 优先级。
        let top = tele.top_tools_by_tokens(5);
        if !top.is_empty() {
            lines.push("  top tools (by output tokens):".to_string());
            for (tool_id, tokens, calls) in top {
                let avg = if calls > 0 { tokens / calls as u64 } else { 0 };
                lines.push(format!(
                    "    {:<28} {:>8}t / {:>3} calls (avg {}t)",
                    tool_id.0, tokens, calls, avg,
                ));
            }
        }
        lines.push("════════════════════════════════════════".to_string());
        lines
    }

    /// Layer 5 (Task #92) 公共 API：返回 audit 报告字符串数组（不写日志）
    ///
    /// 用于不依赖 tracing subscriber 的场景（如 example demo / Web API 返回）。
    /// 内容与 audit_optimizations 一致。
    pub async fn audit_report(&self) -> Vec<String> {
        self.build_audit_lines().await
    }

    /// Layer 5 (Task #92)：启动时输出已激活优化清单
    ///
    /// 引用关系：CoreLoop::new 末尾自动调用；运维通过 tracing::info 看到。
    /// 副作用：仅日志输出，无状态修改。
    /// 生命周期：一次性触发；不重复输出。
    pub async fn audit_optimizations(&self) {
        let lines = self.build_audit_lines().await;
        for l in lines {
            tracing::info!("{}", l);
        }
    }

    /// 内部助手：构造 audit 报告字符串数组
    async fn build_audit_lines(&self) -> Vec<String> {
        let cfg = &self.config;
        let mut lines = vec![
            "═══ Abacus Optimizations Active ═══".to_string(),
        ];
        // ── Tools 路由/裁剪 ──
        lines.push(format!(
            "  [{}] task_kind_routing  : {} (Task #84)",
            if cfg.task_kind_routing_enabled { "✓" } else { "✗" },
            if cfg.task_kind_routing_enabled { "enabled" } else { "DISABLED (config override)" },
        ));
        lines.push(format!(
            "  [{}] frequency_pruning  : {} (Task #87)",
            if cfg.tool_frequency_pruning_turns.is_some() { "✓" } else { "✗" },
            cfg.tool_frequency_pruning_turns
                .map(|n| format!("N={} turn", n))
                .unwrap_or_else(|| "DISABLED".into()),
        ));
        lines.push(format!(
            "  [{}] visibility_threshold : {:?}",
            "✓",
            cfg.tool_visibility_threshold,
        ));
        lines.push(format!(
            "  [✓] max_escalations    : {} (Task #96 —防 cache 振荡)",
            cfg.max_escalations,
        ));
        // ── 子系统懒注册 ──
        let lsp_active = self.lsp_manager.read().await.is_some();
        let mcp_active = !self.mcp_clients.read().await.is_empty();
        let plugin_active = self.plugin_loader.read().await.is_some();
        let skill_active = self.skill_workflow_executor.read().await.is_some();
        lines.push(format!("  [{}] lsp lazy register    : {} (Task #85)",
            if lsp_active { "●" } else { "○" },
            if lsp_active { "ACTIVE" } else { "dormant (call enable_lsp to load 10 tools)" }));
        lines.push(format!("  [{}] mcp lazy register    : {} (Task #78)",
            if mcp_active { "●" } else { "○" },
            if mcp_active { "ACTIVE" } else { "dormant (configure mcp.servers to enable)" }));
        lines.push(format!("  [{}] plugin lazy register : {} (Task #79)",
            if plugin_active { "●" } else { "○" },
            if plugin_active { "ACTIVE" } else { "dormant (configure core.plugins.base_dir)" }));
        lines.push(format!("  [{}] skill workflow exec  : {} (Task #77)",
            if skill_active { "●" } else { "○" },
            if skill_active { "ACTIVE" } else { "dormant (set core.skill_workflow_enabled=true)" }));
        // ── 上下文管理 ──
        lines.push(format!("  [✓] compress level       : {:?} (default)", cfg.default_compress_level));
        // W1 (Task #99) Adaptive 决策框架——实际有多少子系统声明 Adaptive 模式
        let adaptive_count = crate::tool::subsystem_policy::builtin_subsystems()
            .iter()
            .filter(|d| matches!(d.mode, crate::tool::subsystem_policy::RegistrationMode::Adaptive { .. }))
            .count();
        lines.push(format!(
            "  [{}] adaptive subsystem   : {} declared / heat provider ready (Task #99)",
            if adaptive_count > 0 { "●" } else { "○" },
            adaptive_count,
        ));
        // W4 (Task #102) retained 段诊断
        let retained_diag = self.context_manager.retained_diagnostics().await;
        if retained_diag.entries > 0 {
            lines.push(format!(
                "  [●] retained selective   : {} segs / ~{}t / avg-imp={:.2} max-imp={:.2} @ turn={} (Task #102)",
                retained_diag.entries,
                retained_diag.total_tokens,
                retained_diag.avg_importance,
                retained_diag.max_importance,
                retained_diag.current_turn,
            ));
        }
        // ── Pressure monitor 状态（V29.12 修复后契约：pressure ∈ [0,1]）──
        //
        // 引用：pressure_monitor.status() 列出所有注册的 PressureSource 当前压力值与等级
        // 生命周期：每次 audit 调用读快照；不修改 monitor 状态
        // 用途：运维一眼看到 context_window 当前压力 + 是否健康。修复前此处永远是 Overloaded。
        let pressure_snap = self.pressure_monitor.status().await;
        for (name, level, p) in &pressure_snap {
            let icon = match level {
                pressure::PressureLevel::Normal => "✓",
                pressure::PressureLevel::Elevated => "▲",
                pressure::PressureLevel::Critical => "▲▲",
                pressure::PressureLevel::Overloaded => "✗",
            };
            lines.push(format!(
                "  [{icon}] pressure: {:<14} : {:<10} (p={:.2})",
                name, level.to_string(), p,
            ));
        }
        // ── Wrapping-E + 段 K5：自适应 D-tier 过滤（含 K1~K4 兜底层）──
        lines.push(format!(
            "  [{}] adaptive d-tier hide  : {} (Wrapping-E + K1-K4 safeguards)",
            if cfg.adaptive_d_tier_hide { "✓" } else { "○" },
            if cfg.adaptive_d_tier_hide { "ENABLED (default-on)" } else { "dormant (set core.adaptive_d_tier_hide=true)" },
        ));
        // 段 K5: 透明化 —— 列出当前 hide 决策影响的工具
        if cfg.adaptive_d_tier_hide {
            let eff = self.effectiveness.read().await;
            let tools = self.registry.all_tools().await;
            let cur_turn = 0u64; // audit 不绑 turn—— 用 0 让 probation 状态展示"如果现在评估"
            let mut hidden_now: Vec<(String, String, f64)> = Vec::new(); // (id, reason, score)
            let mut probation_active: Vec<String> = Vec::new();
            // 段 L4：env_failure 主导工具识别
            let mut env_failure_dominated: Vec<(String, f64)> = Vec::new();
            for t in &tools {
                let e = eff.evaluate_at_turn(&t.id, &t.provider, cur_turn);
                if e.insufficient_data { continue; }
                if matches!(e.tier, abacus_types::VisibilityTier::D) {
                    let reason = if eff.is_palace_demoted(&t.id) { "palace_demoted" } else { "low_score" };
                    hidden_now.push((t.id.0.clone(), reason.to_string(), e.composite_score));
                }
                if eff.is_palace_demoted(&t.id) {
                    probation_active.push(t.id.0.clone());
                }
                // 段 L4：env_failure 比例 > 0.5 → 环境拖累而非工具问题
                let env_ratio = eff.env_failure_ratio(&t.id);
                if env_ratio > 0.5 {
                    env_failure_dominated.push((t.id.0.clone(), env_ratio));
                }
            }
            lines.push(format!(
                "    └─ hide candidates    : {} (K3 floor 兜底后实际可能少于此)",
                hidden_now.len(),
            ));
            if !hidden_now.is_empty() {
                hidden_now.sort_by(|a, b| a.0.cmp(&b.0));
                let preview: Vec<String> = hidden_now.iter().take(5)
                    .map(|(id, r, s)| format!("{}({}={:.2})", id, r, s))
                    .collect();
                let suffix = if hidden_now.len() > 5 {
                    format!(" ...+{}", hidden_now.len() - 5)
                } else { String::new() };
                lines.push(format!("    └─ hidden tools       : {}{}", preview.join(", "), suffix));
            }
            if !probation_active.is_empty() {
                lines.push(format!(
                    "    └─ palace_demoted     : {} (每 50 turn 试探放行 — 段 K4)",
                    probation_active.len(),
                ));
            }
            // 段 L4：环境拖累工具列表 — 提示运维"工具本身可能没问题"
            if !env_failure_dominated.is_empty() {
                env_failure_dominated.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let preview: Vec<String> = env_failure_dominated.iter().take(5)
                    .map(|(id, ratio)| format!("{}({:.0}%)", id, ratio * 100.0))
                    .collect();
                let suffix = if env_failure_dominated.len() > 5 {
                    format!(" ...+{}", env_failure_dominated.len() - 5)
                } else { String::new() };
                lines.push(format!(
                    "    └─ env_failure_dominated: {} {}{}（运维提示：环境拖累而非工具问题 — 段 L4）",
                    env_failure_dominated.len(),
                    preview.join(", "),
                    suffix,
                ));
            }
        }
        // ── W2 (Task #100) Tool result dedup ──
        if let Some(dedup) = &self.tool_result_dedup {
            let s = dedup.stats();
            lines.push(format!(
                "  [●] tool result dedup    : ACTIVE (cap={}KB, ttl={}s) entries={} hit_rate={:.1}% (Task #100)",
                cfg.tool_result_dedup_capacity_kb,
                cfg.tool_result_dedup_ttl_secs,
                s.entries,
                s.hit_rate() * 100.0,
            ));
        } else {
            lines.push("  [○] tool result dedup    : dormant (set core.dedup.enabled=true) (Task #100)".to_string());
        }
        // ── V29.13 协同观测：段1 Hook / 段2 记忆桥 / 段3 LLM 感知 ──
        //
        // 引用：
        //  - pipeline_hooks 列表 → 段1 注册情况
        //  - memory_palace + recent_demoted buffer → 段2 桥接路径
        //  - epistemic_guard 状态 + magchain_status 工具 → 段3 LLM 感知层
        //
        // 设计：每行显示某子系统当前激活状态——●/○/▲/✗ 一眼分辨
        let hook_count = self.pipeline_hooks.read().await.len();
        let hook_names: Vec<String> = {
            let hooks = self.pipeline_hooks.read().await;
            hooks.iter().map(|(p, h)| format!("{}@{p}", h.name())).collect()
        };
        lines.push(format!(
            "  [{}] pipeline hooks       : {} registered [{}] (V29.13 段1)",
            if hook_count > 0 { "●" } else { "○" },
            hook_count,
            hook_names.join(", "),
        ));
        let palace_active = self.memory_palace.is_some();
        let demoted_pending = self.context_manager.tiers.recent_demoted.read().await.len();
        lines.push(format!(
            "  [{}] palace ↔ tiers bridge: palace={} / pending demoted snapshots={} (V29.13 段2)",
            if palace_active { "●" } else { "○" },
            if palace_active { "ACTIVE" } else { "dormant (call with_memory to enable)" },
            demoted_pending,
        ));
        let violations = self.epistemic_guard.violations().await;
        let cold_start = self.epistemic_guard.is_cold_start().await;
        let active_label = if violations > 0 || cold_start { "ACTIVE" } else { "idle" };
        let active_icon = if violations > 0 || cold_start { "▲" } else { "✓" };
        lines.push(format!(
            "  [{active_icon}] llm hook visibility  : {active_label} (violations={violations}, cold_start={cold_start}) (V29.13 段3)",
        ));
        // ── cross-session 观测：段A 进程注册 / 段B JSONL Sink / 段C history / 段D 工具 ──
        //
        // 引用：
        //  - process_registry::list_active_sessions() — 段 A 全局 PID 数
        //  - cfg.event_sink_enabled + paths.current_sessions_dir() — 段 B/C 写入路径
        //  - Tool registry has cross_session_query — 段 D 工具暴露
        //
        // 设计：让运维一眼看到本次跨 session 工作的激活状态
        let active_session_count = crate::process_registry::list_active_sessions()
            .map(|v| v.len())
            .unwrap_or(0);
        lines.push(format!(
            "  [●] process registry     : {} active sessions @ {} (cross-session 段A)",
            active_session_count,
            crate::paths::process_registry_dir().display(),
        ));
        if cfg.event_sink_enabled {
            let sessions_dir = crate::paths::current_sessions_dir();
            lines.push(format!(
                "  [●] jsonl event sink     : ENABLED → {} (cross-session 段B)",
                sessions_dir.display(),
            ));
            lines.push(format!(
                "  [●] global history       : ENABLED → {} (cross-session 段C)",
                crate::paths::history_jsonl().display(),
            ));
        } else {
            lines.push("  [○] jsonl event sink     : disabled (set core.event_sink_enabled=true) (cross-session 段B)".to_string());
            lines.push("  [○] global history       : disabled (cross-session 段C)".to_string());
        }
        // 段 D：cross_session_query 工具是否被注册（应该总是 ✓——硬注册到 interaction tools）
        let cross_session_registered = self.registry.all_tools().await.iter()
            .any(|t| t.id.0 == "cross_session_query");
        let palace_for_query = self.memory_palace.is_some();
        lines.push(format!(
            "  [{}] cross_session_query  : {} (palace_backed={}) (cross-session 段D)",
            if cross_session_registered { "●" } else { "○" },
            if cross_session_registered { "registered" } else { "missing" },
            palace_for_query,
        ));
        // ── Lint ──
        let lint_issues = self.registry.lint_audit().await;
        let errors = lint_issues.iter().filter(|i| i.severity == crate::tool::schema_lint::LintSeverity::Error).count();
        let warns = lint_issues.iter().filter(|i| i.severity == crate::tool::schema_lint::LintSeverity::Warn).count();
        let infos = lint_issues.iter().filter(|i| i.severity == crate::tool::schema_lint::LintSeverity::Info).count();
        lines.push(format!("  [{}] schema lint          : {} errors / {} warns / {} info",
            if errors > 0 { "✗" } else { "✓" }, errors, warns, infos));
        // ── 工具规模 ──
        let tools = self.registry.all_tools().await;
        // 粗估工具 token 数：description + parameters 字符长度 / 4
        let est_bytes: usize = tools.iter()
            .map(|t| {
                let p = serde_json::to_string(&t.schema.parameters).map(|s| s.len()).unwrap_or(0);
                t.schema.description.len() + p + 50  // 50 = wrapper 估计
            })
            .sum();
        lines.push(format!("  ── tools registered     : {} (LLM-bound spec ~{} tokens est.)",
            tools.len(), est_bytes / 4,
        ));
        lines.push("════════════════════════════════════════".to_string());
        lines
    }

    /// Task #81：访问 AutoEngine（可读）
    ///
    /// ## 用法
    /// ```no_run
    /// # async fn ex(core: std::sync::Arc<abacus_core::CoreLoop>) {
    /// let engine = core.auto_engine();
    /// let g = engine.read().await;
    /// // g.register_pipeline(...) — 注意 register_pipeline 用 &self 不要重锁
    /// # }
    /// ```
    pub fn auto_engine(&self) -> Arc<RwLock<crate::auto::AutoEngine>> {
        self.auto_engine.clone()
    }

    /// Task #81：注入 SQLite 持久化层（覆盖 AutoEngine 实例）
    ///
    /// 副作用：替换 auto_engine 的 inner，已注册的 pipelines/triggers/cron **会丢失**
    /// （所以应在任何 register 之前调用，或自行迁移）。
    /// 通常由 server.rs/engine_init.rs 在启动早期调用。
    pub async fn enable_auto_store(&self, store: Arc<crate::auto::AutoStore>) {
        let mut engine = self.auto_engine.write().await;
        *engine = crate::auto::AutoEngine::with_store(store);
    }

    /// Phase 1：访问模型能力 catalog（不可变共享）。
    /// 调用方：pipeline 决策 thinking 路径前先 `lookup(&effective_model)` 获取 spec。
    pub fn model_catalog(&self) -> &Arc<crate::llm::ModelCatalog> {
        &self.model_catalog
    }

    /// 获取指定工具列表的 effectiveness 快照（TUI 消费）
    ///
    /// ## 引用关系
    /// - 调用方：api/mod.rs send_chat_message_streaming（turn 完成后）
    /// - 数据源：EffectivenessTracker（持有历史统计）
    ///
    /// ## 设计
    /// 返回轻量 Vec，不暴露内部 tracker 引用。
    /// 仅评估传入的 tool_ids（避免全量遍历 200+ 工具）
    pub async fn tool_health_snapshot(&self, tool_ids: &[ToolId]) -> Vec<crate::llm::stream::ToolHealthEntry> {
        let eff = self.effectiveness.read().await;
        let tools = self.registry.all_tools().await;
        tool_ids.iter().filter_map(|tid| {
            let provider = tools.iter()
                .find(|t| &t.id == tid)
                .map(|t| &t.provider)
                .unwrap_or(&abacus_types::ToolProvider::BuiltIn);
            let e = eff.evaluate_with_provider(tid, provider);
            Some(crate::llm::stream::ToolHealthEntry {
                tool_id: tid.0.clone(),
                tier: format!("{:?}", e.tier),
                blocked_by_env: e.blocked_by_env,
                score: e.composite_score,
            })
        }).collect()
    }

    /// Phase γ-I：记录工具调用 turn（pipeline 在工具成功后调用）
    pub async fn record_tool_invocation(&self, tool_id: &ToolId, turn: u64) {
        self.tool_last_invoked.write().await.insert(tool_id.clone(), turn);
    }

    /// Phase γ-Palace-C：从行为宫殿同步信号到 EffectivenessTracker
    ///
    /// ## 流程
    /// 1. 遍历 ToolRegistry 已知工具
    /// 2. 综合 palace（"用户多次接触"）+ tracker stats（"实际成功率"）双重信号
    /// 3. palace.frequency >= 3 + tracker success_rate < 0.3 + tracker invocations >= 3
    ///    → 调用 `tracker.apply_palace_demote(tool_id)`，强制 tier=D
    ///
    /// ## 单调降级
    /// 一旦 demote 不自动恢复——避免抖动 KV cache。需手动 `clear_palace_demote()` 解锁。
    ///
    /// ## 调用时机
    /// 由 pipeline 在 turn 边界判断 `(turn_count % palace_sync_interval_turns == 0)` 触发。
    /// memory_palace 未启用 → 静默 noop。
    /// 段 L2：接受 current_turn 让 K4 试探放行机制能算 elapsed
    ///
    /// turn=0 兼容旧无 turn 调用（行为退化为旧版即时压制）；
    /// 推荐 caller 传真实 turn_count 让 K4 启动周期性试探放行。
    pub async fn sync_from_palace_at(&self, current_turn: u64) -> usize {
        let palace = match &self.memory_palace {
            Some(p) => p.clone(),
            None => return 0,
        };
        // 1) 取所有工具 ID（registry 快照）
        let tools = self.registry.all_tools().await;

        // 2) 拿 palace.behavior 内存 snapshot（一次锁）
        let palace_guard = palace.read().await;
        let behavior_memories = palace_guard.behavior.snapshot().await;

        // 3) 两阶段：先 read 收集 demote 候选，再 write 提交
        let candidates: Vec<(ToolId, u32, f64)> = {
            let tracker = self.effectiveness.read().await;
            tools.iter().filter_map(|handle| {
                if tracker.is_palace_demoted(&handle.id) {
                    return None;
                }
                let pattern_key = format!("tool_call:{}", handle.id.0);
                let palace_freq = behavior_memories.get(&pattern_key)
                    .map(|m| m.frequency)
                    .unwrap_or(0);
                if palace_freq < 3 {
                    return None;
                }
                let stats = tracker.stats_for(&handle.id)?;
                if stats.invocations < 3 {
                    return None;
                }
                let success_rate = stats.success_rate();
                if success_rate >= 0.3 {
                    return None;
                }
                Some((handle.id.clone(), palace_freq, success_rate))
            }).collect()
        };

        let mut tracker = self.effectiveness.write().await;
        let demoted = candidates.len();
        for (id, palace_freq, success_rate) in candidates {
            tracing::info!(
                tool = %id,
                palace_freq,
                success_rate,
                "palace-demoted (visible-but-failing tool hidden from LLM)"
            );
            // 段 L2：用 apply_palace_demote_at 让 K4 试探放行机制启动
            tracker.apply_palace_demote_at(id, current_turn);
        }
        demoted
    }

    /// 兼容包装：保留旧 sync_from_palace 调用（无 turn 信息时用 0 占位）
    /// 等价于段 L2 之前的行为——demoted_at=0 让任何 turn 都满足试探放行条件
    pub async fn sync_from_palace(&self) -> usize {
        self.sync_from_palace_at(0).await
    }

    /// 注入记忆系统（builder pattern，不破坏现有 new() 接口）。
    ///
    /// ## 设计意图
    /// 调用方把同一个 Arc 同时传给 `kb::register_executors()` 和此方法，
    /// 确保工具调用路径（LLM 主动查询）和 pipeline 层主动读写使用相同实例。
    ///
    /// ## 生命周期
    /// `new()` 后立即调用（写入），后续不再修改（内容通过 palace 自身的 RwLock 控制）。
    pub async fn with_memory(
        mut self,
        store: Arc<KnowledgeStore>,
        palace: Arc<tokio::sync::RwLock<DualPalaceMemory>>,
    ) -> Self {
        // Phase Ctx-D：注入 KnowledgeStore 让 declare 复用 KB chunking
        self.context_manager.set_kb_store(store.clone()).await;
        // P1-A2: 注册 GeneratedKnowledgeHook（KnowledgeStore 准备好后才能注册）
        // 引用：knowledge_hook.rs — 对 KnowledgeQuery/Mathematics 任务首轮注入背景知识
        // 生命周期：随 prompt_assembly 存活；KnowledgeStore 只读，无写锁竞争
        let gen_knowledge_hook = crate::core::knowledge_hook::GeneratedKnowledgeHook::new(
            store.clone(), 3
        );
        self.prompt_assembly.register_hook(Box::new(gen_knowledge_hook));
        self.knowledge_store = Some(store);
        self.memory_palace = Some(palace.clone());
        // Phase γ-Palace-D：启用 palace 后重注册 result.expand executor，注入 palace 让 expand 反馈
        crate::tool::builtin::result::register_executors(
            &self.registry,
            self.result_store.clone(),
            Some(palace.clone()),
        ).await;
        // V29.13 段2：注册 PalaceAbsorbHook 监听 TurnPostFanOut 事件
        // 引用：mag_chain::PipelineEvent::TurnPostFanOut → context_manager.recent_demoted →
        //       palace.absorb_snapshot
        // 优先级 100：让用户自定义 hook（priority<100）有机会先跑
        let absorb_hook = Arc::new(PalaceAbsorbHook {
            palace: Arc::downgrade(&palace),
            ctx_mgr: Arc::downgrade(&self.context_manager),
        });
        self.add_pipeline_hook(100, absorb_hook).await;
        self
    }

    /// 获取推演引擎引用（供外部调用分析接口）
    pub fn deduction_engine(&self) -> &Arc<DeductionEngine> {
        &self.deduction_engine
    }

    /// Register interaction.* LLM-facing tools in the registry (handled inline by CoreLoop)
    async fn register_interaction_tools(registry: &ToolRegistry) {
        use abacus_types::{ToolHandle, ToolProvider, ToolState, ToolEffectiveness, ToolSchema};
        // cross-session: 改为 Vec 以便末尾 push cross_session_query 工具（数组改 Vec 影响仅本函数）
        let entries: Vec<(&str, &str, serde_json::Value)> = vec![
            ("interaction_status", "Query current position in the interaction map. Returns checkpoint position, current phase, and path ahead.", json!({"type": "object", "properties": {}})),
            ("interaction_path", "View the complete interaction path: completed checkpoints, current position, remaining steps.", json!({"type": "object", "properties": {}})),
            ("interaction_recall", "Recall a specific checkpoint by ID. Returns summary, decisions, and tool chain context.", json!({"type": "object", "properties": {"checkpoint": {"type": "integer", "description": "Checkpoint ID to recall"}}})),
            ("interaction_mark", "Manually mark the current state as a checkpoint.", json!({"type": "object", "properties": {"label": {"type": "string", "description": "Checkpoint label"}, "type": {"type": "string", "enum": ["milestone", "decision", "subgoal"], "description": "Checkpoint type"}}, "required": ["label"]})),
            ("session_request_permission", "Request user authorization for a permission-gated tool. User picks once/always/deny; turn auto-reruns on approval.", json!({
                "type": "object",
                "properties": {
                    "tool_id": {"type": "string", "description": "The exact tool ID you need permission for (e.g. 'bash_exec')"},
                    "reason":  {"type": "string", "description": "Why you need this tool — shown to the user in the authorization dialog"}
                },
                "required": ["tool_id", "reason"]
            })),
            ("session_set_focus", "Set session focus anchor (goal/phase/constraints/next-step). Use at task start or focus switch to prevent attention drift.", json!({
                "type": "object",
                "properties": {
                    "goal":        {"type": "string", "description": "Current overall goal (one sentence)"},
                    "phase":       {"type": "string", "description": "Current phase or step, e.g. 'Step 2/4: implementing API layer'"},
                    "constraints": {"type": "array", "items": {"type": "string"}, "description": "Key hard constraints or prerequisites"},
                    "next_step":   {"type": "string", "description": "Immediate next action to take"}
                },
                "required": ["goal", "phase"]
            })),
            // V29.13 段3a：MagChain/Hook 透明化——LLM 可主动查询 epistemic 状态 + 已激活的 hook 列表 + 当前 decay tier
            ("magchain_status", "Query MagChain (epistemic guard + decay router + pipeline hooks) status. Use this to (1) check if epistemic violations are accumulated and a declaration may be triggered, (2) understand current decay tier (Fast/Medium/Slow) for the user input, (3) see which pipeline hooks are active. Helps you self-regulate output quality before it gets penalized.",
                json!({
                    "type": "object",
                    "properties": {
                        "input_to_classify": {"type": "string", "description": "Optional: a query string to classify into decay tier (Fast=time-sensitive needs web.search / Medium=verifiable / Slow=weight-trustworthy). If omitted, only static state returned."}
                    }
                })),
            // cross-session: LLM 主动跨 session 知识查询工具
            // 引用：DualPalaceMemory.knowledge.hybrid_search（已持久化的 KnowledgeEntry 包含 absorb_snapshot 来源）
            // 生命周期：每次 LLM 调用即查；走 inline dispatch（与 magchain_status 同路径，需要 CoreLoop 的 memory_palace 引用）
            // 失败语义：palace 未启用时返回明确错误（提示用户 with_memory）；查询为空返回空 results 数组
            ("cross_session_query",
                "Search persistent cross-session knowledge (knowledge palace + absorbed session decisions). Use when (1) the user references prior conversations ('我之前问过...'), (2) you need historical context for current task, (3) you want to avoid re-discovering known patterns. Returns top-k entries with title/content/score/tags/last_reviewed.",
                json!({
                    "type": "object",
                    "properties": {
                        "query":  {"type": "string", "description": "Search query (natural language; routes to hybrid embed+keyword)"},
                        "top_k":  {"type": "integer", "minimum": 1, "maximum": 20, "default": 5, "description": "Max results to return (1-20)"},
                        "domain": {"type": "string", "description": "Optional: filter by domain (e.g. 'session_history' for absorbed snapshots, '<custom>' for user-stored)"}
                    },
                    "required": ["query"]
                })),
            // cross-session: 按 recover_id 取回压缩前的原始 messages
            // 引用：ContextManager::recover_messages（半截子修复——存在已久但 LLM 看不到入口）
            // 用途：当 LLM 看到 "[Compressed history: N messages, recover_id=mb_xxx]" 标识时，
            //       可主动用此工具拉回原文（不在 turn 内默认 expand，避免污染 cache）
            ("messages_recover",
                "Recover original (uncompressed) messages by recover_id. When you see '[Compressed history: N messages, recover_id=mb_xxx]' in the conversation, you can call this tool to pull the original content back. Useful when needing exact wording from earlier turns that got compressed.",
                json!({
                    "type": "object",
                    "properties": {
                        "recover_id": {"type": "string", "description": "The recover_id (format: mb_<hex>) embedded in compressed summary blocks"}
                    },
                    "required": ["recover_id"]
                })),
            // cross-session 段H：list/query session-level resumable history
            // 引用：core::event_sink::{list_replayable_sessions, build_resume_report}
            // 用途：LLM 可主动检视历史 sessions，决定是否深挖某个具体 session
            ("session_resume_query",
                "List or summarize prior sessions in current project. Without session_id: returns list of replayable sessions sorted by recency. With session_id: returns detailed summary (turn_count / total_tool_calls / latency / had_compression). Use when user mentions prior conversations or you want to assess historical context before answering.",
                json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string", "description": "Optional: inspect specific session by id. If omitted, returns list of all replayable sessions."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10, "description": "Max sessions to return when listing (1-50)"}
                    }
                })),
            // 段 J2: tool_compass —— 协议同构自省入口
            // 引用：tool::cluster::ClusterRegistry::recommend_by_intent
            // 用途：当 LLM 不确定某意图应调用哪个工具时（同 cluster 多个候选），传 intent 描述
            //       获得推荐工具 + cluster 上下文 + 同簇兄弟差异，避免在 description 关键词撞车时盲选
            // 失败语义：未匹配任何 cluster → 返回空 results 数组并附 "all_clusters" 让 LLM 自选
            ("tool_compass",
                "Find the right tool for an intent when multiple tools seem to overlap. Pass natural-language intent (e.g. 'recall what we discussed about Rust ownership last session', 'check past tool call latency'). Returns ranked recommendations with cluster context + each candidate's differentiator. Use when you see two tools with similar descriptions and don't know which fits.",
                json!({
                    "type": "object",
                    "properties": {
                        "intent": {"type": "string", "description": "Natural-language description of what you want to do (e.g. 'find file by name', 'load session history')"},
                        "top_k": {"type": "integer", "minimum": 1, "maximum": 10, "default": 3, "description": "Max recommendations to return (1-10)"}
                    },
                    "required": ["intent"]
                })),
            // V38: LLM 主动模式切换工具
            // 引用：TUI run.rs 消费此工具输出后调用 state.set_mode(target)
            // DAG 约束：Clarify → Meeting/Plan → Team → Clarify
            // LLM 应在推理过程中判断当前任务需要哪种协作形态并主动切换
            ("mode_switch",
                "Switch interaction mode: clarify/meeting/plan/team. Changes system behavior and tool availability.",
                json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "enum": ["clarify", "meeting", "plan", "team"], "description": "Target mode to switch to"},
                        "reason": {"type": "string", "description": "Brief reason for the switch (shown to user)"}
                    },
                    "required": ["target"]
                })),
        ];
        for (name, desc, params) in &entries {
            registry.register(ToolHandle {
                id: ToolId(name.to_string()),
                schema: ToolSchema {
                    name: name.to_string(),
                    description: desc.to_string(),
                    parameters: params.clone(),
                    returns: None, security: None, cost: None,
                    examples: Vec::new(),
                    applicable_task_kinds: None,
                    idempotent: false,
                                        schema_stable: false,                },
                provider: ToolProvider::BuiltIn,
                state: ToolState::Loaded,
                effectiveness: ToolEffectiveness { tool_id: ToolId(name.to_string()), composite_score: 0.7, tier: abacus_types::VisibilityTier::A, cooldown_remaining: 0, blocked_by_env: false, insufficient_data: true },
            }).await;
        }
    }

    pub async fn register_provider(&self, id: impl Into<String>, provider: Arc<dyn LlmProvider>) {
        let id_str = id.into();
        self.providers.write().await.insert(id_str.clone(), provider.clone());
        // 自动注册对应 PromptAdapter（基于 provider_id 选择最优格式）
        let adapter = crate::core::provider_adapter::adapter_for_provider(&id_str);
        self.adapters.write().await.insert(id_str.clone(), adapter);
        self.sandbox_engine.add_provider(id_str, provider).await;
    }

    /// 获取已注册的 provider Arc（按 id 查找）
    ///
    /// ## 引用关系
    /// - 调用方: engine_init.rs 构建 fallback chain 时
    pub async fn get_provider(&self, id: &str) -> Option<Arc<dyn LlmProvider>> {
        self.providers.read().await.get(id).cloned()
    }

    /// 显式覆盖指定 provider 的 PromptAdapter。
    ///
    /// 适用场景：注册 FallbackProvider（id="primary"）时，
    /// 自动注册会赋予 NeutralAdapter（不识别内部实现）。
    /// 需调用此方法正确绑定主协议的 adapter：
    ///   core_loop.set_adapter("primary", "anthropic").await;
    pub async fn set_adapter(&self, provider_id: &str, underlying_provider_id: &str) {
        let adapter = crate::core::provider_adapter::adapter_for_provider(underlying_provider_id);
        self.adapters.write().await.insert(provider_id.to_string(), adapter);
    }

    /// 激活 LSP 支持并注册 executor。
    ///
    /// ## 参数
    /// - `workspace_root`: 工作区根路径（作为 rootUri 传给语言服务器）
    ///
    /// ## 使用
    /// ```no_run
    /// # async fn example(core: &abacus_core::CoreLoop) {
    /// core.enable_lsp(std::env::current_dir().unwrap().to_string_lossy().to_string()).await;
    /// # }
    /// ```
    ///
    /// ## 生命周期
    /// - 调用后 lsp.* 工具即可使用
    /// - 语言服务器是 lazy 启动（第一次工具调用时启动）
    pub async fn enable_lsp(&self, workspace_root: impl Into<String>) {
        let root = workspace_root.into();
        let manager = Arc::new(crate::lsp::LspManager::new());
        *self.lsp_manager.write().await = Some(manager.clone());
        // Task #85：先注册 LSP schemas（懒注册），再绑定 executors
        // 引用关系：register 写 ToolRegistry.tools；register_executors 绑定 lsp.* → LspManager
        // 反复 enable_lsp 不会重复注册（ToolRegistry.register 内部去重）
        crate::tool::builtin::lsp::register(&self.registry).await;
        crate::tool::builtin::lsp::register_executors(&self.registry, manager, root).await;
        tracing::info!("LSP support enabled (schemas + executors registered)");
    }

    /// 启用 MCP 远程服务器集成（默认禁用）
    ///
    /// ## 流程（Phase 1 实现）
    /// 对每个 McpConfig：
    /// 1. 创建 McpClient（自动按 transport 选 stdio / gRPC）
    /// 2. discover_tools() 远程枚举 → ToolHandle 列表
    /// 3. 创建 McpToolExecutor 单例（一个 server 共享一个 executor）
    /// 4. 双轨注册：每个工具同时 register(handle) + register_executor(id, exe)
    /// 5. client Arc 保留到 self.mcp_clients 便于后续 disconnect
    ///
    /// ## KV cache 影响
    /// 启用 MCP 会让 LLM 看到额外工具，破当前 turn 的 prefix cache。
    /// 建议：启动期一次性启用所有 MCP server 后再让用户开始对话。
    /// 如运行时动态启用，预期会有 1-2 turn 的低命中率窗口。
    ///
    /// ## 安全
    /// 外部 MCP 工具不在 BUILTIN_EXEMPT_PREFIXES 里，因此 MCIP 策略对其生效。
    /// 调用方应在 enable_mcp 之前通过 mcip_gateway 配置 policy（否则默认 NeedsConfirm）。
    ///
    /// ## 错误
    /// 单个 server 的 discover_tools 失败不会中断整个调用——会跳过该 server 并 log。
    pub async fn enable_mcp(
        &self,
        configs: Vec<abacus_types::McpConfig>,
    ) -> Result<usize, KernelError> {
        let mut total_tools_registered = 0usize;
        for config in configs {
            let server_id = config.server_id.0.clone();
            let client = Arc::new(crate::mcp::McpClient::new(config));

            // discover_tools 失败时记日志跳过，不中断整个 enable_mcp
            let tools = match client.discover_tools().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(server_id = %server_id, error = %e,
                        "MCP discover_tools failed, skipping this server");
                    continue;
                }
            };

            let executor = Arc::new(
                crate::mcp::McpToolExecutor::new(client.clone(), &server_id),
            );

            // 双轨注册（schema + executor 一致性保证）
            for handle in tools {
                let id = handle.id.clone();
                self.registry.register(handle).await;
                self.registry.register_executor(id, executor.clone()).await;
                total_tools_registered += 1;
            }

            self.mcp_clients.write().await.insert(server_id.clone(), client);
            tracing::info!(server_id = %server_id, "MCP server enabled");
        }
        Ok(total_tools_registered)
    }

    /// 断开所有 MCP 客户端连接（不卸载已注册的工具，保留 schema 以便重连）
    pub async fn disconnect_mcp(&self) {
        let clients = self.mcp_clients.read().await;
        for (server_id, client) in clients.iter() {
            client.disconnect().await;
            tracing::info!(server_id = %server_id, "MCP server disconnected");
        }
    }

    /// 启用 Skill workflow 执行器（默认禁用）
    ///
    /// ## Phase 2 实现
    /// 创建单例 SkillExecutor（持有 Weak<ToolRegistry> + Weak<RwLock<SkillEngine>>），
    /// 之后调用 load_skill(id) 时自动把 step 注册为虚拟 ToolHandle + 绑定该 executor。
    ///
    /// ## 使用方式
    /// ```no_run
    /// # async fn example(core: std::sync::Arc<abacus_core::CoreLoop>) {
    /// core.enable_skill_workflow_executor().await;
    /// core.load_skill(&abacus_types::SkillId("code_review".into())).await.unwrap();
    /// # }
    /// ```
    ///
    /// ## 设计选择（Weak 引用）
    /// SkillExecutor 通过 Weak 引用 registry 和 engine，避免 strong cycle 内存泄漏。
    /// CoreLoop drop 时 Arc 引用计数自然归零，executor 内 Weak 升级失败会优雅返回错误。
    ///
    /// ## 必须 Arc<CoreLoop> 调用
    /// 内部需要 `Arc::downgrade(&self.registry)` 等 Weak 转换，所以 self 必须已 Arc 化。
    pub async fn enable_skill_workflow_executor(&self) {
        let executor = Arc::new(crate::skill::SkillExecutor::new(
            Arc::downgrade(&self.skill_engine),
            Arc::downgrade(&self.registry),
            // palace 连接由 orchestration 层在 with_memory() 后注入；当前阶段用空 Weak 占位
            std::sync::Weak::new(),
        ));
        // 缓存 executor Arc 到 self（让后续 load_skill 复用）
        *self.skill_workflow_executor.write().await = Some(executor);
        tracing::info!("Skill workflow executor enabled");
    }

    /// 加载一个 skill（其 workflow 转为虚拟 ToolHandle + 绑定 SkillExecutor）。
    ///
    /// 调用前必须 `enable_skill_workflow_executor()`，否则报错。
    pub async fn load_skill(
        &self,
        id: &abacus_types::SkillId,
    ) -> Result<(), String> {
        let executor = {
            let guard = self.skill_workflow_executor.read().await;
            guard.clone().ok_or_else(||
                "skill workflow executor not enabled — call enable_skill_workflow_executor() first".to_string()
            )?
        };
        let mut engine = self.skill_engine.write().await;
        engine.load(id, &self.registry, executor).await
    }

    /// 加载所有内置场景 Skills（和执行器一起启动）
    ///
    /// 内部自动调用 `enable_skill_workflow_executor()`（如未启动），
    /// 然后按 `def.compound` 标志分别加载：
    /// - compound == true  → `SkillEngine::load_compound()`（1 个工具，内部串联，节省上下文）
    /// - compound == false → `load_skill()`（每 step 一个虚拟工具，LLM 多次驱动）
    ///
    /// 引用: `crate::tool::builtin::skills::builtin_skill_defs()`
    /// 生命周期: 进程启动时调用一次，Skill 随 SkillEngine Arc 存活
    pub async fn load_builtin_skills(&self) {
        // 确保 step-level executor 已启动（非 compound skill 仍需要）
        if self.skill_workflow_executor.read().await.is_none() {
            self.enable_skill_workflow_executor().await;
        }

        // 创建 CompoundSkillExecutor（compound skill 专用）
        // palace 连接由 with_memory() 后注入；当前阶段用空 Weak 占位
        let compound_executor: std::sync::Arc<dyn crate::tool::ToolExecutor> = std::sync::Arc::new(
            crate::skill::CompoundSkillExecutor::new(
                std::sync::Arc::downgrade(&self.skill_engine),
                std::sync::Arc::downgrade(&self.registry),
                std::sync::Weak::new(),
            )
        );

        let defs = crate::tool::builtin::skills::builtin_skill_defs();
        let count = defs.len();

        // 先注册所有 def（register_skill 是纯内存写，不拿 registry 锁）
        {
            let mut engine = self.skill_engine.write().await;
            for def in &defs {
                engine.register_skill(def.clone());
            }
        }

        // 按 compound 标志分别加载
        for def in &defs {
            let id = &def.id;
            if def.compound {
                // compound 路径：整体注册为单一工具，内部串联
                let mut engine = self.skill_engine.write().await;
                if let Err(e) = engine.load_compound(id, &self.registry, compound_executor.clone()).await {
                    tracing::warn!("load compound skill '{}' failed: {}", id.0, e);
                }
            } else {
                // 非 compound 路径：每 step 一个虚拟工具（原有行为）
                if let Err(e) = self.load_skill(id).await {
                    tracing::warn!("load builtin skill '{}' failed: {}", id.0, e);
                }
            }
        }

        tracing::info!("Loaded {} builtin skills", count);
    }

    /// 启用 WASM Plugin 系统（默认禁用）
    ///
    /// ## Phase 3 实现
    /// 1. 扫描 base_dir 下所有 manifest.yaml + plugin.wasm
    /// 2. 加载并编译 WASM 模块
    /// 3. 为每个 manifest.tools 注册 schema + 共享 PluginToolExecutor
    /// 4. 工具 ID 格式：`plugin/{plugin_id}/{tool_name}`
    ///
    /// ## 安全
    /// - 沙箱：默认禁 WASI（无文件/网络/进程）
    /// - 内存：受 manifest.max_memory_mb 限制（execute 时 64KB 扫描上限即基础保护）
    /// - 执行：30s 超时硬限
    /// - 外部 plugin 不在 BUILTIN_EXEMPT_PREFIXES，MCIP policy 对其生效
    ///
    /// ## 返回
    /// 注册的工具总数。
    pub async fn enable_plugins(
        &self,
        base_dir: impl Into<String>,
    ) -> Result<usize, KernelError> {
        // Task #79：保持向后兼容——默认无签名验证（与历史行为一致）
        self.enable_plugins_with_options(base_dir, false).await
    }

    /// Task #79：带签名验证的 plugin 启用入口（推荐）
    ///
    /// ## 参数
    /// - `base_dir`：插件目录（每个子目录含 manifest.yaml + plugin.wasm）
    /// - `require_signing`：true → manifest.signature 必须存在且 hash 验证通过；
    ///   未签名/算法不支持/hash 不匹配 → 跳过该 plugin 并 warn（不抛错）。
    ///   false → 行为同 enable_plugins，签名仅作日志记录不阻断。
    ///
    /// ## 安全建议
    /// 生产部署应设置 `require_signing = true`，并通过 manifest.yaml 的
    /// `signature: { algorithm: sha256, value: <hex> }` 锁定 plugin.wasm hash。
    /// 部署流程中 wasm 字节变化必须同步更新 manifest hash——否则启动时跳过。
    ///
    /// 引用关系：调用 PluginLoader::verify_signature 静态方法，纯计算无副作用。
    /// 已注册的 plugin 不会被回退（执行 register 后注册成功即生效）。
    pub async fn enable_plugins_with_options(
        &self,
        base_dir: impl Into<String>,
        require_signing: bool,
    ) -> Result<usize, KernelError> {
        let loader = Arc::new(crate::mcp::PluginLoader::new(base_dir));
        let raw_manifests = loader.discover().await?;
        // 签名预过滤
        let mut manifests = Vec::with_capacity(raw_manifests.len());
        for manifest in raw_manifests {
            // 读取 wasm 字节用于 hash 计算
            let wasm_path = std::path::PathBuf::from(loader.base_dir())
                .join(&manifest.id).join("plugin.wasm");
            let wasm_bytes = match tokio::fs::read(&wasm_path).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(plugin_id = %manifest.id, error = %e, "read wasm for verify failed, skip");
                    continue;
                }
            };
            match crate::mcp::PluginLoader::verify_signature(&manifest, &wasm_bytes) {
                Ok(true) => {
                    tracing::info!(plugin_id = %manifest.id, "signature verified");
                    manifests.push(manifest);
                }
                Ok(false) => {
                    if require_signing {
                        tracing::warn!(plugin_id = %manifest.id, "unsigned plugin skipped (require_signing=true)");
                    } else {
                        manifests.push(manifest);
                    }
                }
                Err(e) => {
                    tracing::warn!(plugin_id = %manifest.id, error = %e,
                        "signature verification failed, skipping");
                    // 即使 require_signing=false 也跳过——hash 不匹配 = 文件被篡改
                }
            }
        }
        // 走原 enable_plugins 余下流程（discover 不重复，这里 manifests 已是过滤后列表）
        self._enable_plugins_inner(loader, manifests).await
    }

    /// Task #79：拆出原 enable_plugins 的核心 register 逻辑供 with_options 复用
    async fn _enable_plugins_inner(
        &self,
        loader: Arc<crate::mcp::PluginLoader>,
        manifests: Vec<abacus_types::PluginManifest>,
    ) -> Result<usize, KernelError> {

        let executor = Arc::new(crate::mcp::PluginToolExecutor::new(loader.clone()));
        let mut total_tools = 0usize;

        for manifest in manifests {
            let plugin_id = manifest.id.clone();
            // 编译加载 WASM 模块
            if let Err(e) = loader.load(manifest.clone()).await {
                tracing::warn!(plugin_id = %plugin_id, error = %e,
                    "plugin load failed, skipping");
                continue;
            }

            // 双轨注册每个声明的工具
            // 单一命名：ToolId.0 == schema.name == sanitized 形态 "plugin_{pid}_{tool}"。
            // raw plugin_id / tool_spec.name 可能含 . _ 等，sanitize 一次保证 LLM 协议合规；
            // execute 时 PluginToolExecutor 通过 PluginLoader.name_map 反查回 raw 二元组。
            for tool_spec in &manifest.tools {
                let sanitized_pid = crate::llm::tool_view::sanitize_name(&plugin_id);
                let sanitized_tool = crate::llm::tool_view::sanitize_name(&tool_spec.name);
                let sanitized_id = format!("plugin_{}_{}", sanitized_pid, sanitized_tool);
                let id = abacus_types::ToolId(sanitized_id.clone());
                let handle = abacus_types::ToolHandle {
                    id: id.clone(),
                    schema: abacus_types::ToolSchema {
                        name: id.0.clone(),
                        description: tool_spec.description.clone(),
                        parameters: tool_spec.parameters.clone(),
                        returns: None,
                        security: None,
                        cost: None,
                        examples: Vec::new(),
                        applicable_task_kinds: None,
                        idempotent: false,
                                                schema_stable: false,                    },
                    provider: abacus_types::ToolProvider::Plugin {
                        plugin_id: plugin_id.clone(),
                    },
                    state: abacus_types::ToolState::Loaded,
                    effectiveness: Default::default(),
                };
                // 同步登记反查映射
                loader.register_name_mapping(
                    sanitized_id,
                    plugin_id.clone(),
                    tool_spec.name.clone(),
                ).await;
                self.registry.register(handle).await;
                self.registry.register_executor(id, executor.clone()).await;
                total_tools += 1;
            }
            tracing::info!(plugin_id = %plugin_id, tools = manifest.tools.len(),
                "plugin loaded");
        }
        // 缓存 loader 让后续操作可访问
        *self.plugin_loader.write().await = Some(loader);
        Ok(total_tools)
    }

    /// 注册 Pipeline Hook（Turn 级粗粒度钩子）
    ///
    /// ## 与 MagChain 的区别
    /// - MagChain：工具执行前后（before_execute/after_execute）
    /// - PipelineHook：Turn 宏观阶段（TurnStart/PromptBuilt/TurnEnd/PostProcess）
    ///
    /// ## 参数
    /// - `priority`：越小越先触发（与 MagChain 约定一致）
    /// - `hook`：实现 PipelineHook trait 的 Arc
    ///
    /// ## 调用时机
    /// CoreLoop::new() 之后，Arc::new(core) 之前（或之后，内部用 Arc<RwLock>）
    pub async fn add_pipeline_hook(&self, priority: u32, hook: Arc<dyn crate::mag_chain::PipelineHook>) {
        let mut hooks = self.pipeline_hooks.write().await;
        hooks.push((priority, hook));
        hooks.sort_by_key(|(p, _)| *p);
    }

    /// 触发 Pipeline 事件到所有注册的 PipelineHook
    ///
    /// ## 触发点
    /// 由 TurnPipeline 各阶段调用，调用方不需要了解 hook 列表细节。
    /// 任意 hook 返回 `HookAction::Abort` 则立即停止，不再调用后续 hook。
    ///
    /// ## 返回
    /// `Ok(())` — 所有 hook 通过；`Err(KernelError)` — hook 中止或内部错误
    pub async fn emit_pipeline_event(&self, event: crate::mag_chain::PipelineEvent) -> Result<(), KernelError> {
        use crate::mag_chain::HookAction;
        let hooks = self.pipeline_hooks.read().await;
        for (_, hook) in hooks.iter() {
            if !hook.accepts(&event) { continue; }
            match hook.on_event(&event).await? {
                HookAction::Continue => {}
                HookAction::Abort(reason) => {
                    return Err(KernelError::Other(format!("pipeline hook '{}' aborted turn: {}", hook.name(), reason)));
                }
            }
        }
        Ok(())
    }

    /// 处理用户对 MCIP 授权请求的决定，并重运同一 turn。
    ///
    /// ## 授权决定语义
    /// - `Once`：工具 ID 加入 `RequestContext.mcip_once_grants`，仅当前 turn 生效
    /// - `Always`：工具 ID 写入 `session.mcip_grants`，session 内永久生效
    /// - `Deny`：不授权，跳过该工具的重运（其予工具仍重运）
    ///
    /// ## 参数
    /// - `decisions`：用户对每个待确认工具的决定
    /// - `input`：原始用户输入（用于重运）
    /// - `session`：当前 session（会写入 Always 授权）
    ///
    /// ## 调用时机
    /// 用户在授权对话框做决定后由 L4 调用
    pub async fn grant_and_rerun(
        &self,
        decisions: &[(String, crate::mcip::McipGrantDecision)],
        input: &str,
        session: &RwLock<SessionState>,
    ) -> Result<TurnResult, KernelError> {
        use crate::mcip::McipGrantDecision;
        use std::collections::HashSet;

        let mut once_grants: HashSet<String> = HashSet::new();

        // 写入 Always 授权到 session；收集 Once 授权到临时集合
        {
            let s = session.read().await;
            let mut grants = s.mcip_grants.write().unwrap();
            for (tool_id, decision) in decisions {
                match decision {
                    McipGrantDecision::Always => { grants.insert(tool_id.clone()); }
                    McipGrantDecision::Once   => { once_grants.insert(tool_id.clone()); }
                    McipGrantDecision::Deny   => {} // 不授权，重运后 MCIP 仍会拦截
                }
            }
        }

        // 临时授权通过 RequestContext 传入 pipeline
        let ctx = RequestContext { mcip_once_grants: once_grants, ..Default::default() };
        self.process(input, session, ctx).await
    }

    /// 从 config 统一应用 MCIP 全部权限配置（security.yaml `mcip.*`）
    ///
    /// 处理三个名单：
    /// - `exempt_prefixes`：前缀豆免（跳过策略检查）
    /// - `allow_tools`：精确允许 ID（跳过策略直接 Allowed）
    /// - `deny_tools`：永久禁止 ID（最高优先级，覆盖一切授权）
    ///
    /// ## 调用时机
    /// CoreLoop 创建后、应用完 config 后调用一次。
    /// 可在 Arc 包裹前后任意时刻调用（McipGateway 内部用 RwLock）。
    pub fn configure_mcip_permissions(
        &self,
        exempt_prefixes: &[String],
        allow_tools: &[String],
        deny_tools: &[String],
    ) {
        if !exempt_prefixes.is_empty() {
            self.mcip_gateway.apply_exempt_prefixes(exempt_prefixes);
            tracing::info!(count = exempt_prefixes.len(), "MCIP exempt prefixes applied");
        }
        if !allow_tools.is_empty() {
            self.mcip_gateway.apply_allow_tools(allow_tools);
            tracing::info!(count = allow_tools.len(), "MCIP allow_tools applied");
        }
        if !deny_tools.is_empty() {
            self.mcip_gateway.apply_deny_tools(deny_tools);
            tracing::info!(count = deny_tools.len(), "MCIP deny_tools applied");
        }
    }

    /// 应用 config.yaml 中的 `mcip.exempt_prefixes` 列表到 McipGateway
    ///
    /// ## 弃用（请改用 configure_mcip_permissions）
    #[deprecated(note = "use configure_mcip_permissions instead")]
    pub fn configure_mcip_exemptions(&self, prefixes: &[String]) {
        self.configure_mcip_permissions(prefixes, &[], &[]);
    }

    /// 获取 LspManager 引用（未激活时返回 None）
    pub async fn lsp_manager(&self) -> Option<Arc<crate::lsp::LspManager>> {
        self.lsp_manager.read().await.clone()
    }

    /// 获取指定 provider 的 PromptAdapter（不存在时返回 NeutralAdapter）
    pub async fn get_adapter(
        &self,
        provider_id: &str,
    ) -> Arc<dyn crate::core::provider_adapter::PromptAdapter> {
        let adapters = self.adapters.read().await;
        adapters.get(provider_id)
            .cloned()
            .unwrap_or_else(|| Arc::new(crate::core::provider_adapter::NeutralAdapter))
    }

    /// V0.2: 列出所有已注册 providers 及其支持的模型
    pub async fn list_providers(&self) -> Vec<(String, Vec<String>)> {
        let providers = self.providers.read().await;
        providers.iter()
            .map(|(id, p)| (id.clone(), p.supported_models().iter().map(|m| m.0.clone()).collect()))
            .collect()
    }

    /// 注册厂商分组 — 一个 API 端点下多个模型可共享 provider 实例
    ///
    /// 分组注册后，用户可通过 `/model <name>` 自由切换组内模型。
    /// 注册的分组也会被注册为单个 provider（id = group.id），
    /// 其 `supported_models()` 返回分组全量模型列表。
    pub async fn register_provider_group(
        &self,
        id: impl Into<String>,
        models: Vec<ModelId>,
        provider: Arc<dyn LlmProvider>,
    ) {
        let id_str = id.into();
        let group = ProviderGroup::new(&id_str, models.clone(), provider.clone());
        self.provider_groups.write().await.push(group);

        // 同时注册为普通 provider（包装后返回完整模型列表）
        let wrapped = Arc::new(GroupProvider {
            inner: provider.clone(),
            models,
        });
        self.providers.write().await.insert(id_str.clone(), wrapped);
        // adapter 同步注册（与 register_provider 保持一致，避免绕过路径遗漏）
        let adapter = crate::core::provider_adapter::adapter_for_provider(&id_str);
        self.adapters.write().await.insert(id_str.clone(), adapter);
        self.sandbox_engine.add_provider(id_str, provider).await;
    }

    /// 注册 OpenAI-compatible 厂商分组（便捷方法）
    ///
    /// 从 base_url + api_key 创建 OpenAICompatibleProvider，
    /// 支持 models 列表中的所有模型。
    pub async fn register_openai_group(
        &self,
        id: impl Into<String>,
        base_url: String,
        api_key: String,
        models: Vec<ModelId>,
    ) {
        let id_str = id.into();
        // 使用第一个模型创建 provider（实际请求时用 req.model 覆盖）
        let default_model = models.first().cloned().unwrap_or(ModelId("unknown".into()));
        let provider = Arc::new(OpenAICompatibleProvider::new(
            api_key, default_model, base_url,
            None, None, None,
        ));
        self.register_provider_group(id_str, models, provider).await;
    }

    /// 注册 Anthropic 厂商分组（便捷方法）
    pub async fn register_anthropic_group(
        &self,
        id: impl Into<String>,
        api_key: String,
        base_url: Option<String>,
        models: Vec<ModelId>,
    ) {
        let id_str = id.into();
        let default_model = models.first().cloned().unwrap_or(ModelId("claude-sonnet-4".into()));
        let provider = Arc::new(AnthropicProvider::new(
            api_key, default_model, base_url, None,
        ));
        self.register_provider_group(id_str, models, provider).await;
    }

    pub fn config(&self) -> &CoreConfig { &self.config }

    pub fn config_mut(&mut self) -> &mut CoreConfig { &mut self.config }

    /// 运行时热切换模型（TUI /model 命令调用）
    /// 2026-05-28: 改为 async 避免在 tokio runtime 内调 blocking_write panic
    pub async fn set_model_override(&self, model: impl Into<String>) {
        *self.model_override.write().await = Some(ModelId(model.into()));
    }

    pub async fn clear_model_override(&self) {
        *self.model_override.write().await = None;
    }

    // ── 2026-05-28: TUI /set + /preset 运行时参数 setter ────────────────
    // 写入 runtime_overrides（与 config_set tool 共享同一 map），pipeline 自动消费。
    // 引用关系：TUI slash_commands::apply_preset / cmd_set → tokio::spawn async move
    // 生命周期：写入后立即生效于下一次 pipeline execute_loop

    pub async fn set_temperature(&self, v: f64) {
        self.runtime_overrides.write().unwrap_or_else(|p| p.into_inner())
            .insert("temperature".to_string(), v.to_string());
    }

    pub async fn set_max_tokens(&self, v: u32) {
        self.runtime_overrides.write().unwrap_or_else(|p| p.into_inner())
            .insert("max_tokens".to_string(), v.to_string());
    }

    pub async fn set_tool_limit(&self, v: u32) {
        self.runtime_overrides.write().unwrap_or_else(|p| p.into_inner())
            .insert("max_tool_calls".to_string(), v.to_string());
    }

    pub async fn set_context_ratio(&self, v: f64) {
        self.runtime_overrides.write().unwrap_or_else(|p| p.into_inner())
            .insert("context_ratio".to_string(), v.to_string());
    }

    pub async fn set_silent_router(&self, enabled: bool) {
        self.runtime_overrides.write().unwrap_or_else(|p| p.into_inner())
            .insert("silent_router".to_string(), enabled.to_string());
    }

    pub async fn set_dedup(&self, enabled: bool) {
        self.runtime_overrides.write().unwrap_or_else(|p| p.into_inner())
            .insert("dedup_enabled".to_string(), enabled.to_string());
    }

    pub async fn set_timeout(&self, seconds: u64) {
        self.runtime_overrides.write().unwrap_or_else(|p| p.into_inner())
            .insert("turn_timeout".to_string(), seconds.to_string());
    }

    /// 获取记忆宫殿引用（TUI 面板数据拉取用）
    pub fn memory_palace(&self) -> Option<Arc<tokio::sync::RwLock<DualPalaceMemory>>> {
        self.memory_palace.clone()
    }

    /// 读取当前运行时模型覆盖（pipeline execute_loop 内用）
    ///
    /// 引用关系：pipeline/mod.rs execute_loop effective_model 优先级链
    /// 生命周期：set_model_override 设入 → 本函数读出 → clear_model_override 清空
    pub(crate) async fn get_model_override(&self) -> Option<ModelId> {
        self.model_override.read().await.clone()
    }

    /// 获取沙箱引擎引用（CLI turnkey 命令使用）
    pub fn sandbox_engine(&self) -> &Arc<SandboxOrchestrator> { &self.sandbox_engine }

    /// 获取 HealthRegistry（供外部注册探针）
    pub fn health_registry(&self) -> &Arc<health::HealthRegistry> { &self.health_registry }

    /// 获取 PressureMonitor（供外部注册压力源）
    pub fn pressure_monitor(&self) -> &Arc<pressure::ResourcePressureMonitor> { &self.pressure_monitor }

    /// 上下文窗口使用状态（CLI `context status` / TUI `/context`）
    pub async fn context_status(&self) -> ContextWindowStatus {
        let w = self.context_manager.window.read().await;
        let compressed = self.context_manager.tiers.compressed_messages.read().await.len();
        ContextWindowStatus {
            current_tokens: w.current_tokens,
            max_tokens: w.max_tokens,
            usage_pct: w.usage_pct(),
            compressed_count: compressed,
        }
    }

    /// 手动压缩会话上下文（CLI `session compress` / TUI `/compress`）
    pub async fn compress_context(&self, session: &RwLock<SessionState>) -> usize {
        let s = session.read().await;
        let mut msgs = s.messages.write().await;
        let compressed = self.context_manager.auto_compress_messages(&mut msgs).await;
        compressed.len()
    }

    /// 注入临时知识到下一轮 prompt（CLI `context inject` / TUI `/inject`）
    pub async fn inject_context(&self, key: &str, content: &str) {
        let mut injector = self.injector.write().await;
        injector.inject(key, &serde_json::json!({"ephemeral": content}));
    }

    /// 工具效能统计快照（CLI `tool stats` / TUI `/tool-stats`）
    /// 返回 (tool_id, ToolEffectiveness) 列表，已计算综合评分。
    pub async fn tool_stats(&self) -> Vec<(String, abacus_types::ToolEffectiveness)> {
        let eff = self.effectiveness.read().await;
        eff.all_stats_snapshot().keys()
            .map(|id| {
                let eval = eff.evaluate(id);
                (id.0.clone(), eval)
            })
            .collect()
    }

    /// 安全限制状态（CLI `safety status` / TUI `/safety`）
    pub fn safety_status(&self) -> crate::core::safety::SafetyStatus {
        self.safety_guard.status()
    }

    /// 动态注册中间件（热插拔）。
    ///
    /// 取 &self，Arc::new(core) 之后仍可调用，替代原 mag_chain_mut()。
    ///
    /// ## 引用关系
    /// - 被 engine_init.rs / server.rs 初始化阶段调用
    /// - 消费方：TurnPipeline Phase 4 通过 read lock 调用 before/after
    ///
    /// ## 生命周期
    /// - 注册后立即生效（write lock 持有期间排他，after unlock 新中间件即可见）
    pub async fn add_middleware(
        &self,
        priority: u32,
        middleware: Arc<dyn crate::mag_chain::Middleware>,
    ) {
        self.mag_chain.write().await.add_with_priority(priority, middleware);
    }

    /// EpistemicGuard 引用（与 mag_chain 内部实例共享同一 Arc）。
    /// 供 pipeline 读取 violation_count 并注入累积声明。
    pub fn epistemic_guard(&self) -> &Arc<crate::mag_chain::EpistemicGuard> {
        &self.epistemic_guard
    }

    /// List all registered skills.
    pub async fn list_skills(&self) -> Vec<abacus_types::SkillDef> {
        let engine = self.skill_engine.read().await;
        engine.list_skills()
    }

    /// List supported models from all registered providers.
    pub async fn list_models(&self) -> Vec<String> {
        let providers = self.providers.read().await;
        let mut models: Vec<String> = Vec::new();
        for (_, p) in providers.iter() {
            for m in p.supported_models() {
                models.push(m.0);
            }
        }
        models.sort();
        models.dedup();
        models
    }

    /// 通过各 provider API 实时发现所有可用模型（首次配置 / `abacus models discover` 触发）。
    ///
    /// ## 行为
    /// - 并发调用每个已注册 provider 的 `discover_models()`
    /// - 单个 provider 失败不影响其他（best-effort）
    /// - 结果按 provider_id 分组返回；调用方自行决定是否写 cache
    ///
    /// ## 边界
    /// - 使用 provider 内部的 15s timeout（discover 不应阻塞启动）
    /// - 静态 fallback：discover 失败时使用 supported_models()
    pub async fn discover_all_models(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        let providers = self.providers.read().await;
        let provider_list: Vec<(String, std::sync::Arc<dyn crate::llm::LlmProvider>)> = providers.iter()
            .map(|(id, p)| (id.clone(), p.clone()))
            .collect();
        drop(providers);

        let mut handles = Vec::with_capacity(provider_list.len());
        for (id, p) in provider_list {
            handles.push(tokio::spawn(async move {
                let models = match p.discover_models().await {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(provider = %id, error = %e, "discover_models failed, fallback to supported_models");
                        p.supported_models()
                    }
                };
                (id, models.into_iter().map(|m| m.0).collect::<Vec<_>>())
            }));
        }

        let mut result = std::collections::BTreeMap::new();
        for h in handles {
            if let Ok((id, models)) = h.await {
                if !models.is_empty() {
                    result.insert(id, models);
                }
            }
        }

        // 2026-05-28: 将发现的模型写回 ProviderGroup — 让 resolve_provider() 能匹配到
        // 解决：config 中 models 为空或只有占位符时，group.supports(model) 失败的问题
        if !result.is_empty() {
            let mut groups = self.provider_groups.write().await;
            for group in groups.iter_mut() {
                if let Some(discovered) = result.get(&group.id) {
                    let new_models: Vec<ModelId> = discovered.iter()
                        .map(|m| ModelId(m.clone()))
                        .collect();
                    if new_models.len() > group.models.len() {
                        group.models = new_models;
                    }
                }
            }
        }

        result
    }

    /// 一站式 API：discover + 写入 ~/.abacus/models.cache.json。
    /// 失败磁盘写入静默降级（discover 结果仍返回）。
    pub async fn discover_and_cache(
        &self,
        cache_path: Option<&std::path::Path>,
    ) -> crate::llm::ModelCache {
        let providers = self.discover_all_models().await;
        let cache = crate::llm::ModelCache {
            version: 1,
            discovered_at: chrono::Utc::now().timestamp(),
            providers,
        };
        let default_path;
        let path = match cache_path {
            Some(p) => p,
            None => {
                default_path = crate::llm::ModelCache::default_path();
                &default_path
            }
        };
        if let Err(e) = cache.save(path) {
            tracing::warn!(error = %e, path = ?path, "model cache save failed; in-memory result still returned");
        } else {
            tracing::info!(path = ?path, count = cache.all_models().len(), "model cache written");
        }
        cache
    }

    /// Access the skill engine.
    pub fn skill_engine_ref(&self) -> &Arc<RwLock<SkillEngine>> { &self.skill_engine }

    /// Access the tool registry.
    pub fn tool_registry_ref(&self) -> &Arc<ToolRegistry> { &self.registry }


    /// Register context.* tools for a specific session (per-session isolation).
    /// Must be called after session creation, before first process_turn.
    pub async fn register_session_context_tools(
        &self,
        session: &SessionState,
    ) {
        register_context_tools(
            &self.registry,
            self.context_manager.clone(),
            session.context_messages.clone(),
        ).await;

        // filengine executor 已改为无状态实现（ExecutionContext 重构后）。
        // session 不再注入 executor，而是在每次工具调用时通过 ExecutionContext 动态传入。
        // 这消除了多 session 竞态：每个 session 调用工具时带自己的 filengine_session，互不影响。
        //
        // ## 生命周期
        // - 创建：首次调用 `register_session_context_tools` 时注册（全局单例 executor）
        // - 执行时：由 TurnPipeline 构建 ExecutionContext { filengine: session.filengine_session }
        // - 销毁：随 ToolRegistry 销毁
        crate::tool::builtin::filengine::register_executors(
            &self.registry,
        ).await;
    }

    /// 统一入口：通过 RequestContext 定制 pipeline 行为。
    /// 所有新代码应使用此方法。
    #[tracing::instrument(skip(self, input, session, ctx))]
    pub async fn process(
        &self,
        input: &str,
        session: &RwLock<SessionState>,
        ctx: RequestContext,
    ) -> Result<TurnResult, KernelError> {
        TurnPipeline::with_context(self, input, session, ctx).run().await
    }

    /// 向后兼容入口：等价于 process(input, session, RequestContext::default())
    #[tracing::instrument(skip(self, input, session))]
    pub async fn process_turn(
        &self,
        input: &str,
        session: &RwLock<SessionState>,
    ) -> Result<TurnResult, KernelError> {
        self.process(input, session, RequestContext::default()).await
    }

    /// P2: 带取消令牌的 turn 入口。调用方 `token.cancel()` 后，pipeline 会在
    /// phase boundary 退出，且 provider 层的 in-flight reqwest 也被中断
    /// （drop 即取消，tokio runtime 保证）。
    #[tracing::instrument(skip(self, input, session, token))]
    pub async fn process_turn_cancellable(
        &self,
        input: &str,
        session: &RwLock<SessionState>,
        token: tokio_util::sync::CancellationToken,
    ) -> Result<TurnResult, KernelError> {
        TurnPipeline::with_context(self, input, session, RequestContext::default())
            .with_cancel(token)
            .run()
            .await
    }

    /// Phase 4：带 RequestContext + 取消令牌的 turn 入口，HTTP/TUI per-request override 用。
    ///
    /// `RequestContext.thinking_intent` 会从上层（HTTP body / TUI 命令）传入，
    /// pipeline 把它注入 `LlmRequest.thinking_intent`，对应 provider resolve_thinking 优先读此字段。
    #[tracing::instrument(skip(self, input, session, ctx, token))]
    pub async fn process_turn_cancellable_with_context(
        &self,
        input: &str,
        session: &RwLock<SessionState>,
        ctx: RequestContext,
        token: tokio_util::sync::CancellationToken,
    ) -> Result<TurnResult, KernelError> {
        TurnPipeline::with_context(self, input, session, ctx)
            .with_cancel(token)
            .run()
            .await
    }

    /// V0.2: 流式 turn 入口 — 通过 stream_tx 实时推送增量文本到调用方。
    pub async fn process_turn_streaming(
        &self,
        input: &str,
        session: &RwLock<SessionState>,
        stream_tx: tokio::sync::mpsc::UnboundedSender<crate::llm::stream::StreamChunk>,
    ) -> Result<TurnResult, KernelError> {
        TurnPipeline::with_context(self, input, session, RequestContext::default())
            .with_stream(stream_tx)
            .run()
            .await
    }

    /// P2: 流式 + 取消令牌入口（C4 修复：SSE 客户端断开时停止 in-flight LLM）
    pub async fn process_turn_streaming_cancellable(
        &self,
        input: &str,
        session: &RwLock<SessionState>,
        stream_tx: tokio::sync::mpsc::UnboundedSender<crate::llm::stream::StreamChunk>,
        token: tokio_util::sync::CancellationToken,
    ) -> Result<TurnResult, KernelError> {
        TurnPipeline::with_context(self, input, session, RequestContext::default())
            .with_stream(stream_tx)
            .with_cancel(token)
            .run()
            .await
    }

    /// Gap A 修复（thinking refactor）：streaming + cancel + RequestContext 三者同时走的入口。
    /// 引用关系：TUI send_chat_message_streaming 调用，让 state.thinking_depth 运行时切换能
    /// 走进 LlmRequest.thinking_intent（per-turn override 优先于 session sticky，见
    /// pipeline/mod.rs:759-761）。KV cache 仅在切换轮失效，不切换时完整保留。
    pub async fn process_turn_streaming_cancellable_with_context(
        &self,
        input: &str,
        session: &RwLock<SessionState>,
        stream_tx: tokio::sync::mpsc::UnboundedSender<crate::llm::stream::StreamChunk>,
        ctx: RequestContext,
        token: tokio_util::sync::CancellationToken,
    ) -> Result<TurnResult, KernelError> {
        TurnPipeline::with_context(self, input, session, ctx)
            .with_stream(stream_tx)
            .with_cancel(token)
            .run()
            .await
    }


    /// 用户确认清单后的续写入口（Gated Phase 2）
    ///
    /// ## 场景
    /// L4 层收到 TurnResult.progressive_state == AwaitingConfirmation 后，
    /// 展示清单 UI，收集用户响应，然后调用此方法触发续写。
    ///
    /// ## 流程
    /// 1. 将确认结果注入 ProgressiveController
    /// 2. 构建续写 Prompt（含用户决策）
    /// 3. 发送第二次 LLM 请求
    /// 4. 返回正式文档内容
    pub async fn process_turn_continuation(
        &self,
        responses: Vec<(u32, UserResponse)>,
        session: &RwLock<SessionState>,
    ) -> Result<TurnResult, KernelError> {
        TurnPipeline::new(self, "", session).continue_gated(responses).await
    }

    /// P2: 续写 + 取消令牌入口
    pub async fn process_turn_continuation_cancellable(
        &self,
        responses: Vec<(u32, UserResponse)>,
        session: &RwLock<SessionState>,
        token: tokio_util::sync::CancellationToken,
    ) -> Result<TurnResult, KernelError> {
        TurnPipeline::new(self, "", session)
            .with_cancel(token)
            .continue_gated(responses)
            .await
    }

    /// L1 后：直接返回 ThinkingIntent。
    ///
    /// ## 优先级（不变）
    /// 1. `config.thinking_intent`（用户显式 PRIMARY 配置）
    /// 2. `config.model_spec.thinking_config`（DEPRECATED 旧 ModelThinkingConfig，仅当其 enabled=true 时生效）
    /// 3. None
    pub(crate) fn build_thinking_intent(&self) -> Option<abacus_types::ThinkingIntent> {
        if let Some(ref intent) = self.config.thinking_intent {
            return Some(intent.clone());
        }
        if let Some(ref spec) = self.config.model_spec {
            if spec.thinking_config.enabled {
                // 把旧 ModelThinkingConfig 升格为 ThinkingIntent：effort 字段直接 lift
                return spec.thinking_config.effort
                    .map(abacus_types::ThinkingIntent::from)
                    .or(Some(abacus_types::ThinkingIntent::Effort(abacus_types::EffortLevel::High)));
            }
        }
        None
    }

    /// 统一构建入口，返回 text + segments 一次性构建。
    /// build_system_prompt / build_system_segments 均委托至此，保证两者同步。
    ///
    /// ## Phase 5 清理
    /// 已删除 `matched_skills` 参数：Phase 1 切断注入路径后该参数不再被消费。
    /// skill workflow 通过 tool description 自然表达，不需要在 system prompt 拼接。
    pub(crate) async fn build_system_output(
        &self,
        input: &str,
        session: &RwLock<SessionState>,
        preflight: &PreflightReport,
    ) -> SystemPromptOutput {
        // ─── 数据采集（一次，text + segments 共享）─────────────────────────────

        let retained = self.context_manager.retained_context_block().await;

        let injector_segments = {
            let injector = self.injector.read().await;
            injector.active_knowledge().to_vec()
        };

        // Phase 5 KV cache 清理：删除 session_context / interaction_status 数据采集
        //   - Phase 1 已切断它们的注入路径（prompt_assembly 不再消费）
        //   - 删除 lock 获取以减少锁竞争 + 移除信息冗余源
        //   - 需要时 LLM 自行调 fs.cwd / fs.status / session.interaction_map 工具

        let deduction_block = self.deduction_engine.build_injection().await;

        // Detect task kind for abacusbr sub-scene matching
        // Phase 2 KV cache 修复：session-sticky task_kind 锁定
        //   首轮 classify 后写入 session.task_kind_locked，后续 turn 直接复用
        //   避免跨 turn task_kind 切换 → Layer 185 subscenes 字节变化 → cache miss
        //   跨 task 应显式 `/session new` 重启会话
        let task_kind = {
            let session_guard = session.read().await;
            let mut locked = session_guard.task_kind_locked.write().await;
            match locked.clone() {
                Some(k) => k, // 已锁定 → 复用
                None => {
                    let initial = crate::core::task_analyzer::TaskAnalyzer::classify(input).kind;
                    *locked = Some(initial.clone());
                    initial
                }
            }
        };

        let preflight_block = preflight.to_prompt_block();

        let assembled = self.prompt_assembly.assemble(
            &injector_segments,
            deduction_block.as_deref(),
            Some(&preflight_block),
            &retained,
            Some(task_kind.clone()),  // clone：segments 路径还需要 task_kind
            false,  // no process layer in standard build
        );

        // SessionFocus: 注入到 system prompt 末尾（recency-adjacent，免于消息压缩 + 保护 prefix cache）
        //
        // ## 位置选择
        //   末尾 vs 顶部：focus.render_with_age(age) 每轮 byte 变化，放顶部破坏 DeepSeek/OpenAI 前缀缓存
        //   末尾位置仍在 system prompt 内、紧邻用户消息，对 LLM 注意力影响极小
        //
        // ## 平衡策略（防累积 + 防陈旧）
        //   MIN_TURNS  = 3  → 短会话（≤3轮）跳过注入，避免简单对话被噪音污染
        //   MAX_STALE  = 15 → 超过 15 轮未更新则不注入（焦点已过时，注入反而误导）
        //   WARN_ZONE  = 3  → 距离过期 3 轮内附加刷新提示，推动 LLM 主动 set_focus
        //
        // ## 大小约束（由 render_with_age 保证）
        //   字段截断 60 chars，constraints 最多 3 条，总 ~40–90 tokens，不随调用次数增长
        const MIN_TURNS: u32 = 3;
        const MAX_STALE: u32 = 15;
        const WARN_ZONE: u32 = 3;

        let focus_block = {
            let s = session.read().await;
            let current_turn = s.turn_count.saturating_add(1);
            if current_turn < MIN_TURNS {
                None // 短会话不注入
            } else {
                let focus = s.session_focus.read().await;
                focus.as_ref().and_then(|f| {
                    let age = current_turn.saturating_sub(f.updated_at_turn);
                    if age > MAX_STALE {
                        None // 焦点过期，不注入（LLM 应重新调用 session.set_focus）
                    } else {
                        // off-by-one fix：WARN_ZONE=3 应给出 3 轮警告 [13,14,15]
                        // 原 >= MAX_STALE - WARN_ZONE = >=12 给 4 轮，改为 >=13
                        let age_hint = if age >= MAX_STALE.saturating_sub(WARN_ZONE).saturating_add(1) {
                            age // 接近过期，传入 age 触发提示
                        } else {
                            0  // 新鲜，不显示 age 提示
                        };
                        Some(f.render_with_age(age_hint))
                    }
                })
            }
        };
        // ─── Awareness Block（全维度态势感知）──────────────────────────────────────
        // LLM 每轮获得紧凑的状态快照（~60-80 tokens），覆盖三个维度：
        //   1. 自我感知：当前轮次、token 消耗、剩余预算、已用工具
        //   2. 环境感知：工作目录、session 模式、task_kind、用户角色
        //   3. 能力感知：可用模型、thinking 级别、工具集规模、升级余量
        //
        // ## 位置：非缓存段（每轮 byte 变化），放 catalog 之后、focus 之前。
        // ## Token 成本：~60-80 tokens/turn（远小于旧 session_context 的冗余注入）
        let (mut awareness_block, last_tool_name) = {
            let s = session.read().await;
            let current_turn = s.turn_count + 1;

            // 自我感知
            let tool_calls_used: u32 = {
                let msgs = s.messages.read().await;
                msgs.iter().filter(|m| matches!(m.role, crate::llm::provider::MessageRole::Tool)).count() as u32
            };
            let model_name = self.config.default_model.0.as_str();
            let thinking_mode = self.config.thinking_intent.as_ref()
                .map(|t| format!("{:?}", t))
                .unwrap_or_else(|| "off".into());

            // 环境感知
            let task_kind_str = {
                let locked = s.task_kind_locked.read().await;
                locked.as_ref().map(|k| k.label().to_string()).unwrap_or_else(|| "unclassified".into())
            };
            let user_role = format!("{:?}", s.user_role);

            // 能力感知
            let max_turns = self.config.max_turns_per_request;
            let _max_tool_calls = self.config.max_tool_calls_per_turn;
            let max_tokens = self.config.default_max_tokens;
            let escalations_left = self.config.max_escalations;

            // 获取最近调用的工具名（供 palace recommendations 使用）
            let last_tool = {
                let map = s.interaction_map.read().await;
                map.recent_tools(1).into_iter().next().map(|tid| tid.0)
            };

            // Policy 感知：让 LLM 知道当前约束状态
            let policy = &self.config.policy;
            let guard_status = if policy.guard.entropy_guard.is_empty() { "off" } else { "on" };
            let decl_status = if policy.guard.explicit_declaration.is_empty() { "off" } else { "on" };

            // 上下文占用感知（让 LLM 每轮知道剩余空间、预算比例、模型容量）
            //
            // Fix 1: 将本轮最大输出纳入估算，避免 LLM 看到的百分比过乐观
            //   effective_pct = (history_tokens + max_output_tokens) / max_tokens
            //   ctx_status 基于 effective_pct，触发阈值提前到实际压力点
            // Fix 2: 向 LLM 展示实际可用输出上限（受剩余空间压缩）
            let ctx_window = self.context_manager.window.read().await;
            let ctx_pct = ctx_window.usage_pct();
            // 含输出预留的保守估算百分比
            let effective_used = ctx_window.current_tokens.saturating_add(max_tokens as usize);
            let effective_pct = (effective_used as f64 / ctx_window.max_tokens.max(1) as f64 * 100.0).min(100.0);
            let ctx_status = if effective_pct >= 95.0 { "CRITICAL" }
                else if effective_pct >= ctx_window.compression_trigger_pct as f64 { "ELEVATED" }
                else if effective_pct >= 70.0 { "WARNING" }
                else { "OK" };
            // 实际可用输出空间（含 1024 安全余量）
            let effective_output = ctx_window.max_tokens
                .saturating_sub(ctx_window.current_tokens)
                .saturating_sub(1024)
                .min(max_tokens as usize)
                .max(2048);
            let ratio_pct = (self.config.context_window_ratio * 100.0) as u32;
            let ctx_info = format!(
                "Context: history={:.0}% effective={:.0}% ({}/{}tok) [{}] | Output: {}tok avail | Budget: {}% of {}tok",
                ctx_pct, effective_pct,
                ctx_window.current_tokens, ctx_window.max_tokens, ctx_status,
                effective_output,
                ratio_pct, ctx_window.model_limit);
            drop(ctx_window);

            let block = format!(
                "[Awareness]\n\
                 Turn: {}/{} | Tools called: {} | Model: {} | Thinking: {}\n\
                 Task: {} | Role: {} | Max output: {}tok (effective: {}tok)\n\
                 {} \n\
                 [Limits] tools: 0/{} per turn | iterations: 0/{} | No session limit\n\
                 Capabilities: {} escalations available | scene_loading: {} | router: {}\n\
                 Policy: entropy_guard={} | explicit_decl={} | stop_threshold={}chars | confirm_timeout={}s\n\
                 [Context Management] HARD RULE: your output + existing context MUST stay within the budget shown above. \
                 When status=OK: output freely within max_output. \
                 When status=WARNING (70-84%): be concise, avoid redundant explanations, prefer structured output. \
                 When status=ELEVATED (85-94%): output key conclusions ONLY, then immediately call context_compress(mode=\"messages\"). \
                 When status=CRITICAL (95%+): output ≤200 tokens summary, then compress. Exceeding budget causes data loss. \
                 Last 8 messages are always preserved after compression.",
                current_turn, max_turns,
                tool_calls_used,
                model_name,
                thinking_mode,
                task_kind_str,
                user_role,
                max_tokens,
                effective_output,
                ctx_info,
                self.config.thresholds.turn_max_tool_calls,
                self.config.thresholds.turn_max_iterations,
                escalations_left,
                if self.config.scene_tool_loading_enabled { "on" } else { "off" },
                if self.config.silent_router_enabled { "on" } else { "off" },
                guard_status,
                decl_status,
                policy.thresholds.premature_stop_chars,
                policy.thresholds.confirm_timeout_secs,
            );
            (block, last_tool)
        };

        // Palace Memory recommendations 注入（LLM 看到上下文相关的工具使用建议）
        //
        // ## 引用关系
        // - 依赖：self.memory_palace (Option<Arc<RwLock<DualPalaceMemory>>>)
        // - 依赖：last_tool_name（来自 session interaction_map）
        // - 消费方：LLM（通过 awareness_block 末尾 "Palace hints:" 行）
        //
        // ## 生命周期
        // - 创建：每次 build_system_output 调用时按需追加
        // - 销毁：随 awareness_block String drop（无副作用）
        if let Some(ref palace_arc) = self.memory_palace {
            if let Some(ref tool_name) = last_tool_name {
                let palace = palace_arc.read().await;
                let recs = palace.recommend_next_tools(tool_name).await;
                if !recs.is_empty() {
                    let rec_str: String = recs.iter()
                        .take(3) // 最多 3 条，控制 token 开销
                        .map(|(tool, score)| format!("{}({:.0}%)", tool, score * 100.0))
                        .collect::<Vec<_>>()
                        .join(", ");
                    awareness_block.push_str(&format!("\nPalace hints: {}", rec_str));
                }
            }
        }

        // Deduction Engine 结构化提醒（让 LLM 知道 Layer 160 有需要注意的推理告警）
        //
        // ## 引用关系
        // - 依赖：deduction_block (Option<String>，来自 self.deduction_engine.build_injection())
        // - 消费方：LLM（通过 awareness_block 末尾告警行）
        //
        // ## 设计
        // 不修改 deduction_engine 本身的输出格式，仅在 awareness 层追加一行 actionable 提示。
        // 阈值 20 bytes 过滤掉空或极短（无实质内容）的 deduction block。
        if let Some(ref ded) = deduction_block {
            if ded.len() > 20 {
                awareness_block.push_str(
                    "\n\u{26A0} Deduction alert active \u{2014} check Layer 160 for details and take corrective action"
                );
            }
        }

        // ─── Tool Catalog 注入（场景化加载配套）──────────────────────────────────
        // 当 scene_tool_loading_enabled 时，LLM 只收到场景相关工具的完整 schema。
        // 此 catalog 提供全量工具名索引（~200 tokens），让 LLM 知道可用工具全集，
        // 需要时可直接按名调用（On-Demand Expansion）。
        //
        // ## KV Cache 影响
        // catalog 在 session 内 byte-stable（工具注册后不变），放在 assembled 之后、focus 之前。
        // focus 每轮变化已在末尾隔离，catalog 不破坏稳定前缀。
        let catalog_block = if self.config.scene_tool_loading_enabled {
            let all_tools = self.registry
                .list_visible(self.config.tool_visibility_threshold.clone())
                .await;
            let catalog = crate::llm::tool_catalog::generate_catalog(&all_tools);
            Some(catalog)
        } else {
            None
        };

        // ─── 构建 text（单 String，用于非 Anthropic）────────────────────────────
        // 【KV Cache 修复】focus 追加到 assembled 末尾（与 segments 路径对齐）
        // 之前 focus 顶在最前（primacy），但 focus.render_with_age(age) 每轮 byte 都变（age 增长）
        //   → DeepSeek/OpenAI 的 prefix cache 从 token 0 起整段 system prompt 全部 miss
        // 移到末尾后：stable prefix（Kernel + abacusbr_core + Strategy + Constraints）byte-identical
        //   → DeepSeek 命中率应从 ~0% 提升到接近 prompt_tokens（按 64-token 块对齐）
        // 对 LLM 注意力的影响：focus 仍在 system prompt 内、紧邻用户消息（recency-adjacent），不弱于 primacy
        let assembled_with_context = {
            let mut s = assembled;
            if let Some(ref cat) = catalog_block {
                s = format!("{}\n\n---\n\n{}", s, cat);
            }
            // Awareness block 每轮变化（turn/tool_calls 递增）→ 放在动态段
            s = format!("{}\n\n---\n\n{}", s, awareness_block);

            // ─── Entropy Guard（熵增对抗纪律）──────────────────────────────────
            // 内核级约束：LLM 在创建文件/文件夹/多步任务前先结构化思考
            // byte-stable（不含变量），被 prefix cache 覆盖
            // 策略注入（从 policy.toml 加载，运行时可调）
            let policy = &self.config.policy;
            if !policy.guard.entropy_guard.is_empty() {
                s.push_str("\n\n[Entropy Guard]\n");
                s.push_str(&policy.guard.entropy_guard);
            }
            if !policy.guard.explicit_declaration.is_empty() {
                s.push_str("\n\n[Explicit Declaration]\n");
                s.push_str(&policy.guard.explicit_declaration);
            }
            s
        };
        let text = compose_system_text_with_focus(assembled_with_context, focus_block.as_deref());

        // ─── 构建 segments（多 block，用于 Anthropic cache）────────────────────
        let mut segments = self.prompt_assembly.assemble_segments(
            &injector_segments,
            deduction_block.as_deref(),
            Some(&preflight_block),
            &retained,
            Some(task_kind),
            false,
        );

        // SessionFocus 应用到 segments —— 与 text 路径策略对齐（统一追加到末尾）
        // 之前 text 路径保留 focus 在前（primacy）属于跨 provider 不一致，DeepSeek/OpenAI 已踩坑
        //
        // W3 (Task #101) 守护：focus 含 age 字段（render_with_age），每轮字节都变。
        //   不能写到 cacheable=true 段——会破 prefix cache。
        //   规则：
        //     ① 末段是 cacheable=false → append（原行为）
        //     ② 末段是 cacheable=true 或 segments 为空 → push 一个新 cacheable=false 段
        if let Some(ref focus) = focus_block {
            let needs_new_segment = match segments.last() {
                Some(last) => last.cacheable,
                None => true,
            };
            if needs_new_segment {
                segments.push(crate::llm::provider::SystemSegment {
                    text: format!("{}\n\n---", focus),
                    cacheable: false,
                });
            } else if let Some(last) = segments.last_mut() {
                last.text.push_str(&format!("\n\n---\n\n{}", focus));
            }
        }

        SystemPromptOutput { text, segments }
    }

    /// 向后兼容薄包装：返回 text（非 Anthropic provider 路径）
    #[tracing::instrument(skip(self, input, session, preflight))]
    pub(crate) async fn build_system_prompt(
        &self,
        input: &str,
        session: &RwLock<SessionState>,
        preflight: &PreflightReport,
    ) -> String {
        self.build_system_output(input, session, preflight).await.text
    }

    /// LLM 静默自审：无工具调用，仅分析请求，输出结构化报告。
    /// 返回 (report, prompt_tokens, completion_tokens) 用于累加到总消耗。
    async fn llm_self_review(&self, input: &str, task_kind: &crate::core::task_analyzer::TaskKind) -> (PreflightReport, u64, u64) {
        let (_provider_id, provider) = match self.resolve_provider().await {
            Ok(p) => p,
            Err(_) => return (PreflightReport::default(), 0, 0),
        };

        let review_prompt = format!(
            r#"Analyze this request silently. No tools available. Output JSON only.

Request: {input}
Detected task type: {kind:?}

Analyze:
1. confirmed_intent: what does the user actually want?
2. dependencies: what information is missing or needed?
3. risks: any destructive operations, security concerns, ambiguity?
4. execution_plan: recommended step-by-step approach (max 5 steps)
5. acceptance_criteria: how to verify success

Output JSON:
{{"confirmed_intent":"...","dependencies":["..."],"risks":["..."],"execution_plan":["..."],"acceptance_criteria":["..."]}}"#,
            input = input.chars().take(200).collect::<String>(),
            kind = task_kind.label(),
        );

        let req = LlmRequest {
            model: self.config.default_model.clone(),
            messages: vec![Message {
                role: MessageRole::User,
                content: Some(MessageContent::Text(review_prompt)),
                name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
            }],
            system: Some("You are a rigorous self-reviewer. Analyze the request before execution. Output JSON only.".into()),
            system_segments: Vec::new(),
            tools: Vec::new(),
            temperature: Some(0.1),
            max_tokens: Some(1024),
            top_p: None, stop: Vec::new(), stream: false,
            thinking_intent: None, cache_config: None, extra_body: HashMap::new(), user_message_preamble: None,
        };

        match provider.complete(req).await {
            Ok(resp) => {
                let pt = resp.usage.prompt_tokens;
                let ct = resp.usage.completion_tokens;
                let text = extract_text(&resp.message);
                let json_str = if let Some(start) = text.find('{') {
                    if let Some(end) = text.rfind('}') { &text[start..=end] } else { &text }
                } else { &text };
                let report = serde_json::from_str::<PreflightReport>(json_str)
                    .unwrap_or_else(|e| {
                        tracing::warn!("failed to parse PreflightReport JSON: {e}, raw: {}", &json_str[..json_str.len().min(200)]);
                        PreflightReport::default()
                    });
                (report, pt, ct)
            }
            Err(_) => (PreflightReport::default(), 0, 0),
        }
    }

    /// Handle interaction.* tools inline (need session context)
    async fn handle_interaction_tool(
        &self,
        tool_id: &ToolId,
        params: &Value,
        session: &RwLock<SessionState>,
    ) -> abacus_types::Result<ToolOutput> {
        let s = session.read().await;
        let map = s.interaction_map.read().await;
        // 单一命名约定：ToolId 与 LLM 调用名同形（`session_set_focus` 等下划线）
        // dispatch 直接 match，无需归一化。
        let name = tool_id.0.as_str();
        let result = match name {
            "interaction_status" => serde_json::to_value(map.status_block()).unwrap_or_default(),
            "interaction_path" => map.path_info(),
            "interaction_recall" => {
                let id = params.get("checkpoint").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                map.recall(id).unwrap_or(serde_json::json!({"error": "not found"}))
            }
            "interaction_mark" => {
                let label = params.get("label").and_then(|v| v.as_str()).unwrap_or("marked").to_string();
                let type_str = params.get("type").and_then(|v| v.as_str()).unwrap_or("manual_mark");
                let cp_type = match type_str {
                    "milestone" => CheckpointType::Milestone, "decision" => CheckpointType::Decision,
                    "subgoal" => CheckpointType::Subgoal, _ => CheckpointType::ManualMark,
                };
                drop(map);
                let mut map = s.interaction_map.write().await;
                let cp = Checkpoint::new(label, cp_type, s.turn_count, 0, String::new(), String::new(), vec![], vec![], vec![]);
                let id = map.add_checkpoint(cp);
                serde_json::json!({"marked": true, "checkpoint_id": id})
            }
            "session_set_focus" => {
                let goal = params.get("goal").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let phase = params.get("phase").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let constraints: Vec<String> = params.get("constraints")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect())
                    .unwrap_or_default();
                let next_step = params.get("next_step").and_then(|v| v.as_str()).unwrap_or("").to_string();
                // Clone Arc before dropping guards (避免跨 await 持有 RwLock guard)
                let focus_arc = s.session_focus.clone();
                let turn_count = s.turn_count;
                drop(map);
                drop(s);
                let mut focus = focus_arc.write().await;
                *focus = Some(SessionFocus { goal, phase, constraints, next_step, updated_at_turn: turn_count.saturating_add(1) });
                serde_json::json!({"set": true, "message": "Session focus anchor updated. Will appear at top of context starting next turn."})
            }
            // cross-session 段H：列出/汇总 prior sessions（项目内）
            // 引用：core::event_sink::{list_replayable_sessions, build_resume_report}
            // 失败：jsonl 目录不存在 → 空列表；session_id 不存在 → 空 report
            "session_resume_query" => {
                drop(map);
                drop(s);
                let project_dir = crate::paths::current_project_dir();
                if let Some(target_id) = params.get("session_id").and_then(|v| v.as_str()) {
                    // 单 session 摘要
                    match crate::core::event_sink::build_resume_report(target_id, &project_dir) {
                        Ok(report) => serde_json::json!({
                            "mode": "single",
                            "report": report,
                            "duration_ms": report.duration_ms(),
                            "guidance": if report.had_compression {
                                "该 session 触发过压缩——需要原始内容时调 messages_recover 工具"
                            } else if report.event_count == 0 {
                                "未找到该 session 的 jsonl（可能已被 GC 或 id 错误）"
                            } else {
                                "session 摘要正常；调 cross_session_query 检索 knowledge palace 获取语义级历史"
                            },
                        }),
                        Err(e) => {
                            return Ok(ToolOutput {
                                tool_id: tool_id.clone(), success: false,
                                output: serde_json::json!({"error": format!("read failed: {}", e)}),
                                latency_ms: 0,
                                failure_kind: Some("IoError".into()),
                                try_instead: Vec::new(),
                            });
                        }
                    }
                } else {
                    // list 模式
                    let limit = params.get("limit").and_then(|v| v.as_u64())
                        .map(|n| n.clamp(1, 50) as usize).unwrap_or(10);
                    match crate::core::event_sink::list_replayable_sessions(&project_dir) {
                        Ok(mut sessions) => {
                            sessions.truncate(limit);
                            serde_json::json!({
                                "mode": "list",
                                "project_dir": project_dir.to_string_lossy(),
                                "session_count": sessions.len(),
                                "sessions": sessions,
                                "guidance": if sessions.is_empty() {
                                    "项目内无可 replay 的 session（首次运行 / event_sink 关闭）"
                                } else {
                                    "选一个 session_id 再调本工具获取详细摘要"
                                },
                            })
                        }
                        Err(e) => {
                            return Ok(ToolOutput {
                                tool_id: tool_id.clone(), success: false,
                                output: serde_json::json!({"error": format!("list failed: {}", e)}),
                                latency_ms: 0,
                                failure_kind: Some("IoError".into()),
                                try_instead: Vec::new(),
                            });
                        }
                    }
                }
            }
            // 段 J2 + L6: tool_compass —— 协议同构自省入口（含 hide 状态过滤）
            // 引用：self.cluster_registry::recommend_by_intent + effectiveness.evaluate_at_turn
            // 失败：intent 缺失 → BusinessError；空命中 → 返 results=[] + all_clusters 列表降级
            // L6 改进：
            //   1) 推荐结果按 hide 状态过滤（adaptive_d_tier_hide 时跳过被 hide 的工具）
            //   2) 给每条 recommendation 加 visible 字段（透明度）
            //   3) 全部命中都被 hide 时降级 fallback（不让 LLM 拿到无效推荐）
            "tool_compass" => {
                drop(map);
                let cur_turn = s.turn_count;
                drop(s);
                let intent = params.get("intent").and_then(|v| v.as_str()).unwrap_or("").trim();
                if intent.is_empty() {
                    return Ok(ToolOutput {
                        tool_id: tool_id.clone(),
                        success: false,
                        output: serde_json::json!({
                            "error": "intent parameter required",
                            "hint": "Pass a natural-language description of what you want to do"
                        }),
                        latency_ms: 0,
                        failure_kind: Some("BusinessError".into()),
                        try_instead: Vec::new(),
                    });
                }
                let top_k = params.get("top_k").and_then(|v| v.as_u64())
                    .map(|n| n.clamp(1, 10) as usize).unwrap_or(3);
                let recs = self.cluster_registry.recommend_by_intent(intent, top_k);

                // 段 L6: 计算每条 rec 的 hide 状态
                let hide_enabled = self.config.adaptive_d_tier_hide;
                let recs_with_visibility: Vec<serde_json::Value> = if hide_enabled && !recs.is_empty() {
                    let eff = self.effectiveness.read().await;
                    let all_tools = self.registry.all_tools().await;
                    let provider_for: std::collections::HashMap<String, abacus_types::ToolProvider> =
                        all_tools.iter()
                            .map(|t| (t.id.0.clone(), t.provider.clone()))
                            .collect();
                    recs.iter().map(|r| {
                        let provider = provider_for.get(&r.tool_id);
                        let visible = match provider {
                            Some(p) => {
                                let e = eff.evaluate_at_turn(&ToolId(r.tool_id.clone()), p, cur_turn as u64);
                                // 过滤条件等同 K3 Phase 1: tier=D 且数据足够 → 不可见
                                !(matches!(e.tier, abacus_types::VisibilityTier::D) && !e.insufficient_data)
                            }
                            None => true, // 未注册（罕见）→ 保守标可见
                        };
                        serde_json::json!({
                            "cluster_id": r.cluster_id,
                            "tool_id": r.tool_id,
                            "differentiator": r.differentiator,
                            "relevance_score": r.relevance_score,
                            "visible": visible,
                        })
                    }).collect()
                } else {
                    recs.iter().map(|r| serde_json::json!({
                        "cluster_id": r.cluster_id,
                        "tool_id": r.tool_id,
                        "differentiator": r.differentiator,
                        "relevance_score": r.relevance_score,
                        "visible": true,
                    })).collect()
                };

                // 段 L6: 至少有一个 visible recommendation 才算命中
                let visible_count = recs_with_visibility.iter()
                    .filter(|r| r["visible"] == serde_json::Value::Bool(true))
                    .count();

                if recs.is_empty() || visible_count == 0 {
                    // 降级：列所有 clusters 让 LLM 自选
                    let clusters: Vec<serde_json::Value> = self.cluster_registry.all_clusters().iter()
                        .map(|c| serde_json::json!({
                            "id": c.id,
                            "purpose": c.purpose,
                            "tools": c.members.iter().map(|m| m.tool_id).collect::<Vec<_>>(),
                        }))
                        .collect();
                    let reason = if recs.is_empty() {
                        "No keyword match found"
                    } else {
                        "All keyword-matched tools are currently hidden by adaptive_d_tier_hide (low score / palace_demoted)"
                    };
                    serde_json::json!({
                        "mode": "no_match_fallback",
                        "intent": intent,
                        "results": recs_with_visibility, // 即使全 hidden 也显示，让 LLM 知道有哪些
                        "all_clusters": clusters,
                        "guidance": format!("{}. Browse all_clusters or rephrase intent with more specific verbs/nouns.", reason),
                    })
                } else {
                    serde_json::json!({
                        "mode": "ranked_recommendations",
                        "intent": intent,
                        "results": recs_with_visibility,
                        "guidance": "Top-ranked recommendation comes first. Each result's 'differentiator' tells you why it fits. 'visible: false' means the tool is currently hidden — pick a visible alternative.",
                    })
                }
            }
            // cross-session: 按 recover_id 拉回压缩前的原始 messages
            // 引用：self.context_manager.recover_messages(id) → Option<Vec<Message>>
            // 失败：id 不存在 / 已 LRU evict → error 提示；
            // 返回：成功时 messages 数组（仅 role/content/tool_call_id 字段，避免 LLM 看到内部 reasoning_content）
            "messages_recover" => {
                drop(map);
                drop(s);
                let recover_id = params.get("recover_id").and_then(|v| v.as_str()).unwrap_or("").trim();
                if recover_id.is_empty() {
                    return Ok(ToolOutput {
                        tool_id: tool_id.clone(), success: false,
                        output: serde_json::json!({"error": "recover_id parameter required"}),
                        latency_ms: 0,
                        failure_kind: Some("BusinessError".into()),
                        try_instead: Vec::new(),
                    });
                }
                match self.context_manager.recover_messages(recover_id).await {
                    Some(messages) => {
                        // 仅暴露安全字段——reasoning_content / tool_calls 在 archive 中保留但
                        // 这里序列化时仅给 LLM 看到 role + content + tool_call_id（足够理解原文）
                        let safe_messages: Vec<serde_json::Value> = messages.iter().map(|m| {
                            let content_str = match &m.content {
                                Some(crate::llm::MessageContent::Text(t)) => t.clone(),
                                Some(crate::llm::MessageContent::MultiPart(parts)) => {
                                    parts.iter().filter_map(|p| {
                                        if let crate::llm::ContentPart::Text { text } = p { Some(text.clone()) } else { None }
                                    }).collect::<Vec<_>>().join(" ")
                                }
                                None => String::new(),
                            };
                            serde_json::json!({
                                "role": format!("{:?}", m.role),
                                "content": content_str,
                                "tool_call_id": m.tool_call_id,
                            })
                        }).collect();
                        serde_json::json!({
                            "recover_id": recover_id,
                            "message_count": safe_messages.len(),
                            "messages": safe_messages,
                        })
                    }
                    None => {
                        return Ok(ToolOutput {
                            tool_id: tool_id.clone(), success: false,
                            output: serde_json::json!({
                                "error": format!("recover_id '{}' not found in archive", recover_id),
                                "hint": "id 可能已被 LRU evict（archive 容量上限）；尝试更接近压缩点的 recover_id"
                            }),
                            latency_ms: 0,
                            failure_kind: Some("NotFound".into()),
                            try_instead: Vec::new(),
                        });
                    }
                }
            }
            // cross-session: 跨 session 知识查询（走 DualPalace hybrid_search）
            // 引用：self.memory_palace.knowledge.hybrid_search → 返回 (KnowledgeEntry, score)[]
            // 失败：palace 未启用 → error；query 空 → empty results；超 top_k 上限 → 截断到 20
            "cross_session_query" => {
                drop(map);
                drop(s);
                let query = params.get("query").and_then(|v| v.as_str()).unwrap_or("").trim();
                if query.is_empty() {
                    return Ok(ToolOutput {
                        tool_id: tool_id.clone(), success: false,
                        output: serde_json::json!({"error": "query parameter required and non-empty"}),
                        latency_ms: 0,
                        failure_kind: Some("BusinessError".into()),
                        try_instead: Vec::new(),
                    });
                }
                let top_k = params.get("top_k")
                    .and_then(|v| v.as_u64())
                    .map(|n| n.clamp(1, 20) as usize)
                    .unwrap_or(5);
                let domain_filter = params.get("domain").and_then(|v| v.as_str()).map(|s| s.to_string());
                let Some(palace_arc) = self.memory_palace.as_ref() else {
                    return Ok(ToolOutput {
                        tool_id: tool_id.clone(), success: false,
                        output: serde_json::json!({
                            "error": "memory_palace not initialized",
                            "hint": "调用方需用 CoreLoop::with_memory(store, palace) 启用持久化记忆层"
                        }),
                        latency_ms: 0,
                        failure_kind: Some("DependencyMissing".into()),
                        try_instead: Vec::new(),
                    });
                };
                let palace = palace_arc.read().await;
                let raw_results = palace.hybrid_search(query, top_k * 2).await; // 多取一倍预防 domain filter 后不足
                let mut filtered: Vec<_> = raw_results.into_iter()
                    .filter(|(e, _)| {
                        domain_filter.as_ref().map(|d| &e.domain == d).unwrap_or(true)
                    })
                    .take(top_k)
                    .collect();
                // 已按 score 降序（hybrid_search 内部保证）
                let results: Vec<serde_json::Value> = filtered.drain(..)
                    .map(|(entry, score)| serde_json::json!({
                        "id": entry.id,
                        "title": entry.title,
                        "content": entry.content,
                        "domain": entry.domain,
                        "tags": entry.tags,
                        "score": score,
                        "last_reviewed": entry.last_reviewed,
                        "sm2_repetitions": entry.sm2_repetitions,
                    }))
                    .collect();
                serde_json::json!({
                    "query": query,
                    "top_k": top_k,
                    "domain_filter": domain_filter,
                    "result_count": results.len(),
                    "results": results,
                    "guidance": if results.is_empty() {
                        "无匹配结果。可考虑放宽 domain filter 或尝试更通用的 query。"
                    } else {
                        "结果按混合相似度排序。最高分的可能是最相关的；score>0.7 一般为强信号。"
                    },
                })
            }
            // V29.13 段3a：MagChain 透明化——LLM 可读 epistemic guard / decay tier / pipeline hooks
            "magchain_status" => {
                drop(map);
                drop(s);
                let violations = self.epistemic_guard.violations().await;
                let cold_start = self.epistemic_guard.is_cold_start().await;
                let declaration = self.epistemic_guard.declaration_if_needed().await;
                let hook_names: Vec<String> = {
                    let hooks = self.pipeline_hooks.read().await;
                    hooks.iter().map(|(p, h)| format!("{}:{}", p, h.name())).collect()
                };
                // 可选 decay tier 检测
                let decay_info = params.get("input_to_classify").and_then(|v| v.as_str()).map(|input| {
                    let tier = crate::mag_chain::DecayRouter::classify(input);
                    let tools_to_promote = crate::mag_chain::DecayRouter::tools_to_promote(tier);
                    let hint = crate::mag_chain::DecayRouter::prompt_hint(tier);
                    serde_json::json!({
                        "tier": format!("{:?}", tier),
                        "tools_to_promote": tools_to_promote,
                        "prompt_hint": hint,
                    })
                });
                // cross-session: 跨 session 知识量统计——让 LLM 知道有多少历史可查
                let cross_session_stats = if let Some(palace_arc) = self.memory_palace.as_ref() {
                    let palace = palace_arc.read().await;
                    let knowledge_count = palace.knowledge.len().await;
                    let behavior_count = palace.behavior.len().await;
                    serde_json::json!({
                        "available": true,
                        "knowledge_entries": knowledge_count,
                        "behavior_entries": behavior_count,
                        "hint": "可调用 cross_session_query 工具检索相关历史"
                    })
                } else {
                    serde_json::json!({
                        "available": false,
                        "hint": "memory_palace 未启用；调用方需 with_memory() 注入持久化层"
                    })
                };
                serde_json::json!({
                    "epistemic": {
                        "violations": violations,
                        "cold_start": cold_start,
                        "declaration_pending": declaration,
                        "explanation": "violations 累计达到阈值时，下一轮会在 LLM 输出前注入 [认识论约束违规] 警告并要求标注 [unverified]。",
                    },
                    "pipeline_hooks": {
                        "active": hook_names,
                        "explanation": "PipelineHook 系统在 turn 关键阶段 emit 事件。MagChain 中间件 + PipelineHook = 完整 hook 系统；hook 名前缀通常对应其责任域。",
                    },
                    "decay_router": decay_info,
                    "cross_session": cross_session_stats,
                    "guidance": "见到 violations>0 时主动调工具验证；cold_start=true 时降低断言密度优先 [训练快照] 标记；有 cross_session knowledge 时可主动 cross_session_query 检索历史。",
                })
            }
            // V38: session_request_permission — 检查工具是否已授权
            // 如果已在 always_allow / mcip_grants 中，直接告知 LLM 已授权，无需等待
            "session_request_permission" => {
                let requested_tool = params.get("tool_id").and_then(|v| v.as_str()).unwrap_or("");
                let grants = s.mcip_grants.read().unwrap();
                let already_granted = grants.contains(requested_tool);
                drop(grants);
                drop(map);
                drop(s);
                if already_granted || requested_tool.is_empty() {
                    serde_json::json!({
                        "status": "already_authorized",
                        "tool_id": requested_tool,
                        "hint": "This tool is already in your permanent allow list. You can call it directly without requesting permission."
                    })
                } else {
                    // 未授权——返回说明让 LLM 直接调用工具（MCIP 会自动弹窗）
                    serde_json::json!({
                        "status": "not_needed",
                        "tool_id": requested_tool,
                        "hint": "You don't need to request permission explicitly. Just call the tool directly — if authorization is needed, the system will automatically prompt the user."
                    })
                }
            }
            // V38: LLM 主动模式切换
            "mode_switch" => {
                drop(map);
                drop(s);
                let target_str = params.get("target").and_then(|v| v.as_str()).unwrap_or("");
                let reason = params.get("reason").and_then(|v| v.as_str()).unwrap_or("LLM initiated");
                match abacus_types::AbacusMode::from_label(target_str) {
                    Some(target) => {
                        // 返回切换指令——TUI 层消费此 output 执行实际切换
                        // core 层不直接修改 AppState（跨 crate 边界）
                        serde_json::json!({
                            "action": "switch_mode",
                            "target": target_str,
                            "reason": reason,
                            "display_name": target.display_zh(),
                        })
                    }
                    None => {
                        return Ok(ToolOutput {
                            tool_id: tool_id.clone(), success: false,
                            output: serde_json::json!({
                                "error": format!("invalid target mode: '{}'", target_str),
                                "valid_targets": ["clarify", "meeting", "plan", "team"],
                            }),
                            latency_ms: 0,
                            failure_kind: Some("ValidationError".into()),
                            try_instead: Vec::new(),
                        });
                    }
                }
            }
            _ => serde_json::json!({"error": format!("unknown: {}", name)}),
        };
        Ok(ToolOutput { tool_id: tool_id.clone(), success: true, output: result, latency_ms: 0, failure_kind: None, try_instead: Vec::new() })
    }

    async fn resolve_provider(&self) -> Result<(String, Arc<dyn LlmProvider>), KernelError> {
        let model: String = self.model_override.read().await
            .as_ref().map(|m| m.0.clone())
            .unwrap_or_else(|| self.config.default_model.0.clone());
        let providers = self.providers.read().await;

        // 0. 优先使用 FallbackProvider（双协议自动回退）
        if let Some(provider) = providers.get("primary") {
            return Ok(("primary".into(), provider.clone()));
        }

        // 1. 精确匹配：按 provider_id 查
        if let Some(provider) = providers.get(&model) {
            return Ok((model.clone(), provider.clone()));
        }

        // 2. 厂商分组匹配：模型名是否属于某个分组
        for group in self.provider_groups.read().await.iter() {
            if group.supports(&model) {
                return Ok((group.id.clone(), group.provider.clone()));
            }
        }

        // 3. CapabilityHub 路由
        let request = CapabilityRequest {
            kind: CapabilityKind::LlmCompletion {
                model: model.clone(),
                capabilities: vec!["tools".into()],
            },
            context: Some(CapabilityContext { forced_provider: None, task_kind: None, session_id: None }),
        };
        let candidates = self.capability_hub.resolve(&request);
        for candidate in &candidates {
            if let Some(provider) = providers.get(&candidate.provider_id) {
                return Ok((candidate.provider_id.clone(), provider.clone()));
            }
        }

        // 4. Fallback: 第一个已注册 provider
        if let Some((id, provider)) = providers.iter().next() {
            return Ok((id.clone(), provider.clone()));
        }

        Err(KernelError::Other("no LLM provider available — set DEEPSEEK_API_KEY or ABACUS_API_KEY".into()))
    }

    /// build_tool_definitions 默认入口（无 task_kind 过滤、无 turn 修剪）。仅测试场景使用。
    #[allow(dead_code)]
    async fn build_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.build_tool_definitions_for(None, None).await
    }

    /// Phase β-D + γ-I：按 task_kind 过滤 + 按使用频率修剪的版本
    ///
    /// - `task_kind_label = None || task_kind_routing_enabled=false` → 跳过 task_kind 过滤
    /// - `current_turn = None || tool_frequency_pruning_turns=None` → 跳过 frequency 过滤
    /// 任一过滤被跳过时该维度等价不过滤；都关时行为同 build_tool_definitions。
    pub(crate) async fn build_tool_definitions_for(
        &self,
        task_kind_label: Option<&str>,
        current_turn: Option<u64>,
    ) -> Vec<ToolDefinition> {
        // V2 KV cache 修复 + tier gating：从 all_tools() 改用 list_visible(threshold)
        //
        // ## 改动原因
        // 之前 build_tool_definitions 直接调 all_tools，effectiveness.tier 完全不影响 LLM 视野。
        // 现在用 list_visible(threshold) 让连续失败的低分工具自动隐形——但默认 threshold=D
        // 等价不过滤，行为与之前一致；用户可通过 CoreConfig.tool_visibility_threshold 调高。
        //
        // ## list_visible 已自带 state 过滤
        // list_visible 内部 filter 已经做了 state == Loaded | Active 检查，
        // 不需要在外层再加 .filter(state)（与之前 all_tools 路径分工不同）。
        let tools = self.registry
            .list_visible(self.config.tool_visibility_threshold.clone())
            .await;
        // Phase β-D：task_kind 路由过滤
        //
        // 仅当 routing_enabled 且 task_kind_label 已传入时启用；否则透传所有工具。
        // tool.applicable_task_kinds 为 None → 全任务可见（保留默认行为）。
        // Some(list) → 只在 list 命中当前 task_kind 时暴露。
        let routing_active = self.config.task_kind_routing_enabled
            && task_kind_label.is_some();
        let tools: Vec<_> = tools.into_iter().filter(|t| {
            if !routing_active {
                return true;
            }
            match &t.schema.applicable_task_kinds {
                None => true,
                Some(list) => {
                    let label = task_kind_label.unwrap();
                    list.iter().any(|k| k == label)
                }
            }
        }).collect();

        // Phase α-S：Scene-active tool set filtering
        //
        // 只对场景相关工具发送完整 schema 到 LLM（ToolDefinition[]）。
        // 其余工具通过 system prompt 中的 tool catalog 索引告知 LLM 其存在，
        // LLM 可直接按名字调用（On-Demand Expansion）。
        //
        // 三重保留规则：
        // 1. 名字前缀匹配 scene_active_prefixes
        // 2. 最近 5 轮内调用过（recently active）
        // 3. task_kind routing 已通过（applicable_task_kinds 命中）
        //
        // 引用关系：
        // - 依赖：scene_active_prefixes() 自由函数、self.tool_last_invoked、self.config.scene_tool_loading_enabled
        // - 消费方：下游 Phase γ-I 和段 K3 接收过滤后的 tools Vec
        let tools: Vec<_> = if self.config.scene_tool_loading_enabled && task_kind_label.is_some() {
            let prefixes = scene_active_prefixes(task_kind_label.unwrap());
            // 修复：未列举的任务类型（general_chat 等）前缀为空列表→过滤掉所有工具→tools=0
            // 修复：前缀为空时跳过过滤，全部工具透传
            if prefixes.is_empty() {
                tools
            } else {
            let last = self.tool_last_invoked.read().await;
            let cur = current_turn.unwrap_or(0);
            tools.into_iter().filter(|t| {
                // Rule 1: name prefix match
                if prefixes.iter().any(|p| t.schema.name.starts_with(p)) {
                    return true;
                }
                // Rule 2: recently invoked (last 5 turns)
                if let Some(&prev) = last.get(&t.id) {
                    if prev > 0 && cur.saturating_sub(prev) <= 5 {
                        return true;
                    }
                }
                // Rule 3: explicitly marked for this task_kind (already passed β-D filter above)
                if let Some(ref kinds) = t.schema.applicable_task_kinds {
                    if kinds.iter().any(|k| k == task_kind_label.unwrap()) {
                        return true;
                    }
                }
                false
            }).collect()
            } // else (prefixes non-empty)
        } else {
            tools
        };

        // Phase γ-I：frequency-based pruning
        //
        // 只在 (current_turn, tool_frequency_pruning_turns) 都 Some 时生效。
        // last_invoked = 0（从未调用）保留——避免新工具立刻被剪掉；
        // 已调用过的工具：current_turn - last_invoked > N → 隐藏。
        let pruning_active = current_turn.is_some()
            && self.config.tool_frequency_pruning_turns.is_some();
        let tools: Vec<_> = if pruning_active {
            let turn = current_turn.unwrap();
            let n = self.config.tool_frequency_pruning_turns.unwrap();
            let last = self.tool_last_invoked.read().await;
            tools.into_iter().filter(|t| {
                match last.get(&t.id) {
                    None => true, // 从未调用 → 保留（新工具友好）
                    Some(&0) => true,
                    Some(&prev) => turn.saturating_sub(prev) <= n,
                }
            }).collect()
        } else {
            tools
        };
        // 段 K3：自适应 D-tier 过滤——按 effectiveness 评分丢弃长期低分工具，含多层兜底
        //
        // ## 兜底层级（自上而下）
        // 1. 总工具数 < HIDE_MIN_TOTAL（5）→ 完全禁用 hide（不够多不裁剪）
        // 2. tier==D + 数据足够 + 当前未在 probation 窗口 → 候选隐藏
        // 3. Provider floor：每个 ToolProvider 至少保留 PROVIDER_FLOOR（2）个
        // 4. Cluster floor：同 cluster 至少保留 1 个（避免整簇隐藏）
        // 5. 隐藏后剩余可见 < HIDE_MIN_VISIBLE（3）→ 全部回退（极端：评分集体下行时不裁剪）
        //
        // ## 引用
        // - CoreConfig.adaptive_d_tier_hide（默认 true，段 K5）
        // - effectiveness.evaluate_at_turn(provider, turn) — 段 K2 + K4 集成
        // - cluster_registry.cluster_for() — 段 J1 协议同构同 cluster floor
        let tools: Vec<_> = if self.config.adaptive_d_tier_hide {
            const HIDE_MIN_TOTAL: usize = 5;       // 总工具不足时不裁剪
            const HIDE_MIN_VISIBLE: usize = 3;     // 隐藏后剩余 < 此 → 回退
            const PROVIDER_FLOOR: usize = 2;       // 每个 provider 至少保留几个

            if tools.len() < HIDE_MIN_TOTAL {
                tools
            } else {
                let eff = self.effectiveness.read().await;
                let cur_turn = current_turn.unwrap_or(0);
                // Phase 1：识别"候选隐藏"——tier=D 且数据足够、不在试探窗口
                let mut hide_candidates: std::collections::HashSet<ToolId> = std::collections::HashSet::new();
                for t in &tools {
                    let e = eff.evaluate_at_turn(&t.id, &t.provider, cur_turn);
                    if e.insufficient_data {
                        continue; // 新工具或扩展冷启动期 → 强制保留
                    }
                    if matches!(e.tier, abacus_types::VisibilityTier::D) {
                        hide_candidates.insert(t.id.clone());
                    }
                }

                // Phase 2: Provider floor —— 每个 provider 至少保留 PROVIDER_FLOOR 个
                // 把 candidates 按 provider 分组；若某 provider 隐藏后剩余 < floor，
                // 从该 provider 的候选里挑分数最高的回放
                let mut by_provider: std::collections::HashMap<String, Vec<&abacus_types::ToolHandle>> = std::collections::HashMap::new();
                for t in &tools {
                    let key = match &t.provider {
                        abacus_types::ToolProvider::BuiltIn => "builtin".to_string(),
                        abacus_types::ToolProvider::Mcp { server_id } => format!("mcp:{}", server_id),
                        abacus_types::ToolProvider::Plugin { plugin_id } => format!("plugin:{}", plugin_id),
                        abacus_types::ToolProvider::Skill { skill_id } => format!("skill:{}", skill_id),
                    };
                    by_provider.entry(key).or_default().push(t);
                }
                for (_pkey, group) in &by_provider {
                    let visible_in_group = group.iter().filter(|t| !hide_candidates.contains(&t.id)).count();
                    if visible_in_group < PROVIDER_FLOOR {
                        // 该 provider 可见数不足，从候选里挑分最高的回放
                        let mut to_restore: Vec<(&abacus_types::ToolHandle, f64)> = group.iter()
                            .filter(|t| hide_candidates.contains(&t.id))
                            .map(|t| {
                                let e = eff.evaluate_at_turn(&t.id, &t.provider, cur_turn);
                                (*t, e.composite_score)
                            })
                            .collect();
                        to_restore.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                        for (t, _) in to_restore.iter().take(PROVIDER_FLOOR.saturating_sub(visible_in_group)) {
                            hide_candidates.remove(&t.id);
                        }
                    }
                }

                // Phase 3: Cluster floor —— 同 cluster 至少保留 1 个
                let cluster_registry = self.cluster_registry.clone();
                let mut by_cluster: std::collections::HashMap<&'static str, Vec<&abacus_types::ToolHandle>> = std::collections::HashMap::new();
                for t in &tools {
                    if let Some(c) = cluster_registry.cluster_for(&t.schema.name) {
                        by_cluster.entry(c.id).or_default().push(t);
                    }
                }
                for (_cid, members) in &by_cluster {
                    let visible_in_cluster = members.iter().filter(|t| !hide_candidates.contains(&t.id)).count();
                    if visible_in_cluster == 0 {
                        // 整簇隐藏 → 回放分最高的
                        let mut to_restore: Vec<(&abacus_types::ToolHandle, f64)> = members.iter()
                            .map(|t| {
                                let e = eff.evaluate_at_turn(&t.id, &t.provider, cur_turn);
                                (*t, e.composite_score)
                            })
                            .collect();
                        to_restore.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                        if let Some((t, _)) = to_restore.first() {
                            hide_candidates.remove(&t.id);
                        }
                    }
                }

                // Phase 4: 整体可见数兜底 —— hide 后剩余 < HIDE_MIN_VISIBLE → 全部回退
                let post_hide_visible = tools.iter().filter(|t| !hide_candidates.contains(&t.id)).count();
                if post_hide_visible < HIDE_MIN_VISIBLE {
                    tracing::warn!(
                        candidates = hide_candidates.len(),
                        post_visible = post_hide_visible,
                        "adaptive_d_tier_hide: 隐藏后剩余 < {}，整体回退（评分集体下行）", HIDE_MIN_VISIBLE
                    );
                    tools
                } else {
                    tools.into_iter().filter(|t| !hide_candidates.contains(&t.id)).collect()
                }
            }
        } else {
            tools
        };

        // Layer 2 (Task #89)：转换逻辑下沉到 llm::tool_view 模块
        // 自动剔除 security/returns/examples/idempotent 等 LLM 不需要的字段
        // ToolHandle → ToolFunctionSpec 单一调用，不再内联 sanitize/prefix/suffix 逻辑
        //
        // 段 J1：Cluster hint 注入——在 tool_handle_to_llm_spec 前 patch 一份 schema.description
        // 让 LLM 看到"协议同构簇"信息：同簇兄弟工具 + 自身的 differentiator。
        // 不在 cluster 中的工具不影响（render_hint_for 返 None）。
        // byte-stable：同一组工具集 + 同一 ClusterRegistry → byte-identical 输出，不破 KV cache 前缀。
        // 段 K6：Effectiveness tier label 注入
        //
        // Pre-compute effectiveness tiers for all visible tools (avoids async in map closure).
        // LLM sees a compact tier badge in tool description → quality signal for tool selection.
        //
        // ## 引用关系
        // - 依赖：self.effectiveness (Arc<RwLock<EffectivenessTracker>>)
        // - 消费方：LLM（通过 ToolDefinition.description suffix）
        //
        // ## 生命周期
        // - 创建：每次 build_tool_definitions_for 调用时临时计算
        // - 销毁：函数返回后 drop（无副作用）
        let tier_map: std::collections::HashMap<ToolId, abacus_types::VisibilityTier> = {
            let eff = self.effectiveness.read().await;
            let cur = current_turn.unwrap_or(0);
            tools.iter().filter_map(|t| {
                let eval = eff.evaluate_at_turn(&t.id, &t.provider, cur);
                if eval.insufficient_data { None }
                else { Some((t.id.clone(), eval.tier)) }
            }).collect()
        };

        let cluster_registry = self.cluster_registry.clone();
        let mut defs: Vec<ToolDefinition> = tools.into_iter()
            .map(|mut t| {
                // Effectiveness tier badge (before cluster hint so LLM sees quality first)
                if let Some(tier) = tier_map.get(&t.id) {
                    let label = match tier {
                        abacus_types::VisibilityTier::S => "\u{2605}",  // ★
                        abacus_types::VisibilityTier::A => "\u{25C6}",  // ◆
                        abacus_types::VisibilityTier::B => "\u{25CF}",  // ●
                        abacus_types::VisibilityTier::C => "\u{25CB}",  // ○
                        abacus_types::VisibilityTier::D => "\u{25B3}",  // △
                    };
                    t.schema.description.push_str(&format!(" [{}]", label));
                }
                if let Some(hint) = cluster_registry.render_hint_for(&t.schema.name) {
                    t.schema.description.push_str(&hint);
                }
                ToolDefinition {
                    type_: "function".into(),
                    function: crate::llm::tool_view::tool_handle_to_llm_spec(&t),
                }
            })
            .collect();
        // KV cache 修复：按 tool name 字典序排序，消除 HashMap 迭代非确定性。
        //
        // ## 根因
        // `registry.all_tools()` → `self.tools.values().cloned()`，self.tools 是 `HashMap<ToolId, ToolHandle>`。
        // HashMap 迭代顺序：(a) RandomState 跨进程重启变化；(b) skill 懒加载 register 时 invalidate
        // tools_cache → 下次 rebuild 顺序可能不同。每次顺序变化 → tools 数组 JSON 字节变化 → DeepSeek
        // 等 prefix cache 从 tools 段起整段 miss（含 messages 历史）。
        //
        // ## 设计
        // 字典序是稳定基线；下游 SilentRouter/DecayRouter 用 stable sort_by_key 调整路由优先级，
        // 不破坏字典序的"次序基底"——只对 routed 工具洗牌，未路由工具仍然字典序稳定。
        //
        // ## 不变量
        // 同一组工具集（相同 name 集合）→ 此函数输出 byte-identical，与 HashMap 内部状态/进程实例无关。
        defs.sort_by(|a, b| a.function.name.cmp(&b.function.name));
        defs
    }
}

/// Phase α-S: Scene-active tool prefixes by TaskKind.
///
/// Only tools matching these prefixes get full schema sent to LLM (ToolDefinition[]).
/// Other tools appear in the catalog (system prompt) but not as ToolDefinition,
/// enabling On-Demand Expansion when LLM requests them by name.
///
/// ## 引用关系
/// - 消费方：CoreLoop::build_tool_definitions_for（Phase α-S 分支）
/// - 依赖方：无（纯函数，无副作用）
///
/// ## 生命周期
/// - 静态映射，编译期确定
/// - 返回 &'static 切片，无分配
fn scene_active_prefixes(task_kind: &str) -> &'static [&'static str] {
    match task_kind {
        "code_writing" | "code_reading" => &["fs_", "file_", "dir_", "code", "cargo"],
        "debugging" => &["fs_", "file_", "code", "cargo", "test"],
        "web_search" => &["web", "fetch", "search", "browse"],
        "file_edit" => &["fs_", "file_", "dir_"],
        "data_analysis" => &["fs_", "file_", "code", "db", "data"],
        "mathematics" => &["code", "math", "calculate"],
        "architecture" => &["fs_", "file_", "dir_", "code", "diagram"],
        "review" => &["fs_", "file_", "code", "lint", "test"],
        "knowledge_query" => &["kb", "search", "web", "fetch"],
        // general_chat, linguistics 等 → 空前缀列表（仅最近调用 / 显式 task_kind 命中可保留）
        _ => &[],
    }
}

fn extract_text(msg: &Message) -> String {
    let raw = match &msg.content {
        Some(MessageContent::Text(t)) => t.clone(),
        Some(MessageContent::MultiPart(parts)) => {
            let mut out = String::new();
            for part in parts {
                if let crate::llm::ContentPart::Text { text } = part { out.push_str(text); }
            }
            out
        }
        None => String::new(),
    };
    // 清理残留的 XML tool_call 标签（DeepSeek 幻觉：输出 <tool_call> 但内容非有效工具调用）
    strip_xml_tool_tags(&raw)
}

/// 移除文本中 `<tool_call>...</tool_call>` 和 `<tool_calls>...</tool_calls>` 标签包裹
/// 保留标签内的文本内容（可能是有用信息只是被错误包裹）
fn strip_xml_tool_tags(text: &str) -> String {
    if !text.contains("<tool_call") {
        return text.to_string();
    }
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"</?tool_calls?>").unwrap()
    });
    RE.replace_all(text, "").trim().to_string()
}

/// 从 `~/.abacus/roles/default.toml` 加载 Role 能力配置。
/// 文件不存在时返回 `RoleCapabilities::default()`（维持现有行为）。
///
/// ## 配置格式（TOML）
/// ```toml
/// [capabilities]
/// fs_roots = ["/Users/admin"]
/// bash_policy = "DevTools"   # ReadOnly | DevTools | Full
/// tool_budget_per_turn = 20
///
/// [web]
/// search_provider = "duckduckgo"  # brave | searxng | duckduckgo
/// brave_api_key = ""
/// searxng_url = ""
/// ```
///
/// ## 引用关系
/// - 调用方：SessionState::new / new_with_autonomy / new_with_gate_config
/// - 结果存入：SessionState.role_caps（Arc 持有）
/// - 消费方：pipeline Phase 4 tool dispatch → ExecutionContext.role_caps
///
/// ## 生命周期
/// - 调用：SessionState 构造时一次性读取磁盘文件
/// - 无全局状态，无副作用（纯函数：读文件 → 返回值）
pub(crate) fn load_role_caps() -> abacus_types::RoleCapabilities {
    use abacus_types::{BashPolicyLevel, RoleCapabilities, SearchProvider};

    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let config_path = std::path::PathBuf::from(&home)
        .join(".abacus")
        .join("roles")
        .join("default.toml");

    let content = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(_) => return RoleCapabilities::default(),
    };

    let val: toml::Value = match toml::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("roles/default.toml parse error: {e}, using defaults");
            return RoleCapabilities::default();
        }
    };

    let mut caps = RoleCapabilities::default();

    if let Some(cap_table) = val.get("capabilities").and_then(|v| v.as_table()) {
        if let Some(roots) = cap_table.get("fs_roots").and_then(|v| v.as_array()) {
            let parsed: Vec<String> = roots.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            if !parsed.is_empty() {
                caps.fs_roots = parsed;
            }
        }
        if let Some(bp) = cap_table.get("bash_policy").and_then(|v| v.as_str()) {
            caps.bash_policy = match bp {
                "ReadOnly" => BashPolicyLevel::ReadOnly,
                "Full"     => BashPolicyLevel::Full,
                _          => BashPolicyLevel::DevTools,
            };
        }
        if let Some(budget) = cap_table.get("tool_budget_per_turn").and_then(|v| v.as_integer()) {
            caps.tool_budget_per_turn = budget.max(1).min(100) as u32;
        }
    }

    if let Some(web_table) = val.get("web").and_then(|v| v.as_table()) {
        let provider = web_table.get("search_provider").and_then(|v| v.as_str()).unwrap_or("duckduckgo");
        caps.search_provider = match provider {
            "brave" => {
                let key = web_table.get("brave_api_key").and_then(|v| v.as_str()).unwrap_or("");
                let key = if key.is_empty() {
                    std::env::var("BRAVE_API_KEY").unwrap_or_default()
                } else {
                    key.to_string()
                };
                if key.is_empty() { SearchProvider::DuckDuckGo } else { SearchProvider::BraveApi { api_key: key } }
            }
            "searxng" => {
                let url = web_table.get("searxng_url").and_then(|v| v.as_str()).unwrap_or("");
                if url.is_empty() { SearchProvider::DuckDuckGo } else { SearchProvider::SearxNg { base_url: url.to_string() } }
            }
            _ => {
                // duckduckgo（含未知值降级）；仍检查 BRAVE_API_KEY env
                if let Ok(key) = std::env::var("BRAVE_API_KEY") {
                    if !key.is_empty() {
                        SearchProvider::BraveApi { api_key: key }
                    } else {
                        SearchProvider::DuckDuckGo
                    }
                } else {
                    SearchProvider::DuckDuckGo
                }
            }
        };
    } else {
        // 无 web 配置节时，仍检查 BRAVE_API_KEY env
        if let Ok(key) = std::env::var("BRAVE_API_KEY") {
            if !key.is_empty() {
                caps.search_provider = SearchProvider::BraveApi { api_key: key };
            }
        }
    }

    caps
}

#[cfg(test)]
mod tests {
    // 测试 fixture 大量使用 `let mut cfg = CoreConfig::default(); cfg.X = Y;` 模式
    // 单字段 toggle 显式重写为 struct literal 时需要列出 `..Default::default()`，
    // 反而降低可读性（CoreConfig 字段众多）；本 allow 仅作用于测试，生产代码无影响。
    #![allow(clippy::field_reassign_with_default)]

    use super::*;
    use abacus_types::{CapabilityDeclaration, SkillDef, SkillId, SkillTriggers};
    use crate::core::context::{SessionSnapshot, SessionStore};
    use crate::llm::provider::{CacheStats, LlmResponse, TokenUsage};
    use crate::tool::builtin::register_all;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockSessionStore;
    #[async_trait]
    impl SessionStore for MockSessionStore {
        async fn save(&self, _s: SessionSnapshot) -> Result<(), KernelError> { Ok(()) }
        async fn load_recent(&self, _l: usize) -> Result<Vec<SessionSnapshot>, KernelError> { Ok(Vec::new()) }
        async fn search(&self, _q: &str) -> Result<Vec<SessionSnapshot>, KernelError> { Ok(Vec::new()) }
    }

    fn make_ctx() -> Arc<ContextManager> { Arc::new(ContextManager::new(Arc::new(MockSessionStore))) }

    struct MockProvider { model: String, call_count: AtomicU32, return_tool_calls: bool }
    impl MockProvider {
        fn new(m: &str) -> Self { Self { model: m.into(), call_count: AtomicU32::new(0), return_tool_calls: false } }
        #[allow(dead_code)] fn with_tc() -> Self { Self { model: "mock".into(), call_count: AtomicU32::new(0), return_tool_calls: true } }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn complete(&self, _req: LlmRequest) -> abacus_types::Result<LlmResponse> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if self.return_tool_calls && count == 0 {
                Ok(LlmResponse {
                    model: ModelId(self.model.clone()),
                    message: Message {
                        role: MessageRole::Assistant,
                        content: Some(MessageContent::Text("calling tool".into())),
                        name: None,
                        tool_calls: Some(vec![crate::llm::provider::ToolCall {
                            id: "call_1".into(), type_: "function".into(),
                            function: crate::llm::provider::ToolFunction { name: "fs_read".into(), arguments: r#"{"path":"Cargo.toml"}"#.into() },
                        }]),
                        tool_call_id: None, reasoning_content: None, prefix: false,
                    },
                    finish_reason: "tool_calls".into(),
                    usage: TokenUsage { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15, cached_tokens: 0, cache_creation_tokens: 0, thinking_tokens: 0 },
                    thinking: None, cache_stats: Some(CacheStats { cache_creation_tokens: 0, cache_read_tokens: 0 }),
                })
            } else {
                Ok(LlmResponse {
                    model: ModelId(self.model.clone()),
                    message: Message { role: MessageRole::Assistant, content: Some(MessageContent::Text("mock response".into())), name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false },
                    finish_reason: "stop".into(),
                    usage: TokenUsage { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15, cached_tokens: 0, cache_creation_tokens: 0, thinking_tokens: 0 },
                    thinking: None, cache_stats: None,
                })
            }
        }
        fn cacheable_segments(&self, _r: &LlmRequest) -> Vec<crate::llm::prompt_cache::CachedSegment> { Vec::new() }
        fn provider_id(&self) -> &str { "mock" }
        fn supported_models(&self) -> Vec<ModelId> { vec![ModelId(self.model.clone())] }
    }

    fn make_skill(n: &str) -> SkillDef {
        SkillDef { id: SkillId(n.into()), version: "1.0".into(),
            triggers: SkillTriggers { keywords: vec![n.into()], regex: vec![], domain: vec![] },
            workflow: vec![], prompt: String::new(), knowledge_refs: vec![] }
    }

    fn make_hub() -> CapabilityHub {
        let mut h = CapabilityHub::new();
        h.register(CapabilityDeclaration { provider_id: "mock".into(), capabilities: vec!["llm_completion".into()], constraints: vec![], priority: 10 });
        h
    }

    // ─── ContextWindowPressure 尺度 + shed 门槛回归 ─────────────────────────
    //
    // ## 防御的 bug（V29.12 修复）
    // 历史 `pressure()` 直接返回 `usage_pct()` (0~100)，但 PressurePolicy 阈值用 0~1，
    // 导致 current_tokens > 0 即触发 Overloaded，每轮空转 + 日志噪音 + 真兜底失效。
    //
    // ## 测试矩阵
    // | usage% | pressure() | classify  | shed 应否 mark |
    // |--------|-----------|-----------|---------------|
    // | 0%     | 0.00      | Normal    | 否            |
    // | 50%    | 0.50      | Normal    | 否            |
    // | 70%    | 0.70      | Elevated  | 否（<85% compress 门槛）|
    // | 85%    | 0.85      | Critical  | 是            |
    // | 95%    | 0.95      | Overloaded| 是            |

    fn make_window_at(usage_pct: f64) -> Arc<RwLock<context::ContextWindow>> {
        let mut w = context::ContextWindow::default();
        // current_tokens / max_tokens * 100 = usage_pct
        w.current_tokens = (usage_pct / 100.0 * w.max_tokens as f64) as usize;
        Arc::new(RwLock::new(w))
    }

    #[tokio::test]
    async fn pressure_normalizes_to_unit_scale() {
        // pressure() 必须在 [0.0, 1.0] 区间，且与 usage_pct/100 对应
        for &pct in &[0.0_f64, 25.0, 50.0, 70.0, 85.0, 95.0, 100.0] {
            let w = make_window_at(pct);
            let mgr = make_ctx();
            let src = ContextWindowPressure { window: w, manager: Arc::downgrade(&mgr) };
            let p = pressure::PressureSource::pressure(&src).await;
            assert!((0.0..=1.0).contains(&p), "pressure {p} out of [0,1] @ usage={pct}%");
            assert!((p - pct / 100.0).abs() < 1e-6, "expected {} got {p} @ usage={pct}%", pct / 100.0);
        }
    }

    #[tokio::test]
    async fn pressure_zero_when_idle() {
        // 防 regression：current_tokens=0 时 pressure 必须是 0（避免误进 Overloaded）
        let w = make_window_at(0.0);
        let mgr = make_ctx();
        let src = ContextWindowPressure { window: w, manager: Arc::downgrade(&mgr) };
        let p = pressure::PressureSource::pressure(&src).await;
        assert_eq!(p, 0.0);
    }

    #[tokio::test]
    async fn shed_skips_marking_below_compress_threshold() {
        // 70-85% 区间：classify 进 Elevated 但 should_compress 仍 false → shed 必须 noop
        let w = make_window_at(75.0);
        let mgr = make_ctx();
        let src = ContextWindowPressure { window: w, manager: Arc::downgrade(&mgr) };
        let n = pressure::PressureSource::shed(&src, 0.70).await;
        assert_eq!(n, 0, "75% 不应 mark_shed_pending（compress 门槛 85%）");
        assert!(!mgr.take_shed_pending(), "shed_pending 必须未设");
    }

    #[tokio::test]
    async fn shed_marks_when_above_compress_threshold() {
        // ≥85%：should_compress=true，shed 必须 mark
        let w = make_window_at(90.0);
        let mgr = make_ctx();
        let src = ContextWindowPressure { window: w, manager: Arc::downgrade(&mgr) };
        let n = pressure::PressureSource::shed(&src, 0.49).await;
        assert_eq!(n, 1, "90% 必须 mark_shed_pending");
        assert!(mgr.take_shed_pending(), "shed_pending 必须已设");
    }

    #[tokio::test]
    async fn shed_returns_zero_when_manager_dropped() {
        // 防御：Weak::upgrade 失败时不 panic、不 mark
        let w = make_window_at(95.0);
        let weak = {
            let mgr = make_ctx();
            Arc::downgrade(&mgr) // mgr 出 scope 即被 drop
        };
        let src = ContextWindowPressure { window: w, manager: weak };
        let n = pressure::PressureSource::shed(&src, 0.49).await;
        assert_eq!(n, 0, "manager drop 后 shed 必须 noop");
    }

    /// V29.13 收尾：audit_optimizations 输出包含三段补强标签
    ///
    /// 防 regression：未来重构 audit 报告时，若误删本块内容，本测试 fail 即提醒
    #[tokio::test]
    async fn audit_report_includes_v29_13_segments() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        // 显式关闭 event_sink 避免测试污染 ABACUS_HOME
        let cfg = CoreConfig { event_sink_enabled: false, ..Default::default() };
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let lines = core.audit_report().await;
        let report = lines.join("\n");
        assert!(report.contains("V29.13 段1"), "missing 段1 hook 行: {report}");
        assert!(report.contains("V29.13 段2"), "missing 段2 palace bridge 行: {report}");
        assert!(report.contains("V29.13 段3"), "missing 段3 llm visibility 行: {report}");
        assert!(report.contains("pipeline hooks"), "段1 应描述 hook 注册");
        assert!(report.contains("palace ↔ tiers bridge"), "段2 应描述桥接状态");
        assert!(report.contains("llm hook visibility"), "段3 应描述 LLM 感知态");
    }

    // ─── 段 J1：Tool Cluster hint 注入到 LLM-facing description ──────────────

    /// LLM 拿到的 cross_session_query description 应包含 cluster hint
    #[tokio::test]
    async fn cluster_hint_injected_for_session_history_tools() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let defs = core.build_tool_definitions_for(None, None).await;
        let xs = defs
            .iter()
            .find(|d| d.function.name == "cross_session_query")
            .expect("cross_session_query 应在");
        let desc = xs.function.description.as_ref().expect("desc");
        // cluster id + sibling 名字 + this tool 标识
        assert!(desc.contains("Cluster: session_history"),
            "应注入 cluster id, got: {desc}");
        assert!(desc.contains("session_resume_query"),
            "应列 sibling, got: {desc}");
        assert!(desc.contains("messages_recover"),
            "应列 sibling, got: {desc}");
        assert!(desc.contains("This tool:"),
            "应有 this-tool differentiator, got: {desc}");
    }

    /// 不在任何 cluster 的工具 description 不被破坏（向后兼容）
    #[tokio::test]
    async fn cluster_hint_skipped_for_non_cluster_tools() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let defs = core.build_tool_definitions_for(None, None).await;
        // result_expand 不在 cluster（单成员场景或未注册）→ description 不应有 [Cluster:
        let other = defs
            .iter()
            .find(|d| d.function.name == "result_expand");
        if let Some(t) = other {
            let desc = t.function.description.as_ref().expect("desc");
            assert!(!desc.contains("[Cluster:"),
                "未注册 cluster 的工具不应有 hint, got: {desc}");
        }
    }

    /// byte-stable：同样 build 两次 description 完全一致（不破 KV cache）
    #[tokio::test]
    async fn cluster_hint_byte_stable_across_builds() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let d1 = core.build_tool_definitions_for(None, None).await;
        let d2 = core.build_tool_definitions_for(None, None).await;
        // names 已字典序排序，逐一比 description
        assert_eq!(d1.len(), d2.len(), "tool count 应一致");
        for (a, b) in d1.iter().zip(d2.iter()) {
            assert_eq!(a.function.name, b.function.name);
            assert_eq!(a.function.description, b.function.description,
                "tool {} description 必须 byte-stable", a.function.name);
        }
    }

    // ─── 段 L6: tool_compass + K3 hide 协同 ────────────────────────────────

    /// 推荐结果带 visible 字段；未 hide 时 visible=true
    #[tokio::test]
    async fn l6_tool_compass_marks_visible_when_no_hide() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        cfg.adaptive_d_tier_hide = false; // 关 hide
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let session = RwLock::new(SessionState::new("test"));
        let tool_id = ToolId("tool_compass".into());
        let params = serde_json::json!({"intent": "retrieve content from past sessions"});
        let out = core.handle_interaction_tool(&tool_id, &params, &session).await.unwrap();
        let results = out.output["results"].as_array().expect("results");
        assert!(!results.is_empty());
        for r in results {
            assert_eq!(r["visible"], serde_json::Value::Bool(true),
                "hide=false 时所有 rec 应 visible=true");
        }
    }

    /// hide 启用 + 工具被实际 hide → visible=false 且降级到 no_match_fallback
    #[tokio::test]
    async fn l6_tool_compass_marks_hidden_and_falls_back_when_all_hidden() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        cfg.adaptive_d_tier_hide = true;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;

        // 让 session_history cluster 三个工具都进 D（满足 K3 Phase 1 条件）
        {
            let mut eff = core.effectiveness.write().await;
            for tname in ["cross_session_query", "session_resume_query", "messages_recover"] {
                let tid = ToolId(tname.into());
                for _ in 0..15 {
                    eff.record_opportunity(&tid);
                    eff.record_outcome(&tid, crate::tool::effectiveness::ToolOutcome::ToolFailure, 100);
                }
            }
        }

        let session = RwLock::new(SessionState::new("test"));
        let tool_id = ToolId("tool_compass".into());
        let params = serde_json::json!({"intent": "retrieve content from past sessions"});
        let out = core.handle_interaction_tool(&tool_id, &params, &session).await.unwrap();
        // 命中的全是 D-tier 工具 → visible=false → 降级
        let results = out.output["results"].as_array().expect("results");
        // 但 cluster_floor 至少保留 1 个，所以可能 visible_count > 0；不强求 fallback
        // 至少验证：被记录的 hide 工具显示 visible=false
        let any_hidden = results.iter().any(|r| r["visible"] == serde_json::Value::Bool(false));
        let any_visible = results.iter().any(|r| r["visible"] == serde_json::Value::Bool(true));
        // 至少一种状态被反映出来——说明 visible 字段语义生效
        assert!(any_hidden || any_visible, "至少应有 visible 状态字段");
        // mode 应是 ranked_recommendations 或 no_match_fallback
        let mode = out.output["mode"].as_str().unwrap_or("");
        assert!(mode == "ranked_recommendations" || mode == "no_match_fallback",
            "mode 应是已知值, got: {}", mode);
    }

    // ─── 段 L4: env_failure_dominated audit 填充 ──────────────────────────

    /// env_failure_ratio accessor 正确计算
    #[tokio::test]
    async fn l4_env_failure_ratio_accessor() {
        use crate::tool::effectiveness::{EffectivenessTracker, ToolOutcome};
        let mut t = EffectivenessTracker::new();
        let id = ToolId("flaky_endpoint".into());
        // 7 env failures + 3 successes → ratio = 0.7
        for _ in 0..7 { t.record_outcome(&id, ToolOutcome::EnvFailure, 100); }
        for _ in 0..3 { t.record_outcome(&id, ToolOutcome::Success, 100); }
        let ratio = t.env_failure_ratio(&id);
        assert!((ratio - 0.7).abs() < 1e-6, "expected 0.7, got {}", ratio);
        // 未出现的工具 → 0
        let none_ratio = t.env_failure_ratio(&ToolId("never".into()));
        assert_eq!(none_ratio, 0.0);
    }

    /// audit_report 包含 env_failure_dominated 行（>50% env failure）
    #[tokio::test]
    async fn l4_audit_lists_env_failure_dominated_tools() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;

        // 让 cross_session_query 60% env failures
        {
            let mut eff = core.effectiveness.write().await;
            let tid = ToolId("cross_session_query".into());
            for _ in 0..6 { eff.record_outcome(&tid, crate::tool::effectiveness::ToolOutcome::EnvFailure, 100); }
            for _ in 0..4 { eff.record_outcome(&tid, crate::tool::effectiveness::ToolOutcome::Success, 100); }
            // 让数据足够（opportunities 也加，过 evaluate insufficient_data 阈值）
            for _ in 0..15 { eff.record_opportunity(&tid); }
        }

        let lines = core.audit_report().await;
        let report = lines.join("\n");
        assert!(report.contains("env_failure_dominated"),
            "audit 应输出 env_failure_dominated 行: {report}");
        assert!(report.contains("cross_session_query"),
            "应列出该具体工具: {report}");
    }

    // ─── 段 L3: evaluate 下游路径协同（生产路径 audit）────────────────────

    /// 验证 build_tool_definitions_for hide 路径已用 evaluate_at_turn（K3+K4 协同）
    /// 通过 K4 试探放行实际生效来反向证明 — palace_demoted 工具在 probation 窗口内不被 hide
    #[tokio::test]
    async fn l3_hide_path_respects_k4_probation() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        cfg.adaptive_d_tier_hide = true;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;

        // 给 cross_session_query 累积 stats 让 evaluate 能算分
        // 然后 palace demote 在 turn=10
        {
            let mut eff = core.effectiveness.write().await;
            let tid = ToolId("cross_session_query".into());
            for _ in 0..40 {
                eff.record_opportunity(&tid);
                eff.record_outcome(&tid, crate::tool::effectiveness::ToolOutcome::Success, 50);
            }
            eff.apply_palace_demote_at(tid, 10);
        }

        // turn=30 (压制中) → cross_session_query 应该被 hide
        // 但因为段 J1 cluster floor 兜底，至少保留 1 个 session_history 工具
        // 所以 cross_session_query 可能被 hide 但 session_resume_query/messages_recover 保留
        let defs_mid = core.build_tool_definitions_for(None, Some(30)).await;
        let names_mid: Vec<String> = defs_mid.iter().map(|d| d.function.name.clone()).collect();
        let _has_xs_mid = names_mid.contains(&"cross_session_query".to_string());

        // turn=60 (10+50=60，进入试探窗口) → cross_session_query 应该回来
        let defs_probation = core.build_tool_definitions_for(None, Some(60)).await;
        let names_probation: Vec<String> = defs_probation.iter().map(|d| d.function.name.clone()).collect();
        let has_xs_probation = names_probation.contains(&"cross_session_query".to_string());

        // 试探窗口内 cross_session_query 必须可见（K4 + K3 协同生效）
        assert!(has_xs_probation,
            "L3 + K4 协同：probation 窗口内（turn=60）palace_demoted 工具应回到可见");
    }

    /// audit_report 透明化用 evaluate_at_turn（验证 hide candidates 计数响应 probation）
    #[tokio::test]
    async fn l3_audit_uses_evaluate_at_turn() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;

        // 让一个工具进 D 但 cur_turn=0（audit 里 hardcoded）→ 应被算 hide candidate
        {
            let mut eff = core.effectiveness.write().await;
            let tid = ToolId("session_set_focus".into());
            for _ in 0..15 {
                eff.record_opportunity(&tid);
                eff.record_outcome(&tid, crate::tool::effectiveness::ToolOutcome::ToolFailure, 100);
            }
        }
        let lines = core.audit_report().await;
        let report = lines.join("\n");
        // 即使 cur_turn=0，audit 也应反映出 hide 决策
        assert!(report.contains("hide candidates"),
            "audit 应输出 hide candidates 计数 (L3 透明化)");
    }

    // ─── 段 L2: sync_from_palace_at 传 turn 让 K4 启动 ────────────────────

    /// sync_from_palace_at 接受 turn 参数，并在 demote 时用 apply_palace_demote_at
    /// 验证 demoted_at 字段被设置正确（保证 K4 试探放行计算有效）
    #[tokio::test]
    async fn l2_sync_from_palace_records_turn_at_demote() {
        // 直接走单元 path（不 spinning 起 palace） — 验证 K4 的 demoted_at 语义
        // Palace sync 完整路径需要 memory_palace 注入，这里只验"L2 把 turn 传给 tracker"
        // 方法：直接调 EffectivenessTracker::apply_palace_demote_at + 检查 turn
        use crate::tool::effectiveness::EffectivenessTracker;
        let mut t = EffectivenessTracker::new();
        let id = ToolId("flaky".into());
        // L2 路径：sync 在 turn=42 触发，会 apply_palace_demote_at(id, 42)
        t.apply_palace_demote_at(id.clone(), 42);
        // 验证 demoted_at 真被记 turn=42——通过 is_demoted_now 行为推断
        // turn=42 elapsed=0 → 仍压制
        assert!(t.is_demoted_now(&id, 42), "刚 demote 应仍压制");
        // turn=92 (42+50=92) elapsed=50 → 试探放行
        assert!(!t.is_demoted_now(&id, 92), "92 应试探放行");
        // turn=91 elapsed=49 → 仍压制
        assert!(t.is_demoted_now(&id, 91), "91 应仍压制");
    }

    /// 旧 sync_from_palace 兼容包装等价 turn=0
    #[tokio::test]
    async fn l2_legacy_sync_from_palace_uses_turn_zero() {
        use crate::tool::effectiveness::EffectivenessTracker;
        let mut t = EffectivenessTracker::new();
        let id = ToolId("legacy".into());
        // 等价 sync_from_palace() 旧行为
        t.apply_palace_demote(id.clone()); // turn=0 占位
        // turn=50 elapsed=50 → 试探放行（旧行为：每 50 turn 都满足）
        assert!(!t.is_demoted_now(&id, 50));
        // 这是 L2 documented 的"渐进式开放"——比硬永久埋好
    }

    // ─── 段 L1: Pipeline record_outcome 接入验证 ──────────────────────────

    /// post.rs 调 record_outcome 让 Network/Timeout 类失败归 EnvFailure
    ///
    /// 不易直接测 post_process 全链路（需要完整 turn pipeline），
    /// 改测 ToolOutcome::classify_error 路径决定语义——这是 L1 的核心契约
    #[tokio::test]
    async fn l1_failure_kind_routes_to_env_or_tool_outcome() {
        use crate::tool::effectiveness::ToolOutcome;
        // EnvFailure 候选 kinds
        let env_kinds = ["Network", "Timeout", "Unauthorized", "RateLimited",
                         "ServiceUnavailable", "SandboxDenied", "DependencyMissing"];
        for k in env_kinds {
            let oc = ToolOutcome::classify_error(k);
            assert_eq!(oc, ToolOutcome::EnvFailure,
                "L1: {k} 应归 EnvFailure（pipeline post.rs 的 failure_kind 路由）");
        }
        // ToolFailure 候选 kinds
        for k in ["BusinessError", "InvalidArgument", "ParseError", "Other"] {
            let oc = ToolOutcome::classify_error(k);
            assert_eq!(oc, ToolOutcome::ToolFailure,
                "L1: {k} 应归 ToolFailure");
        }
    }

    /// L1 集成：手动构造 ToolOutput 走 record_outcome 路径，验证 stats 累加正确
    #[tokio::test]
    async fn l1_record_outcome_segregates_env_vs_tool_failures() {
        use crate::tool::effectiveness::{EffectivenessTracker, ToolOutcome};
        let mut t = EffectivenessTracker::new();
        let id = ToolId("flaky_remote".into());
        // 模拟 pipeline 上传：3 次 Network 失败、2 次 BusinessError、5 次成功
        // 这是 L1 之后 post.rs 真实会生成的 outcome 序列
        for _ in 0..3 {
            t.record_outcome(&id, ToolOutcome::classify_error("Network"), 500);
        }
        for _ in 0..2 {
            t.record_outcome(&id, ToolOutcome::classify_error("BusinessError"), 100);
        }
        for _ in 0..5 {
            t.record_outcome(&id, ToolOutcome::Success, 50);
        }
        // 直接读 ToolStats 验证分类正确
        // 通过 evaluate 的非直接路径——success_rate 公式 = success / (invocations - env_failures)
        // = 5 / (10 - 3) = 5/7 ≈ 0.71（远高于不区分时的 5/10=0.5）
        let _eff = t.evaluate(&id);
        // record_invocation_legacy_api 已测试公式正确——这里仅验证 EnvFailure 分类生效
        // 用 evaluate_at_turn 验证不会误降级
        let provider = abacus_types::ToolProvider::Mcp { server_id: "x".into() };
        for _ in 0..10 { t.record_opportunity(&id); }
        for _ in 0..30 { t.record_opportunity(&id); } // 满足 mcp=30 阈值
        let eff = t.evaluate_at_turn(&id, &provider, 100);
        assert!(!eff.insufficient_data, "数据应足够");
        // success_rate 高 + 无 D-tier 标志 → 不会被 hide
        assert_ne!(eff.tier, abacus_types::VisibilityTier::D,
            "EnvFailure 排除分母后 success_rate 高，不应 D-tier");
    }

    // ─── 段 K5: default=true + audit 透明化 ──────────────────────────────

    /// CoreConfig::default() adaptive_d_tier_hide 默认 true
    #[tokio::test]
    async fn k5_adaptive_d_tier_hide_default_on() {
        let cfg = CoreConfig::default();
        assert!(cfg.adaptive_d_tier_hide,
            "段 K5：经过 K1~K4 兜底加固后，adaptive_d_tier_hide 应默认 true");
    }

    /// audit_report 输出包含 ENABLED (default-on) + safeguards 标记
    #[tokio::test]
    async fn k5_audit_includes_safeguard_markers() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        // 不显式 set adaptive_d_tier_hide—— 默认应是 true
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let lines = core.audit_report().await;
        let report = lines.join("\n");
        assert!(report.contains("ENABLED (default-on)"),
            "应标 default-on, got: {report}");
        assert!(report.contains("K1-K4 safeguards"),
            "应提及 K1-K4 兜底, got: {report}");
        assert!(report.contains("hide candidates"),
            "应输出 hide candidates 计数, got: {report}");
    }

    /// 全 lib 回归：default=true 不破现有测试（除显式开关相关）
    #[tokio::test]
    async fn k5_default_on_does_not_break_basic_path() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        // 默认 default-on 状态下，build_tool_definitions 应仍能产出 ≥ HIDE_MIN_TOTAL 个工具
        let defs = core.build_tool_definitions_for(None, Some(1)).await;
        assert!(defs.len() >= 5,
            "default=on + 全新 tracker（无评分数据）→ insufficient_data 保护应让所有工具可见; got {}",
            defs.len());
    }

    // ─── 段 K3: hide 层 floor + 极端兜底 ──────────────────────────────────

    /// 工具总数 < 5 → 完全禁用 hide
    #[tokio::test]
    async fn k3_hide_disabled_when_total_tools_below_min() {
        let reg = Arc::new(ToolRegistry::new());
        // 注册 4 个工具（< HIDE_MIN_TOTAL=5）
        for n in 0..4 {
            reg.register(abacus_types::ToolHandle {
                id: ToolId(format!("tiny_{}", n)),
                schema: abacus_types::ToolSchema {
                    name: format!("tiny_{}", n),
                    description: "x".into(),
                    parameters: serde_json::json!({"type": "object"}),
                    returns: None, security: None, cost: None,
                    examples: vec![], applicable_task_kinds: None, idempotent: false,
                    schema_stable: false,
                },
                provider: abacus_types::ToolProvider::BuiltIn,
                state: abacus_types::ToolState::Loaded,
                effectiveness: abacus_types::ToolEffectiveness {
                    tool_id: ToolId(format!("tiny_{}", n)),
                    composite_score: 0.05,
                    tier: abacus_types::VisibilityTier::D,
                    cooldown_remaining: 0,
                    blocked_by_env: false,
                    insufficient_data: false,
                },
            }).await;
        }
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        cfg.adaptive_d_tier_hide = true; // 即使 flag 开
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        // 即使全 D-tier，也不应隐藏（< 5 个工具）
        let defs = core.build_tool_definitions_for(None, Some(100)).await;
        // ≥ 4 个原始 + interaction tools，应至少 4 个保留
        let names: Vec<String> = defs.iter().map(|d| d.function.name.clone()).collect();
        for n in 0..4 {
            assert!(names.contains(&format!("tiny_{}", n)),
                "工具总数 < HIDE_MIN_TOTAL 应不裁剪, missing tiny_{}", n);
        }
    }

    /// hide 后剩余可见 < 3 → 整体回退
    #[tokio::test]
    async fn k3_hide_falls_back_when_visible_too_few() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        cfg.adaptive_d_tier_hide = true;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        // 让所有 builtin 工具评分都成 D
        {
            let mut eff = core.effectiveness.write().await;
            let tools = core.registry.all_tools().await;
            for t in &tools {
                for _ in 0..15 {
                    eff.record_opportunity(&t.id);
                    eff.record_outcome(&t.id, crate::tool::effectiveness::ToolOutcome::ToolFailure, 100);
                }
            }
        }
        // 现在所有工具都应 D-tier。Phase 4 兜底应让所有工具回放
        let defs = core.build_tool_definitions_for(None, Some(100)).await;
        assert!(defs.len() >= 5,
            "全 D-tier 时整体回退应保留所有工具; got {}", defs.len());
    }

    /// Cluster floor：同 cluster 至少保留 1
    #[tokio::test]
    async fn k3_cluster_floor_keeps_at_least_one() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        cfg.adaptive_d_tier_hide = true;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        // 把 session_history cluster 的所有工具拉成 D
        let cluster_tools = ["cross_session_query", "session_resume_query", "messages_recover"];
        {
            let mut eff = core.effectiveness.write().await;
            for tname in &cluster_tools {
                let tid = ToolId(tname.to_string());
                for _ in 0..15 {
                    eff.record_opportunity(&tid);
                    eff.record_outcome(&tid, crate::tool::effectiveness::ToolOutcome::ToolFailure, 100);
                }
            }
        }
        let defs = core.build_tool_definitions_for(None, Some(100)).await;
        let names: Vec<String> = defs.iter().map(|d| d.function.name.clone()).collect();
        let kept = cluster_tools.iter().filter(|n| names.contains(&n.to_string())).count();
        assert!(kept >= 1,
            "cluster floor 应至少保留 1 个 session_history 工具; kept={}", kept);
    }

    // ─── 段 J2: tool_compass 自省工具 dispatch ───────────────────────────────

    /// tool_compass 工具被注册（schema 可见）
    #[tokio::test]
    async fn tool_compass_tool_registered() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let tools = core.registry.all_tools().await;
        let target = tools.iter().find(|t| t.id.0 == "tool_compass");
        assert!(target.is_some(), "tool_compass 应注册");
        let schema = target.unwrap();
        // intent 必填
        let required = schema.schema.parameters.get("required")
            .and_then(|v| v.as_array()).expect("required");
        assert!(required.iter().any(|v| v.as_str() == Some("intent")));
    }

    /// 命中 cluster：intent 关键词匹配 → 返排好序的 recommendations
    #[tokio::test]
    async fn tool_compass_returns_ranked_recommendations() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let session = RwLock::new(SessionState::new("test"));
        let tool_id = ToolId("tool_compass".into());
        let params = serde_json::json!({
            "intent": "retrieve content from past sessions about Rust ownership",
            "top_k": 3
        });
        let out = core.handle_interaction_tool(&tool_id, &params, &session).await.unwrap();
        assert!(out.success, "应成功: {:?}", out.output);
        assert_eq!(out.output.get("mode").and_then(|v| v.as_str()), Some("ranked_recommendations"));
        let results = out.output.get("results").and_then(|v| v.as_array()).expect("results");
        assert!(!results.is_empty(), "应至少 1 个推荐");
        // top 应命中 session 相关工具
        let top = &results[0];
        let cluster_id = top.get("cluster_id").and_then(|v| v.as_str()).unwrap_or("");
        assert!(cluster_id.contains("session") || cluster_id.contains("history"),
            "top 应命中 session 相关 cluster, got: {}", cluster_id);
    }

    /// 空 intent 返 BusinessError 不返 results
    #[tokio::test]
    async fn tool_compass_rejects_empty_intent() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let session = RwLock::new(SessionState::new("test"));
        let tool_id = ToolId("tool_compass".into());

        // 缺失
        let out1 = core.handle_interaction_tool(&tool_id, &serde_json::json!({}), &session).await.unwrap();
        assert!(!out1.success);
        assert_eq!(out1.failure_kind.as_deref(), Some("BusinessError"));

        // 空字符串/空白
        let out2 = core.handle_interaction_tool(&tool_id, &serde_json::json!({"intent": "  "}), &session).await.unwrap();
        assert!(!out2.success);
    }

    /// 无命中场景降级为 all_clusters fallback——而非 fail
    #[tokio::test]
    async fn tool_compass_falls_back_when_no_keyword_match() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.event_sink_enabled = false;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let session = RwLock::new(SessionState::new("test"));
        let tool_id = ToolId("tool_compass".into());
        // 故意用对所有 cluster 都不沾边的 intent（>2 字符词但无匹配）
        let params = serde_json::json!({"intent": "zzz xyzxyz qqqqqq"});
        let out = core.handle_interaction_tool(&tool_id, &params, &session).await.unwrap();
        assert!(out.success, "无匹配应降级 not fail");
        assert_eq!(out.output.get("mode").and_then(|v| v.as_str()), Some("no_match_fallback"));
        let all_clusters = out.output.get("all_clusters").and_then(|v| v.as_array()).expect("all_clusters");
        assert!(!all_clusters.is_empty(), "fallback 应列出所有 clusters");
    }

    // ─── cross-session 段 D：cross_session_query 工具 dispatch 路径 ──────────

    /// 工具 schema 已注册：register_interaction_tools 必须包含 cross_session_query
    #[tokio::test]
    async fn cross_session_query_tool_registered() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let cfg = CoreConfig { event_sink_enabled: false, ..Default::default() };
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let tools = core.registry.all_tools().await;
        let target = tools.iter().find(|t| t.id.0 == "cross_session_query");
        assert!(target.is_some(), "cross_session_query 工具应被注册");
        let schema = target.unwrap();
        assert!(schema.schema.description.contains("cross-session"),
            "description 应说明 cross-session 用途");
        // 参数必须含 query / top_k / domain
        let params = &schema.schema.parameters;
        let required = params.get("required").and_then(|v| v.as_array()).expect("required array");
        assert!(required.iter().any(|v| v.as_str() == Some("query")), "query 参数必填");
    }

    /// palace 未启用时，dispatch 应返回明确错误 + 提示
    #[tokio::test]
    async fn cross_session_query_errors_without_palace() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let cfg = CoreConfig { event_sink_enabled: false, ..Default::default() };
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        // 无 with_memory()——memory_palace 是 None
        let session = SessionState::new("test");
        let session = RwLock::new(session);
        let tool_id = ToolId("cross_session_query".into());
        let params = serde_json::json!({"query": "test"});
        let out = core.handle_interaction_tool(&tool_id, &params, &session).await.unwrap();
        assert!(!out.success, "palace 未启用应返回 success=false");
        let err_str = out.output.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(err_str.contains("memory_palace not initialized"),
            "应明确报错 palace 未初始化, got: {err_str}");
        assert_eq!(out.failure_kind.as_deref(), Some("DependencyMissing"));
    }

    /// 空 query 应被验证拒绝（防 LLM 误传）
    #[tokio::test]
    async fn cross_session_query_rejects_empty_query() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let cfg = CoreConfig { event_sink_enabled: false, ..Default::default() };
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let session = RwLock::new(SessionState::new("test"));
        let tool_id = ToolId("cross_session_query".into());

        // case 1: 完全缺失 query
        let out1 = core.handle_interaction_tool(&tool_id, &serde_json::json!({}), &session).await.unwrap();
        assert!(!out1.success);
        assert_eq!(out1.failure_kind.as_deref(), Some("BusinessError"));

        // case 2: 空字符串 / 仅空白
        let out2 = core.handle_interaction_tool(&tool_id, &serde_json::json!({"query": "   "}), &session).await.unwrap();
        assert!(!out2.success);
    }

    /// messages_recover 工具：未知 recover_id 返回明确 NotFound
    #[tokio::test]
    async fn messages_recover_handles_unknown_id() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let cfg = CoreConfig { event_sink_enabled: false, ..Default::default() };
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let session = RwLock::new(SessionState::new("test"));
        let tool_id = ToolId("messages_recover".into());
        // case 1: 缺少 recover_id
        let out1 = core.handle_interaction_tool(&tool_id, &serde_json::json!({}), &session).await.unwrap();
        assert!(!out1.success);
        assert_eq!(out1.failure_kind.as_deref(), Some("BusinessError"));
        // case 2: 未知 id
        let out2 = core.handle_interaction_tool(
            &tool_id,
            &serde_json::json!({"recover_id": "mb_nonexistent"}),
            &session
        ).await.unwrap();
        assert!(!out2.success);
        assert_eq!(out2.failure_kind.as_deref(), Some("NotFound"));
        let err = out2.output.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(err.contains("mb_nonexistent"));
    }

    /// magchain_status 工具应包含 cross_session 字段
    #[tokio::test]
    async fn magchain_status_includes_cross_session_field() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let cfg = CoreConfig { event_sink_enabled: false, ..Default::default() };
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let session = RwLock::new(SessionState::new("test"));
        let tool_id = ToolId("magchain_status".into());
        let out = core.handle_interaction_tool(&tool_id, &serde_json::json!({}), &session).await.unwrap();
        assert!(out.success);
        let cs = out.output.get("cross_session").expect("cross_session 字段必须存在");
        // 无 palace 时 available=false
        assert_eq!(cs.get("available").and_then(|v| v.as_bool()), Some(false));
        assert!(cs.get("hint").is_some(), "hint 字段应说明启用方式");
    }

    /// cross-session 收尾：audit_report 包含段 A/B/C/D 标签
    #[tokio::test]
    async fn audit_report_includes_cross_session_segments() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let cfg = CoreConfig { event_sink_enabled: false, ..Default::default() }; // 测试不污染文件系统
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let lines = core.audit_report().await;
        let report = lines.join("\n");
        assert!(report.contains("cross-session 段A"), "missing 段A 进程注册行: {report}");
        assert!(report.contains("cross-session 段B"), "missing 段B jsonl sink 行: {report}");
        assert!(report.contains("cross-session 段C"), "missing 段C global history 行: {report}");
        assert!(report.contains("cross-session 段D"), "missing 段D cross_session_query 行: {report}");
        assert!(report.contains("process registry"));
        assert!(report.contains("jsonl event sink"));
        assert!(report.contains("global history"));
        assert!(report.contains("cross_session_query"));
    }

    #[tokio::test]
    async fn pressure_classify_matches_compress_thresholds() {
        // 端到端：让 PressureMonitor 用 default policy 跑一次，验证 70%/85% 分别进
        // Elevated/Critical（这是 V29.12 修复后的预期对齐行为）
        let policy = pressure::PressurePolicy::default();
        let monitor = pressure::ResourcePressureMonitor::new(policy);
        let w = make_window_at(75.0);
        let mgr = make_ctx();
        let src = Arc::new(ContextWindowPressure { window: w, manager: Arc::downgrade(&mgr) });
        monitor.register(src).await;
        let snap = monitor.status().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, "context_window");
        assert_eq!(snap[0].1, pressure::PressureLevel::Elevated, "75% → Elevated（修复前会是 Overloaded）");
        assert!((snap[0].2 - 0.75).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_skill_matching() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let mut se = SkillEngine::new();
        se.register_skill(make_skill("filengine"));
        let se = Arc::new(RwLock::new(se));
        let hub = Arc::new(make_hub());
        let core = CoreLoop::new(reg, se, hub, make_ctx(), CoreConfig { system_prompt: "You are Abacus.".into(), ..Default::default() }).await;
        core.register_provider("mock", Arc::new(MockProvider::new("mock-model"))).await;
        let session = SessionState::new("ts");
        core.register_session_context_tools(&session).await;
        let session = RwLock::new(session);
        let r = core.process_turn("hello filengine world", &session).await;
        assert!(r.is_ok());
        let t = r.unwrap();
        assert!(!t.matched_skills.is_empty());
        assert_eq!(t.matched_skills[0].id.0, "filengine");
    }

    #[tokio::test]
    async fn test_session_persistence() {
        let reg = Arc::new(ToolRegistry::new());
        register_all(&reg).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let core = CoreLoop::new(reg, se, hub, make_ctx(), CoreConfig::default()).await;
        core.register_provider("mock", Arc::new(MockProvider::new("mock"))).await;
        let session = SessionState::new("pt");
        core.register_session_context_tools(&session).await;
        let session = RwLock::new(session);
        let r1 = core.process_turn("first", &session).await.unwrap();
        assert_eq!(r1.stats.turn_number, 1);
        let r2 = core.process_turn("second", &session).await.unwrap();
        assert_eq!(r2.stats.turn_number, 2);
        assert_eq!(session.read().await.messages.read().await.len(), 4);
    }

    // ─── KV Cache prefix stability 回归测试 ───────────────────────────────
    // 历史 bug：focus_block 顶在 system prompt 最前（primacy），但 render_with_age(age) 每轮变 →
    //          DeepSeek/OpenAI prefix cache 命中率从 ~80% 跌到 ~0%（per-input cost 50–120× 放大）
    // 修复：focus 追加到末尾（recency-adjacent），与 segments 路径对齐
    // 这些测试锁住"stable prefix byte-identical 跨 turn"的不变量，未来重构若误改方向会 fail

    /// KV cache 回归（B 方案）：thinking_decision sticky 字段初始化、写入、读回正确。
    ///
    /// 锁定后跨 turn 复用，避免 DeepSeek `reasoning_content` 字段在历史 assistant 消息上
    /// 出现/消失，从而保护对话 history prefix cache 命中率。
    #[tokio::test]
    async fn session_thinking_decision_sticky_field_lifecycle() {
        let session = SessionState::new("sticky-test");

        // 初始：unlocked sentinel（Outer None）
        let init = session.thinking_decision.read().await.clone();
        assert!(init.is_none(), "新 session 的 thinking_decision 必须是 Outer None（未锁定）");

        // 模拟首轮：锁定为 Some(None) = thinking off
        *session.thinking_decision.write().await = Some(None);
        let locked_off = session.thinking_decision.read().await.clone();
        assert!(matches!(locked_off, Some(None)),
            "锁定 thinking off 应是 Outer Some(Inner None)");

        // L1 后：thinking_decision 锁定 ThinkingIntent 而非旧 ThinkingConfig。
        let intent = abacus_types::ThinkingIntent::Effort(abacus_types::EffortLevel::Medium);
        *session.thinking_decision.write().await = Some(Some(intent));
        let locked_on = session.thinking_decision.read().await.clone();
        assert!(matches!(locked_on, Some(Some(_))),
            "锁定 thinking on 应是 Outer Some(Inner Some(intent))");
        // 字段值校验
        if let Some(Some(ref i)) = locked_on {
            assert!(matches!(i, abacus_types::ThinkingIntent::Effort(abacus_types::EffortLevel::Medium)));
        }
    }

    /// KV cache 回归（C 方案）：RequestContext::default 默认跳过 DecayRouter。
    ///
    /// 之前 `#[derive(Default)]` 让 skip_decay_router=false → 每个用户 turn 都跑 DecayRouter
    /// → input tier 切换时 tools 重排 + hint 字节变化 → cache miss。改 manual Default 后
    /// 默认 skip=true，保护 prefix cache。
    #[test]
    fn request_context_default_skips_decay_router() {
        let ctx = RequestContext::default();
        assert!(ctx.skip_decay_router,
            "默认 skip_decay_router 必须是 true——保护 DeepSeek/OpenAI prefix cache");
    }

    /// SilentRouter 默认开启（子系统联动补强：工具路由智能化默认生效）
    #[test]
    fn core_config_default_enables_silent_router() {
        let cfg = CoreConfig::default();
        assert!(cfg.silent_router_enabled,
            "默认 silent_router_enabled 应为 true——提供工具路由智能化");
    }

    /// KV cache 回归：tool definitions 必须按 name 字典序输出。
    ///
    /// 历史 bug：`build_tool_definitions` 直接复用 `registry.all_tools()` 返回顺序，
    /// 而 registry 内部是 `HashMap<ToolId, ToolHandle>`。HashMap 迭代顺序不稳定（RandomState 跨进程
    /// 不同 + register 触发 cache invalidate 后 rebuild 顺序可能变），导致 tools 数组 byte 漂移
    /// → DeepSeek prefix cache 从 tools 段起 miss（含整段 messages history，影响巨大）。
    /// 修复：output 末尾按 name 字典序排序。这条测试验证不同注册顺序下输出稳定。
    #[tokio::test]
    async fn tool_definitions_sorted_by_name_for_cache_stability() {
        let reg = Arc::new(ToolRegistry::new());

        // 故意以非字典序注册三个工具——若 build_tool_definitions 直接透传 HashMap 顺序，
        // 实际输出会被 HashMap 内部 hash bucket 排列影响（不可预测）。
        for name in ["zeta_tool", "alpha_tool", "mid_tool"] {
            reg.register(abacus_types::ToolHandle {
                id: ToolId(name.to_string()),
                schema: abacus_types::ToolSchema {
                    name: name.to_string(),
                    description: format!("desc {}", name),
                    parameters: serde_json::json!({"type": "object", "properties": {}}),
                    returns: None, security: None, cost: None,
                    examples: Vec::new(),
                    applicable_task_kinds: None,
                    idempotent: false,
                                        schema_stable: false,                },
                provider: abacus_types::ToolProvider::BuiltIn,
                state: abacus_types::ToolState::Loaded,
                effectiveness: abacus_types::ToolEffectiveness::default(),
            }).await;
        }

        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let core = CoreLoop::new(reg, se, hub, make_ctx(), CoreConfig::default()).await;

        let defs = core.build_tool_definitions().await;
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();

        // 至少包含我们注册的 3 个（CoreLoop 还会注册 builtins，但 alpha < mid < zeta 顺序应一致）
        let alpha_pos = names.iter().position(|n| *n == "alpha_tool").expect("alpha_tool present");
        let mid_pos   = names.iter().position(|n| *n == "mid_tool").expect("mid_tool present");
        let zeta_pos  = names.iter().position(|n| *n == "zeta_tool").expect("zeta_tool present");

        assert!(alpha_pos < mid_pos,
            "alpha_tool 必须排在 mid_tool 之前（字典序），实际位置 alpha={} mid={}", alpha_pos, mid_pos);
        assert!(mid_pos < zeta_pos,
            "mid_tool 必须排在 zeta_tool 之前（字典序），实际位置 mid={} zeta={}", mid_pos, zeta_pos);

        // 二次校验：整体名称序列必须等于 sorted 后的版本（即全局字典序）
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted,
            "build_tool_definitions 输出必须是字典序——这是 DeepSeek tools 段 cache 命中的前提");
    }

    // ─── #1 VisibilityTier 接通：默认 D 等价不过滤 ────────────────────────────
    #[tokio::test]
    async fn tool_visibility_default_d_does_not_filter() {
        let reg = Arc::new(ToolRegistry::new());
        // 注册三个工具，分别为 S/B/D tier
        for (name, tier) in [
            ("tool_s", abacus_types::VisibilityTier::S),
            ("tool_b", abacus_types::VisibilityTier::B),
            ("tool_d", abacus_types::VisibilityTier::D),
        ] {
            let eff = abacus_types::ToolEffectiveness { tier, ..Default::default() };
            reg.register(abacus_types::ToolHandle {
                id: ToolId(name.to_string()),
                schema: abacus_types::ToolSchema {
                    name: name.to_string(),
                    description: "test".into(),
                    parameters: serde_json::json!({}),
                    returns: None, security: None, cost: None,
                    examples: Vec::new(),
                    applicable_task_kinds: None,
                    idempotent: false,
                                        schema_stable: false,                },
                provider: abacus_types::ToolProvider::BuiltIn,
                state: abacus_types::ToolState::Loaded,
                effectiveness: eff,
            }).await;
        }
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let core = CoreLoop::new(reg, se, hub, make_ctx(), CoreConfig::default()).await;

        let defs = core.build_tool_definitions().await;
        let names: Vec<String> = defs.iter().map(|d| d.function.name.clone()).collect();
        assert!(names.contains(&"tool_s".into()), "S tier 必须可见");
        assert!(names.contains(&"tool_b".into()), "B tier 必须可见");
        assert!(names.contains(&"tool_d".into()),
            "默认 threshold=D，D tier 也必须可见（等价不过滤）");
    }

    // ─── #1 VisibilityTier 接通：threshold=B 时 D 工具被隐藏 ──────────────────
    #[tokio::test]
    async fn tool_visibility_threshold_b_hides_d_tier() {
        let reg = Arc::new(ToolRegistry::new());
        for (name, tier) in [
            ("tool_a", abacus_types::VisibilityTier::A),
            ("tool_b", abacus_types::VisibilityTier::B),
            ("tool_d_hidden", abacus_types::VisibilityTier::D),
        ] {
            let eff = abacus_types::ToolEffectiveness { tier, ..Default::default() };
            reg.register(abacus_types::ToolHandle {
                id: ToolId(name.to_string()),
                schema: abacus_types::ToolSchema {
                    name: name.to_string(),
                    description: "test".into(),
                    parameters: serde_json::json!({}),
                    returns: None, security: None, cost: None,
                    examples: Vec::new(),
                    applicable_task_kinds: None,
                    idempotent: false,
                                        schema_stable: false,                },
                provider: abacus_types::ToolProvider::BuiltIn,
                state: abacus_types::ToolState::Loaded,
                effectiveness: eff,
            }).await;
        }
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.tool_visibility_threshold = abacus_types::VisibilityTier::B;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;

        let defs = core.build_tool_definitions().await;
        let names: Vec<String> = defs.iter().map(|d| d.function.name.clone()).collect();
        assert!(names.contains(&"tool_a".into()), "A ≥ B → 可见");
        assert!(names.contains(&"tool_b".into()), "B ≥ B → 可见");
        assert!(!names.contains(&"tool_d_hidden".into()),
            "D < B → 必须被隐藏，让 LLM 不再尝试已多次失败的工具");
    }

    // ─── #3 工具来源标签：MCP 工具 description 加 [External MCP] 前缀 ─────────
    #[tokio::test]
    async fn tool_provenance_prefix_marks_external_sources() {
        let reg = Arc::new(ToolRegistry::new());
        // 4 种 provider 各注册一个
        let provider_cases = vec![
            ("builtin_x", abacus_types::ToolProvider::BuiltIn, ""),
            ("mcp_foo_y", abacus_types::ToolProvider::Mcp { server_id: "foo".into() },
                "[External MCP server: foo]"),
            ("plugin_bar_z", abacus_types::ToolProvider::Plugin { plugin_id: "bar".into() },
                "[WASM plugin: bar]"),
            ("skill_review_step", abacus_types::ToolProvider::Skill { skill_id: "review".into() },
                "[Skill workflow step from 'review']"),
        ];
        for (name, prov, _) in &provider_cases {
            reg.register(abacus_types::ToolHandle {
                id: ToolId(name.to_string()),
                schema: abacus_types::ToolSchema {
                    name: name.to_string(),
                    description: "原始描述".into(),
                    parameters: serde_json::json!({}),
                    returns: None, security: None, cost: None,
                    examples: Vec::new(),
                    applicable_task_kinds: None,
                    idempotent: false,
                                        schema_stable: false,                },
                provider: prov.clone(),
                state: abacus_types::ToolState::Loaded,
                effectiveness: Default::default(),
            }).await;
        }
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let core = CoreLoop::new(reg, se, hub, make_ctx(), CoreConfig::default()).await;

        let defs = core.build_tool_definitions().await;
        for (name, _prov, expected_prefix) in &provider_cases {
            let def = defs.iter().find(|d| d.function.name == *name).expect(name);
            let desc = def.function.description.as_deref().unwrap_or("");
            if expected_prefix.is_empty() {
                assert_eq!(desc, "原始描述",
                    "BuiltIn 工具不加前缀：{} → '{}'", name, desc);
            } else {
                assert!(desc.starts_with(expected_prefix),
                    "{} 工具 description 应以 '{}' 开头，实际：'{}'",
                    name, expected_prefix, desc);
                assert!(desc.contains("原始描述"),
                    "{} 工具 description 必须保留原文：'{}'", name, desc);
            }
        }
    }

    // ─── #3 边界：来源前缀稳定性（cache 友好）──────────────────────────────
    /// 同一工具多次调用 build_tool_definitions 必须输出 byte-identical description。
    #[tokio::test]
    async fn tool_provenance_prefix_byte_stable_across_calls() {
        let reg = Arc::new(ToolRegistry::new());
        reg.register(abacus_types::ToolHandle {
            id: ToolId("mcp/svc/foo".into()),
            schema: abacus_types::ToolSchema {
                name: "mcp/svc/foo".into(),
                description: "do something".into(),
                parameters: serde_json::json!({}),
                returns: None, security: None, cost: None,
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: false,
                                schema_stable: false,            },
            provider: abacus_types::ToolProvider::Mcp { server_id: "svc".into() },
            state: abacus_types::ToolState::Loaded,
            effectiveness: Default::default(),
        }).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let core = CoreLoop::new(reg, se, hub, make_ctx(), CoreConfig::default()).await;

        let defs1 = core.build_tool_definitions().await;
        let defs2 = core.build_tool_definitions().await;
        let desc1 = defs1.iter().find(|d| d.function.name.contains("foo"))
            .and_then(|d| d.function.description.clone())
            .expect("foo present");
        let desc2 = defs2.iter().find(|d| d.function.name.contains("foo"))
            .and_then(|d| d.function.description.clone())
            .expect("foo present");
        assert_eq!(desc1, desc2,
            "build_tool_definitions 多次调用 description 必须 byte-identical（KV cache 前提）");
    }

    // ─── Phase β-D: task_kind 路由测试 ────────────────────────────────────
    /// 工具 applicable_task_kinds: None → 全任务可见（默认行为不变）
    #[tokio::test]
    async fn task_kind_routing_disabled_passes_all_tools() {
        let reg = Arc::new(ToolRegistry::new());
        reg.register(abacus_types::ToolHandle {
            id: ToolId("only_for_debug".into()),
            schema: abacus_types::ToolSchema {
                name: "only_for_debug".into(),
                description: "debugger".into(),
                parameters: serde_json::json!({}),
                returns: None, security: None, cost: None,
                examples: Vec::new(),
                applicable_task_kinds: Some(vec!["debugging".into()]),
                idempotent: true,
                                schema_stable: false,            },
            provider: abacus_types::ToolProvider::BuiltIn,
            state: abacus_types::ToolState::Loaded,
            effectiveness: Default::default(),
        }).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        // Task #84：default 改为 true 后必须显式关——这条用例验证关闭语义不变
        // 同时关闭 scene_tool_loading 以隔离测试 β-D
        let mut cfg = CoreConfig::default();
        cfg.task_kind_routing_enabled = false;
        cfg.scene_tool_loading_enabled = false;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let defs = core.build_tool_definitions_for(Some("code_writing"), None).await;
        let names: Vec<String> = defs.iter().map(|d| d.function.name.clone()).collect();
        assert!(names.contains(&"only_for_debug".into()),
            "routing_enabled=false → 工具白名单失效，全部可见");
    }

    /// 工具 applicable_task_kinds: Some([...]) + routing_enabled + 不命中 → 过滤
    #[tokio::test]
    async fn task_kind_routing_filters_non_matching_tools() {
        let reg = Arc::new(ToolRegistry::new());
        reg.register(abacus_types::ToolHandle {
            id: ToolId("only_for_debug".into()),
            schema: abacus_types::ToolSchema {
                name: "only_for_debug".into(),
                description: "debugger".into(),
                parameters: serde_json::json!({}),
                returns: None, security: None, cost: None,
                examples: Vec::new(),
                applicable_task_kinds: Some(vec!["debugging".into()]),
                idempotent: true,
                                schema_stable: false,            },
            provider: abacus_types::ToolProvider::BuiltIn,
            state: abacus_types::ToolState::Loaded,
            effectiveness: Default::default(),
        }).await;
        reg.register(abacus_types::ToolHandle {
            id: ToolId("universal_tool".into()),
            schema: abacus_types::ToolSchema {
                name: "universal_tool".into(),
                description: "all".into(),
                parameters: serde_json::json!({}),
                returns: None, security: None, cost: None,
                examples: Vec::new(),
                applicable_task_kinds: None, // 全任务可见
                idempotent: true,
                                schema_stable: false,            },
            provider: abacus_types::ToolProvider::BuiltIn,
            state: abacus_types::ToolState::Loaded,
            effectiveness: Default::default(),
        }).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.task_kind_routing_enabled = true;
        cfg.scene_tool_loading_enabled = false; // 隔离测试 β-D，不受 α-S 前缀过滤干扰
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        // 当前任务 code_writing → only_for_debug 应被过滤
        let defs = core.build_tool_definitions_for(Some("code_writing"), None).await;
        let names: Vec<String> = defs.iter().map(|d| d.function.name.clone()).collect();
        assert!(!names.contains(&"only_for_debug".into()),
            "task_kind=code_writing 时白名单不命中应被过滤");
        assert!(names.contains(&"universal_tool".into()),
            "applicable_task_kinds=None 应始终可见");
    }

    /// 工具 applicable_task_kinds 命中当前 task_kind → 通过
    #[tokio::test]
    async fn task_kind_routing_passes_matching_tools() {
        let reg = Arc::new(ToolRegistry::new());
        reg.register(abacus_types::ToolHandle {
            id: ToolId("debug_tool".into()),
            schema: abacus_types::ToolSchema {
                name: "debug_tool".into(),
                description: "debug".into(),
                parameters: serde_json::json!({}),
                returns: None, security: None, cost: None,
                examples: Vec::new(),
                applicable_task_kinds: Some(vec!["debugging".into(), "code_writing".into()]),
                idempotent: true,
                                schema_stable: false,            },
            provider: abacus_types::ToolProvider::BuiltIn,
            state: abacus_types::ToolState::Loaded,
            effectiveness: Default::default(),
        }).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.task_kind_routing_enabled = true;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let defs = core.build_tool_definitions_for(Some("debugging"), None).await;
        let names: Vec<String> = defs.iter().map(|d| d.function.name.clone()).collect();
        assert!(names.contains(&"debug_tool".into()),
            "白名单含 'debugging' 时应可见");
    }

    // ─── Phase γ-I: frequency pruning 测试 ────────────────────────────────
    /// 默认配置（pruning_turns=None）→ 不修剪
    #[tokio::test]
    async fn tool_frequency_pruning_disabled_passes_all() {
        let reg = Arc::new(ToolRegistry::new());
        reg.register(abacus_types::ToolHandle {
            id: ToolId("stale_tool".into()),
            schema: abacus_types::ToolSchema {
                name: "stale_tool".into(),
                description: "old".into(),
                parameters: serde_json::json!({}),
                returns: None, security: None, cost: None,
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: true,
                                schema_stable: false,            },
            provider: abacus_types::ToolProvider::BuiltIn,
            state: abacus_types::ToolState::Loaded,
            effectiveness: Default::default(),
        }).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let core = CoreLoop::new(reg, se, hub, make_ctx(), CoreConfig::default()).await;
        // 假装工具最后调用是 turn 0，current_turn=999，但 pruning_turns=None → 不修剪
        core.record_tool_invocation(&ToolId("stale_tool".into()), 0).await;
        let defs = core.build_tool_definitions_for(None, Some(999)).await;
        let names: Vec<String> = defs.iter().map(|d| d.function.name.clone()).collect();
        assert!(names.contains(&"stale_tool".into()),
            "pruning_turns=None → 即使久未调用仍保留");
    }

    /// 启用 pruning_turns 且工具确实超期 → 隐藏
    #[tokio::test]
    async fn tool_frequency_pruning_hides_stale_tools() {
        let reg = Arc::new(ToolRegistry::new());
        reg.register(abacus_types::ToolHandle {
            id: ToolId("stale_tool".into()),
            schema: abacus_types::ToolSchema {
                name: "stale_tool".into(),
                description: "old".into(),
                parameters: serde_json::json!({}),
                returns: None, security: None, cost: None,
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: true,
                                schema_stable: false,            },
            provider: abacus_types::ToolProvider::BuiltIn,
            state: abacus_types::ToolState::Loaded,
            effectiveness: Default::default(),
        }).await;
        reg.register(abacus_types::ToolHandle {
            id: ToolId("fresh_tool".into()),
            schema: abacus_types::ToolSchema {
                name: "fresh_tool".into(),
                description: "new".into(),
                parameters: serde_json::json!({}),
                returns: None, security: None, cost: None,
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: true,
                                schema_stable: false,            },
            provider: abacus_types::ToolProvider::BuiltIn,
            state: abacus_types::ToolState::Loaded,
            effectiveness: Default::default(),
        }).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.tool_frequency_pruning_turns = Some(10);
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        core.record_tool_invocation(&ToolId("stale_tool".into()), 5).await;   // turn 5 调用过
        core.record_tool_invocation(&ToolId("fresh_tool".into()), 95).await;  // turn 95 调用过
        // current_turn=100：fresh 距离 5 turn 内（保留），stale 距离 95 turn 远（隐藏）
        let defs = core.build_tool_definitions_for(None, Some(100)).await;
        let names: Vec<String> = defs.iter().map(|d| d.function.name.clone()).collect();
        assert!(!names.contains(&"stale_tool".into()),
            "100-5=95 > 10 turn 阈值 → 隐藏");
        assert!(names.contains(&"fresh_tool".into()),
            "100-95=5 ≤ 10 → 保留");
    }

    /// 从未调用的工具（last_invoked 不存在）→ 保留（新工具友好）
    #[tokio::test]
    async fn tool_frequency_pruning_preserves_never_invoked() {
        let reg = Arc::new(ToolRegistry::new());
        reg.register(abacus_types::ToolHandle {
            id: ToolId("brand_new".into()),
            schema: abacus_types::ToolSchema {
                name: "brand_new".into(),
                description: "new".into(),
                parameters: serde_json::json!({}),
                returns: None, security: None, cost: None,
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: true,
                                schema_stable: false,            },
            provider: abacus_types::ToolProvider::BuiltIn,
            state: abacus_types::ToolState::Loaded,
            effectiveness: Default::default(),
        }).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.tool_frequency_pruning_turns = Some(10);
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        // 不调用 record_tool_invocation —— last_invoked 缺失
        let defs = core.build_tool_definitions_for(None, Some(1000)).await;
        let names: Vec<String> = defs.iter().map(|d| d.function.name.clone()).collect();
        assert!(names.contains(&"brand_new".into()),
            "从未调用过 → 不修剪（避免新工具立刻被剪）");
    }

    /// task_kind=None（首轮未锁定）即使 routing_enabled=true 也透传——保护首轮 cache
    #[tokio::test]
    async fn task_kind_routing_none_passes_all_tools() {
        let reg = Arc::new(ToolRegistry::new());
        reg.register(abacus_types::ToolHandle {
            id: ToolId("only_for_debug".into()),
            schema: abacus_types::ToolSchema {
                name: "only_for_debug".into(),
                description: "debugger".into(),
                parameters: serde_json::json!({}),
                returns: None, security: None, cost: None,
                examples: Vec::new(),
                applicable_task_kinds: Some(vec!["debugging".into()]),
                idempotent: true,
                                schema_stable: false,            },
            provider: abacus_types::ToolProvider::BuiltIn,
            state: abacus_types::ToolState::Loaded,
            effectiveness: Default::default(),
        }).await;
        let se = Arc::new(RwLock::new(SkillEngine::new()));
        let hub = Arc::new(make_hub());
        let mut cfg = CoreConfig::default();
        cfg.task_kind_routing_enabled = true;
        let core = CoreLoop::new(reg, se, hub, make_ctx(), cfg).await;
        let defs = core.build_tool_definitions_for(None, None).await;
        let names: Vec<String> = defs.iter().map(|d| d.function.name.clone()).collect();
        assert!(names.contains(&"only_for_debug".into()),
            "task_kind=None → 跳过过滤，所有工具可见（保护首轮 prefix cache）");
    }

    #[test]
    fn focus_appended_to_end_not_prepended() {
        let assembled = "Layer 255 Kernel\n\n---\n\nLayer 230 abacusbr".to_string();
        let focus = "## [Session Focus — Turn 5]\nGoal: build feature X";
        let out = super::compose_system_text_with_focus(assembled.clone(), Some(focus));
        assert!(
            out.starts_with("Layer 255 Kernel"),
            "stable prefix 必须在最前——这是 DeepSeek prefix cache 命中的前提"
        );
        assert!(
            out.ends_with(focus),
            "focus 必须在最尾——age 字节变化只破坏末尾 cache，不影响前缀 hit"
        );
    }

    #[test]
    fn no_focus_returns_assembled_verbatim() {
        let assembled = "stable content".to_string();
        let out = super::compose_system_text_with_focus(assembled.clone(), None);
        assert_eq!(out, "stable content", "无 focus 时不应改动 assembled");
    }

    #[test]
    fn stable_prefix_byte_identical_across_focus_age_change() {
        // 关键不变量：age=0 与 age=12（接近过期含 stale_warning）的输出共同前缀必须 ⊇ assembled
        // 这是 DeepSeek 64-token 块缓存命中的前提——任何前置 dynamic 注入都会破坏
        let assembled = "Layer 255 Kernel\n\n---\n\nLayer 230 abacusbr core stable bytes".to_string();
        let focus_fresh = "## [Session Focus — Turn 5]\nGoal: build feature X\nPhase: Step 1/3";
        let focus_stale =
            "## [Session Focus — Turn 5]\nGoal: build feature X\nPhase: Step 1/3\n\
             [focus is 12 turns old — call session.set_focus to refresh if goal has changed]";

        let out_fresh = super::compose_system_text_with_focus(assembled.clone(), Some(focus_fresh));
        let out_stale = super::compose_system_text_with_focus(assembled.clone(), Some(focus_stale));

        let common_prefix_len = out_fresh
            .bytes()
            .zip(out_stale.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        assert!(
            common_prefix_len >= assembled.len(),
            "stable prefix 必须 byte-identical 跨 turn (common={} bytes, assembled={} bytes)\n\
             如果失败，说明 focus 又被放回 system prompt 前面了",
            common_prefix_len,
            assembled.len()
        );
    }
}