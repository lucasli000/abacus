//! HealthRegistry — subsystem health tracking with graceful degradation.
//!
//! Each subsystem (LLM provider, session store, config dir) implements HealthProbe.
//! Registry is checked each turn; state transitions produce user-visible warnings.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

use crate::core::event_sink::{EventBus, EventKind};

#[derive(Debug, Clone, PartialEq)]
pub enum HealthState {
    Healthy,
    Degraded { reason: String, since: Instant },
    Unhealthy { reason: String, since: Instant },
}

impl HealthState {
    pub fn is_healthy(&self) -> bool { matches!(self, Self::Healthy) }
    pub fn is_available(&self) -> bool { !matches!(self, Self::Unhealthy { .. }) }
}

#[async_trait::async_trait]
pub trait HealthProbe: Send + Sync {
    fn subsystem(&self) -> &str;
    async fn check(&self) -> HealthState;
    async fn heal(&self) -> HealthState;
    fn user_message(&self, state: &HealthState) -> Option<String>;
}

pub struct HealthRegistry {
    probes: RwLock<Vec<Arc<dyn HealthProbe>>>,
    states: RwLock<HashMap<String, HealthState>>,
    pending_warnings: RwLock<Vec<String>>,
    /// 统一 EventBus 引用（可选）
    event_bus: Option<Arc<EventBus>>,
}

impl Default for HealthRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self {
            probes: RwLock::new(Vec::new()),
            states: RwLock::new(HashMap::new()),
            pending_warnings: RwLock::new(Vec::new()),
            event_bus: None,
        }
    }

    /// 绑定 EventBus（推荐——让状态变更事件进入统一观测层）
    pub fn with_event_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.event_bus = Some(bus);
        self
    }

    pub async fn register(&self, probe: Arc<dyn HealthProbe>) {
        let state = probe.check().await;
        let subsystem = probe.subsystem().to_string();
        self.states.write().await.insert(subsystem.clone(), state.clone());
        self.probes.write().await.push(probe);
        // 通过 EventBus 通知初始状态
        if let Some(ref bus) = self.event_bus {
            let kind = match &state {
                HealthState::Degraded { reason, .. } => EventKind::SubsystemDegraded {
                    subsystem: subsystem.clone(),
                    reason: reason.clone(),
                },
                HealthState::Unhealthy { reason, .. } => EventKind::SubsystemUnhealthy {
                    subsystem: subsystem.clone(),
                    reason: reason.clone(),
                },
                HealthState::Healthy => return, // 健康初始状态无需通知
            };
            bus.emit(kind);
        }
    }

    /// Called each turn. Checks probes, attempts healing, returns warnings.
    pub async fn tick(&self) -> Vec<String> {
        let probes = self.probes.read().await.clone();
        let mut warnings = Vec::new();
        for probe in &probes {
            let new_state = probe.check().await;
            let subsystem = probe.subsystem().to_string();
            let old = self.states.read().await.get(&subsystem).cloned();
            if old.as_ref() != Some(&new_state) {
                let final_state = if !new_state.is_healthy() {
                    probe.heal().await
                } else {
                    new_state
                };
                // 通过 EventBus 发送状态变更
                if let Some(ref bus) = self.event_bus {
                    let kind = match &final_state {
                        HealthState::Degraded { reason, .. } => EventKind::SubsystemDegraded {
                            subsystem: subsystem.clone(), reason: reason.clone(),
                        },
                        HealthState::Unhealthy { reason, .. } => EventKind::SubsystemUnhealthy {
                            subsystem: subsystem.clone(), reason: reason.clone(),
                        },
                        HealthState::Healthy => EventKind::SubsystemHealed {
                            subsystem: subsystem.clone(),
                        },
                    };
                    bus.emit(kind);
                }
                if let Some(msg) = probe.user_message(&final_state) {
                    warnings.push(msg.clone());
                    self.pending_warnings.write().await.push(msg);
                }
                self.states.write().await.insert(subsystem, final_state);
            }
        }
        warnings
    }

    pub async fn drain_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.pending_warnings.write().await)
    }

    pub async fn is_available(&self, subsystem: &str) -> bool {
        self.states.read().await.get(subsystem)
            .map(|s| s.is_available())
            .unwrap_or(true)
    }

    pub async fn status(&self) -> HashMap<String, HealthState> {
        self.states.read().await.clone()
    }
}
