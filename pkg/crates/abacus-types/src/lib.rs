pub mod model;
pub mod model_preference;
pub mod model_registry;
pub mod abacus_mode;
pub mod error;
pub mod engine;
pub mod sandbox;
pub mod progressive;
pub mod user_profile {
    //! UserProfile — 用户单源真相 (Single Source of Truth)
    use serde::{Deserialize, Serialize};
    use std::collections::HashSet;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct UserProfile {
        pub display_name: String,
        pub default_model: String,
        pub thinking: String,
        pub autonomy: String,
        pub safe_operations: HashSet<String>,
        pub safe_shell_prefixes: Vec<String>,
        pub tool_timeout_secs: u64,
        pub max_tool_calls_per_turn: u32,
        pub context_window_tokens: usize,
        pub ui: UiPreferences,
        pub features: FeatureFlags,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct UiPreferences {
        pub theme: String,
        pub language: String,
        pub info_density: String,
        pub streaming: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct FeatureFlags {
        pub silent_router: bool,
        pub auto_mode_switch: bool,
        pub cross_session_memory: bool,
        pub adaptive_progress: bool,
        pub workflow_engine: bool,
        pub usage_driven_reveal: bool,
    }

    impl Default for UserProfile {
        fn default() -> Self {
            Self {
                display_name: "User".into(),
                default_model: "auto".into(),
                thinking: "off".into(),
                autonomy: "confirm_sensitive".into(),
                safe_operations: HashSet::new(),
                safe_shell_prefixes: vec![],
                tool_timeout_secs: 60,
                max_tool_calls_per_turn: 500,
                context_window_tokens: 128_000,
                ui: UiPreferences {
                    theme: "dark".into(), language: "zh".into(),
                    info_density: "normal".into(), streaming: true,
                },
                features: FeatureFlags {
                    silent_router: true, auto_mode_switch: true,
                    cross_session_memory: true, adaptive_progress: true,
                    workflow_engine: false, usage_driven_reveal: true,
                },
            }
        }
    }

    impl UserProfile {
        pub fn load(path: impl AsRef<std::path::Path>) -> Self {
            let path = path.as_ref();
            if path.exists() {
                match std::fs::read_to_string(path) {
                    Ok(content) => match serde_json::from_str(&content) {
                        Ok(profile) => return profile,
                        Err(e) => tracing::warn!("UserProfile parse error: {e}"),
                    },
                    Err(e) => tracing::warn!("UserProfile read error: {e}"),
                }
            }
            Self::default()
        }

        pub fn save(&self, path: impl AsRef<std::path::Path>) {
            let path = path.as_ref();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(path, json);
            }
        }

        pub fn default_path() -> std::path::PathBuf {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".abacus").join("profile.json")
        }

        pub fn load_default() -> Self { Self::load(Self::default_path()) }

        pub fn requires_confirmation(&self, tool_id: &str) -> bool {
            match self.autonomy.as_str() {
                "full" => false,
                "manual" | "confirm_all" => true,
                _ => {
                    let s = matches!(tool_id, "fs_write" | "fs_move"
                        | "fs_mkdir" | "bash_exec" | "web_fetch");
                    s && !self.safe_operations.contains(tool_id)
                }
            }
        }
    }
}
pub mod collections;

// V33: AbacusMode + ModeArtifact 统一导出
pub use abacus_mode::{AbacusMode, ModeArtifact};

pub use collections::BoundedFifo;

// model re-exports
pub use model::{
    ModelId,
    ProviderId,
    QualifiedModelId,
    CapabilitySet,
    SchemaFormat,
    RateLimits,
    LatencyTier,
    ThinkingEffort,
    ThinkingIntent,
    EffortLevel,
    ThinkingModeKind,
    MultiTurnReplay,
    ThinkingCapabilities,
    ModelThinkingConfig,
    ModelSpec,
    Pricing,
    lookup_pricing,
    InlineModelSpec,
};

// model_preference re-exports
pub use model_preference::{
    ModelPreference,
    preference_file_path,
    load_from_file as load_model_preference,
    save_to_file as save_model_preference,
};

