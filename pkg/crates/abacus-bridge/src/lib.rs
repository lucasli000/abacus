//! Abacus NAPI-RS Bridge — exposes core engine to TypeScript/Bun
//!
//! Architecture: Polling model (not ThreadsafeFunction)
//! - Rust side: send_message starts async work, pushes events to a queue
//! - TS side: calls poll_events() at regular intervals to drain the queue
//! This avoids Bun's ThreadsafeFunction compatibility issues.

use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Event pushed to the event queue
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamEvent {
    pub event_type: String,
    pub data: String,
}

/// Internal engine state
struct EngineInner {
    model: String,
    thinking: String,
    initialized: bool,
    /// Event queue — drained by poll_events()
    event_queue: Vec<StreamEvent>,
}

/// Abacus engine bridge
#[napi]
pub struct AbacusBridge {
    inner: Arc<RwLock<EngineInner>>,
}

#[napi]
impl AbacusBridge {
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(EngineInner {
                model: String::new(),
                thinking: String::new(),
                initialized: false,
                event_queue: Vec::new(),
            })),
        }
    }

    /// Initialize the engine
    #[napi]
    pub async fn init(&self, model: String, thinking: String) -> Result<()> {
        let mut inner = self.inner.write().await;
        inner.model = model;
        inner.thinking = thinking;
        inner.initialized = true;
        Ok(())
    }

    /// Destroy the bridge
    #[napi]
    pub async fn destroy(&self) {
        let mut inner = self.inner.write().await;
        inner.initialized = false;
        inner.event_queue.clear();
    }

    /// Check if initialized
    #[napi]
    pub async fn is_initialized(&self) -> bool {
        self.inner.read().await.initialized
    }

    /// Get model name
    #[napi]
    pub async fn get_model(&self) -> String {
        self.inner.read().await.model.clone()
    }

    /// Get thinking level
    #[napi]
    pub async fn get_thinking(&self) -> String {
        self.inner.read().await.thinking.clone()
    }

    /// Echo test (no streaming)
    #[napi]
    pub async fn send_message_echo(&self, input: String) -> Result<String> {
        let inner = self.inner.read().await;
        if !inner.initialized {
            return Err(Error::from_reason("Bridge not initialized"));
        }
        Ok(format!("Echo: {}", input))
    }

    /// Send a message — starts async processing, events queued for polling
    #[napi]
    pub async fn send_message(&self, input: String) -> Result<()> {
        let inner = self.inner.clone();
        {
            let guard = inner.read().await;
            if !guard.initialized {
                return Err(Error::from_reason("Bridge not initialized"));
            }
        }

        // Spawn async task that pushes events to the queue
        tokio::spawn(async move {
            // Simulate thinking
            {
                let mut guard = inner.write().await;
                guard.event_queue.push(StreamEvent {
                    event_type: "chunk".to_string(),
                    data: serde_json::json!({
                        "kind": "thinking",
                        "text": "Let me analyze your request..."
                    }).to_string(),
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            // Simulate text delta
            {
                let mut guard = inner.write().await;
                let model = guard.model.clone();
                guard.event_queue.push(StreamEvent {
                    event_type: "chunk".to_string(),
                    data: serde_json::json!({
                        "kind": "text_delta",
                        "text": format!("You said: '{}'. This is a simulated response from {}.", input, model)
                    }).to_string(),
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            // Simulate complete
            {
                let mut guard = inner.write().await;
                guard.event_queue.push(StreamEvent {
                    event_type: "chunk".to_string(),
                    data: serde_json::json!({
                        "kind": "complete",
                        "stats": {
                            "promptTokens": 42,
                            "completionTokens": 128,
                            "cachedTokens": 0,
                            "totalLatencyMs": 150,
                            "toolCalls": 0,
                            "iterations": 1
                        }
                    }).to_string(),
                });
            }
        });

        Ok(())
    }

    /// Poll for pending events — drains the queue
    /// Returns JSON array of StreamEvent objects
    #[napi]
    pub async fn poll_events(&self) -> Result<String> {
        let mut inner = self.inner.write().await;
        let events: Vec<StreamEvent> = inner.event_queue.drain(..).collect();
        serde_json::to_string(&events)
            .map_err(|e| Error::from_reason(format!("JSON serialization error: {}", e)))
    }

    /// Cancel current turn
    #[napi]
    pub fn cancel_turn(&self) {
        // Phase 0: no-op
    }

    /// Continue generation
    #[napi]
    pub async fn continue_generation(&self) -> Result<()> {
        // Phase 0: no-op
        Ok(())
    }

    /// Start team mode
    #[napi]
    pub async fn start_team(&self, _goal: String) -> Result<()> {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut guard = inner.write().await;
            guard.event_queue.push(StreamEvent {
                event_type: "chunk".to_string(),
                data: serde_json::json!({
                    "kind": "team_progress",
                    "phase": "planning",
                    "tasks": []
                }).to_string(),
            });
        });
        Ok(())
    }

    /// Send team message
    #[napi]
    pub async fn send_team_message(&self, input: String) -> Result<()> {
        self.send_message(input).await
    }

    /// Start meeting mode
    #[napi]
    pub async fn start_meeting(&self, topic: String) -> Result<()> {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut guard = inner.write().await;
            guard.event_queue.push(StreamEvent {
                event_type: "chunk".to_string(),
                data: serde_json::json!({
                    "kind": "meeting_started",
                    "topic": topic
                }).to_string(),
            });
        });
        Ok(())
    }

    /// Send meeting message
    #[napi]
    pub async fn send_meeting_message(&self, input: String) -> Result<()> {
        self.send_message(input).await
    }

    /// Confirm tool execution (MCIP)
    #[napi]
    pub async fn confirm_tools(&self, _decisions_json: String) -> Result<()> {
        // Phase 0: no-op
        Ok(())
    }

    /// Save session
    #[napi]
    pub async fn save_session(&self) -> Result<String> {
        Ok("session-saved".to_string())
    }

    /// Load session
    #[napi]
    pub async fn load_session(&self, _path: String) -> Result<()> {
        Ok(())
    }

    /// List sessions
    #[napi]
    pub async fn list_sessions(&self) -> Result<String> {
        Ok("[]".to_string())
    }

    /// Get config
    #[napi]
    pub async fn get_config(&self) -> Result<String> {
        Ok("{}".to_string())
    }

    /// Reload config
    #[napi]
    pub async fn reload_config(&self) -> Result<()> {
        Ok(())
    }

    /// List models
    #[napi]
    pub async fn list_models(&self) -> Result<String> {
        Ok("[\"claude-sonnet-4\", \"gpt-4o\", \"deepseek-v3\"]".to_string())
    }

    /// Execute slash command
    #[napi]
    pub async fn execute_slash_command(&self, command: String) -> Result<()> {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut guard = inner.write().await;
            guard.event_queue.push(StreamEvent {
                event_type: "chunk".to_string(),
                data: serde_json::json!({
                    "kind": "command_result",
                    "command": command,
                    "success": true
                }).to_string(),
            });
        });
        Ok(())
    }
}
