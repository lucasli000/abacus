//! expert_config — Meeting 专家角色持久化配置
//!
//! ## 设计意图
//! 用户可以通过 `/expert` 命令自定义会诊专家阵容：
//!   - 指定专家名称、领域、使用的模型（支持真多模型）
//!   - 未配置模型时降级为主模型 + 专家 system prompt
//!   - 配置持久化到 ~/.abacus/experts.yaml
//!
//! ## 目录 & 文件
//! `~/.abacus/experts.yaml`
//!
//! ## 引用关系
//! - 写: slash_commands::cmd_expert_* 写入
//! - 读: api/mod.rs::ensure_meeting_handle 读取，构建 AbacusOrchestratorConfig
//!
//! ## 生命周期
//! 进程内懒加载，写入后持久存在，跨 session 有效

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ─── 目录定位 ─────────────────────────────────────────────────────────────

/// ~/.abacus/experts.yaml
pub fn experts_path() -> PathBuf {
    abacus_core::paths::global_dir().join("config/experts.yaml")
}

fn ensure_abacus_dir() -> std::io::Result<()> {
    abacus_core::paths::ensure_global_dirs()
}

// ─── 数据类型 ─────────────────────────────────────────────────────────────

/// 单个专家角色配置
///
/// 引用关系:
///   生产者: cmd_expert_add / cmd_expert_set / default_experts() / ~/.abacus/experts.yaml
///   消费者: to_orchestrator_config() → MeetingSessionBuilder.with_config()
///   与 SpecialistRegistration 1:1 映射（to_registration()）
///
/// 向后兼容: 新增字段均有 #[serde(default)]，旧格式 YAML 仍可正常解析
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpertDef {
    /// 唯一 ID（路由 / 命令参数用）
    pub id: String,
    /// 显示名（Panel / 消息流）
    pub name: String,
    /// 专业领域（语义路由评分 / system prompt 主题）
    pub domain: String,
    /// 使用的模型 ID（None = 降级到主模型）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 语义路由标签（帮助 MeetingRouter 匹配此专家）
    #[serde(default)]
    pub hint_tags: Vec<String>,

    // ──── V41: 完整配置开放 ────

    /// 角色: leader / pm / advisor / member（默认 member）
    /// 决定专家在 Meeting 中的发言优先级和合成权重
    #[serde(default)]
    pub role: String,
    /// 专家指导策略 (system prompt)
    /// 为空时自动从 domain 生成（auto_guide_strategy）
    #[serde(default)]
    pub guide_strategy: String,
    /// 反模式描述（后验校验时用于拦截不当输出）
    #[serde(default)]
    pub anti_pattern: String,
    /// 能力声明标签（增强语义路由匹配精度）
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// 允许使用的工具模式（如 ["fs_read", "bash_exec", "cg_*"]）
    /// 为空 = 不限制
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// 参与度限制（控制在 Meeting 中的发言频次和资源消耗）
    #[serde(default)]
    pub engagement: ExpertEngagement,
}

