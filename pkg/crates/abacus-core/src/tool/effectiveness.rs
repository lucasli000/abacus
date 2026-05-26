//! Tool effectiveness scoring and visibility management
//!
//! Provides [`EffectivenessTracker`] which computes a 5-tier composite score
//! for each tool, driving `VisibilityTier` assignment and prompt exposure strategy.
//!
//! ## Scoring Formula
//!
//! `composite_score = adoption_rate × 0.50 + trend × 0.25 + success_rate × 0.15 + latency_score × 0.10`
//!
//! - **adoption_rate**: invocations / opportunities (how often the LLM chooses this tool)
//! - **trend**: recent vs overall success rate shift (positive = improving)
//! - **success_rate**: successful invocations / total invocations
//! - **latency_score**: 1.0 - (avg_latency / 5000), clamped [0, 1]
//!
//! ## Tier Mapping
//!
//! | Score Range | VisibilityTier | Behavior |
//! |-------------|----------------|----------|
//! | >= 0.80 | S | Always visible, no cooldown |
//! | >= 0.60 | A | Visible, no cooldown |
//! | >= 0.40 | B | Visible, no cooldown |
//! | >= 0.20 | C | Visible, 10-turn cooldown |
//! | < 0.20 | D | Visible, 30-turn cooldown |
//!
//! ## Data Requirements
//!
//! First `min_samples` (default: 10) opportunities are considered "insufficient data"
//! and return a default A-tier score. User favorites bypass all scoring at S-tier.
//!
//! ## Known Limitations
//!
//! - `record_opportunity` is called once per turn for all visible tools before the
//!   LLM call. `record_invocation` is called after each tool execution. This means
//!   tools the LLM chooses not to call still count as opportunities but no invocations,
//!   which correctly penalizes low-adoption tools.
//! - Trend calculation uses only the last 10 exit codes.

use std::collections::HashMap;
use abacus_types::{ToolEffectiveness, ToolId, ToolProvider, VisibilityTier};

/// 段 K2: 按 ToolProvider 类别返回 min_samples 阈值
///
/// ## 设计动机
/// BuiltIn 工具天然在 abacus 内部，从一开始就用真实业务逻辑跑——10 次 opportunity
/// 足够判断；MCP/Plugin/Skill 是远端/动态加载，cold-start 期失败概率高（auth 摸索、
/// network warm-up、依赖加载），过早评分容易误判 D-tier 进而隐藏。
///
/// 给扩展工具更长的"试用期"（30 次 opportunity）让它们有机会摆脱误判。
///
/// ## 引用关系
/// - 调用：evaluate_with_provider(tool_id, provider) 获取阈值
/// - 调用方：CoreLoop::build_tool_definitions_for 在 hide 决策时传入 ToolHandle.provider
pub fn min_samples_for(provider: &ToolProvider) -> u64 {
    match provider {
        ToolProvider::BuiltIn => 10,
        // 扩展工具——网络/动态加载/远端，给更长冷启动期
        ToolProvider::Mcp { .. } | ToolProvider::Plugin { .. } | ToolProvider::Skill { .. } => 30,
    }
}

/// Per-tool statistics tracked by the effectiveness system.
#[derive(Debug, Clone, Default)]
pub struct ToolStats {
    /// Number of times this tool was visible (opportunity to be called)
    pub opportunities: u64,
    /// Number of times this tool was actually invoked by the LLM
    pub invocations: u64,
    /// Number of successful invocations
    pub successes: u64,
    /// Cumulative latency in milliseconds
    pub total_latency_ms: u64,
    /// Recent exit codes (last 10): 0 = success, 1 = failure
    pub recent_exit_codes: Vec<u32>,
    /// 段 K1: 环境失败次数（不记入 success_rate 分母）
    ///
    /// ## 引用关系
    /// - 写入：`record_invocation` 收到 `ToolOutcome::EnvFailure` 时累加
    /// - 读取：`success_rate()` 用 `invocations - env_failures` 作分母
    ///
    /// ## 设计动机
    /// MCP / Plugin 工具失败往往是环境问题（网络超时、auth 401、sandbox 拒绝）
    /// 而非工具本身设计有缺陷。把这类失败从 success_rate 排除，避免对扩展工具不公平
    /// 的"环境恶化即评分下降"循环。
    pub env_failures: u64,
}

/// 段 K1: 工具调用结果分类
///
/// ## 区分动机
/// 单纯的 success/failure 二分把环境失败（超时、auth）和工具逻辑失败混在一起，
/// 对 MCP/Plugin 工具特别不公平。引入第三类 `EnvFailure` 让评分系统能区分：
/// - `Success`：工具成功完成
/// - `ToolFailure`：工具自身错误（参数错、内部 bug）→ 计入 success_rate 分母
/// - `EnvFailure`：环境问题（网络/auth/sandbox/timeout）→ 不计入 success_rate
///
/// ## 引用关系
/// - 创建：CoreLoop pipeline 在工具执行返回时根据 KernelError 类型分类
/// - 消费：`EffectivenessTracker::record_invocation` 累加对应字段
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    /// 工具成功完成
    Success,
    /// 工具自身错误（参数验证失败 / 业务逻辑错 / panic 兜底）
    ToolFailure,
    /// 环境失败——非工具自身责任
    /// 包含：网络超时、auth 失败、sandbox 拒绝、底层服务不可达等
    EnvFailure,
}

