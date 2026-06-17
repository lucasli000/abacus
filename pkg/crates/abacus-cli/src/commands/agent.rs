//! Agent CLI 命令 — 外部 Agent 管理

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use crate::output::OutputFormatter;

#[derive(Debug, Parser)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub action: AgentAction,
}

#[derive(Debug, Subcommand)]
pub enum AgentAction {
    /// 安装外部 Agent
    Install {
        /// Agent 来源（npm 包名 / git 仓库 / 本地目录 / MCP 端点）
        source: String,
        /// 信任级别覆盖
        #[arg(long, default_value = "standard")]
        trust: String,
    },
    /// 列出已安装 Agent
    List,
    /// 卸载 Agent
    Remove {
        /// Agent ID
        agent_id: String,
    },
    /// 查看 Agent 详情
    Info {
        /// Agent ID
        agent_id: String,
    },
    /// 健康状态检查
    Health,
    /// 启用 Agent
    Enable {
        /// Agent ID
        agent_id: String,
    },
    /// 禁用 Agent
    Disable {
        /// Agent ID
        agent_id: String,
    },
}

/// Agent 配置条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigEntry {
    pub id: String,
    pub source: String,
    pub version: String,
    pub installed_at: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub trust_override: Option<String>,
}

fn default_true() -> bool { true }

pub async fn handle_agent(
    action: &AgentAction,
    formatter: &mut Box<dyn OutputFormatter>,
) -> color_eyre::eyre::Result<()> {
    match action {
        AgentAction::Install { source, trust } => handle_install(source, trust, formatter),
        AgentAction::List => handle_list(formatter),
        AgentAction::Remove { agent_id } => handle_remove(agent_id, formatter),
        AgentAction::Info { agent_id } => handle_info(agent_id, formatter),
        AgentAction::Health => handle_health(formatter),
        AgentAction::Enable { agent_id } => handle_enable(agent_id, true, formatter),
        AgentAction::Disable { agent_id } => handle_enable(agent_id, false, formatter),
    }
}

fn handle_install(source: &str, trust: &str, f: &mut Box<dyn OutputFormatter>) -> color_eyre::eyre::Result<()> {
    let (agent_id, _endpoint, transport_type) = if source.starts_with("mcp://") {
        let id = source.split('/').last().unwrap_or("unknown");
        (id.to_string(), source.to_string(), "mcp")
    } else if source.starts_with("http://") || source.starts_with("https://") {
        let id = source.split('/').last().unwrap_or("unknown");
        (id.to_string(), source.to_string(), "http")
    } else if source.starts_with('@') || !source.contains('/') {
        let id = source.split('/').last().unwrap_or(source);
        let id = id.trim_start_matches('@');
        (id.to_string(), format!("npx -y {}", source), "mcp")
    } else {
        let id = std::path::Path::new(source)
            .file_name().and_then(|n| n.to_str()).unwrap_or("local-agent");
        (id.to_string(), source.to_string(), "mcp")
    };

    let config_path = agents_config_path();
    let mut entries = load_agents_config(&config_path);

    if entries.iter().any(|e| e.id == agent_id) {
        f.format_error("agent", &format!("Agent '{}' is already installed", agent_id), None);
        return Ok(());
    }

    entries.push(AgentConfigEntry {
        id: agent_id.clone(),
        source: source.to_string(),
        version: "0.0.0".to_string(),
        installed_at: chrono::Utc::now().to_rfc3339(),
        enabled: true,
        trust_override: Some(trust.to_string()),
    });

    save_agents_config(&config_path, &entries)?;
    f.format_message("system", &format!(
        "Agent '{}' installed (transport: {}, trust: {})", agent_id, transport_type, trust
    ), None);
    Ok(())
}