// V31: 新模型注册表（按 model_id 维度的 SSoT）— 价格 / 能力 / 限制 / 生命周期聚合
pub use model_registry::{
    ModelInfo,
    lookup_model,
    lookup_model_or_default,
    all_models,
    models_by_provider,
    DEFAULT_CNY_TO_USD_RATE,
};

// error re-exports
pub use user_profile::{
    UserProfile,
    UiPreferences,
    FeatureFlags,
};

pub use error::{
    KernelError,
    Result,
};

// engine re-exports
pub use engine::{
    ToolId,
    ToolSchema,
    ToolSecurity,
    ToolCost,
    TriggerPattern,
    ToolProvider,
    ToolState,
    ToolEffectiveness,
    VisibilityTier,
    ToolHandle,
    SkillId,
    SkillDef,
    SkillTriggers,
    SkillStep,
    SkillExperience,
    Sm2State,
    SkillExecutionRecord,
    CapabilityDeclaration,
    CapabilityRequest,
    CapabilityKind,
    CapabilityContext,
    PluginManifest,
    PluginSignature,
    PluginToolSpec,
    ToolExample,
    ToolOutput,
    TurnStats,
    UserRole,
    ServerId,
    McpConfig,
    // V35: Role 能力系统
    RoleCapabilities,
    BashPolicyLevel,
    SearchProvider,
    // Multi-provider configuration
    ProviderEntry,
    ProviderType,
    ModelEntry,
};

// sandbox re-exports
pub use sandbox::{
    ModelAssignment,
    Criterion,
    CriterionKind,
    StepState,
    PhaseState,
    TaskState,
    SandboxEvent,
    SandboxEventKind,
    StepSpec,
    PhaseSpec,
    TaskSpec,
    SandboxConfig,
};