impl ToolOutcome {
    /// 从 KernelError 推断 outcome
    ///
    /// ## 启发式
    /// - Network/Timeout/Unauthorized/RateLimited → EnvFailure
    /// - InvalidArgument/BusinessError/Other → ToolFailure
    /// - 调用方明确知道是 Success 时直接构造 ToolOutcome::Success（不走本函数）
    pub fn classify_error(err_kind: &str) -> Self {
        match err_kind {
            // 环境层失败——网络/认证/限流/sandbox/超时/panic
            "Network" | "Timeout" | "Unauthorized" | "RateLimited"
            | "ServiceUnavailable" | "SandboxDenied" | "DependencyMissing"
            | "Panic" => ToolOutcome::EnvFailure,
            // 其他 → 工具层失败
            _ => ToolOutcome::ToolFailure,
        }
    }
}


impl ToolStats {
    /// Ratio of invocations to opportunities (how often the LLM picks this tool).
    /// Returns 0.0 if no opportunities recorded.
    pub fn adoption_rate(&self) -> f64 {
        if self.opportunities == 0 { return 0.0; }
        self.invocations as f64 / self.opportunities as f64
    }

    /// Ratio of successes to (invocations - env_failures).
    /// Returns 1.0 if no eligible invocations recorded.
    ///
    /// ## 段 K1 公式调整
    /// 旧：success / invocations（环境失败被算做工具失败，对扩展工具不公平）
    /// 新：success / (invocations - env_failures)
    /// 当 env_failures >= invocations（极端环境恶化）→ 返 1.0（不评判，缺数据）
    pub fn success_rate(&self) -> f64 {
        let denom = self.invocations.saturating_sub(self.env_failures);
        if denom == 0 { return 1.0; }
        self.successes as f64 / denom as f64
    }

    /// Average latency per invocation in milliseconds.
    /// Returns 0.0 if no invocations recorded.
    pub fn avg_latency_ms(&self) -> f64 {
        if self.invocations == 0 { return 0.0; }
        self.total_latency_ms as f64 / self.invocations as f64
    }
}

/// Tracks and evaluates tool effectiveness for visibility tier assignment.
///
/// Maintains per-tool statistics and supports user favorites that bypass
/// normal scoring at S-tier.
///
/// Stats map is bounded at MAX_TRACKED_TOOLS; LFU eviction when full.
#[derive(Debug, Clone)]
pub struct EffectivenessTracker {
    stats: HashMap<ToolId, ToolStats>,
    user_favorites: Vec<ToolId>,
    min_samples: u64,
    /// Phase γ-Palace-C：行为宫殿同步而来的"降级标记"
    ///
    /// 标记的工具会在 `evaluate()` 时强制 tier=D，无视 stats 计算结果。
    /// 由 `CoreLoop::sync_from_palace()` 在 N turn 间隔批量写入，
    /// 同步信号：palace.behavior frequency >= 3 且 success_rate < 0.3。
    ///
    /// ## 段 K4 升级：HashSet<ToolId> → HashMap<ToolId, u64>
    /// 旧版只记录"是否被 demote"——单调降级无法自动恢复，对 MCP/插件极不友好
    /// （网络/auth 临时失败 ≥3 次永久埋）。新版 value 是 demote 时的 turn 号，
    /// 让 `is_palace_demoted_at_turn(turn)` 能算出"距上次 demote 多少 turn"，
    /// 每 PROBATION_INTERVAL_TURNS 试探放行 1 次（exploration arm）。
    /// 试探 turn 内若调用成功 → `clear_palace_demote_for(tool_id)` 自动清除。
    ///
    /// ## 引用关系
    /// - 写：`apply_palace_demote(tool_id, turn)` 由 sync_from_palace 调
    /// - 读：`is_demoted_now(tool_id, current_turn)` 评估每次 evaluate 时调
    /// - 清：用户显式 `clear_palace_demote()`；试探成功后 `clear_palace_demote_for(tool_id)`
    palace_demoted: std::collections::HashMap<ToolId, u64>,
}

/// 段 K4: 试探放行间隔（turn 数）
///
/// ## 设计选择
/// 50 太小→cache 抖动；100 太大→失误恢复慢。50 turn ≈ 普通 session 1/3，
/// 给降级工具足够"忘掉过去"的时间窗，又不会让用户长时间见不到工具。
/// 该常数公开，方便测试 + 配置覆写预留。
pub const PROBATION_INTERVAL_TURNS: u64 = 50;

/// Maximum number of tools tracked before eviction.
const MAX_TRACKED_TOOLS: usize = 200;

impl Default for EffectivenessTracker {
    fn default() -> Self { Self::new() }
}

impl EffectivenessTracker {
    /// Create a new tracker with default min_samples (10).
    pub fn new() -> Self {
        Self {
            stats: HashMap::new(),
            user_favorites: Vec::new(),
            min_samples: 10,
            palace_demoted: std::collections::HashMap::new(),
        }
    }

