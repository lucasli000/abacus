use color_eyre::eyre::Result;
use crate::OutputFormatter;
use crate::engine_init;
use super::ModelAction;
use std::path::PathBuf;

/// 把 available_models 列表写入 ~/.abacus/config.yaml 的顶层 `available_models` 字段
///
/// 行为：
/// - 不存在 config.yaml → 创建（仅含 available_models）
/// - 已存在 → 解析为 yaml::Value，更新/插入 available_models 字段，atomic 写回
/// - 解析失败 → 备份原文件为 .bak，写入新内容
fn write_available_models_to_config(models: &[String]) -> Result<PathBuf> {
    let cfg_path = abacus_core::paths::config_yaml();

    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut root: serde_yaml::Value = if cfg_path.exists() {
        let content = std::fs::read_to_string(&cfg_path)?;
        match serde_yaml::from_str(&content) {
            Ok(v) => v,
            Err(_) => {
                // 解析失败：备份后从空开始
                let bak = cfg_path.with_extension("yaml.bak");
                let _ = std::fs::copy(&cfg_path, &bak);
                serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
            }
        }
    } else {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    };

    if let serde_yaml::Value::Mapping(ref mut map) = root {
        let yaml_models: Vec<serde_yaml::Value> = models.iter()
            .map(|m| serde_yaml::Value::String(m.clone()))
            .collect();
        map.insert(
            serde_yaml::Value::String("available_models".into()),
            serde_yaml::Value::Sequence(yaml_models),
        );
    }

    let serialized = serde_yaml::to_string(&root)?;
    let tmp = cfg_path.with_extension("yaml.tmp");
    std::fs::write(&tmp, serialized)?;
    std::fs::rename(&tmp, &cfg_path)?;
    Ok(cfg_path)
}

pub async fn handle_model(args: &super::ModelArgs, formatter: &mut Box<dyn OutputFormatter>) -> Result<()> {
    match &args.action {
        ModelAction::List => {
            // 优先读 cache（首次启动 discover 已写入），cache miss 提示
            use abacus_core::llm::ModelCache;
            match ModelCache::load(&ModelCache::default_path()) {
                Ok(Some(cache)) => {
                    formatter.format_message("model", "Available Models (cached):", None);
                    for (provider, models) in &cache.providers {
                        formatter.format_message("model", &format!("  [{}]", provider), None);
                        for m in models {
                            formatter.format_message("model", &format!("    • {}", m), None);
                        }
                    }
                    formatter.format_message("model",
                        &format!("Total: {} models from {} providers (last discovered: {})",
                            cache.all_models().len(),
                            cache.providers.len(),
                            chrono::DateTime::from_timestamp(cache.discovered_at, 0)
                                .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                                .unwrap_or_else(|| "unknown".into())),
                        None);
                }
                _ => {
                    formatter.format_message("model",
                        "No model cache found. Run `abacus model discover` to populate it.", None);
                }
            }
        }
        ModelAction::Show { name } => {
            formatter.format_message("model", &format!("Model: {}", name), None);
            if name.contains("deepseek") {
                formatter.format_message("model", "  Provider: DeepSeek", None);
                formatter.format_message("model", "  Context: 131072 tokens", None);
                formatter.format_message("model", "  Thinking: extended", None);
                formatter.format_message("model", "  Capabilities: chat, code, tool_use, reasoning", None);
            } else {
                formatter.format_message("model", "  (not in registry)", None);
            }
        }
        ModelAction::Ping { name } => {
            match engine_init::create_engine(name, None, "low").await {
                Ok((core, session)) => {
                    match core.process_turn("ping", &session).await {
                        Ok(_) => formatter.format_message("model", "✓ reachable", None),
                        Err(e) => formatter.format_message("model", &format!("✗ {}", e), None),
                    }
                }
                Err(e) => formatter.format_message("model", &format!("✗ {}", e), None),
            }
        }
        ModelAction::Discover { path, write_config } => {
            // 用默认 model 初始化 engine（仅为获取注册的 provider 列表）
            let default_model = abacus_types::ModelId::AUTO; // engine_init 按配置链解析真实 provider
            match engine_init::create_engine(default_model, None, "off").await {
                Ok((core, _session)) => {
                    formatter.format_message("model", "🔍 Discovering models from all registered providers...", None);
                    let cache = core.discover_and_cache(path.as_deref()).await;
                    let total = cache.all_models().len();
                    formatter.format_message("model",
                        &format!("✓ Discovered {} models from {} providers", total, cache.providers.len()), None);
                    for (provider, models) in &cache.providers {
                        formatter.format_message("model",
                            &format!("  [{}] {} models", provider, models.len()), None);
                    }
                    let cache_path = path.clone().unwrap_or_else(abacus_core::llm::ModelCache::default_path);
                    formatter.format_message("model",
                        &format!("Cache written to: {}", cache_path.display()), None);

                    // --write-config: 把 union 写入 ~/.abacus/config.yaml [available_models]
                    if *write_config {
                        match write_available_models_to_config(&cache.all_models()) {
                            Ok(path) => formatter.format_message("model",
                                &format!("✓ available_models written to {}", path.display()), None),
                            Err(e) => formatter.format_error("CONFIG", &e.to_string(), None),
                        }
                    }
                }
                Err(e) => {
                    formatter.format_error("ENGINE", &e.to_string(), None);
                }
            }
        }
    }
    Ok(())
}