//! paths — Abacus 路径分层解析
//!
//! ## 场景
//! 多窗口/多实例下统一路径解析。三层架构：
//! - **全局层** `~/.abacus/`：配置、扩展、SQLite 共享 db、全局历史
//! - **项目层** `<cwd>/`：项目级 abacusbr.md / abacusbr.local.md
//! - **Session 层**：项目层下 `sessions/{uuid}.jsonl` 等
//!
//! ## 目录结构
//! ```
//! ~/.abacus/
//! ├── abacusbr.md               # 行为规范（顶层独立文件）
//! ├── palace/                   # 双宫殿记忆系统
//! │   ├── palace.db
//! │   ├── memory.db
//! │   ├── memory_embeddings.db
//! │   ├── knowledge.db
//! │   └── md/                   # Markdown 记忆文件
//! ├── config/                   # 配置文件
//! │   ├── config.yaml
//! │   ├── providers.yaml
//! │   ├── models.yaml
//! │   ├── security.yaml
//! │   ├── mcp_servers.yaml
//! │   ├── model_preference.json
//! │   ├── always_allow.json
//! │   ├── policy.toml
//! │   ├── prompt_roles.toml
//! │   ├── subscenes.toml
//! │   ├── profile.json
//! │   └── conf.d/
//! ├── data/                     # 运行时数据
//! │   ├── deduction_metrics.db
//! │   ├── task_logs.db
//! │   ├── models.cache.json
//! │   ├── history.jsonl
//! │   └── sessions.db
//! ├── sessions/                 # 进程注册表
//! ├── projects/                 # 项目注册表
//! │   └── registry.json
//! ├── meetings/
//! ├── skills/
//! └── disclaimer_ack
//! ```
//!
//! ## 引用关系
//! - 被所有需要持久化路径的模块调用（替代 hardcoded `~/.abacus/*`）
//! - 消费方：knowledge_store / memory_palace / deduction / sandbox / tui / engine_init

use std::path::{Path, PathBuf};

/// 全局根目录。优先 `ABACUS_HOME` env，否则 `ABACUS_CONFIG_DIR`（向后兼容），否则 `$HOME/.abacus`。
///
/// 副作用：无（仅解析路径）
pub fn global_dir() -> PathBuf {
    if let Ok(v) = std::env::var("ABACUS_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    // 向后兼容：ABACUS_CONFIG_DIR 覆盖
    if let Ok(v) = std::env::var("ABACUS_CONFIG_DIR") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".abacus")
}

// ─── 根级文件 ──────────────────────────────────────────────────────────

/// 行为规范（顶层独立文件）
pub fn abacusbr_md() -> PathBuf { global_dir().join("abacusbr.md") }

/// 免责声明标记
pub fn disclaimer_ack() -> PathBuf { global_dir().join("disclaimer_ack") }

// ─── config/ 配置文件 ─────────────────────────────────────────────────

pub fn config_yaml() -> PathBuf { global_dir().join("config/config.yaml") }
pub fn providers_yaml() -> PathBuf { global_dir().join("config/providers.yaml") }
pub fn models_yaml() -> PathBuf { global_dir().join("config/models.yaml") }
pub fn security_yaml() -> PathBuf { global_dir().join("config/security.yaml") }
pub fn mcp_servers_yaml() -> PathBuf { global_dir().join("config/mcp_servers.yaml") }
pub fn model_preference_json() -> PathBuf { global_dir().join("config/model_preference.json") }
pub fn always_allow_json() -> PathBuf { global_dir().join("config/always_allow.json") }
pub fn policy_toml() -> PathBuf { global_dir().join("config/policy.toml") }
pub fn prompt_roles_toml() -> PathBuf { global_dir().join("config/prompt_roles.toml") }
pub fn subscenes_toml() -> PathBuf { global_dir().join("config/subscenes.toml") }
pub fn profile_json() -> PathBuf { global_dir().join("config/profile.json") }
pub fn conf_d_dir() -> PathBuf { global_dir().join("config/conf.d") }

// ─── palace/ 双宫殿记忆系统 ────────────────────────────────────────────