    /// Phase γ-Palace-C / 段 K4：行为宫殿信号驱动的降级（带 turn 号）
    ///
    /// ## 段 K4 升级
    /// 接受 `current_turn` 参数——记录降级时刻，让试探放行能算"距今多久"。
    /// 旧调用方（无 turn 信息）→ 用 `apply_palace_demote_legacy()` 兼容。
    ///
    /// 同 tool_id 多次降级会更新 turn（最近一次为准），等于"重置 probation 计时"。
    pub fn apply_palace_demote_at(&mut self, tool_id: ToolId, current_turn: u64) {
        self.palace_demoted.insert(tool_id, current_turn);
    }

    /// 兼容旧 API（不带 turn）
    ///
    /// 用 turn=0 占位——下次 `is_demoted_now(_, current)` 会算 current-0=current
    /// 距离，可能立即触发试探放行。这个行为对旧 API 是渐进式开放，没有错误风险。
    pub fn apply_palace_demote(&mut self, tool_id: ToolId) {
        self.apply_palace_demote_at(tool_id, 0);
    }

    /// 显式清除所有 palace 降级标记（管理员恢复用）
    pub fn clear_palace_demote(&mut self) {
        self.palace_demoted.clear();
    }

    /// 段 K4：试探成功后自动清除单个 tool 的降级标记
    pub fn clear_palace_demote_for(&mut self, tool_id: &ToolId) {
        self.palace_demoted.remove(tool_id);
    }

    /// 段 L4：环境失败比例 accessor —— 环境主导失败的工具识别
    ///
    /// ## 返回
    /// - `0.0` —— 无 invocation 或无 env_failure
    /// - `(0.0, 1.0]` —— env_failures / invocations 比例
    ///
    /// ## 用途
    /// audit_report 用此识别"该工具是否被环境拖累"——比例高（>0.5）意味着
    /// 工具自身可能没问题，但运行环境（网络/auth/sandbox）有持续问题，
    /// 应给运维提示而非降级该工具。
    pub fn env_failure_ratio(&self, tool_id: &ToolId) -> f64 {
        let stats = match self.stats.get(tool_id) {
            Some(s) => s,
            None => return 0.0,
        };
        if stats.invocations == 0 { return 0.0; }
        stats.env_failures as f64 / stats.invocations as f64
    }

    /// 段 L4：直接读 ToolStats 的副本（audit 用）
    pub fn stats_snapshot(&self, tool_id: &ToolId) -> Option<ToolStats> {
        self.stats.get(tool_id).cloned()
    }

    /// 查询某工具是否被 palace 降级（无视 probation 状态）
    pub fn is_palace_demoted(&self, tool_id: &ToolId) -> bool {
        self.palace_demoted.contains_key(tool_id)
    }

    /// 段 K4：基于 current_turn 决定 probation 状态
    ///
    /// ## 返回
    /// - `true` —— 当前 turn 仍处于"压制期"（仍然 hide）
    /// - `false` —— 当前 turn 落在"试探窗口"（让 LLM 重新看到这工具，给 1 次机会）
    ///
    /// ## 算法
    /// `(current_turn - demoted_at_turn) % PROBATION_INTERVAL_TURNS == 0`
    /// 即每 50 turn 周期性放行 1 turn。
    /// 边界：current_turn < demoted_at_turn → 视为压制（防 turn 倒退异常）
    pub fn is_demoted_now(&self, tool_id: &ToolId, current_turn: u64) -> bool {
        let demoted_at = match self.palace_demoted.get(tool_id) {
            Some(&t) => t,
            None => return false, // 未降级 → 不压制
        };
        if current_turn < demoted_at {
            return true; // turn 倒退异常 → 保守压制
        }
        let elapsed = current_turn - demoted_at;
        if elapsed == 0 {
            return true; // 刚降级即查询 → 压制
        }
        // 每 PROBATION_INTERVAL_TURNS 周期最后 1 turn 放行
        elapsed % PROBATION_INTERVAL_TURNS != 0
    }

    /// Evict least-used tools when stats map exceeds capacity.
    ///
    /// 触发时记录 warn 日志——帮助运维发现工具膨胀或监控数据丢失
    fn evict_if_needed(&mut self) {
        if self.stats.len() <= MAX_TRACKED_TOOLS {
            return;
        }
        // Remove tools with lowest invocations (LFU), excluding favorites
        let mut candidates: Vec<_> = self.stats.iter()
            .filter(|(id, _)| !self.user_favorites.contains(id))
            .map(|(id, s)| (id.clone(), s.invocations))
            .collect();
        candidates.sort_by_key(|(_, inv)| *inv);
        let to_remove = self.stats.len() - MAX_TRACKED_TOOLS;
        let evicted_ids: Vec<_> = candidates.iter().take(to_remove)
            .map(|(id, _)| id.0.as_str())
            .collect();
        tracing::warn!(
            count = to_remove, total = self.stats.len(),
            evicted = %evicted_ids.join(","),
            "effectiveness stats eviction triggered (cap={})", MAX_TRACKED_TOOLS
        );
        for (id, _) in candidates.into_iter().take(to_remove) {
            self.stats.remove(&id);
        }
    }

    /// Record that a tool was visible to the LLM (opportunity).
    /// Called once per turn for all visible tools before the LLM call.
    pub fn record_opportunity(&mut self, tool_id: &ToolId) {
        self.stats.entry(tool_id.clone()).or_default().opportunities += 1;
        self.evict_if_needed();
    }

