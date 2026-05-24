//! SubsystemPolicy —— 工具子系统注册策略
//!
//! ## 设计动机（Layer 3）
//! 当前 register_all 是"All In"模式——即使用户不启用 LSP/MCP/Plugin/Skill，
//! 这些工具的 schema 仍然全部注册，浪费 LLM 视野中的位置（最高占 1500 tokens/轮）。
//!
//! 本模块强制每个新增子系统在 register_all 时显式声明策略：
//! - `Always`：核心能力，无条件注册（如 filengine.fs.* / db.* / kb.*）
//! - `Lazy`：仅 enable_X() 时注册（如 LSP / MCP / WASM Plugin / Skill workflow）
//! - `Adaptive`：根据运行时信号动态决定（如基于 hit rate）
//!
//! ## 引用关系
//! - 被 [`crate::tool::builtin::register_all`] 调用 —— 决定哪些子系统执行 register
//! - 被 [`crate::core::CoreLoop::audit_optimizations`] 读取 —— 报告各子系统状态
//!
//! ## 防御未来 bug
//! 新增子系统的开发者必须 `match` 所有 RegistrationMode 变体（编译器强制），
//! 不会出现"忘记决定"的情况。

use abacus_types::ToolId;
use std::collections::{HashMap, HashSet};

/// 子系统注册策略
#[derive(Debug, Clone)]
pub enum RegistrationMode {
    /// 总是注册（核心能力）
    Always,
    /// 仅 enable_X() 时注册（外部依赖 / 安全敏感）
    Lazy,
    /// 自适应：基于历史命中率决定
    ///
    /// `min_hit_rate` —— 0.0~1.0，低于此率则不注册（隐藏避免噪声）
    /// 当前为占位语义 —— Layer 4 接 TokenBudgetMonitor 后实装
    Adaptive { min_hit_rate: f64 },
}

/// 单个子系统的策略声明
#[derive(Debug, Clone)]
pub struct SubsystemDecl {
    pub name: &'static str,
    pub mode: RegistrationMode,
    /// 子系统含的工具 prefix（如 "filengine_fs_", "lsp."）
    pub tool_prefix: &'static str,
    /// 简短描述（audit 报告用）
    pub description: &'static str,
}

/// 全局 SubsystemRegistry —— 所有内置子系统的策略声明
///
/// 引用关系：register_all 内部消费；audit_optimizations 读取上报。
/// 添加新子系统时在此处加 entry，编译期保证不遗漏 mode 决策。
pub fn builtin_subsystems() -> Vec<SubsystemDecl> {
    vec![
        SubsystemDecl {
            name: "filengine.fs",
            mode: RegistrationMode::Always,
            tool_prefix: "filengine_fs_",
            description: "filesystem ops (read/write/grep/etc.) — core capability",
        },
        SubsystemDecl {
            name: "filengine.web",
            mode: RegistrationMode::Always,
            tool_prefix: "filengine_web_",
            description: "web fetch/search — core capability",
        },
        SubsystemDecl {
            name: "filengine.bash",
            mode: RegistrationMode::Always,
            tool_prefix: "filengine_bash_",
            description: "bash execution — core capability",
        },
        SubsystemDecl {
            name: "code",
            mode: RegistrationMode::Always,
            tool_prefix: "code_",
            description: "Rhai script executor — core",
        },
        SubsystemDecl {
            name: "db",
            mode: RegistrationMode::Always,
            tool_prefix: "db_",
            description: "SQLite query/CRUD — core",
        },
        SubsystemDecl {
            name: "kb",
            mode: RegistrationMode::Always,
            tool_prefix: "kb_",
            description: "knowledge base search/ingest — core",
        },
        SubsystemDecl {
            name: "orchestrate",
            mode: RegistrationMode::Always,
            tool_prefix: "orchestrate_",
            description: "task assess/upgrade — core",
        },
        SubsystemDecl {
            name: "result",
            mode: RegistrationMode::Always,
            tool_prefix: "result_",
            description: "result store expand — core",
        },
        SubsystemDecl {
            name: "lsp",
            mode: RegistrationMode::Lazy,
            tool_prefix: "lsp_",
            description: "LSP integration (10 tools) — needs enable_lsp",
        },
        SubsystemDecl {
            name: "mcp",
            mode: RegistrationMode::Lazy,
            tool_prefix: "mcp_",
            description: "MCP servers — needs enable_mcp(configs)",
        },
        SubsystemDecl {
            name: "plugin",
            mode: RegistrationMode::Lazy,
            tool_prefix: "plugin_",
            description: "WASM plugins — needs enable_plugins_with_options(...)",
        },
        SubsystemDecl {
            name: "skill",
            mode: RegistrationMode::Lazy,
            tool_prefix: "skill_",
            description: "Skill workflow steps — needs enable_skill_workflow_executor + load_skill",
        },
    ]
}

