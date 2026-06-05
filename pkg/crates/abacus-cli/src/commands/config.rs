use color_eyre::eyre::Result;
use crate::OutputFormatter;
use super::ConfigAction;

fn config_path() -> std::path::PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".abacus/config.toml"))
        .unwrap_or_else(|| std::path::PathBuf::from(".abacus/config.toml"))
}

pub async fn handle_config(args: &super::ConfigArgs, formatter: &mut Box<dyn OutputFormatter>) -> Result<()> {
    match &args.action {
        ConfigAction::ListKeys => {
            let keys = [
                ("core.default_model",    "对话使用的模型（如 deepseek-v4-flash, auto=走配置链）"),
                ("core.system_prompt",    "AI 助理的自定义角色设定语"),
                ("core.temperature",      "回答的随机创意程度，0 = 严谨保守，1 = 天马行空"),
                ("core.max_tokens",       "单次回复最长长度"),
                ("core.thinking",          "[PRIMARY] 思考意图：off / adaptive / minimal / low / medium / high / max / xhigh / <整数 budget>"),
                ("core.thinking_display",  "[PRIMARY] Anthropic adaptive 显示：summarized / omitted"),
                ("core.context_window",   "记忆对话的总量上限"),
                ("core.max_turns",        "单次任务最多来回对话轮数"),
                ("core.max_tool_calls",   "单次回复最多执行工具次数"),
                ("server.max_sessions",   "服务器最大同时连接数"),
            ];
            formatter.format_message("config", "Available configuration keys (config.toml):", None);
            for (key, desc) in &keys {
                formatter.format_message("config", &format!("  {:<26} {}", key, desc), None);
            }
            formatter.format_message("config", "\nProvider 配置见 provider.toml（LLM 供应商 + 模型参数）", None);
            formatter.format_message("config", "Set via: abacus config set <key> <value>", None);
            formatter.format_message("config", "Override via env: ABACUS_<UPPER_KEY>", None);
        }
        ConfigAction::Show => {
            let path = config_path();
            if path.exists() {
                let content = std::fs::read_to_string(&path)?;
                formatter.format_message("config", &format!("# {}\n{}", path.display(), content), None);
            } else {
                formatter.format_message("config", &format!("No config found at {}", path.display()), None);
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
            // 解析-修改-序列化：消除 push_str 破坏结构 / 缩进的隐患
            let path = config_path();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let mut root: toml::Value = if path.exists() {
                let content = std::fs::read_to_string(&path)?;
                toml::from_str(&content).unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()))
            } else {
                toml::Value::Table(toml::map::Map::new())
            };

            // key 支持点分嵌套: "core.temperature" → core.temperature
            let parts: Vec<&str> = key.split('.').collect();
            if let Err(e) = set_nested_toml(&mut root, &parts, value) {
                formatter.format_error("CONFIG_PATH_CONFLICT", &e, None);
                return Ok(());
            }

            let serialized = toml::to_string_pretty(&root)
                .map_err(|e| color_eyre::eyre::eyre!("TOML serialize error: {e}"))?;
            std::fs::write(&path, serialized)?;
            // 含 API key / token 风险，强制 0o600
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&path) {
                    let mut perms = meta.permissions();
                    perms.set_mode(0o600);
                    let _ = std::fs::set_permissions(&path, perms);
                }
            }
            formatter.format_message("config", &format!("[✓] Set {} = {}", key, value), None);
        }
        ConfigAction::Edit => {
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".into());
            let path = config_path();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if !path.exists() {
                let template = r#"# Abacus Configuration (config.toml)
# LLM provider 配置见 provider.toml

[core]
default_model = "auto"
max_turns = 25
temperature = 0.6
"#;
                std::fs::write(&path, template)?;
            }
            formatter.format_message("config", &format!("Opening with {}...", editor), None);
            std::process::Command::new(&editor).arg(&path).status()?;
        }
        ConfigAction::Validate => {
            let path = config_path();
            if !path.exists() {
                formatter.format_error("CONFIG_MISSING", "No config file found", None);
                return Ok(());
            }
            let content = std::fs::read_to_string(&path)?;
            match toml::from_str::<toml::Value>(&content) {
                Ok(_) => formatter.format_message("config", "[✓] Configuration valid", None),
                Err(e) => formatter.format_error("CONFIG_INVALID", &format!("TOML error: {}", e), None),
            }
        }
    }
    Ok(())
}