    /// Record that a tool was actually invoked by the LLM.
    /// Updates invocations count, success count, latency, and exit code history.
    ///
    /// ## 段 K1 兼容性
    /// 旧 API（success: bool）保留以避免大面积 break。内部转换为 ToolOutcome：
    /// - true → Success
    /// - false → ToolFailure（保守归类——调用方若知道是 EnvFailure 应改用 record_outcome）
    pub fn record_invocation(&mut self, tool_id: &ToolId, success: bool, latency_ms: u64) {
        let outcome = if success { ToolOutcome::Success } else { ToolOutcome::ToolFailure };
        self.record_outcome(tool_id, outcome, latency_ms);
    }

    /// 段 K1: 区分 ToolFailure / EnvFailure 的精确版
    ///
    /// ## 字段更新
    /// - 总是 +1 invocations（无论何种 outcome——env_failure 也是"被调过"）
    /// - Success：+1 successes，exit_code=0
    /// - ToolFailure：exit_code=1
    /// - EnvFailure：+1 env_failures，exit_code=2（区别于纯工具失败，便于审计）
    pub fn record_outcome(&mut self, tool_id: &ToolId, outcome: ToolOutcome, latency_ms: u64) {
        let entry = self.stats.entry(tool_id.clone()).or_default();
        entry.invocations += 1;
        match outcome {
            ToolOutcome::Success => {
                entry.successes += 1;
                entry.recent_exit_codes.push(0);
            }
            ToolOutcome::ToolFailure => {
                entry.recent_exit_codes.push(1);
            }
            ToolOutcome::EnvFailure => {
                entry.env_failures += 1;
                entry.recent_exit_codes.push(2);
            }
        }
        entry.total_latency_ms += latency_ms;
        if entry.recent_exit_codes.len() > 10 {
            entry.recent_exit_codes.remove(0);
        }
    }

    /// 段 K2: 按 provider 阈值评估
    ///
    /// 与 `evaluate()` 等价但用 `min_samples_for(provider)` 决定 insufficient_data 边界。
    /// 旧 `evaluate()` 仍可用——内部默认 BuiltIn 阈值（保持现有行为）。
    pub fn evaluate_with_provider(&self, tool_id: &ToolId, provider: &ToolProvider) -> ToolEffectiveness {
        self.evaluate_with_threshold(tool_id, min_samples_for(provider))
    }

    /// 段 K4: 按 provider + current_turn 评估（含 probation 试探放行）
    ///
    /// ## 流程
    /// 1. user_favorite → 直接 S（不进入 probation 路径）
    /// 2. palace_demoted 但 `is_demoted_now(turn) == false`（落在试探窗口）
    ///    → 走正常 stats 路径（让 LLM 重新看到该工具）
    /// 3. palace_demoted 且仍压制 → 强制 D
    /// 4. 其他 → 同 evaluate_with_threshold
    pub fn evaluate_at_turn(
        &self,
        tool_id: &ToolId,
        provider: &ToolProvider,
        current_turn: u64,
    ) -> ToolEffectiveness {
        if self.user_favorites.contains(tool_id) {
            return ToolEffectiveness {
                tool_id: tool_id.clone(),
                composite_score: 1.0,
                tier: VisibilityTier::S,
                cooldown_remaining: 0,
                blocked_by_env: false,
                insufficient_data: false,
            };
        }
        // 段 K4 关键：palace_demoted 但当前 turn 在试探窗口 → 不压制
        if self.palace_demoted.contains_key(tool_id) && self.is_demoted_now(tool_id, current_turn) {
            return ToolEffectiveness {
                tool_id: tool_id.clone(),
                composite_score: 0.1,
                tier: VisibilityTier::D,
                cooldown_remaining: 30,
                blocked_by_env: false,
                insufficient_data: false,
            };
        }
        // 试探窗口内 / 未降级 → 走正常评分（用 provider 阈值）
        // 注意：这里要绕过 evaluate_with_threshold 的 palace_demoted 强 D 检查
        let min = min_samples_for(provider);
        self.evaluate_skip_palace(tool_id, min)
    }

    /// 内部：评分时跳过 palace_demoted 强制 D 分支
    fn evaluate_skip_palace(&self, tool_id: &ToolId, min_samples: u64) -> ToolEffectiveness {
        if self.user_favorites.contains(tool_id) {
            return ToolEffectiveness {
                tool_id: tool_id.clone(),
                composite_score: 1.0,
                tier: VisibilityTier::S,
                cooldown_remaining: 0,
                blocked_by_env: false,
                insufficient_data: false,
            };
        }
        let stats = match self.stats.get(tool_id) {
            Some(s) => s,
            None => return ToolEffectiveness {
                tool_id: tool_id.clone(),
                composite_score: 0.6,
                tier: VisibilityTier::A,
                cooldown_remaining: 0,
                blocked_by_env: false,
                insufficient_data: true,
            },
        };
        if stats.opportunities < min_samples {
            return ToolEffectiveness {
                tool_id: tool_id.clone(),
                composite_score: 0.6,
                tier: VisibilityTier::A,
                cooldown_remaining: 0,
                blocked_by_env: false,
                insufficient_data: true,
            };
        }
        let score = Self::tool_composite_score(stats);
        let tier = Self::score_to_tier(score);
        // env_failures ≥ 80% of invocations → 标记环境阻塞
        let env_blocked = stats.invocations > 0
            && stats.env_failures as f64 / stats.invocations as f64 >= 0.8;
        ToolEffectiveness {
            tool_id: tool_id.clone(),
            composite_score: score,
            tier: tier.clone(),
            cooldown_remaining: match tier {
                VisibilityTier::C => 10,
                VisibilityTier::D => 30,
                _ => 0,
            },
            blocked_by_env: env_blocked,
            insufficient_data: false,
        }
    }

