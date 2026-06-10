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
use thiserror::Error;

/// 路径解析错误（治本：替换 `unwrap_or_else(PathBuf::from("/tmp"))` 静默 fallback）
///
/// ## 为什么是显式错误而不是 silent fallback
///
/// 旧代码 `dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))` 有三个真实 bug：
/// 1. **Windows 错**：`/tmp` 在 Windows 上是相对路径，不是有效的 FS 根
/// 2. **多用户错**：`/tmp` 全局可写，symlink 攻击可让非特权用户影响 root 用户的 abacus
/// 3. **真空路径**：如果 `$HOME` 真没设，**应该让用户知道**，而不是装作一切正常
///
/// `try_global_dir()` 强制调用方**显式决策**：报错 / 提示 / 用临时目录。
#[derive(Debug, Error)]
pub enum PathsError {
    #[error("cannot resolve home directory: $HOME and $USERPROFILE both unset")]
    HomeUnavailable,
    #[error("ABACUS_HOME is set but empty")]
    AbacusHomeEmpty,
}

/// 全局根目录（Result 形式，治本路径）
///
/// 优先 `ABACUS_HOME` env，否则 `$HOME/.abacus` / `$USERPROFILE/.abacus` (Windows)。
///
/// ## 调用方必须显式处理错误
/// - TUI 启动 → toast 提示用户设 `ABACUS_HOME` 或修 home 目录
/// - CLI 命令 → 返回 `Err`，用户看到错误退出非 0
/// - Library 内部调用 → `try_global_dir().unwrap_or_else(|_| temp_dir_fallback())`
///   这种用法允许，但调用方应记 `tracing::warn!`
pub fn try_global_dir() -> Result<PathBuf, PathsError> {
    // 优先 ABACUS_HOME
    if let Ok(v) = std::env::var("ABACUS_HOME") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
        return Err(PathsError::AbacusHomeEmpty);
    }
    // 标准路径：$HOME (Linux/macOS) / $USERPROFILE (Windows)
    if let Some(home) = dirs::home_dir() {
        return Ok(home.join(".abacus"));
    }
    Err(PathsError::HomeUnavailable)
}

/// 全局根目录（fallback 形式，向后兼容）
///
/// ## 行为
/// - 正常情况：返回 `try_global_dir()` 的结果
/// - 错误情况：
///   - **生产 build (release)**：fallback 到 `<temp_dir>/.abacus`，**记 `tracing::warn!`**
///   - **debug build**：`debug_assert!` 让开发者立即看到 bug
///
/// ## 为什么不 panic
/// 旧代码会 fallback `/tmp`（bug），新代码用 `<temp_dir>/.abacus`（更安全——
/// std::env::temp_dir() 在 Windows 上是 `%TEMP%`、macOS 是 `$TMPDIR` per-user）。
pub fn global_dir() -> PathBuf {
    match try_global_dir() {
        Ok(p) => p,
        Err(e) => {
            debug_assert!(false, "global_dir fallback triggered: {e}");
            tracing::warn!(
                "abacus: cannot resolve home directory ({e}); using temporary path. \
                 Set $ABACUS_HOME or fix $HOME/$USERPROFILE to silence this warning."
            );
            // 治本：temp_dir 替代 /tmp（Windows 兼容 + per-user 隔离）
            std::env::temp_dir().join(".abacus")
        }
    }
}

// ─── 全局配置 ─────────────────────────────────────────────────────────────

/// 全局行为配置文件（TOML 格式）
pub fn config_toml() -> PathBuf { global_dir().join("config.toml") }
/// 供应商配置文件：所有 provider + 模型参数（TOML 格式）
pub fn provider_toml() -> PathBuf { global_dir().join("provider.toml") }
/// 模型能力 catalog 覆盖文件（TOML 格式，合并到内置 spec）
pub fn models_toml() -> PathBuf { global_dir().join("models.toml") }
/// 安全 / MCIP 权限配置文件（TOML 格式）
pub fn security_toml() -> PathBuf { global_dir().join("security.toml") }
/// 行为规范文件（Markdown 格式，Layer 230 注入 system prompt）
pub fn abacusbr_md() -> PathBuf { global_dir().join("abacusbr.md") }
/// conf.d 目录：用户可放入 *.toml / *.yaml 扩展片段（按文件名排序合并）
pub fn conf_d_dir() -> PathBuf { global_dir().join("conf.d") }

// ─── 全局数据 ─────────────────────────────────────────────────────────────

