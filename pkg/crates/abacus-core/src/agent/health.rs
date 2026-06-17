//! Agent 健康检查 — 定期 ping + 状态追踪
//!
//! ## 设计
//! - 定期检查所有已安装 Agent 的连接状态
//! - MCP Agent: 尝试 tools/list 探活
//! - HTTP Agent: GET /health 探活
//! - 状态更新到 AgentRegistry + AgentLearner
//!
//! ## 引用关系
//! - 持有: AgentRegistry 引用
//! - 消费: CoreLoop 初始化时启动后台任务
//! - 下游: AgentRegistry::update_health, AgentLearner::record_health_change

use std::sync::Arc;
use std::time::Duration;
use crate::agent::registry::AgentRegistry;
use crate::agent::learning::AgentLearner;
use abacus_types::agent::AgentInstance;

/// 健康检查器
pub struct AgentHealthChecker {
    registry: Arc<AgentRegistry>,
    learner: AgentLearner,
    interval: Duration,
}

impl AgentHealthChecker {
    pub fn new(
        registry: Arc<AgentRegistry>,
        learner: AgentLearner,
        interval_secs: u64,
    ) -> Self {
        Self {
            registry,
            learner,
            interval: Duration::from_secs(interval_secs.max(10)), // 最低 10s
        }
    }

    /// 启动后台健康检查任务
    pub fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.interval);
            loop {
                interval.tick().await;
                self.check_all().await;
            }
        })
    }

    /// 检查所有 Agent 健康状态
    pub async fn check_all(&self) {
        let agents = self.registry.list().await;

        for agent in &agents {
            if !agent.connected {
                continue;
            }

            let (reachable, latency_ms) = self.ping_agent(agent).await;

            self.registry.update_health(
                &agent.manifest.id,
                reachable,
                latency_ms,
            ).await;

            self.learner.record_health_change(
                &agent.manifest.id,
                reachable,
            );
        }
    }

    /// Ping 单个 Agent
    async fn ping_agent(&self, agent: &AgentInstance) -> (bool, u64) {
        let transport_type = &agent.manifest.transport.transport_type;
        let endpoint = &agent.manifest.transport.endpoint;

        let start = std::time::Instant::now();

        let reachable = match transport_type.as_str() {
            "mcp" => {
                // MCP: 尝试 tools/list
                // TODO: 复用已有的 McpClient::discover_tools()
                // 当前简化: 检查 endpoint 进程是否存活
                self.ping_mcp(endpoint).await
            }
            "http" => {
                // HTTP: GET /health
                self.ping_http(endpoint).await
            }
            _ => false,
        };

        let latency_ms = start.elapsed().as_millis() as u64;
        (reachable, latency_ms)
    }

    /// MCP 探活（简化版：检查 endpoint 命令是否可达）
    async fn ping_mcp(&self, endpoint: &str) -> bool {
        // 简化实现：尝试连接 endpoint
        // 完整实现：复用 McpClient::connect() + discover_tools()
        if endpoint.starts_with("npx") || endpoint.starts_with("node") {
            // npm 包: 检查是否可执行
            tokio::process::Command::new("sh")
                .args(["-c", &format!("{} --version 2>/dev/null", endpoint.split_whitespace().next().unwrap_or("echo"))])
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false)
        } else {
            // 本地命令: 检查文件是否存在
            std::path::Path::new(endpoint.split_whitespace().next().unwrap_or("")).exists()
        }
    }

    /// HTTP 探活
    async fn ping_http(&self, endpoint: &str) -> bool {
        let url = if endpoint.ends_with("/health") {
            endpoint.to_string()
        } else {
            format!("{}/health", endpoint.trim_end_matches('/'))
        };

        match reqwest::Client::new()
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }
}
