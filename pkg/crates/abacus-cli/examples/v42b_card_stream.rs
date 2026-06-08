//! V42-B 消息流重构演示
//!
//! ## 目的
//!
//! **仅演示**消息流的 V42-B CardStream 渲染效果（替代 V40 messages.rs 的 1439 行）。
//! 其他区域（顶栏 / 看板 / 输入栏 / 仪表盘）**完全复用项目内置渲染函数**，
//! 不做任何 demo 自由发挥, 真实反映项目实际布局。
//!
//! ## 演示数据
//!
//! 4 张 Card 注入 `state.cards: CardStream`:
//! - UserCard       — 用户输入消息
//! - LlmCard (active) — LLM 思考+回复（流式中, 顶部 shimmer 光带）
//! - AbacusCard     — 工具调用 (fs_edit + Generic event)
//! - ExpertCard     — Meeting 专家
//!
//! ## 运行
//!
//! ```bash
//! cargo run -p abacus-cli --example v42b_card_stream
//! ```
//!
//! 渲染一帧到 stdout 然后退出。
//!
//! ## 与生产差异
//!
//! - 不连真实 LLM (只渲染静态演示数据)
//! - 不响应键盘/鼠标事件 (仅画一帧)
//! - 其他区域调用方式与生产一致 (modes/common.rs)

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use abacus_cli::tui::cards::{AbacusCard, ExpertCard, LlmCard, UserCard};
use abacus_cli::tui::modes::common::{render_body_and_input, render_overlays, render_standard_frame};
use abacus_cli::tui::state::{AbacusMode, AppState};

fn build_demo_state() -> AppState {
    use abacus_ui_kit::CardCollapse;

    let mut state = AppState::new(AbacusMode::Clarify);
    state.model_name = "deepseek-v4".into();
    state.thinking_depth = "high".into();
    state.context_window = 200_000;
    state.turn_count = 3;

    // Card 1: UserCard — 用户输入
    let id1 = state.cards.alloc_id();
    state.cards.push_static(Box::new(UserCard::new(
        id1,
        "帮我用 Rust 写一个 HTTP server, 监听 :8080, 支持 GET/POST",
        "09:23",
    )));

    // Card 2: LlmCard (active, 流式中) — LLM 思考+回复
    let id2 = state.cards.alloc_id();
    let mut llm = LlmCard::new(id2, "deepseek-v4", "high");
    llm.append_thinking("用户要 HTTP server, 简单场景, 不需要框架。直接用 std::net::TcpListener, 手动解析 HTTP/1.1 协议。");
    llm.append_reply("下面是一个用纯 Rust 标准库写的 HTTP server, 支持 GET/POST:\n\n```rust\nuse std::net::TcpListener;\n```");
    state.cards.push_active(Box::new(llm));

    // Card 3: AbacusCard (折叠态) — 工具调用
    let id3 = state.cards.alloc_id();
    let mut abacus = AbacusCard::new(id3, "fs_edit");
    use abacus_cli::tui::state::{EventLevel, TraceEvent, TraceKind, ToolStatus};
    abacus.push_event(TraceEvent {
        id: 1,
        time: "09:24".into(),
        category: "tool".into(),
        level: EventLevel::Info,
        kind: TraceKind::ToolCall {
            name: "fs_edit".into(),
            args: r#"{"path": "src/main.rs"}"#.into(),
            output: Some("OK".into()),
            status: ToolStatus::Success,
        },
        duration_ms: Some(12),
    });
    abacus.push_event(TraceEvent {
        id: 2,
        time: "09:24".into(),
        category: "tool".into(),
        level: EventLevel::Info,
        kind: TraceKind::Generic { content: "已重写 main.rs".into() },
        duration_ms: None,
    });
    state.cards.push_static(Box::new(abacus));
    state.cards.set_collapse(id3, CardCollapse::Collapsed);

    // Card 4: ExpertCard — Meeting 专家
    let id4 = state.cards.alloc_id();
    let mut expert = ExpertCard::new(id4, "Dr. Smith", "anthropic-opus");
    expert.append_reply("作为系统架构专家, 我建议:\n1. 使用 tokio 异步运行时\n2. hyper / axum 框架\n3. 错误处理用 anyhow");
    state.cards.push_static(Box::new(expert));

    state
}

fn main() {
    let state = build_demo_state();

    // 与生产 TUI 完全一致的渲染入口: 120 列 × 40 行
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|f| {
        // 1. render_standard_frame: 画顶栏 + 计算 body/input/status 区域
        // 该函数内部已调 components::render_top_bar + render_global_background
        let frame = match render_standard_frame(f, &state, 40) {
            Some(f) => f,
            None => return, // 终端太小, 走 warning 路径
        };

        // 2. render_body_and_input: 画消息流 (V42-B render_cards) + 看板 + 输入栏
        // 内部调 components::render_cards (V42-B 路径!) + render_panel + render_input_bar_focused
        let input_area = render_body_and_input(f, &state, &frame);

        // 3. render_overlays: 画 status bar + 弹窗层
        render_overlays(f, &state, input_area, frame.body, frame.status);
    }).unwrap();

    // 打印 buffer
    let buffer = terminal.backend().buffer();
    println!("╔══════════════════════════════════════════════════════════════════════════════════════════════════════════════════════════╗");
    println!("║ V42-B 消息流重构演示 — 完整复用项目内置渲染函数 (modes/common.rs 路径)                                              ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════════════════════════════════════════════════╝");
    println!();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
    println!();
    println!("=== V42-B 消息流重构演示 ===");
    println!();
    println!("【演示范围】仅消息流区域 (V40 messages.rs 1439 行 → V42-B CardStream)");
    println!("【其他区域】顶栏/看板/输入栏/仪表盘 完全复用项目内置渲染函数 (布局与生产一致)");
    println!();
    println!("【消息流数据】state.cards: CardStream 包含 4 张 Card:");
    println!("  1. UserCard         [09:23] Expanded    用户输入");
    println!("  2. LlmCard (active) [09:23] Streaming   LLM 思考+回复 (流式 shimmer)");
    println!("  3. AbacusCard       [09:24] Collapsed   工具调用 (fs_edit + Generic)");
    println!("  4. ExpertCard       [09:25] Expanded    Meeting 专家 Dr. Smith");
    println!();
    println!("【调用链】(与生产 modes/common.rs 完全一致)");
    println!("  render_standard_frame  → 顶栏 + 全局背景 + 计算区域");
    println!("  render_body_and_input  → V42-B render_cards + render_panel + render_input_bar_focused");
    println!("  render_overlays        → status bar + 弹窗层");
    println!();
    println!("【V42-B 升级】完成");
    println!("  - 删除 V40 messages.rs (1439 行) + msg_geometry.rs (245 行)");
    println!("  - 新增 cards/* (7 文件: card_stream + render + hit_test + writer + 4 Card 实现)");
    println!("  - 9 个 V40 字段标 #[deprecated] + V42-B helper 方法升级");
}
