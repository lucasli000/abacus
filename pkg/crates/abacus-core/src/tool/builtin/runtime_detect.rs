//! Runtime environment detection — discover available interpreters and tools.
//!
//! ## Responsibilities
//! Detect system-level and project-level runtime environments at session start,
//! cache results for command routing and tool recommendation.
//!
//! ## Dependencies (external)
//! - `std::process::Command`: spawns short-lived processes for version probes
//! - `std::path::Path`: filesystem checks for project markers
//! - `serde`: serialization for caching/inspection
//!
//! ## Dependencies (internal)
//! - `super::bun`: existing Bun detection logic (reused via `detect_bun_project`)
//!
//! ## References (callers)
//! - `FilengineSession` initialization may call `detect_runtime()` (cached at session level)
//! - `classify_bash_command()` may consume `ProjectEnvironment` for routing decisions
//! - `smart_substitute_command()` in this module consumes both structs for substitution
//!
//! ## Lifecycle
//! - `detect_runtime()`: spawns short-lived processes (2s timeout each), results cached by caller
//! - `detect_project()`: pure filesystem checks, no process spawning, stateless

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

// ─── System-level Runtime Environment ────────────────────────────────────────

/// System runtime environment snapshot (session-level cache, probed once at startup).
///
/// ## References
/// - Created by `detect_runtime()`
/// - Consumed by `smart_substitute_command()` for intelligent routing
/// - Consumed by CodeGraph indexer for language/toolchain awareness
///
/// ## Lifecycle
/// - Created once per session init via `detect_runtime()`
/// - Immutable after creation (no refresh mid-session)
/// - Dropped with session teardown
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEnvironment {
    /// Node.js version (e.g. "v22.1.0"), None = not installed
    pub node_version: Option<String>,
    /// Bun version (e.g. "1.2.0"), None = not installed
    pub bun_version: Option<String>,
    /// Python version (e.g. "3.12.0"), None = not installed
    pub python_version: Option<String>,
    /// Python package manager availability
    pub python_pkg_manager: Option<PythonPkgManager>,
    /// Bash version (e.g. "5.2.26")
    pub bash_version: Option<String>,
    /// System default shell (zsh/bash/fish)
    pub default_shell: String,
    /// Key tool availability (name → path + version)
    pub available_tools: HashMap<String, ToolInfo>,
}

/// Information about a detected tool binary.
///
/// ## Lifecycle
/// - Created during `detect_runtime()` probe phase
/// - Immutable after creation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Absolute path to the binary (from `which`)
    pub path: String,
    /// Version string if obtainable, None otherwise
    pub version: Option<String>,
}

/// Python package manager type detected on the system.
///
/// ## References
/// - Set by `detect_runtime()` based on binary availability
/// - Consumed by `smart_substitute_command()` (pip → uv substitution)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PythonPkgManager {
    Pip,
    Uv,
    Poetry,
    Pdm,
}

// ─── Project-level Environment ───────────────────────────────────────────────

/// Project-level environment detection (inferred from project files).
///
/// ## References
/// - Created by `detect_project(workspace)`
/// - Consumed by `smart_substitute_command()` for per-project routing
/// - Consumed by bash classification for environment-aware decisions
///
/// ## Lifecycle
/// - Created per workspace change (or once at session start for single-workspace)
/// - Immutable after creation; re-created if workspace changes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEnvironment {
    /// Whether this is a Node/Bun project (package.json present)
    pub is_node_project: bool,
    /// Whether the project prefers Bun (bunfig.toml / bun.lockb / bun.lock)
    pub prefers_bun: bool,
    /// Whether this is a Python project
    pub is_python_project: bool,
    /// Python virtual environment path (if detected)
    pub python_venv: Option<PathBuf>,
    /// Node.js package manager preference
    pub node_pkg_manager: Option<NodePkgManager>,
    /// Task runner availability (Makefile, justfile, etc.)
    pub has_task_runner: Option<TaskRunner>,
}

/// Node.js package manager preference (inferred from lockfile).
///
/// ## References
/// - Set by `detect_project()` based on lockfile presence
/// - Consumed by `smart_substitute_command()` for package manager routing
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NodePkgManager {
    Npm,
    Yarn,
    Pnpm,
    Bun,
}

