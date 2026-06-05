//! Model preference persistence — user's model selection preferences.
//!
//! Stores:
//! - Global default model
//! - Last `/model` selection (persisted across sessions)
//! - Per-task-kind defaults
//! - Provider and model aliases for shorthand input
//!
//! ## References
//! - Created by: CLI `/model` command, config initialization
//! - Consumed by: model router (resolves which model to use for a request)
//! - Persisted at: `~/.abacus/model_preference.json`
//!
//! ## Lifecycle
//! - Loaded once at session start (or lazily on first model resolution)
//! - Mutated in-memory by `/model` commands
//! - Saved to disk on mutation (write-through)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{ProviderId, QualifiedModelId};

/// User's model selection preferences.
///
/// ## Resolution priority (highest to lowest):
/// 1. `last_selected` — explicit user choice via `/model` in current or previous session
/// 2. `task_defaults[task_kind]` — per-task-kind override
/// 3. `default` — global fallback
///
/// All fields are optional for backward compatibility — a freshly created
/// preference file (or missing file) deserializes to `ModelPreference::default()`.
///
/// ## References
/// - Produced by: `load_from_file()`, user `/model` command handler
/// - Consumed by: model router `resolve_for_task()`
/// - Persisted at: `preference_file_path()`
///
/// ## Lifecycle
/// - Created: first `/model` command or explicit config init
/// - Destroyed: never (persists across sessions); user can manually delete file
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelPreference {
    /// Global default model (used when no task-specific or session override exists).
    #[serde(default)]
    pub default: Option<QualifiedModelId>,

    /// Last model selected via `/model` command — persisted across sessions.
    /// Takes highest priority in resolution.
    #[serde(default)]
    pub last_selected: Option<QualifiedModelId>,

    /// Per-task-kind model defaults (e.g. "code" → "anthropic:claude-opus-4-7").
    /// Keys are task_kind strings matching `AbacusMode` or custom task labels.
    #[serde(default)]
    pub task_defaults: HashMap<String, QualifiedModelId>,

    /// Provider aliases: shorthand → full provider id (e.g. "ant" → "anthropic").
    #[serde(default)]
    pub provider_aliases: HashMap<String, String>,

    /// Model aliases: shorthand → full qualified model id (e.g. "opus" → "anthropic:claude-opus-4-7").
    #[serde(default)]
    pub model_aliases: HashMap<String, QualifiedModelId>,
}

impl Default for ModelPreference {
    fn default() -> Self {
        Self {
            default: None,
            last_selected: None,
            task_defaults: HashMap::new(),
            provider_aliases: HashMap::new(),
            model_aliases: HashMap::new(),
        }
    }
}

impl ModelPreference {
    /// Resolve the best model for a given task kind.
    ///
    /// Priority order:
    /// 1. `last_selected` (if set)
    /// 2. `task_defaults[task_kind]` (if task_kind provided and entry exists)
    /// 3. `default`
    /// 4. `None` (caller must handle fallback)
    ///
    /// Alias expansion is NOT performed here — call `resolve_alias()` on the
    /// result if alias support is needed.
    pub fn resolve_for_task(&self, task_kind: Option<&str>) -> Option<&QualifiedModelId> {
        // Highest priority: explicit session selection
        if let Some(ref last) = self.last_selected {
            return Some(last);
        }
        // Second: task-specific default
        if let Some(kind) = task_kind {
            if let Some(task_model) = self.task_defaults.get(kind) {
                return Some(task_model);
            }
        }
        // Fallback: global default
        self.default.as_ref()
    }

    /// Resolve a user input string through aliases.
    ///
    /// Resolution steps:
    /// 1. Check `model_aliases` for exact match → return the mapped `QualifiedModelId`
    /// 2. Parse as `QualifiedModelId`; if provider portion matches a `provider_aliases` key,
    ///    expand the provider alias
    /// 3. Return the parsed (possibly alias-expanded) id
    pub fn resolve_alias(&self, input: &str) -> QualifiedModelId {
        let trimmed = input.trim();

        // Step 1: exact model alias match
        if let Some(resolved) = self.model_aliases.get(trimmed) {
            return resolved.clone();
        }

        // Step 2: parse and expand provider alias
        let mut parsed = QualifiedModelId::parse(trimmed);
        if let Some(ref provider) = parsed.provider {
            if let Some(expanded) = self.provider_aliases.get(&provider.0) {
                parsed.provider = Some(ProviderId(expanded.clone()));
            }
        }

        parsed
    }

    /// Update the last-selected model (called by `/model` command handler).
    ///
    /// This is the write-through mutation point — caller should follow with `save_to_file()`.
    pub fn set_last_selected(&mut self, model: QualifiedModelId) {
        self.last_selected = Some(model);
    }
}

