//! paths — Abacus 路径分层解析
//!
//! ## 场景
//! 多窗口/多实例下统一路径解析。三层架构：
//! - **全局层** `~/.abacus/`：配置、扩展、SQLite 共享 db、全局历史
//! - **项目层** `~/.abacus/projects/<escaped-cwd>/`：sessions, logs, 项目级 memory
//! - **Session 层**：项目层下 `sessions/{uuid}.jsonl` 等
//!
//! ## 引用关系
//! - 被所有需要持久化路径的模块调用（替代 hardcoded `~/.abacus/*`）
//! - 消费方：knowledge_store / memory_palace / deduction / sandbox / tui / engine_init
//!
//! ## 生命周期
//! - 创建：lazy（每次调用即解析，函数级无状态）
//! - 销毁：N/A（纯函数）
//! - 副作用：仅 `ensure_*` 函数会调 `std::fs::create_dir_all`
//!
//! ## ABACUS_HOME env var
//! 用户可设置 `ABACUS_HOME=/some/path` 完全覆盖全局根；未设置则用 `$HOME/.abacus`。
//! 项目层在全局根下，自动跟随覆盖。

use std::path::{Path, PathBuf};

/// 全局根目录。优先 `ABACUS_HOME` env，否则 `$HOME/.abacus`。
///
/// 副作用：无（仅解析路径）
pub fn global_dir() -> PathBuf {
    if let Ok(v) = std::env::var("ABACUS_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".abacus")
}

// ─── 数据目录（记忆/知识库/日志统一归档）────────────────────────────────
// V43.7: 所有持久化数据文件统一到 ~/.abacus/data/
// 向后兼容：如果旧路径存在（~/.abacus/knowledge.db），优先使用旧路径（避免迁移中断）
// 新安装直接用 data/ 子目录

/// 数据存储根目录
pub fn data_dir() -> PathBuf { global_dir().join("data") }

fn resolve_db(name: &str) -> PathBuf {
    let legacy = global_dir().join(name);
    if legacy.exists() {
        return legacy; // 向后兼容：旧文件存在时不迁移
    }
    let data = data_dir();
    // 确保 data/ 目录存在
    let _ = std::fs::create_dir_all(&data);
    data.join(name)
}

pub fn knowledge_db() -> PathBuf { resolve_db("knowledge.db") }
pub fn palace_db() -> PathBuf { resolve_db("palace.db") }
pub fn memory_db() -> PathBuf { resolve_db("memory.db") }
pub fn deduction_metrics_db() -> PathBuf { resolve_db("deduction_metrics.db") }
pub fn task_logs_db() -> PathBuf { resolve_db("task_logs.db") }

// ─── 全局配置 / 历史 / 全局 memory ──────────────────────────────────────

pub fn config_yaml() -> PathBuf { global_dir().join("config.yaml") }
pub fn providers_json() -> PathBuf { global_dir().join("providers.json") }
pub fn models_yaml() -> PathBuf { global_dir().join("models.yaml") }
pub fn security_yaml() -> PathBuf { global_dir().join("security.yaml") }
pub fn abacusbr_md() -> PathBuf { global_dir().join("abacusbr.md") }
pub fn conf_d_dir() -> PathBuf { global_dir().join("conf.d") }
pub fn history_jsonl() -> PathBuf { global_dir().join("history.jsonl") }
/// 全局 markdown memory（跨项目复用的通用知识，agent 可加载为基底）
pub fn global_memory_dir() -> PathBuf { global_dir().join("memory") }

// ─── 项目层 ─────────────────────────────────────────────────────────────

/// projects 容器目录（所有项目都在此下）
pub fn projects_dir() -> PathBuf { global_dir().join("projects") }

/// 指定 cwd 的项目目录。转义规则：路径分隔符替换为 `-`
///
/// 例：`/path/to/project` → `~/.abacus/projects/-path-to-project/`
pub fn project_dir(cwd: &Path) -> PathBuf {
    projects_dir().join(escape_cwd(cwd))
}

/// 当前 cwd 的项目目录
pub fn current_project_dir() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    project_dir(&cwd)
}

/// 项目级 sessions 目录
pub fn project_sessions_dir(cwd: &Path) -> PathBuf { project_dir(cwd).join("sessions") }

/// 项目级 logs 目录
pub fn project_logs_dir(cwd: &Path) -> PathBuf { project_dir(cwd).join("logs") }

/// 项目级 markdown memory 目录（项目特定规则，agent 进入时加载）
pub fn project_memory_dir(cwd: &Path) -> PathBuf { project_dir(cwd).join("memory") }