/// 在 toml::Value::Table 中按 key 路径取/建子表，最后一段写入值
///
/// 返回 `Err` 当路径中间段是 scalar/string——避免静默覆盖用户已有配置。
/// 调用方应捕获错误并提示用户「该路径与现有 scalar 冲突」并询问是否要 `--force` 覆盖。
fn set_nested_toml(root: &mut toml::Value, parts: &[&str], value: &str) -> Result<(), String> {
    if parts.is_empty() { return Ok(()); }

    if !root.is_table() {
        *root = toml::Value::Table(toml::map::Map::new());
    }
    let table = root.as_table_mut().expect("just ensured is_table");

    if parts.len() == 1 {
        // 末段：尝试按 TOML 字面量解析（支持 true/false/int/float/string/array/table）
        let parsed = parse_toml_value(value);
        table.insert(parts[0].to_string(), parsed);
        return Ok(());
    }

    // 中间段：递归下钻。
    // 若 key 已存在但不是 Table，**静默覆盖会销毁用户数据**——拒绝。
    let key = parts[0];
    let needs_insert = !table.contains_key(key);
    if needs_insert {
        table.insert(key.to_string(), toml::Value::Table(toml::map::Map::new()));
    } else {
        // 存在 → 必须为 Table
        let existing = table.get(key).expect("checked contains_key");
        if !existing.is_table() {
            return Err(format!(
                "path conflict: key '{}' exists as {} (expected table for nested path '{}')",
                key,
                value_type_name(existing),
                parts.join(".")
            ));
        }
    }
    let next = table.get_mut(key).expect("just inserted or validated as table");
    set_nested_toml(next, &parts[1..], value)
}

/// toml::Value 类型名（用于错误提示）
fn value_type_name(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_)  => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_)   => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_)   => "array",
        toml::Value::Table(_)   => "table",
    }
}