    /// 内部实现——接受外部阈值
    fn evaluate_with_threshold(&self, tool_id: &ToolId, min_samples: u64) -> ToolEffectiveness {
        // 三段优先级：user_favorite > palace_demoted > stats-driven
        if self.user_favorites.contains(tool_id) {
            return ToolEffectiveness {
                tool_id: tool_id.clone(),
                composite_score: 1.0,
                tier: VisibilityTier::S,
                cooldown_remaining: 0,
                blocked_by_env: false,
                insufficient_data: false,
            };
        }
        if self.palace_demoted.contains_key(tool_id) {
            return ToolEffectiveness {
                tool_id: tool_id.clone(),
                composite_score: 0.1,
                tier: VisibilityTier::D,
                cooldown_remaining: 30,
                blocked_by_env: false,
                insufficient_data: false,
            };
        }
        let stats = match self.stats.get(tool_id) {
            Some(s) => s,
            None => return ToolEffectiveness {
                tool_id: tool_id.clone(),
                composite_score: 0.6,
                tier: VisibilityTier::A,
                cooldown_remaining: 0,
                blocked_by_env: false,
                insufficient_data: true,
            },
        };
        if stats.opportunities < min_samples {
            return ToolEffectiveness {
                tool_id: tool_id.clone(),
                composite_score: 0.6,
                tier: VisibilityTier::A,
                cooldown_remaining: 0,
                blocked_by_env: false,
                insufficient_data: true,
            };
        }
        let score = Self::tool_composite_score(stats);
        let tier = Self::score_to_tier(score);
        // env_failures ≥ 80% of invocations → 标记环境阻塞
        let env_blocked = stats.invocations > 0
            && stats.env_failures as f64 / stats.invocations as f64 >= 0.8;
        ToolEffectiveness {
            tool_id: tool_id.clone(),
            composite_score: score,
            tier: tier.clone(),
            cooldown_remaining: match tier {
                VisibilityTier::C => 10,
                VisibilityTier::D => 30,
                _ => 0,
            },
            blocked_by_env: env_blocked,
            insufficient_data: false,
        }
    }

    /// Evaluate a tool and return its effectiveness rating.
    ///
    /// Returns S-tier with 1.0 score for user favorites.
    /// Returns default A-tier with `insufficient_data: true` if below `min_samples`.
    ///
    /// ## 段 K2: 兼容 wrapper
    /// 默认用 self.min_samples（10）阈值——保持旧行为。新代码应优先调
    /// `evaluate_with_provider(tool_id, provider)` 让扩展工具享受 30 次冷启动期。
    pub fn evaluate(&self, tool_id: &ToolId) -> ToolEffectiveness {
        self.evaluate_with_threshold(tool_id, self.min_samples)
    }

    /// Mark a tool as a user favorite (bypasses normal scoring, always S-tier).
    pub fn add_favorite(&mut self, tool_id: ToolId) {
        if !self.user_favorites.contains(&tool_id) {
            self.user_favorites.push(tool_id);
        }
    }

    /// Reset all statistics for a tool, forcing re-evaluation from scratch.
    pub fn reset(&mut self, tool_id: &ToolId) {
        self.stats.remove(tool_id);
    }

    /// Return a snapshot of all tool stats (tool_id → ToolStats).
    /// Used by DeductionEngine to collect real metrics.
    pub fn all_stats_snapshot(&self) -> &HashMap<ToolId, ToolStats> {
        &self.stats
    }

    /// Get stats for a specific tool.
    pub fn stats_for(&self, tool_id: &ToolId) -> Option<&ToolStats> {
        self.stats.get(tool_id)
    }

    // NOTE: Cooldown tick 由 ToolRegistry::tick_cooldowns() 管理（per-handle state）。
    // EffectivenessTracker 只负责评分，不管理 cooldown 生命周期。

    /// R6: 统一的工具复合评分公式
    /// `score = adoption_rate × 0.50 + trend × 0.25 + success_rate × 0.15 + latency_score × 0.10`
    ///
    /// 被 evaluate() 和 experience_signal() 共同调用，消除公式分歧。
    fn tool_composite_score(stats: &ToolStats) -> f64 {
        let adoption = stats.adoption_rate();
        let success = stats.success_rate();
        let latency_s = 1.0 - (stats.avg_latency_ms() / 5000.0).clamp(0.0, 1.0);

        let trend = if stats.recent_exit_codes.len() >= 5 {
            let recent_success: f64 = stats.recent_exit_codes.iter()
                .filter(|&&c| c == 0).count() as f64 / stats.recent_exit_codes.len() as f64;
            (recent_success - success).clamp(-1.0, 1.0)
        } else { 0.0 };

        adoption * 0.50 + trend * 0.25 + success * 0.15 + latency_s * 0.10
    }

