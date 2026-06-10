//! ScriptHook — 脚本级 Pipeline 钩子
//!
//! 让用户通过配置即可在 Turn 关键阶段触发自定义脚本，无需修改 Rust 代码。
//!
//! ## 支持的脚本运行时
//!
//! | 前缀 | 运行时 | 沙箱 | 开销 |
//! |------|--------|------|------|
//! | `rhai://` | Rhai 表达式 | ✅ 默认沙箱 | 零（内嵌引擎） |
//! | 无前缀 / `sh://` | Shell 命令 | ❌ 系统权限 | spawn 子进程 |
//! | `py://` | Python 代码 | ❌ 子进程隔离 | spawn 子进程 |
//!
//! ## 配置示例 (config.toml)
//!
//! ```yaml
//! magchain:
//!   hooks:
//!     # Rhai：Turn 开始记录 session
//!     - on: TurnStart
//!       run: rhai://print("turn started: " + input.session_id)
//!     # Shell：Turn 结束后写审计日志
//!     - on: TurnEnd
//!       run: /usr/local/bin/abacus-hooks/turn-end.sh
//!     # Python：LlmComplete 后统计 token
//!     - on: LlmComplete
//!       run: py://print(f"llm call #{loop_iter}: {completion_tokens} tokens")
//! ```
//!
//! ## 事件数据
//!
//! 所有事件数据通过 **环境变量** 传入子进程（sh/py）：
//! - `ABACUS_EVENT` — 事件名（如 `TurnStart`）
//! - `ABACUS_*` — 事件字段（如 `ABACUS_INPUT`, `ABACUS_SESSION_ID`）
//!
//! Rhai 脚本中通过 `input` 变量访问事件数据（JSON 对象）。

use std::collections::HashMap;
use abacus_types::KernelError;
use tokio::process::Command;

use crate::mag_chain::{PipelineEvent, PipelineHook, HookAction};

// ─── ScriptRuntime ────────────────────────────────────────────────────────

/// 脚本运行时类型
#[derive(Debug, Clone)]
enum ScriptRuntime {
    /// Rhai 表达式（内嵌引擎执行，零开销）
    Rhai(String),
    /// Shell 命令（spawn 子进程）
    Shell(String),
    /// Python 代码（spawn 子进程）
    Python(String),
}

impl ScriptRuntime {
    fn from_uri(uri: &str) -> Result<Self, KernelError> {
        if let Some(code) = uri.strip_prefix("rhai://") {
            return Ok(Self::Rhai(code.to_string()));
        }
        if let Some(code) = uri.strip_prefix("py://") {
            return Ok(Self::Python(code.to_string()));
        }
        if let Some(cmd) = uri.strip_prefix("sh://") {
            return Ok(Self::Shell(cmd.to_string()));
        }
        // 没有 :// 前缀 → 视为 shell 命令（兼容裸路径如 /usr/bin/foo）
        if !uri.contains("://") {
            return Ok(Self::Shell(uri.to_string()));
        }
        Err(KernelError::Other(format!(
            "未知脚本运行时: {uri}。支持: rhai://, sh://, py://, 或直接写 shell 命令路径"
        )))
    }
}

// ─── HookConfig ───────────────────────────────────────────────────────────

/// 钩子配置（从 YAML 反序列化）
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HookConfig {
    /// 触发事件名：TurnStart / PromptBuilt / LlmComplete /
    ///             PostProcess / TurnPostFanOut / TurnEnd
    pub on: String,
    /// 脚本运行指令：rhai://<expr> | py://<code> | sh://<cmd> | /path/to/cmd
    pub run: String,
    /// 优先级（越小越先触发，默认 100）
    #[serde(default = "default_priority")]
    pub priority: u32,
}

fn default_priority() -> u32 { 100 }

// ─── ScriptHook ───────────────────────────────────────────────────────────

/// 脚本钩子 — 实现 PipelineHook trait
pub struct ScriptHook {
    name: String,
    event_name: String,
    runtime: ScriptRuntime,
    priority: u32,
}

impl ScriptHook {
    /// 从配置创建 ScriptHook
    pub fn from_config(cfg: HookConfig) -> Result<Self, KernelError> {
        let runtime = ScriptRuntime::from_uri(&cfg.run)?;
        Ok(Self {
            name: format!("script:{}", cfg.on),
            event_name: cfg.on,
            runtime,
            priority: cfg.priority,
        })
    }

    /// 获取优先级（用于注册时排序）
    pub fn priority(&self) -> u32 { self.priority }
}

#[async_trait::async_trait]
impl PipelineHook for ScriptHook {
    fn name(&self) -> &str { &self.name }

    fn accepts(&self, event: &PipelineEvent) -> bool {
        let name = event_name(event);
        self.event_name.eq_ignore_ascii_case(name)
    }