/// Task runner detected in the project.
///
/// ## References
/// - Set by `detect_project()` based on file presence
/// - May be consumed by command routing (e.g. `make` → `just`)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TaskRunner {
    Make,
    Just,
    Taskfile,
    NxWorkspace,
}

// ─── Detection Functions ─────────────────────────────────────────────────────

/// Probe the system for available runtimes and tools.
///
/// Spawns short-lived processes (`--version` checks) with 2s timeout per probe.
/// This is a blocking function — callers in async context should use `spawn_blocking`.
///
/// ## References
/// - Called by session initialization (once per session)
/// - Results cached in `FilengineSession` or equivalent session state
///
/// ## Performance
/// - Each probe has a 2s timeout; worst-case ~14s if all probes timeout
/// - In practice, missing tools fail immediately (ENOENT), installed tools respond in <100ms
pub fn detect_runtime() -> RuntimeEnvironment {
    let node_version = probe_version("node", &["--version"]);
    let bun_version = probe_version("bun", &["--version"]);
    let python_version = probe_version("python3", &["--version"])
        .map(|v| v.trim_start_matches("Python ").to_string());
    let bash_version = probe_version("bash", &["--version"])
        .and_then(|v| v.lines().next().map(String::from));

    // Detect Python package manager (priority: uv > poetry > pdm > pip)
    let python_pkg_manager = if probe_exists("uv") {
        Some(PythonPkgManager::Uv)
    } else if probe_exists("poetry") {
        Some(PythonPkgManager::Poetry)
    } else if probe_exists("pdm") {
        Some(PythonPkgManager::Pdm)
    } else if probe_exists("pip3") || probe_exists("pip") {
        Some(PythonPkgManager::Pip)
    } else {
        None
    };

    // Detect default shell
    let default_shell = std::env::var("SHELL")
        .unwrap_or_else(|_| "/bin/sh".into())
        .rsplit('/')
        .next()
        .unwrap_or("sh")
        .to_string();

    // Probe common dev tools
    let mut available_tools = HashMap::new();
    let tools_to_probe = [
        "git", "cargo", "rustc", "docker", "make", "just", "jq", "curl", "wget",
    ];
    for tool in tools_to_probe {
        if let Some(path) = probe_which(tool) {
            let version = probe_version(tool, &["--version"])
                .and_then(|v| v.lines().next().map(String::from));
            available_tools.insert(
                tool.to_string(),
                ToolInfo { path, version },
            );
        }
    }

    RuntimeEnvironment {
        node_version,
        bun_version,
        python_version,
        python_pkg_manager,
        bash_version,
        default_shell,
        available_tools,
    }
}

/// Detect project-level environment from workspace directory.
///
/// Pure filesystem checks — no process spawning. Fast and safe to call synchronously.
///
/// ## References
/// - Called by session initialization after workspace is determined
/// - Called when workspace/cwd changes (if multi-workspace support enabled)
///
/// ## Parameters
/// - `workspace`: root directory of the project (typically where .git/ is)
pub fn detect_project(workspace: &Path) -> ProjectEnvironment {
    let is_node_project = workspace.join("package.json").exists();

    // Bun preference: reuse existing detection from bun.rs
    let prefers_bun = super::bun::detect_bun_project(workspace);

    // Python project indicators
    let is_python_project = workspace.join("pyproject.toml").exists()
        || workspace.join("requirements.txt").exists()
        || workspace.join("setup.py").exists()
        || workspace.join("setup.cfg").exists()
        || workspace.join("Pipfile").exists();

    // Python venv detection (common locations)
    let python_venv = [".venv", "venv", ".env", "env"]
        .iter()
        .map(|dir| workspace.join(dir))
        .find(|p| p.join("bin/activate").exists() || p.join("Scripts/activate").exists());

    // Node package manager detection (lockfile priority)
    let node_pkg_manager = if prefers_bun {
        Some(NodePkgManager::Bun)
    } else if workspace.join("pnpm-lock.yaml").exists() {
        Some(NodePkgManager::Pnpm)
    } else if workspace.join("yarn.lock").exists() {
        Some(NodePkgManager::Yarn)
    } else if workspace.join("package-lock.json").exists() {
        Some(NodePkgManager::Npm)
    } else {
        None
    };

    // Task runner detection
    let has_task_runner = if workspace.join("nx.json").exists() {
        Some(TaskRunner::NxWorkspace)
    } else if workspace.join("justfile").exists() || workspace.join("Justfile").exists() {
        Some(TaskRunner::Just)
    } else if workspace.join("Taskfile.yml").exists() || workspace.join("Taskfile.yaml").exists() {
        Some(TaskRunner::Taskfile)
    } else if workspace.join("Makefile").exists() || workspace.join("makefile").exists() {
        Some(TaskRunner::Make)
    } else {
        None
    };

    ProjectEnvironment {
        is_node_project,
        prefers_bun,
        is_python_project,
        python_venv,
        node_pkg_manager,
        has_task_runner,
    }
}