    /// Return tool affinity scores for Silent Router (experience signal).
    ///
    /// Returns tools with sufficient data, scored by composite_score.
    /// Used as the experience dimension in fusion routing.
    ///
    /// ## R6 修复
    /// 与 `evaluate()` 使用完全相同的加权公式，消除内部分歧：
    /// `score = adoption × 0.50 + trend × 0.25 + success × 0.15 + latency × 0.10`
    pub fn experience_signal(&self) -> crate::core::silent_router::ExperienceSignal {
        let data_points: u32 = self.stats.values()
            .map(|s| s.opportunities as u32)
            .sum::<u32>();
        let tool_scores: Vec<(ToolId, f64)> = self.stats.iter()
            .filter(|(_, s)| s.opportunities >= self.min_samples)
            .map(|(id, s)| {
                let score = Self::tool_composite_score(s);
                (id.clone(), score)
            })
            .filter(|(_, score)| *score > 0.1)
            .collect();
        crate::core::silent_router::ExperienceSignal { tool_scores, data_points }
    }

    /// Map a composite score to a visibility tier.
    fn score_to_tier(score: f64) -> VisibilityTier {
        if score >= 0.80 { VisibilityTier::S }
        else if score >= 0.60 { VisibilityTier::A }
        else if score >= 0.40 { VisibilityTier::B }
        else if score >= 0.20 { VisibilityTier::C }
        else { VisibilityTier::D }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_tool_default_a() {
        let tracker = EffectivenessTracker::new();
        let eff = tracker.evaluate(&ToolId("filengine_fs_read".into()));
        assert_eq!(eff.tier, VisibilityTier::A);
        assert!(eff.insufficient_data);
    }

    #[test]
    fn test_after_enough_data() {
        let mut tracker = EffectivenessTracker::new();
        let tid = ToolId("filengine_fs_read".into());
        for i in 0..15 {
            tracker.record_opportunity(&tid);
            if i < 12 {
                tracker.record_invocation(&tid, true, 10);
            }
        }
        let eff = tracker.evaluate(&tid);
        assert!(!eff.insufficient_data);
    }

    #[test]
    fn test_user_favorite_always_s() {
        let mut tracker = EffectivenessTracker::new();
        let tid = ToolId("filengine_fs_read".into());
        tracker.add_favorite(tid.clone());
        let eff = tracker.evaluate(&tid);
        assert_eq!(eff.tier, VisibilityTier::S);
    }

    #[test]
    fn test_reset_clears_stats() {
        let mut tracker = EffectivenessTracker::new();
        let tid = ToolId("filengine_fs_read".into());
        tracker.record_opportunity(&tid);
        tracker.record_invocation(&tid, true, 5);
        tracker.reset(&tid);
        let eff = tracker.evaluate(&tid);
        assert!(eff.insufficient_data);
    }

    #[test]
    fn test_low_success_downgrades() {
        let mut tracker = EffectivenessTracker::new();
        let tid = ToolId("web_fetch".into());
        for _ in 0..15 {
            tracker.record_opportunity(&tid);
            tracker.record_invocation(&tid, false, 3000);
        }
        let eff = tracker.evaluate(&tid);
        assert!(eff.composite_score < 0.6);
    }

    #[test]
    fn test_score_to_tier_mapping() {
        assert_eq!(EffectivenessTracker::score_to_tier(0.85), VisibilityTier::S);
        assert_eq!(EffectivenessTracker::score_to_tier(0.70), VisibilityTier::A);
        assert_eq!(EffectivenessTracker::score_to_tier(0.50), VisibilityTier::B);
        assert_eq!(EffectivenessTracker::score_to_tier(0.30), VisibilityTier::C);
        assert_eq!(EffectivenessTracker::score_to_tier(0.10), VisibilityTier::D);
    }

    // ─── Phase γ-Palace-C: palace 降级测试 ─────────────────────────────────
    #[test]
    fn test_palace_demote_forces_d_tier() {
        let mut t = EffectivenessTracker::new();
        let id = ToolId("low_quality".into());
        // 先正常累积一些 stats（A-tier 区间）
        for _ in 0..15 {
            t.record_opportunity(&id);
        }
        for _ in 0..12 {
            t.record_invocation(&id, true, 50);
        }
        // 应是 A 或更高
        let before = t.evaluate(&id);
        assert!(matches!(before.tier, VisibilityTier::S | VisibilityTier::A | VisibilityTier::B),
            "高成功率工具应该是 A 或 B");
        // 应用 palace 降级
        t.apply_palace_demote(id.clone());
        let after = t.evaluate(&id);
        assert_eq!(after.tier, VisibilityTier::D,
            "palace 降级后强制 D tier");
        assert_eq!(after.composite_score, 0.1);
    }

    #[test]
    fn test_palace_demote_overridden_by_user_favorite() {
        let mut t = EffectivenessTracker::new();
        let id = ToolId("trusted_tool".into());
        t.add_favorite(id.clone());
        t.apply_palace_demote(id.clone());
        let eval = t.evaluate(&id);
        // user_favorite 优先级高于 palace_demote
        assert_eq!(eval.tier, VisibilityTier::S,
            "user_favorite 应优先于 palace_demote");
    }

    #[test]
    fn test_clear_palace_demote_restores() {
        let mut t = EffectivenessTracker::new();
        let id = ToolId("tool".into());
        t.apply_palace_demote(id.clone());
        assert!(t.is_palace_demoted(&id));
        t.clear_palace_demote();
        assert!(!t.is_palace_demoted(&id));
    }

    // ─── 段 K1: ToolOutcome / env_failure 不拉 success_rate ────────────────

    #[test]
    fn env_failure_does_not_drop_success_rate() {
        let mut t = EffectivenessTracker::new();
        let id = ToolId("flaky_mcp".into());
        // 14 次环境失败 + 1 次真实失败 + 5 次成功
        // 旧 success_rate = 5/20 = 0.25（被环境失败拖垮）
        // 新 success_rate = 5/(20-14) = 5/6 ≈ 0.83（环境失败被排除）
        for _ in 0..14 { t.record_outcome(&id, ToolOutcome::EnvFailure, 100); }
        for _ in 0..1  { t.record_outcome(&id, ToolOutcome::ToolFailure, 100); }
        for _ in 0..5  { t.record_outcome(&id, ToolOutcome::Success, 100); }
        let stats = t.stats.get(&id).unwrap();
        assert_eq!(stats.invocations, 20);
        assert_eq!(stats.env_failures, 14);
        assert_eq!(stats.successes, 5);
        let sr = stats.success_rate();
        assert!(sr > 0.8, "env_failure 应被排除分母, success_rate={}", sr);
    }

    #[test]
    fn classify_error_routes_env_vs_tool() {
        // 环境层
        for k in ["Network", "Timeout", "Unauthorized", "RateLimited",
                  "ServiceUnavailable", "SandboxDenied", "DependencyMissing"] {
            assert_eq!(ToolOutcome::classify_error(k), ToolOutcome::EnvFailure,
                "{k} 应归 EnvFailure");
        }
        // 工具层（默认 fallback）
        for k in ["InvalidArgument", "BusinessError", "ParseError", "Other"] {
            assert_eq!(ToolOutcome::classify_error(k), ToolOutcome::ToolFailure,
                "{k} 应归 ToolFailure");
        }
    }

    #[test]
    fn record_invocation_legacy_api_still_works() {
        // 旧 API record_invocation(success: bool) 仍可用，行为等同 ToolOutcome
        let mut t = EffectivenessTracker::new();
        let id = ToolId("legacy".into());
        t.record_invocation(&id, true, 50);
        t.record_invocation(&id, false, 50);
        let stats = t.stats.get(&id).unwrap();
        assert_eq!(stats.invocations, 2);
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.env_failures, 0, "legacy false → ToolFailure 不计 env");
    }