    async fn on_event(&self, event: &PipelineEvent) -> Result<HookAction, KernelError> {
        match &self.runtime {
            ScriptRuntime::Rhai(script) => {
                let ctx = build_event_value(event);
                let executor = crate::code_exec::CodeExecutor::new();
                executor.execute(script, Some(ctx))?;
            }
            ScriptRuntime::Shell(cmd) => {
                let envs = build_event_env(event);
                // 保留 PATH 等基础环境变量，仅覆盖 ABACUS_* 事件变量
                let child = Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .envs(envs)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| KernelError::Other(format!("spawn sh hook: {e}")))?;
                // 不等待——detach，不阻塞 pipeline
                // stderr 通过 tracing 日志暴露
                tokio::spawn(async move {
                    let output = child.wait_with_output().await;
                    if let Ok(out) = output {
                        if !out.stderr.is_empty() {
                            let msg = String::from_utf8_lossy(&out.stderr);
                            tracing::warn!("script hook (sh) stderr: {msg}");
                        }
                    }
                });
            }
            ScriptRuntime::Python(code) => {
                let envs = build_event_env(event);
                let child = Command::new("python3")
                    .arg("-c")
                    .arg(code)
                    .envs(envs)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)  // task cancel/panic → 自动杀子进程
                    .spawn()
                    .map_err(|e| KernelError::Other(format!("spawn py hook: {e}")))?;
                tokio::spawn(async move {
                    let output = child.wait_with_output().await;
                    if let Ok(out) = output {
                        if !out.stderr.is_empty() {
                            let msg = String::from_utf8_lossy(&out.stderr);
                            tracing::warn!("script hook (py) stderr: {msg}");
                        }
                    }
                });
            }
        }
        Ok(HookAction::Continue)
    }
}

// ─── 事件辅助函数 ─────────────────────────────────────────────────────────

/// 获取事件名称字符串
fn event_name(event: &PipelineEvent) -> &'static str {
    match event {
        PipelineEvent::TurnStart { .. } => "TurnStart",
        PipelineEvent::PromptBuilt { .. } => "PromptBuilt",
        PipelineEvent::LlmComplete { .. } => "LlmComplete",
        PipelineEvent::PostProcess => "PostProcess",
        PipelineEvent::TurnPostFanOut { .. } => "TurnPostFanOut",
        PipelineEvent::TurnEnd { .. } => "TurnEnd",
        PipelineEvent::TriageResult { .. } => "TriageResult",
    }
}

/// 构建事件 JSON（用于 Rhai 的 input 变量）
fn build_event_value(event: &PipelineEvent) -> serde_json::Value {
    match event {
        PipelineEvent::TurnStart { input, session_id } => serde_json::json!({
            "event": "TurnStart",
            "input": input,
            "session_id": session_id,
        }),
        PipelineEvent::PromptBuilt { system_len, dynamic_blocks } => serde_json::json!({
            "event": "PromptBuilt",
            "system_len": system_len,
            "dynamic_blocks": dynamic_blocks,
        }),
        PipelineEvent::LlmComplete { loop_iter, completion_tokens } => serde_json::json!({
            "event": "LlmComplete",
            "loop_iter": loop_iter,
            "completion_tokens": completion_tokens,
        }),
        PipelineEvent::PostProcess => serde_json::json!({
            "event": "PostProcess",
        }),
        PipelineEvent::TurnPostFanOut { turn_number, session_id, tool_calls, all_success, was_compressed } => serde_json::json!({
            "event": "TurnPostFanOut",
            "turn_number": turn_number,
            "session_id": session_id,
            "tool_calls": tool_calls,
            "all_success": all_success,
            "was_compressed": was_compressed,
        }),
        PipelineEvent::TurnEnd { response_len, tool_calls, latency_ms, completion_tokens } => serde_json::json!({
            "event": "TurnEnd",
            "response_len": response_len,
            "tool_calls": tool_calls,
            "latency_ms": latency_ms,
            "completion_tokens": completion_tokens,
        }),
        PipelineEvent::TriageResult { stats, turn_number } => serde_json::json!({
            "event": "TriageResult",
            "summary": stats.summary_line(),
            "turn_number": turn_number,
        }),
    }
}

