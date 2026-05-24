//! HealthRegistry — subsystem health tracking with graceful degradation.
//!
//! Each subsystem (LLM provider, session store, config dir) implements HealthProbe.
//! Registry is checked each turn; state transitions produce user-visible warnings.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

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
        }
    }

    pub async fn register(&self, probe: Arc<dyn HealthProbe>) {
        let state = probe.check().await;
        self.states.write().await.insert(probe.subsystem().to_string(), state);
        self.probes.write().await.push(probe);
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