    #[test]
    fn all_env_failures_no_real_invocations_returns_one() {
        // 全部 env failure → 没有"真实可评判"的 invocation → success_rate=1.0（不评判）
        let mut t = EffectivenessTracker::new();
        let id = ToolId("dead_endpoint".into());
        for _ in 0..10 { t.record_outcome(&id, ToolOutcome::EnvFailure, 1000); }
        let stats = t.stats.get(&id).unwrap();
        assert_eq!(stats.success_rate(), 1.0,
            "全 env_failure 应返 1.0 不评判（denom=0）");
    }

    #[test]
    fn record_outcome_exit_codes_distinguish_outcome_kinds() {
        let mut t = EffectivenessTracker::new();
        let id = ToolId("foo".into());
        t.record_outcome(&id, ToolOutcome::Success, 10);
        t.record_outcome(&id, ToolOutcome::ToolFailure, 10);
        t.record_outcome(&id, ToolOutcome::EnvFailure, 10);
        let stats = t.stats.get(&id).unwrap();
        // 0=Success, 1=ToolFailure, 2=EnvFailure
        assert_eq!(stats.recent_exit_codes, vec![0u32, 1, 2]);
    }

    // ─── 段 K2: 分 provider 评分门槛 ─────────────────────────────────────

    #[test]
    fn min_samples_for_returns_higher_for_extensions() {
        // BuiltIn 沿用 10
        assert_eq!(min_samples_for(&ToolProvider::BuiltIn), 10);
        // Extensions 提到 30
        assert_eq!(min_samples_for(&ToolProvider::Mcp { server_id: "x".into() }), 30);
        assert_eq!(min_samples_for(&ToolProvider::Plugin { plugin_id: "p".into() }), 30);
        assert_eq!(min_samples_for(&ToolProvider::Skill { skill_id: "s".into() }), 30);
    }

    #[test]
    fn extension_tool_stays_insufficient_until_30_samples() {
        let mut t = EffectivenessTracker::new();
        let id = ToolId("mcp_remote".into());
        let mcp_provider = ToolProvider::Mcp { server_id: "abc".into() };

        // 跑 20 次 opportunity（builtin 阈值过线，但 mcp 没过）
        for _ in 0..20 {
            t.record_opportunity(&id);
            t.record_outcome(&id, ToolOutcome::Success, 50);
        }

        // BuiltIn 视角：已过线
        let as_builtin = t.evaluate_with_provider(&id, &ToolProvider::BuiltIn);
        assert!(!as_builtin.insufficient_data, "BuiltIn 阈值 10，20 应过线");

        // MCP 视角：仍 insufficient（阈值 30）
        let as_mcp = t.evaluate_with_provider(&id, &mcp_provider);
        assert!(as_mcp.insufficient_data,
            "MCP 阈值 30，20 应仍 insufficient_data");
    }