/// W1 (Task #99)：子系统热度信号源
///
/// `Adaptive { min_hit_rate }` 路径用此 trait 决定是否注册——返回 [0.0, 1.0] 评分。
///
/// ## 实现指南
/// 评分语义应当是**子系统级 adoption rate**，即"该子系统的工具被 LLM 选用的频率"，
/// 而非单工具的 success rate。具体公式由实现决定（见 `EffectivenessHeatProvider`）。
///
/// ## 默认行为
/// `NoopHeatProvider` 始终返回 0.0——配合 Adaptive 阈值 > 0.0 即等价于"永不注册"，
/// 与改造前的占位语义一致。
pub trait SubsystemHeatProvider: Send + Sync {
    /// 查询某子系统的热度评分。
    ///
    /// `name` 子系统名（如 "lsp"）；`prefix` tool_id 前缀（如 "lsp."）——实现可二选一使用。
    fn heat(&self, name: &str, prefix: &str) -> f64;
}

/// 占位实现：始终返回 0——保持改造前的行为（Adaptive 永不注册）。
pub struct NoopHeatProvider;

impl SubsystemHeatProvider for NoopHeatProvider {
    fn heat(&self, _name: &str, _prefix: &str) -> f64 {
        0.0
    }
}

/// 基于 EffectivenessTracker 的真实实现
///
/// ## 评分公式
/// `heat = (sum_invocations / sum_opportunities) * mean_success_rate`
///
/// - 分子项 = 子系统级 adoption rate（聚合所有匹配工具的命中机会）
/// - 分母项 = 平均 success rate（避免"被调用很多但都失败"的子系统得高分）
///
/// 不直接 mean(per-tool-adoption)，因为那会让"很多冷工具 + 一个热工具"的子系统失真。
pub struct EffectivenessHeatProvider<'a> {
    pub stats: &'a HashMap<ToolId, crate::tool::effectiveness::ToolStats>,
}

impl<'a> EffectivenessHeatProvider<'a> {
    pub fn new(stats: &'a HashMap<ToolId, crate::tool::effectiveness::ToolStats>) -> Self {
        Self { stats }
    }
}

impl<'a> SubsystemHeatProvider for EffectivenessHeatProvider<'a> {
    fn heat(&self, _name: &str, prefix: &str) -> f64 {
        let mut sum_invocations = 0u64;
        let mut sum_opportunities = 0u64;
        let mut sum_successes = 0u64;
        let mut tool_count = 0u32;
        for (tool_id, stats) in self.stats.iter() {
            if tool_id.0.starts_with(prefix) {
                sum_invocations += stats.invocations;
                sum_opportunities += stats.opportunities;
                sum_successes += stats.successes;
                tool_count += 1;
            }
        }
        if tool_count == 0 || sum_opportunities == 0 {
            return 0.0;
        }
        let adoption = sum_invocations as f64 / sum_opportunities as f64;
        let success = if sum_invocations == 0 {
            // 全是 opportunity 但没人调——视为冷
            0.0
        } else {
            sum_successes as f64 / sum_invocations as f64
        };
        (adoption * success).clamp(0.0, 1.0)
    }
}

/// 检查给定 tool_id 是否属于"应当注册"的子系统（向后兼容入口）
///
/// `enabled_lazy` —— 当前已启用的 Lazy 子系统名集（如 `["lsp", "mcp"]`）
///
/// Adaptive 路径走 `NoopHeatProvider`——等价于改造前的占位行为（永不注册）。
/// 需要真实热度决策时改用 [`should_register_with_heat`]。
pub fn should_register(prefix: &str, enabled_lazy: &HashSet<&str>) -> bool {
    should_register_with_heat(prefix, enabled_lazy, &NoopHeatProvider)
}