/// 全局数据目录（数据库文件统一存放）
pub fn data_dir() -> PathBuf { global_dir().join("data") }
/// 知识库数据库（FTS5 全文检索 + 语义搜索）
pub fn knowledge_db() -> PathBuf { data_dir().join("knowledge.db") }
/// 记忆宫殿数据库（行为宫 + 知识宫 + 记忆桥）
pub fn palace_db() -> PathBuf { data_dir().join("palace.db") }
/// 通用记忆数据库
pub fn memory_db() -> PathBuf { data_dir().join("memory.db") }
/// 推演指标数据库
pub fn deduction_metrics_db() -> PathBuf { data_dir().join("deduction_metrics.db") }
/// 任务日志数据库
pub fn task_logs_db() -> PathBuf { data_dir().join("task_logs.db") }
/// 会话数据库
pub fn sessions_db() -> PathBuf { data_dir().join("sessions.db") }
/// 历史记录（JSONL 格式）
pub fn history_jsonl() -> PathBuf { data_dir().join("history.jsonl") }

// ─── 全局资源 ─────────────────────────────────────────────────────────────

/// 全局记忆目录（Markdown 文件，用户可手动添加）
pub fn global_memory_dir() -> PathBuf { global_dir().join("memory") }
/// 用户技能目录（YAML 文件）
pub fn skills_dir() -> PathBuf { global_dir().join("skills") }
/// 会议记录目录
pub fn meetings_dir() -> PathBuf { global_dir().join("meetings") }
/// 项目目录（所有项目都在此下）
pub fn projects_dir() -> PathBuf { global_dir().join("projects") }

// ─── 项目层 ─────────────────────────────────────────────────────────────

/// 指定 cwd 的项目目录。转义规则：路径分隔符替换为 `-`
///
/// 例：`/path/to/project` → `~/.abacus/projects/-path-to-project/`
pub fn project_dir(cwd: &Path) -> PathBuf {
    projects_dir().join(escape_cwd(cwd))
}

/// 当前 cwd 的项目目录（fallback 形式）
///
/// ## 行为
/// - 正常情况：返回 `try_current_project_dir()` 的结果
/// - 错误情况：cwd 不可读时 fallback 到 **global_dir 根**（而非 `PathBuf::from("/")`——
///   旧版会 vacuum 整个 FS 根，是严重 bug）
pub fn current_project_dir() -> PathBuf {
    match try_current_project_dir() {
        Ok(p) => p,
        Err(e) => {
            debug_assert!(false, "current_project_dir fallback: {e}");
            tracing::warn!(
                "abacus: cannot resolve cwd ({e}); using global dir as project root fallback. \
                 This may cause session data to leak across projects."
            );
            // 治本：用 global_dir 根而非 "/" — 防止真空路径 + 防止跨盘 symlink 攻击
            global_dir()
        }
    }
}

/// 当前 cwd 的项目目录（Result 形式，治本路径）
#[derive(Debug, Error)]
pub enum CwdError {
    #[error("cannot read current working directory: {0}")]
    Io(#[from] std::io::Error),
}

pub fn try_current_project_dir() -> Result<PathBuf, CwdError> {
    let cwd = std::env::current_dir()?;
    Ok(project_dir(&cwd))
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
/// 确保全局目录结构存在
///
/// 目录结构：
/// ```text
/// ~/.abacus/
/// ├── config.toml        # 核心配置
/// ├── provider.toml      # 供应商配置
/// ├── models.toml        # 模型能力覆盖
/// ├── security.toml      # 安全配置
/// ├── abacusbr.md        # 行为规范
/// ├── conf.d/            # 扩展配置
/// ├── data/              # 数据库文件
/// │   ├── knowledge.db
/// │   ├── palace.db
/// │   ├── memory.db
/// │   ├── sessions.db
/// │   └── ...
/// ├── memory/            # 全局记忆
/// ├── skills/            # 用户技能
/// ├── meetings/          # 会议记录
/// └── projects/          # 项目目录
/// ```
pub fn ensure_global_dirs() -> std::io::Result<()> {
    // 根目录
    std::fs::create_dir_all(global_dir())?;
    // 配置相关
    std::fs::create_dir_all(conf_d_dir())?;
    // 数据目录（数据库统一存放）
    std::fs::create_dir_all(data_dir())?;
    // 资源目录
    std::fs::create_dir_all(global_memory_dir())?;
    std::fs::create_dir_all(skills_dir())?;
    std::fs::create_dir_all(meetings_dir())?;
    std::fs::create_dir_all(projects_dir())?;
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
// 🟡#2 治本：见 config.rs 同注释
#[allow(unsafe_code)]
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
