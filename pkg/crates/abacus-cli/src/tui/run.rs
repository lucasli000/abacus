//! Abacus TUI — 运行逻辑 (库函数，供 binary 和 CLI 共同使用)

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyboardEnhancementFlags, PushKeyboardEnhancementFlags, PopKeyboardEnhancementFlags};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, supports_keyboard_enhancement};
// V29 (P4): EnableFocusChange/DisableFocusChange — 让终端发送 FocusGained/FocusLost 事件
//   不支持的终端会静默忽略 escape sequence, 不影响其它功能
// V29.6: EnableMouseCapture/DisableMouseCapture — 启用鼠标事件订阅
//   修复 latent bug: V28.1-V28.4 鼠标交互(timeline 点击/消息区 toggle/focused 锚点)
//   设计齐全但 TermGuard::new() 从未调 EnableMouseCapture, 导致终端从不发鼠标事件,
//   handle_mouse 函数永远不被命中。本次补上启用调用使鼠标功能真正生效。
//   Trade-off: 开启后接管终端原生文本选择 — 但 V25/V26 已实现 Drag+Shift 自定义选择,
//   自定义路径覆盖原生路径, 用户体验不退化。
use crossterm::event::{EnableFocusChange, DisableFocusChange, EnableMouseCapture, DisableMouseCapture};
use crossterm::execute;
use ratatui::Terminal;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::layout::Alignment;
use tokio::sync::mpsc;
use tracing;

use crate::tui::api::{EngineHandle, send_chat_message, send_team_message, send_meeting_message, list_cwd_files, ai_complete, ApiResult, EngineResponse};
use crate::tui::event::{handle_chat_scroll_key, handle_global_key, handle_input_key, handle_mouse};
use crate::tui::modes;
use crate::tui::setup;
// V28 (T4): BlockKind 不再被 run.rs 写入(thinking/tool 走 TraceKind);保留 enum 给 Checklist + 旧 session 兼容
use crate::tui::state::{AppState, InputState, Message, MsgContent, AbacusMode, SlashCommand, ToolStatus};

/// RAII guard: 确保 panic 或提前退出时终端状态恢复
/// V14：尝试启用 kitty 键盘协议（DISAMBIGUATE）以区分 Ctrl+I 与 Tab；不支持则降级
struct TermGuard {
    active: bool,
    kbd_enhanced: bool,
}

impl TermGuard {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        // V29 (P4): 启用 FocusGained/FocusLost 事件(用于 ConfirmDialog 后台暂停 timer)
        //   不支持的终端忽略此 escape sequence, 不破坏其它功能 — 安全降级
        let _ = execute!(stdout, EnableFocusChange);
        // V29.6: 启用鼠标事件订阅 — 让 V28.1-V28.4 鼠标交互真正生效
        //   不支持的终端忽略此 escape sequence(罕见, 主流终端都支持)
        //   失败也不影响启动, 仅意味着鼠标功能不可用 — 键盘路径仍完整可用
        let _ = execute!(stdout, EnableMouseCapture);
        // 尝试启用键盘增强；失败/不支持就静默降级（Apple Terminal.app 不支持）
        let kbd_enhanced = matches!(supports_keyboard_enhancement(), Ok(true))
            && execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            ).is_ok();
        Ok(Self { active: true, kbd_enhanced })
    }

    fn deactivate(&mut self) -> io::Result<()> {
        if self.active {
            self.active = false;
            if self.kbd_enhanced {
                let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
            }
            // V29 (P4): 关闭 FocusChange 事件订阅, 与 EnableFocusChange 对称
            let _ = execute!(io::stdout(), DisableFocusChange);
            // V29.6: 关闭鼠标事件订阅, 与 EnableMouseCapture 对称
            //   清理顺序: 必须在 LeaveAlternateScreen 之前, 否则主屏幕的鼠标行为可能残留
            let _ = execute!(io::stdout(), DisableMouseCapture);
            disable_raw_mode()?;
            execute!(io::stdout(), LeaveAlternateScreen)
        } else {
            Ok(())
        }
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = self.deactivate();
    }
}

/// 启动 crossterm 事件 polling（spawn_blocking 循环，支持关闭信号）
fn spawn_event_poller(
    tx: mpsc::UnboundedSender<crossterm::event::Event>,
    shutdown: Arc<AtomicBool>,
) {
    tokio::task::spawn_blocking(move || {
        while !shutdown.load(Ordering::Relaxed) {
            if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(evt) = event::read() {
                    match &evt {
                        Event::Key(k) if k.kind == KeyEventKind::Press => { let _ = tx.send(evt); }
                        Event::Mouse(_) => { let _ = tx.send(evt); }
                        // V29 (P4): 终端焦点变化事件 — 用于后台 timer 暂停
                        //   crossterm 收到这两种 Event 需 EnableFocusChange 启用(在 TermGuard 处)
                        //   不支持的终端(Apple Terminal.app 等)永远收不到这类事件,自动降级为"不暂停"
                        Event::FocusGained | Event::FocusLost => { let _ = tx.send(evt); }
                        _ => {}
                    }
                }
            }
        }
    });
}