/// 尝试把字符串解析为 TOML 字面量；解析失败当字符串原样存
///
/// 顺序：bool → int → float → array → inline-table → string fallback。
///
/// **重要**：toml crate 的 `Value::from_str`（含 `parse::<toml::Value>()`）把首字符
/// `[` 误判为 table header 起始，导致 `[1, 2, 3]` / `["a"]` 永远解析失败——
///
/// 正确做法是包一层 dummy key 后再解析。本函数已修复此 bug。
fn parse_toml_value(s: &str) -> toml::Value {
    // 试 boolean
    match s {
        "true" => return toml::Value::Boolean(true),
        "false" => return toml::Value::Boolean(false),
        _ => {}
    }
    // 试 integer
    if let Ok(n) = s.parse::<i64>() {
        return toml::Value::Integer(n);
    }
    // 试 float（拒绝 NaN/Inf——TOML 浮点必须有限）
    if let Ok(f) = s.parse::<f64>() {
        if f.is_finite() {
            return toml::Value::Float(f);
        }
    }
    // 试 array：包一层 dummy key 绕过 table-header 歧义
    if s.starts_with('[') && s.ends_with(']') {
        let wrapped = format!("__v = {}", s);
        match wrapped.parse::<toml::Value>() {
            Ok(v) if v.get("__v").map(|x| x.is_array()).unwrap_or(false) => {
                return v.get("__v").cloned().unwrap();
            }
            _ => {
                eprintln!("warning: value looks like array ({:?}) but TOML parse failed; storing as string", s);
            }
        }
    }
    // 试 inline table：同样包一层（虽然 `{` 不歧义，但保持一致）
    if s.starts_with('{') && s.ends_with('}') {
        let wrapped = format!("__v = {}", s);
        match wrapped.parse::<toml::Value>() {
            Ok(v) if v.get("__v").map(|x| x.is_table()).unwrap_or(false) => {
                return v.get("__v").cloned().unwrap();
            }
            _ => {
                eprintln!("warning: value looks like inline table ({:?}) but TOML parse failed; storing as string", s);
            }
        }
    }
    toml::Value::String(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_table() -> toml::Value {
        toml::Value::Table(toml::map::Map::new())
    }

    // ── 🔴#3: parse_toml_value 字面量解析 ────────────────────────────────

    #[test]
    fn parse_toml_bool_int_float() {
        assert_eq!(parse_toml_value("true"),  toml::Value::Boolean(true));
        assert_eq!(parse_toml_value("false"), toml::Value::Boolean(false));
        assert_eq!(parse_toml_value("42"),    toml::Value::Integer(42));
        assert_eq!(parse_toml_value("3.14"),  toml::Value::Float(3.14));
    }

    #[test]
    fn parse_toml_array_inline_table_and_fallback() {
        // 合法 array
        let v = parse_toml_value(r#"[1, 2, 3]"#);
        assert!(v.is_array(), "expected array, got {:?}", v);
        if let toml::Value::Array(a) = v {
            assert_eq!(a.len(), 3);
        }
        // 合法 string array
        let v = parse_toml_value(r#"["a", "b"]"#);
        assert!(v.is_array());
        // 合法 inline table
        let v = parse_toml_value("{name = \"x\", n = 1}");
        assert!(v.is_table());
        // 坏 array → fallback string（用户错误不会被静默吞）
        let v = parse_toml_value("[1, 2, oops]");
        assert!(matches!(v, toml::Value::String(_)), "expected string fallback, got {:?}", v);
        // 普通字符串原样
        let v = parse_toml_value("hello world");
        assert_eq!(v, toml::Value::String("hello world".into()));
    }

    // ── 🔴#4: set_nested_toml 中间段冲突保护 ─────────────────────────────

    #[test]
    fn set_nested_creates_missing_intermediate() {
        let mut root = empty_table();
        set_nested_toml(&mut root, &["core", "temperature"], "0.7").unwrap();
        let core = root.get("core").unwrap();
        assert!(core.is_table());
        assert_eq!(
            core.get("temperature").unwrap(),
            &toml::Value::Float(0.7)
        );
    }

    #[test]
    fn set_nested_rejects_scalar_intermediate() {
        // 模拟：config.toml 中已有 `name = "alice"`（scalar），用户尝试 `name.full = "x"`
        let mut root = empty_table();
        set_nested_toml(&mut root, &["name"], "alice").unwrap();
        // 再次尝试在 `name` 下设嵌套键——`name` 是 string，**必须拒绝**
        let err = set_nested_toml(&mut root, &["name", "full"], "x").unwrap_err();
        assert!(err.contains("path conflict"), "got: {}", err);
        assert!(err.contains("name"), "got: {}", err);
        assert!(err.contains("string"), "got: {}", err);
        // 原 scalar 必须保持不变
        assert_eq!(root.get("name").unwrap(), &toml::Value::String("alice".into()));
    }

    #[test]
    fn set_nested_overwrites_leaf_value() {
        let mut root = empty_table();
        set_nested_toml(&mut root, &["core", "max_turns"], "10").unwrap();
        set_nested_toml(&mut root, &["core", "max_turns"], "50").unwrap();
        assert_eq!(
            root.get("core").unwrap().get("max_turns").unwrap(),
            &toml::Value::Integer(50)
        );
    }

    #[test]
    fn set_nested_empty_parts_is_noop() {
        let mut root = empty_table();
        set_nested_toml(&mut root, &[], "x").unwrap();
        assert!(root.as_table().unwrap().is_empty());
    }
}
