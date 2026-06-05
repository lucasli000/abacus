use color_eyre::eyre::Result;
use crate::OutputFormatter;

/// MCP servers 配置文件路径：~/.abacus/mcp_servers.toml
///
/// 每个 server 一个 [[servers]] table：
/// ```toml
/// [[servers]]
/// id = "..."
/// transport = "stdio"
/// command = "..."
/// args = ["-y", "@modelcontextprotocol/server-example"]
/// env = { KEY = "value" }
/// ```
fn mcp_config_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".abacus")
        .join("mcp_servers.toml")
}

fn read_mcp_root() -> Result<toml::Value> {
    let path = mcp_config_path();
    if !path.exists() {
        return Ok(toml::Value::Table(toml::map::Map::new()));
    }
    let content = std::fs::read_to_string(&path)?;
    Ok(toml::from_str(&content).unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new())))
}

fn write_mcp_root(root: &toml::Value) -> Result<()> {
    let path = mcp_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialized = toml::to_string_pretty(root)
        .map_err(|e| color_eyre::eyre::eyre!("TOML serialize error: {e}"))?;
    std::fs::write(&path, serialized)?;
    // 文件可能含 API key / token，强制 0o600（与 provider.toml / config.toml 一致）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(&path, perms);
        }
    }
    Ok(())
}

pub async fn handle_mcp(args: &super::McpArgs, formatter: &mut Box<dyn OutputFormatter>) -> Result<()> {
    match &args.action {
        super::McpAction::Add { server_id, command, env } => {
            let mut root = read_mcp_root()?;
            if !root.is_table() {
                root = toml::Value::Table(toml::map::Map::new());
            }
            let table = root.as_table_mut().unwrap();

            // 取出 / 新建 [[servers]] array
            let servers = table.entry("servers".to_string())
                .or_insert_with(|| toml::Value::Array(Vec::new()));
            if !servers.is_array() {
                *servers = toml::Value::Array(Vec::new());
            }
            let arr = servers.as_array_mut().unwrap();

            // 构造新 server entry
            let mut entry = toml::map::Map::new();
            entry.insert("id".into(), toml::Value::String(server_id.clone()));
            entry.insert("transport".into(), toml::Value::String("stdio".into()));
            if let Some(cmd) = command {
                entry.insert("command".into(), toml::Value::String(cmd.clone()));
            }
            if let Some(env_str) = env {
                // env 支持 "K=V" 或 "K=V,K2=V2" 形式（CLI 简写）
                let mut env_table = toml::map::Map::new();
                for pair in env_str.split(',') {
                    if let Some((k, v)) = pair.split_once('=') {
                        env_table.insert(k.trim().to_string(), toml::Value::String(v.trim().to_string()));
                    }
                }
                entry.insert("env".into(), toml::Value::Table(env_table));
            }
            arr.push(toml::Value::Table(entry));

            write_mcp_root(&root)?;
            formatter.format_message("mcp", &format!("[✓] MCP server '{}' added", server_id), None);
            formatter.format_message("mcp", &format!("   config saved to {}", mcp_config_path().display()), None);
        }
        super::McpAction::Remove { server_id } => {
            let mut root = read_mcp_root()?;
            if let Some(servers) = root.get_mut("servers").and_then(|v| v.as_array_mut()) {
                servers.retain(|entry| {
                    entry.as_table()
                        .and_then(|t| t.get("id"))
                        .and_then(|v| v.as_str())
                        .map(|id| id != server_id)
                        .unwrap_or(true)
                });
            }
            write_mcp_root(&root)?;
            formatter.format_message("mcp", &format!("[✓] MCP server '{}' removed", server_id), None);
        }
        super::McpAction::List => {
            let root = read_mcp_root()?;
            let servers: Vec<&toml::Value> = root.get("servers")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().collect())
                .unwrap_or_default();
            if servers.is_empty() {
                formatter.format_message("mcp", "Configured MCP Servers:", None);
                formatter.format_message("mcp", "  (no servers configured — add via `abacus mcp add` or edit ~/.abacus/mcp_servers.toml)", None);
            } else {
                formatter.format_message("mcp", "Configured MCP Servers:", None);
                for entry in &servers {
                    let id = entry.as_table()
                        .and_then(|t| t.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let transport = entry.as_table()
                        .and_then(|t| t.get("transport"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    formatter.format_message("mcp", &format!("  • {} (transport: {})", id, transport), None);
                }
            }
        }
        super::McpAction::Connect { server_id } => {
            use abacus_types::{McpConfig, ServerId};
            use abacus_core::mcp::McpClient;
            formatter.format_message("mcp", &format!("Connecting to '{}'...", server_id), None);
            let config = McpConfig {
                server_id: ServerId(server_id.clone()),
                transport: "stdio".into(),
                address: "".into(),
                tls: false,
                request_signing: false,
            };
            let client = McpClient::new(config);
            match client.connect().await {
                Ok(()) => formatter.format_message("mcp", &format!("[✓] Connected to '{}'", server_id), None),
                Err(e) => formatter.format_error("MCP_CONNECT", &format!("Failed: {}", e), None),
            }
        }
        super::McpAction::Discover { server_id } => {
            use abacus_types::{McpConfig, ServerId};
            use abacus_core::mcp::McpClient;
            formatter.format_message("mcp", &format!("Discovering tools from '{}'...", server_id), None);
            let config = McpConfig {
                server_id: ServerId(server_id.clone()),
                transport: "stdio".into(),
                address: "".into(),
                tls: false,
                request_signing: false,
            };
            let client = McpClient::new(config);
            let _ = client.connect().await;
            match client.discover_tools().await {
                Ok(tools) => {
                    formatter.format_message("mcp", &format!("Found {} tools:", tools.len()), None);
                    for tool in &tools {
                        formatter.format_message("mcp", &format!("  • {} — {}", tool.id.0, tool.schema.description), None);
                    }
                }
                Err(e) => formatter.format_error("MCP_DISCOVER", &format!("Failed: {}", e), None),
            }
        }
        super::McpAction::Status => {
            formatter.format_message("mcp", "MCP Status: no active connections", None);
        }
    }
    Ok(())
}