fn handle_list(f: &mut Box<dyn OutputFormatter>) -> color_eyre::eyre::Result<()> {
    let entries = load_agents_config(&agents_config_path());

    if entries.is_empty() {
        f.format_message("system", "No agents installed. Use 'abacus agent install <source>'.", None);
        return Ok(());
    }

    let mut lines = format!("Installed agents ({}):\n", entries.len());
    for e in &entries {
        let status = if e.enabled { "✓" } else { "✗" };
        let trust = e.trust_override.as_deref().unwrap_or("standard");
        lines.push_str(&format!("  {} {:<20} v{:<8} trust:{:<10} src:{}\n",
            status, e.id, e.version, trust, e.source));
    }
    f.format_message("system", &lines, None);
    Ok(())
}

fn handle_remove(agent_id: &str, f: &mut Box<dyn OutputFormatter>) -> color_eyre::eyre::Result<()> {
    let config_path = agents_config_path();
    let mut entries = load_agents_config(&config_path);
    let before = entries.len();
    entries.retain(|e| e.id != agent_id);

    if entries.len() == before {
        f.format_error("agent", &format!("Agent '{}' not found", agent_id), None);
        return Ok(());
    }

    save_agents_config(&config_path, &entries)?;
    f.format_message("system", &format!("Agent '{}' removed", agent_id), None);
    Ok(())
}

fn handle_info(agent_id: &str, f: &mut Box<dyn OutputFormatter>) -> color_eyre::eyre::Result<()> {
    let entries = load_agents_config(&agents_config_path());
    match entries.iter().find(|e| e.id == agent_id) {
        Some(e) => {
            let info = format!(
                "Agent: {}\n  Source: {}\n  Version: {}\n  Installed: {}\n  Enabled: {}\n  Trust: {}",
                e.id, e.source, e.version, e.installed_at, e.enabled,
                e.trust_override.as_deref().unwrap_or("standard")
            );
            f.format_message("system", &info, None);
        }
        None => f.format_error("agent", &format!("Agent '{}' not found", agent_id), None),
    }
    Ok(())
}

fn handle_health(f: &mut Box<dyn OutputFormatter>) -> color_eyre::eyre::Result<()> {
    let entries = load_agents_config(&agents_config_path());
    if entries.is_empty() {
        f.format_message("system", "No agents installed.", None);
        return Ok(());
    }
    let mut lines = String::from("Agent health:\n");
    for e in &entries {
        let status = if e.enabled { "○ unknown" } else { "✗ disabled" };
        lines.push_str(&format!("  {} {:<20} {}\n", status, e.id, e.source));
    }
    f.format_message("system", &lines, None);
    Ok(())
}

fn handle_enable(agent_id: &str, enabled: bool, f: &mut Box<dyn OutputFormatter>) -> color_eyre::eyre::Result<()> {
    let config_path = agents_config_path();
    let mut entries = load_agents_config(&config_path);
    match entries.iter_mut().find(|e| e.id == agent_id) {
        Some(e) => {
            e.enabled = enabled;
            save_agents_config(&config_path, &entries)?;
            let state = if enabled { "enabled" } else { "disabled" };
            f.format_message("system", &format!("Agent '{}' {}", agent_id, state), None);
        }
        None => f.format_error("agent", &format!("Agent '{}' not found", agent_id), None),
    }
    Ok(())
}

// ─── 配置文件操作 ───

fn agents_config_path() -> std::path::PathBuf {
    dirs::home_dir().unwrap_or_default().join(".abacus").join("agents.toml")
}

fn load_agents_config(path: &std::path::Path) -> Vec<AgentConfigEntry> {
    if !path.exists() { return Vec::new(); }
    let content = match std::fs::read_to_string(path) { Ok(c) => c, Err(_) => return Vec::new() };
    let value: toml::Value = match toml::from_str(&content) { Ok(v) => v, Err(_) => return Vec::new() };
    value.get("agents").and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.clone().try_into::<AgentConfigEntry>().ok()).collect())
        .unwrap_or_default()
}

fn save_agents_config(path: &std::path::Path, entries: &[AgentConfigEntry]) -> color_eyre::eyre::Result<()> {
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }

    #[derive(Serialize)]
    struct AgentsConfig {
        agents: Vec<AgentConfigEntry>,
    }

    let config = AgentsConfig { agents: entries.to_vec() };
    let content = toml::to_string_pretty(&config)?;
    std::fs::write(path, content)?;

    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)); }
    Ok(())
}
