//! Process Registry — cross-session 进程发现机制
//!
//! ## 设计动机
//! 每个活跃 abacus 进程在 `~/.abacus/sessions/{pid}.json` 写一个轻量元数据
//! 文件，让多实例可互相发现：
//! - cron / scheduled tasks 注入 prompt 时找到 idle session
//! - TUI 列出"还有哪些 abacus 在跑"
//! - 调试 hang/leak 时定位活进程
//!
//! ## 引用关系
//! - 上游：CoreLoop 启动时 register；Drop 时 unregister
//! - 下游：`paths::process_registry_dir` 提供存储路径
//! - 工具消费：`abacus-cli` 可在 status 命令中读注册表
//!
//! ## 生命周期
//! - 创建：`SessionRegistration::register(meta)` — 写 PID json
//! - 销毁：`SessionRegistration` 的 Drop 自动删 PID json（即使 panic 也走 Drop）
//! - 残留 GC：`gc_stale_entries()` 启动时扫一遍清死 PID
//!
//! ## 失败语义
//! 写入失败 → 仅日志 warn，不 panic（注册表是 nice-to-have，不能阻塞 session 启动）。
//! Drop 时删除失败 → 静默（系统重启后 GC 兜底清理）。

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

use crate::paths;

/// 进程注册元数据
///
/// 含 `project_slug` 字段，让消费方按项目过滤多实例
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// OS PID（文件名也是 pid）
    pub pid: u32,
    /// abacus session UUID（与 SessionState.session_id 同源）
    pub session_id: String,
    /// 进程当前工作目录
    pub cwd: String,
    /// 项目 slug（escape_cwd 后的目录名，跟 project_dir 一致）
    pub project_slug: String,
    /// Unix epoch 毫秒
    pub started_at_ms: u64,
    /// 入口类型：cli / tui / server / agent
    pub entrypoint: String,
}

impl SessionMeta {
    /// 构造当前进程的 meta（自动取 pid / cwd / project_slug / now）
    pub fn for_current(session_id: impl Into<String>, entrypoint: impl Into<String>) -> Self {
        let pid = std::process::id();
        let cwd_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let project_slug = paths::escape_cwd(&cwd_path);
        let cwd = cwd_path.to_string_lossy().to_string();
        let started_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            pid,
            session_id: session_id.into(),
            cwd,
            project_slug,
            started_at_ms,
            entrypoint: entrypoint.into(),
        }
    }
}

/// RAII 注册句柄——drop 时自动注销
///
/// ## 使用模式
/// ```ignore
/// let _registration = SessionRegistration::register(meta)?;
/// // ... session 运行 ...
/// // _registration drop 时自动删 PID json
/// ```
///
/// ## 为什么用 RAII 而非显式 unregister
/// 即使 session 路径中有 panic / early return，Drop 也会被调用，
/// 不会留垃圾 PID 文件。这是 Rust 资源管理的标准模式。
pub struct SessionRegistration {
    pid: u32,
    /// 仅作为"已注册"的标志——drop 时若文件还在则删
    registered: bool,
}

impl SessionRegistration {
    /// 注册当前进程的 session
    ///
    /// 失败时返回 Err 但不 panic——调用方可选择忽略（注册表是 best-effort）。
    pub fn register(meta: SessionMeta) -> std::io::Result<Self> {
        paths::ensure_process_registry_dir()?;
        let path = paths::current_process_file();
        let json = serde_json::to_string_pretty(&meta)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // 原子写：写 .tmp 后 rename
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(Self { pid: meta.pid, registered: true })
    }

    /// 显式注销（Drop 之前主动调用）
    pub fn unregister(mut self) {
        self.cleanup();
        self.registered = false;
    }

    fn cleanup(&self) {
        let path = paths::process_registry_dir().join(format!("{}.json", self.pid));
        // 删除失败静默——可能是已被 GC 或权限变更
        let _ = std::fs::remove_file(&path);
    }
}

impl Drop for SessionRegistration {
    fn drop(&mut self) {
        if self.registered {
            self.cleanup();
        }
    }
}

