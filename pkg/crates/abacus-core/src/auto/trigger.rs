//! Trigger — 事件触发器

/// 触发事件
#[derive(Debug, Clone)]
pub struct TriggerEvent {
    pub name: String,
    pub payload: Option<serde_json::Value>,
}

/// 事件触发器：事件名 + 目标 Pipeline
#[derive(Debug, Clone)]
pub struct Trigger {
    pub event_name: String,
    pub pipeline_id: String,
}

impl Trigger {
    pub fn new(event_name: impl Into<String>, pipeline_id: impl Into<String>) -> Self {
        Self {
            event_name: event_name.into(),
            pipeline_id: pipeline_id.into(),
        }
    }

    pub fn matches(&self, event: &TriggerEvent) -> bool {
        self.event_name == event.name
    }
}