/// 当前项目的 sessions / logs / memory 快捷
pub fn current_sessions_dir() -> PathBuf { current_project_dir().join("sessions") }
pub fn current_logs_dir() -> PathBuf { current_project_dir().join("logs") }
pub fn current_project_memory_dir() -> PathBuf { current_project_dir().join("memory") }

// ─── 工具进程 tmp（per-PID，避免多实例 plugin 互踩）────────────────────

/// MCP plugin tmp 目录，per-PID 隔离
///
/// 形如 `/tmp/.abacus-{pid}/plugins`
pub fn mcp_plugin_tmp_dir() -> PathBuf {
    PathBuf::from(format!("/tmp/.abacus-{}/plugins", std::process::id()))
}

// ─── cross-session: 进程注册表 ──────────────────────────────────────────

/// 进程注册表目录 `~/.abacus/sessions/`
///
/// 每个活跃 abacus 进程一个 PID json（`{pid}.json`），让多实例可互相发现
/// （cron 触发 / 跨实例协调 / 调试可视化的基础）。
pub fn process_registry_dir() -> PathBuf { global_dir().join("sessions") }

/// 当前进程的注册文件路径
pub fn current_process_file() -> PathBuf {
    process_registry_dir().join(format!("{}.json", std::process::id()))
}

/// 确保注册表目录存在
pub fn ensure_process_registry_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(process_registry_dir())
}

// ─── 转义规则 ───────────────────────────────────────────────────────────

/// 把 cwd 绝对路径转义为安全的目录名。
///
/// 规则：
/// - `/` → `-`
/// - `\` → `-` (Windows path)
/// - `:` → `-` (Windows drive letter)
/// - 其他字符保留
///
/// 注意：转义不可逆，但保证幂等（同一 cwd 永远产生同一 escaped 名）。
pub fn escape_cwd(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect()
}

// ─── 副作用：确保目录存在 ────────────────────────────────────────────────

/// 确保全局目录存在。在 abacus 启动早期调用。
pub fn ensure_global_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(global_dir())?;
    std::fs::create_dir_all(projects_dir())?;
    std::fs::create_dir_all(global_memory_dir())?;
    Ok(())
}

/// 确保指定 cwd 的项目目录及子目录存在。在 session 启动时调用。
pub fn ensure_project_dirs(cwd: &Path) -> std::io::Result<()> {
    let p = project_dir(cwd);
    std::fs::create_dir_all(p.join("sessions"))?;
    std::fs::create_dir_all(p.join("logs"))?;
    std::fs::create_dir_all(p.join("memory"))?;
    Ok(())
}

/// 确保当前 cwd 的项目目录存在
pub fn ensure_current_project_dirs() -> std::io::Result<()> {
    let cwd = std::env::current_dir()?;
    ensure_project_dirs(&cwd)
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_cwd_basic() {
        assert_eq!(
            escape_cwd(Path::new("/home/user/myproject")),
            "-home-user-myproject"
        );
    }

    #[test]
    fn test_escape_cwd_with_dots() {
        // dots are preserved; only path separators replaced
        assert_eq!(
            escape_cwd(Path::new("/home/u/.config/foo")),
            "-home-u-.config-foo"
        );
    }

    #[test]
    fn test_escape_cwd_idempotent() {
        let p = Path::new("/home/user/some/proj");
        let a = escape_cwd(p);
        let b = escape_cwd(p);
        assert_eq!(a, b, "escape must be deterministic");
    }

    #[test]
    fn test_abacus_home_env_var_overrides() {
        // 合并为单测避免 cargo test 并发时 env var 竞争。
        // 两个原独立测试都依赖 ABACUS_HOME process-global
        // 一起运行时会互踩（一个 set 后另一个 remove 接上）。
        let original = std::env::var("ABACUS_HOME").ok();

        // case 1: global_dir 读 ABACUS_HOME
        std::env::set_var("ABACUS_HOME", "/tmp/test-abacus-home");
        assert_eq!(global_dir(), PathBuf::from("/tmp/test-abacus-home"));

        // case 2: project_dir 在 global 下的 projects/<escaped>
        std::env::set_var("ABACUS_HOME", "/tmp/test-abacus-home-2");
        let p = project_dir(Path::new("/x/y/z"));
        assert_eq!(p, PathBuf::from("/tmp/test-abacus-home-2/projects/-x-y-z"));

        // restore
        match original {
            Some(v) => std::env::set_var("ABACUS_HOME", v),
            None => std::env::remove_var("ABACUS_HOME"),
        }
    }

    #[test]
    fn test_mcp_plugin_tmp_dir_includes_pid() {
        let p = mcp_plugin_tmp_dir();
        let s = p.to_string_lossy();
        assert!(s.contains(&std::process::id().to_string()));
        assert!(s.starts_with("/tmp/.abacus-"));
    }
}
