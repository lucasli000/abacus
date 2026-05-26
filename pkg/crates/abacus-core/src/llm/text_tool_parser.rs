//! 文本格式工具调用提取器
//!
//! ## 触发场景
//! LLM 在 `content` 字段中以文本格式输出工具调用（而非标准 `tool_calls` 字段）时调用。
//! 常见于 DeepSeek thinking 模式下模型退化为文本格式表达工具调用。
//!
//! ## 支持格式
//! 1. **XML**：`<function_calls><invoke name="X"><parameter name="k">v</parameter></invoke></function_calls>`
//! 2. **JSON object**：`{"tool": "X", "params": {...}}` / `{"tool_name": "X", "arguments": {...}}`
//! 3. **代码块 JSON**：` ```json\n{"tool": "X", ...}\n``` `
//!
//! ## 引用关系
//! - 消费者：`deepseek::parse_response`（blocking 路径）、`deepseek::stream_complete`（streaming 末尾组装）
//! - 生产者：`LlmProvider::complete` / `stream_complete` 返回的 `LlmResponse`
//!
//! ## 约束
//! - 纯函数，无全局状态，无副作用
//! - 零流式延迟：仅在 Done 事件后、最终 LlmResponse 组装阶段调用
//! - 延迟上限 20ms（字符串操作，典型 <0.5ms）

use crate::llm::provider::{ToolCall, ToolFunction};

/// 从 response content 文本中提取工具调用。
///
/// 返回 `(cleaned_content, tool_calls)`：
/// - `cleaned_content`：剔除工具调用声明后的纯文本内容
/// - `tool_calls`：解析出的工具调用列表（空 = 未找到）
///
/// 若未找到任何工具调用，返回原 content 不变。
pub fn extract_text_tool_calls(content: &str) -> (String, Vec<ToolCall>) {
    // 优先尝试 XML 格式（function_calls 标签）
    if let Some(result) = try_extract_xml_tool_calls(content) {
        return result;
    }
    // 其次尝试 JSON 格式（顶层对象或代码块）
    if let Some(result) = try_extract_json_tool_calls(content) {
        return result;
    }
    (content.to_string(), vec![])
}

// ── XML 格式解析 ─────────────────────────────────────────────────────────────
//
// 目标格式：
//   <function_calls>
//     <invoke name="tool_name">
//       <parameter name="key">value</parameter>
//     </invoke>
//   </function_calls>

fn try_extract_xml_tool_calls(content: &str) -> Option<(String, Vec<ToolCall>)> {
    let open_tag = "<function_calls>";
    let close_tag = "</function_calls>";

    let start = content.find(open_tag)?;
    let end_offset = content[start..].find(close_tag)?;
    let end = start + end_offset + close_tag.len();

    let block = &content[start..end];
    let mut calls = Vec::new();
    let mut id_counter: u32 = 0;
    let mut pos = 0;

    while let Some(invoke_rel) = block[pos..].find("<invoke") {
        let inv_start = pos + invoke_rel;

        // 找 </invoke> 结束位置
        let inv_end_rel = block[inv_start..].find("</invoke>")?;
        let inv_end = inv_start + inv_end_rel + "</invoke>".len();

        let invoke_block = &block[inv_start..inv_end];

        // 提取 name 属性
        let name = extract_xml_attr_value(invoke_block, "name").unwrap_or_default();
        if name.is_empty() {
            pos = inv_end;
            continue;
        }

        // 提取 <parameter> 列表
        let mut args: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        let param_open = "<parameter";
        let param_close = "</parameter>";
        let mut param_pos = 0;
        loop {
            let Some(ps_rel) = invoke_block[param_pos..].find(param_open) else { break };
            let ps = param_pos + ps_rel;
            let param_name = extract_xml_attr_value(&invoke_block[ps..], "name")
                .unwrap_or_default();
            // 找到 > 后的内容直到 </parameter>
            let Some(gt_rel) = invoke_block[ps..].find('>') else { break };
            let content_start = ps + gt_rel + 1;
            let Some(pe_rel) = invoke_block[content_start..].find(param_close) else { break };
            let raw_val = invoke_block[content_start..content_start + pe_rel].trim();

            // 尝试将 value 解析为 JSON（处理数字/bool/object），否则作为字符串
            let json_val = serde_json::from_str::<serde_json::Value>(raw_val)
                .unwrap_or_else(|_| serde_json::Value::String(raw_val.to_string()));

            if !param_name.is_empty() {
                args.insert(param_name, json_val);
            }
            param_pos = content_start + pe_rel + param_close.len();
        }

        id_counter += 1;
        calls.push(ToolCall {
            id: format!("txt_{id_counter}"),
            type_: "function".into(),
            function: ToolFunction {
                name,
                arguments: serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
            },
        });

        pos = inv_end;
    }

    if calls.is_empty() {
        return None;
    }

    // 剔除 function_calls 块，保留前后文本
    let cleaned = format!("{}{}", content[..start].trim_end(), content[end..].trim_start())
        .trim()
        .to_string();
    Some((cleaned, calls))
}

/// 从 XML 片段中提取 `attr="value"` 形式的属性值（支持单引号和双引号）
fn extract_xml_attr_value(fragment: &str, attr: &str) -> Option<String> {
    let dq_pattern = format!("{}=\"", attr);
    let sq_pattern = format!("{}='", attr);

    if let Some(start) = fragment.find(dq_pattern.as_str()) {
        let after = &fragment[start + dq_pattern.len()..];
        let end = after.find('"')?;
        return Some(after[..end].to_string());
    }
    if let Some(start) = fragment.find(sq_pattern.as_str()) {
        let after = &fragment[start + sq_pattern.len()..];
        let end = after.find('\'')?;
        return Some(after[..end].to_string());
    }
    None
}

