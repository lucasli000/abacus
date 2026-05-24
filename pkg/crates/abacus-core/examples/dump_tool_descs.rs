//! 实际发到 LLM 的 ToolDefinition 字段占比
use abacus_core::tool::ToolRegistry;
use abacus_core::tool::builtin::register_all;

#[tokio::main]
async fn main() {
    let registry = ToolRegistry::new();
    register_all(&registry).await;
    let tools = registry.all_tools().await;
    
    // 模拟 build_tool_definitions_for 的关键路径——构造 ToolFunctionSpec 三字段
    let mut total = 0usize;
    let mut name_total = 0;
    let mut desc_total = 0;
    let mut params_total = 0;
    let mut wrap_total = 0;
    for t in &tools {
        let cost_suffix = t.schema.cost.as_ref().map(|c| {
            format!(" [~{}t/{}/{}]", c.tokens, c.latency, c.risk)
        }).unwrap_or_default();
        let final_desc = format!("{}{}", t.schema.description, cost_suffix);
        let name_bytes = serde_json::to_string(&t.schema.name).unwrap().len();
        let desc_bytes = serde_json::to_string(&final_desc).unwrap().len();
        let params_bytes = serde_json::to_string(&t.schema.parameters).unwrap().len();
        // ToolDefinition wrapper: {"type":"function","function":{"name":..,"description":..,"parameters":..}}
        let wrap_bytes = r#"{"type":"function","function":{"name":,"description":,"parameters":}},"#.len();
        let one = name_bytes + desc_bytes + params_bytes + wrap_bytes;
        total += one;
        name_total += name_bytes;
        desc_total += desc_bytes;
        params_total += params_bytes;
        wrap_total += wrap_bytes;
    }
    println!("=== Actual LLM-bound ToolDefinition breakdown (38 tools post-LSP-lazy) ===");
    println!("  name (含引号)         : {:>6} bytes  {:>4} tokens", name_total, name_total/4);
    println!("  description+cost     : {:>6} bytes  {:>4} tokens", desc_total, desc_total/4);
    println!("  parameters (JSON Schema): {:>6} bytes  {:>4} tokens", params_total, params_total/4);
    println!("  wrapper/{{type/function}}: {:>6} bytes  {:>4} tokens", wrap_total, wrap_total/4);
    println!("  TOTAL                : {:>6} bytes  {:>4} tokens", total, total/4);
}
