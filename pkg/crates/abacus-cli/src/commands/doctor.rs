use color_eyre::eyre::Result;
use crate::OutputFormatter;

/// V29.9 (C3): doctor 报告生成 — 纯函数, 不依赖 OutputFormatter
///
/// 引用关系:
///   - 调用方: `handle_doctor`(CLI 路径) + TUI `cmd_doctor`(slash_commands.rs)
///   - 输出: 行集合, 每行包含 [✓]/[○] 标识 + 描述
/// 生命周期: 一次性快照 — 当前状态读取(filesystem + env), 不缓存
/// 设计取舍:
///   - 抽函数而非 trait 抽象: 调用面只两个, 不值得引入新 trait
///   - 字符串而非结构体: TUI 只需要文本展示, CLI 也只是逐行打印
pub fn build_doctor_report() -> Vec<String> {
    let mut lines = Vec::with_capacity(8);
    lines.push("Abacus System Check".to_string());
    lines.push("─────────────────────────".to_string());

    let data_dir = abacus_core::paths::global_dir();

    // Config file
    let config_path = abacus_core::paths::config_yaml();
    lines.push(if config_path.exists() {
        format!("[✓] Config: {}", config_path.display())
    } else {
        format!("[○] Config: {} (not found)", config_path.display())
    });

    // Data directory
    lines.push(if data_dir.exists() {
        format!("[✓] Data dir: {}", data_dir.display())
    } else {
        format!("[○] Data dir: {} (will be created)", data_dir.display())
    });

    // Session database
    let db_path = abacus_core::paths::sessions_db();
    lines.push(if db_path.exists() {
        let size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
        format!("[✓] Session DB: {} ({} bytes)", db_path.display(), size)
    } else {
        "[○] Session DB: not yet created".to_string()
    });

    // Plugin directory
    let plugin_dir = abacus_core::paths::global_dir().join("plugins");
    lines.push(if plugin_dir.exists() {
        let count = std::fs::read_dir(&plugin_dir).map(|d| d.count()).unwrap_or(0);
        format!("[✓] Plugins: {} found", count)
    } else {
        "[○] Plugins: none installed".to_string()
    });

    // Environment
    let env_count = std::env::vars().filter(|(k, _)| k.starts_with("ABACUS_")).count();
    lines.push(format!("[✓] Env vars: {} ABACUS_* set", env_count));

    // Version
    lines.push(format!("[✓] Version: {}", env!("CARGO_PKG_VERSION")));
    lines.push("─────────────────────────".to_string());
    lines
}

pub async fn handle_doctor(formatter: &mut Box<dyn OutputFormatter>) -> Result<()> {
    for line in build_doctor_report() {
        formatter.format_message("doctor", &line, None);
    }
    formatter.format_done(0, None, None);
    Ok(())
}