pub fn knowledge_db() -> PathBuf { global_dir().join("palace/knowledge.db") }
pub fn palace_db() -> PathBuf { global_dir().join("palace/palace.db") }
pub fn memory_db() -> PathBuf { global_dir().join("palace/memory.db") }
pub fn memory_embeddings_db() -> PathBuf { global_dir().join("palace/memory_embeddings.db") }
/// 全局 Markdown 记忆目录（`palace/md/`，用户可编辑的 markdown 文件）
pub fn palace_md_dir() -> PathBuf { global_dir().join("palace/md") }

// ─── data/ 运行时数据 ──────────────────────────────────────────────────

pub fn deduction_metrics_db() -> PathBuf { global_dir().join("data/deduction_metrics.db") }
pub fn task_logs_db() -> PathBuf { global_dir().join("data/task_logs.db") }
pub fn models_cache_json() -> PathBuf { global_dir().join("data/models.cache.json") }
pub fn history_jsonl() -> PathBuf { global_dir().join("data/history.jsonl") }
pub fn sessions_db() -> PathBuf { global_dir().join("data/sessions.db") }

// ─── projects/ 项目注册表 ──────────────────────────────────────────────

pub fn projects_dir() -> PathBuf { global_dir().join("projects") }
pub fn project_registry_json() -> PathBuf { global_dir().join("projects/registry.json") }

// ─── sessions/ 进程注册表 ──────────────────────────────────────────────

pub fn process_registry_dir() -> PathBuf { global_dir().join("sessions") }
pub fn current_process_file() -> PathBuf {
    process_registry_dir().join(format!("{}.json", std::process::id()))
}

// ─── meetings/ 会议 ───────────────────────────────────────────────────

pub fn meetings_dir() -> PathBuf { global_dir().join("meetings") }

// ─── skills/ Skill 定义 ───────────────────────────────────────────────

pub fn skills_dir() -> PathBuf { global_dir().join("skills") }

// ─── 工具进程 tmp（per-PID，避免多实例 plugin 互踩）────────────────────

pub fn mcp_plugin_tmp_dir() -> PathBuf {
    PathBuf::from(format!("/tmp/.abacus-{}/plugins", std::process::id()))
}

// ─── 向后兼容：检测旧路径文件是否存在 ──────────────────────────────────

/// 检测旧路径文件是否存在（平铺在 ~/.abacus/ 根下的文件）
/// 用于启动时自动迁移到新目录结构。
pub fn has_legacy_flat_files() -> bool {
    let dir = global_dir();
    // 检测标志：旧位置的 config.yaml 存在但新位置 config/config.yaml 不存在
    let old_config = dir.join("config.yaml");
    let new_config = dir.join("config/config.yaml");
    old_config.exists() && !new_config.exists()
}

/// 旧 → 新路径映射，供迁移逻辑使用
pub fn legacy_migration_pairs() -> Vec<(PathBuf, PathBuf)> {
    let dir = global_dir();
    vec![
        (dir.join("config.yaml"),              dir.join("config/config.yaml")),
        (dir.join("providers.yaml"),           dir.join("config/providers.yaml")),
        (dir.join("models.yaml"),              dir.join("config/models.yaml")),
        (dir.join("security.yaml"),            dir.join("config/security.yaml")),
        (dir.join("mcp_servers.yaml"),         dir.join("config/mcp_servers.yaml")),
        (dir.join("model_preference.json"),    dir.join("config/model_preference.json")),
        (dir.join("always_allow.json"),        dir.join("config/always_allow.json")),
        (dir.join("policy.toml"),              dir.join("config/policy.toml")),
        (dir.join("prompt_roles.toml"),        dir.join("config/prompt_roles.toml")),
        (dir.join("subscenes.toml"),           dir.join("config/subscenes.toml")),
        (dir.join("profile.json"),             dir.join("config/profile.json")),
        (dir.join("knowledge.db"),             dir.join("palace/knowledge.db")),
        (dir.join("palace.db"),                dir.join("palace/palace.db")),
        (dir.join("memory.db"),                dir.join("palace/memory.db")),
        (dir.join("memory_embeddings.db"),     dir.join("palace/memory_embeddings.db")),
        (dir.join("memory"),                   dir.join("palace/md")),
        (dir.join("deduction_metrics.db"),     dir.join("data/deduction_metrics.db")),
        (dir.join("task_logs.db"),             dir.join("data/task_logs.db")),
        (dir.join("models.cache.json"),        dir.join("data/models.cache.json")),
        (dir.join("history.jsonl"),            dir.join("data/history.jsonl")),
    ]
}

