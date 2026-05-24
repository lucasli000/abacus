use abacus_core::tool::ToolRegistry;
use abacus_core::tool::builtin::register_all;
use abacus_core::core::env::{register_env_tools, EnvMap};
use abacus_core::core::context::{register_context_tools, ContextManager, SessionStore, SessionSnapshot};
use abacus_types::KernelError;
use std::sync::Arc;
use tokio::sync::RwLock;

struct Noop;
#[async_trait::async_trait]
impl SessionStore for Noop {
    async fn save(&self, _: SessionSnapshot) -> Result<(), KernelError> { Ok(()) }
    async fn load_recent(&self, _: usize) -> Result<Vec<SessionSnapshot>, KernelError> { Ok(vec![]) }
    async fn search(&self, _: &str) -> Result<Vec<SessionSnapshot>, KernelError> { Ok(vec![]) }
}

#[tokio::main]
async fn main() {
    let reg = Arc::new(ToolRegistry::new());
    register_all(reg.as_ref()).await;
    register_env_tools(reg.as_ref(), Arc::new(RwLock::new(EnvMap::default()))).await;
    let ctx = Arc::new(ContextManager::new(Arc::new(Noop)));
    let dummy = Arc::new(RwLock::new(Vec::new()));
    register_context_tools(reg.as_ref(), ctx, dummy).await;

    let tools = reg.all_tools().await;
    let mut by_prefix: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for t in &tools {
        let prefix = t.id.0.split(['.','/','_']).next().unwrap_or("").to_string();
        by_prefix.entry(prefix).or_default().push(t.id.0.clone());
    }
    let total: usize = tools.len();
    println!("Total: {}", total);
    let mut bytes = 0usize;
    for t in &tools {
        let p = serde_json::to_string(&t.schema.parameters).map(|s| s.len()).unwrap_or(0);
        bytes += t.schema.description.len() + p + 50;
    }
    println!("Estimated LLM-bound bytes: {} (~{} tokens)", bytes, bytes/4);
    for (p, ids) in &by_prefix {
        println!("  [{}] {}: {}", p, ids.len(), ids.join(", "));
    }
}
