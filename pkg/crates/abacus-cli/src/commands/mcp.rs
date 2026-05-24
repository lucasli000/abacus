use color_eyre::eyre::Result;
use crate::OutputFormatter;

pub async fn handle_mcp(args: &super::McpArgs, formatter: &mut Box<dyn OutputFormatter>) -> Result<()> {
    match &args.action {
        super::McpAction::Add { server_id, command, env } => {
            let config_path = dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".abacus")
                .join("mcp_servers.yaml");
            // 防御性：parent() 在根路径下为 None，回退到当前目录
            if let Some(parent) = config_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }

            let mut content = String::new();
            if config_path.exists() {
                content = std::fs::read_to_string(&config_path)?;
            }
            content.push_str(&format!("\n{}:\n  command: {}\n", server_id,
                command.as_deref().unwrap_or("")));
            if let Some(env_str) = env {
                content.push_str(&format!("  env: {}\n", env_str));
            }
            std::fs::write(&config_path, content)?;
            formatter.format_message("mcp", &format!("[✓] MCP server '{}' added", server_id), None);
            formatter.format_message("mcp", &format!("   config saved to {}", config_path.display()), None);
        }
        super::McpAction::Remove { server_id } => {
            let config_path = dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".abacus")
                .join("mcp_servers.yaml");
            if config_path.exists() {
                let content = std::fs::read_to_string(&config_path)?;
                let filtered: Vec<&str> = content.lines()
                    .filter(|l| !l.starts_with(&format!("{}:", server_id)))
                    .collect();
                std::fs::write(&config_path, filtered.join("\n"))?;
            }
            formatter.format_message("mcp", &format!("[✓] MCP server '{}' removed", server_id), None);
        }
        super::McpAction::List => {
            formatter.format_message("mcp", "Configured MCP Servers:", None);
            formatter.format_message("mcp", "  (no servers configured — add to ~/.abacus/config.yaml)", None);
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
