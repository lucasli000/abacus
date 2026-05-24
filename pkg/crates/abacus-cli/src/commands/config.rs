use color_eyre::eyre::Result;
use crate::OutputFormatter;
use super::ConfigAction;

pub async fn handle_config(args: &super::ConfigArgs, formatter: &mut Box<dyn OutputFormatter>) -> Result<()> {
    let config_path = dirs::home_dir()
        .map(|h| h.join(".abacus/config.yaml"))
        .unwrap_or_else(|| std::path::PathBuf::from(".abacus/config.yaml"));

    match &args.action {
        ConfigAction::ListKeys => {
            let keys = [
                ("llm.api_key",           "API 密钥（DeepSeek/OpenAI 等，必配项）"),
                ("llm.base_url",          "API 服务地址（不改默认即可）"),
                ("llm.anthropic_api_key", "Anthropic 协议的 API 密钥"),
                ("llm.openai_api_key",    "OpenAI 兼容接口的 API 密钥"),
                ("core.default_model",    "对话使用的模型（如 deepseek-v4-flash）"),
                ("core.system_prompt",    "AI 助理的自定义角色设定语"),
                ("core.temperature",      "回答的随机创意程度，0 = 严谨保守，1 = 天马行空"),
                ("core.max_tokens",       "单次回复最长长度"),
                ("core.thinking",          "[PRIMARY] 思考意图：off / adaptive / minimal / low / medium / high / max / xhigh / <整数 budget>"),
                ("core.thinking_display",  "[PRIMARY] Anthropic adaptive 显示：summarized / omitted"),
                ("core.thinking_enabled",  "[DEPRECATED] 改用 core.thinking。是否让模型先思考再回答（true/false）"),
                ("core.thinking_effort",   "[DEPRECATED] 改用 core.thinking。思考深度：低(off) 中(low) 高(medium) 极高(high)"),
                ("core.context_window",   "记忆对话的总量上限"),
                ("core.max_turns",        "单次任务最多来回对话轮数"),
                ("core.max_tool_calls",   "单次回复最多执行工具次数"),
                ("server.max_sessions",   "服务器最大同时连接数"),
            ];
            formatter.format_message("config", "Available configuration keys:", None);
            for (key, desc) in &keys {
                formatter.format_message("config", &format!("  {:<26} {}", key, desc), None);
            }
            formatter.format_message("config", "\nSet via: abacus config set <key> <value>", None);
            formatter.format_message("config", "Override via env: ABACUS_<UPPER_KEY>", None);
        }
        ConfigAction::Show => {
            if config_path.exists() {
                let content = std::fs::read_to_string(&config_path)?;
                formatter.format_message("config", &format!("# {}\n{}", config_path.display(), content), None);
            } else {
                formatter.format_message("config", &format!("No config found at {}", config_path.display()), None);
                formatter.format_message("config", "Run `abacus config edit` to create one.", None);
            }
            let env_vars: Vec<_> = std::env::vars()
                .filter(|(k, _)| k.starts_with("ABACUS_"))
                .collect();
            if !env_vars.is_empty() {
                formatter.format_message("config", "\n# Environment overrides:", None);
                for (k, v) in &env_vars {
                    let display_val = if k == "ABACUS_API_KEY" || k == "ABACUS_SERVER_TOKEN" {
                        if v.is_empty() { "(not set)".into() } else { "[REDACTED]".into() }
                    } else {
                        v.clone()
                    };
                    formatter.format_message("config", &format!("  {} = {}", k, display_val), None);
                }
            }
        }
        ConfigAction::Set { key, value } => {
            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut content = if config_path.exists() {
                std::fs::read_to_string(&config_path)?
            } else {
                "# Abacus Configuration\n".into()
            };
            content.push_str(&format!("{}: {}\n", key, value));
            std::fs::write(&config_path, &content)?;
            formatter.format_message("config", &format!("[✓] Set {} = {}", key, value), None);
        }
        ConfigAction::Edit => {
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".into());
            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if !config_path.exists() {
                // V29.13: max_turns 默认 25, 同步 abacus-core/src/config.rs default_config()
                std::fs::write(&config_path, "# Abacus Configuration\ndefault_model: deepseek-v4-flash\nmax_turns: 25\ntemperature: 0.6\n")?;
            }
            formatter.format_message("config", &format!("Opening with {}...", editor), None);
            std::process::Command::new(&editor).arg(&config_path).status()?;
        }
        ConfigAction::Validate => {
            if !config_path.exists() {
                formatter.format_error("CONFIG_MISSING", "No config file found", None);
                return Ok(());
            }
            let content = std::fs::read_to_string(&config_path)?;
            match serde_yaml::from_str::<serde_yaml::Value>(&content) {
                Ok(_) => formatter.format_message("config", "[✓] Configuration valid", None),
                Err(e) => formatter.format_error("CONFIG_INVALID", &format!("YAML error: {}", e), None),
            }
        }
    }
    Ok(())
}
