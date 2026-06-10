//! LLM 行为策略配置 — 运行时加载，无需重编译
//!
//! ## 依赖
//! - `toml` crate: 解析 ~/.abacus/policy.toml
//! - `std::fs`: 读取配置文件
//!
//! ## 引用关系
//! - 被 CoreLoop 持有（Arc<PolicyConfig>）
//! - 被 pipeline::execute_loop 消费（阈值）
//! - 被 build_system_output 消费（guard/declaration 文本注入 system prompt）
//! - 被 preflight.rs 消费（destructive_patterns）
//!
//! ## 生命周期
//! - 进程启动时加载一次（PolicyConfig::load()）
//! - 配置变更后下次 session 生效（不热加载）

use serde::Deserialize;

/// LLM 行为策略配置（从 ~/.abacus/policy.toml 加载）
///
/// 所有字段有内置默认值——配置文件不存在或部分缺失时用默认值填充。
#[derive(Debug, Clone, Deserialize)]
pub struct PolicyConfig {
    #[serde(default = "default_guard")]
    pub guard: GuardConfig,
    #[serde(default = "default_thresholds")]
    pub thresholds: ThresholdConfig,
    #[serde(default = "default_preflight")]
    pub preflight: PreflightConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuardConfig {
    /// Entropy Guard 文本（注入 system prompt）。空字符串 = 禁用。
    #[serde(default = "default_entropy_guard")]
    pub entropy_guard: String,
    /// Explicit Declaration 文本（注入 system prompt）。空字符串 = 禁用。
    #[serde(default = "default_explicit_declaration")]
    pub explicit_declaration: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThresholdConfig {
    /// LLM 短文本检测阈值（字符数）
    #[serde(default = "default_premature_stop_chars")]
    pub premature_stop_chars: usize,
    /// 最多注入几次续写提示
    #[serde(default = "default_premature_stop_max_retries")]
    pub premature_stop_max_retries: u32,
    /// Pipeline 侧 confirm 超时（秒）
    #[serde(default = "default_confirm_timeout_secs")]
    pub confirm_timeout_secs: u64,
    /// Bash 默认超时（秒）
    #[serde(default = "default_bash_timeout")]
    pub bash_default_timeout: u64,
    /// Bash 最大超时（秒）
    #[serde(default = "default_bash_max_timeout")]
    pub bash_max_timeout: u64,
    /// 通用工具执行超时（秒）— 非 bash 工具的安全网
    ///
    /// 引用关系：注入到 ExecutionContext.tool_default_timeout → ToolRegistry::execute() 消费
    /// 生命周期：进程启动时加载，session 内不变
    #[serde(default = "default_tool_timeout")]
    pub tool_default_timeout: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreflightConfig {
    /// 触发 llm_self_review 的破坏性模式
    #[serde(default = "default_destructive_patterns")]
    pub destructive_patterns: Vec<String>,
}

// ─── Default values ──────────────────────────────────────────

fn default_guard() -> GuardConfig {
    GuardConfig {
        entropy_guard: default_entropy_guard(),
        explicit_declaration: default_explicit_declaration(),
    }
}

fn default_entropy_guard() -> String {
    "Before creating files/folders or starting multi-step tasks, a moment of planning pays off:\n\
     • Read the existing structure first. Editing what's there is almost always better than creating something new.\n\
     • Appending to an existing file or following the project's naming conventions feels natural and avoids fragmentation.\n\
     • One coherent change per task — speculative files tend to cause confusion downstream.\n\
     • If you're unsure about placement, asking the user takes one message and saves a cleanup cycle.\n\
     Single-file edits, appending to existing files, and running commands are straightforward — just do them.".into()
}

fn default_explicit_declaration() -> String {
    "Transparency is what makes your output trustworthy. When you encounter any of these, stating it explicitly is the most helpful response:\n\
     • Blocked — tool denied, permission missing, file not found, command failed. Starting with [Blocked] gives the user immediate context.\n\
     • Stuck — tried multiple approaches, none worked. Starting with [Stuck] signals you need a different strategy, not more retries.\n\
     • Need input — ambiguous requirement, multiple valid options, missing context. Starting with [Need Input] is direct and respects the user's time.\n\
     • Partial — task partially done, remaining steps require user action. Starting with [Partial] sets clear expectations.\n\
     Format: start with [Blocked], [Stuck], [Need Input], or [Partial], then explain in 1-2 sentences.".into()
}

fn default_thresholds() -> ThresholdConfig {
    ThresholdConfig {
        premature_stop_chars: default_premature_stop_chars(),
        premature_stop_max_retries: default_premature_stop_max_retries(),
        confirm_timeout_secs: default_confirm_timeout_secs(),
        bash_default_timeout: default_bash_timeout(),
        bash_max_timeout: default_bash_max_timeout(),
        tool_default_timeout: default_tool_timeout(),
    }
}

fn default_premature_stop_chars() -> usize { 200 }
fn default_premature_stop_max_retries() -> u32 { u32::MAX }
fn default_confirm_timeout_secs() -> u64 { 60 }
fn default_bash_timeout() -> u64 { 30 }
fn default_bash_max_timeout() -> u64 { 120 }
/// 通用工具超时默认 60s（bash 有独立超时，此为其他工具安全网）
fn default_tool_timeout() -> u64 { 60 }

fn default_preflight() -> PreflightConfig {
    PreflightConfig {
        destructive_patterns: default_destructive_patterns(),
    }
}

fn default_destructive_patterns() -> Vec<String> {
    vec![
        "delete all".into(), "drop table".into(), "truncate".into(),
        "rm -rf".into(), "format disk".into(),
        "覆盖全部".into(), "清空".into(), "删除所有".into(),
    ]
}

// ─── Loading ──────────────────────────────────────────────────

impl PolicyConfig {
    /// 加载策略配置。优先级：~/.abacus/policy.toml > 内置默认值。
    /// 文件不存在或解析失败时静默 fallback 到默认值（不阻塞启动）。
    pub fn load() -> Self {
        let path = dirs::home_dir()
            .map(|h| h.join(".abacus").join("policy.toml"))
            .unwrap_or_default();

        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => match toml::from_str(&content) {
                    Ok(config) => {
                        tracing::info!("policy loaded from {}", path.display());
                        return config;
                    }
                    Err(e) => {
                        tracing::warn!("policy.toml parse error (using defaults): {}", e);
                    }
                }
                Err(e) => {
                    tracing::warn!("policy.toml read error (using defaults): {}", e);
                }
            }
        }

        Self::default()
    }
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            guard: default_guard(),
            thresholds: default_thresholds(),
            preflight: default_preflight(),
        }
    }
}
