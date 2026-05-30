//! `code.execute` — Rhai scripting tool
//!
//! Registers a built-in tool that evaluates Rhai scripts locally.
//! Used by the LLM for deterministic data operations without round-trips.
//!
//! ## Dependencies
//!
//! | Crate | Version | Usage |
//! |-------|---------|-------|
//! | `rhai` | workspace (feature "serde") | Script parsing + sandboxed eval |
//! | `serde_json` | workspace | Parameter parsing + result serialization |
//!
//! ## Imports from Abacus
//!
//! - `crate::code_exec::CodeExecutor`: Core evaluation engine
//! - `crate::tool::{ToolExecutor, ToolRegistry}`: Trait + registry binding
//! - `abacus_types::*`: Tool handles, schema, security annotations
//!
//! ## Registered Tool
//!
//! ```text
//! code.execute(script: string, input?: json)
//! ```
//!
//! - `script`: Rhai expression or block to evaluate
//! - `input` (optional): JSON data available as `input` variable
//!
//! Returns JSON result of script evaluation.

use std::sync::Arc;

use abacus_types::{
    ToolCost, ToolEffectiveness, ToolHandle, ToolId, ToolProvider, ToolSchema,
    ToolSecurity, ToolState,
};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::code_exec::CodeExecutor;
use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};

/// `CodeExecutor` tool — wraps `CodeExecutor` for ToolRegistry.
pub struct CodeExecutorTool {
    #[allow(dead_code)]
    executor: Arc<CodeExecutor>,
}

impl Default for CodeExecutorTool {
    fn default() -> Self { Self::new() }
}

impl CodeExecutorTool {
    /// Create a new instance wrapping a fresh `CodeExecutor`.
    pub fn new() -> Self {
        Self {
            executor: Arc::new(CodeExecutor::new()),
        }
    }

    /// Register the `code_execute` tool in the registry.
    pub async fn register(registry: &ToolRegistry) {
        let handle = ToolHandle {
            id: ToolId("code_execute".into()),
            schema: ToolSchema { short_description: None,
                name: "code_execute".into(),
                description: "Execute a Rhai script locally for data operations. "
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "script": {
                            "type": "string",
                            "description": "Rhai script to evaluate"
                        },
                        "input": {
                            "type": "object",
                            "description": "Optional input data available as `input` variable in script"
                        }
                    },
                    "required": ["script"]
                }),
                returns: Some(json!({
                    "type": "object",
                    "description": "Script evaluation result"
                })),
                security: Some(ToolSecurity {
                    allowed_paths: None,
                    max_size_mb: Some(1),
                    confirm_required: false,
                    needs_sandbox: false,
                }),
                cost: Some(ToolCost {
                    tokens: 0,
                    latency: "5ms".into(),
                    risk: "low".into(),
                }),
                examples: Vec::new(),
                applicable_task_kinds: None,
                idempotent: false,
                                schema_stable: false,            },
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        };
        registry.register(handle).await;
        let tool = Arc::new(CodeExecutorToolHandler {
            executor: Arc::new(CodeExecutor::new()),
        });
        registry.register_executor(ToolId("code_execute".into()), tool).await;
    }
}

/// Internal executor implementation.
struct CodeExecutorToolHandler {
    executor: Arc<CodeExecutor>,
}

#[async_trait]
impl ToolExecutor for CodeExecutorToolHandler {
    async fn execute(&self, _tool_id: &ToolId, params: Value, _ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        let script = params.get("script")
            .and_then(|v| v.as_str())
            .ok_or_else(|| abacus_types::KernelError::Other(
                "missing 'script' parameter".into()
            ))?;
        let input = params.get("input").cloned();
        self.executor.execute(script, input)
    }
}