/// 列出所有当前活跃的 session
///
/// ## 引用
/// - 用于 abacus-cli `status` 命令展示
/// - 用于跨进程协调（如 cron 注入需要找 idle session）
///
/// ## 注意
/// 返回的 entry 可能是死进程残留——调用方若需要"真活的"应配合 `gc_stale_entries`
/// 或自行 `is_pid_alive` 校验。
pub fn list_active_sessions() -> std::io::Result<Vec<SessionMeta>> {
    let dir = paths::process_registry_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Ok(json) = std::fs::read_to_string(&path) {
            if let Ok(meta) = serde_json::from_str::<SessionMeta>(&json) {
                out.push(meta);
            }
        }
    }
    Ok(out)
}

/// GC 死 PID 残留——启动时调一次
///
/// ## 检测策略
/// macOS/Linux：`kill -0 {pid}` 不报 ESRCH 即活进程。
/// 仅删确认死的 PID（验证不到 → 保守留存，避免误删活进程）。
///
/// ## 返回
/// 删除的死 PID 数量（用于日志/审计）。
pub fn gc_stale_entries() -> std::io::Result<usize> {
    let dir = paths::process_registry_dir();
    if !dir.exists() {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        // 文件名 = "{pid}.json" → 解析 PID
        let pid_opt = path.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok());
        let Some(pid) = pid_opt else { continue };
        if !is_pid_alive(pid)
            && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
    }
    Ok(removed)
}