// progressive re-exports
pub use progressive::{
    OutputStrategy,
    Checklist,
    ChecklistItem,
    ChecklistCategory,
    DecisionBlock,
    DecisionOption,
    UserResponse,
    SectionPlan,
    SectionStatus,
    ProgressiveState,
    OutputAction,
    AutonomyLevel,
    TimeoutBehavior,
    ComplexityProfile,
    ComplexityDimensions,
    GateScope,
    ExecutionEstimate,
    DeterministicEstimate,
    ModelDependentEstimate,
    EstimateMethod,
    HumanEquivalent,
    EstimateConfidence,
    ProgressiveEvent,
    Deviation,
    DeviationSeverity,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kernel_error_display() {
        let err = KernelError::Provider("test error".into());
        assert!(err.to_string().contains("test error"));
    }

    #[test]
    fn test_tool_id_display() {
        let id = ToolId("fs_read".to_string());
        assert_eq!(id.0, "fs_read");
    }

    #[test]
    fn test_thinking_effort_from_str() {
        assert_eq!(ThinkingEffort::from_str_loose("high"), Some(ThinkingEffort::High));
        assert_eq!(ThinkingEffort::from_str_loose("medium"), Some(ThinkingEffort::Medium));
        assert_eq!(ThinkingEffort::from_str_loose("low"), Some(ThinkingEffort::Low));
        assert_eq!(ThinkingEffort::from_str_loose("unknown"), None);
    }

    #[test]
    fn test_thinking_intent_from_str() {
        // 标准档位
        assert_eq!(ThinkingIntent::from_str_loose("off"), Some(ThinkingIntent::Off));
        assert_eq!(ThinkingIntent::from_str_loose("adaptive"), Some(ThinkingIntent::Adaptive));
        assert_eq!(ThinkingIntent::from_str_loose("auto"), Some(ThinkingIntent::Adaptive));
        assert_eq!(
            ThinkingIntent::from_str_loose("minimal"),
            Some(ThinkingIntent::Effort(EffortLevel::Minimal))
        );
        assert_eq!(
            ThinkingIntent::from_str_loose("high"),
            Some(ThinkingIntent::Effort(EffortLevel::High))
        );
        assert_eq!(
            ThinkingIntent::from_str_loose("max"),
            Some(ThinkingIntent::Effort(EffortLevel::Max))
        );
        assert_eq!(
            ThinkingIntent::from_str_loose("xhigh"),
            Some(ThinkingIntent::Effort(EffortLevel::XHigh))
        );
        // 整数 → Budget；0 特殊化为 Off
        assert_eq!(ThinkingIntent::from_str_loose("8192"), Some(ThinkingIntent::Budget(8192)));
        assert_eq!(ThinkingIntent::from_str_loose("0"), Some(ThinkingIntent::Off));
        // 未知值
        assert_eq!(ThinkingIntent::from_str_loose("nonsense"), None);
    }

    #[test]
    fn test_thinking_intent_round_trip() {
        for s in ["off", "adaptive", "minimal", "low", "medium", "high", "max", "xhigh"] {
            let intent = ThinkingIntent::from_str_loose(s).expect(s);
            assert_eq!(intent.to_str(), s, "round-trip failed for {s}");
        }
        assert_eq!(ThinkingIntent::Budget(4096).to_str(), "4096");
    }

    #[test]
    fn test_thinking_effort_lifts_to_intent() {
        // 旧 → 新单向有损 lift
        assert_eq!(
            ThinkingIntent::from(ThinkingEffort::Off),
            ThinkingIntent::Off
        );
        assert_eq!(
            ThinkingIntent::from(ThinkingEffort::Low),
            ThinkingIntent::Effort(EffortLevel::Low)
        );
        assert_eq!(
            ThinkingIntent::from(ThinkingEffort::High),
            ThinkingIntent::Effort(EffortLevel::High)
        );
    }

    #[test]
    fn test_effort_level_rank_order() {
        // 用于降级路径：rank 单调递增保证"找最近支持档"
        assert!(EffortLevel::Minimal.rank() < EffortLevel::Low.rank());
        assert!(EffortLevel::Low.rank() < EffortLevel::Medium.rank());
        assert!(EffortLevel::Medium.rank() < EffortLevel::High.rank());
        assert!(EffortLevel::High.rank() < EffortLevel::Max.rank());
        assert!(EffortLevel::Max.rank() < EffortLevel::XHigh.rank());
    }

    #[test]
    fn test_thinking_capabilities_default_is_none() {
        let caps = ThinkingCapabilities::default();
        assert!(!caps.is_supported());
        assert!(!caps.supports_adaptive());
        assert!(!caps.supports_budget());
        assert_eq!(caps.multi_turn_replay, MultiTurnReplay::None);
    }

    #[test]
    fn test_thinking_capabilities_predicates() {
        let caps = ThinkingCapabilities {
            supported_modes: vec![
                ThinkingModeKind::AdaptiveEffort,
                ThinkingModeKind::ExtendedBudget,
            ],
            default_mode: Some(ThinkingModeKind::AdaptiveEffort),
            effort_levels: vec![EffortLevel::Low, EffortLevel::High],
            budget_range: Some((1024, 64000)),
            multi_turn_replay: MultiTurnReplay::None,
        };
        assert!(caps.is_supported());
        assert!(caps.supports_adaptive());
        assert!(caps.supports_budget());
    }

    #[test]
    fn test_model_spec_default_has_no_thinking_capabilities() {
        // 保守默认：未声明的模型不被假设支持思考
        let spec = ModelSpec::default();
        assert!(!spec.thinking_capabilities.is_supported());
    }

    #[test]
    fn test_result_ok() {
        // 该测试故意构造已知 Ok 值，clippy::unnecessary_literal_unwrap 在此场景属于反向风险——
        // 直接 .unwrap() 测的就是 unwrap 行为本身。allow 是显式选择。
        #[allow(clippy::unnecessary_literal_unwrap)]
        let v: i32 = {
            let result: Result<i32> = Ok(42);
            result.unwrap()
        };
        assert_eq!(v, 42);
    }

    #[test]
    fn test_result_err() {
        let result: Result<i32> = Err(KernelError::Validation("bad input".into()));
        assert!(result.is_err());
    }
}