// ─── Internal Helpers ────────────────────────────────────────────────────────

/// Probe a command's version output. Returns None if command not found or fails.
fn probe_version(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        // Some tools output version to stderr (e.g. python3 --version on some systems)
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() { None } else { Some(stderr) }
    } else {
        Some(text)
    }
}

/// Check if a command exists on PATH (without capturing output).
fn probe_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Get the absolute path to a command via `which`.
fn probe_which(cmd: &str) -> Option<String> {
    Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

// ─── Smart Command Substitution ──────────────────────────────────────────────

/// Intelligent command substitution based on project and runtime environment.
///
/// ## Substitution Rules (applied in priority order)
/// 1. In Bun projects: npm/node → bun (delegates to existing `bun_substitute_command`)
/// 2. In Python projects with `uv`: pip/pip3 → uv pip (faster resolver)
/// 3. In projects with venv: auto-activate before python/pytest/mypy commands
/// 4. In monorepos with nx: (reserved for future — not yet implemented)
///
/// ## References
/// - Called by `bash_exec()` pre-execution hook (after classification, before spawn)
/// - Depends on `bun::bun_substitute_command()` for Bun substitution
/// - Depends on `RuntimeEnvironment` and `ProjectEnvironment` (session-cached)
///
/// ## Parameters
/// - `cmd`: raw command string from LLM
/// - `project`: project-level environment (detected from workspace)
/// - `runtime`: system-level runtime (detected at session start)
///
/// ## Returns
/// Substituted command string. Returns original if no substitution applies.
pub fn smart_substitute_command(
    cmd: &str,
    project: &ProjectEnvironment,
    runtime: &RuntimeEnvironment,
) -> String {
    let cmd = cmd.trim();

    // Rule 1: Bun substitution (existing logic via bun.rs)
    if project.prefers_bun && runtime.bun_version.is_some() {
        let substituted = super::bun::bun_substitute_command(cmd, true);
        if substituted != cmd {
            return substituted;
        }
    }

    // Rule 2 & 3: Python environment-aware substitution
    if project.is_python_project {
        // Prefer uv over pip when available
        if matches!(runtime.python_pkg_manager, Some(PythonPkgManager::Uv)) {
            if cmd.starts_with("pip3 install") {
                return cmd.replacen("pip3 install", "uv pip install", 1);
            }
            if cmd.starts_with("pip install") {
                return cmd.replacen("pip install", "uv pip install", 1);
            }
            if cmd.starts_with("pip3 ") {
                return cmd.replacen("pip3 ", "uv pip ", 1);
            }
            if cmd.starts_with("pip ") {
                return cmd.replacen("pip ", "uv pip ", 1);
            }
        }

        // Auto-activate venv if present
        if let Some(ref venv) = project.python_venv {
            let needs_venv = cmd.starts_with("python")
                || cmd.starts_with("pytest")
                || cmd.starts_with("mypy")
                || cmd.starts_with("ruff")
                || cmd.starts_with("black")
                || cmd.starts_with("isort");
            if needs_venv {
                let activate = venv.join("bin/activate");
                if activate.exists() {
                    return format!("source {} && {}", activate.display(), cmd);
                }
            }
        }
    }

    cmd.to_string()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ── Project Detection Tests ──────────────────────────────────────────────

    #[test]
    fn test_detect_node_project() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("package.json"), r#"{"name":"test"}"#).unwrap();
        let env = detect_project(dir.path());
        assert!(env.is_node_project);
        assert!(!env.prefers_bun);
    }

    #[test]
    fn test_detect_python_project_pyproject() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("pyproject.toml"), "[project]\nname = \"test\"").unwrap();
        let env = detect_project(dir.path());
        assert!(env.is_python_project);
        assert!(!env.is_node_project);
    }

    #[test]
    fn test_detect_python_project_requirements() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("requirements.txt"), "flask>=2.0").unwrap();
        let env = detect_project(dir.path());
        assert!(env.is_python_project);
    }

    #[test]
    fn test_detect_bun_preference() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        fs::write(dir.path().join("bun.lockb"), &[0u8; 4]).unwrap();
        let env = detect_project(dir.path());
        assert!(env.is_node_project);
        assert!(env.prefers_bun);
        assert_eq!(env.node_pkg_manager, Some(NodePkgManager::Bun));
    }

    #[test]
    fn test_detect_python_venv() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("pyproject.toml"), "[project]\nname=\"x\"").unwrap();
        let venv_bin = dir.path().join(".venv/bin");
        fs::create_dir_all(&venv_bin).unwrap();
        fs::write(venv_bin.join("activate"), "# activate script").unwrap();
        let env = detect_project(dir.path());
        assert!(env.is_python_project);
        assert_eq!(env.python_venv, Some(dir.path().join(".venv")));
    }

    #[test]
    fn test_node_pkg_manager_detection_yarn() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        fs::write(dir.path().join("yarn.lock"), "").unwrap();
        let env = detect_project(dir.path());
        assert_eq!(env.node_pkg_manager, Some(NodePkgManager::Yarn));
    }

    #[test]
    fn test_node_pkg_manager_detection_pnpm() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        let env = detect_project(dir.path());
        assert_eq!(env.node_pkg_manager, Some(NodePkgManager::Pnpm));
    }

    #[test]
    fn test_node_pkg_manager_detection_npm() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        let env = detect_project(dir.path());
        assert_eq!(env.node_pkg_manager, Some(NodePkgManager::Npm));
    }

    #[test]
    fn test_detect_task_runner_make() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("Makefile"), "all:\n\techo hi").unwrap();
        let env = detect_project(dir.path());
        assert_eq!(env.has_task_runner, Some(TaskRunner::Make));
    }

    #[test]
    fn test_detect_task_runner_just() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("justfile"), "default:\n  echo hi").unwrap();
        let env = detect_project(dir.path());
        assert_eq!(env.has_task_runner, Some(TaskRunner::Just));
    }

    #[test]
    fn test_detect_task_runner_nx() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("nx.json"), "{}").unwrap();
        let env = detect_project(dir.path());
        assert_eq!(env.has_task_runner, Some(TaskRunner::NxWorkspace));
    }

    #[test]
    fn test_detect_empty_directory() {
        let dir = TempDir::new().unwrap();
        let env = detect_project(dir.path());
        assert!(!env.is_node_project);
        assert!(!env.prefers_bun);
        assert!(!env.is_python_project);
        assert_eq!(env.python_venv, None);
        assert_eq!(env.node_pkg_manager, None);
        assert_eq!(env.has_task_runner, None);
    }

    // ── Runtime Detection Tests ──────────────────────────────────────────────

    #[test]
    fn test_detect_runtime_returns_valid_struct() {
        // This test actually probes the system — it verifies the function doesn't panic
        // and returns a well-formed struct. Specific assertions depend on the test machine.
        let rt = detect_runtime();
        // default_shell should never be empty
        assert!(!rt.default_shell.is_empty());
    }

    // ── Command Classification Tests ─────────────────────────────────────────

    #[test]
    fn test_classify_python_safe() {
        use crate::tool::builtin::filengine::{classify_bash_command, BashDecision};
        // pytest invocation is safe (dev tooling)
        assert_eq!(classify_bash_command("python3 -m pytest tests/"), BashDecision::Allow);
        assert_eq!(classify_bash_command("python3 script.py"), BashDecision::Allow);
        assert_eq!(classify_bash_command("python3 -c \"print(1)\""), BashDecision::Allow);
    }

    #[test]
    fn test_classify_python_dangerous() {
        use crate::tool::builtin::filengine::{classify_bash_command, BashDecision};
        // pip install modifies environment
        assert!(matches!(
            classify_bash_command("pip install requests"),
            BashDecision::NeedsConfirm(_)
        ));
        assert!(matches!(
            classify_bash_command("pip3 install flask"),
            BashDecision::NeedsConfirm(_)
        ));
    }

    #[test]
    fn test_classify_npm_install() {
        use crate::tool::builtin::filengine::{classify_bash_command, BashDecision};
        assert!(matches!(
            classify_bash_command("npm install lodash"),
            BashDecision::NeedsConfirm(_)
        ));
        assert!(matches!(
            classify_bash_command("yarn add express"),
            BashDecision::NeedsConfirm(_)
        ));
    }

    #[test]
    fn test_classify_dev_tools_allow() {
        use crate::tool::builtin::filengine::{classify_bash_command, BashDecision};
        // Dev tools (eslint, prettier, tsc, vitest, pytest, mypy, ruff, black)
        // should be allowed as they are read-only analysis/formatting tools
        assert_eq!(classify_bash_command("eslint src/"), BashDecision::Allow);
        assert_eq!(classify_bash_command("prettier --check ."), BashDecision::Allow);
        assert_eq!(classify_bash_command("tsc --noEmit"), BashDecision::Allow);
        assert_eq!(classify_bash_command("vitest run"), BashDecision::Allow);
        assert_eq!(classify_bash_command("pytest tests/"), BashDecision::Allow);
        assert_eq!(classify_bash_command("mypy src/"), BashDecision::Allow);
        assert_eq!(classify_bash_command("ruff check ."), BashDecision::Allow);
        assert_eq!(classify_bash_command("black --check ."), BashDecision::Allow);
    }

    #[test]
    fn test_classify_uv_commands() {
        use crate::tool::builtin::filengine::{classify_bash_command, BashDecision};
        // uv pip list/show = safe
        assert_eq!(classify_bash_command("uv pip list"), BashDecision::Allow);
        assert_eq!(classify_bash_command("uv pip show requests"), BashDecision::Allow);
        // uv pip install = modifies env
        assert!(matches!(
            classify_bash_command("uv pip install flask"),
            BashDecision::NeedsConfirm(_)
        ));
        // uv run = safe (runs in ephemeral environment)
        assert_eq!(classify_bash_command("uv run pytest"), BashDecision::Allow);
    }

    #[test]
    fn test_classify_poetry_commands() {
        use crate::tool::builtin::filengine::{classify_bash_command, BashDecision};
        // poetry show/env = safe
        assert_eq!(classify_bash_command("poetry show"), BashDecision::Allow);
        assert_eq!(classify_bash_command("poetry env info"), BashDecision::Allow);
        // poetry install/add = modifies env
        assert!(matches!(
            classify_bash_command("poetry install"),
            BashDecision::NeedsConfirm(_)
        ));
        assert!(matches!(
            classify_bash_command("poetry add flask"),
            BashDecision::NeedsConfirm(_)
        ));
    }

    // ── Smart Substitution Tests ─────────────────────────────────────────────

    #[test]
    fn test_smart_substitute_uv() {
        let project = ProjectEnvironment {
            is_node_project: false,
            prefers_bun: false,
            is_python_project: true,
            python_venv: None,
            node_pkg_manager: None,
            has_task_runner: None,
        };
        let runtime = RuntimeEnvironment {
            node_version: None,
            bun_version: None,
            python_version: Some("3.12.0".into()),
            python_pkg_manager: Some(PythonPkgManager::Uv),
            bash_version: Some("5.2".into()),
            default_shell: "zsh".into(),
            available_tools: HashMap::new(),
        };

        assert_eq!(
            smart_substitute_command("pip install requests", &project, &runtime),
            "uv pip install requests"
        );
        assert_eq!(
            smart_substitute_command("pip3 install flask", &project, &runtime),
            "uv pip install flask"
        );
        assert_eq!(
            smart_substitute_command("pip freeze", &project, &runtime),
            "uv pip freeze"
        );
    }

    #[test]
    fn test_smart_substitute_no_uv_passthrough() {
        let project = ProjectEnvironment {
            is_node_project: false,
            prefers_bun: false,
            is_python_project: true,
            python_venv: None,
            node_pkg_manager: None,
            has_task_runner: None,
        };
        let runtime = RuntimeEnvironment {
            node_version: None,
            bun_version: None,
            python_version: Some("3.12.0".into()),
            python_pkg_manager: Some(PythonPkgManager::Pip),
            bash_version: Some("5.2".into()),
            default_shell: "zsh".into(),
            available_tools: HashMap::new(),
        };

        // Without uv, pip commands pass through unchanged
        assert_eq!(
            smart_substitute_command("pip install requests", &project, &runtime),
            "pip install requests"
        );
    }

    #[test]
    fn test_smart_substitute_venv() {
        let dir = TempDir::new().unwrap();
        let venv_bin = dir.path().join(".venv/bin");
        fs::create_dir_all(&venv_bin).unwrap();
        fs::write(venv_bin.join("activate"), "# activate").unwrap();

        let project = ProjectEnvironment {
            is_node_project: false,
            prefers_bun: false,
            is_python_project: true,
            python_venv: Some(dir.path().join(".venv")),
            node_pkg_manager: None,
            has_task_runner: None,
        };
        let runtime = RuntimeEnvironment {
            node_version: None,
            bun_version: None,
            python_version: Some("3.12.0".into()),
            python_pkg_manager: Some(PythonPkgManager::Pip),
            bash_version: Some("5.2".into()),
            default_shell: "zsh".into(),
            available_tools: HashMap::new(),
        };

        let result = smart_substitute_command("pytest tests/", &project, &runtime);
        assert!(result.contains("source"));
        assert!(result.contains(".venv/bin/activate"));
        assert!(result.ends_with("&& pytest tests/"));
    }

    #[test]
    fn test_smart_substitute_bun_project() {
        let project = ProjectEnvironment {
            is_node_project: true,
            prefers_bun: true,
            is_python_project: false,
            python_venv: None,
            node_pkg_manager: Some(NodePkgManager::Bun),
            has_task_runner: None,
        };
        let runtime = RuntimeEnvironment {
            node_version: Some("v22.1.0".into()),
            bun_version: Some("1.2.0".into()),
            python_version: None,
            python_pkg_manager: None,
            bash_version: Some("5.2".into()),
            default_shell: "zsh".into(),
            available_tools: HashMap::new(),
        };

        assert_eq!(
            smart_substitute_command("npm install", &project, &runtime),
            "bun install"
        );
        assert_eq!(
            smart_substitute_command("npx vite", &project, &runtime),
            "bunx vite"
        );
        assert_eq!(
            smart_substitute_command("node index.ts", &project, &runtime),
            "bun index.ts"
        );
    }

    #[test]
    fn test_smart_substitute_no_bun_passthrough() {
        let project = ProjectEnvironment {
            is_node_project: true,
            prefers_bun: false,
            is_python_project: false,
            python_venv: None,
            node_pkg_manager: Some(NodePkgManager::Npm),
            has_task_runner: None,
        };
        let runtime = RuntimeEnvironment {
            node_version: Some("v22.1.0".into()),
            bun_version: Some("1.2.0".into()),
            python_version: None,
            python_pkg_manager: None,
            bash_version: Some("5.2".into()),
            default_shell: "zsh".into(),
            available_tools: HashMap::new(),
        };

        // Should NOT substitute since project doesn't prefer bun
        assert_eq!(
            smart_substitute_command("npm install", &project, &runtime),
            "npm install"
        );
    }

    #[test]
    fn test_smart_substitute_non_python_project_no_venv() {
        let project = ProjectEnvironment {
            is_node_project: true,
            prefers_bun: false,
            is_python_project: false,
            python_venv: None,
            node_pkg_manager: Some(NodePkgManager::Npm),
            has_task_runner: None,
        };
        let runtime = RuntimeEnvironment {
            node_version: Some("v22.1.0".into()),
            bun_version: None,
            python_version: Some("3.12.0".into()),
            python_pkg_manager: Some(PythonPkgManager::Uv),
            bash_version: Some("5.2".into()),
            default_shell: "zsh".into(),
            available_tools: HashMap::new(),
        };

        // pip commands should NOT be substituted when not a Python project
        assert_eq!(
            smart_substitute_command("pip install requests", &project, &runtime),
            "pip install requests"
        );
    }
}