//! Layer 5 demo —— 启动 audit 输出实际样子
use std::sync::Arc;
use abacus_core::{ConfigManager, CoreLoop};
use abacus_core::core::{CoreConfig, context::ContextManager};
use abacus_core::skill::SkillEngine;
use abacus_core::capability::CapabilityHub;
use abacus_core::tool::ToolRegistry;
use abacus_types::ModelId;
use tokio::sync::RwLock;

struct NoopStore;
#[async_trait::async_trait]
impl abacus_core::core::context::SessionStore for NoopStore {
    async fn save(&self, _: abacus_core::core::context::SessionSnapshot) -> Result<(), abacus_types::KernelError> { Ok(()) }
    async fn load_recent(&self, _: usize) -> Result<Vec<abacus_core::core::context::SessionSnapshot>, abacus_types::KernelError> { Ok(vec![]) }
    async fn search(&self, _: &str) -> Result<Vec<abacus_core::core::context::SessionSnapshot>, abacus_types::KernelError> { Ok(vec![]) }
}

#[tokio::main]
async fn main() {
    let _ = ConfigManager::new(std::collections::HashMap::new());
    let registry = Arc::new(ToolRegistry::new());
    let skill = Arc::new(RwLock::new(SkillEngine::new()));
    let cap = Arc::new(CapabilityHub::new());
    let ctx = Arc::new(ContextManager::new(Arc::new(NoopStore)));
    let cfg = CoreConfig {
        max_turns_per_request: 5,
        max_tool_calls_per_turn: 8,
        default_model: ModelId("test".into()),
        silent_router_enabled: false,
        event_sink_enabled: false,
        ..Default::default()
    };
    let core = CoreLoop::new(registry, skill, cap, ctx, cfg).await;
    println!();
    for line in core.audit_report().await {
        println!("{}", line);
    }
}
