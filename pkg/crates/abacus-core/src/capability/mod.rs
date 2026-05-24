
use abacus_types::{
    CapabilityDeclaration, CapabilityKind, CapabilityRequest,
};

pub struct CapabilityHub {
    declarations: Vec<CapabilityDeclaration>,
}

impl Default for CapabilityHub {
    fn default() -> Self {
        Self::new()
    }
}

impl CapabilityHub {
    pub fn new() -> Self {
        Self {
            declarations: Vec::new(),
        }
    }

    pub fn register(&mut self, decl: CapabilityDeclaration) {
        self.declarations.push(decl);
    }

    pub fn register_batch(&mut self, decls: Vec<CapabilityDeclaration>) {
        self.declarations.extend(decls);
    }

    /// Resolve a request to the best matching declarations (top-3)
    pub fn resolve(&self, request: &CapabilityRequest) -> Vec<&CapabilityDeclaration> {
        let mut candidates: Vec<&CapabilityDeclaration> = match &request.kind {
            CapabilityKind::ToolExecution(_tool_id) => self
                .declarations
                .iter()
                .filter(|d| d.capabilities.iter().any(|c| c == "tool_execution"))
                .collect(),
            CapabilityKind::KnowledgeQuery { domain: _, .. } => self
                .declarations
                .iter()
                .filter(|d| d.capabilities.iter().any(|c| c == "knowledge_query"))
                .collect(),
            CapabilityKind::LlmCompletion { model, .. } => self
                .declarations
                .iter()
                .filter(|d| {
                    d.capabilities.iter().any(|c| c == "llm_completion")
                        || d.provider_id == *model
                })
                .collect(),
            CapabilityKind::ResourceAccess { resource: _, .. } => self
                .declarations
                .iter()
                .filter(|d| d.capabilities.iter().any(|c| c == "resource_access"))
                .collect(),
        };

        // Filter by forced_provider if set
        if let Some(ctx) = &request.context {
            if let Some(ref forced) = ctx.forced_provider {
                candidates.retain(|d| d.provider_id == *forced);
            }
        }

        // Sort by priority descending
        candidates.sort_by_key(|c| std::cmp::Reverse(c.priority));

        // Top-3
        candidates.into_iter().take(3).collect()
    }

    pub fn clear(&mut self) {
        self.declarations.clear();
    }

    pub fn len(&self) -> usize {
        self.declarations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.declarations.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_types::{CapabilityContext, CapabilityRequest};

    #[test]
    fn test_resolve_by_provider() {
        let mut hub = CapabilityHub::new();
        hub.register(CapabilityDeclaration {
            provider_id: "deepseek".into(),
            capabilities: vec!["llm_completion".into()],
            constraints: vec![],
            priority: 10,
        });
        hub.register(CapabilityDeclaration {
            provider_id: "qwen".into(),
            capabilities: vec!["llm_completion".into()],
            constraints: vec![],
            priority: 5,
        });

        let results = hub.resolve(&CapabilityRequest {
            kind: CapabilityKind::LlmCompletion {
                model: "deepseek-chat".into(),
                capabilities: vec![],
            },
            context: None,
        });
            assert_eq!(results.len(), 2);
        assert_eq!(results[0].provider_id, "deepseek");
    }

    #[test]
    fn test_forced_provider() {
        let mut hub = CapabilityHub::new();
        hub.register(CapabilityDeclaration {
            provider_id: "deepseek".into(),
            capabilities: vec!["llm_completion".into()],
            constraints: vec![],
            priority: 10,
        });
        hub.register(CapabilityDeclaration {
            provider_id: "qwen".into(),
            capabilities: vec!["llm_completion".into()],
            constraints: vec![],
            priority: 5,
        });

        let results = hub.resolve(&CapabilityRequest {
            kind: CapabilityKind::LlmCompletion {
                model: "deepseek-chat".into(),
                capabilities: vec![],
            },
            context: Some(CapabilityContext {
                forced_provider: Some("qwen".into()),
                task_kind: None,
                session_id: None,
            }),
        });
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].provider_id, "qwen");
    }
}