    #[test]
    fn extension_tool_passes_threshold_at_30_samples() {
        let mut t = EffectivenessTracker::new();
        let id = ToolId("plugin_x".into());
        let plugin = ToolProvider::Plugin { plugin_id: "p".into() };
        for _ in 0..32 {
            t.record_opportunity(&id);
            t.record_outcome(&id, ToolOutcome::Success, 50);
        }
        let eff = t.evaluate_with_provider(&id, &plugin);
        assert!(!eff.insufficient_data, "Plugin 30 阈值，32 应过线");
    }

    #[test]
    fn evaluate_legacy_keeps_builtin_threshold() {
        // 旧 evaluate() 应保持 builtin 阈值（10）行为不变——回归保护
        let mut t = EffectivenessTracker::new();
        let id = ToolId("any".into());
        for _ in 0..12 {
            t.record_opportunity(&id);
            t.record_outcome(&id, ToolOutcome::Success, 50);
        }
        let eff = t.evaluate(&id);
        assert!(!eff.insufficient_data, "旧 API 应保持 builtin=10 行为");
    }

    // ─── 段 K4: palace_demoted 试探放行 ───────────────────────────────────

    #[test]
    fn is_demoted_now_blocks_during_suppression_window() {
        let mut t = EffectivenessTracker::new();
        let id = ToolId("demoted_x".into());
        t.apply_palace_demote_at(id.clone(), 10);
        // turn 10 → 0 elapsed → 压制
        assert!(t.is_demoted_now(&id, 10));
        // turn 11~59 → 1~49 elapsed → 压制（未到 PROBATION_INTERVAL）
        for turn in 11..60 {
            assert!(t.is_demoted_now(&id, turn),
                "turn {} 应仍压制", turn);
        }
    }

    #[test]
    fn is_demoted_now_releases_at_probation_interval() {
        let mut t = EffectivenessTracker::new();
        let id = ToolId("demoted_y".into());
        t.apply_palace_demote_at(id.clone(), 0);
        // turn 50 = PROBATION_INTERVAL → 试探放行
        assert!(!t.is_demoted_now(&id, PROBATION_INTERVAL_TURNS),
            "turn 50 应进入试探窗口");
        // turn 51 → 又压制
        assert!(t.is_demoted_now(&id, PROBATION_INTERVAL_TURNS + 1));
        // turn 100 → 又试探
        assert!(!t.is_demoted_now(&id, PROBATION_INTERVAL_TURNS * 2));
    }

    #[test]
    fn is_demoted_now_returns_false_for_undemoted() {
        let t = EffectivenessTracker::new();
        let id = ToolId("never_demoted".into());
        assert!(!t.is_demoted_now(&id, 50), "未降级工具任何 turn 都返 false");
    }

    #[test]
    fn evaluate_at_turn_lets_demoted_tool_visible_in_probation() {
        let mut t = EffectivenessTracker::new();
        let id = ToolId("flaky_mcp".into());
        let provider = ToolProvider::Mcp { server_id: "x".into() };
        // 1) 累积一些 stats（B-tier 区间，让评分有意义）
        for _ in 0..40 {
            t.record_opportunity(&id);
            t.record_outcome(&id, ToolOutcome::Success, 100);
        }
        // 2) demote 在 turn=10
        t.apply_palace_demote_at(id.clone(), 10);

        // turn 30（压制中）→ tier=D，composite=0.1
        let mid = t.evaluate_at_turn(&id, &provider, 30);
        assert_eq!(mid.tier, VisibilityTier::D);
        assert!((mid.composite_score - 0.1).abs() < 1e-6);

        // turn 60（10 + 50 = 60，进入试探窗口）→ 走正常评分（应是 S 或 A）
        let probation = t.evaluate_at_turn(&id, &provider, 60);
        assert_ne!(probation.tier, VisibilityTier::D,
            "试探窗口应让工具回到正常评分; got {:?}", probation);
    }

    #[test]
    fn clear_palace_demote_for_removes_single_tool() {
        let mut t = EffectivenessTracker::new();
        let id_a = ToolId("a".into());
        let id_b = ToolId("b".into());
        t.apply_palace_demote_at(id_a.clone(), 5);
        t.apply_palace_demote_at(id_b.clone(), 5);
        assert!(t.is_palace_demoted(&id_a));
        assert!(t.is_palace_demoted(&id_b));
        t.clear_palace_demote_for(&id_a);
        assert!(!t.is_palace_demoted(&id_a), "a 应被清");
        assert!(t.is_palace_demoted(&id_b), "b 应保留");
    }

    #[test]
    fn turn_regression_keeps_demoted_safe() {
        // 防御：current_turn < demoted_at（异常 turn 倒退）→ 保守压制
        let mut t = EffectivenessTracker::new();
        let id = ToolId("z".into());
        t.apply_palace_demote_at(id.clone(), 100);
        assert!(t.is_demoted_now(&id, 50), "turn 倒退应保守压制");
    }
}