// ── JSON 格式解析 ─────────────────────────────────────────────────────────────
//
// 支持两种形态：
//
// A. 完整 content 本身是 JSON 对象：
//      {"tool": "config_set", "params": {"key": "ctx", "value": "100"}}
//
// B. 内嵌代码块：
//      ```json
//      {"tool": "config_set", "params": {...}}
//      ```

fn try_extract_json_tool_calls(content: &str) -> Option<(String, Vec<ToolCall>)> {
    let trimmed = content.trim();

    // 尝试整体作为 JSON 对象解析
    if trimmed.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(call) = json_value_to_tool_call(&v) {
                return Some((String::new(), vec![call]));
            }
        }
    }

    // 尝试从代码块中提取 JSON
    extract_json_from_fence(content)
}

/// 将 JSON 值解析为 ToolCall（要求包含 tool/tool_name 字段）
fn json_value_to_tool_call(v: &serde_json::Value) -> Option<ToolCall> {
    let obj = v.as_object()?;

    // 工具名：tool > tool_name > function_name
    let name = obj.get("tool")
        .or_else(|| obj.get("tool_name"))
        .or_else(|| obj.get("function_name"))
        .or_else(|| obj.get("name"))
        .and_then(|n| n.as_str())?
        .to_string();

    if name.is_empty() {
        return None;
    }

    // 参数：params > arguments > input > parameters
    let args = obj.get("params")
        .or_else(|| obj.get("arguments"))
        .or_else(|| obj.get("input"))
        .or_else(|| obj.get("parameters"))
        .cloned()
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

    Some(ToolCall {
        id: "txt_json_1".into(),
        type_: "function".into(),
        function: ToolFunction {
            name,
            arguments: serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
        },
    })
}

/// 从 ```json ... ``` 代码块中提取 ToolCall
fn extract_json_from_fence(content: &str) -> Option<(String, Vec<ToolCall>)> {
    // 支持 ```json 和 ``` 两种开头
    for fence_open in &["```json\n", "```json\r\n", "```\n"] {
        if let Some(fence_start) = content.find(fence_open) {
            let after = &content[fence_start + fence_open.len()..];
            if let Some(fence_end) = after.find("```") {
                let json_str = after[..fence_end].trim();
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
                    if let Some(call) = json_value_to_tool_call(&v) {
                        let before = content[..fence_start].trim_end();
                        let after_block = content[fence_start + fence_open.len() + fence_end + 3..].trim_start();
                        let cleaned = if before.is_empty() && after_block.is_empty() {
                            String::new()
                        } else {
                            format!("{}{}", before, after_block).trim().to_string()
                        };
                        return Some((cleaned, vec![call]));
                    }
                }
            }
        }
    }
    None
}

// ── 单元测试 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xml_single_invoke() {
        let content = "<function_calls>\n<invoke name=\"fs_read\">\n<parameter name=\"path\">/tmp/a.txt</parameter>\n</invoke>\n</function_calls>";
        let (cleaned, calls) = extract_text_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "fs_read");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["path"], "/tmp/a.txt");
        assert!(cleaned.is_empty());
    }

    #[test]
    fn test_xml_multi_invoke() {
        let content = "<function_calls>\
            <invoke name=\"tool_a\"><parameter name=\"x\">1</parameter></invoke>\
            <invoke name=\"tool_b\"><parameter name=\"y\">hello</parameter></invoke>\
        </function_calls>";
        let (_cleaned, calls) = extract_text_tool_calls(content);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "tool_a");
        assert_eq!(calls[1].function.name, "tool_b");
    }

    #[test]
    fn test_xml_with_surrounding_text() {
        let content = "思考中...\n<function_calls><invoke name=\"search\"><parameter name=\"q\">rust</parameter></invoke></function_calls>\n结果如下";
        let (cleaned, calls) = extract_text_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert!(cleaned.contains("思考中"));
        assert!(cleaned.contains("结果如下"));
        assert!(!cleaned.contains("function_calls"));
    }

    #[test]
    fn test_json_object() {
        let content = r#"{"tool": "config_set", "params": {"key": "ctx", "value": "100"}}"#;
        let (_cleaned, calls) = extract_text_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "config_set");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["key"], "ctx");
    }

    #[test]
    fn test_json_fence() {
        let content = "我将调用工具：\n```json\n{\"tool\": \"shell\", \"params\": {\"cmd\": \"ls\"}}\n```\n";
        let (_cleaned, calls) = extract_text_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "shell");
    }

    #[test]
    fn test_no_tool_call() {
        let content = "这是普通回复，没有工具调用。";
        let (cleaned, calls) = extract_text_tool_calls(content);
        assert!(calls.is_empty());
        assert_eq!(cleaned, content);
    }

    #[test]
    fn test_xml_numeric_param() {
        let content = "<function_calls><invoke name=\"calc\"><parameter name=\"n\">42</parameter></invoke></function_calls>";
        let (_cleaned, calls) = extract_text_tool_calls(content);
        assert_eq!(calls.len(), 1);
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        // 数字 42 被解析为 JSON Number，不是字符串
        assert_eq!(args["n"], 42);
    }
}