/// 参与度限制 — 对齐 orchestrator::EngagementLimit
///
/// 引用关系: ExpertDef.engagement → to_orchestrator() → SpecialistRegistration.engagement
/// 生命周期: 随 ExpertDef 加载/保存
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpertEngagement {
    /// 每轮最大发言次数（默认 50）
    #[serde(default = "default_max_speeches")]
    pub max_speeches_per_round: u32,
    /// 最大思考 token 数（默认 100_000）
    #[serde(default = "default_max_thinking")]
    pub max_thinking_tokens: usize,
    /// 最低置信度阈值，低于此值不展示输出（默认 0.2）
    #[serde(default = "default_min_confidence")]
    pub min_confidence: f64,
    /// 响应超时秒数（默认 900 = 15min，0 = 无超时）
    #[serde(default = "default_timeout")]
    pub response_timeout_secs: u64,
    /// 每步最大工具调用数（默认 100）
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls_per_think: u32,
    /// 思考深度覆盖: off/low/medium/high（None = 跟随全局）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

fn default_max_speeches() -> u32 { 50 }
fn default_max_thinking() -> usize { 100_000 }
fn default_min_confidence() -> f64 { 0.2 }
fn default_timeout() -> u64 { 900 }
fn default_max_tool_calls() -> u32 { 100 }

impl Default for ExpertEngagement {
    fn default() -> Self {
        Self {
            max_speeches_per_round: default_max_speeches(),
            max_thinking_tokens: default_max_thinking(),
            min_confidence: default_min_confidence(),
            response_timeout_secs: default_timeout(),
            max_tool_calls_per_think: default_max_tool_calls(),
            thinking: None,
        }
    }
}

impl ExpertEngagement {
    /// 转换为 orchestrator 层的 EngagementLimit
    pub fn to_orchestrator(&self) -> abacus_orchestrator::specialist::EngagementLimit {
        abacus_orchestrator::specialist::EngagementLimit {
            max_speeches_per_round: self.max_speeches_per_round,
            max_thinking_tokens: self.max_thinking_tokens,
            min_confidence: self.min_confidence,
            response_timeout_secs: self.response_timeout_secs,
            max_tool_calls_per_think: self.max_tool_calls_per_think,
            thinking: self.thinking.clone(),
        }
    }
}

impl ExpertDef {
    /// 自动生成 guide_strategy（当用户未自定义时从 domain 推导）
    ///
    /// 引用关系: to_registration() 在 guide_strategy 为空时调用
    fn auto_guide_strategy(&self) -> String {
        match self.domain.as_str() {
            "security" => format!(
                "你是 {}，专注于安全审查。重点关注：输入验证、权限边界、注入风险、\
                 加密合规、认证漏洞。发现问题时给出具体的修复建议，附代码示例。",
                self.name
            ),
            "architecture" => format!(
                "你是 {}，专注于架构设计。重点关注：接口职责单一性、耦合度、\
                 抽象层次一致性、命名准确性、设计模式适用性。",
                self.name
            ),
            "performance" => format!(
                "你是 {}，专注于性能分析。重点关注：N+1 查询、热路径内存分配、\
                 锁粒度、批处理机会、缓存策略。量化估算影响范围。",
                self.name
            ),
            "testing" => format!(
                "你是 {}，专注于测试质量。重点关注：边界条件覆盖、副作用隔离、\
                 断言粒度、测试可读性、覆盖率盲区。",
                self.name
            ),
            "product" => format!(
                "你是 {}，专注于产品视角。重点关注：用户体验一致性、功能完整性、\
                 边缘用例处理、错误信息友好度。",
                self.name
            ),
            _ => format!(
                "你是 {}，专注于 {} 领域。从你的专业视角分析问题，\
                 给出具体、可操作的建议。",
                self.name, self.domain
            ),
        }
    }

    /// 获取有效的 guide_strategy（用户自定义优先，空则自动生成）
    pub fn effective_guide_strategy(&self) -> String {
        if self.guide_strategy.is_empty() {
            self.auto_guide_strategy()
        } else {
            self.guide_strategy.clone()
        }
    }

    /// 转换为 orchestrator 层的 SpecialistRegistration
    ///
    /// 所有字段直接映射——用户配置 → 运行时行为
    pub fn to_registration(&self) -> abacus_orchestrator::specialist::SpecialistRegistration {
        abacus_orchestrator::specialist::SpecialistRegistration {
            id: self.id.clone(),
            domain: self.domain.clone(),
            name: self.name.clone(),
            role: parse_role(&self.role),
            model: self.model.clone().unwrap_or_default(),
            guide_strategy: self.effective_guide_strategy(),
            anti_pattern: self.anti_pattern.clone(),
            capabilities: self.capabilities.clone(),
            tags: self.hint_tags.clone(),
            allowed_tools: self.allowed_tools.clone(),
            engagement: self.engagement.to_orchestrator(),
        }
    }
}

/// 解析角色字符串为 AgentRole 枚举
/// 支持: leader / pm / advisor / member（大小写不敏感）
/// 无法识别时默认 Member
fn parse_role(s: &str) -> abacus_orchestrator::team::AgentRole {
    match s.to_lowercase().as_str() {
        "leader" => abacus_orchestrator::team::AgentRole::Leader,
        "pm" => abacus_orchestrator::team::AgentRole::PM,
        "advisor" => abacus_orchestrator::team::AgentRole::Advisor,
        _ => abacus_orchestrator::team::AgentRole::Member,
    }
}

// ─── 默认专家配置 ─────────────────────────────────────────────────────────

/// 内置默认专家（无 experts.yaml 时使用）
///
/// 引用关系: load_experts() fallback
/// 设计意图: 3 个典型审查角色覆盖常见代码质量维度
pub fn default_experts() -> Vec<ExpertDef> {
    vec![
        ExpertDef {
            id: "security".to_string(),
            name: "Security Expert".to_string(),
            domain: "security".to_string(),
            model: None,
            hint_tags: vec![
                "auth".into(), "crypto".into(),
                "permission".into(), "inject".into(), "token".into(),
            ],
            role: "advisor".into(),
            guide_strategy: String::new(), // 自动生成
            anti_pattern: String::new(),
            capabilities: vec!["code_review".into(), "threat_model".into()],
            allowed_tools: vec![],
            engagement: ExpertEngagement::default(),
        },
        ExpertDef {
            id: "arch".to_string(),
            name: "Architecture Expert".to_string(),
            domain: "architecture".to_string(),
            model: None,
            hint_tags: vec![
                "interface".into(), "coupling".into(),
                "design".into(), "pattern".into(), "abstraction".into(),
            ],
            role: "leader".into(),
            guide_strategy: String::new(),
            anti_pattern: String::new(),
            capabilities: vec!["system_design".into(), "refactoring".into()],
            allowed_tools: vec![],
            engagement: ExpertEngagement::default(),
        },
        ExpertDef {
            id: "perf".to_string(),
            name: "Performance Expert".to_string(),
            domain: "performance".to_string(),
            model: None,
            hint_tags: vec![
                "latency".into(), "throughput".into(),
                "memory".into(), "query".into(), "cache".into(),
            ],
            role: "member".into(),
            guide_strategy: String::new(),
            anti_pattern: String::new(),
            capabilities: vec!["profiling".into(), "optimization".into()],
            allowed_tools: vec![],
            engagement: ExpertEngagement::default(),
        },
    ]
}

// ─── 持久化 API ───────────────────────────────────────────────────────────

/// 读取专家配置（不存在时返回默认配置）
pub fn load_experts() -> Vec<ExpertDef> {
    let path = experts_path();
    if !path.exists() {
        return default_experts();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_yaml::from_str::<Vec<ExpertDef>>(&content)
            .unwrap_or_else(|_| default_experts()),
        Err(_) => default_experts(),
    }
}

/// 保存专家配置到 ~/.abacus/experts.yaml
pub fn save_experts(experts: &[ExpertDef]) -> std::io::Result<()> {
    ensure_abacus_dir()?;
    let yaml = serde_yaml::to_string(experts)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(experts_path(), yaml)
}

/// 构建 AbacusOrchestratorConfig（供 MeetingSessionBuilder.with_config() 使用）
pub fn to_orchestrator_config(
    experts: &[ExpertDef],
) -> abacus_orchestrator::meeting::config::AbacusOrchestratorConfig {
    abacus_orchestrator::meeting::config::AbacusOrchestratorConfig {
        meeting: Default::default(),
        specialists: experts.iter().map(|e| e.to_registration()).collect(),
    }
}

/// 格式化单个专家条目（/expert list 展示用）
pub fn format_expert_entry(expert: &ExpertDef, index: usize) -> String {
    let model_str = expert.model.as_deref().unwrap_or("主模型");
    let tags = if expert.hint_tags.is_empty() {
        String::new()
    } else {
        format!("  [{}]", expert.hint_tags.join(", "))
    };
    format!(
        "[{}] {} · {} · {}{}",
        index + 1,
        expert.name,
        expert.domain,
        model_str,
        tags,
    )
}

// ─── 测试 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_experts_has_three() {
        assert_eq!(default_experts().len(), 3);
    }

    #[test]
    fn expert_to_registration_maps_all_fields() {
        let expert = ExpertDef {
            id: "security".to_string(),
            name: "Security Expert".to_string(),
            domain: "security".to_string(),
            model: Some("claude-opus-4".to_string()),
            hint_tags: vec!["auth".into()],
            role: "advisor".into(),
            guide_strategy: "Custom strategy".into(),
            anti_pattern: "No prod changes".into(),
            capabilities: vec!["threat_model".into()],
            allowed_tools: vec!["fs_read".into()],
            engagement: ExpertEngagement {
                max_thinking_tokens: 50_000,
                min_confidence: 0.1,
                ..Default::default()
            },
        };
        let reg = expert.to_registration();
        assert_eq!(reg.model, "claude-opus-4");
        assert_eq!(reg.guide_strategy, "Custom strategy"); // 自定义优先
        assert_eq!(reg.anti_pattern, "No prod changes");
        assert_eq!(reg.capabilities, vec!["threat_model"]);
        assert_eq!(reg.allowed_tools, vec!["fs_read"]);
        assert_eq!(reg.engagement.max_thinking_tokens, 50_000);
        assert_eq!(reg.engagement.min_confidence, 0.1);
        // role 映射
        assert_eq!(reg.role, abacus_orchestrator::team::AgentRole::Advisor);
    }

    #[test]
    fn expert_empty_guide_uses_auto() {
        let expert = ExpertDef {
            id: "security".into(),
            name: "Sec".into(),
            domain: "security".into(),
            model: None,
            hint_tags: vec![],
            role: String::new(),
            guide_strategy: String::new(), // 空 → 自动生成
            anti_pattern: String::new(),
            capabilities: vec![],
            allowed_tools: vec![],
            engagement: ExpertEngagement::default(),
        };
        let reg = expert.to_registration();
        assert!(reg.guide_strategy.contains("安全审查")); // auto-generated
        assert_eq!(reg.role, abacus_orchestrator::team::AgentRole::Member); // 默认
    }

    #[test]
    fn expert_no_model_uses_empty_string() {
        let expert = ExpertDef {
            id: "arch".into(),
            name: "Arch".into(),
            domain: "architecture".into(),
            model: None,
            hint_tags: vec![],
            role: "leader".into(),
            guide_strategy: String::new(),
            anti_pattern: String::new(),
            capabilities: vec![],
            allowed_tools: vec![],
            engagement: ExpertEngagement::default(),
        };
        let reg = expert.to_registration();
        assert_eq!(reg.model, "");
        assert_eq!(reg.role, abacus_orchestrator::team::AgentRole::Leader);
    }

    #[test]
    fn format_entry_shows_model() {
        let expert = ExpertDef {
            id: "s".into(),
            name: "Security Expert".into(),
            domain: "security".into(),
            model: Some("claude-opus-4".to_string()),
            hint_tags: vec!["auth".into()],
            role: String::new(),
            guide_strategy: String::new(),
            anti_pattern: String::new(),
            capabilities: vec![],
            allowed_tools: vec![],
            engagement: ExpertEngagement::default(),
        };
        let line = format_expert_entry(&expert, 0);
        assert!(line.contains("claude-opus-4"));
        assert!(line.contains("auth"));
    }

    #[test]
    fn backward_compat_old_yaml() {
        // 旧格式 YAML（仅 5 字段）应能正常解析
        let yaml = r#"
- id: test
  name: Test Expert
  domain: testing
  hint_tags: [unit, integration]
"#;
        let experts: Vec<ExpertDef> = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(experts.len(), 1);
        assert_eq!(experts[0].role, ""); // 默认空 → Member
        assert!(experts[0].guide_strategy.is_empty());
        assert_eq!(experts[0].engagement.max_speeches_per_round, 50);
    }
}