/// Returns the default filesystem path for the model preference file.
///
/// Location: `~/.abacus/model_preference.json`
///
/// ## Lifecycle
/// - Directory created on first `save_to_file()` call
/// - File may not exist on first run (handled gracefully by `load_from_file()`)
pub fn preference_file_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".abacus/config/model_preference.json")
}

/// Load model preferences from a JSON file.
///
/// Behavior:
/// - File does not exist → returns `Ok(ModelPreference::default())`
/// - File exists but is empty → returns `Ok(ModelPreference::default())`
/// - File exists with invalid JSON → returns `Err` with context
/// - File exists with valid JSON → deserializes (missing fields use serde defaults)
///
/// ## Error handling
/// - IO errors (permission denied, etc.) are propagated
/// - JSON parse errors are propagated with file path context
pub fn load_from_file(path: &Path) -> std::result::Result<ModelPreference, String> {
    if !path.exists() {
        return Ok(ModelPreference::default());
    }

    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read model preference file '{}': {}", path.display(), e))?;

    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(ModelPreference::default());
    }

    serde_json::from_str(trimmed)
        .map_err(|e| format!("Failed to parse model preference file '{}': {}", path.display(), e))
}

/// Save model preferences to a JSON file.
///
/// Behavior:
/// - Creates parent directories if they don't exist
/// - Writes pretty-printed JSON for human readability
/// - Atomic-ish: writes full content (no append)
///
/// ## Error handling
/// - Directory creation failure → propagated
/// - File write failure → propagated
pub fn save_to_file(pref: &ModelPreference, path: &Path) -> std::result::Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create directory '{}': {}", parent.display(), e))?;
    }

    let json = serde_json::to_string_pretty(pref)
        .map_err(|e| format!("Failed to serialize model preference: {}", e))?;

    std::fs::write(path, json)
        .map_err(|e| format!("Failed to write model preference file '{}': {}", path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelId;

    #[test]
    fn test_qualified_model_id_parse_full() {
        let qid = QualifiedModelId::parse("anthropic:claude-opus-4-7");
        assert!(qid.is_qualified());
        assert_eq!(qid.provider_name(), Some("anthropic"));
        assert_eq!(qid.model_name(), "claude-opus-4-7");
    }

    #[test]
    fn test_qualified_model_id_parse_simple() {
        let qid = QualifiedModelId::parse("deepseek-v4-flash");
        assert!(!qid.is_qualified());
        assert_eq!(qid.provider_name(), None);
        assert_eq!(qid.model_name(), "deepseek-v4-flash");
    }

    #[test]
    fn test_qualified_model_id_parse_empty() {
        let qid = QualifiedModelId::parse("");
        assert!(!qid.is_qualified());
        assert_eq!(qid.model_name(), "");
    }

    #[test]
    fn test_qualified_model_id_parse_multiple_colons() {
        // "host:port:model" → provider="host", model="port:model"
        let qid = QualifiedModelId::parse("host:port:model");
        assert!(qid.is_qualified());
        assert_eq!(qid.provider_name(), Some("host"));
        assert_eq!(qid.model_name(), "port:model");
    }

    #[test]
    fn test_qualified_model_id_parse_whitespace() {
        let qid = QualifiedModelId::parse("  anthropic : claude-opus-4-7  ");
        assert!(qid.is_qualified());
        assert_eq!(qid.provider_name(), Some("anthropic"));
        assert_eq!(qid.model_name(), "claude-opus-4-7");
    }

    #[test]
    fn test_qualified_model_id_parse_empty_provider() {
        // ":model" → provider segment is empty, treated as None
        let qid = QualifiedModelId::parse(":model-name");
        assert!(!qid.is_qualified());
        assert_eq!(qid.model_name(), "model-name");
    }

    #[test]
    fn test_qualified_model_id_display_roundtrip() {
        let cases = [
            "anthropic:claude-opus-4-7",
            "deepseek-v4-flash",
            "openai:gpt-5",
        ];
        for input in cases {
            let qid = QualifiedModelId::parse(input);
            let display = qid.to_string();
            let reparsed = QualifiedModelId::parse(&display);
            assert_eq!(qid, reparsed, "roundtrip failed for '{}'", input);
        }
    }

    #[test]
    fn test_qualified_model_id_from_model_id() {
        let mid = ModelId("gpt-5".to_string());
        let qid: QualifiedModelId = mid.into();
        assert!(!qid.is_qualified());
        assert_eq!(qid.model_name(), "gpt-5");
    }

    #[test]
    fn test_qualified_model_id_from_tuple() {
        let pid = ProviderId("openai".to_string());
        let mid = ModelId("gpt-5".to_string());
        let qid: QualifiedModelId = (pid, mid).into();
        assert!(qid.is_qualified());
        assert_eq!(qid.provider_name(), Some("openai"));
        assert_eq!(qid.model_name(), "gpt-5");
    }

    #[test]
    fn test_model_preference_resolve_priority() {
        // last_selected > task_default > default
        let pref = ModelPreference {
            default: Some(QualifiedModelId::parse("default-model")),
            last_selected: None,
            task_defaults: {
                let mut m = HashMap::new();
                m.insert("code".to_string(), QualifiedModelId::parse("anthropic:claude-opus-4-7"));
                m
            },
            provider_aliases: HashMap::new(),
            model_aliases: HashMap::new(),
        };

        // No last_selected, with matching task → task_default wins
        assert_eq!(
            pref.resolve_for_task(Some("code")).unwrap().model_name(),
            "claude-opus-4-7"
        );

        // No last_selected, no matching task → default wins
        assert_eq!(
            pref.resolve_for_task(Some("writing")).unwrap().model_name(),
            "default-model"
        );
        assert_eq!(
            pref.resolve_for_task(None).unwrap().model_name(),
            "default-model"
        );

        // With last_selected → always wins
        let mut pref2 = pref.clone();
        pref2.set_last_selected(QualifiedModelId::parse("override-model"));
        assert_eq!(
            pref2.resolve_for_task(Some("code")).unwrap().model_name(),
            "override-model"
        );
    }

    #[test]
    fn test_model_preference_alias_resolution() {
        let pref = ModelPreference {
            default: None,
            last_selected: None,
            task_defaults: HashMap::new(),
            provider_aliases: {
                let mut m = HashMap::new();
                m.insert("ant".to_string(), "anthropic".to_string());
                m
            },
            model_aliases: {
                let mut m = HashMap::new();
                m.insert("opus".to_string(), QualifiedModelId::parse("anthropic:claude-opus-4-7"));
                m
            },
        };

        // Exact model alias match
        let resolved = pref.resolve_alias("opus");
        assert_eq!(resolved.provider_name(), Some("anthropic"));
        assert_eq!(resolved.model_name(), "claude-opus-4-7");

        // Provider alias expansion
        let resolved = pref.resolve_alias("ant:claude-sonnet-4-6");
        assert_eq!(resolved.provider_name(), Some("anthropic"));
        assert_eq!(resolved.model_name(), "claude-sonnet-4-6");

        // No alias match → pass-through
        let resolved = pref.resolve_alias("deepseek-v4-flash");
        assert!(!resolved.is_qualified());
        assert_eq!(resolved.model_name(), "deepseek-v4-flash");
    }

    #[test]
    fn test_model_preference_serde_roundtrip() {
        let pref = ModelPreference {
            default: Some(QualifiedModelId::parse("anthropic:claude-opus-4-7")),
            last_selected: Some(QualifiedModelId::parse("deepseek-v4-flash")),
            task_defaults: {
                let mut m = HashMap::new();
                m.insert("code".to_string(), QualifiedModelId::parse("anthropic:claude-opus-4-7"));
                m.insert("chat".to_string(), QualifiedModelId::parse("deepseek-v4-flash"));
                m
            },
            provider_aliases: {
                let mut m = HashMap::new();
                m.insert("ant".to_string(), "anthropic".to_string());
                m
            },
            model_aliases: {
                let mut m = HashMap::new();
                m.insert("opus".to_string(), QualifiedModelId::parse("anthropic:claude-opus-4-7"));
                m
            },
        };

        let json = serde_json::to_string_pretty(&pref).expect("serialize");
        let deserialized: ModelPreference = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(pref, deserialized);
    }

    #[test]
    fn test_model_preference_serde_missing_fields() {
        // Backward compat: JSON with only some fields still deserializes
        let json = r#"{"default": {"provider": null, "model": "gpt-5"}}"#;
        let pref: ModelPreference = serde_json::from_str(json).expect("deserialize partial");
        assert_eq!(pref.default.as_ref().unwrap().model_name(), "gpt-5");
        assert!(pref.last_selected.is_none());
        assert!(pref.task_defaults.is_empty());
    }

    #[test]
    fn test_model_preference_load_missing_file() {
        // Loading from a non-existent path returns default (no error)
        let path = Path::new("/tmp/abacus_test_nonexistent_12345/model_preference.json");
        let result = load_from_file(path);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), ModelPreference::default());
    }

    #[test]
    fn test_model_preference_save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("abacus_test_pref_roundtrip");
        let path = dir.join("model_preference.json");

        // Clean up from any previous run
        let _ = std::fs::remove_dir_all(&dir);

        let pref = ModelPreference {
            default: Some(QualifiedModelId::parse("openai:gpt-5")),
            last_selected: Some(QualifiedModelId::parse("anthropic:claude-opus-4-7")),
            task_defaults: HashMap::new(),
            provider_aliases: HashMap::new(),
            model_aliases: HashMap::new(),
        };

        save_to_file(&pref, &path).expect("save");
        let loaded = load_from_file(&path).expect("load");
        assert_eq!(pref, loaded);

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