/// W1 (Task #99) 实装入口：带 heat provider 的注册决策
///
/// ## 决策流
/// - `Always` → true
/// - `Lazy` → `enabled_lazy.contains(&name)`
/// - `Adaptive { min_hit_rate }` → `provider.heat(name, prefix) >= min_hit_rate`
///
/// ## KV cache 友好性
/// 调用方应在 CoreLoop::new 一次性求值——决定后整个 session 不切换，避免 tool list 抖动。
pub fn should_register_with_heat(
    prefix: &str,
    enabled_lazy: &HashSet<&str>,
    heat_provider: &dyn SubsystemHeatProvider,
) -> bool {
    let subsystems = builtin_subsystems();
    let decl = subsystems.iter().find(|s| s.tool_prefix == prefix);
    match decl {
        Some(d) => match d.mode {
            RegistrationMode::Always => true,
            RegistrationMode::Lazy => enabled_lazy.contains(&d.name),
            RegistrationMode::Adaptive { min_hit_rate } => {
                let h = heat_provider.heat(d.name, d.tool_prefix);
                h >= min_hit_rate
            }
        },
        // 未声明的子系统默认 Always（向后兼容；Lint 会标记）
        None => true,
    }
}

/// 反向工具：根据 tool_id 找所属子系统名
pub fn subsystem_of(tool_id: &ToolId) -> Option<&'static str> {
    let subsystems = builtin_subsystems();
    subsystems.iter()
        .find(|s| tool_id.0.starts_with(s.tool_prefix))
        .map(|s| s.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_subsystem_registers() {
        let enabled = HashSet::new();
        assert!(should_register("filengine_fs_", &enabled));
        assert!(should_register("db_", &enabled));
    }

    #[test]
    fn lazy_subsystem_skips_when_not_enabled() {
        let enabled = HashSet::new();
        assert!(!should_register("lsp_", &enabled));
        assert!(!should_register("mcp_", &enabled));
        assert!(!should_register("plugin_", &enabled));
    }

    #[test]
    fn lazy_subsystem_registers_when_enabled() {
        let mut enabled = HashSet::new();
        enabled.insert("lsp");
        assert!(should_register("lsp_", &enabled));
        assert!(!should_register("mcp_", &enabled), "mcp not in enabled set");
    }

    #[test]
    fn subsystem_of_lookups() {
        assert_eq!(subsystem_of(&ToolId("filengine_fs_read".into())), Some("filengine.fs"));
        assert_eq!(subsystem_of(&ToolId("lsp_hover".into())), Some("lsp"));
        assert_eq!(subsystem_of(&ToolId("unknown_x".into())), None);
    }

    // ─── W1 (Task #99) Adaptive 决策测试 ───────────────────────────────────

    use crate::tool::effectiveness::ToolStats;

    /// `NoopHeatProvider` 永远返回 0——配合 Adaptive 阈值 > 0 即等价于"永不注册"
    #[test]
    fn noop_heat_provider_returns_zero() {
        let p = NoopHeatProvider;
        assert_eq!(p.heat("lsp", "lsp."), 0.0);
        assert_eq!(p.heat("kb", "kb_"), 0.0);
    }

    /// 向后兼容：旧 should_register 等价 NoopHeatProvider 路径
    #[test]
    fn should_register_legacy_matches_noop_provider() {
        let enabled = HashSet::new();
        let p = NoopHeatProvider;
        for prefix in &["filengine_fs_", "lsp_", "kb_", "unknown_"] {
            assert_eq!(
                should_register(prefix, &enabled),
                should_register_with_heat(prefix, &enabled, &p),
                "prefix {} 行为应一致",
                prefix
            );
        }
    }

    /// EffectivenessHeatProvider：聚合 sum(invocations)/sum(opportunities) * mean(success)
    #[test]
    fn effectiveness_heat_aggregates_subsystem_level() {
        let mut stats = HashMap::new();
        stats.insert(
            ToolId("kb_search".into()),
            ToolStats { opportunities: 100, invocations: 50, successes: 40, total_latency_ms: 0, recent_exit_codes: vec![], env_failures: 0 },
        );
        stats.insert(
            ToolId("kb_ingest".into()),
            ToolStats { opportunities: 100, invocations: 30, successes: 30, total_latency_ms: 0, recent_exit_codes: vec![], env_failures: 0 },
        );
        // 不相关的工具不应影响
        stats.insert(
            ToolId("lsp_hover".into()),
            ToolStats { opportunities: 1000, invocations: 0, successes: 0, total_latency_ms: 0, recent_exit_codes: vec![], env_failures: 0 },
        );
        let p = EffectivenessHeatProvider::new(&stats);
        // kb 子系统：(50+30)/(100+100) = 0.4 adoption；(40+30)/(50+30) = 0.875 success
        // heat = 0.4 * 0.875 = 0.35
        let h = p.heat("kb", "kb_");
        assert!((h - 0.35).abs() < 1e-6, "expected ~0.35, got {}", h);
    }

    #[test]
    fn effectiveness_heat_zero_when_no_opportunities() {
        let stats = HashMap::new();
        let p = EffectivenessHeatProvider::new(&stats);
        assert_eq!(p.heat("kb", "kb_"), 0.0);
    }

    #[test]
    fn effectiveness_heat_zero_when_invocations_zero() {
        let mut stats = HashMap::new();
        stats.insert(
            ToolId("kb_search".into()),
            ToolStats { opportunities: 100, invocations: 0, successes: 0, total_latency_ms: 0, recent_exit_codes: vec![], env_failures: 0 },
        );
        let p = EffectivenessHeatProvider::new(&stats);
        // adoption=0 → heat=0（无论 success_rate）
        assert_eq!(p.heat("kb", "kb_"), 0.0);
    }

    /// 注入一个测试用 mock provider 验证 Adaptive 决策路径
    struct MockHeatProvider {
        score: f64,
    }
    impl SubsystemHeatProvider for MockHeatProvider {
        fn heat(&self, _name: &str, _prefix: &str) -> f64 {
            self.score
        }
    }

    #[test]
    fn adaptive_below_threshold_returns_false() {
        // 临时声明用——should_register_with_heat 通过 prefix 反查 builtin_subsystems，
        // 当前没有 Adaptive 子系统，所以这里测试 mode 决策的对应分支需要 mock
        // 改用 enum 直接比对方式
        let mode = RegistrationMode::Adaptive { min_hit_rate: 0.5 };
        let p = MockHeatProvider { score: 0.3 };
        let pass = match mode {
            RegistrationMode::Adaptive { min_hit_rate } => p.heat("test", "test.") >= min_hit_rate,
            _ => true,
        };
        assert!(!pass, "score=0.3 < 0.5 应被拒绝");
    }

    #[test]
    fn adaptive_above_threshold_returns_true() {
        let mode = RegistrationMode::Adaptive { min_hit_rate: 0.3 };
        let p = MockHeatProvider { score: 0.5 };
        let pass = match mode {
            RegistrationMode::Adaptive { min_hit_rate } => p.heat("test", "test.") >= min_hit_rate,
            _ => false,
        };
        assert!(pass, "score=0.5 ≥ 0.3 应通过");
    }

    /// 兜底：未声明子系统默认 Always，与 heat 无关
    #[test]
    fn unknown_subsystem_defaults_to_always_regardless_of_heat() {
        let enabled = HashSet::new();
        let p = NoopHeatProvider;
        assert!(should_register_with_heat("unknown_new_", &enabled, &p));
    }

    /// Always 子系统不消费 heat——保 KV cache 稳定
    #[test]
    fn always_subsystem_ignores_heat_provider() {
        let enabled = HashSet::new();
        // 给一个永远 0 的 provider，但 Always 子系统仍应注册
        let p = NoopHeatProvider;
        assert!(should_register_with_heat("filengine_fs_", &enabled, &p));
        assert!(should_register_with_heat("kb_", &enabled, &p));
    }

    /// Lazy 子系统不消费 heat——只看 enabled_lazy 集合
    #[test]
    fn lazy_subsystem_ignores_heat_provider() {
        let mut enabled = HashSet::new();
        enabled.insert("lsp");
        // 高 heat 也不能让 mcp 注册（因为不在 enabled）
        struct HighHeat;
        impl SubsystemHeatProvider for HighHeat {
            fn heat(&self, _: &str, _: &str) -> f64 {
                1.0
            }
        }
        assert!(should_register_with_heat("lsp_", &enabled, &HighHeat));
        assert!(!should_register_with_heat("mcp_", &enabled, &HighHeat));
    }
}
