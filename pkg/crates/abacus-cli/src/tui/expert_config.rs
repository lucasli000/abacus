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
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".abacus")
        .join("experts.yaml")
}

fn ensure_abacus_dir() -> std::io::Result<()> {
    let dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".abacus");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(())
}

// ─── 数据类型 ─────────────────────────────────────────────────────────────

/// 单个专家角色配置
///
/// 引用关系:
///   生产者: cmd_expert_add / cmd_expert_set / default_experts()
///   消费者: to_orchestrator_config() → MeetingSessionBuilder.with_config()
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
}

impl ExpertDef {
    /// 生成专家角色的 system prompt（guide_strategy）
    pub fn guide_strategy(&self) -> String {
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

    /// 转换为 orchestrator 层的 SpecialistRegistration
    pub fn to_registration(&self) -> abacus_orchestrator::specialist::SpecialistRegistration {
        abacus_orchestrator::specialist::SpecialistRegistration {
            id: self.id.clone(),
            domain: self.domain.clone(),
            name: self.name.clone(),
            role: abacus_orchestrator::team::AgentRole::Member,
            // None 时传空字符串 → orchestrator 层降级到主模型
            model: self.model.clone().unwrap_or_default(),
            guide_strategy: self.guide_strategy(),
            anti_pattern: String::new(),
            capabilities: vec![],
            tags: self.hint_tags.clone(),
            allowed_tools: vec![],
            engagement: Default::default(),
        }
    }
}

// ─── 默认专家配置 ─────────────────────────────────────────────────────────

/// 内置默认专家（无 experts.yaml 时使用）
pub fn default_experts() -> Vec<ExpertDef> {
    vec![
        ExpertDef {
            id: "security".to_string(),
            name: "Security Expert".to_string(),
            domain: "security".to_string(),
            model: None,
            hint_tags: vec![
                "auth".to_string(), "crypto".to_string(),
                "permission".to_string(), "inject".to_string(), "token".to_string(),
            ],
        },
        ExpertDef {
            id: "arch".to_string(),
            name: "Architecture Expert".to_string(),
            domain: "architecture".to_string(),
            model: None,
            hint_tags: vec![
                "interface".to_string(), "coupling".to_string(),
                "design".to_string(), "pattern".to_string(), "abstraction".to_string(),
            ],
        },
        ExpertDef {
            id: "perf".to_string(),
            name: "Performance Expert".to_string(),
            domain: "performance".to_string(),
            model: None,
            hint_tags: vec![
                "latency".to_string(), "throughput".to_string(),
                "memory".to_string(), "query".to_string(), "cache".to_string(),
            ],
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
    fn expert_to_registration_maps_model() {
        let expert = ExpertDef {
            id: "security".to_string(),
            name: "Security Expert".to_string(),
            domain: "security".to_string(),
            model: Some("claude-opus-4".to_string()),
            hint_tags: vec![],
        };
        let reg = expert.to_registration();
        assert_eq!(reg.model, "claude-opus-4");
        assert!(reg.guide_strategy.contains("安全审查"));
    }

    #[test]
    fn expert_no_model_uses_empty_string() {
        let expert = ExpertDef {
            id: "arch".to_string(),
            name: "Arch".to_string(),
            domain: "architecture".to_string(),
            model: None,
            hint_tags: vec![],
        };
        let reg = expert.to_registration();
        assert_eq!(reg.model, ""); // orchestrator 层降级到主模型
    }

    #[test]
    fn format_entry_shows_model() {
        let expert = ExpertDef {
            id: "s".to_string(),
            name: "Security Expert".to_string(),
            domain: "security".to_string(),
            model: Some("claude-opus-4".to_string()),
            hint_tags: vec!["auth".to_string()],
        };
        let line = format_expert_entry(&expert, 0);
        assert!(line.contains("claude-opus-4"));
        assert!(line.contains("auth"));
    }
}