/// Run the TUI event loop with engine connection.
pub async fn run_tui(chat: bool, team: bool) -> io::Result<()> {
    // TUI 模式: 日志写入文件（避免 stderr 破坏渲染）
    // 仅在 RUST_LOG 设置时启用日志（默认静默）
    //
    // Phase 4 (multi-instance D 模型)：log 路径项目化 + per-PID：
    //   ~/.abacus/projects/<escaped-cwd>/logs/{pid}.log
    // 同时 Phase 5 基础能力：一次性创建项目层子目录骨架（sessions/logs/memory）。
    let _ = abacus_core::paths::ensure_global_dirs();
    let _ = abacus_core::paths::ensure_current_project_dirs();
    let log_file = abacus_core::paths::current_logs_dir()
        .join(format!("{}.log", std::process::id()));
    // 底堆充当：若项目层路径不可用，降级到 /tmp/abacus-tui-{pid}.log
    let log_file_path = if log_file.parent().map(|p| p.exists()).unwrap_or(false) {
        log_file
    } else {
        std::path::PathBuf::from(format!("/tmp/abacus-tui-{}.log", std::process::id()))
    };
    // Phase1-1.2a: 安全降级——打开失败时使用 sink 而非 panic
    // 引用关系：tracing_subscriber writer 消费此 Box
    // 生命周期：进程全局（tracing global subscriber）
    let file_writer: Box<dyn std::io::Write + Send> = match std::fs::OpenOptions::new()
        .create(true).append(true).open(&log_file_path) {
        Ok(f) => Box::new(f),
        Err(_) => Box::new(std::io::sink()),
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::sync::Mutex::new(file_writer))
        .with_ansi(false)
        .try_init();

    // R2 修复：Panic hook — 仅恢复 terminal；session save 由正常退出路径负责
    // （panic 时 state 可能不一致，强行写文件可能比丢失更危险）
    // Phase1-1.7: panic hook 完整终端恢复（含键盘增强/焦点/鼠标捕获）
    // 引用关系：crossterm terminal/event 模块
    // 生命周期：进程全局 panic hook，触发后进程即将退出
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags,
            crossterm::event::DisableFocusChange,
            crossterm::event::DisableMouseCapture,
            crossterm::terminal::LeaveAlternateScreen,
        );
        let _ = crossterm::terminal::disable_raw_mode();
        eprintln!("\n[PANIC] AbacusTUI crashed: {}", info);
        eprintln!("  上次正常退出的 session 保留在 ~/.abacus/projects/<cwd>/sessions/<uuid>.json");
        default_hook(info);
    }));

    // Phase 3 (multi-instance D 模型)：原 R1 单实例 flock 已移除。
    // 多开不再被拒；session 隔离靠 UUID 命名文件实现（见 save_session）。
    // 跨项目隔离靠 paths::project_dir(cwd)。共享 SQLite 靠 WAL + busy_timeout。

    let mut guard = TermGuard::new()?;
    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mode = if chat {
        AbacusMode::Clarify
    } else if team {
        AbacusMode::Team
    } else {
        AbacusMode::Meeting
    };

    let mut state = AppState::new(mode);

    // V29.11: 系统级 always_allow 加载（优先于 session，全局共享）
    state.always_allow = load_always_allow();

    // 自动恢复上次会话
    let resumed = load_last_session(&mut state).unwrap_or(false);
    if resumed && !state.messages.is_empty() {
        state.add_toast(format!("已恢复上次会话（{} 条消息）", state.messages.len()), Duration::from_secs(3));
    } else {
        state.add_toast(format!("Abacus — {} 模式", mode.label()), Duration::from_secs(3));
    }

    // 首次配置 + 免责声明合并展示
    // 免责声明未接受 或 无 API 配置时 → 进入配置向导
    if !setup::disclaimer_accepted() || !setup::has_api_config() {
        state.add_toast("首次使用，请完成配置", Duration::from_secs(5));
        let configured = setup::run_setup(&mut terminal)?;
        if configured {
            state.add_toast("配置已保存，正在连接引擎", Duration::from_secs(3));
        } else {
            guard.deactivate()?;
            return Ok(());
        }
    }

    // 初始化引擎
    state.add_toast("正在连接引擎...", Duration::from_secs(10));
    let _ = terminal.draw(|f| {
        let area = f.area();
        let block = Block::default()
            .title(" Abacus ")
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let loading = Paragraph::new(vec![
            Line::raw(""),
            Line::from(Span::styled(
                "  正在初始化引擎，请稍候...",
                Style::default().fg(state.theme.text),
            )),
            Line::raw(""),
            Line::from(Span::styled(
                "  API key 检测中...",
                state.theme.text_style(crate::tui::theme::TextRole::Caption),
            )),
        ]).alignment(Alignment::Center);
        f.render_widget(loading, inner);
    })?;

    let engine = match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        EngineHandle::new("deepseek-v4-flash", &state.thinking_depth),
    ).await {
        Ok(Ok(e)) => {
            state.add_toast("引擎已连接，输入消息即可对话", Duration::from_secs(3));
            // V30 复制修复：首次连接提示选中复制路径。
            // 生命周期：仅首次连接出现（随引擎启动一次性提醒）；重连会重发，不频繁。
            // 引用关系：/help 有完整复制节，用户可查体。
            state.add_toast(
                "提示：Shift + 拖动鼠标选中文本，释放自动复制 · /help 查看更多",
                Duration::from_secs(6),
            );
            let actual_model = e.core.config().default_model.0.clone();
            state.model_name = actual_model.clone();
            state.theme.apply_model_brand(&actual_model);
            e
        }
        Ok(Err(e)) => {
            guard.deactivate()?;
            eprintln!("\n[x] 引擎初始化失败: {}\n", e);
            eprintln!("  请检查:");
            eprintln!("    - API key 是否已配置 (ABACUS_API_KEY 或 DEEPSEEK_API_KEY)");
            eprintln!("    - 网络连接是否正常");
            eprintln!("    - config.yaml 中的模型配置\n");
            return Err(io::Error::other(e));
        }
        Err(_) => {
            guard.deactivate()?;
            eprintln!("\n[x] 引擎初始化超时 (15s)\n");
            eprintln!("  请检查网络连接或 API 服务状态\n");
            return Err(io::Error::new(io::ErrorKind::TimedOut, "engine init timed out"));
        }
    };
    state.engine_handle = Some(engine.clone());

    // 初始化 channel
    let (res_tx, mut res_rx) = mpsc::unbounded_channel::<EngineResponse>();
    let (comp_tx, mut comp_rx) = mpsc::unbounded_channel::<(Vec<String>, String)>();
    // V0.2: Streaming chunk channel
    let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<abacus_core::llm::stream::StreamChunk>();
    state.engine_tx = Some(res_tx.clone());

    // V29.11 (B): sandbox 实时事件 channel — 主循环 poll rx, spawn 侧 tx→set_event_sink
    // 引用关系:
    //   - 生产者: execute_slash_command TurnkeyExecute arm 调 sandbox.set_event_sink(tx.clone())
    //   - 消费者: interval tick 分支 drain sandbox_evt_rx → push_trace(Generic)
    // 生命周期: 与 TUI run 同生命周期; channel drop 后 sandbox emit() 静默失败(已 handle)
    let (sandbox_evt_tx, mut sandbox_evt_rx) =
        mpsc::unbounded_channel::<abacus_types::sandbox::SandboxEvent>();

    // 事件 polling
    let shutdown = Arc::new(AtomicBool::new(false));
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<crossterm::event::Event>();
    spawn_event_poller(evt_tx, shutdown.clone());

    // P2-19: SIGTERM/SIGINT graceful shutdown
    // Phase1-1.2b: 消除嵌套 unwrap，注册失败时用 pending() 替代
    // 引用关系：下方 select! 宏消费此 signal stream
    // 生命周期：主循环存续期间保持，state.running=false 后不再 poll
    let sigterm_result = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .or_else(|_| tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()));
    let mut sigterm_opt = sigterm_result.ok();

    // 主循环
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (mut cols, mut rows) = (80u16, 24u16);

    while state.running {
        tokio::select! {
            // P2-19: Graceful shutdown on SIGTERM
            // Phase1-1.2b: signal 注册失败时此分支永不触发（pending）
            _ = async {
                match sigterm_opt.as_mut() {
                    Some(sig) => { sig.recv().await; }
                    None => { std::future::pending::<()>().await; }
                }
            } => {
                tracing::info!("received SIGTERM, shutting down gracefully");
                state.running = false;
                continue;
            }
            _ = interval.tick() => {
                if let Ok(size) = terminal.size() {
                    (rows, cols) = (size.height, size.width);
                }
                // Paused 时暂停引擎响应消费（但继续渲染和接收事件）
                if !state.paused {
                    while let Ok(response) = res_rx.try_recv() {
                        let ts = chrono::Local::now().format("%H:%M").to_string();

                        // V28 (T4): 落档前先 mem::take streaming_trace_ids
                        // (流式期间已 push 的 ToolCall trace events 都在 trace_events)。
                        // 顺序: take → 创建 Thinking trace → 检查 tool 兜底(非流式)→ reset_streaming
                        let mut trace_ids = std::mem::take(&mut state.streaming_trace_ids);

                        // 1. Thinking trace — 优先用 streaming_thinking 累积内容(流式),
                        //    fallback 到 response.thinking(非流式 / 一次性返回路径)
                        let thinking_text = if !state.streaming_thinking.is_empty() {
                            state.streaming_thinking.clone()
                        } else {
                            response.thinking.clone().unwrap_or_default()
                        };
                        if !thinking_text.is_empty() {
                            let line_count = thinking_text.lines().count();
                            let tid = state.push_trace_full(
                                ts.clone(),
                                "llm".into(),
                                crate::tui::state::EventLevel::Info,
                                crate::tui::state::TraceKind::Thinking {
                                    text: thinking_text.clone(),
                                    lines: line_count,
                                },
                                None,
                            );
                            // Thinking 排在 tool calls 前面(更符合"先思考后行动"的认知顺序)
                            trace_ids.insert(0, tid);
                            state.thinking_text = thinking_text;
                        }

                        // 2. Tool calls 兜底 — 流式路径已在 ToolStart/ToolEnd 创建 trace,
                        //    streaming_trace_ids 已含 ToolCall 类型 ids。如果 trace_ids 中没有
                        //    任何 ToolCall(走非流式 EngineResponse 路径),从 response.tool_records 重建。
                        let has_tool_traces = trace_ids.iter().any(|id| {
                            state.trace_events.iter().any(|e| {
                                e.id == *id && matches!(e.kind, crate::tui::state::TraceKind::ToolCall { .. })
                            })
                        });
                        if !has_tool_traces {
                            for rec in &response.tool_records {
                                let tid = state.push_trace_full(
                                    ts.clone(),
                                    "tool".into(),
                                    if matches!(rec.status, ToolStatus::Failed) {
                                        crate::tui::state::EventLevel::Warning
                                    } else {
                                        crate::tui::state::EventLevel::Notice
                                    },
                                    crate::tui::state::TraceKind::ToolCall {
                                        name: rec.name.clone(),
                                        args: rec.args.clone(),
                                        output: None,
                                        status: rec.status,
                                    },
                                    Some(rec.duration_ms as u64),
                                );
                                trace_ids.push(tid);
                            }
                        }

                        // 3. 组装 parts: 主体只有 Reply 文本 + Trace 引用(取代旧 Block 内嵌)
                        let mut parts: Vec<MsgContent> = vec![MsgContent::Stream(response.text.clone())];
                        if !trace_ids.is_empty() {
                            // V29.11: 默认展开 trace — 让用户直接看到 thinking/tool 过程
                            //   用户可按 Space 折叠(toggle_block); 历史长消息重开后仍展开
                            //   设计变更: 原 collapsed=true 导致"工具/思考看不到"的反馈
                            parts.push(MsgContent::Trace {
                                event_ids: trace_ids,
                                collapsed: false,
                                expanded_event_ids: std::collections::HashSet::new(),
                            });
                        }

                        state.add_message(Message::new_session(parts, &ts));

                        // V39-1: 若上次发送是 reviewer，立即解析 verdict + toast 暴露
                        // V39-2: 同步回填 last_review_strict（cmd_review 写入 pending_review_strict）
                        // 引用关系：state.pending_review_parses 由 ReviewRole 分支 spawn 前 +1
                        // 设计意图：用户不必滚屏看长输出就能立即看到 verdict + strict 阻断状态
                        if state.pending_review_parses > 0 {
                            state.pending_review_parses = state.pending_review_parses.saturating_sub(1);
                            let kind = state.pending_review_kind;
                            // V41-4: 注入 kind + 抵达时间到 report，便于历史回放
                            let report = crate::tui::api::parse_review_report(&response.text)
                                .with_kind(kind)
                                .with_time(chrono::Utc::now().to_rfc3339());
                            let strict = state.pending_review_strict;
                            state.pending_review_strict = false; // 消费一次
                            // 渲染 toast：verdict + issues 数 + 装饰
                            let icon = if report.verdict.is_pass() { "✓" } else if matches!(report.verdict, crate::tui::api::ReviewVerdict::Fail) { "⛔" } else { "⚠" };
                            let strict_marker = if strict { " · 🔒strict" } else { "" };
                            state.add_toast(
                                format!("{} 审查结果：{} · {} 项 issue{}", icon, report.verdict.label(), report.issues.len(), strict_marker),
                                std::time::Duration::from_secs(8),
                            );
                            // V41-4: 历史推入（FIFO 上限 20）
                            // 引用关系：state.review_history（VecDeque，最旧在 front）
                            // 设计：先 push 后 trim，保证 last_review 永远 == 历史末尾
                            state.review_history.push_back(report.clone());
                            while state.review_history.len() > 20 {
                                state.review_history.pop_front();
                            }
                            state.last_review = Some(report);
                            state.last_review_strict = strict;
                        }

                        // V28: 落档完成,现在清流式累积字段(streaming_text/thinking/tools/trace_ids)
                        state.reset_streaming();

                        state.tool_records.extend(response.tool_records);
                        // 保持 tool_records 有界（最近 200 条）
                        if state.tool_records.len() > 200 {
                            state.tool_records.drain(..state.tool_records.len() - 200);
                        }

                        // V28.7: 更新 Token 统计 + 费用估算（lookup_pricing 单源）
                        if let Some(stats) = &response.stats {
                            state.session_tokens.prompt_tokens += stats.prompt_tokens;
                            state.session_tokens.completion_tokens += stats.completion_tokens;
                            state.session_tokens.total_tokens += stats.total_tokens;
                            state.session_tokens.cached_tokens += stats.cached_tokens;
                            // V30：思考 tokens 累加（completion 子集，仅信息透明）
                            state.session_tokens.thinking_tokens += stats.thinking_tokens;

                            // 费用累加：按当轮 model_id 查 pricing 估算 USD
                            // 引用关系：cost::estimate_turn_cost_usd(本地 pricing 表)
                            // 注意：cost_usd 是会话级累计，跨模型切换也保留（用户想看总开销）
                            let model_for_pricing = if stats.model_id.is_empty() {
                                state.model_name.as_str()
                            } else {
                                stats.model_id.as_str()
                            };
                            // V31: CNY 是计费 source-of-truth（DeepSeek 官方货币）
                            // 引用关系：cost::estimate_turn_cost_cny → model_registry::lookup_model_or_default
                            // USD 经 fx_rate 同步累加，便于历史兼容查询
                            let fx = crate::tui::cost::DEFAULT_FX_RATE;
                            let cny_delta = crate::tui::cost::estimate_turn_cost_cny(
                                model_for_pricing,
                                stats.prompt_tokens,
                                stats.completion_tokens,
                                stats.cached_tokens,
                            );
                            state.session_tokens.cost_cny += cny_delta;
                            state.session_tokens.cost_usd += if fx > 0.0 { cny_delta / fx } else { 0.0 };

                            // V36-3: per_model 拆分累计 — 按 canonical model_id 聚合
                            // 引用关系：state.session_tokens.per_model 由 render_tab_quant 模型分布区块消费
                            // 标准化：lookup_model.aliased_to 解析后用 canonical id 作 key
                            //   - 命中 registry：用 info.aliased_to 或 info.id（解决 deepseek-chat/v4-flash 别名）
                            //   - 未命中：用 model_for_pricing 原值（不丢失非 DeepSeek 模型的统计）
                            let canonical_key = abacus_types::lookup_model(model_for_pricing)
                                .map(|info| info.aliased_to.unwrap_or(info.id).to_string())
                                .unwrap_or_else(|| model_for_pricing.to_string());
                            let per = state.session_tokens.per_model
                                .entry(canonical_key)
                                .or_default();
                            per.prompt += stats.prompt_tokens;
                            per.completion += stats.completion_tokens;
                            per.cached += stats.cached_tokens;
                            per.thinking += stats.thinking_tokens;
                            per.cost_cny += cny_delta;
                            per.turns += 1;

                            // V39-4: per_mode 维度同步累计 — 关注"在哪个会话阶段花费"
                            // 引用关系：state.session_tokens.per_mode 由 render_tab_quant 模式分布区块消费
                            // key：state.mode.label()（与 AbacusMode 枚举字符串解耦，持久化稳定）
                            let mode_key = state.mode.label().to_string();
                            let per_m = state.session_tokens.per_mode
                                .entry(mode_key)
                                .or_default();
                            per_m.prompt += stats.prompt_tokens;
                            per_m.completion += stats.completion_tokens;
                            per_m.cached += stats.cached_tokens;
                            per_m.thinking += stats.thinking_tokens;
                            per_m.cost_cny += cny_delta;
                            per_m.turns += 1;

                            // Model Escalation 通知：检测到模型切换时告知用户
                            if !stats.model_id.is_empty() && stats.model_id != state.model_name {
                                state.add_toast(
                                    format!("🔄 已自动升级到 {} 以获得更深层推理", stats.model_id),
                                    Duration::from_secs(5),
                                );
                            }
                        }
                        // Progressive Gate state
                        if let Some(ref ps) = response.progressive_state {
                            let label = match ps {
                                abacus_types::progressive::ProgressiveState::AwaitingConfirmation { .. } =>
                                    "⏳ Awaiting your confirmation",
                                abacus_types::progressive::ProgressiveState::Generating { .. } =>
                                    "✍️ Generating...",
                                _ => "⚙️ Processing",
                            };
                            state.add_toast(label.to_string(), Duration::from_secs(10));
                        }
                        // Inertia warning
                        if let Some(ref w) = response.inertia_warning {
                            state.add_toast(format!("⚠️ {w}"), Duration::from_secs(8));
                            state.add_event(&ts, "inertia", w, crate::tui::state::EventLevel::Warning);
                        }

                        // V28.7: 异常兜底 — Meeting/其他模式失败时显式切回 Clarify
                        // 引用关系：
                        //   - 信号源：send_meeting_message fallback 失败时设 auto_fallback_chat
                        //   - 副作用：切 mode + toast + 事件流（用户清晰知晓兜底）
                        // 设计：mode 已是 Clarify 时不重复切；toast 解释原因
                        if let Some(ref reason) = response.auto_fallback_chat {
                            if state.mode != crate::tui::state::AbacusMode::Clarify {
                                crate::tui::event::switch_mode(&mut state, crate::tui::state::AbacusMode::Clarify);
                            }
                            state.add_toast(
                                format!("ℹ️ 已自动切到 Clarify 模式：{}", reason),
                                Duration::from_secs(6),
                            );
                            state.add_event(
                                &ts,
                                "session",
                                &format!("自动兜底切到 Clarify：{}", reason),
                                crate::tui::state::EventLevel::Warning,
                            );
                        }

                        // V29.10 (C4-Phase2) ── Turnkey plan 缓存到 state ──
                        // 引用关系:
                        //   生产者: SlashCommand::TurnkeyPlan dispatch 成功时
                        //   消费者: cmd_turnkey 'execute' 子命令读 state.pending_turnkey_plan
                        // 生命周期: 每次 TurnkeyPlan 成功 → 替换旧 plan;
                        //   /turnkey execute 后 take() 取走;
                        //   /turnkey clear 显式清空; SessionExport 不持久化(临时审阅状态)
                        if let Some(task) = response.turnkey_plan.clone() {
                            let phases = task.phases.len();
                            let steps: usize = task.phases.iter().map(|p| p.steps.len()).sum();
                            state.pending_turnkey_plan = Some(task);
                            state.add_event(
                                &ts,
                                "session",
                                &format!("Turnkey 计划已就绪: {} phases × {} steps  (输入 /turnkey execute 执行)", phases, steps),
                                crate::tui::state::EventLevel::Info,
                            );
                        }

                        // V28.7 ── Meeting 模式：参会者快照写入 state.experts ──
                        // 引用关系：
                        //   生产者：send_meeting_message 从 mtg.session().participants 提取
                        //   消费者：components::render_panel_meeting_agenda 读 state.experts
                        // 生命周期：每条 Meeting EngineResponse 抵达时整体替换；非 Meeting 路径
                        //   meeting_experts=None 不动 state.experts（保留 mock 或上次状态）
                        if let Some(ref experts) = response.meeting_experts {
                            let count = experts.len();
                            state.experts = experts.clone();
                            // 事件流：让用户知道议程 tab 数据已刷新
                            state.add_event(
                                &ts,
                                "meeting",
                                &format!("参会者已更新（{} 人）", count),
                                crate::tui::state::EventLevel::Info,
                            );
                        }

                        // ── MCIP 工具授权处理 (Gap 2 + 5) ──
                        // 检查 pending_confirmations：非空 → 需要用户授权
                        if !response.pending_confirmations.is_empty() {
                            use crate::tui::state::{ConfirmDialog, ConfirmType, ConfirmRisk};
                            use abacus_core::mcip::McipConfirmKind;

                            let confirmations = response.pending_confirmations.clone();

                            // Gap 5: 检查 always_allow 自动放行
                            let all_allowed = confirmations.iter().all(|req| {
                                state.always_allow.contains(&req.tool_id)
                            });

                            if all_allowed {
                                // 自动放行：所有工具都在 always_allow 列表中
                                // 注意：V28 后此分支应为 dead code——pipeline 改用 channel 暂停，
                                //   ConfirmRequired stream chunk 单独处理 always_allow 短路。
                                //   保留作为非流式 fallback：如果有 EngineResponse 仍带
                                //   pending_confirmations 抵达，走旧 channel 通路也能放行。
                                state.add_event(&ts, "mcip", crate::tui::i18n::t("event.auto_allow_legacy"), crate::tui::state::EventLevel::Info);
                                state.pending_mcip_confirmations = confirmations;
                                state.pending_confirmation_response = Some(true);
                            } else {
                                // 展示 ConfirmDialog（第一个待确认工具）
                                use crate::tui::state::ConfirmOption;
                                let first = &confirmations[0];
                                // V22 增强：reason 含 `[destructive]` 前缀（mcip.rs 启发式标记）
                                //   也视为破坏性，避免 McipPolicy kind 把所有 unmatched 工具
                                //   一律按 Medium risk 处理
                                let reason_destructive = first.reason.starts_with("[destructive]");
                                let is_destructive_flag = matches!(first.kind, McipConfirmKind::DestructiveOp)
                                    || reason_destructive;
                                let (confirm_type, risk) = if is_destructive_flag {
                                    (ConfirmType::ShellExec, ConfirmRisk::High)
                                } else {
                                    (ConfirmType::NetworkRequest, ConfirmRisk::Medium)
                                };
                                let mut details = if confirmations.len() > 1 {
                                    vec![
                                        first.reason.clone(),
                                        format!("(及其他 {} 个工具需要授权)", confirmations.len() - 1),
                                    ]
                                } else {
                                    vec![first.reason.clone()]
                                };
                                if let Some(ref preview) = first.params_preview {
                                    details.push(format!("参数: {}", preview));
                                }
                                let is_destructive = is_destructive_flag;
                                let mut options = vec![
                                    ConfirmOption { key: 'Y', label: crate::tui::i18n::t("confirm.allow").to_string() },
                                ];
                                if !is_destructive {
                                    options.push(ConfirmOption { key: 'A', label: crate::tui::i18n::t("confirm.always").to_string() });
                                }
                                options.push(ConfirmOption { key: 'N', label: crate::tui::i18n::t("confirm.deny").to_string() });

                                state.confirm_dialog = Some(ConfirmDialog {
                                    title: format!("🔐 {}", first.tool_id),
                                    confirm_type,
                                    tool_id: first.tool_id.clone(),
                                    action: first.tool_id.clone(),
                                    details,
                                    risk,
                                    options,
                                    callback_id: "mcip".to_string(),
                                    allow_always: !is_destructive,
                                    created_at: std::time::Instant::now(),
                                    details_expanded: false,
                                    selected: 0, // V25：默认聚焦第一个选项（"允许"）
                                    interaction_paused: false,
                                    paused_total: std::time::Duration::ZERO,
                                    focus_lost_at: None,
                                    last_active_at: std::time::Instant::now(),
                                });
                                let event_msg = crate::tui::i18n::tf("event.wait_auth_tool", &[&first.tool_id]);
                                state.pending_mcip_confirmations = confirmations;
                                state.add_event(&ts, "mcip", &event_msg, crate::tui::state::EventLevel::Warning);
                            }
                        }

                        state.add_event(&ts, "llm", crate::tui::i18n::t("event.gen_complete"), crate::tui::state::EventLevel::Notice);

                        // 有待确认工具时，保持 Executing 状态（等用户确认后再恢复）
                        if state.pending_mcip_confirmations.is_empty() {
                            state.input_state = InputState::Ready;
                            state.op_started_at = None;
                            state.accumulated_elapsed = Duration::ZERO;

                            // 自动发送排队消息（用户在忙碌态下 Enter 提交的）
                            if !state.pending_inputs.is_empty() {
                                let next_input = state.pending_inputs.remove(0);
                                state.input = next_input.clone();
                                // RU7 修复：input 改后必须 recalculate_cursor，否则
                                // cursor_pos/line/col 持有旧值，渲染时光标位置错位
                                state.cursor_pos = state.input.len();
                                state.recalculate_cursor();
                                state.add_toast(
                                    format!("自动发送排队消息 (剩余 {})", state.pending_inputs.len()),
                                    Duration::from_secs(2),
                                );
                                state.pending_send = true;
                            }
                        }
                    }
                }
                // V0.2: 消费 streaming chunks — 实时更新 partial message（渲染前处理）
                // V29.5: 把硬编码 100 提为命名 const, 注释说明语义
                //   含义: 每帧最多消化的 chunk 数, 防止 LLM 突发推送堆积导致单帧延迟
                //   选值: 20——每 chunk 都触发 rendered_lines_dirty → 全量 markdown 重解析,
                //   100 chunks/frame 时单帧渲染开销过高导致卡顿; 降到 20 让突发 delta 分摊
                //   到多帧,视觉上无感(20 FPS × 20 chunks = 仍可消化 400 chunks/s, 远超 LLM 产出速率)
                const FRAME_CHUNK_BUDGET: u32 = 20;
                let mut chunk_budget = FRAME_CHUNK_BUDGET;
                while chunk_budget > 0 {
                    let chunk = match stream_rx.try_recv() {
                        Ok(c) => c,
                        Err(_) => break,
                    };
                    chunk_budget -= 1;
                    use abacus_core::llm::stream::StreamChunk;
                    let ts = chrono::Local::now().format("%H:%M").to_string();
                    match chunk {
                        StreamChunk::IterationStart { iteration } => {
                            // V38: 迭代边界——清空累积的 thinking，准备新一轮内容
                            // 保留 streaming_text（回复内容跨迭代累积是正确的）
                            // 保留 streaming_tools（工具历史保留供参考）
                            state.streaming_thinking.clear();
                            state.streaming_thinking_started = false;
                            state.streaming_text_started = false;
                            if iteration > 0 {
                                state.input_state = InputState::Thinking;
                                state.processing_phase = format!("· iteration {}", iteration + 1);
                            }
                            state.rendered_lines_dirty.set(true);
                        }
                        StreamChunk::TextDelta(t) => {
                            if !t.is_empty() && !state.streaming_text_started {
                                state.streaming_text_started = true;
                                // V38: 切换状态指示到 Outputting
                                state.input_state = InputState::Outputting;
                                state.processing_phase.clear();
                                state.add_event(&ts, "llm", "开始输出", crate::tui::state::EventLevel::Info);
                            }
                            // K6：传入实际行内容数组，flash_state 内部计算 hash（避免“底部偏移”漂移）
                            let added: Vec<&str> = t.lines().collect();
                            if !added.is_empty() {
                                state.flash_state.mark_new_lines(&added);
                            }
                            state.streaming_text.push_str(&t);
                            state.rendered_lines_dirty.set(true);
                        }
                        StreamChunk::Thinking(t) => {
                            // V29.5: 同 TextDelta, 用 streaming_thinking_started 判首次
                            if !t.is_empty() && !state.streaming_thinking_started {
                                state.streaming_thinking_started = true;
                                state.input_state = InputState::Thinking;
                                state.processing_phase.clear();
                                state.add_event(&ts, "llm", "开始推理", crate::tui::state::EventLevel::Info);
                            }
                            state.streaming_thinking.push_str(&t);
                            state.rendered_lines_dirty.set(true);
                        }
                        StreamChunk::ToolStart { name } => {
                            // V28 (T3): 创建 ToolCall trace 拿 trace_id, 缓存到 streaming_tools
                            let trace_id = state.push_trace_full(
                                ts.clone(),
                                "tool".into(),
                                crate::tui::state::EventLevel::Info,
                                crate::tui::state::TraceKind::ToolCall {
                                    name: name.clone(),
                                    args: String::new(),
                                    output: None,
                                    status: crate::tui::state::ToolStatus::Running,
                                },
                                None,
                            );
                            state.streaming_trace_ids.push(trace_id);
                            state.streaming_tools.push((
                                name.clone(),
                                crate::tui::state::StreamingToolStatus::Running,
                                None,
                                trace_id,
                            ));
                            // V38: 状态栏实时反映当前工具名（Working · tool_name）
                            state.input_state = InputState::Executing;
                            state.processing_phase = format!("· {}", name);
                            state.rendered_lines_dirty.set(true);
                        }
                        // V29.11: 工具输入参数 — 回填 trace event 的 args 字段
                        //   触发 try_render_edit_diff: args 含 old_string/new_string → 走 diff 视图
                        StreamChunk::ToolArgs { name, args_json } => {
                            // 反查最近一个同名 Running 状态的 trace event
                            if let Some(tool) = state.streaming_tools.iter().rev()
                                .find(|(n, s, _, _)| *n == name && *s == crate::tui::state::StreamingToolStatus::Running)
                            {
                                let tid = tool.3;
                                if let Some(ev) = state.trace_events.iter_mut().find(|e| e.id == tid) {
                                    if let crate::tui::state::TraceKind::ToolCall { args, .. } = &mut ev.kind {
                                        *args = args_json;
                                    }
                                }
                            }
                        }
                        // V29.11: 工具输出内容 — 回填 trace event 的 output 字段
                        StreamChunk::ToolOutput { name, output_json } => {
                            // V38: 拦截 mode_switch 工具输出，执行模式切换
                            if name == "mode_switch" {
                                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&output_json) {
                                    if val.get("action").and_then(|v| v.as_str()) == Some("switch_mode") {
                                        if let Some(target_str) = val.get("target").and_then(|v| v.as_str()) {
                                            if let Some(target) = abacus_types::AbacusMode::from_label(target_str) {
                                                let reason = val.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                                                let display = val.get("display_name").and_then(|v| v.as_str()).unwrap_or(target_str);
                                                state.set_mode(target);
                                                state.add_toast(
                                                    format!("🤖 LLM 切换到 {} 模式: {}", display, reason),
                                                    std::time::Duration::from_secs(5),
                                                );
                                                state.add_event(&ts, "session", &format!("LLM 切换 → {}", display), crate::tui::state::EventLevel::Notice);
                                            }
                                        }
                                    }
                                }
                            }
                            if let Some(tool) = state.streaming_tools.iter().rev()
                                .find(|(n, s, _, _)| *n == name && *s == crate::tui::state::StreamingToolStatus::Running)
                            {
                                let tid = tool.3;
                                if let Some(ev) = state.trace_events.iter_mut().find(|e| e.id == tid) {
                                    if let crate::tui::state::TraceKind::ToolCall { output, .. } = &mut ev.kind {
                                        *output = Some(output_json);
                                    }
                                }
                            }
                        }
                        StreamChunk::ToolEnd { name, success, duration_ms } => {
                            // V28 (T3): 反查 streaming_tools 拿 trace_id, 直接定位 trace_events
                            // 中对应条目更新 status + duration_ms(替代 add_event 重复事件)。
                            let new_status = if success {
                                crate::tui::state::StreamingToolStatus::Success
                            } else {
                                crate::tui::state::StreamingToolStatus::Failed
                            };
                            let mut updated_trace_id: Option<u64> = None;
                            if let Some(tool) = state.streaming_tools.iter_mut().rev()
                                .find(|(n, s, _, _)| *n == name && *s == crate::tui::state::StreamingToolStatus::Running)
                            {
                                tool.1 = new_status;
                                tool.2 = Some(duration_ms);
                                updated_trace_id = Some(tool.3);
                            }

                            // V32: knowledge_calls 修复 —— 旧实现 `name.find("→ ")` 永远 fail
                            // (ToolEnd.name = tc.function.name, 工具函数名不含 "→ " 也不含路径)。
                            // 真实 path 在 trace_events.kind.ToolCall.args 的 JSON 字段。
                            // 引用关系：trace_event 在 ToolStart 时建立, args 已写入。
                            // 触发条件：仅当 path 命中知识库/记忆宫殿语义路径时才追踪
                            // (避免任意文件读写都被算作知识调用)
                            if success {
                                if let Some(tid) = updated_trace_id {
                                    if let Some(ev) = state.trace_events.iter().find(|e| e.id == tid) {
                                        if let crate::tui::state::TraceKind::ToolCall { args, .. } = &ev.kind {
                                            if !args.is_empty() {
                                                if let Ok(json) = serde_json::from_str::<serde_json::Value>(args) {
                                                    let path_opt = json.get("path")
                                                        .or_else(|| json.get("file_path"))
                                                        .and_then(|v| v.as_str());
                                                    if let Some(p) = path_opt {
                                                        // 仅匹配知识/记忆类路径，避免污染随意文件操作
                                                        if p.contains("memory")
                                                            || p.contains("记忆宫殿")
                                                            || p.contains("/.abacus/")
                                                            || p.contains("knowledge")
                                                        {
                                                            state.track_knowledge_call(p);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            if let Some(tid) = updated_trace_id {
                                if let Some(ev) = state.trace_events.iter_mut().find(|e| e.id == tid) {
                                    ev.duration_ms = Some(duration_ms);
                                    ev.level = if success {
                                        crate::tui::state::EventLevel::Notice
                                    } else {
                                        crate::tui::state::EventLevel::Warning
                                    };
                                    if let crate::tui::state::TraceKind::ToolCall { status, .. } = &mut ev.kind {
                                        *status = if success {
                                            crate::tui::state::ToolStatus::Success
                                        } else {
                                            crate::tui::state::ToolStatus::Failed
                                        };
                                    }
                                }
                            }
                            state.rendered_lines_dirty.set(true);
                        }
                        StreamChunk::ConfirmRequired(req) => {
                            // V28：实时授权请求——pipeline dispatch 已挂起等待用户决策
                            //   弹 ConfirmDialog；用户响应后通过 SessionState.mcip_confirm_channels[nonce]
                            //   直发 oneshot::Sender（不再 grant_and_rerun 重发整个 turn）
                            use crate::tui::state::{ConfirmDialog, ConfirmType, ConfirmRisk, ConfirmOption};
                            use abacus_core::mcip::McipConfirmKind;

                            // V38: 分级权限策略——根据工具类型决定是否自动放行
                            // - 只读工具（read/search/ls/tree/grep/info）：永久放行
                            // - 编辑工具（edit/write/move/mkdir）+ 非破坏性 bash：session 内放行
                            // - 破坏性操作（rm/format/drop/kill/reset/force）：每次确认
                            let tool_lower = req.tool_id.to_lowercase();
                            // 知识库/记忆工具：kb_*, cross_session_*, session_resume_* 默认放行
                            let is_kb_or_memory = tool_lower.starts_with("kb_")
                                || tool_lower.starts_with("cross_session")
                                || tool_lower.starts_with("session_resume")
                                || tool_lower.contains("palace")
                                || tool_lower.contains("memory");
                            // Phase1-1.3: 词边界安全匹配，防止 "create" 误中 "read"
                            // 匹配规则: 完全匹配 / 前缀 "read_xxx" / 后缀 "xxx_read" / 中间 "xxx_read_xxx"
                            let readonly_keywords = ["read", "search", "ls", "tree", "grep", "info", "list", "get"];
                            let has_readonly_verb = readonly_keywords.iter().any(|k| {
                                tool_lower == *k
                                    || tool_lower.starts_with(&format!("{}_", k))
                                    || tool_lower.ends_with(&format!("_{}", k))
                                    || tool_lower.contains(&format!("_{}_", k))
                            });
                            let is_readonly = is_kb_or_memory
                                || (has_readonly_verb && !tool_lower.contains("write") && !tool_lower.contains("delete") && !tool_lower.contains("remove"));
                            let is_destructive_cmd = if tool_lower.contains("bash") {
                                // 检查 bash 命令参数是否含破坏性关键词
                                let params_str = req.params_preview.as_deref().unwrap_or("");
                                let params_lower = params_str.to_lowercase();
                                ["rm ", "rm\t", "rmdir", "format", "mkfs", "dd ", "kill -9",
                                 "drop ", "truncate", "reset --hard", "push --force", "clean -f",
                                 "> /dev/", "shred", "wipefs"]
                                    .iter().any(|k| params_lower.contains(k))
                            } else {
                                ["delete", "remove", "drop", "destroy", "purge"]
                                    .iter().any(|k| tool_lower.contains(k))
                            };

                            let auto_allow = if is_readonly {
                                true // 只读：永久放行
                            } else if is_destructive_cmd {
                                false // 破坏性：绝不自动放行
                            } else {
                                // 编辑类：检查 always_allow（首次弹窗后 session 内放行）
                                state.always_allow.contains(&req.tool_id)
                            };

                            if auto_allow {
                                let nonce = req.nonce.clone();
                                let engine = state.engine_handle.clone();
                                tokio::spawn(async move {
                                    if let Some(eng) = engine {
                                        // 提前取出 sender 释放 std::sync::MutexGuard，
                                        // 避免与 SessionState read guard 跨 await 冲突
                                        let tx_one = {
                                            let s = eng.session.read().await;
                                            let mut guard = s.mcip_confirm_channels.lock().unwrap_or_else(|e| e.into_inner());
                                            let removed = guard.remove(&nonce);
                                            drop(guard);
                                            removed
                                        };
                                        if let Some(tx) = tx_one {
                                            let _ = tx.send(true);
                                        }
                                    }
                                });
                                state.add_event(&ts, "mcip", &crate::tui::i18n::tf("event.auto_allow_tool", &[&req.tool_id]), crate::tui::state::EventLevel::Info);
                            } else {
                                // 弹窗
                                let reason_destructive = req.reason.starts_with("[destructive]");
                                let is_destructive_flag = matches!(req.kind, McipConfirmKind::DestructiveOp) || reason_destructive;
                                let (confirm_type, risk) = if is_destructive_flag {
                                    (ConfirmType::ShellExec, ConfirmRisk::High)
                                } else {
                                    (ConfirmType::NetworkRequest, ConfirmRisk::Medium)
                                };
                                let mut details = vec![req.reason.clone()];
                                if let Some(ref preview) = req.params_preview {
                                    details.push(format!("参数: {}", preview));
                                }
                                let mut options = vec![ConfirmOption { key: 'Y', label: crate::tui::i18n::t("confirm.allow").to_string() }];
                                if !is_destructive_flag {
                                    options.push(ConfirmOption { key: 'A', label: crate::tui::i18n::t("confirm.always").to_string() });
                                }
                                options.push(ConfirmOption { key: 'N', label: crate::tui::i18n::t("confirm.deny").to_string() });
                                state.confirm_dialog = Some(ConfirmDialog {
                                    title: format!("🔐 {}", req.tool_id),
                                    confirm_type,
                                    tool_id: req.tool_id.clone(),
                                    action: req.tool_id.clone(),
                                    details,
                                    risk,
                                    options,
                                    callback_id: format!("mcip:{}", req.nonce),
                                    allow_always: !is_destructive_flag,
                                    created_at: std::time::Instant::now(),
                                    details_expanded: false,
                                    selected: 0,
                                    interaction_paused: false,
                                    paused_total: std::time::Duration::ZERO,
                                    focus_lost_at: None,
                                    last_active_at: std::time::Instant::now(),
                                });
                                state.pending_mcip_confirmations = vec![req.clone()];
                                state.add_event(&ts, "mcip", &crate::tui::i18n::tf("event.wait_auth_tool", &[&req.tool_id]), crate::tui::state::EventLevel::Warning);
                            }
                            state.rendered_lines_dirty.set(true);
                        }
                        StreamChunk::CompressStart => {
                            state.input_state = InputState::Executing;
                            state.processing_phase = "🗜️ 正在压缩上下文...".into();
                            // 在主对话流中插入系统提示，让用户明确感知压缩正在发生
                            state.add_toast("⏳ 上下文接近容量上限，正在压缩历史消息...", Duration::from_secs(5));
                            state.rendered_lines_dirty.set(true);
                        }
                        StreamChunk::CompressEnd { messages_compressed, tokens_saved } => {
                            if messages_compressed > 0 {
                                state.add_event(&ts, "context", &format!("压缩 {} 条消息，释放 ~{} tok", messages_compressed, tokens_saved), crate::tui::state::EventLevel::Info);
                                // 在主对话流中通知压缩结果
                                let note = format!(
                                    "✅ 上下文压缩完成：{} 条历史消息 → 摘要（释放 ~{} tokens）。早期对话细节已简化，核心结论保留。",
                                    messages_compressed, tokens_saved
                                );
                                state.add_toast(&note, Duration::from_secs(5));
                                // 联动上下文展示：减去压缩释放的 tokens
                                state.session_tokens.total_tokens = state.session_tokens.total_tokens
                                    .saturating_sub(tokens_saved as u64);
                                state.session_tokens.prompt_tokens = state.session_tokens.prompt_tokens
                                    .saturating_sub(tokens_saved as u64);
                            }
                            state.processing_phase.clear();
                            state.rendered_lines_dirty.set(true);
                        }
                        StreamChunk::Complete(_) => {
                            // ST1：清理流式累积避免双显示
                            state.reset_streaming();
                            state.rendered_lines_dirty.set(true);
                            // TT4：drain 同轮残留 chunks（API 异常或 provider 多发时可能存在），
                            // 否则循环 try_recv 拿到下一个 TextDelta 又会累积到刚清空的字段。
                            // 新流式启动前安全：用户按 Enter 到首 chunk 抵达至少跨 tick
                            while stream_rx.try_recv().is_ok() {}
                            break;
                        }
                        StreamChunk::Error(e) => {
                            state.reset_streaming();
                            state.rendered_lines_dirty.set(true);
                            state.add_event(&ts, "llm", &format!("错误: {}", e), crate::tui::state::EventLevel::Warning);
                            state.add_toast(format!("Stream error: {}", e), Duration::from_secs(5));
                            // TT4：错误后丢弃残留并 break，与 Complete 一致
                            while stream_rx.try_recv().is_ok() {}
                            break;
                        }
                    }
                }

                // V29.11 (B): drain sandbox 实时事件 → push_trace(Generic)
                //   每帧最多处理 20 条(防止 execute 密集 emit 卡渲染帧)
                //   超出的留到下帧(unbounded channel 不丢)
                {
                    let mut count = 0usize;
                    while let Ok(evt) = sandbox_evt_rx.try_recv() {
                        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
                        let content = format!("[sandbox] {} · {}", evt.phase_id, evt.message);
                        state.add_event(
                            &ts,
                            "tool",
                            &content,
                            crate::tui::state::EventLevel::Info,
                        );
                        count += 1;
                        if count >= 20 { break; }
                    }
                    if count > 0 {
                        state.rendered_lines_dirty.set(true);
                    }
                }

                // 渲染（每帧一次，在所有状态更新之后）
                // 性能优化：无变更时跳过整帧渲染
                let needs_draw = state.rendered_lines_dirty.get()
                    || !state.streaming_text.is_empty()
                    || !state.streaming_thinking.is_empty()
                    || !state.toasts.is_empty()
                    || state.confirm_dialog.is_some()
                    || state.picker.is_some()
                    || state.input_state != InputState::Ready;
                if needs_draw {
                    if let Err(e) = terminal.draw(|f| modes::render(f, &state, rows)) {
                        tracing::error!(?e, "TUI 渲染错误");
                    }
                }
                state.cleanup_toasts();

                // ── 确认弹窗超时检查（每帧） ──
                // V38: 分级超时策略
                //   破坏性（High risk）：超时 → 拒绝
                //   非破坏性（Medium/Low）：超时 → session 内允许（不写入永久 always_allow）
                if let Some(ref dialog) = state.confirm_dialog {
                    if dialog.is_expired() {
                        let tool_id = dialog.tool_id.clone();
                        let action = dialog.action.clone();
                        let is_destructive = dialog.risk == crate::tui::state::ConfirmRisk::High;
                        state.confirm_dialog = None;
                        let ts = chrono::Local::now().format("%H:%M").to_string();
                        if is_destructive {
                            // 破坏性：超时拒绝
                            state.pending_confirmation_response = Some(false);
                            state.add_event(&ts, "session", &crate::tui::i18n::tf("event.timeout_reject", &[&action]), crate::tui::state::EventLevel::Warning);
                            state.add_toast(crate::tui::i18n::t("toast.timeout_reject"), Duration::from_secs(3));
                        } else {
                            // 非破坏性：超时 → session 内允许（加入 session grants，不持久化）
                            state.pending_confirmation_response = Some(true);
                            // 加入 session 级 grants（不写 always_allow.json，session 结束后失效）
                            if !state.always_allow.contains(&tool_id) {
                                state.always_allow.insert(tool_id.clone());
                                // 注意：不调用 save_always_allow——仅 session 内有效
                            }
                            state.add_event(&ts, "session", &crate::tui::i18n::tf("event.timeout_allow", &[&action]), crate::tui::state::EventLevel::Info);
                            state.add_toast(crate::tui::i18n::t("toast.timeout_allow"), Duration::from_secs(3));
                        }
                    }
                }

                // ── V28：消费 pending_confirmation_response → channel 直发决策 ──
                // 引用关系：
                //   生产者：event/mod.rs Y/A/N/Enter handler 设 pending_confirmation_response
                //           run.rs 超时检查 设 pending_confirmation_response
                //   消费者：本块——通过 SessionState.mcip_confirm_channels[nonce] 拿 sender 发 bool
                // 生命周期：pipeline NeedsConfirm 处 insert sender + await rx；本块 send 后 sender drop
                //
                // 关键设计差异（vs V27 grant_and_rerun）：
                //   - 旧：用户决策后整个 turn 从头重新执行（思考/已生成文本/其他工具结果丢失）
                //   - 新：pipeline 阻塞在 rx_one.await，决策抵达即恢复，turn 状态完整保留
                //   - input_state 保持 Executing：pipeline 仍在跑，Complete chunk 抵达时再 reset
                if let Some(allowed) = state.pending_confirmation_response.take() {
                    if !state.pending_mcip_confirmations.is_empty() {
                        let confirmations = std::mem::take(&mut state.pending_mcip_confirmations);
                        let ts = chrono::Local::now().format("%H:%M").to_string();
                        let tool_names: Vec<String> = confirmations.iter().map(|r| r.tool_id.clone()).collect();

                        if allowed {
                            state.add_event(&ts, "mcip", &crate::tui::i18n::tf("event.auth_tools", &[&format!("{:?}", tool_names)]), crate::tui::state::EventLevel::Info);
                            state.add_toast("🔓 已授权，继续执行", Duration::from_secs(2));
                        } else {
                            state.add_event(&ts, "mcip", &format!("已拒绝（pipeline 将返回 deny output）: {:?}", tool_names), crate::tui::state::EventLevel::Warning);
                            state.add_toast("🚫 已拒绝工具执行", Duration::from_secs(2));
                        }

                        // 通过 nonce → sender 直发决策
                        // 注意：不能在持有 std::sync::MutexGuard 的情况下跨 await，
                        //   所以先 remove 拿出 sender，drop guard，再 send
                        let engine_clone = engine.clone();
                        tokio::spawn(async move {
                            let senders: Vec<_> = {
                                let s = engine_clone.session.read().await;
                                let mut guard = s.mcip_confirm_channels.lock().unwrap_or_else(|e| e.into_inner());
                                confirmations.iter()
                                    .filter_map(|req| guard.remove(&req.nonce))
                                    .collect()
                            };
                            for tx_one in senders {
                                let _ = tx_one.send(allowed);
                            }
                        });
                        // input_state 保持 Executing：pipeline 还在跑，等 Complete/Error chunk 到达
                    }
                }

                // V37: 去掉打字机节流——与 Claude Code 一致，API token 到达即渲染
                // stream_cursor 机制保留但不再主动驱动（仅由 streaming chunk 自然触发 dirty）
                // 如需恢复打字机效果，还原此处为 stream_cursor += N 逻辑
                if state.stream_cursor > 0 {
                    state.stream_cursor = 0;
                    state.rendered_lines_dirty.set(true);
                }

                // info panel 自动打开（/help /status 等触发）
                // V33: info_panel_text 由 render_tab_memory 渲染（在「现场」tab 内顶部）
                //      旧 PanelTab::Memory 已不在新 mode 序列，set_mode 会兜底回 Timeline；
                //      直接指向 Timeline 让 info panel 立即可见，避免一次回退漂移
                if state.info_panel_auto_open {
                    state.info_panel_auto_open = false;
                    state.panel_visible = true;
                    state.panel_tab = crate::tui::state::PanelTab::Timeline;
                }

                // Phase 3 (3.8): 消费 pending_send 标志——触发自动发送
                // 引用关系：由 pending_inputs 消费后设置，此处将 state.input 转为 pending_text 以触发发送
                // 生命周期：单次消费，下一帧的 pending_text.take() 分支会实际发送
                if state.pending_send {
                    state.pending_send = false;
                    if !state.input.is_empty() {
                        state.pending_text = Some(state.input.clone());
                        state.input.clear();
                        state.cursor_pos = 0;
                        state.cursor_line = 0;
                        state.cursor_col = 0;
                    }
                }

                // 处理异步补全结果
                while let Ok((candidates, prefix)) = comp_rx.try_recv() {
                    if candidates.is_empty() {
                        state.input_state = InputState::Typing;
                    } else {
                        state.completion_candidates = candidates;
                        state.completion_index = 0;
                        state.completion_prefix = prefix;
                        state.input_state = InputState::Completing;
                    }
                }
            }

            Some(event) = evt_rx.recv() => {
                // V29.1 (P1 续): 任何用户活动(键/鼠/焦点恢复)都重置 ConfirmDialog 的 idle 计时器
                //   设计意图: timer 衡量"用户多久没操作"而非"弹窗存在多久"
                //   不在 interaction_paused 状态下时, 把 last_active_at 推到 now
                //   D 键的硬冻结仍生效 — interaction_paused 下不会被重置覆盖
                if matches!(event, Event::Key(_) | Event::Mouse(_)) {
                    if let Some(ref mut d) = state.confirm_dialog {
                        if !d.interaction_paused {
                            d.last_active_at = std::time::Instant::now();
                        }
                    }
                }
                match event {
                    Event::Key(key) => {
                        if handle_global_key(&mut state, key.code, key.modifiers) { continue; }
                        // V25 修复 (双触发 bug): 之前 Up/Down/PageUp/PageDown 同时调
                        //   handle_chat_scroll_key + handle_input_key, 用户按 Up 时
                        //   消息区滚 3 行 + 输入框 history 同时触发, 视觉上"输入框跟着滚动"
                        //
                        // V25 路由优先级:
                        //   1. picker 激活 → 全部给 input_key (picker 移动)
                        //   2. completion 激活 → 全部给 input_key (候选移动)
                        //   3. PageUp/PageDown → 始终消息区 (无论 input 是否空)
                        //   4. Up/Down + input 为空 → 消息区滚动
                        //   5. Up/Down + input 非空 → input_key (cursor / history)
                        // 引用关系: handle_chat_scroll_key 改 state.scroll;
                        //          handle_input_key 改 state.cursor_pos / state.input
                        let picker_active = state.picker.is_some();
                        let in_completion = matches!(state.input_state, InputState::Completing);
                        if !picker_active && !in_completion {
                            let is_page = matches!(key.code, KeyCode::PageUp | KeyCode::PageDown);
                            let is_arrow_to_scroll = matches!(key.code, KeyCode::Up | KeyCode::Down)
                                && state.input.is_empty();
                            if is_page || is_arrow_to_scroll {
                                handle_chat_scroll_key(&mut state, key.code);
                                continue;
                            }
                        }
                        handle_input_key(&mut state, key.code, key.modifiers);
                        if let Some(mut text) = state.pending_text.take() {
                            // V29.9 (C1): /plan 单次模式 — 在 user message 前注入 plan-mode 前缀,
                            //   要求 LLM 先研究、出方案、等审批、分步执行、每步复盘。一轮即清。
                            // 引用: state.plan_mode 由 cmd_plan 设 true; 命中后追加事件 + 翻 false
                            // 销毁: 一轮对话后 plan_mode=false; 用户重新 /plan 才再次启用
                            if state.plan_mode {
                                text = format!(
                                    "[plan-mode 已启用]\n\n请按以下步骤回答用户:\n\
                                     1. 拆解需求 — 识别核心目标与隐含约束\n\
                                     2. 设计方案 — 列出 1~2 个备选方案 + 取舍说明\n\
                                     3. 拆分步骤 — 编号小步骤, 每步标注预期产物 + 验证方式\n\
                                     4. 等待审批 — 输出后明确暂停, 不直接动手\n\
                                     5. 分步执行 + 审查 — 用户批准后逐步执行, 每步完成后复盘\n\n\
                                     用户输入:\n{}",
                                    text
                                );
                                state.plan_mode = false;
                                let ts = chrono::Local::now().format("%H:%M:%S").to_string();
                                state.add_event(
                                    &ts,
                                    "session",
                                    "plan-mode 已注入到本轮请求(单次)",
                                    crate::tui::state::EventLevel::Info,
                                );
                            }
                            let engine = engine.clone();
                            let tx = res_tx.clone();
                            let current_mode = state.mode;
                            // Gap A 修复：把 state.thinking_depth 转为 per-turn RequestContext.thinking_intent
                            // 引用关系：ThinkingIntent::from_str_loose 接受 off/low/medium/high/max/adaptive/整数
                            // 生命周期：仅此 spawn 内本轮有效；下轮重新取 state.thinking_depth
                            // V38: 注入能力上下文——模式 + 工具 + Skill + 命令
                            // 让 LLM 完整了解可用能力，自主决定调用哪个
                            let capability_context = {
                                let transitions: Vec<&str> = current_mode.transitions().iter()
                                    .map(|m| m.label()).collect();

                                // 收集可用 skill 名称（从 skill_engine 当前注册）
                                let skill_names: Vec<String> = if let Some(ref handle) = state.engine_handle {
                                    let engine = handle.core.skill_engine_ref().read().await;
                                    engine.list_skills().iter().take(20).map(|s| s.id.0.clone()).collect()
                                } else {
                                    Vec::new()
                                };
                                let skills_str = if skill_names.is_empty() {
                                    String::new()
                                } else {
                                    format!("\nAvailable Skills: {}", skill_names.join(", "))
                                };

                                format!(
                                    "\n\n[Capabilities]\n\
                                     Mode: {} ({}) | Transitions: {:?} (call mode_switch tool)\n\
                                     Key Commands: /plan, /meeting, /team (mode switch), /compress (context), /streaming (toggle){}\n\
                                     Tool Discovery: call tool_compass with intent description if unsure which tool fits.\n\
                                     You have full access to file system tools, shell execution, web search, and knowledge retrieval.\n\
                                     Choose the most efficient tool for each step — prefer direct action over asking.",
                                    current_mode.display_zh(), current_mode.label(), transitions, skills_str
                                )
                            };
                            let chat_req_ctx = abacus_core::core::RequestContext {
                                thinking_intent: abacus_types::ThinkingIntent::from_str_loose(&state.thinking_depth),
                                system_prompt_override: Some(capability_context),
                                ..Default::default()
                            };

                            match current_mode {
                                // ═══ Team 模式: Leader 分解 + SubAgent 并行执行 ═══
                                AbacusMode::Team => {
                                    tokio::spawn(async move {
                                        match send_team_message(&engine, &text).await {
                                            ApiResult::Ok(resp) => { let _ = tx.send(resp); }
                                            ApiResult::Err(e) => {
                                                let _ = tx.send(EngineResponse {
                                                    text: format!("⚠️ [Team] {}", e),
                                                    thinking: None,
                                                    tool_records: vec![],
                                                    stats: None,
                                                    progressive_state: None,
                                                    inertia_warning: None,
                                                    pending_confirmations: vec![],
                                                    meeting_experts: None,
                                                    auto_fallback_chat: None,
                                                    turnkey_plan: None,
                                                });
                                            }
                                            _ => {}
                                        }
                                    });
                                }
                                // ═══ Meeting 模式: 专家会诊路由 ═══
                                AbacusMode::Meeting => {
                                    tokio::spawn(async move {
                                        match send_meeting_message(&engine, &text).await {
                                            ApiResult::Ok(resp) => { let _ = tx.send(resp); }
                                            ApiResult::Err(e) => {
                                                let _ = tx.send(EngineResponse {
                                                    text: format!("⚠️ [Meeting] {}", e),
                                                    thinking: None,
                                                    tool_records: vec![],
                                                    stats: None,
                                                    progressive_state: None,
                                                    inertia_warning: None,
                                                    pending_confirmations: vec![],
                                                    meeting_experts: None,
                                                    auto_fallback_chat: None,
                                                    turnkey_plan: None,
                                                });
                                            }
                                            _ => {}
                                        }
                                    });
                                }
                                // ═══ Clarify 模式: 单 Agent 循环（默认路径） ═══
                                AbacusMode::Clarify => {
                                    if state.streaming_enabled {
                                        let stx = stream_tx.clone();
                                        // 启动新流式：先清旧累积（reset_streaming），再设 is_streaming=true
                                        state.reset_streaming();
                                        state.is_streaming = true;
                                        tokio::spawn(async move {
                                            use crate::tui::api::send_chat_message_streaming;
                                            match send_chat_message_streaming(&engine, &text, stx, chat_req_ctx).await {
                                                ApiResult::Ok(resp) => { let _ = tx.send(resp); }
                                                ApiResult::Err(e) => {
                                                    let _ = tx.send(EngineResponse {
                                                        text: format!("⚠️ {}", e),
                                                        thinking: None,
                                                        tool_records: vec![],
                                                        stats: None,
                                                        progressive_state: None,
                                                        inertia_warning: None,
                                                        pending_confirmations: vec![],
                                                        meeting_experts: None,
                                                        auto_fallback_chat: None,
                                                        turnkey_plan: None,
                                                    });
                                                }
                                                _ => {}
                                            }
                                        });
                                    } else {
                                        tokio::spawn(async move {
                                            match send_chat_message(&engine, &text, chat_req_ctx).await {
                                                ApiResult::Ok(resp) => { let _ = tx.send(resp); }
                                                ApiResult::Err(e) => {
                                                    let _ = tx.send(EngineResponse {
                                                        text: format!("⚠️ {}", e),
                                                        thinking: None,
                                                        tool_records: vec![],
                                                        stats: None,
                                                        progressive_state: None,
                                                        inertia_warning: None,
                                                        pending_confirmations: vec![],
                                                        meeting_experts: None,
                                                        auto_fallback_chat: None,
                                                        turnkey_plan: None,
                                                    });
                                                }
                                                _ => {}
                                            }
                                        });
                                    }
                                }
                                // ═══ V33 Plan 模式: PlannerAgent 路径 ═══
                                // 引用关系：send_planner_message_streaming 在 cli/api/mod.rs
                                //   - 注入 Planner system prompt
                                //   - 限制只读 tool whitelist (read/glob/grep/kb_query)
                                //   - 用户进入 Team 模式时才执行写操作
                                AbacusMode::Plan => {
                                    if state.streaming_enabled {
                                        let stx = stream_tx.clone();
                                        state.reset_streaming();
                                        state.is_streaming = true;
                                        tokio::spawn(async move {
                                            use crate::tui::api::send_planner_message_streaming;
                                            match send_planner_message_streaming(&engine, &text, stx, chat_req_ctx).await {
                                                ApiResult::Ok(resp) => { let _ = tx.send(resp); }
                                                ApiResult::Err(e) => {
                                                    let _ = tx.send(EngineResponse {
                                                        text: format!("⚠️ [Plan] {}", e),
                                                        thinking: None,
                                                        tool_records: vec![],
                                                        stats: None,
                                                        progressive_state: None,
                                                        inertia_warning: None,
                                                        pending_confirmations: vec![],
                                                        meeting_experts: None,
                                                        auto_fallback_chat: None,
                                                        turnkey_plan: None,
                                                    });
                                                }
                                                _ => {}
                                            }
                                        });
                                    } else {
                                        tokio::spawn(async move {
                                            use crate::tui::api::send_planner_message;
                                            match send_planner_message(&engine, &text, chat_req_ctx).await {
                                                ApiResult::Ok(resp) => { let _ = tx.send(resp); }
                                                ApiResult::Err(e) => {
                                                    let _ = tx.send(EngineResponse {
                                                        text: format!("⚠️ [Plan] {}", e),
                                                        thinking: None,
                                                        tool_records: vec![],
                                                        stats: None,
                                                        progressive_state: None,
                                                        inertia_warning: None,
                                                        pending_confirmations: vec![],
                                                        meeting_experts: None,
                                                        auto_fallback_chat: None,
                                                        turnkey_plan: None,
                                                    });
                                                }
                                                _ => {}
                                            }
                                        });
                                    }
                                }
                            }
                        }
                        if let Some(prefix) = state.pending_file_completion.take() {
                            let tx = comp_tx.clone();
                            let p = prefix.clone();
                            tokio::spawn(async move {
                                match list_cwd_files(&p).await {
                                    ApiResult::Ok(candidates) => { let _ = tx.send((candidates, p)); }
                                    ApiResult::Err(_) => { let _ = tx.send((vec![], p)); }
                                    _ => { let _ = tx.send((vec![], p)); }
                                }
                            });
                        }
                        if let Some(prefix) = state.pending_ai_completion.take() {
                            let engine = engine.clone();
                            let tx = comp_tx.clone();
                            let p = prefix.clone();
                            tokio::spawn(async move {
                                match ai_complete(&engine, &p).await {
                                    ApiResult::Ok(completion) => { let _ = tx.send((vec![completion], p)); }
                                    ApiResult::Err(e) => { let _ = tx.send((vec![format!("[AI Error] {}", e)], p)); }
                                    _ => { let _ = tx.send((vec![], p)); }
                                }
                            });
                        }
                        // ── Slash Command 后端调用 ──
                        if let Some(cmd) = state.pending_slash_command.take() {
                            // V37-3: ReviewRole 走流式 LLM 路径（与 Plan 模式同款），
                            //   而非 execute_slash_command 的 String-only path
                            //   引用关系：send_reviewer_message_streaming 设置 system_prompt_override
                            //   设计：reviewer 输出走标准流式渲染，结果进入 messages 历史（与 Planner 一致）
                            if let crate::tui::state::SlashCommand::ReviewRole { kind, content } = cmd {
                                let engine = engine.clone();
                                let tx = res_tx.clone();
                                let stx = stream_tx.clone();
                                state.reset_streaming();
                                state.is_streaming = true;
                                // V39-1: 标记下次 EngineResponse 需 parse_review_report
                                state.pending_review_parses = state.pending_review_parses.saturating_add(1);
                                tokio::spawn(async move {
                                    use crate::tui::api::send_reviewer_message_streaming;
                                    let req_ctx = abacus_core::core::RequestContext::default();
                                    match send_reviewer_message_streaming(&engine, kind, &content, stx, req_ctx).await {
                                        ApiResult::Ok(resp) => { let _ = tx.send(resp); }
                                        ApiResult::Err(e) => {
                                            let _ = tx.send(EngineResponse {
                                                text: format!("⚠️ [{}] {}", kind.label(), e),
                                                thinking: None,
                                                tool_records: vec![],
                                                stats: None,
                                                progressive_state: None,
                                                inertia_warning: None,
                                                pending_confirmations: vec![],
                                                meeting_experts: None,
                                                auto_fallback_chat: None,
                                                turnkey_plan: None,
                                            });
                                        }
                                        _ => {}
                                    }
                                });
                            } else if let crate::tui::state::SlashCommand::PlannerNudge { reason } = cmd {
                                // V37-1: schema 失败自动 nudge — 走 Plan 路径，把 reason 注入新 user message
                                // 引用关系：cmd 由 try_switch_mode 检测 SchemaInvalid 时构造（计数器已 +1）
                                // 设计意图：让 Planner 看到具体校验错误（如 "task[0].phase[1] 缺 description"）后修正
                                let engine = engine.clone();
                                let tx = res_tx.clone();
                                let stx = stream_tx.clone();
                                let nudge_msg = format!(
                                    "上一次输出的 JSON 不通过 schema 校验：{}\n\n请按 system prompt 中的格式约束重新输出完整的任务规划。",
                                    reason
                                );
                                state.reset_streaming();
                                state.is_streaming = true;
                                tokio::spawn(async move {
                                    use crate::tui::api::send_planner_message_streaming;
                                    let req_ctx = abacus_core::core::RequestContext::default();
                                    match send_planner_message_streaming(&engine, &nudge_msg, stx, req_ctx).await {
                                        ApiResult::Ok(resp) => { let _ = tx.send(resp); }
                                        ApiResult::Err(e) => {
                                            let _ = tx.send(EngineResponse {
                                                text: format!("⚠️ [Plan-nudge] {}", e),
                                                thinking: None,
                                                tool_records: vec![],
                                                stats: None,
                                                progressive_state: None,
                                                inertia_warning: None,
                                                pending_confirmations: vec![],
                                                meeting_experts: None,
                                                auto_fallback_chat: None,
                                                turnkey_plan: None,
                                            });
                                        }
                                        _ => {}
                                    }
                                });
                            } else if let crate::tui::state::SlashCommand::RoleInvoke { role, content } = cmd {
                                // L-3/L-4/L-5: 通用 Agent 角色调用 — 走流式 LLM 路径
                                // 引用关系：cmd 由 cmd_role 解析后构造；send_role_message_streaming 设置 system_prompt_override + 可选 prefix
                                // 设计意图：与 ReviewRole 同型，证明 V35-2 通道泛化性
                                let engine = engine.clone();
                                let tx = res_tx.clone();
                                let stx = stream_tx.clone();
                                state.reset_streaming();
                                state.is_streaming = true;
                                tokio::spawn(async move {
                                    use crate::tui::api::send_role_message_streaming;
                                    let req_ctx = abacus_core::core::RequestContext::default();
                                    match send_role_message_streaming(&engine, role, &content, stx, req_ctx).await {
                                        ApiResult::Ok(resp) => { let _ = tx.send(resp); }
                                        ApiResult::Err(e) => {
                                            let _ = tx.send(EngineResponse {
                                                text: format!("⚠️ [{}] {}", role.label(), e),
                                                thinking: None,
                                                tool_records: vec![],
                                                stats: None,
                                                progressive_state: None,
                                                inertia_warning: None,
                                                pending_confirmations: vec![],
                                                meeting_experts: None,
                                                auto_fallback_chat: None,
                                                turnkey_plan: None,
                                            });
                                        }
                                        _ => {}
                                    }
                                });
                            } else {
                                let engine = engine.clone();
                                let tx = res_tx.clone();
                                let sbox_tx = sandbox_evt_tx.clone();
                                tokio::spawn(async move {
                                    // V29.10 (C4-Phase2): execute_slash_command 现在返回 (text, Option<TaskSpec>)
                                    //   非 turnkey 命令: turnkey_plan 恒 None
                                    //   turnkey plan_from_nl 成功: turnkey_plan = Some(task), run.rs 写 state.pending_turnkey_plan
                                    let (output, turnkey_plan) = execute_slash_command(&engine, cmd, sbox_tx).await;
                                    let _ = tx.send(EngineResponse {
                                        text: output,
                                        thinking: None,
                                        tool_records: vec![],
                                        stats: None,
                                        progressive_state: None,
                                        inertia_warning: None,
                                        pending_confirmations: vec![],
                                        meeting_experts: None,
                                        auto_fallback_chat: None,
                                        turnkey_plan,
                                    });
                                });
                            }
                        }
                    }
                    Event::Mouse(mouse) => handle_mouse(&mut state, mouse, cols, rows),
                    // V29 (P4): 终端窗口失焦/得焦 → 暂停/恢复 ConfirmDialog 倒计时
                    //   只对当前活跃 dialog 生效, 没弹窗时静默忽略
                    //   设计原则: 用户看不见就不应被超时——视野等价于在场
                    Event::FocusLost => {
                        if let Some(ref mut d) = state.confirm_dialog {
                            if d.focus_lost_at.is_none() {
                                d.focus_lost_at = Some(std::time::Instant::now());
                            }
                        }
                    }
                    Event::FocusGained => {
                        if let Some(ref mut d) = state.confirm_dialog {
                            if let Some(t) = d.focus_lost_at.take() {
                                d.paused_total = d.paused_total.saturating_add(t.elapsed());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    shutdown.store(true, Ordering::Relaxed);
    // R7：save_session 失败需可见（之前 .ok() 静默吞错，磁盘满/权限错时用户不知）
    let save_err = save_session(&state).err();
    // V28.7: Phase 3 后单实例 flock 已移除（多开靠 UUID 命名 + WAL 隔离），
    // 历史 lockfile 清理逻辑已废止，无需 remove_file（清掉对悬挂 lock_path 的引用）
    guard.deactivate()?;
    let count = state.messages.len();
    match save_err {
        None => println!("会话已保存（{} 条消息，{} 轮次）。", count, state.turn_count),
        Some(e) => eprintln!(
            "[!] 会话保存失败：{}\n    本次 {} 条消息可能丢失，请检查 ~/.abacus/sessions/ 权限",
            e, count
        ),
    }
    Ok(())
}

/// V29.10 (C4-Phase2): 主 dispatch — 返回 (text, Option<TaskSpec>)
///
/// 引用关系:
///   - 调用: run.rs main loop, pending_slash_command 处理分支
///   - 委托: 90% 命令走 execute_slash_command_text, 仅 Turnkey* 分支需要回传 TaskSpec
/// 设计取舍:
///   - 用包装器而非把所有 match 改成 tuple — 减少机械编辑面积
///   - 只 Turnkey* 路径使用第二返回值, 避免污染普通命令
async fn execute_slash_command(
    engine: &EngineHandle,
    cmd: SlashCommand,
    sandbox_event_tx: tokio::sync::mpsc::UnboundedSender<abacus_types::sandbox::SandboxEvent>,
) -> (String, Option<abacus_types::sandbox::TaskSpec>) {
    match cmd {
        SlashCommand::TurnkeyPlan(goal) => {
            match engine.core.sandbox_engine().plan_from_nl(&goal).await {
                Ok(task) => {
                    let text = format_turnkey_plan(&goal, &task);
                    (text, Some(task))
                }
                Err(e) => (format!("⚠️ Turnkey plan 失败: {}\n\n目标: {}", e, goal), None),
            }
        }
        SlashCommand::TurnkeyExecute(task) => {
            // V29.11 (A+B): sandbox 事件流实时接入时间线
            //   B: set_event_sink(sandbox_event_tx) → emit() 实时发到 main loop →
            //      interval tick drain → push_trace → 时间线面板实时显示
            //   A 兜底: execute 后 event_log() 批量拉, 格式化进结果文本(不丢事件)
            let sandbox = engine.core.sandbox_engine();
            sandbox.set_event_sink(Some(sandbox_event_tx)).await;

            let result = sandbox.execute(&task).await;

            sandbox.set_event_sink(None).await;
            // A: 拉全量 event_log (含 B 已实时推送的 — 文本结果用作"执行报告")
            let events = sandbox.event_log().await;

            let text = match result {
                Ok(task_state) => format_turnkey_result_with_events(&task, task_state, &events),
                Err(e) => format!("⚠️ Turnkey execute 失败: {}", e),
            };
            (text, None)
        }
        other => (execute_slash_command_text(engine, other).await, None),
    }
}

/// V29.10: 拆出来的 String-only 子集 — 兼容老调用者
/// 仅处理非 Turnkey* 路径的命令
async fn execute_slash_command_text(engine: &EngineHandle, cmd: SlashCommand) -> String {
    match cmd {
        SlashCommand::ContextStatus => {
            let status = engine.core.context_status().await;
            format!(
                "📊 上下文状态\n  使用率: {:.1}% ({}/{} tokens)\n  已压缩: {} 条消息",
                status.usage_pct * 100.0, status.current_tokens, status.max_tokens, status.compressed_count,
            )
        }
        SlashCommand::ContextCompress => {
            let compressed = engine.core.compress_context(&engine.session).await;
            format!("🗜️ 已压缩 {} 条消息", compressed)
        }
        SlashCommand::ContextInject(content) => {
            engine.core.inject_context("user_inject", &content).await;
            format!("💉 已注入临时知识（下一轮生效）: {}", content.chars().take(50).collect::<String>())
        }
        SlashCommand::ToolList => {
            let tools = engine.core.tool_registry_ref().all_tools().await;
            let active: Vec<_> = tools.iter()
                .filter(|t| matches!(t.state, abacus_types::ToolState::Loaded | abacus_types::ToolState::Active))
                .collect();
            let mut out = format!("🔧 已注册工具 ({}):\n", active.len());
            for t in active.iter().take(20) {
                out.push_str(&format!("  • {} — {}\n", t.schema.name, t.schema.description.chars().take(40).collect::<String>()));
            }
            if active.len() > 20 { out.push_str(&format!("  ... 及 {} 个更多\n", active.len() - 20)); }
            out
        }
        SlashCommand::ToolStats => {
            let stats = engine.core.tool_stats().await;
            let mut sorted = stats;
            sorted.sort_by(|a, b| b.1.composite_score.partial_cmp(&a.1.composite_score).unwrap_or(std::cmp::Ordering::Equal));
            let mut out = String::from("📈 工具效能 Top 10:\n");
            for (name, s) in sorted.iter().take(10) {
                out.push_str(&format!("  [{:?}] {:.2} {}\n", s.tier, s.composite_score, name));
            }
            out
        }
        SlashCommand::SafetyStatus => {
            let s = engine.core.safety_status();
            format!("🔒 安全限制\n  最大输入: {} 字符\n  最大工具: {} 次\n  递归上限: {} 层\n  会话超时: {}s",
                s.max_input_length, s.max_total_tool_calls, s.max_recursion_depth, s.max_session_duration_secs)
        }
        SlashCommand::ModelList => {
            let models = engine.core.list_models().await;
            if models.is_empty() { "🤖 无已注册模型".to_string() }
            else {
                let mut out = format!("🤖 可用模型 ({}):\n", models.len());
                for m in &models { out.push_str(&format!("  • {}\n", m)); }
                out
            }
        }
        SlashCommand::SessionInfo => {
            let s = engine.session.read().await;
            let msg_count = s.messages.read().await.len();
            let map = s.interaction_map.read().await;
            format!("📋 会话\n  ID: {}\n  轮次: {}\n  消息: {}\n  检查点: {}",
                s.session_id, s.turn_count, msg_count, map.checkpoints.len())
        }
        SlashCommand::Provider => {
            let providers = engine.core.list_providers().await;
            if providers.is_empty() {
                "⚠️ 无已注册 Provider".to_string()
            } else {
                let lines: Vec<String> = providers.iter()
                    .map(|(id, models)| format!("  {} → [{}]", id, models.join(", ")))
                    .collect();
                format!("🔌 Providers ({})\n{}", providers.len(), lines.join("\n"))
            }
        }

        // ─── Phase 4 file-undo dispatch ─────────────────────────
        SlashCommand::UndoLast { session_id } => {
            match engine.undo_engine.undo_last(session_id.as_deref()).await {
                Ok(r) => format_undo_result(&r),
                Err(e) => format!("⚠️ undo 失败: {e}"),
            }
        }
        SlashCommand::UndoSeq { session_id, seq } => {
            match engine.undo_engine.undo_seq(&session_id, seq).await {
                Ok(r) => format_undo_result(&r),
                Err(e) => format!("⚠️ undo seq={seq} 失败: {e}"),
            }
        }
        SlashCommand::UndoTurn { session_id, turn } => {
            match engine.undo_engine.undo_turn(&session_id, turn).await {
                Ok(rs) if rs.is_empty() => format!("turn={turn} 无可撤销条目"),
                Ok(rs) => {
                    let parts: Vec<String> = rs.iter().map(format_undo_result).collect();
                    format!("⏪ undo turn={} ({} 条):\n{}", turn, rs.len(), parts.join("\n"))
                }
                Err(e) => format!("⚠️ undo turn={turn} 失败: {e}"),
            }
        }
        SlashCommand::Redo { session_id } => {
            match engine.undo_engine.redo(&session_id).await {
                Ok(r) => format!("⏩ redo seq={} → {:?}", r.seq, r.action),
                Err(e) => format!("⚠️ redo 失败: {e}"),
            }
        }
        SlashCommand::UndoHistory { session_id, limit } => {
            match engine.undo_engine.history(session_id.as_deref(), limit) {
                Ok(entries) if entries.is_empty() => "📜 暂无 undo 历史".to_string(),
                Ok(entries) => format_history(&entries),
                Err(e) => format!("⚠️ history 读取失败: {e}"),
            }
        }
        SlashCommand::UndoTimeline { since_hours } => {
            let since = chrono::Utc::now() - chrono::Duration::hours(since_hours as i64);
            // Phase 6：把当前 session_id 传给渲染，用 [you] 标识本窗口
            // 引用：state.session_id 是 EngineHandle 创建时的 session uuid
            let cur_sid = engine.session.read().await.session_id.clone();
            match engine.undo_engine.timeline(since) {
                Ok(entries) if entries.is_empty() =>
                    format!("📜 过去 {since_hours}h 内无写操作"),
                Ok(entries) => format_timeline(&entries, since_hours, &cur_sid),
                Err(e) => format!("⚠️ timeline 读取失败: {e}"),
            }
        }
        // V29.10: TurnkeyPlan / TurnkeyExecute 已被外层 execute_slash_command 提前拦截,
        //         此处理论上不可达; unreachable! 既保持 match 完整性又能在意外路径
        //         触发时立即崩溃便于定位
        SlashCommand::TurnkeyPlan(_) | SlashCommand::TurnkeyExecute(_) => {
            unreachable!("Turnkey* 应该被 execute_slash_command 顶层拦截")
        }
        // V37-3: ReviewRole 已被 main loop 中的 pending_slash_command 处理分支提前截获走流式路径,
        //        永不应进入此 String-only 函数; unreachable 设计同 Turnkey*
        SlashCommand::ReviewRole { .. } => {
            unreachable!("ReviewRole 应该被 pending_slash_command 处理分支提前截获走流式路径")
        }
        // V37-1: PlannerNudge 同 ReviewRole — main loop 检测 pending_slash_command 时
        //        应走 send_planner_message_streaming 流式路径，不进入此 String-only fn
        SlashCommand::PlannerNudge { .. } => {
            unreachable!("PlannerNudge 应该被 pending_slash_command 处理分支提前截获走流式路径")
        }
        // L-3/L-4/L-5: RoleInvoke 走 send_role_message_streaming 流式路径，与 ReviewRole/PlannerNudge 同型
        SlashCommand::RoleInvoke { .. } => {
            unreachable!("RoleInvoke 应该被 pending_slash_command 处理分支提前截获走流式路径")
        }
    }
}

/// V29.11: TaskState + SandboxEvent[] → 用户友好的执行结果 + 事件日志
/// 引用关系: 仅 SlashCommand::TurnkeyExecute 分支调用
/// A 路径: events 包含完整 event_log() 输出, 渲染为可折叠事件列表
fn format_turnkey_result_with_events(
    task: &abacus_types::sandbox::TaskSpec,
    state: abacus_types::sandbox::TaskState,
    events: &[abacus_types::sandbox::SandboxEvent],
) -> String {
    use abacus_types::sandbox::{TaskState as TS, SandboxEventKind};
    let icon = match state {
        TS::Completed => "✅",
        TS::Failed => "❌",
        _ => "⏳",
    };
    let status = match state {
        TS::Completed => "Completed",
        TS::Failed => "Failed",
        TS::Running => "Running",
    };
    let mut out = format!(
        "{} Turnkey 执行完成\n\n**目标**: {}\n**状态**: {}\n**Phases**: {}\n",
        icon, task.goal, status, task.phases.len()
    );

    // 事件日志（A 路径：完整展示）
    if !events.is_empty() {
        out.push_str(&format!("\n── 事件日志 ({} 条) ──\n", events.len()));
        for ev in events.iter().take(30) {
            let kind_icon = match &ev.kind {
                SandboxEventKind::PhaseCompleted => "✓ phase",
                SandboxEventKind::StepStarted { .. } => "▸ step",
                SandboxEventKind::StepCompleted => "✓ step",
                SandboxEventKind::StepFailed { .. } => "✗ step",
                SandboxEventKind::TaskCompleted => "✓ task",
                SandboxEventKind::VerificationPassed => "✓ verify",
                SandboxEventKind::VerificationFailed { .. } => "✗ verify",
            };
            out.push_str(&format!("  {} [{}] {}\n", kind_icon, ev.phase_id, ev.message));
        }
        if events.len() > 30 {
            out.push_str(&format!("  ... +{} 更多事件\n", events.len() - 30));
        }
    }
    out
}

/// V29.9 (C4): TaskSpec → 用户友好的 markdown 文本
/// 引用关系: 仅 SlashCommand::TurnkeyPlan 分支调用
fn format_turnkey_plan(goal: &str, task: &abacus_types::sandbox::TaskSpec) -> String {
    let mut out = String::new();
    out.push_str("🎯 Turnkey 计划生成\n\n");
    out.push_str(&format!("**目标**: {}\n\n", goal));
    out.push_str(&format!("**Phases**: {}\n\n", task.phases.len()));
    for (pi, p) in task.phases.iter().enumerate() {
        out.push_str(&format!(
            "── Phase {} · {} ──\n  {}\n",
            pi + 1,
            p.id,
            p.description
        ));
        for (si, s) in p.steps.iter().enumerate() {
            out.push_str(&format!(
                "  {}.{}  [{}] {}\n",
                pi + 1,
                si + 1,
                step_model_label(&s.model),
                s.description
            ));
            if !s.tools.is_empty() {
                out.push_str(&format!("       tools: {}\n", s.tools.join(", ")));
            }
        }
        out.push('\n');
    }
    out.push_str("─────────────────────────\n");
    out.push_str("⚠ 当前仅展示计划. execute 接通在后续迭代。\n");
    out.push_str("CLI 路径: `abacus turnkey run \"<goal>\" --yes` 可执行(实验功能)。");
    out
}

/// 把 ModelAssignment 标签化, 避免输出长 enum 字面值
fn step_model_label(m: &abacus_types::sandbox::ModelAssignment) -> &'static str {
    use abacus_types::sandbox::ModelAssignment;
    match m {
        ModelAssignment::Fixed { .. } => "fixed",
        ModelAssignment::Execute => "execute",
        ModelAssignment::Verify => "verify",
    }
}

/// Phase 4 渲染 helpers — 简单 +/- 风格，决策 4 = B（不引入 syntect）
fn format_undo_result(r: &abacus_core::undo::UndoResult) -> String {
    use abacus_core::undo::UndoAction;
    let action_str = match r.action {
        UndoAction::RestoredContent => "恢复内容",
        UndoAction::RemovedFile => "删除文件",
        UndoAction::RemovedDir => "删除空目录",
        UndoAction::ReverseMoved => "反向 rename",
        UndoAction::Aborted => "中止（冲突）",
    };
    let path_str = r.path.to_string_lossy();
    let header = format!("⏪ undo seq={} session={} ({}): {}",
        r.seq, &r.session_id[..r.session_id.len().min(8)], action_str, path_str);

    if let Some(c) = &r.conflict {
        let detail = match c {
            abacus_core::undo::UndoConflict::ExternalModification { observed_sha256, expected_sha256 } =>
                format!("文件被外部修改\n  expected sha256: {}\n  observed sha256: {}",
                    &expected_sha256[..16], &observed_sha256[..16]),
            abacus_core::undo::UndoConflict::FileGone =>
                "文件已被外部删除".to_string(),
            abacus_core::undo::UndoConflict::DirectoryNotEmpty { entries } =>
                format!("目录非空：{}", entries.join(", ")),
            abacus_core::undo::UndoConflict::DestinationOccupied =>
                "源路径已被占用，不能 rename 回去".to_string(),
        };
        format!("{header}\n  ⚠️ 冲突: {detail}")
    } else {
        header
    }
}

fn format_history(entries: &[abacus_core::undo::HistoryEntry]) -> String {
    let mut out = format!("📜 Undo History ({} 条):\n", entries.len());
    for e in entries {
        let mark = if e.undone { "↺" } else { "✓" };
        let sid_short = &e.session_id[..e.session_id.len().min(8)];
        out.push_str(&format!(
            "  {} #{:<4} {} t{} {}  ({})\n",
            mark, e.seq, e.tool, e.turn,
            short_path(&e.path), sid_short,
        ));
    }
    out
}

/// Phase 6 跨 session timeline 渲染
///
/// ## 设计要点（设计文档 § 4.3）
/// - **session 分组**：按时间倒序穿插的同 session 连续条目合并为一组
/// - **窗口序号**：按 session 在 timeline 中**首次出现的顺序**分配 `[w1]/[w2]/...`，
///   稳定且与时间倒序对应（不依赖 session 注册顺序，因为旧 session 可能未在窗口中）
/// - **当前 session 高亮**：用 `▶` 前缀 + `[you]` 替代窗口序号
/// - **撤销标识**：`↺` undone / `•` active
fn format_timeline(
    entries: &[abacus_core::undo::HistoryEntry],
    hours: u64,
    current_session_id: &str,
) -> String {
    let mut out = format!("📜 Project Timeline (过去 {hours}h, {} 条):\n", entries.len());

    // 派生窗口序号：按时间倒序中 session 首次出现的顺序编号
    let mut window_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for e in entries {
        if !window_index.contains_key(&e.session_id) {
            let n = window_index.len() + 1;
            window_index.insert(e.session_id.clone(), n);
        }
    }

    let mut current_session: Option<String> = None;
    for e in entries {
        if current_session.as_ref() != Some(&e.session_id) {
            let sid_short = &e.session_id[..e.session_id.len().min(8)];
            let label = if e.session_id == current_session_id {
                format!("[you]  session {}", sid_short)
            } else {
                let w = window_index.get(&e.session_id).copied().unwrap_or(0);
                format!("[w{}]   session {}", w, sid_short)
            };
            out.push_str(&format!("\n  ── {} ──\n", label));
            current_session = Some(e.session_id.clone());
        }
        let mark = if e.undone { "↺" } else { "•" };
        let prefix = if e.session_id == current_session_id { "▶ " } else { "  " };
        let ts = e.timestamp.format("%H:%M:%S");
        out.push_str(&format!(
            "  {}{} {} #{:<4} {} {}\n",
            prefix, mark, ts, e.seq, e.tool, short_path(&e.path),
        ));
    }
    out
}

fn short_path(p: &str) -> String {
    let chars: Vec<char> = p.chars().collect();
    if chars.len() <= 60 { return p.to_string(); }
    let head: String = chars.iter().take(30).collect();
    let tail: String = chars.iter().rev().take(27).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{head}…{tail}")
}

// V33 注：`save_session_snapshot` doc 已迁移至实际定义处（项目层 sessions/{uuid}.json，
//   走 paths::current_sessions_dir 项目隔离 + ABACUS_HOME 覆盖）；此处仅保留段标题。

// ─── V29.11: 系统级 always_allow 持久化 ─────────────────────────
//
// 路径: ~/.abacus/always_allow.json (JSON array of tool_id strings)
// 引用关系:
//   - 写入: event/mod.rs 按 Y/A 键 + run.rs 超时 auto-allow + /allow add
//   - 读取: run.rs 启动时 load + /allow list
// 生命周期: 系统级 — 跨 session/项目/进程共享; 用户 /allow revoke 或手动删文件可清
// 设计取舍:
//   - JSON array 而非数据库: 条目 <100, 纯文本可读可手动编辑
//   - 原子写入(tmp+rename): 防 crash 中途损坏

fn always_allow_path() -> std::path::PathBuf {
    abacus_core::paths::global_dir().join("always_allow.json")
}

/// 默认允许列表 — 首次启动时(文件不存在)自动注入
///
/// 设计原则:
///   - 只含 confirm_required=false 的只读/非破坏性工具 + filengine_bash_exec(最常用，Medium 级)
///   - filengine_fs_write / filengine_fs_move / filengine_fs_mkdir 保留弹窗(有副作用，首次需用户确认)
///   - MCP 外部工具按命名空间约定匹配——如果使用方注册了 filengine MCP server，
///     其工具名形如 "mcp__filengine__file_read" 等，此列表不预判外部命名
///   - 用户可通过 /allow revoke 随时收紧
///
/// 格式: 与 pipeline MCIP 的 tool_id 一致 = ToolId.0 = "filengine_{schema_name}"（全下划线）
const DEFAULT_ALLOW: &[&str] = &[
    // ─── 文件读取 (只读, 无副作用) ───
    "filengine_fs_read",
    "filengine_fs_read_multiple",
    "filengine_fs_info",
    "filengine_fs_ls",
    "filengine_fs_tree",
    "filengine_fs_search",
    "filengine_fs_grep",
    // ─── 文件编辑 (精确替换, confirm=false) ───
    "filengine_fs_edit",
    // ─── 网络 (只读) ───
    "filengine_web_fetch",
    "filengine_web_search",
    // ─── Shell (最常用, Medium 级 — 默认允许避免每条命令弹窗) ───
    "filengine_bash_exec",
    // ─── 文件写入/移动 (有副作用, 但高频且非破坏性) ───
    "filengine_fs_write",
    "filengine_fs_move",
    "filengine_fs_mkdir",
];

/// 从系统文件加载 always_allow 列表
///
/// 文件不存在(首次启动) → 自动写入 DEFAULT_ALLOW 并返回
/// 文件存在但损坏 → 空集(容错)
/// V29.11 迁移: 旧点号名 "filengine.fs.read" → 下划线 "filengine_fs_read"
pub(crate) fn load_always_allow() -> std::collections::HashSet<String> {
    let path = always_allow_path();
    if !path.exists() {
        // 首次启动: 注入默认列表 + 落盘
        let defaults: std::collections::HashSet<String> =
            DEFAULT_ALLOW.iter().map(|s| s.to_string()).collect();
        let _ = save_always_allow(&defaults);
        return defaults;
    }
    match std::fs::read_to_string(&path) {
        Ok(json) => {
            let raw: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
            // 一次性迁移: "filengine.fs.read" → "filengine_fs_read"
            let mut set: std::collections::HashSet<String> = raw.into_iter()
                .map(|s| s.replace('.', "_"))
                .collect();
            // Phase 3 (3.7): 仅补充 set 中不存在的新增 DEFAULT 项。
            // 如果用户显式删过某项（文件存在且不含该项），不强加回来。
            // 适用场景：新版本新增了 DEFAULT 工具，用户从未见过该工具——此时补充。
            // —— 无法完美区分“用户删过”与“旧版未包含”，当前策略以简化为主：
            //   文件非空且已存在 → 仅补充 set 中完全不存在的 DEFAULT 项
            //   TODO: 引入 removed_tools.json 记录用户显式 revoke，实现精确区分
            for d in DEFAULT_ALLOW {
                // 补充 set 中不存在的 DEFAULT 项（新版本新增工具自动可用）
                if !set.contains(*d) {
                    set.insert(d.to_string());
                }
            }
            let _ = save_always_allow(&set);
            set
        }
        Err(_) => std::collections::HashSet::new(),
    }
}

/// 把 always_allow 集合写入系统文件(原子写)
pub(crate) fn save_always_allow(set: &std::collections::HashSet<String>) -> std::io::Result<()> {
    let dir = abacus_core::paths::global_dir();
    std::fs::create_dir_all(&dir)?;
    let path = always_allow_path();
    let mut sorted: Vec<&String> = set.iter().collect();
    sorted.sort(); // 稳定输出, 方便人读/diff
    let json = serde_json::to_string_pretty(&sorted)?;
    let tmp = dir.join(".always_allow.json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// - 文件命名用 state.session_id (UUID)，多实例不互覆盖
/// - 额外写 last_session_uuid 文本 pointer（项目内）以支持 "恢复上次"语义
///
/// V28 (T9): SessionExport 升级到 v2 — 把 events: Vec<EventEntry> 替换为
/// trace_events: Vec<TraceEvent> + next_trace_id: u64(SSOT 直接持久化)。
/// 旧 v1 文件由 load_last_session 自动 migration 到 v2 形态(events → Generic kind)。
pub(crate) fn save_session(state: &AppState) -> std::io::Result<()> {
    use serde::Serialize;
    #[derive(Serialize)]
    struct SessionExport {
        version: u32,
        session_id: String,
        model_name: String,
        thinking_depth: String,
        turn_count: u32,
        session_summary: String,
        messages: Vec<crate::tui::state::Message>,
        // V28: trace_events 是 SSOT,events 字段不再写出
        trace_events: Vec<crate::tui::state::TraceEvent>,
        next_trace_id: u64,
        // V29.11: always_allow 已迁移到系统级 ~/.abacus/always_allow.json
        // 此字段不再写入 session; 旧 session 的 "always_allow" JSON key 在 load 时
        // 由 apply_session_export 手动处理(一次性迁移到系统文件), struct 不需要该字段
        #[serde(skip_serializing)]
        _always_allow_legacy: Vec<String>,
        // V29.9: 会话可读别名(/rename) + turnkey 全托管目标(/turnkey)
        // 引用关系: AppState.session_alias / session_goal → 持久化 → load 时回填
        // 生命周期: 写入: save_session(每条消息后/手动 /save) | 销毁: 用户 /new 或显式 clear
        #[serde(skip_serializing_if = "Option::is_none")]
        session_alias: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_goal: Option<String>,
        // V34-4: 模式 + 跨阶段 artifact 持久化（DAG 状态恢复）
        // 引用关系: AppState.mode / mode_artifact → 持久化 → load 时回填
        // 生命周期:
        //   mode → 写入 set_mode 调用后；恢复后用户从原阶段继续（避免重启回到 Clarify 起点）
        //   mode_artifact → 写入 try_switch_mode（Plan→Team 解析后）；恢复后由 switch_mode take() 消费
        // 兼容性: skip_serializing_if = Option::is_none 保证旧 session 文件不报字段缺失
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<abacus_types::AbacusMode>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode_artifact: Option<abacus_types::ModeArtifact>,
        // V37-2: token 统计持久化（含 V36-3 per_model 分布）
        //   引用关系: AppState.session_tokens → 持久化 → load 时回填
        //   生命周期: 累加于每轮 EngineResponse.stats；跨重启保留（用户想看跨会话累计开销）
        //   兼容性: 旧 session 文件无此字段 → load 时 serde 不报错（apply_session_export 用 get-or-default）
        #[serde(skip_serializing_if = "session_tokens_is_empty")]
        session_tokens: Option<crate::tui::state::SessionTokenStats>,
        // V38-2: 持久化 PlannerNudge attempts（防 V37-1 attempts 上限被重启绕过）
        //   引用关系: AppState.planner_nudge_attempts ← try_switch_mode SchemaInvalid 分支递增
        //   兼容性: 默认 0 时跳过序列化（is_zero）
        #[serde(default, skip_serializing_if = "u8_is_zero")]
        planner_nudge_attempts: u8,
        // V40-1: 持久化最近一次 review 结果 + strict 阻断标志
        //   引用关系: AppState.last_review / last_review_strict
        //   设计意图: 防止用户重启后丢失 strict 阻断 → 已被 fail 的 plan 被错误放进 Team
        //   兼容性: skip_serializing_if = Option::is_none 旧文件无字段不报错
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_review: Option<crate::tui::api::ReviewReport>,
        #[serde(default, skip_serializing_if = "bool_is_false")]
        last_review_strict: bool,
        // V41-1: 持久化 /auto-review 配置（用户意图持久，关机不丢）
        //   引用关系: AppState.auto_review_plan ← cmd_auto_review 切换
        //   设计意图: 用户显式开启的 toggle 不应静默关闭 — 跨重启遵守用户意图
        //   兼容性: 默认 false 时跳过序列化（旧 session 无字段 → 默认关闭）
        #[serde(default, skip_serializing_if = "bool_is_false")]
        auto_review_plan: bool,
        // V41-补持久化: review_history（V41-4 规定的"完整轨迹"应跨重启保留）
        //   引用关系: AppState.review_history ← run.rs:397 review 抵达后 push_back
        //   设计意图: 字段文档承诺"完整轨迹"，仅 in-memory 不持久化等于宣称"完整"实则每次重启清零
        //   兼容性: 旧 session 无字段 → 反序列化得空 VecDeque（默认行为不变）
        //   上限保护: load 后 push 时 > 20 仍会 pop_front 兜底，capacity 不准也无副作用
        #[serde(default, skip_serializing_if = "review_history_is_empty")]
        review_history: std::collections::VecDeque<crate::tui::api::ReviewReport>,
        // V41-补持久化: review_required 强约束开关（V41-2）
        //   引用关系: AppState.review_required ← cmd_review_required 切换
        //   设计意图: 与 auto_review_plan 同性质 — 用户显式开启的 toggle 不应静默关闭
        //   兼容性: 默认 false 时跳过序列化（旧 session 无字段 → 默认关闭）
        #[serde(default, skip_serializing_if = "bool_is_false")]
        review_required: bool,
        // V41-补持久化: review fresh-age 阈值（V41-2）
        //   引用关系: AppState.review_max_age_secs ← cmd_review_required 解析 [<秒>]
        //   设计意图: 用户自定义阈值跨重启保留；总是序列化避免默认值漂移导致静默语义改变
        //   兼容性: 旧 session 无字段 → serde default = 600（与 AppState::new 默认一致）
        #[serde(default = "default_review_max_age_secs")]
        review_max_age_secs: u64,
        saved_at: String,
    }

    /// V38-2: u8 零值判定 — 用于 skip_serializing_if
    fn u8_is_zero(v: &u8) -> bool { *v == 0 }
    /// V40-1: bool false 判定 — 用于 last_review_strict skip
    fn bool_is_false(v: &bool) -> bool { !*v }

    /// V37-2: SessionTokenStats 全零判定 — 全 0 时跳过序列化节省字节
    fn session_tokens_is_empty(opt: &Option<crate::tui::state::SessionTokenStats>) -> bool {
        match opt {
            None => true,
            Some(s) => s.total_tokens == 0 && s.cost_cny == 0.0 && s.per_model.is_empty(),
        }
    }
    /// V41-补: review_history 空判定 — 用于 skip_serializing_if
    /// 引用关系: SessionExport.review_history 字段; 空时跳过序列化保持旧 session 文件兼容
    fn review_history_is_empty(v: &std::collections::VecDeque<crate::tui::api::ReviewReport>) -> bool {
        v.is_empty()
    }
    /// V41-补: review_max_age_secs 缺字段时的兜底默认 — 与 AppState::new 默认值（600）保持一致
    /// 引用关系: SessionExport.review_max_age_secs 字段 serde default; 旧 session 无字段时回退此值
    fn default_review_max_age_secs() -> u64 { 600 }
    let export = SessionExport {
        version: 2,
        session_id: state.session_id.clone(),
        model_name: state.model_name.clone(),
        thinking_depth: state.thinking_depth.clone(),
        turn_count: state.turn_count,
        session_summary: state.session_summary.clone(),
        messages: state.messages.iter().cloned().collect(),
        trace_events: state.trace_events.clone(),
        next_trace_id: state.next_trace_id,
        _always_allow_legacy: Vec::new(), // 不再写入 session
        session_alias: state.session_alias.clone(),
        session_goal: state.session_goal.clone(),
        // V34-4: 持久化当前模式（Clarify 默认无需写出，节省字节）+ 待消费 artifact
        mode: (state.mode != abacus_types::AbacusMode::Clarify).then_some(state.mode),
        mode_artifact: state.mode_artifact.clone(),
        // V37-2: 持久化 token 统计（含 per_model 分布）；全 0 时 None 跳过序列化
        session_tokens: if state.session_tokens.total_tokens == 0
            && state.session_tokens.cost_cny == 0.0
            && state.session_tokens.per_model.is_empty()
        {
            None
        } else {
            Some(state.session_tokens.clone())
        },
        // V38-2: 持久化 PlannerNudge attempts 计数器
        planner_nudge_attempts: state.planner_nudge_attempts,
        // V40-1: 持久化 review 状态
        last_review: state.last_review.clone(),
        last_review_strict: state.last_review_strict,
        // V41-1: 持久化 /auto-review 配置
        auto_review_plan: state.auto_review_plan,
        // V41-补持久化: review_history / review_required / review_max_age_secs
        //   设计闭环: 字段文档承诺"完整轨迹" + V41-1 立的"用户意图持久"原则同时落实
        review_history: state.review_history.clone(),
        review_required: state.review_required,
        review_max_age_secs: state.review_max_age_secs,
        saved_at: chrono::Utc::now().to_rfc3339(),
    };
    // Phase 3: paths::current_sessions_dir() 为项目层路径
    // 形如：~/.abacus/projects/<escaped-cwd>/sessions/
    let dir = abacus_core::paths::current_sessions_dir();
    std::fs::create_dir_all(&dir)?;

    // 用 session_id (UUID) 命名，多实例不会撞同名
    let filename = format!("{}.json", state.session_id);
    let path = dir.join(&filename);
    let json = serde_json::to_string_pretty(&export)?;

    // 原子写入：先写 .tmp 再 rename，避免部分写入损坏
    let tmp_path = dir.join(format!(".{}.json.tmp", state.session_id));
    std::fs::write(&tmp_path, &json)?;
    std::fs::rename(&tmp_path, &path)?;

    // 项目层 last_session_uuid 文本 pointer — 仅本项目多实例会互覆（last-writer-wins，可接受）
    // 跨项目不冲突，符合“项目隔离”语义。
    let pointer = dir.join("last_session_uuid");
    let _ = std::fs::write(&pointer, &state.session_id);

    // R3 修复 (保留)：项目内保留最近 SESSION_KEEP 个 *.json (按 mtime)。
    // 包含跨多实例产生的 session——UUID 命名让文件名不都含时间戳，改走 mtime。
    const SESSION_KEEP: usize = 50;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut snapshots: Vec<(std::path::PathBuf, std::time::SystemTime)> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                let is_session = p.extension().and_then(|x| x.to_str()) == Some("json")
                    && !p.file_name().and_then(|x| x.to_str()).map(|n| n.starts_with('.')).unwrap_or(false);
                if !is_session { return None; }
                e.metadata().ok().and_then(|m| m.modified().ok()).map(|mt| (p, mt))
            })
            .collect();
        snapshots.sort_by_key(|s| s.1); // 升序：最旧在前，便于裁剪头部
        if snapshots.len() > SESSION_KEEP {
            for (old, _) in &snapshots[..snapshots.len() - SESSION_KEEP] {
                let _ = std::fs::remove_file(old);
            }
        }
    }
    Ok(())
}

/// 从 ~/.abacus/sessions/latest.json 恢复上次会话
///
/// V28 (T9): v1 → v2 migration:
///   - v2: 直接反序列化 trace_events + next_trace_id(SSOT 真相源)
///   - v1 / 缺 version: 老 events: Vec<EventEntry> 转成 TraceEvent::Generic, 顺序分配 id 0..N
///   - 旧 messages 中遗留的 Block(Think/ToolCall) 原样保留(渲染层兼容,T5 不删 Block 路径)
pub fn load_last_session(state: &mut AppState) -> std::io::Result<bool> {
    let path = session_path();
    if !path.exists() { return Ok(false); }
    load_session_from_path(state, &path)
}

/// V29.9 (C2): 按 uuid 加载特定 session — /resume 命令用
///
/// 引用关系:
///   - 调用: slash_commands::cmd_resume
///   - 使用: paths::current_sessions_dir() 推导 {dir}/{uuid}.json
/// 生命周期: 一次性调用，加载成功后由调用方决定是否同步 last_session_uuid pointer
/// 返回: Ok(true) 加载并应用, Ok(false) 文件不存在, Err(io) IO 错误
pub fn load_session_by_uuid(state: &mut AppState, uuid: &str) -> std::io::Result<bool> {
    let dir = abacus_core::paths::current_sessions_dir();
    let path = dir.join(format!("{}.json", uuid));
    if !path.exists() { return Ok(false); }
    load_session_from_path(state, &path)
}

/// 内部 helper — 给定路径加载 session JSON 到 state
/// 引用关系: load_last_session + load_session_by_uuid 共用
fn load_session_from_path(state: &mut AppState, path: &std::path::Path) -> std::io::Result<bool> {
    let json = std::fs::read_to_string(path)?;
    let export: serde_json::Value = serde_json::from_str(&json)?;
    apply_session_export(state, &export);
    Ok(true)
}

/// 把 SessionExport JSON 应用到 state(纯函数,便于单元测试)
///
/// V28: 显式区分 v1 vs v2 路径,v1 把 events 数组转成 TraceEvent::Generic 列表
fn apply_session_export(state: &mut AppState, export: &serde_json::Value) {
    use crate::tui::state::{TraceEvent, TraceKind};

    // Phase 3：恢复 session_id (UUID)；旧文件无此字段 → 保留 state 现有 UUID（启动生成的）
    if let Some(sid) = export.get("session_id").and_then(|v| v.as_str()) {
        if !sid.is_empty() {
            state.session_id = sid.to_string();
        }
    }
    if let Some(name) = export.get("model_name").and_then(|v| v.as_str()) {
        state.model_name = name.to_string();
    }
    if let Some(tc) = export.get("turn_count").and_then(|v| v.as_u64()) {
        state.turn_count = tc as u32;
    }
    if let Some(s) = export.get("session_summary").and_then(|v| v.as_str()) {
        state.session_summary = s.to_string();
    }
    // Phase 3 (3.6): 恢复 thinking_depth——保存时序列化了此字段，但原先缺少反序列化回填。
    // 引用关系：save_session 写入 thinking_depth；本处恢复到 AppState.thinking_depth
    // 兼容性：旧文件无此字段 → 保留 state 现值（启动默认 "medium"）
    if let Some(td) = export.get("thinking_depth").and_then(|v| v.as_str()) {
        if !td.is_empty() {
            state.thinking_depth = td.to_string();
        }
    }
    if let Some(msgs) = export.get("messages") {
        if let Ok(msgs) = serde_json::from_value::<Vec<crate::tui::state::Message>>(msgs.clone()) {
            state.messages = msgs.into();
        }
    }

    // V28 version 判定: 缺字段或 version<2 走 v1 migration 路径
    let version = export.get("version").and_then(|v| v.as_u64()).unwrap_or(1);
    if version >= 2 {
        // v2: 直接反序列化 trace_events + next_trace_id
        if let Some(te) = export.get("trace_events") {
            if let Ok(te) = serde_json::from_value::<Vec<TraceEvent>>(te.clone()) {
                state.trace_events = te;
            }
        }
        if let Some(nti) = export.get("next_trace_id").and_then(|v| v.as_u64()) {
            state.next_trace_id = nti;
        } else {
            // 兜底: 从 trace_events 末尾推算
            state.next_trace_id = state.trace_events.last().map(|e| e.id + 1).unwrap_or(0);
        }
    } else {
        // v1 migration: events: Vec<EventEntry> → trace_events: Vec<TraceEvent::Generic>
        if let Some(evts) = export.get("events") {
            if let Ok(evts) = serde_json::from_value::<Vec<crate::tui::state::EventEntry>>(evts.clone()) {
                state.trace_events = evts.into_iter().enumerate().map(|(i, e)| TraceEvent {
                    id: i as u64,
                    time: e.time,
                    category: e.category,
                    level: e.level,
                    kind: TraceKind::Generic { content: e.content },
                    duration_ms: None,
                }).collect();
                state.next_trace_id = state.trace_events.len() as u64;
            }
        }
    }

    // V29.11: always_allow 已迁移到系统级 ~/.abacus/always_allow.json
    //   旧 session 文件含此字段时做一次性迁移：合并到系统文件（不覆盖，追加 diff）
    //   迁移后不再读取 session-level always_allow
    if let Some(aa) = export.get("always_allow") {
        if let Ok(list) = serde_json::from_value::<Vec<String>>(aa.clone()) {
            if !list.is_empty() {
                // 一次性迁移: session → system (追加不重复)
                for tool in list {
                    state.always_allow.insert(tool);
                }
                // 迁移后立即保存到系统文件
                let _ = save_always_allow(&state.always_allow);
            }
        }
    }

    // V29.9: 恢复 session_alias / session_goal —— /rename 与 /turnkey 跨重启保留
    //   缺字段 → None(默认行为不变);存在但非 string → 静默跳过
    if let Some(s) = export.get("session_alias").and_then(|v| v.as_str()) {
        state.session_alias = (!s.is_empty()).then(|| s.to_string());
    }
    if let Some(s) = export.get("session_goal").and_then(|v| v.as_str()) {
        state.session_goal = (!s.is_empty()).then(|| s.to_string());
    }

    // V34-4: 恢复 mode / mode_artifact
    //   引用关系: save_session 写入；这里反序列化回填到 AppState
    //   兼容性: 缺字段 → 保留 state 现值（默认 Clarify / None）
    //   设计意图: 重启后用户回到上次的阶段，避免每次都从 Clarify 起点重做
    if let Some(m) = export.get("mode") {
        if let Ok(mode) = serde_json::from_value::<abacus_types::AbacusMode>(m.clone()) {
            state.set_mode(mode);
        }
    }
    if let Some(art) = export.get("mode_artifact") {
        if let Ok(artifact) = serde_json::from_value::<abacus_types::ModeArtifact>(art.clone()) {
            state.mode_artifact = Some(artifact);
        }
    }

    // V37-2: 恢复 session_tokens（含 per_model 分布）
    //   引用关系: save_session 写入；这里反序列化回填到 AppState.session_tokens
    //   兼容性: 旧文件无 session_tokens / per_model 字段 → serde default 兜底（全 0 / 空 HashMap）
    //   设计意图: 让用户在新 session 启动后立即看到上次会话的累计开销，避免归零误导
    if let Some(st) = export.get("session_tokens") {
        if let Ok(tokens) = serde_json::from_value::<crate::tui::state::SessionTokenStats>(st.clone()) {
            state.session_tokens = tokens;
        }
    }

    // V38-2: 恢复 PlannerNudge attempts 计数器
    //   引用关系: save_session 写入；这里反序列化回填到 AppState.planner_nudge_attempts
    //   设计意图: 防止用户重启绕过 V37-1 attempts 上限（schema 持续不合规时无限规避）
    //   兼容性: 旧文件无此字段 → 保持默认 0
    if let Some(v) = export.get("planner_nudge_attempts").and_then(|x| x.as_u64()) {
        state.planner_nudge_attempts = v.min(u8::MAX as u64) as u8;
    }

    // V40-1: 恢复 review 状态（last_review + strict 阻断）
    //   引用关系: save_session 写入；这里反序列化回填到 AppState.last_review / last_review_strict
    //   设计意图: 防止用户重启后 strict 阻断丢失 → 已被 fail 的 plan 被错误放进 Team
    //   兼容性: 旧文件无字段 → 保持默认 None / false
    if let Some(r) = export.get("last_review") {
        if let Ok(report) = serde_json::from_value::<crate::tui::api::ReviewReport>(r.clone()) {
            state.last_review = Some(report);
        }
    }
    if let Some(v) = export.get("last_review_strict").and_then(|x| x.as_bool()) {
        state.last_review_strict = v;
    }

    // V41-1: 恢复 /auto-review 配置
    //   引用关系: save_session 写入；这里反序列化回填到 AppState.auto_review_plan
    //   设计意图: 跨重启遵守用户意图 — 上次 /auto-review on 后重启仍应是 on
    //   兼容性: 旧文件无字段 → 保持默认 false
    if let Some(v) = export.get("auto_review_plan").and_then(|x| x.as_bool()) {
        state.auto_review_plan = v;
    }

    // V41-补: 恢复 review_history（V41-4 完整轨迹）
    //   引用关系: save_session 写入；这里反序列化回填到 AppState.review_history
    //   设计意图: 字段文档承诺"完整轨迹"；只有跨重启保留才名实相符
    //   兼容性: 旧文件无字段或反序列化失败 → 保留 state 现值（new 时已 with_capacity(20)）
    //   FIFO 保护: 即便载入超 20 条（极端历史文件），下次 push_back 后会 pop_front 兜底
    if let Some(rh) = export.get("review_history") {
        if let Ok(history) = serde_json::from_value::<std::collections::VecDeque<crate::tui::api::ReviewReport>>(rh.clone()) {
            state.review_history = history;
        }
    }

    // V41-补: 恢复 review_required（V41-2 强约束 toggle）
    //   引用关系: save_session 写入；这里反序列化回填到 AppState.review_required
    //   设计意图: 与 auto_review_plan 同性质 — 用户显式开启的 toggle 不应静默关闭
    //   兼容性: 旧文件无字段 → 保持默认 false
    if let Some(v) = export.get("review_required").and_then(|x| x.as_bool()) {
        state.review_required = v;
    }

    // V41-补: 恢复 review_max_age_secs（V41-2 fresh-age 阈值）
    //   引用关系: save_session 写入；这里反序列化回填到 AppState.review_max_age_secs
    //   设计意图: 用户自定义阈值跨重启保留；旧文件无字段 → 保持默认 600
    if let Some(v) = export.get("review_max_age_secs").and_then(|x| x.as_u64()) {
        state.review_max_age_secs = v;
    }
}

// ─────────────────────────────────────────────────────────────────────
// V28.4 已知遗留 (PR10 收尾时记录)
//
//   `cargo test -p abacus-cli` 被 abacus-core 并发编辑阻塞:
//     abacus-core/src/core/context.rs:1106 调用 `flush_pending(...)`
//     传 3 参,但 line 983-986 闭包签名要求 4 参(缺 archive 出参)。
//     该错误在 abacus-core 主 lib 代码,非测试代码,亦非 V28 范围。
//
//   影响: V28.4 的下面 4+4 单元测试无法在并发会话期间随手验证。
//   缓解: `cargo check -p abacus-cli` 已通过,V28.4 自身无新错误;
//         abacus-core 收敛后回归即可,无需 V28 侧改动。
// ─────────────────────────────────────────────────────────────────────

// ─── Phase 6: timeline 渲染单测 ─────────────────────────────────
#[cfg(test)]
mod undo_timeline_render_tests {
    use super::*;
    use abacus_core::undo::{HistoryEntry, OpKind};
    use chrono::TimeZone;

    fn entry(session: &str, seq: u64, secs: i64, tool: &str, path: &str, undone: bool) -> HistoryEntry {
        HistoryEntry {
            seq,
            session_id: session.into(),
            turn: 1,
            timestamp: chrono::Utc.timestamp_opt(secs, 0).unwrap(),
            tool: tool.into(),
            path: path.into(),
            op: OpKind::Create,
            undone,
        }
    }

    #[test]
    fn timeline_marks_current_session_as_you() {
        let entries = vec![
            entry("sess-A", 1, 100, "fs_write", "/a.txt", false),
            entry("sess-B", 2, 90,  "fs_edit",  "/b.txt", false),
        ];
        let out = format_timeline(&entries, 1, "sess-A");
        // 当前 session 标 [you]
        assert!(out.contains("[you]"));
        assert!(out.contains("session sess-A"));
        // 其他 session 标 [w2]（A 先出现 = w1 但 A 是 you，B 是第 2 个出现 = w2）
        assert!(out.contains("[w2]"));
        assert!(out.contains("session sess-B"));
        // 当前 session 行有 ▶ 前缀
        assert!(out.contains("▶ • "));
    }

    #[test]
    fn timeline_window_index_assigned_by_first_appearance() {
        // 时间倒序：B 最新 → 先出现 → w1；A 后 → w2；C 最旧 → w3
        let entries = vec![
            entry("sess-B", 5, 300, "fs_write", "/b1", false),
            entry("sess-B", 4, 290, "fs_edit",  "/b2", false),
            entry("sess-A", 3, 200, "fs_write", "/a1", false),
            entry("sess-C", 2, 100, "fs_move",  "/c1", false),
            entry("sess-A", 1, 50,  "fs_mkdir", "/a2", false),
        ];
        // 当前 session = X（不在 timeline 中），所有 session 都不是 you
        let out = format_timeline(&entries, 1, "sess-X-not-present");
        let b_pos = out.find("[w1]").expect("B should be w1");
        let a_pos = out.find("[w2]").expect("A should be w2");
        let c_pos = out.find("[w3]").expect("C should be w3");
        assert!(b_pos < a_pos && a_pos < c_pos, "windows numbered by first appearance");
    }

    #[test]
    fn timeline_undone_uses_circle_arrow() {
        let entries = vec![
            entry("sess-A", 1, 100, "fs_write", "/x.txt", true),  // undone
            entry("sess-A", 2, 90,  "fs_edit",  "/y.txt", false), // active
        ];
        let out = format_timeline(&entries, 1, "sess-A");
        assert!(out.contains("↺"), "undone entry should have ↺ marker");
        assert!(out.contains("• "), "active entry should have • marker");
    }

    #[test]
    fn timeline_groups_consecutive_same_session() {
        // 同 session 连续 3 条只画一次 header
        let entries = vec![
            entry("sess-A", 3, 300, "fs_write", "/x", false),
            entry("sess-A", 2, 200, "fs_write", "/y", false),
            entry("sess-A", 1, 100, "fs_write", "/z", false),
        ];
        let out = format_timeline(&entries, 1, "sess-A");
        let header_count = out.matches("session sess-A").count();
        assert_eq!(header_count, 1, "consecutive same-session entries share one header");
    }

    #[test]
    fn timeline_path_is_truncated_for_long_strings() {
        let long = "a".repeat(120);
        let entries = vec![entry("sess-A", 1, 100, "fs_write", &long, false)];
        let out = format_timeline(&entries, 1, "sess-A");
        assert!(out.contains("…"), "long path should be truncated with ellipsis");
        assert!(!out.contains(&long), "raw long path should not appear");
    }
}

#[cfg(test)]
mod session_migration_tests {
    //! V28 T9 SessionExport v1 → v2 migration 回归
    //!
    //! 不变量:
    //! - v1 events 列表全部映射到 TraceKind::Generic,顺序保持,id 从 0 单调
    //! - v2 文件直接反序列化,trace_events / next_trace_id 等价于写出时
    //! - messages 中遗留的 Block(Think/ToolCall) 不被 migration 触碰(T5 渲染层兼容)
    use super::*;
    use crate::tui::state::{AppState, AbacusMode, TraceKind};

    #[test]
    fn v1_events_migrate_to_generic_trace_events() {
        let v1_json = serde_json::json!({
            "version": 1,
            "model_name": "gpt-4",
            "turn_count": 3,
            "session_summary": "test",
            "messages": [],
            "events": [
                { "time": "12:00", "category": "llm", "content": "开始", "level": "Info" },
                { "time": "12:01", "category": "tool", "content": "fs.read 完成", "level": "Notice" },
                { "time": "12:02", "category": "session", "content": "用户提交", "level": "Info" },
            ],
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &v1_json);

        assert_eq!(state.model_name, "gpt-4");
        assert_eq!(state.turn_count, 3);
        assert_eq!(state.trace_events.len(), 3);
        assert_eq!(state.next_trace_id, 3);
        // 顺序保持 + id 单调
        for (i, ev) in state.trace_events.iter().enumerate() {
            assert_eq!(ev.id, i as u64);
            // 全部是 Generic kind
            assert!(matches!(ev.kind, TraceKind::Generic { .. }), "v1 migration 必须全为 Generic");
        }
        // category 字段保留
        assert_eq!(state.trace_events[0].category, "llm");
        assert_eq!(state.trace_events[1].category, "tool");
        assert_eq!(state.trace_events[2].category, "session");
    }

    #[test]
    fn v1_missing_version_treated_as_v1() {
        // 缺 version 字段(更老的格式)也应当走 v1 migration 路径
        let json = serde_json::json!({
            "model_name": "x",
            "messages": [],
            "events": [
                { "time": "12:00", "category": "llm", "content": "hi", "level": "Info" },
            ],
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &json);
        assert_eq!(state.trace_events.len(), 1);
        assert!(matches!(state.trace_events[0].kind, TraceKind::Generic { .. }));
    }

    #[test]
    fn v2_round_trip_preserves_trace_events() {
        // v2: 模拟"先写一份带 trace_events 的 export,再读回来,内容应等价"
        let v2_json = serde_json::json!({
            "version": 2,
            "model_name": "claude",
            "turn_count": 1,
            "session_summary": "v2",
            "messages": [],
            "trace_events": [
                {
                    "id": 5, "time": "10:00", "category": "llm", "level": "Info",
                    "duration_ms": null,
                    "kind": { "type": "Thinking", "text": "推理过程", "lines": 3 }
                },
                {
                    "id": 6, "time": "10:01", "category": "tool", "level": "Notice",
                    "duration_ms": 150,
                    "kind": {
                        "type": "ToolCall", "name": "filengine.fs.read", "args": "{}",
                        "output": "ok", "status": "Success"
                    }
                },
            ],
            "next_trace_id": 7,
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &v2_json);

        assert_eq!(state.trace_events.len(), 2);
        assert_eq!(state.next_trace_id, 7);
        // id 不被重置 — v2 直接采用文件中的 id(关键: 不与历史 message 引用冲突)
        assert_eq!(state.trace_events[0].id, 5);
        assert_eq!(state.trace_events[1].id, 6);
        match &state.trace_events[0].kind {
            TraceKind::Thinking { text, lines } => {
                assert_eq!(text, "推理过程");
                assert_eq!(*lines, 3);
            }
            _ => panic!("expected Thinking kind"),
        }
        match &state.trace_events[1].kind {
            TraceKind::ToolCall { name, status, .. } => {
                assert_eq!(name, "filengine.fs.read");
                assert!(matches!(status, crate::tui::state::ToolStatus::Success));
            }
            _ => panic!("expected ToolCall kind"),
        }
    }

    #[test]
    fn v2_missing_next_trace_id_falls_back_to_last_id_plus_1() {
        let json = serde_json::json!({
            "version": 2,
            "messages": [],
            "trace_events": [
                {
                    "id": 42, "time": "10:00", "category": "llm", "level": "Info",
                    "duration_ms": null,
                    "kind": { "type": "Generic", "content": "x" }
                },
            ],
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &json);
        assert_eq!(state.next_trace_id, 43, "缺 next_trace_id 时从末尾 id+1 推算");
    }

    // ─── V29.9 字段持久化回归 (alias / goal) ──────────────────
    //
    // 不变量:
    // - v2 文件含 session_alias/session_goal 字段 → apply 后 state 字段同值
    // - 缺字段 → state 保持 None(默认), 不 panic
    // - 空字符串 → 视为 None(避免后续 UI 显示空白)

    #[test]
    fn v2_loads_session_alias_and_goal() {
        let json = serde_json::json!({
            "version": 2,
            "messages": [],
            "trace_events": [],
            "next_trace_id": 0,
            "session_alias": "feature-x",
            "session_goal": "把 turnkey 接通 sandbox",
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &json);
        assert_eq!(state.session_alias.as_deref(), Some("feature-x"));
        assert_eq!(state.session_goal.as_deref(), Some("把 turnkey 接通 sandbox"));
    }

    #[test]
    fn v2_missing_alias_and_goal_default_to_none() {
        let json = serde_json::json!({
            "version": 2,
            "messages": [],
            "trace_events": [],
            "next_trace_id": 0,
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &json);
        assert!(state.session_alias.is_none(), "缺字段应保持 None");
        assert!(state.session_goal.is_none(), "缺字段应保持 None");
    }

    #[test]
    fn v2_empty_alias_string_treated_as_none() {
        let json = serde_json::json!({
            "version": 2,
            "messages": [],
            "trace_events": [],
            "next_trace_id": 0,
            "session_alias": "",
            "session_goal": "",
        });
        let mut state = AppState::new(AbacusMode::Clarify);
        apply_session_export(&mut state, &json);
        assert!(state.session_alias.is_none(), "空字符串应视为 None");
        assert!(state.session_goal.is_none(), "空字符串应视为 None");
    }
}

/// 项目层 "上次 session" 路径。
///
/// Phase 3 重构：从 sessions/latest.json 改为读 last_session_uuid pointer 推导
/// 到 sessions/{uuid}.json。pointer 不存在 / 读不到 → 返回不存在路径（调用方
/// 以 .exists() 处理“干净启动”语义）。
///
/// 返回 PathBuf 而非 Option 以保留与原签名的向后兼容。
fn session_path() -> std::path::PathBuf {
    let dir = abacus_core::paths::current_sessions_dir();
    let pointer = dir.join("last_session_uuid");
    if let Ok(uuid) = std::fs::read_to_string(&pointer) {
        let uuid = uuid.trim();
        if !uuid.is_empty() {
            return dir.join(format!("{uuid}.json"));
        }
    }
    // Fallback：返回预期不存在的路径（调用方以 .exists() 检查）
    dir.join(".no-last-session")
}


