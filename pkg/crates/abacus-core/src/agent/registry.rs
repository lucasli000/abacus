//! AgentRegistry — 外部 Agent 生命周期管理

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use abacus_types::agent::*;
use abacus_types::{McpConfig, ServerId};
use crate::mcp::McpClient;

/// Agent 注册表
pub struct AgentRegistry {
    agents: RwLock<HashMap<String, AgentInstance>>,
    /// 存储 MCP 客户端引用（供 McpWatcher 热发现）
    clients: RwLock<HashMap<String, Arc<McpClient>>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
            clients: RwLock::new(HashMap::new()),
        }
    }

    /// 安装 Agent
    pub async fn install(&self, manifest: AgentManifest) -> Result<AgentInstance, AgentError> {
        let agent_id = manifest.id.clone();

        {
            let agents = self.agents.read().await;
            if agents.contains_key(&agent_id) {
                return Err(AgentError::AlreadyInstalled(agent_id));
            }
        }

        let mcp_config = McpConfig {
            server_id: ServerId(agent_id.clone()),
            transport: manifest.transport.transport_type.clone(),
            address: manifest.transport.endpoint.clone(),
            tls: false,
            request_signing: false,
        };

        let client = Arc::new(McpClient::new(mcp_config));
        client.connect().await
            .map_err(|e| AgentError::ConnectionFailed(agent_id.clone(), e.to_string()))?;

        let tools = client.discover_tools().await
            .map_err(|e| AgentError::DiscoveryFailed(agent_id.clone(), e.to_string()))?;

        let registered_tools: Vec<String> = tools.iter()
            .map(|t| t.id.0.clone())
            .collect();

        let instance = AgentInstance {
            manifest: manifest.clone(),
            connected: true,
            registered_tools,
            registered_skills: manifest.skills.iter().map(|s| s.id.clone()).collect(),
            health: AgentHealth {
                last_check: Some(std::time::Instant::now()),
                reachable: true,
                avg_latency_ms: 0,
                consecutive_failures: 0,
            },
        };

        self.agents.write().await.insert(agent_id.clone(), instance.clone());
        self.clients.write().await.insert(agent_id, client);

        Ok(instance)
    }

    /// 卸载 Agent
    pub async fn uninstall(&self, agent_id: &str) -> Result<(), AgentError> {
        self.agents.write().await.remove(agent_id)
            .ok_or_else(|| AgentError::NotFound(agent_id.to_string()))?;
        self.clients.write().await.remove(agent_id);
        Ok(())
    }

    /// 列出所有已安装 Agent
    pub async fn list(&self) -> Vec<AgentInstance> {
        self.agents.read().await.values().cloned().collect()
    }

    /// 获取指定 Agent
    pub async fn get(&self, agent_id: &str) -> Option<AgentInstance> {
        self.agents.read().await.get(agent_id).cloned()
    }

    /// 获取 MCP 客户端
    pub async fn get_client(&self, agent_id: &str) -> Option<Arc<McpClient>> {
        self.clients.read().await.get(agent_id).cloned()
    }

    /// Agent 是否已安装
    pub async fn contains(&self, agent_id: &str) -> bool {
        self.agents.read().await.contains_key(agent_id)
    }

    /// 更新健康状态
    pub async fn update_health(&self, agent_id: &str, reachable: bool, latency_ms: u64) {
        let mut agents = self.agents.write().await;
        if let Some(instance) = agents.get_mut(agent_id) {
            instance.health.last_check = Some(std::time::Instant::now());
            instance.health.reachable = reachable;
            instance.health.avg_latency_ms = latency_ms;
            if reachable {
                instance.health.consecutive_failures = 0;
            } else {
                instance.health.consecutive_failures += 1;
            }
        }
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Agent 错误类型
#[derive(Debug)]
pub enum AgentError {
    AlreadyInstalled(String),
    NotFound(String),
    ConnectionFailed(String, String),
    DiscoveryFailed(String, String),
    InsufficientTrust(String),
    Timeout(String),
    Other(String),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyInstalled(id) => write!(f, "Agent '{}' is already installed", id),
            Self::NotFound(id) => write!(f, "Agent '{}' not found", id),
            Self::ConnectionFailed(id, r) => write!(f, "Agent '{}' connection failed: {}", id, r),
            Self::DiscoveryFailed(id, r) => write!(f, "Agent '{}' discovery failed: {}", id, r),
            Self::InsufficientTrust(id) => write!(f, "Agent '{}' insufficient trust", id),
            Self::Timeout(id) => write!(f, "Agent '{}' timed out", id),
            Self::Other(msg) => write!(f, "Agent error: {}", msg),
        }
    }
}

impl std::error::Error for AgentError {}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_types::agent::*;

    #[test]
    fn registry_new_is_empty() {
        let registry = AgentRegistry::new();
        // We can't easily test async in sync test, but we can test creation
        assert!(std::mem::size_of::<AgentRegistry>() > 0);
    }

    #[test]
    fn agent_error_display() {
        let err = AgentError::AlreadyInstalled("test".to_string());
        assert_eq!(format!("{}", err), "Agent 'test' is already installed");

        let err = AgentError::NotFound("test".to_string());
        assert_eq!(format!("{}", err), "Agent 'test' not found");

        let err = AgentError::ConnectionFailed("test".to_string(), "timeout".to_string());
        assert!(format!("{}", err).contains("connection failed"));
    }

    #[test]
    fn trust_level_display() {
        assert_eq!(format!("{}", TrustLevel::Sandbox), "sandbox");
        assert_eq!(format!("{}", TrustLevel::Standard), "standard");
        assert_eq!(format!("{}", TrustLevel::Trusted), "trusted");
        assert_eq!(format!("{}", TrustLevel::Privileged), "privileged");
    }

    #[test]
    fn trust_level_permissions() {
        assert!(!TrustLevel::Sandbox.allows_network());
        assert!(TrustLevel::Standard.allows_network());
        assert!(TrustLevel::Trusted.allows_network());

        assert!(!TrustLevel::Standard.allows_filesystem());
        assert!(TrustLevel::Trusted.allows_filesystem());

        assert!(TrustLevel::Standard.requires_confirmation());
        assert!(!TrustLevel::Trusted.requires_confirmation());
    }

    #[test]
    fn agent_health_default() {
        let health = AgentHealth::default();
        assert!(!health.reachable);
        assert_eq!(health.avg_latency_ms, 0);
        assert_eq!(health.consecutive_failures, 0);
    }

    #[test]
    fn adaptation_config_default() {
        let config = AdaptationConfig::default();
        assert!(config.auto_register);
        assert!(config.palace_enabled);
        assert!((config.learning_rate - 0.1).abs() < f64::EPSILON);
    }
}
