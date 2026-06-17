//! 外部 Agent 模块
//!
//! ## 模块结构
//! - `executor` — ExternalAgentToolExecutor（MCP 协议调用外部 Agent 工具）
//! - `skill_executor` — AgentSkillExecutor（LLM 直接调用 Agent 技能）
//! - `skill_discovery` — AgentSkillDiscovery（从 Agent 发现并注册技能）
//! - `registry` — AgentRegistry（安装/卸载/列表/健康检查）
//! - `adaptation` — AdaptationPipeline（工具自动注册）
//! - `learning` — AgentLearner（BehaviorPalace 集成，跨 session 学习）
//! - `health` — AgentHealthChecker（定期 ping + 状态追踪）
//! - `errors` — AgentError + FallbackStrategy + RateLimiter

pub mod executor;
pub mod skill_executor;
pub mod skill_discovery;
pub mod registry;
pub mod adaptation;
pub mod learning;
pub mod health;
pub mod errors;