/// 构建事件环境变量（用于 sh/py 子进程）
fn build_event_env(event: &PipelineEvent) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("ABACUS_EVENT".into(), event_name(event).to_string());

    match event {
        PipelineEvent::TurnStart { input, session_id } => {
            env.insert("ABACUS_INPUT".into(), input.clone());
            env.insert("ABACUS_SESSION_ID".into(), session_id.clone());
        }
        PipelineEvent::PromptBuilt { system_len, dynamic_blocks } => {
            env.insert("ABACUS_SYSTEM_LEN".into(), system_len.to_string());
            env.insert("ABACUS_DYNAMIC_BLOCKS".into(), dynamic_blocks.to_string());
        }
        PipelineEvent::LlmComplete { loop_iter, completion_tokens } => {
            env.insert("ABACUS_LOOP_ITER".into(), loop_iter.to_string());
            env.insert("ABACUS_COMPLETION_TOKENS".into(), completion_tokens.to_string());
        }
        PipelineEvent::PostProcess => {}
        PipelineEvent::TurnPostFanOut { turn_number, session_id, tool_calls, all_success, was_compressed } => {
            env.insert("ABACUS_TURN_NUMBER".into(), turn_number.to_string());
            env.insert("ABACUS_SESSION_ID".into(), session_id.clone());
            env.insert("ABACUS_TOOL_CALLS".into(), tool_calls.to_string());
            env.insert("ABACUS_ALL_SUCCESS".into(), all_success.to_string());
            env.insert("ABACUS_WAS_COMPRESSED".into(), was_compressed.to_string());
        }
        PipelineEvent::TurnEnd { response_len, tool_calls, latency_ms, completion_tokens } => {
            env.insert("ABACUS_RESPONSE_LEN".into(), response_len.to_string());
            env.insert("ABACUS_TOOL_CALLS".into(), tool_calls.to_string());
            env.insert("ABACUS_LATENCY_MS".into(), latency_ms.to_string());
            env.insert("ABACUS_COMPLETION_TOKENS".into(), completion_tokens.to_string());
        }
        PipelineEvent::TriageResult { stats, turn_number } => {
            env.insert("ABACUS_TRIAGE_SUMMARY".into(), stats.summary_line());
            env.insert("ABACUS_TURN_NUMBER".into(), turn_number.to_string());
        }
    }
    env
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mag_chain::PipelineEvent;

    #[test]
    fn test_script_runtime_from_uri() {
        assert!(matches!(ScriptRuntime::from_uri("rhai://print(1)").unwrap(), ScriptRuntime::Rhai(_)));
        assert!(matches!(ScriptRuntime::from_uri("sh://echo hi").unwrap(), ScriptRuntime::Shell(_)));
        assert!(matches!(ScriptRuntime::from_uri("/usr/bin/true").unwrap(), ScriptRuntime::Shell(_)));
        assert!(matches!(ScriptRuntime::from_uri("py://print('hi')").unwrap(), ScriptRuntime::Python(_)));
        // 有 :// 但前缀不匹配 → 报错；无前缀裸路径 → shell
        assert!(ScriptRuntime::from_uri("unknown://x").is_err());
    }

    #[test]
    fn test_event_name() {
        let e = PipelineEvent::TurnStart { input: "".into(), session_id: "".into() };
        assert_eq!(super::event_name(&e), "TurnStart");
        let e = PipelineEvent::TurnEnd { response_len: 0, tool_calls: 0, latency_ms: 0, completion_tokens: 0 };
        assert_eq!(super::event_name(&e), "TurnEnd");
    }

    #[test]
    fn test_build_event_env_turn_start() {
        let e = PipelineEvent::TurnStart { input: "hello".into(), session_id: "s1".into() };
        let env = build_event_env(&e);
        assert_eq!(env.get("ABACUS_EVENT").unwrap(), "TurnStart");
        assert_eq!(env.get("ABACUS_INPUT").unwrap(), "hello");
        assert_eq!(env.get("ABACUS_SESSION_ID").unwrap(), "s1");
    }

    #[test]
    fn test_build_event_env_turn_end() {
        let e = PipelineEvent::TurnEnd { response_len: 100, tool_calls: 3, latency_ms: 5000, completion_tokens: 200 };
        let env = build_event_env(&e);
        assert_eq!(env.get("ABACUS_EVENT").unwrap(), "TurnEnd");
        assert_eq!(env.get("ABACUS_RESPONSE_LEN").unwrap(), "100");
        assert_eq!(env.get("ABACUS_TOOL_CALLS").unwrap(), "3");
        assert_eq!(env.get("ABACUS_LATENCY_MS").unwrap(), "5000");
        assert_eq!(env.get("ABACUS_COMPLETION_TOKENS").unwrap(), "200");
    }

    #[test]
    fn test_build_event_value() {
        let e = PipelineEvent::LlmComplete { loop_iter: 2, completion_tokens: 512 };
        let v = build_event_value(&e);
        assert_eq!(v["event"], "LlmComplete");
        assert_eq!(v["loop_iter"], 2);
        assert_eq!(v["completion_tokens"], 512);
    }

    #[test]
    fn test_accepts() {
        let hook = ScriptHook {
            name: "test".into(),
            event_name: "TurnEnd".into(),
            runtime: ScriptRuntime::Rhai("print(1)".into()),
            priority: 100,
        };
        let turn_start = PipelineEvent::TurnStart { input: "".into(), session_id: "".into() };
        let turn_end = PipelineEvent::TurnEnd { response_len: 0, tool_calls: 0, latency_ms: 0, completion_tokens: 0 };
        assert!(!hook.accepts(&turn_start));
        assert!(hook.accepts(&turn_end));
    }

    #[tokio::test]
    async fn test_rhai_hook_executes() {
        let hook = ScriptHook {
            name: "test".into(),
            event_name: "TurnStart".into(),
            runtime: ScriptRuntime::Rhai("print(input.event)".into()),
            priority: 100,
        };
        let event = PipelineEvent::TurnStart { input: "hi".into(), session_id: "s1".into() };
        let result = hook.on_event(&event).await;
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), HookAction::Continue));
    }
}