/// 执行旧 → 新路径迁移（搬文件 + 创建父目录）
/// 返回迁移的文件数
pub fn migrate_legacy_files() -> std::io::Result<usize> {
    let pairs = legacy_migration_pairs();
    let mut count = 0usize;
    for (old_path, new_path) in &pairs {
        if !old_path.exists() {
            continue;
        }
        // 新位置已有文件则跳过
        if new_path.exists() {
            continue;
        }
        // 创建父目录
        if let Some(parent) = new_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // 目录用 rename，文件用 rename
        std::fs::rename(old_path, new_path)?;
        count += 1;
    }
    Ok(count)
}

// ─── 项目层路径（项目目录下的本地文件） ─────────────────────────────────

/// 指定 cwd 的项目 session 目录（项目目录下的 .abacus/sessions）
pub fn project_sessions_dir(cwd: &Path) -> PathBuf {
    project_dir(cwd).join("sessions")
}

/// 当前 cwd 的项目 session 目录
pub fn current_sessions_dir() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    project_sessions_dir(&cwd)
}

/// 指定 cwd 的项目 logs 目录
pub fn project_logs_dir(cwd: &Path) -> PathBuf { project_dir(cwd).join("logs") }

/// 当前 cwd 的项目 logs 目录
pub fn current_logs_dir() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    project_logs_dir(&cwd)
}

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

/// 把 cwd 绝对路径转义为安全的目录名。
pub fn escape_cwd(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect()
}

/// 项目目录下的 abacusbr.md（全局文件不存在时的 fallback）
pub fn project_abacusbr_md(cwd: &Path) -> PathBuf { cwd.join("abacusbr.md") }

/// 项目目录下的 abacusbr.local.md（项目级覆盖，追加到全局之后）
pub fn project_abacusbr_local_md(cwd: &Path) -> PathBuf { cwd.join("abacusbr.local.md") }

// ─── 确保目录存在 ─────────────────────────────────────────────────────

/// 确保全局目录结构存在。在 abacus 启动早期调用。
pub fn ensure_global_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(global_dir())?;
    std::fs::create_dir_all(global_dir().join("config"))?;
    std::fs::create_dir_all(global_dir().join("config/conf.d"))?;
    std::fs::create_dir_all(global_dir().join("config/roles"))?;
    std::fs::create_dir_all(global_dir().join("palace/md"))?;
    std::fs::create_dir_all(global_dir().join("data"))?;
    std::fs::create_dir_all(global_dir().join("sessions"))?;
    std::fs::create_dir_all(global_dir().join("projects"))?;
    std::fs::create_dir_all(global_dir().join("meetings"))?;
    std::fs::create_dir_all(global_dir().join("skills"))?;
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

/// 确保进程注册表目录存在
pub fn ensure_process_registry_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(process_registry_dir())
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_abacus_home_env_var() {
        let original = std::env::var("ABACUS_HOME").ok();
        std::env::set_var("ABACUS_HOME", "/tmp/test-abacus-home");
        assert_eq!(global_dir(), PathBuf::from("/tmp/test-abacus-home"));
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

    #[test]
    fn test_paths_use_new_structure() {
        assert!(config_yaml().to_string_lossy().contains("config/config.yaml"));
        assert!(providers_yaml().to_string_lossy().contains("config/providers.yaml"));
        assert!(palace_db().to_string_lossy().contains("palace/palace.db"));
        assert!(knowledge_db().to_string_lossy().contains("palace/knowledge.db"));
        assert!(deduction_metrics_db().to_string_lossy().contains("data/deduction_metrics.db"));
        assert!(history_jsonl().to_string_lossy().contains("data/history.jsonl"));
        assert!(palace_md_dir().to_string_lossy().contains("palace/md"));
        assert!(abacusbr_md().to_string_lossy().ends_with("abacusbr.md"));
        assert!(!abacusbr_md().to_string_lossy().contains("/config/"));
    }
}