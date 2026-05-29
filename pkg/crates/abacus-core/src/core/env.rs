use serde::Serialize;
use std::sync::Arc;
use tokio::sync::RwLock;

use abacus_types::{ToolCost, ToolEffectiveness, ToolHandle, ToolId, ToolProvider, ToolSchema, ToolSecurity, ToolState};

use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};

/// Environment map — a compact snapshot of the local workspace injected into
/// the system prompt each turn (~200-400 tokens).
#[derive(Debug, Clone, Serialize)]
pub struct EnvMap {
    pub os: String,
    pub cwd: String,
    pub allowed_roots: Vec<String>,
    pub project: Option<ProjectInfo>,
    pub git: Option<GitStatus>,
    pub tool_activity: ToolActivity,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectInfo {
    pub workspace: Vec<String>,
    pub deps_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitStatus {
    pub branch: String,
    pub dirty_files: usize,
    pub last_commit: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ToolActivity {
    pub files_read: Vec<String>,
    pub files_written: Vec<String>,
    pub searches: Vec<String>,
    pub web_fetches: u32,
    pub last_bash_cmd: Option<String>,
}

impl Default for EnvMap {
    fn default() -> Self {
        Self::new()
    }
}

impl EnvMap {
    pub fn new() -> Self {
        let os = std::env::consts::OS.to_string();
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".into());
        Self {
            os,
            cwd,
            allowed_roots: Vec::new(),
            project: None,
            git: None,
            tool_activity: ToolActivity::default(),
        }
    }

    pub async fn refresh_git(&mut self) {
        let branch = git_cmd(&["rev-parse", "--abbrev-ref", "HEAD"]).await;
        let dirty = git_cmd(&["status", "--porcelain"]).await;
        let last_log = git_cmd(&["log", "--oneline", "-3"]).await;

        if let Some(b) = branch {
            let dirty_count = dirty.as_ref().map(|d| d.lines().count()).unwrap_or(0);
            self.git = Some(GitStatus {
                branch: b.trim().to_string(),
                dirty_files: dirty_count,
                last_commit: last_log.unwrap_or_default().trim().to_string(),
            });
        }
    }

    pub async fn refresh_project(&mut self) {
        let cargo_path = format!("{}/Cargo.toml", self.cwd);
        let content = tokio::fs::read_to_string(&cargo_path).await.ok();
        if let Some(text) = content {
            let deps: Vec<&str> = text
                .lines()
                .filter(|l| l.trim().starts_with("serde") || l.trim().starts_with("tokio"))
                .collect();
            let members: Vec<String> = text
                .lines()
                .filter(|l| l.trim().starts_with("\""))
                .map(|l| l.trim().trim_matches('"').trim_matches(',').to_string())
                .filter(|l| l.starts_with("crates/"))
                .collect();
            self.project = Some(ProjectInfo {
                workspace: members,
                deps_count: deps.len(),
            });
        }
    }

    pub fn set_allowed_roots(&mut self, roots: &[String]) {
        self.allowed_roots = roots.to_vec();
    }

    pub fn format_block(&self) -> String {
        let mut lines = vec!["## Environment".to_string()];
        lines.push(format!("  os: {}", self.os));
        lines.push(format!("  cwd: {}", self.cwd));

        if let Some(proj) = &self.project {
            lines.push(format!(
                "  workspace: [{}]",
                proj.workspace.join(", ")
            ));
        }

        if let Some(git) = &self.git {
            lines.push(format!("  git: {} ({} dirty)", git.branch, git.dirty_files));
            lines.push(format!("  recent: {}", git.last_commit.lines().next().unwrap_or("")));
        }

        let act = &self.tool_activity;
        if !act.files_read.is_empty() || !act.files_written.is_empty() {
            let reads = act.files_read.last().map(|s| s.as_str()).unwrap_or("");
            let writes = act.files_written.last().map(|s| s.as_str()).unwrap_or("");
            lines.push(format!("  last read: {reads} | last write: {writes}"));
        }

        lines.join("\n")
    }
}

async fn git_cmd(args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .output()
        .await
        .ok()?;
    if output.status.success() {
        String::from_utf8(output.stdout).ok()
    } else {
        None
    }
}

// ─── env.status Tool ───────────────────────────────────────────────────────

pub struct EnvToolExecutor {
    pub env_map: Arc<RwLock<EnvMap>>,
}

#[async_trait::async_trait]
impl ToolExecutor for EnvToolExecutor {
    async fn execute(&self, tool_id: &ToolId, _params: serde_json::Value, _ctx: &ExecutionContext) -> abacus_types::Result<serde_json::Value> {
        match tool_id.0.as_str() {
            "env_status" => {
                let env = self.env_map.read().await;
                Ok(serde_json::json!({"environment": env.format_block()}))
            }
            other => Err(abacus_types::KernelError::Other(format!(
                "unknown env tool: {other}"
            ))),
        }
    }
}

pub async fn register_env_tools(registry: &ToolRegistry, env_map: Arc<RwLock<EnvMap>>) {
    let executor = Arc::new(EnvToolExecutor {
        env_map,
    }) as Arc<dyn ToolExecutor>;

    let tool = ToolHandle {
        id: ToolId("env_status".into()),
        schema: ToolSchema {
            name: "env_status".into(),
            description: "Snapshot local workspace: OS/CWD/git/project/tool activity".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}, "required": []}),
            returns: None,
            security: Some(ToolSecurity {
                allowed_paths: None,
                max_size_mb: None,
                confirm_required: false,
                needs_sandbox: false,
            }),
            cost: Some(ToolCost {
                tokens: 128,
                latency: "10ms".into(),
                risk: "low".into(),
            }),
            examples: Vec::new(),
            applicable_task_kinds: None,
            idempotent: true,
                        schema_stable: false,        },
        provider: ToolProvider::BuiltIn,
        state: ToolState::Loaded,
        effectiveness: ToolEffectiveness::default(),
    };

    let tid = tool.id.clone();
    registry.register(tool).await;
    registry.register_executor(tid, executor).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_env_map_new() {
        let map = EnvMap::new();
        assert_eq!(map.os, std::env::consts::OS);
        assert!(!map.cwd.is_empty());
    }

    #[tokio::test]
    async fn test_env_map_git() {
        let mut map = EnvMap::new();
        map.refresh_git().await;
        // may be None if not in a git repo
        if let Some(git) = &map.git {
            assert!(!git.branch.is_empty());
        }
    }

    #[tokio::test]
    async fn test_env_map_format() {
        let mut map = EnvMap::new();
        map.refresh_git().await;
        map.refresh_project().await;
        let block = map.format_block();
        assert!(block.contains("os:"));
        assert!(block.contains("cwd:"));
        println!("{block}");
    }
}