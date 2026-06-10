//! TUI 渲染压测 —— 模拟 100 条消息 + 滚动操作，测量帧率
//!
//! 运行: cargo run -p abacus-cli --example tui_bench
//! 使用 TestBackend（无需 TTY），可在 CI / headless 环境运行

use std::time::Instant;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn main() {
    // TestBackend: 80x40 虚拟终端（无需真实 TTY）
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    // 构建 state 填充大量消息
    use abacus_cli::tui::state::{AppState, Message, MsgContent, MsgRole, BlockKind, AbacusMode};

    // V33: AbacusMode::Chat 重命名为 Clarify（澄清模式作为默认入口）
    let mut state = AppState::new(AbacusMode::Clarify);
    state.model_name = "bench-model".into();

    // 插入 100 条混合消息（用户 + AI，含代码块和长文本）
    for i in 0..100 {
        if i % 3 == 0 {
            state.add_message(Message {
                role: MsgRole::User,
                parts: vec![MsgContent::Stream(format!("这是第 {} 条用户消息，包含一些中文内容用于测试 word-wrap 的行为是否正确。", i))],
                time: format!("{}:{:02}", 14 + i / 60, i % 60),
            });
        } else if i % 3 == 1 {
            state.add_message(Message {
                role: MsgRole::Session,
                parts: vec![
                    MsgContent::Block {
                        kind: BlockKind::Think,
                        summary: format!("Thinking ({} lines)", i * 2),
                        detail: "分析用户需求...\n检查可用工具...\n规划执行步骤...".into(),
                        collapsed: true,
                    },
                    MsgContent::Stream(format!(
                        "```rust\nfn bench_{}() {{\n    let x = {};\n    println!(\"result: {{}}\", x * 2);\n}}\n```\n\n这是解释文本第 {} 段。涵盖了多行内容用于验证渲染性能在大量消息时是否仍然流畅。",
                        i, i * 42, i
                    )),
                ],
                time: format!("{}:{:02}", 14 + i / 60, (i + 1) % 60),
            });
        } else {
            let long_text = "这是一段较长的专家分析文本。".repeat(10);
            state.add_message(Message {
                role: MsgRole::Expert("architect".into()),
                parts: vec![MsgContent::Stream(long_text)],
                time: format!("{}:{:02}", 14 + i / 60, (i + 2) % 60),
            });
        }
    }

    // 添加事件（面板时间线）
    for i in 0..50 {
        state.add_event(
            format!("14:{:02}", i % 60),
            ["llm", "tool", "session"][i % 3],
            format!("事件 #{}: 测试渲染性能", i),
            abacus_cli::tui::state::EventLevel::Info,
        );
    }

    // ─── 压测 1: 底部渲染（auto-scroll）帧率 ───
    let frames = 60;
    let rows = 40u16;
    let start = Instant::now();
    for _ in 0..frames {
        terminal.draw(|f| {
            abacus_cli::tui::modes::render(f, &state, rows);
        }).unwrap();
    }
    let elapsed = start.elapsed();
    let fps1 = frames as f64 / elapsed.as_secs_f64();

    // ─── 压测 2: 滚动到中间位置渲染 ───
    state.scroll = 150;
    state.mark_dirty();
    let start = Instant::now();
    for _ in 0..frames {
        terminal.draw(|f| {
            abacus_cli::tui::modes::render(f, &state, rows);
        }).unwrap();
    }
    let elapsed = start.elapsed();
    let fps2 = frames as f64 / elapsed.as_secs_f64();

    // ─── 压测 3: streaming 模式（每帧追加文本）───
    state.scroll = 0;
    state.begin_streaming_session();
    // V42-B 拆卡: thinking 走 ThinkingCard
    let th_id = state.cards.alloc_id();
    let mut th = abacus_cli::tui::cards::ThinkingCard::new(th_id, "gpt-4");
    th.append("正在分析...\n检查依赖...\n规划方案...");
    state.cards.push_active(Box::new(th));
    state.cards.finish_active();
    // reply 走 LlmCard（begin_streaming_session 已推入 LlmCard）
    state.begin_streaming_session();
    let start = Instant::now();
    for i in 0..frames {
        if let Some(llm) = state.cards.card_downcast_mut::<abacus_cli::tui::cards::LlmCard>(
            state.cards.active_id().unwrap()
        ) {
            llm.append_reply(&format!("第{}帧的流式文本输出。包含中英文混合 content for word-wrap testing. ", i));
        }
        state.mark_dirty();
        terminal.draw(|f| {
            abacus_cli::tui::modes::render(f, &state, rows);
        }).unwrap();
    }
    let elapsed = start.elapsed();
    let fps3 = frames as f64 / elapsed.as_secs_f64();

    // 输出结果
    println!("╭─────────────────────────────────────────╮");
    println!("│       Abacus TUI 渲染压测结果           │");
    println!("├─────────────────────────────────────────┤");
    println!("│ 终端: 120x40  消息: 100条  事件: 50条   │");
    println!("├─────────────────────────────────────────┤");
    println!("│ 场景 1 (底部渲染):  {:>6.1} FPS          │", fps1);
    println!("│ 场景 2 (滚动中间):  {:>6.1} FPS          │", fps2);
    println!("│ 场景 3 (streaming): {:>6.1} FPS          │", fps3);
    println!("├─────────────────────────────────────────┤");
    println!("│ 每帧耗时:                               │");
    println!("│   底部: {:>6.2} ms                        │", 1000.0 / fps1);
    println!("│   滚动: {:>6.2} ms                        │", 1000.0 / fps2);
    println!("│   流式: {:>6.2} ms                        │", 1000.0 / fps3);
    println!("╰─────────────────────────────────────────╯");

    if fps1 < 30.0 || fps2 < 30.0 || fps3 < 20.0 {
        println!("\n⚠️  部分场景低于目标帧率！");
        println!("  建议: streaming markdown 增量解析优化");
    } else {
        println!("\n✓ 所有场景流畅 (≥20 FPS)");
    }
}