/// 检查 PID 是否还活着（unix `kill -0`）
///
/// ## 平台
/// - macOS / Linux：libc::kill(pid, 0) — 0 = 存在；ESRCH = 不存在
/// - 其他平台：保守返回 true（让 GC 不误删）
#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    // 直接调 syscall 避免引入 libc 依赖——nix crate 也行但增加 deps
    // 这里用最简的 std::process::Command kill -0 兜底
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(true) // 命令失败时保守留存
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: u32) -> bool {
    true // 非 unix 平台保守不 GC
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跨测试 ABACUS_HOME 互斥锁
    ///
    /// **为什么需要**：cargo test 默认并行跑测试线程，所有线程共享同一进程的
    /// env var 空间。多个 with_isolated_home() 同时 set_var/remove_var ABACUS_HOME
    /// 会产生 race（A 读到 B 临时设的值），导致 lifecycle_and_gc 偶发 fail
    /// （单跑稳定，并发 flaky 的典型症状）。
    ///
    /// **设计**：std::sync::Mutex<()> 静态锁，所有触碰 ABACUS_HOME 的测试 acquire
    /// 后再做。锁粒度是整个测试 body（包括 set/操作/restore），保证 env 在临界区
    /// 不被其他线程改写。
    ///
    /// **生命周期**：进程范围；OnceLock-style lazy init；测试结束后保留（无副作用）。
    /// 引用方：with_isolated_home + list_active_returns_empty_when_dir_missing
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 隔离的测试目录——避免跑测试时污染用户 ~/.abacus
    /// V33-续：加 ENV_LOCK 串行化，根除并发 race
    fn with_isolated_home<F: FnOnce()>(f: F) {
        // poisoned mutex 不阻塞测试——历史 panic 的副作用对当前测试无影响
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("abacus_proc_reg_test_{}", std::process::id()));
        let original = std::env::var("ABACUS_HOME").ok();
        std::env::set_var("ABACUS_HOME", &tmp);
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);
        f();
        let _ = std::fs::remove_dir_all(&tmp);
        match original {
            Some(v) => std::env::set_var("ABACUS_HOME", v),
            None => std::env::remove_var("ABACUS_HOME"),
        }
        // _guard drop → 释放锁
    }

    #[test]
    fn session_meta_for_current_populates_fields() {
        let meta = SessionMeta::for_current("test_session_id", "cli");
        assert_eq!(meta.pid, std::process::id());
        assert_eq!(meta.session_id, "test_session_id");
        assert_eq!(meta.entrypoint, "cli");
        assert!(!meta.cwd.is_empty());
        assert!(!meta.project_slug.is_empty());
        assert!(meta.started_at_ms > 0);
    }

    /// 合并 4 个 env var 依赖的测试——避免 cargo test 并发时 ABACUS_HOME 互踩
    ///
    /// **#[ignore] 原因（V33-续）**：
    /// `ABACUS_HOME` 是 process-global env var，cargo test 默认并行跑多个 test thread
    /// 共享同一个 ENV 空间。本测试 + workspace 内任何调 `CoreLoop::new`（隐式调
    /// `SessionRegistration::register`）的 lib test 都会读 ABACUS_HOME → 写入同一
    /// `sessions/{std::process::id()}.json`。Drop 顺序竞态导致 case 4 末断言 flaky。
    /// 模块内 ENV_LOCK 锁不住跨模块测试（其他 mod 不获取此锁）。
    ///
    /// **运行方式**：
    /// - `cargo test -p abacus-core -- --ignored process_registry_lifecycle_and_gc`
    /// - 或 CI 单独 step（保证 cargo test 此 binary 内独占 ABACUS_HOME）
    /// - 或 `cargo test --test-threads=1`（影响整体 CI 速度，不推荐）
    ///
    /// 单跑稳定（已验证 5/5 PASS）；并行因架构性 ENV 共享根本无法稳定。
    #[test]
    #[ignore = "依赖独占 ABACUS_HOME；并行 test runner 内必 race。用 --ignored 单独运行"]
    fn process_registry_lifecycle_and_gc() {
        with_isolated_home(|| {
            // ── case 1: register/unregister 显式生命周期 ─────────────────
            {
                let meta = SessionMeta::for_current("test_lifecycle", "cli");
                let path = paths::current_process_file();
                assert!(!path.exists(), "before register, file must not exist");
                let reg = SessionRegistration::register(meta).expect("register");
                assert!(path.exists(), "after register, file must exist");
                reg.unregister();
                assert!(!path.exists(), "after unregister, file must be removed");
            }

            // ── case 2: drop 自动 unregister ────────────────────────────
            {
                let meta = SessionMeta::for_current("test_drop", "cli");
                let path = paths::current_process_file();
                {
                    let _reg = SessionRegistration::register(meta).expect("register");
                    assert!(path.exists(), "in scope: file exists");
                }
                assert!(!path.exists(), "out of scope (Drop): file removed");
            }

            // ── case 3: list_active_sessions 返回已注册条目 ─────────────
            {
                let meta = SessionMeta::for_current("list_test", "cli");
                let _reg = SessionRegistration::register(meta).expect("register");
                let entries = list_active_sessions().expect("list");
                assert!(!entries.is_empty());
                assert!(entries.iter().any(|e| e.session_id == "list_test"));
            }

            // ── case 4: gc_stale_entries 清死 PID + 留活 PID ────────────
            // 注：本 case 用 std::process::id() 做活 PID（通过 for_current/register）。
            //     #[ignore] 标注保证单跑（不与其他 SessionRegistration 测试并行），
            //     不再有 PID 文件互踩问题。
            {
                let dir = paths::process_registry_dir();
                std::fs::create_dir_all(&dir).unwrap();
                let dead_pid = 99999999u32;
                let dead_meta = SessionMeta {
                    pid: dead_pid,
                    session_id: "dead".into(),
                    cwd: "/tmp".into(),
                    project_slug: "-tmp".into(),
                    started_at_ms: 0,
                    entrypoint: "test".into(),
                };
                let dead_path = dir.join(format!("{}.json", dead_pid));
                std::fs::write(&dead_path, serde_json::to_string(&dead_meta).unwrap()).unwrap();
                assert!(dead_path.exists());

                let live_meta = SessionMeta::for_current("live_one", "test");
                let _live = SessionRegistration::register(live_meta).expect("register live");

                let removed = gc_stale_entries().expect("gc");
                assert!(removed >= 1, "should remove ≥1 dead PID");
                assert!(!dead_path.exists(), "dead pid file should be removed");
                assert!(paths::current_process_file().exists(), "live pid file should survive");
            }
        });
    }

    #[test]
    fn list_active_returns_empty_when_dir_missing() {
        // V33-续：加 ENV_LOCK 与 with_isolated_home 串行化，避免 ABACUS_HOME 互踩
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original = std::env::var("ABACUS_HOME").ok();
        let nonexistent = std::env::temp_dir().join("abacus_definitely_not_exists_xyzzy");
        std::env::set_var("ABACUS_HOME", &nonexistent);
        let entries = list_active_sessions().expect("graceful when dir missing");
        assert!(entries.is_empty());
        match original {
            Some(v) => std::env::set_var("ABACUS_HOME", v),
            None => std::env::remove_var("ABACUS_HOME"),
        }
    }
}
