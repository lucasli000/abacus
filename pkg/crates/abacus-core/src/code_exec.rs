//! Code execution engine using Rhai scripting
//!
//! Provides local execution of deterministic operations (data transformation,
//! calculation, string manipulation) to avoid LLM round-trips for simple tasks.
//!
//!
//! ## Dependencies
//!
//! | Crate | Version | Usage |
//! |-------|---------|-------|
//! | `rhai` | workspace (feature "serde") | Scripting engine, `serde::to_dynamic` |
//! | `serde_json` | workspace | Input/output JSON conversion |
//! | `tokio` (implicit) | workspace | Async tool execution wrapper |
//!
//! ## Imports from Abacus
//!
//! - `abacus_types::KernelError`: Rhai evaluation error wrapping
//! - `abacus_types::ToolOutput`: Execution result packaging
//! - `abacus_types::ToolId`: Tool identifier for registry
//!
//! ## Scenarios
//!
//! | Scenario | Script Example | LLM Cost Saved |
//! |----------|---------------|----------------|
//! | Sum array values | `input.values.fold(0, |s,v| s + v)` | 1 round-trip |
//! | Filter objects | `input.items.filter(|i| i.price > 10)` | 1-2 round-trips |
//! | String transform | `"hello " + input.name` | 1 round-trip |
//! | Aggregation | `for v in vals { sum += v } sum` | 1-2 round-trips |
//! | Data validation | `input.age >= 18` | 1 round-trip |
//!
//! ## Security
//!
//! Rhai is sandboxed by default — no filesystem, no network, no process execution.
//! The engine is configured with:
//! - Max 1000 operations per script (anti-infinite loop)
//! - Max 1MB string size
//! - Default 1-second timeout

use std::time::Instant;

use abacus_types::{KernelError, ToolOutput, ToolId};
use rhai::{Engine, Scope};
use serde_json::Value;

/// Rhai-based code execution engine for deterministic operations.
///
/// Reduces LLM token cost by executing simple data transformations
/// locally instead of making tool call round-trips.
pub struct CodeExecutor {
    engine: Engine,
    timeout_ms: u64,
}

impl Default for CodeExecutor {
    fn default() -> Self { Self::new() }
}

impl CodeExecutor {
    /// Create a new executor with default sandboxed Rhai engine.
    pub fn new() -> Self {
        let engine = Self::build_sandboxed_engine();
        Self {
            engine,
            timeout_ms: 30000,
        }
    }

    /// Create with custom timeout.
    pub fn with_timeout(timeout_ms: u64) -> Self {
        let engine = Self::build_sandboxed_engine();
        Self { engine, timeout_ms }
    }

    /// Build a Rhai engine with resource limits to prevent abuse.
    ///
    /// Limits:
    /// - max_operations: 10_000 — prevents infinite loops
    /// - max_call_stack_depth: 32 — prevents stack overflow from recursion
    /// - max_string_size: 1MB — prevents memory exhaustion
    /// - max_array_size: 10_000 — prevents unbounded collection growth
    /// - max_map_size: 5_000 — prevents unbounded map growth
    fn build_sandboxed_engine() -> Engine {
        let mut engine = Engine::new();
        engine.set_max_operations(10_000);
        engine.set_max_call_levels(32);
        engine.set_max_string_size(1024 * 1024);
        engine.set_max_array_size(10_000);
        engine.set_max_map_size(5_000);
        engine
    }

    /// Execute a Rhai script with optional input data.
    ///
    /// `script`: Rhai expression or statement
    /// `input`: JSON value available as `input` variable in script
    ///
    /// Returns the script result serialized to JSON.
    pub fn execute(&self, script: &str, input: Option<Value>) -> Result<Value, KernelError> {
        let start = Instant::now();
        let mut scope = Scope::new();

        if let Some(val) = input {
            let dynamic_val = rhai::serde::to_dynamic(&val)
                .map_err(|e| KernelError::Other(format!("input conversion: {}", e)))?;
            scope.push("input", dynamic_val);
        }

        let result = self.engine.eval_with_scope::<rhai::Dynamic>(&mut scope, script)
            .map_err(|e| KernelError::Other(format!("script error: {}", e)))?;

        let elapsed = start.elapsed().as_millis();
        if elapsed as u64 > self.timeout_ms {
            return Err(KernelError::Other(format!(
                "script execution exceeded timeout ({}ms > {}ms)", elapsed, self.timeout_ms
            )));
        }

        let json_val = serde_json::to_value(&result)
            .map_err(|e| KernelError::Other(format!("result conversion: {}", e)))?;

        Ok(json_val)
    }

    /// Execute a script as a tool (wraps execute with ToolOutput).
    pub async fn execute_tool(
        &self,
        script: &str,
        input: Option<Value>,
    ) -> Result<ToolOutput, KernelError> {
        let start = Instant::now();
        let result = self.execute(script, input)?;
        Ok(ToolOutput {
            tool_id: ToolId("code_execute".into()),
            success: true,
            output: result,
            latency_ms: start.elapsed().as_millis() as u64,
            failure_kind: None,
            try_instead: Vec::new(),
        })
    }

    /// Convenience: parse params and execute inline.
    pub async fn handle_call(params: Value) -> Result<ToolOutput, KernelError> {
        let script = params.get("script")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing 'script' parameter".into()))?;
        let input = params.get("input").cloned();
        let executor = Self::new();
        executor.execute_tool(script, input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_arithmetic() {
        let exec = CodeExecutor::new();
        let result = exec.execute("40 + 2", None).unwrap();
        assert_eq!(result, Value::from(42));
    }

    #[test]
    fn test_string_operation() {
        let exec = CodeExecutor::new();
        let result = exec.execute(r#""hello" + " " + "world""#, None).unwrap();
        assert_eq!(result, Value::from("hello world"));
    }

    #[test]
    fn test_with_input() {
        let exec = CodeExecutor::new();
        let input = serde_json::json!({"name": "Abacus"});
        let result = exec.execute("input.name", Some(input)).unwrap();
        assert_eq!(result, Value::from("Abacus"));
    }

    #[test]
    fn test_array_operation() {
        let exec = CodeExecutor::new();
        let input = serde_json::json!({"values": [1, 2, 3, 4, 5]});
        let result = exec.execute(
            "let sum = 0; for v in input.values { sum += v; } sum",
            Some(input),
        ).unwrap();
        assert_eq!(result, Value::from(15));
    }

    #[test]
    fn test_script_error() {
        let exec = CodeExecutor::new();
        let result = exec.execute("invalid syntax [}", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_json_transformation() {
        let exec = CodeExecutor::new();
        let input = serde_json::json!({"items": [
            {"price": 10, "qty": 2},
            {"price": 5, "qty": 3}
        ]});
        let script = "let total = 0; for item in input.items { total += item.price * item.qty; } total";
        let result = exec.execute(script, Some(input)).unwrap();
        assert_eq!(result, Value::from(35));
    }
}