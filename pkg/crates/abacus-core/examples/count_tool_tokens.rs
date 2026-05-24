//! 单轮 LLM 调用 token 开销估算
use abacus_core::tool::ToolRegistry;
use abacus_core::tool::builtin::register_all;

#[tokio::main]
async fn main() {
    // ── Part 1: 工具 schema ──────────────────────────────────────
    let registry = ToolRegistry::new();
    register_all(&registry).await;
    let tools = registry.all_tools().await;
    let mut tool_total = 0usize;
    for t in &tools {
        let json = serde_json::to_string(&t.schema).unwrap_or_default();
        tool_total += json.len();
    }
    let openai_wrapper_per_tool = 40; // {"type":"function","function":{...}}
    let tool_array_overhead = 4; // [..]
    let tool_bytes = tool_total + tools.len() * openai_wrapper_per_tool + tool_array_overhead;

    // ── Part 2: System prompt（layered，估算）─────────────────
    // 基于 prompt_assembly 实际 layer 结构
    let layer_estimates: Vec<(&str, usize)> = vec![
        ("Layer 255 base identity", 200),
        ("Layer 240 capabilities", 300),
        ("Layer 220 task analysis", 400),
        ("Layer 200 epistemic guard", 500),
        ("Layer 180 KB primer (ICL)", 1500),
        ("Layer 160 deduction injection", 600),
        ("Layer 140 effectiveness/tier", 200),
        ("Layer 120 retained content", 1000),
        ("Layer 100 examples/skills", 800),
        ("Layer 80 routing hints", 200),
        ("Layer 60 inertia warning", 100),
        ("Layer 40 thinking config", 200),
        ("Layer 20 footer", 100),
    ];
    let layer_total: usize = layer_estimates.iter().map(|(_, b)| b).sum();

    // ── Part 3: 一轮简单对话 messages ────────────────────────────
    let user_msg = 100;        // 短用户输入
    let assistant_thinking = 300; // CoT
    let tool_call = 200;       // tool call JSON
    let tool_result = 3000;    // 典型 fs.read 返回 1KB 文件 ≈ 3KB JSON
    let assistant_response = 500;

    let _messages_total = user_msg + assistant_thinking + tool_call + tool_result + assistant_response;

    println!("\n═══ Abacus 单轮 LLM 请求 token 估算（DeepSeek/OpenAI 系，1 token ≈ 4 bytes）═══\n");
    println!("【输入端 - 发送到 LLM 的 prompt】");
    println!("  ① 工具定义 (38 tools)        : {:>6} bytes ≈ {:>5} tokens", tool_bytes, tool_bytes/4);
    println!("  ② System prompt (layered)    : {:>6} bytes ≈ {:>5} tokens", layer_total, layer_total/4);
    println!("  ③ 用户消息                   : {:>6} bytes ≈ {:>5} tokens", user_msg, user_msg/4);
    println!("  ④ 工具调用结果（中等大小）   : {:>6} bytes ≈ {:>5} tokens", tool_result, tool_result/4);
    let input_total = tool_bytes + layer_total + user_msg + tool_result;
    println!("                                ───────────────────────────");
    println!("  小计（一次工具调用前提示）   : {:>6} bytes ≈ {:>5} tokens", input_total, input_total/4);

    println!("\n【输出端 - LLM 生成】");
    println!("  ⑤ Assistant CoT + tool_call : {:>6} bytes ≈ {:>5} tokens", assistant_thinking + tool_call, (assistant_thinking + tool_call)/4);
    println!("  ⑥ Final response             : {:>6} bytes ≈ {:>5} tokens", assistant_response, assistant_response/4);

    println!("\n【典型场景估算】");
    let simple_one_shot = tool_bytes + layer_total + user_msg + assistant_response;
    println!("  • 简单对话 0 工具调用        : ≈ {:>5} tokens (input)", simple_one_shot/4);
    let one_tool = tool_bytes + layer_total + user_msg + assistant_thinking + tool_call + tool_result;
    println!("  • 1 次工具调用 (fs.read 中等): ≈ {:>5} tokens (input)", one_tool/4);
    let three_tools = tool_bytes + layer_total + user_msg + (assistant_thinking + tool_call + tool_result) * 3;
    println!("  • 3 次工具调用               : ≈ {:>5} tokens (input)", three_tools/4);
    let agent_loop = tool_bytes + layer_total + user_msg + (assistant_thinking + tool_call + tool_result) * 8;
    println!("  • 8 次工具调用 (agent loop) : ≈ {:>5} tokens (input)", agent_loop/4);

    println!("\n【KV cache 友好度】");
    println!("  工具 defs (前缀稳定)         : {:>5} tokens 应 100% 命中 cache", tool_bytes/4);
    println!("  System prompt (前缀稳定)     : {:>5} tokens 应 100% 命中 cache", layer_total/4);
    println!("  历史 messages (前缀稳定)     : 每轮新增 ~{} tokens（缓存命中后续轮）", (assistant_thinking + tool_call + tool_result) / 4);
}
