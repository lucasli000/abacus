//! Abacus TUI — 运行逻辑 (库函数，供 binary 和 CLI 共同使用)

use std::io::{self, Write};
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
use crossterm::event::{EnableFocusChange, DisableFocusChange, EnableMouseCapture, DisableMouseCapture, EnableBracketedPaste, DisableBracketedPaste};
use crossterm::execute;
use ratatui::Terminal;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::layout::Alignment;
use tokio::sync::mpsc;
use tracing;

use crate::tui::i18n::t;
use crate::tui::util::safe_prefix;

use crate::tui::api::{EngineHandle, send_chat_message, send_team_message, send_meeting_message_streaming, send_plan_and_execute_streaming, list_cwd_files, ai_complete, ApiResult, EngineResponse};
use crate::tui::event::{handle_chat_scroll_key, handle_global_key, handle_input_key, handle_mouse};
use crate::tui::modes;
use crate::tui::setup;
// V28 (T4): BlockKind 不再被 run.rs 写入(thinking/tool 走 TraceKind);保留 enum 给 Checklist + 旧 session 兼容
use crate::tui::state::{AppState, InputState, Message, MsgContent, AbacusMode, SlashCommand, ToolStatus};

/// 单次 turn 内 streaming_timeline 条目上限（FIFO 裁剪，防止内存无限增长）
const STREAMING_TIMELINE_MAX_ENTRIES: usize = 1000;

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
        // OSC 0: 设置终端窗口/tab 标题（Warp/iTerm2/Terminal.app 通用）
        // OSC 1: 设置 tab 图标文本（iTerm2 专属，其他终端静默忽略）
        let _ = write!(stdout, "\x1b]0;⬡ Abacus\x07");
        let _ = write!(stdout, "\x1b]1;⬡\x07");
        // V29 (P4): 启用 FocusGained/FocusLost 事件(用于 ConfirmDialog 后台暂停 timer)
        //   不支持的终端忽略此 escape sequence, 不破坏其它功能 — 安全降级
        let _ = execute!(stdout, EnableFocusChange);
        // V29.6: 启用鼠标事件订阅 — 让 V28.1-V28.4 鼠标交互真正生效
        //   不支持的终端忽略此 escape sequence(罕见, 主流终端都支持)
        //   失败也不影响启动, 仅意味着鼠标功能不可用 — 键盘路径仍完整可用
        let _ = execute!(stdout, EnableMouseCapture);
        // 2026-05-28: 启用 bracketed paste — 粘贴文本作为 Event::Paste(String) 一次性到达,
        // 不再逐字符触发 KeyCode::Enter 导致多行文本被切成多条消息
        let _ = execute!(stdout, EnableBracketedPaste);
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
            let _ = execute!(io::stdout(), DisableBracketedPaste);
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
        // P2 资源泄漏修复：清理 wire trace 临时文件
        // wire_trace 是 debug-only 功能，但文件在进程退出时不会自动删除
        #[cfg(debug_assertions)]
        abacus_core::llm::wire_trace::cleanup_wire_trace();
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
                        // 2026-05-28: bracketed paste — 粘贴文本作为整块到达
                        Event::Paste(_) => { let _ = tx.send(evt); }
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
    // 🟡#11：打开失败要给用户看到告警，否则用户 TUI 卡死/异常时无法 forensic
    let file_writer: Box<dyn std::io::Write + Send> = match std::fs::OpenOptions::new()
        .create(true).append(true).open(&log_file_path) {
        Ok(f) => Box::new(f),
        Err(e) => {
            // eprintln 走 stderr——TUI 模式下原始 stderr 仍可见（终端 alt screen 切换前）
            eprintln!(
                "⚠ abacus: log file unavailable ({}: {e}), tracing logs will be dropped",
                log_file_path.display()
            );
            Box::new(std::io::sink())
        }
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
        eprintln!("  Last saved session at ~/.abacus/projects/<cwd>/sessions/<uuid>.json");
        default_hook(info);
    }));

    // Phase 3 (multi-instance D 模型)：原 R1 单实例 flock 已移除。
    // 多开不再被拒；session 隔离靠 UUID 命名文件实现（见 save_session）。
    // 跨项目隔离靠 paths::project_dir(cwd)。共享 SQLite 靠 WAL + busy_timeout。

    let mut guard = TermGuard::new()?;
    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    // V34: Plan/Team 已降级为执行策略；--team 参数改为默认 Meeting，--chat 保留 Clarify
    let mode = if chat {
        AbacusMode::Clarify
    } else {
        // team 参数不再对应 AbacusMode::Team；保留 CLI 参数向后兼容，但统一进入 Meeting
        let _ = team; // 抑制 unused variable 警告
        AbacusMode::Meeting
    };

    // 从 config.toml 读取 [tui.panel] sections 覆盖
    let panel_sections = load_panel_sections_from_config();
    let mut state = AppState::new_with_sections(mode, panel_sections);

    // V29.11: 系统级 always_allow 加载（优先于 session，全局共享）
    state.always_allow = load_always_allow();

    // V41: 不自动恢复上次会话——每次启动都是干净 session
    // 会话仍持久化到 sessions/{uuid}.json，用户可通过 /resume <uuid> 主动恢复
    // 设计理由：避免"进入就看到上次对话"的困惑，保持每次启动的确定性
    state.add_toast(format!("Abacus — {}", mode.label()), Duration::from_secs(3));

    // 首次配置 + 免责声明合并展示
    // 免责声明未接受 或 无 API 配置时 → 进入配置向导
    if !setup::disclaimer_accepted() || !setup::has_api_config() {
        state.add_toast(t("toast.first_setup"), Duration::from_secs(5));
        let configured = setup::run_setup(&mut terminal)?;
        if configured {
            state.add_toast(t("toast.config_saved"), Duration::from_secs(3));
        } else {
            guard.deactivate()?;
            return Ok(());
        }
    }

    // 初始化引擎（连接状态在 loading 画面已有展示，不再重复 toast）
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
                "  Initializing engine...",
                Style::default().fg(state.theme.text),
            )),
            Line::raw(""),
            Line::from(Span::styled(
                "  Checking API key...",
                state.theme.text_style(abacus_ui_kit::TextRole::Caption),
            )),
        ]).alignment(Alignment::Center);
        f.render_widget(loading, inner);
    })?;

    let engine = match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        EngineHandle::new(abacus_types::ModelId::AUTO, &state.thinking_depth),
    ).await {
        Ok(Ok(e)) => {
            state.add_toast(t("toast.engine_connected"), Duration::from_secs(3));
            // V30 复制修复：首次连接提示选中复制路径。
            // 生命周期：仅首次连接出现（随引擎启动一次性提醒）；重连会重发，不频繁。
            // 引用关系：/help 有完整复制节，用户可查体。
            // 复制提示已在 /help 中有完整说明，不在连接时打扰用户
            // FIX(v2.0.5): 不再 list_models() 拿第一个 — 那个总是返回 deepseek-v4-flash
            // （provider 列表里排第一），会覆盖用户在 setup 阶段选的真实 model。
            // 改用 config 里的 default_model（engine 实际使用的模型）作权威源。
            // 只有当 state.model_name 完全为空时才用 config 兜底。
            let config_model = e.core.config().default_model.0.clone();
            if state.model_name.is_empty() {
                state.model_name = config_model.clone();
            }
            state.theme.apply_model_brand(&state.model_name);
            // V40: 同步 context 到 TUI state
            // model_max_context = LLM 物理上限（model_spec 定义）
            // context_window = 有效窗口（physical × context_window_ratio, 最小 128K）
            // 引用：abacus-core/src/core/pipeline/mod.rs:598-600 的 available 计算
            // TUI 用 effective 而非 physical 作分母，progress 条与 pipeline 实际 budget 一致
            if let Some(ref spec) = e.core.config().model_spec {
                state.model_max_context = spec.context_window;
                let ratio = e.core.config().context_window_ratio.clamp(0.1, 1.0);
                state.context_window =
                    ((spec.context_window as f64 * ratio) as usize).max(128_000);
            }
            // 可用模型列表延迟到首次打开 /model picker 时通过 pending_model_fetch 触发
            // FIX(v2.0.5): 同步 thinking_depth — engine 初始化时已从 config 读取 thinking_intent，
            // 但 TUI state.thinking_depth 仍是硬编码默认值。用 config 的 thinking_intent 回写，
            // 让 /thinking picker 和看板 Thinking:xxx 显示真实值。
            if let Some(ref intent) = e.core.config().thinking_intent {
                let label = match intent {
                    abacus_types::ThinkingIntent::Off => "off",
                    abacus_types::ThinkingIntent::Adaptive => "adaptive",
                    abacus_types::ThinkingIntent::Effort(abacus_types::EffortLevel::Minimal) => "minimal",
                    abacus_types::ThinkingIntent::Effort(abacus_types::EffortLevel::Low) => "low",
                    abacus_types::ThinkingIntent::Effort(abacus_types::EffortLevel::Medium) => "medium",
                    abacus_types::ThinkingIntent::Effort(abacus_types::EffortLevel::High) => "high",
                    abacus_types::ThinkingIntent::Effort(abacus_types::EffortLevel::Max) => "max",
                    abacus_types::ThinkingIntent::Effort(abacus_types::EffortLevel::XHigh) => "xhigh",
                    abacus_types::ThinkingIntent::Budget(n) => {
                        // Budget 用数字字符串，/thinking picker 会识别
                        state.thinking_depth = n.to_string();
                        ""
                    }
                };
                if !label.is_empty() {
                    state.thinking_depth = label.to_string();
                }
            }
            // 避免启动时同步阻塞（虽然 list_models 是内存操作，但避免任何潜在 lock 争用）
            state.pending_model_fetch = true;

            // 异步拉取记忆宫殿本体数据（行为宫殿条目数 + 知识宫殿 domain 分布）
            {
                let palace_opt = e.core.memory_palace();
                if let Some(palace) = palace_opt {
                    let p = palace.read().await;
                    let behavior = p.behavior.len().await;
                    let domains = p.knowledge.domain_summary().await;
                    let total: u32 = domains.iter().map(|(_, c)| c).sum();
                    state.palace_data = Some(crate::tui::state::PalaceSnapshot {
                        behavior_count: behavior,
                        behavior_active: 0,
                        behavior_top_tags: Vec::new(),
                        knowledge_domains: domains,
                        knowledge_total: total,
                        knowledge_due: 0,
                    });
                }
            }

            // V42-B: 同步本地模型服务健康状态到 TUI 面板
            if let Some(h) = e.core.local_model_health() {
                state.local_health = Some(h);
            }

            e
        }
        Ok(Err(e)) => {
            guard.deactivate()?;
            eprintln!("\n[x] Engine init failed: {}\n", e);
            eprintln!("  Please check:");
            eprintln!("    - API key configured (ABACUS_API_KEY or DEEPSEEK_API_KEY)");
            eprintln!("    - Network connectivity");
            eprintln!("    - Model config in config.toml\n");
            return Err(io::Error::other(e));
        }
        Err(_) => {
            guard.deactivate()?;
            eprintln!("\n[x] Engine init timed out (15s)\n");
            eprintln!("  Please check network or API service status\n");
            return Err(io::Error::new(io::ErrorKind::TimedOut, "engine init timed out"));
        }
    };
    state.engine_handle = Some(engine.clone());

    // V42-B FIX: 启动时初始化所有注册工具的 health 快照
    // 引用关系：state.tool_health 由 ToolsSection 读取，原仅在工具调用时填充导致
    // 新会话面板显示 "Builtin 0 External 0"（误导用户工具未注册）
    {
        let tools = engine.core.tool_registry_ref().all_tools().await;
        let tool_ids: Vec<abacus_types::ToolId> = tools.iter()
            .filter(|t| matches!(t.state, abacus_types::ToolState::Loaded | abacus_types::ToolState::Active))
            .map(|t| t.id.clone())
            .collect();
        if !tool_ids.is_empty() {
            let health = engine.core.tool_health_snapshot(&tool_ids).await;
            for entry in health {
                state.tool_health.insert(entry.tool_id.clone(), entry);
            }
        }
    }

    // 启动 AutoEngine Runner——将 AutoHealth 快照推送到自动化 Tab
    // 生命周期：_auto_runner_handle drop 后 runner task 退出；与 TUI 同生命周期
    // 引用关系：health_rx 在 interval tick 分支被 try_recv；state.auto_health 消费
    let auto_engine = std::sync::Arc::new(abacus_core::auto::AutoEngine::new());
    let auto_runner = abacus_core::auto::runner::JobRunner::new(
        auto_engine,
        abacus_core::auto::runner::RunnerConfig::default(),
    );
    let (_auto_runner_handle, mut auto_health_rx) = auto_runner.spawn();

    // 初始化 channel
    let (res_tx, mut res_rx) = mpsc::unbounded_channel::<EngineResponse>();
    let (comp_tx, mut comp_rx) = mpsc::unbounded_channel::<(Vec<String>, String)>();
    // T-2 fix: 独立 channel 传递 discover_all_models 结果，防止占用 comp_rx
    // 生命周期：model_list_tx 在 spawn 内发送一次后 drop；model_list_rx 在 tick 分支拥收
    // 2026-05-28: 扩展为 (models, provider_groups, provider_statuses) 三元组
    // provider_statuses: Vec<(provider_id, available, error_msg)>
    type DiscoverResult = (Vec<String>, Vec<(String, Vec<String>)>, Vec<(String, bool, Option<String>)>);
    let (model_list_tx, mut model_list_rx) = mpsc::unbounded_channel::<DiscoverResult>();
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
    let mut config_recheck_ticks = 0u8;

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
                // 定期清理已完成的后台任务（避免 JoinHandle 泄漏）
                state.task_registry.reap_finished();

                if let Ok(size) = terminal.size() {
                    let new_rows = size.height;
                    let new_cols = size.width;
                    if new_cols != cols || new_rows != rows {
                        (rows, cols) = (new_rows, new_cols);
                        // Resize debounce: 标记 dirty 但延迟 3 帧（150ms）再真正重建
                        // 避免拖动 resize 时每帧全量重建 message lines
                        state.resize_debounce_frames = 3;
                    }
                    if state.resize_debounce_frames > 0 {
                        state.resize_debounce_frames -= 1;
                        if state.resize_debounce_frames == 0 {
                            // debounce 结束，触发缓存失效
                            state.mark_render_dirty();
                        }
                    }
                }
                // 延迟拉取可用模型列表：通过 /v1/models 动态发现，失败时 provider 内部自动 fallback 静态列表
                // 消费方：state.available_models → open_picker_model 优先使用
                // 生命周期：engine 连接后设 pending_model_fetch=true → 首次 tick 触发 → 拉取完毕后 false
                // 压缩暂存消息自动发送（Communication 阶段：没有 CompressAutoResume）
                // 条件：有暂存消息 + 不在 busy 状态（压缩完成、input_state 已恢复）
                if state.pending_compress_input.is_some()
                    && !matches!(state.input_state,
                        crate::tui::state::InputState::Thinking
                        | crate::tui::state::InputState::Executing
                        | crate::tui::state::InputState::Outputting)
                {
                    let pending = state.pending_compress_input.take().unwrap();
                    state.add_toast(
                        format!("{}: {}", t("compress.toast_done"), pending.chars().take(20).collect::<String>()),
                        Duration::from_secs(2),
                    );
                    state.input = pending;
                    crate::tui::event::submit_message(&mut state);
                }

                // T-2 fix: discover_all_models 是网络调用，不能在主循环 tick 里直接 .await
                // 改为 spawn 异步，结果通过独立的 model_list_rx channel 回传
                while let Ok((models, provider_groups, provider_statuses)) = model_list_rx.try_recv() {
                    state.available_models = models;
                    state.available_providers = provider_groups;
                    state.provider_statuses = provider_statuses.clone();
                    // FIX(v2.0.5): discover 只更新可选列表，不覆盖当前模型。
                    // 之前用 `deepseek-v4-flash` 作为 sentinel，会把用户选择的 mimo-v2-pro
                    // 覆盖成 provider 列表里的第一个模型，导致看板与实际 LLM 卡片不一致。
                    if state.model_name.is_empty() {
                        if let Some(first) = state.available_models.first() {
                            state.model_name = first.clone();
                            state.theme.apply_model_brand(first);
                        }
                    }
                    // 启动时 active_provider_id 为空（仅在首次 Complete 回传后填充）
                    // 从 discover 结果中取第一个 available provider 作初始值
                    if state.active_provider_id.is_empty() {
                        if let Some((id, _, _)) = provider_statuses.iter().find(|(_, avail, _)| *avail) {
                            state.active_provider_id = id.clone();
                        } else if let Some((id, _, _)) = provider_statuses.first() {
                            state.active_provider_id = id.clone();
                        }
                    }
                }
                if state.pending_model_fetch {
                    state.pending_model_fetch = false;
                    if let Some(ref engine) = state.engine_handle {
                        let engine_clone = engine.clone();
                        let tx = model_list_tx.clone();
                        tokio::spawn(async move {
                            // 使用带状态的 discover（同时检测每个 provider 的可用性）
                            let (discovered, statuses) = engine_clone.core.discover_all_models_with_status().await;
                            // 按 provider 分组保留（用于 picker 分组显示）
                            let providers_grouped: Vec<(String, Vec<String>)> = discovered.iter()
                                .map(|(id, ms)| (id.clone(), ms.clone()))
                                .collect();
                            let flat: Vec<String> = discovered.into_values().flatten().collect();
                            let models = if flat.is_empty() {
                                engine_clone.core.list_models().await
                            } else {
                                flat
                            };
                            let _ = tx.send((models, providers_grouped, statuses));
                        });
                    }
                }
                // ── 热加载：检测 config.toml 变化，实时更新 context_window ──
                // 🟡#19 治本：
                // - 用 tokio::time::sleep 实现 500ms 时间 debounce（不是 20-tick 计数，
                //   tick 频率受 render 帧率影响，时序不稳定）
                // - metadata + read_to_string 放 spawn_blocking 隔离阻塞 I/O
                // - 避免 torn JSON：read_to_string 一次读完整文件，toml::from_str 是原子解析
                config_recheck_ticks = config_recheck_ticks.wrapping_add(1);
                if config_recheck_ticks >= 20 {
                    config_recheck_ticks = 0;
                    // V42-B FIX: 移除阻塞式 sleep(500ms)。spawn_blocking 已隔离 I/O，
                    // mtime 比较本身就是最好的 debounce；额外 sleep 每 1s 冻结事件循环 500ms，
                    // 是 TUI 输入/流式卡顿的主因。
                    let config_path = abacus_core::paths::config_toml();
                    // 优化：先只做 metadata 检查（轻量），mtime 变化后再读文件内容
                    let mtime_result = tokio::task::spawn_blocking({
                        let path = config_path.clone();
                        move || {
                            std::fs::metadata(&path).ok()
                                .and_then(|m| m.modified().ok())
                        }
                    }).await.ok().flatten();
                    if let Some(mtime) = mtime_result {
                        if state.config_mtime != Some(mtime) {
                            // mtime 变化，再读取文件内容
                            let check_result = tokio::task::spawn_blocking(move || {
                                let content = std::fs::read_to_string(&config_path).ok()?;
                                Some((mtime, content))
                            }).await.ok().flatten();
                            if let Some((mtime, content)) = check_result {
                                state.config_mtime = Some(mtime);
                                state.pending_model_fetch = true;
                                if let Ok(toml_val) = toml::from_str::<toml::Value>(&content) {
                                    if let Some(ref engine) = state.engine_handle {
                                        let changed = engine.core.reload_from_toml(&toml_val);
                                        if let Some(cw) = toml_val.get("core")
                                            .and_then(|c| c.get("context_window"))
                                            .and_then(|v| v.as_integer())
                                        {
                                            let new_val = cw as usize;
                                            if new_val != state.context_window {
                                                state.context_window = new_val;
                                            }
                                        }
                                        if !changed.is_empty() {
                                            state.add_toast(
                                                format!("config hot-reloaded: {}", changed.join(", ")),
                                                std::time::Duration::from_secs(2),
                                            );
                                        }
                                    }
                                } else {
                                    state.add_toast(
                                        "config.toml parse error, skipped hot-reload".to_string(),
                                        std::time::Duration::from_secs(5),
                                    );
                                }
                            }
                        }
                    }
                }

                // Paused 时暂停引擎响应消费（但继续渲染和接收事件）
                if !state.paused {
                    let _ = process_engine_response(&mut state, &mut res_rx).await;
                }
                let had_streaming_update = process_streaming_chunks(&mut state, &mut stream_rx).await;
                // V40: 全量 drain 后统一设一次 dirty（替代每 chunk 重复设置）
                // had_streaming_update 同时作为分区渲染信号：true = 仅 streaming 尾部需重建
                if had_streaming_update {
                    state.streaming_content_dirty.set(true);
                    state.mark_render_dirty();
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
                        state.mark_render_dirty();
                    }
                }

                // AutoHealth 快照更新：自动化 Tab 实时显示 Runner 状态
                // 生命周期：Runner 每 tick 可能推送新快照；大字符不益(只取最新一条)
                while let Ok(health) = auto_health_rx.try_recv() {
                    state.auto_health = health;
                    state.mark_render_dirty();
                }

                // P1 优化：条件渲染
                // - streaming 期间：每帧渲染（内容持续变化 + 光标动画）
                // - 非 streaming：仅在有状态变化时渲染（事件/响应/resize/toast）
                // ratatui 双缓冲 diff 保证即使每帧调也只写变化区域到终端
                let needs_draw = state.is_streaming_active()
                    || !matches!(state.input_state, InputState::Ready) // spinner 动画需要持续渲染
                    || state.is_render_dirty()
                    || state.resize_debounce_frames > 0
                    || !state.toasts.is_empty()
                    || state.confirm_dialog.is_some()
                    || state.frame_dirty.get()
                    || state.picker.is_some()        // picker 打开后持续渲染
                    || state.show_settings           // 设置模态框
                    || state.theme_preview_open;     // 主题色板预览面板
                if needs_draw {
                    state.frame_dirty.set(false);
                    if let Err(e) = terminal.draw(|f| modes::render(f, &state, rows)) {
                        tracing::error!(?e, "TUI render error");
                    }
                }
                state.cleanup_toasts();

                // ── 确认弹窗超时检查（每帧） ──
                // V38: 分级超时策略
                //   破坏性（High risk）：超时 → 拒绝
                //   非破坏性（Medium/Low）：超时 → session 内允许（不写入永久 always_allow）
                if let Some(ref dialog) = state.confirm_dialog {
                    if dialog.is_expired() {
                        let _tool_id = dialog.tool_id.clone();
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
                            // 非破坏性：超时 → 仅此次单次放行，不写入 always_allow
                            // Bug fix: 原来会把工具永久加入 session always_allow，导致
                            // 后续所有该工具调用全部静默放行不弹窗。
                            // 改为仅放行当前次请求，不改变 always_allow 状态。
                            state.pending_confirmation_response = Some(true);
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
                            state.add_toast("🔓 Authorized, continuing", Duration::from_secs(2));
                        } else {
                            state.add_event(&ts, "mcip", &format!("Denied (pipeline returns deny output): {:?}", tool_names), crate::tui::state::EventLevel::Warning);
                            state.add_toast("🚫 Tool execution denied", Duration::from_secs(2));
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
                    } else {
                        // P1 fix: pending_confirmation_response 有值但 pending_mcip_confirmations 为空
                        // → 决策丢失，pipeline 永久死锁。记录 error 日志便于排查。
                        tracing::error!(
                            allowed,
                            "pending_confirmation_response consumed but pending_mcip_confirmations is empty — decision lost, pipeline may deadlock"
                        );
                    }
                }

                // V37: 去掉打字机节流——与 Claude Code 一致，API token 到达即渲染
                // stream_cursor 机制保留但不再主动驱动（仅由 streaming chunk 自然触发 dirty）
                // 如需恢复打字机效果，还原此处为 stream_cursor += N 逻辑
                if state.stream_cursor > 0 {
                    state.stream_cursor = 0;
                    state.mark_render_dirty();
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
                        state.clear_input();
                    }
                }

                // 处理异步补全结果（Editor 态忽略——编辑器不发起补全）
                while let Ok((candidates, prefix)) = comp_rx.try_recv() {
                    if state.input_state == InputState::Editor { continue; }
                    if candidates.is_empty() {
                        state.input_state = InputState::Typing;
                    } else {
                        state.completion.set_popup(candidates, prefix);
                        state.input_state = InputState::Completing;
                    }
                }
            }

            Some(event) = evt_rx.recv() => {
                // P1 优化：用户事件触发渲染
                state.frame_dirty.set(true);
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
                        if let Some(text) = state.pending_text.take() {
                            // V34: plan_mode 字段已删除（/plan-prefix 功能废弃），此处直接发送用户文本
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
                                     Choose the most efficient tool for each step — prefer direct action over asking.{}",
                                    current_mode.display_zh(), current_mode.label(), transitions, skills_str,
                                    // V35: 模式过渡上下文注入 — 携带上阶段产物摘要给 LLM
                                    // 引用: state.transition_hint 由 try_switch_mode 写入
                                    // 生命周期: hint 存在即注入，不限制次数（LLM 自然理解前置语境）
                                    state.transition_hint.as_ref()
                                        .map(|(hint, _)| format!("\n\n[Mode Context]\n{}", hint))
                                        .unwrap_or_default()
                                )
                            };
                            let chat_req_ctx = abacus_core::core::RequestContext {
                                thinking_intent: abacus_types::ThinkingIntent::from_str_loose(&state.thinking_depth),
                                system_prompt_override: Some(capability_context),
                                ..Default::default()
                            };

                            match current_mode {
                                // ═══ Meeting 模式: 多专家会诊 + Host 综合（流式连续流程） ═══
                                AbacusMode::Meeting => {
                                    let stx = stream_tx.clone();
                                    // 修复：Meeting 模式切换前保存未落档内容
                                    if !state.last_llm_text().is_empty() && !state.streaming_complete {
                                        let ts = chrono::Local::now().format("%H:%M").to_string();
                                        let mut parts: Vec<crate::tui::state::MsgContent> = Vec::new();
                                        let thinking = state.last_llm_thinking();
                                        if !thinking.is_empty() {
                                            let line_count = thinking.lines().count();
                                            let first_line = thinking.lines()
                                                .find(|l| !l.trim().is_empty()).unwrap_or("").trim();
                                            let preview: String = first_line.chars().take(40).collect();
                                            let summary = if preview.is_empty() {
                                                format!("💭 {} lines", line_count)
                                            } else {
                                                format!("💭 {} lines · {}", line_count, preview)
                                            };
                                            parts.push(crate::tui::state::MsgContent::Block {
                                                kind: crate::tui::state::BlockKind::Think,
                                                summary,
                                                collapsed: true,
                                                detail: thinking,
                                            });
                                        }
                                        let text = state.take_last_llm_text();
                                        if !text.is_empty() {
                                            parts.push(crate::tui::state::MsgContent::Stream(text));
                                        }
                                        if !parts.is_empty() {
                                            state.add_message(crate::tui::state::Message::new_session(parts, &ts));
                                        }
                                    }
                                    state.reset_streaming();
                                    state.begin_streaming_session();
                                    state.task_registry.register(tokio::spawn(async move {
                                        match send_meeting_message_streaming(&engine, &text, stx).await {
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
                                                    turnkey_plan: None, needs_clarify: None, tokens_freed: None,
                                                });
                                            }
                                            _ => { let _ = tx.send(EngineResponse::default()); }
                                        }
                                    }));
                                }
                                // ═══ Clarify 模式: 单 Agent 循环（默认路径） ═══
                                AbacusMode::Clarify => {
                                    if state.streaming_enabled {
                                        let stx = stream_tx.clone();
                                        // 启动新流式：先保存未落档的流式内容，再清旧累积
                                        // 修复：用户在中途发消息时，上一轮 streaming 内容可能未落档
                                        if !state.last_llm_text().is_empty() && !state.streaming_complete {
                                            // streaming 未完成但有内容 → 用户主动打断，保存为临时消息
                                            let ts = chrono::Local::now().format("%H:%M").to_string();
                                            let mut parts: Vec<crate::tui::state::MsgContent> = Vec::new();
                                            let thinking = state.last_llm_thinking();
                                            if !thinking.is_empty() {
                                                let line_count = thinking.lines().count();
                                                let first_line = thinking.lines()
                                                    .find(|l| !l.trim().is_empty()).unwrap_or("").trim();
                                                let preview: String = first_line.chars().take(40).collect();
                                                let summary = if preview.is_empty() {
                                                    format!("💭 {} lines", line_count)
                                                } else {
                                                    format!("💭 {} lines · {}", line_count, preview)
                                                };
                                                parts.push(crate::tui::state::MsgContent::Block {
                                                    kind: crate::tui::state::BlockKind::Think,
                                                    summary,
                                                    collapsed: true,
                                                    detail: thinking,
                                                });
                                            }
                                            let text = state.take_last_llm_text();
                                            if !text.is_empty() {
                                                parts.push(crate::tui::state::MsgContent::Stream(text));
                                            }
                                            if !parts.is_empty() {
                                                state.add_message(crate::tui::state::Message::new_session(parts, &ts));
                                            }
                                        }
                                        state.reset_streaming();
                                        state.begin_streaming_session();
                                        state.task_registry.register(tokio::spawn(async move {
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
                                                        turnkey_plan: None, needs_clarify: None, tokens_freed: None,
                                                    });
                                                }
                                                _ => { let _ = tx.send(EngineResponse::default()); }
                                            }
                                        }));
                                    } else {
                                        state.task_registry.register(tokio::spawn(async move {
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
                                                        turnkey_plan: None, needs_clarify: None, tokens_freed: None,
                                                    });
                                                }
                                                _ => { let _ = tx.send(EngineResponse::default()); }
                                            }
                                        }));
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
                                state.begin_streaming_session();
                                // 防并发：ReviewRole 调 LLM，设 Outputting 让输入框显示对应状态
                                state.set_busy_state(InputState::Outputting);
                                state.processing_phase = format!("review/{}", kind.label());
                                state.op_started_at = Some(std::time::Instant::now());
                                // V39-1: 标记下次 EngineResponse 需 parse_review_report
                                state.pending_review_parses = state.pending_review_parses.saturating_add(1);
                                state.task_registry.register(tokio::spawn(async move {
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
                                                turnkey_plan: None, needs_clarify: None, tokens_freed: None,
                                            });
                                        }
                                        _ => {}
                                    }
                                }));
                            } else if let crate::tui::state::SlashCommand::ExecuteWithPlan { task } = cmd {
                                // V34: /plan <task> 执行策略 — 调 send_plan_and_execute_streaming，不切换 mode
                                // 引用关系：cmd 由 slash_commands.rs::cmd_plan 构造
                                // 设计意图：Plan 降级为策略，在 Clarify mode 内部异步执行
                                let engine = engine.clone();
                                let tx = res_tx.clone();
                                let stx = stream_tx.clone();
                                // 构造独立 RequestContext（slash cmd 处理域，无法复用 message 路径的 chat_req_ctx）
                                let plan_req_ctx = abacus_core::core::RequestContext {
                                    thinking_intent: abacus_types::ThinkingIntent::from_str_loose(&state.thinking_depth),
                                    ..Default::default()
                                };
                                state.reset_streaming();
                                state.begin_streaming_session();
                                state.set_busy_state(InputState::Thinking);
                                state.processing_phase = "planning".into();
                                state.op_started_at = Some(std::time::Instant::now());
                                state.task_registry.register(tokio::spawn(async move {
                                    match send_plan_and_execute_streaming(&engine, &task, stx, plan_req_ctx).await {
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
                                                turnkey_plan: None, needs_clarify: None, tokens_freed: None,
                                            });
                                        }
                                        _ => { let _ = tx.send(EngineResponse::default()); }
                                    }
                                }));
                            } else if let crate::tui::state::SlashCommand::ExecuteWithTeam { task } = cmd {
                                // V34: /team <task> 执行策略 — 调 send_team_message，不切换 mode
                                // 引用关系：cmd 由 slash_commands.rs::cmd_team 构造
                                // 设计意图：Team 降级为策略，在 Clarify mode 内部异步执行
                                let engine = engine.clone();
                                let tx = res_tx.clone();
                                let stx = stream_tx.clone();
                                state.reset_streaming();
                                state.begin_streaming_session();
                                state.set_busy_state(InputState::Thinking);
                                state.processing_phase = "team".into();
                                state.op_started_at = Some(std::time::Instant::now());
                                state.task_registry.register(tokio::spawn(async move {
                                    match send_team_message(&engine, &task, stx).await {
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
                                                turnkey_plan: None, needs_clarify: None, tokens_freed: None,
                                            });
                                        }
                                        _ => { let _ = tx.send(EngineResponse::default()); }
                                    }
                                }));
                            } else if let crate::tui::state::SlashCommand::RoleInvoke { role, content } = cmd {
                                // L-3/L-4/L-5: 通用 Agent 角色调用 — 走流式 LLM 路径
                                // 引用关系：cmd 由 cmd_role 解析后构造；send_role_message_streaming 设置 system_prompt_override + 可选 prefix
                                // 设计意图：与 ReviewRole 同型，证明 V35-2 通道泛化性
                                let engine = engine.clone();
                                let tx = res_tx.clone();
                                let stx = stream_tx.clone();
                                state.reset_streaming();
                                state.begin_streaming_session();
                                state.set_busy_state(InputState::Outputting);
                                state.processing_phase = format!("team/{}", role.label());
                                state.op_started_at = Some(std::time::Instant::now());
                                state.task_registry.register(tokio::spawn(async move {
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
                                                turnkey_plan: None, needs_clarify: None, tokens_freed: None,
                                            });
                                        }
                                        _ => {}
                                    }
                                }));
                            } else {
                                let engine = engine.clone();
                                let tx = res_tx.clone();
                                let sbox_tx = sandbox_evt_tx.clone();
                                state.task_registry.register(tokio::spawn(async move {
                                    // V29.10 (C4-Phase2): execute_slash_command 现在返回 (text, Option<TaskSpec>, Option<usize>)
                                    //   非 turnkey 命令: turnkey_plan 恒 None
                                    //   turnkey plan_from_nl 成功: turnkey_plan = Some(task), run.rs 写 state.pending_turnkey_plan
                                    //   tokens_freed: compress 命令返回释放的 token 数，用于同步 session_tokens
                                    let (output, turnkey_plan, tokens_freed) = execute_slash_command(&engine, cmd, sbox_tx).await;
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
                                        needs_clarify: None,
                                        tokens_freed,
                                    });
                                }));
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
                    // 2026-05-28: bracketed paste — 粘贴内容整块插入输入栏（保留换行）
                    // 不触发 submit，用户粘贴后可编辑再手动 Enter 发送
                    Event::Paste(text) => {
                        // 规范化粘贴文本：CRLF→LF，strip tabs→spaces
                        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                        state.input.insert_str(state.cursor_pos, &normalized);
                        state.cursor_pos += normalized.len();
                        state.recalculate_cursor();
                        state.mark_render_dirty();
                        // 2026-05-28: 粘贴 > 5 行时自动打开全屏编辑器
                        let line_count = state.input.matches('\n').count() + 1;
                        if line_count > 5 && state.input_state != crate::tui::state::InputState::Editor {
                            state.open_editor();
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    shutdown.store(true, Ordering::Relaxed);
    // R7：save_session 失败需可见（之前 .ok() 静默吞错，磁盘满/权限错时用户不知）
    let save_err = crate::tui::state::session_export::save_session(&state).err();
    // V28.7: Phase 3 后单实例 flock 已移除（多开靠 UUID 命名 + WAL 隔离），
    // 历史 lockfile 清理逻辑已废止，无需 remove_file（清掉对悬挂 lock_path 的引用）
    guard.deactivate()?;
    let count = state.messages.len();
    match save_err {
        None => println!("Session saved ({} messages, {} turns).", count, state.turn_count),
        Some(e) => eprintln!(
            "[!] Session save failed: {}\n    {} messages may be lost, check ~/.abacus/sessions/ permissions",
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
) -> (String, Option<abacus_types::sandbox::TaskSpec>, Option<usize>) {
    match cmd {
        SlashCommand::TurnkeyPlan(goal) => {
            match engine.core.sandbox_engine().plan_from_nl(&goal).await {
                Ok(task) => {
                    let text = format_turnkey_plan(&goal, &task);
                    (text, Some(task), None)
                }
                Err(e) => (format!("⚠️ Turnkey plan failed: {}\n\nGoal: {}", e, goal), None, None),
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
                Err(e) => format!("⚠️ Turnkey execute failed: {}", e),
            };
            (text, None, None)
        }
        other => {
            let r = execute_slash_command_text(engine, other).await;
            (r.text, None, r.tokens_freed)
        }
    }
}

/// Slash 命令执行结果 — 包含文本输出和可选的 token 统计
struct SlashCommandResult {
    text: String,
    tokens_freed: Option<usize>,
}

/// V29.10: 拆出来的 String-only 子集 — 兼容老调用者
/// 仅处理非 Turnkey* 路径的命令
async fn execute_slash_command_text(engine: &EngineHandle, cmd: SlashCommand) -> SlashCommandResult {
    match cmd {
        SlashCommand::ContextStatus => {
            let status = engine.core.context_status().await;
            SlashCommandResult {
                text: format!(
                    "📊 Context status\n  Usage: {:.1}% ({}/{} tokens)\n  Compressed: {} messages",
                    status.usage_pct, status.current_tokens, status.max_tokens, status.compressed_count,
                ),
                tokens_freed: None,
            }
        }
        SlashCommand::BudgetStatus => {
            let snap = engine.core.llm_budget().snapshot().await;
            // BUG-3 fix: 区分"未启用"和"已启用但当前为 0"
            //   旧实现用 LlmBudgetConfig::default() 的字段当 hint，
            //   而 default 永远是 0.0/0, 用户看到的提示与 config 无关。
            //   snap 字段直接从 cfg 拷贝, 反映用户真实配置的上限值。
            let enabled = snap.max_cost_usd > 0.0 || snap.max_total_tokens > 0;
            let text = if !enabled {
                format!(
                    "💰 LLM budget: **DISABLED**\n  Set [llm_budget] in config.toml:\n    max_cost_usd = {}\n    max_total_tokens = {}\n    (currently unlimited)",
                    snap.max_cost_usd, snap.max_total_tokens
                )
            } else {
                format!("💰 LLM budget: **{snap}**")
            };
            SlashCommandResult { text, tokens_freed: None }
        }
        SlashCommand::ContextCompress => {
            let result = engine.core.compress_context(&engine.session).await;
            SlashCommandResult {
                text: format!("🗜️ Compressed {} messages, freed {} tokens", result.compressed_count, result.tokens_freed),
                tokens_freed: Some(result.tokens_freed),
            }
        }
        SlashCommand::ContextInject(content) => {
            engine.core.inject_context("user_inject", &content).await;
            SlashCommandResult {
                text: format!("💉 Injected ephemeral knowledge (next turn): {}", content.chars().take(50).collect::<String>()),
                tokens_freed: None,
            }
        }
        SlashCommand::ToolList => {
            let tools = engine.core.tool_registry_ref().all_tools().await;
            let active: Vec<_> = tools.iter()
                .filter(|t| matches!(t.state, abacus_types::ToolState::Loaded | abacus_types::ToolState::Active))
                .collect();
            let mut out = format!("🔧 Registered tools ({}):\n", active.len());
            for t in active.iter().take(20) {
                out.push_str(&format!("  • {} — {}\n", t.schema.name, t.schema.description.chars().take(40).collect::<String>()));
            }
            if active.len() > 20 { out.push_str(&format!("  ... +{} more\n", active.len() - 20)); }
            SlashCommandResult { text: out, tokens_freed: None }
        }
        SlashCommand::ToolStats => {
            let stats = engine.core.tool_stats().await;
            let mut sorted = stats;
            sorted.sort_by(|a, b| b.1.composite_score.partial_cmp(&a.1.composite_score).unwrap_or(std::cmp::Ordering::Equal));
            let mut out = String::from("📈 Tool performance Top 10:\n");
            for (name, s) in sorted.iter().take(10) {
                out.push_str(&format!("  [{:?}] {:.2} {}\n", s.tier, s.composite_score, name));
            }
            SlashCommandResult { text: out, tokens_freed: None }
        }
        SlashCommand::SafetyStatus => {
            let s = engine.core.safety_status();
            SlashCommandResult {
                text: format!("🔒 Safety limits (per turn)\n  Max input: {} chars\n  Max tool calls: {}\n  Session: unlimited",
                    s.max_input_length, s.max_total_tool_calls),
                tokens_freed: None,
            }
        }
        SlashCommand::ModelList => {
            let models = engine.core.list_models().await;
            if models.is_empty() {
                SlashCommandResult { text: "🤖 No registered models".to_string(), tokens_freed: None }
            } else {
                let mut out = format!("🤖 Available models ({}):\n", models.len());
                for m in &models { out.push_str(&format!("  • {}\n", m)); }
                SlashCommandResult { text: out, tokens_freed: None }
            }
        }
        SlashCommand::SessionInfo => {
            let s = engine.session.read().await;
            let msg_count = s.messages.read().await.len();
            let map = s.interaction_map.read().await;
            SlashCommandResult {
                text: format!("📋 Session\n  ID: {}\n  Turns: {}\n  Messages: {}\n  Checkpoints: {}",
                    s.session_id, s.turn_count, msg_count, map.checkpoints.len()),
                tokens_freed: None,
            }
        }
        SlashCommand::Provider => {
            let providers = engine.core.list_providers().await;
            if providers.is_empty() {
                SlashCommandResult { text: "⚠️ No registered providers".to_string(), tokens_freed: None }
            } else {
                let lines: Vec<String> = providers.iter()
                    .map(|(id, models)| format!("  {} → [{}]", id, models.join(", ")))
                    .collect();
                SlashCommandResult { text: format!("🔌 Providers ({})\n{}", providers.len(), lines.join("\n")), tokens_freed: None }
            }
        }

        // ─── Phase 4 file-undo dispatch ─────────────────────────
        SlashCommand::UndoLast { session_id } => {
            let text = match engine.undo_engine.undo_last(session_id.as_deref()).await {
                Ok(r) => format_undo_result(&r),
                Err(e) => format!("⚠️ undo failed: {e}"),
            };
            SlashCommandResult { text, tokens_freed: None }
        }
        SlashCommand::UndoSeq { session_id, seq } => {
            let text = match engine.undo_engine.undo_seq(&session_id, seq).await {
                Ok(r) => format_undo_result(&r),
                Err(e) => format!("⚠️ undo seq={seq} failed: {e}"),
            };
            SlashCommandResult { text, tokens_freed: None }
        }
        SlashCommand::UndoTurn { session_id, turn } => {
            let text = match engine.undo_engine.undo_turn(&session_id, turn).await {
                Ok(rs) if rs.is_empty() => format!("turn={turn} no undoable entries"),
                Ok(rs) => {
                    let parts: Vec<String> = rs.iter().map(format_undo_result).collect();
                    format!("⏪ undo turn={} ({} entries):\n{}", turn, rs.len(), parts.join("\n"))
                }
                Err(e) => format!("⚠️ undo turn={turn} failed: {e}"),
            };
            SlashCommandResult { text, tokens_freed: None }
        }
        SlashCommand::Redo { session_id } => {
            let text = match engine.undo_engine.redo(&session_id).await {
                Ok(r) => format!("⏩ redo seq={} → {:?}", r.seq, r.action),
                Err(e) => format!("⚠️ redo failed: {e}"),
            };
            SlashCommandResult { text, tokens_freed: None }
        }
        SlashCommand::UndoHistory { session_id, limit } => {
            let text = match engine.undo_engine.history(session_id.as_deref(), limit) {
                Ok(entries) if entries.is_empty() => "📜 No undo history".to_string(),
                Ok(entries) => format_history(&entries),
                Err(e) => format!("⚠️ history read failed: {e}"),
            };
            SlashCommandResult { text, tokens_freed: None }
        }
        SlashCommand::UndoTimeline { since_hours } => {
            let since = chrono::Utc::now() - chrono::Duration::hours(since_hours as i64);
            // Phase 6：把当前 session_id 传给渲染，用 [you] 标识本窗口
            // 引用：state.session_id 是 EngineHandle 创建时的 session uuid
            let cur_sid = engine.session.read().await.session_id.clone();
            let text = match engine.undo_engine.timeline(since) {
                Ok(entries) if entries.is_empty() =>
                    format!("📜 No writes in past {since_hours}h"),
                Ok(entries) => format_timeline(&entries, since_hours, &cur_sid),
                Err(e) => format!("⚠️ timeline read failed: {e}"),
            };
            SlashCommandResult { text, tokens_freed: None }
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
        // L-3/L-4/L-5: RoleInvoke 走 send_role_message_streaming 流式路径，与 ReviewRole 同型
        SlashCommand::RoleInvoke { .. } => {
            unreachable!("RoleInvoke 应该被 pending_slash_command 处理分支提前截获走流式路径")
        }
        // V34: ExecuteWithPlan/ExecuteWithTeam 走流式路径，同 ReviewRole
        SlashCommand::ExecuteWithPlan { .. } | SlashCommand::ExecuteWithTeam { .. } => {
            unreachable!("ExecuteWithPlan/ExecuteWithTeam 应该被 pending_slash_command 处理分支提前截获走流式路径")
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
        "{} Turnkey complete\n\n**Goal**: {}\n**Status**: {}\n**Phases**: {}\n",
        icon, task.goal, status, task.phases.len()
    );

    // 事件日志（A 路径：完整展示）
    if !events.is_empty() {
        out.push_str(&format!("\n── Event log ({} entries) ──\n", events.len()));
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
            out.push_str(&format!("  ... +{} more events\n", events.len() - 30));
        }
    }
    out
}

/// V29.9 (C4): TaskSpec → 用户友好的 markdown 文本
/// 引用关系: 仅 SlashCommand::TurnkeyPlan 分支调用
fn format_turnkey_plan(goal: &str, task: &abacus_types::sandbox::TaskSpec) -> String {
    let mut out = String::new();
    out.push_str("🎯 Turnkey plan generated\n\n");
    out.push_str(&format!("**Goal**: {}\n\n", goal));
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
    out.push_str("⚠ Preview only. Execute will be connected in future iterations.\n");
    out.push_str("CLI: `abacus turnkey run \"<goal>\" --yes` (experimental).");
    out
}

/// 把 ModelAssignment 标签化, 避免输出长 enum 字面值
fn step_model_label(m: &abacus_types::sandbox::ModelAssignment) -> &'static str {
    use abacus_types::sandbox::ModelAssignment;
    match m {
        ModelAssignment::Auto => "auto",
        ModelAssignment::Fixed { .. } => "fixed",
        ModelAssignment::Execute => "execute",
        ModelAssignment::Verify => "verify",
    }
}

/// Phase 4 渲染 helpers — 简单 +/- 风格，决策 4 = B（不引入 syntect）
fn format_undo_result(r: &abacus_core::undo::UndoResult) -> String {
    use abacus_core::undo::UndoAction;
    let action_str = match r.action {
        UndoAction::RestoredContent => "restored",
        UndoAction::RemovedFile => "removed file",
        UndoAction::RemovedDir => "removed dir",
        UndoAction::ReverseMoved => "reverse rename",
        UndoAction::Aborted => "aborted (conflict)",
    };
    let path_str = r.path.to_string_lossy();
    let header = format!("⏪ undo seq={} session={} ({}): {}",
        r.seq, safe_prefix(&r.session_id, 8), action_str, path_str);

    if let Some(c) = &r.conflict {
        let detail = match c {
            abacus_core::undo::UndoConflict::ExternalModification { observed_sha256, expected_sha256 } =>
                format!("File externally modified\n  expected sha256: {}\n  observed sha256: {}",
                    expected_sha256.get(..16).unwrap_or(expected_sha256),
                    observed_sha256.get(..16).unwrap_or(observed_sha256)),
            abacus_core::undo::UndoConflict::FileGone =>
                "File externally deleted".to_string(),
            abacus_core::undo::UndoConflict::DirectoryNotEmpty { entries } =>
                format!("Directory not empty: {}", entries.join(", ")),
            abacus_core::undo::UndoConflict::DestinationOccupied =>
                "Destination occupied, cannot rename back".to_string(),
        };
        format!("{header}\n  ⚠️ Conflict: {detail}")
    } else {
        header
    }
}

fn format_history(entries: &[abacus_core::undo::HistoryEntry]) -> String {
    let mut out = format!("📜 Undo History ({} entries):\n", entries.len());
    for e in entries {
        let mark = if e.undone { "↺" } else { "✓" };
        let sid_short = safe_prefix(&e.session_id, 8);
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
    let mut out = format!("📜 Project Timeline (past {hours}h, {} entries):\n", entries.len());

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
            let sid_short = safe_prefix(&e.session_id, 8);
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
    let char_count = p.chars().count();
    if char_count <= 60 { return p.to_string(); }
    let mut result = String::with_capacity(62);
    for c in p.chars().take(30) {
        result.push(c);
    }
    result.push('…');
    // 取最后 27 个字符
    let skip = char_count.saturating_sub(27);
    for c in p.chars().skip(skip) {
        result.push(c);
    }
    result
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
///   - 只含 confirm_required=false 的只读/非破坏性工具 + bash_exec(最常用，Medium 级)
///   - fs_write / fs_move / fs_mkdir 保留弹窗(有副作用，首次需用户确认)
///   - 用户可通过 /allow revoke 随时收紧
///
/// 格式: ToolId.0 = schema_name（全下划线，如 fs_read / bash_exec / web_fetch）
const DEFAULT_ALLOW: &[&str] = &[
    // ─── 文件读取 (只读, 无副作用) ───
    "fs_read",
    "fs_read_multiple",
    "fs_info",
    "fs_ls",
    "fs_tree",
    "fs_search",
    "fs_grep",
    // ─── 文件编辑 (精确替换, confirm=false) ───
    "fs_edit",
    // ─── 网络 (只读) ───
    "web_fetch",
    "web_search",
    // ─── Shell (最常用, Medium 级 — 默认允许避免每条命令弹窗) ───
    "bash_exec",
    // ─── 文件写入/移动 (有副作用, 但高频且非破坏性) ───
    "fs_write",
    "fs_move",
    "fs_mkdir",
];

/// 从系统文件加载 always_allow 列表
///
/// 文件不存在(首次启动) → 自动写入 DEFAULT_ALLOW 并返回
/// 文件存在但损坏 → 空集(容错)
/// V29.11 迁移: 旧点号名 "filengine.fs.read" → 下划线 "fs_read"
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
            // 一次性迁移: "filengine.fs.read" → "fs_read"
            let mut set: std::collections::HashSet<String> = raw.into_iter()
                .map(|s| s.replace('.', "_"))
                .collect();
            // Phase 3 (3.7): 仅补充 set 中不存在的新增 DEFAULT 项。
            // 如果用户显式删过某项（文件存在且不含该项），不强加回来。
            // 适用场景：新版本新增了 DEFAULT 工具，用户从未见过该工具——此时补充。
            // —— 无法完美区分"用户删过"与"旧版未包含"，当前策略以简化为主：
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


/// 从 config.toml 读取 [tui.panel] sections 配置。
///
/// 用户可在 `~/.abacus/config.toml` 中写入：
/// ```toml
/// [tui.panel]
/// sections = ["llm", "tools", "local", "palace", "timeline", "focus"]
/// ```
/// 未配置时返回 `None`（使用默认布局）。
/// Process engine responses from the response channel.
/// Extracted from `run_tui()` to reduce main loop complexity.
/// Returns `true` if the main loop should break (currently always returns `false`).
async fn process_engine_response(
    state: &mut AppState,
    res_rx: &mut mpsc::UnboundedReceiver<EngineResponse>,
) -> bool {
    while let Ok(response) = res_rx.try_recv() {
        state.frame_dirty.set(true);
        let ts = chrono::Local::now().format("%H:%M").to_string();

        // V28 (T4): 落档前先 mem::take streaming_trace_ids
        // (流式期间已 push 的 ToolCall trace events 都在 trace_events)。
        // 顺序: take → 创建 Thinking trace → 检查 tool 兜底(非流式)→ reset_streaming
        let mut trace_ids = std::mem::take(&mut state.streaming_trace_ids);

        // 1. Thinking trace — V42-B 优先用 LlmCard.thinking（流式累积）,
        //    fallback 到 response.thinking(非流式 / 一次性返回路径)
        let from_card = state.last_llm_thinking();
        let thinking_text = if !from_card.is_empty() {
            from_card
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
            state.thinking_text = thinking_text.clone();
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

        // 3. 组装 parts: Thinking Block（内联可见）→ Trace（工具）→ Reply
        let mut parts: Vec<MsgContent> = Vec::new();

        // thinking 提升为内联 Block：用户不需要展开 trace 即可看到推理过程
        // V42-B: thinking_text 来自 LlmCard.thinking 或 response.thinking
        // 生命周期：随 Message 持久化，collapsed=true 默认折叠，Space 展开
        if !thinking_text.is_empty() {
            let line_count = thinking_text.lines().count();
            // 摘要：行数 + 首行内容预览（截断到 40 chars）
            // 让用户折叠态也能看到思考方向，决定是否展开
            let first_line = thinking_text.lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            let preview: String = first_line.chars().take(40).collect();
            let summary = if preview.is_empty() {
                format!("💭 {} lines", line_count)
            } else {
                format!("💭 {} lines · {}", line_count, preview)
            };
            parts.push(MsgContent::Block {
                kind: crate::tui::state::BlockKind::Think,
                summary,
                collapsed: true,
                detail: thinking_text.clone(),
            });
        }

        // trace 只保留工具调用（thinking 已单独展示，从 trace_ids 中移除）
        let tool_trace_ids: Vec<u64> = trace_ids.iter().copied()
            .filter(|id| state.trace_events.iter().any(|e|
                e.id == *id && matches!(e.kind, crate::tui::state::TraceKind::ToolCall { .. })
            ))
            .collect();
        if !tool_trace_ids.is_empty() {
            parts.push(MsgContent::Trace {
                event_ids: tool_trace_ids,
                // P1: Trace 默认折叠——减少消息噪音，用户按需展开
                collapsed: true,
                expanded_event_ids: std::collections::HashSet::new(),
            });
        }

        // 2026-06-11: response.text 为空时不要 push 空 Stream part，
        //   避免 add_message 收到一条仅含空内容的消息（浪费渲染 + 噪声占位）
        if !response.text.is_empty() {
            parts.push(MsgContent::Stream(response.text.clone()));
        }
        // 若 parts 仍为空（无 thinking / 无 trace / 无 reply），跳过 add_message
        //   防止 state.messages 累积全空 Message 触发后续渲染 panics
        if parts.is_empty() {
            // 用 warning 事件通知用户，UI 不显示占位消息
            state.add_event(&ts, "llm", "Empty response (no text, no thinking, no tools)", crate::tui::state::EventLevel::Warning);
            continue;
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
                format!("{} Review: {} · {} issue(s){}", icon, report.verdict.label(), report.issues.len(), strict_marker),
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

        // V42-B: 落档完成，reset CardStream
        state.reset_streaming();

        let response_tool_count = response.tool_records.len();
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
            // latest_prompt_tokens：set 语义（非累加）——记录最新轮完整 context 大小
            // 用途：InputBar context % 显示真实 context 窗口占用，不是累计账单 token
            state.session_tokens.latest_prompt_tokens = stats.prompt_tokens;

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

            // Model Escalation 通知：检测到模型切换时同步 state.model_name + 主题色
            if !stats.model_id.is_empty() && stats.model_id != state.model_name {
                state.model_name = stats.model_id.clone();
                state.theme.apply_model_brand(&stats.model_id);
                state.add_toast(
                    format!("🔄 Auto-escalated to {} for deeper reasoning", stats.model_id),
                    Duration::from_secs(5),
                );
            }
        }
        // /compress 命令释放的 token 数 — 同步扣减 session_tokens
        if let Some(freed) = response.tokens_freed {
            state.session_tokens.total_tokens = state.session_tokens.total_tokens.saturating_sub(freed as u64);
            state.session_tokens.prompt_tokens = state.session_tokens.prompt_tokens.saturating_sub(freed as u64);
            state.session_tokens.compress_count += 1;
            state.session_tokens.compress_tokens_saved += freed as u64;
        }
        // Progressive Gate state — 状态在 status bar 已有显示，无需 toast
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
                                crate::tui::event::switch_mode(state, crate::tui::state::AbacusMode::Clarify);
                            }
                            state.add_toast(
                                format!("ℹ️ Auto-fallback to Clarify: {}", reason),
                Duration::from_secs(6),
            );
            state.add_event(
                &ts,
                "session",
                &format!("Auto-fallback to Clarify: {}", reason),
                crate::tui::state::EventLevel::Warning,
            );
        }

        // 2026-05-27: Meeting 路由失败 → needs_clarify 信号 → 自动切到 Clarify
        // 引用关系:
        //   信号源: send_meeting_message_streaming 路由预检返回 NoMatch 时设 needs_clarify
        //   副作用: 切 mode + toast 建议 + 保留用户输入到 preserved_input
        if let Some(ref suggestion) = response.needs_clarify {
            if state.mode != crate::tui::state::AbacusMode::Clarify {
                                crate::tui::event::switch_mode(state, crate::tui::state::AbacusMode::Clarify);
                            }
                            state.add_toast(
                                format!("💡 Suggest clarify: {}", suggestion),
                Duration::from_secs(8),
            );
            state.add_event(
                &ts,
                "session",
                "Meeting route no match, auto-switch to Clarify",
                crate::tui::state::EventLevel::Notice,
            );
        }

        // V41: 策略自动推荐 — Clarify 模式收到响应后分析用户输入复杂度
        // 引用关系：TaskAnalyzer(abacus-core) → toast 建议
        // 触发条件：Clarify 模式 + 本 session 未建议过 + 用户输入足够长
        // 生命周期：meeting_suggested_this_session 标记防重复（/new 重置）
        if state.mode == crate::tui::state::AbacusMode::Clarify
            && !state.meeting_suggested_this_session
        {
            let last_user_text: Option<String> = state.messages.iter().rev()
                .find(|m| matches!(m.role, crate::tui::state::MsgRole::User))
                .and_then(|m| m.parts.iter().find_map(|p| match p {
                    crate::tui::state::MsgContent::Stream(t) => Some(t.clone()),
                    _ => None,
                }));
            if let Some(ref input_text) = last_user_text {
                if input_text.len() > 20 {
                    let cx = abacus_core::core::task_analyzer::TaskAnalyzer::analyze_complexity(input_text);
                    if cx.domain_count >= 2 && cx.score > 0.4 {
                        state.meeting_suggested_this_session = true;
                        state.add_toast(
                            "💡 Multi-domain topic, try /meeting for expert panel".to_string(),
                            Duration::from_secs(8),
                        );
                    } else if cx.dimensions.structural > 0.6 && cx.score > 0.5 {
                        state.meeting_suggested_this_session = true;
                        state.add_toast(
                            "💡 Complex task, try /plan for auto planning".to_string(),
                            Duration::from_secs(6),
                        );
                    }
                }
            }
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
            // V41: 设置 plan_phase = AwaitingApproval（两阶段状态机）
            let task_descs: Vec<String> = task.phases.iter()
                .map(|p| p.description.clone())
                .collect();
            state.plan_phase = Some(crate::tui::state::PlanPhase::AwaitingApproval {
                plan_summary: task.goal.clone(),
                tasks: task_descs,
            });
            state.pending_turnkey_plan = Some(task);
            // 消息流中展示策略选项（用户输入 A/S/T 将在 event/mod.rs 中被拦截）
            state.add_message(crate::tui::state::Message::new_session(
                vec![crate::tui::state::MsgContent::Stream(format!(
                    "📋 Plan ready — {} phases, {} steps\n\n\
                     Choose strategy:\n\
                     [A] Auto — tool calls auto-approved\n\
                     [S] Step-by-step — confirm each op\n\
                     [T] Team — multi-agent parallel\n\
                     [C] Cancel\n\n\
                     Enter A/S/T/C:",
                    phases, steps,
                ))],
                ts.clone(),
            ));
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
                &format!("Participants updated ({})", count),
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
                } else if first.tool_id.contains("bash") || first.tool_id.contains("shell") {
                    let cmd_risk = first.params_preview.as_deref()
                        .map(|p| crate::tui::state::assess_command_risk(p))
                        .unwrap_or(ConfirmRisk::Medium);
                    (ConfirmType::ShellExec, cmd_risk)
                } else {
                    (ConfirmType::NetworkRequest, ConfirmRisk::Medium)
                };
                let mut details = if confirmations.len() > 1 {
                    vec![
                        first.reason.clone(),
                        format!("(+{} more tools need auth)", confirmations.len() - 1),
                    ]
                } else {
                    vec![first.reason.clone()]
                };
                if let Some(ref preview) = first.params_preview {
                    details.push(format!("params: {}", preview));
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
                    selected: 0,
                    interaction_paused: false,
                    paused_total: std::time::Duration::ZERO,
                    focus_lost_at: None,
                    last_active_at: std::time::Instant::now(),
                    suggested_action: first.suggested_action,
                });
                let event_msg = crate::tui::i18n::tf("event.wait_auth_tool", &[&first.tool_id]);
                state.pending_mcip_confirmations = confirmations;
                state.add_event(&ts, "mcip", &event_msg, crate::tui::state::EventLevel::Warning);
            }
        }

        state.add_event(&ts, "llm", crate::tui::i18n::t("event.gen_complete"), crate::tui::state::EventLevel::Notice);

        // 2026-05-27: Clarify 模式下检测是否建议 Meeting
        if state.mode == crate::tui::state::AbacusMode::Clarify
            && !state.meeting_suggested_this_session
        {
            let tool_count = response_tool_count;
            if crate::tui::modes::analyzer::suggest_mode_from_response(
                &response.text,
                tool_count,
                state.meeting_suggested_this_session,
            ).is_some() {
                state.meeting_suggested_this_session = true;
                state.add_toast(
                    "💡 This topic may suit expert panel (/meeting)".to_string(),
                    Duration::from_secs(8),
                );
            }
        }

        // 有待确认工具时，保持 Executing 状态（等用户确认后再恢复）
        if state.pending_mcip_confirmations.is_empty() {
            // Editor 态保护：不覆盖用户正在编辑的状态
            if state.input_state != InputState::Editor {
                state.input_state = InputState::Ready;
            }
            state.op_started_at = None;
            state.accumulated_elapsed = Duration::ZERO;

            // 自动发送排队消息（用户在忙碌态下 Enter 提交的）
            if !state.pending_inputs.is_empty() {
                let next_input = state.pending_inputs.pop_front().unwrap();
                state.input = next_input.clone();
                // RU7 修复：input 改后必须 recalculate_cursor，否则
                // cursor_pos/line/col 持有旧值，渲染时光标位置错位
                state.cursor_pos = state.input.len();
                state.recalculate_cursor();
                state.add_toast(
                    format!("Auto-sending queued ({} remaining)", state.pending_inputs.len()),
                    Duration::from_secs(2),
                );
                state.pending_send = true;
            }
        }
    }
    false
}

/// Process streaming chunks from the stream channel.
/// Extracted from `run_tui()` to reduce main loop complexity.
/// Returns `true` if any streaming update occurred.
async fn process_streaming_chunks(
    state: &mut AppState,
    stream_rx: &mut mpsc::UnboundedReceiver<abacus_core::llm::stream::StreamChunk>,
) -> bool {
    // V0.2: 消费 streaming chunks — 实时更新 partial message（渲染前处理）
    // V40: 全量 drain — 移除 per-frame chunk budget 限制。
    //   旧设计(FRAME_CHUNK_BUDGET=20)的假设是"每 chunk 触发全量重建导致单帧开销过高"。
    //   配合分区渲染优化(streaming 期间不再全量 rebuild)，瓶颈消除，
    //   现在单帧内 drain 所有 pending chunks 再统一渲染一次，延迟从 50ms*N 降为 0。
    //   LLM 实际产出速率 ~100 tokens/s = ~100 chunks/50ms_frame ≈ 5 chunks/frame,
    //   全量 drain 无 CPU 压力。
    let mut had_streaming_update = false;
    while let Ok(chunk) = stream_rx.try_recv() {
        use abacus_core::llm::stream::StreamChunk;
        let ts = chrono::Local::now().format("%H:%M").to_string();
        let chunk_for_cards = chunk.clone();
        match chunk {
            StreamChunk::IterationStart { iteration } => {
                // V42-B: 迭代边界——CardStream handle_chunk 已在前面处理 clear_thinking
                // V42-B: streaming_text_started / streaming_thinking_started 已删除
                if iteration > 0 {
                    state.set_busy_state(InputState::Thinking);
                    state.processing_phase = format!("· iteration {}", iteration + 1);
                    // V40: timeline 迭代分隔
                    state.push_timeline_entry(
                        crate::tui::state::TimelineEntry::Iteration { number: iteration + 1 }
                    );
                }
                had_streaming_update = true;
            }
            StreamChunk::TextDelta(t) => {
                // 100ms 门控：实时估算 ctx_live_tokens（流式期间及时更新上下文占用）
                // 估算 = latest_prompt_tokens（上轮真实值）+ 本轮已生成字符 / 3
                // 除数 3 是英文(~0.25 tok/char)和中文(~1.5 tok/char)的折中
                // 100ms 保证视觉平滑跟动（TUI 帧率 ~60fps），O(1) 计算无性能压力
                let now = std::time::Instant::now();
                let should_refresh = state.ctx_estimate_at
                    .map(|t| now.duration_since(t).as_millis() >= 100)
                    .unwrap_or(true);
                if should_refresh {
                    let gen_est = (state.active_llm_text_len() / 3) as u64;
                    // clamp 到 context_window：估算不可能超过物理上限
                    let raw = state.session_tokens.latest_prompt_tokens
                        .saturating_add(gen_est);
                    let cap = state.context_window as u64;
                    state.ctx_live_tokens = if cap > 0 { raw.min(cap) } else { raw };
                    state.ctx_estimate_at = Some(now);
                }
                // V42-B: 首次 TextDelta 检测（替代 streaming_text_started 标志）
                // handle_chunk 在 match 之后才执行，此时 LlmCard 尚未收到文本
                if !t.is_empty() && state.active_llm_text_len() == 0 {
                    // V38: 切换状态指示到 Outputting
                    state.set_busy_state(InputState::Outputting);
                    state.processing_phase.clear();
                    state.add_event(&ts, "llm", crate::tui::i18n::t("event.outputting"), crate::tui::state::EventLevel::Info);
                }
                // K6：传入实际行内容数组，flash_state 内部计算 hash（避免"底部偏移"漂移）
                let added: Vec<&str> = t.lines().collect();
                if !added.is_empty() {
                    state.flash_state.mark_new_lines(&added);
                }
                // V42-B: timeline Text entry — 从 active LlmCard 取累计长度（零拷贝）
                let card_len = state.active_llm_text_len();
                if let Some(crate::tui::state::TimelineEntry::Text { end, .. }) =
                    state.streaming_timeline.last_mut()
                {
                    *end = card_len;
                } else {
                    state.push_timeline_entry(
                        crate::tui::state::TimelineEntry::Text { start: card_len.saturating_sub(t.len()), end: card_len }
                    );
                }
                // 增量送入 streaming md 引擎（mdstream committed/pending 分割）
                {
                    let mut smd_ref = state.streaming_md.borrow_mut();
                    if smd_ref.is_none() {
                        *smd_ref = Some(crate::tui::md_stream::StreamingMd::new());
                    }
                    if let Some(ref mut smd) = *smd_ref {
                        smd.append(&t);
                    }
                }
                had_streaming_update = true;
            }
            StreamChunk::Thinking(t) => {
                // V42-B: 首次 Thinking 检测（替代 streaming_thinking_started 标志）
                if !t.is_empty() && state.active_llm_thinking_len() == 0 {
                    state.set_busy_state(InputState::Thinking);
                    state.processing_phase.clear();
                    state.add_event(&ts, "llm", crate::tui::i18n::t("event.thinking"), crate::tui::state::EventLevel::Info);
                    // V40: timeline Thinking entry（首次创建，后续仅更新 summary）
                    // 存最近 2 行非空内容（\n 分隔），渲染层展示 2 行 live preview
                    let summary = {
                        let lines: Vec<String> = t.lines()
                            .filter(|l| !l.trim().is_empty())
                            .take(2)
                            .map(|l| if l.chars().count() > 50 {
                                format!("{}…", l.chars().take(47).collect::<String>())
                            } else { l.to_string() })
                            .collect();
                        lines.join("\n")
                    };
                    state.push_timeline_entry(
                        crate::tui::state::TimelineEntry::Thinking { summary }
                    );
                } else if !t.is_empty() {
                    // V42-B: 后续 thinking chunk: 更新 timeline 摘要
                    let thinking_preview = state.active_llm_thinking();
                    let last2: Vec<String> = thinking_preview
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .collect::<Vec<_>>()
                        .into_iter().rev().take(2).rev()
                        .map(|l| if l.chars().count() > 50 {
                            format!("{}…", l.chars().take(47).collect::<String>())
                        } else { l.to_string() })
                        .collect();
                    if let Some(crate::tui::state::TimelineEntry::Thinking { summary }) =
                        state.streaming_timeline.iter_mut().rev()
                            .find(|e| matches!(e, crate::tui::state::TimelineEntry::Thinking { .. }))
                    {
                        if !last2.is_empty() {
                            *summary = last2.join("\n");
                        }
                    }
                }
                // V42-B: thinking 已累积到 CardStream（handle_chunk 处理）
                had_streaming_update = true;
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
                // streaming_tools Vec 上限 100（防止长 turn 积累过多条目）
                if state.streaming_tools.len() >= 100 {
                    state.streaming_tools.remove(0); // FIFO 淘汰最旧
                }
                state.streaming_tools.push((
                    name.clone(),
                    crate::tui::state::StreamingToolStatus::Running,
                    None,
                    trace_id,
                ));
                // V38: 状态栏实时反映当前工具名（Working · tool_name）
                state.set_busy_state(InputState::Executing);
                state.processing_phase = format!("· {}", name);
                // V40: timeline Tool entry（ToolArgs/ToolEnd 会原地更新）
                state.push_timeline_entry(
                    crate::tui::state::TimelineEntry::Tool {
                        name: name.clone(),
                        context: String::new(), // ToolArgs 到达后填充
                        status: crate::tui::state::StreamingToolStatus::Running,
                        duration_ms: None,
                        failure_kind: None,
                        trace_id,
                    }
                );
                had_streaming_update = true;
            }
            // V29.11: 工具输入参数 — 回填 trace event 的 args 字段
            //   触发 try_render_edit_diff: args 含 old_string/new_string → 走 diff 视图
            StreamChunk::ToolArgs { name, args_json } => {
                // T-1 fix: 用 trace_event_index O(1) 查找，替代原来 O(n) iter().find()
                // 2026-05-28: 用 trace_id 精确匹配（修复并行同名工具只更新最后一个的 bug）
                let matched_tid = state.streaming_tools.iter().rev()
                    .find(|(n, s, _, _)| *n == name && *s == crate::tui::state::StreamingToolStatus::Running)
                    .map(|t| t.3);
                if let Some(tid) = matched_tid {
                    // 更新 trace event 的 args
                    if let Some(&idx) = state.trace_event_index.get(&tid) {
                        if let Some(ev) = state.trace_events.get_mut(idx) {
                            if let crate::tui::state::TraceKind::ToolCall { args, .. } = &mut ev.kind {
                                *args = args_json.clone();
                            }
                        }
                    }
                    // 更新 timeline Tool entry 的 context — 用 trace_id 精确匹配
                    let summary = crate::tui::components::block_detail::extract_tool_param_summary(&args_json);
                    if let Some(crate::tui::state::TimelineEntry::Tool { context, .. }) =
                        state.streaming_timeline.iter_mut().rev()
                            .find(|e| matches!(e, crate::tui::state::TimelineEntry::Tool { trace_id: t, .. } if *t == tid))
                    {
                        *context = summary;
                    }
                }
                had_streaming_update = true;
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
                                        format!("🤖 Mode switch: {} — {}", display, reason),
                                        std::time::Duration::from_secs(5),
                                    );
                                    state.add_event(&ts, "session", &format!("LLM switch → {}", display), crate::tui::state::EventLevel::Notice);
                                }
                            }
                        }
                    }
                }
                // V40: timeline ToolOutput — bash/read 工具推送输出摘要
                // 先提取摘要（借用 output_json），再 move 给 trace（避免 clone）
                let tool_lower = name.to_lowercase();
                let is_bash = tool_lower.contains("bash") || tool_lower.contains("exec");
                let is_read = tool_lower.contains("read");
                if (is_bash || is_read) && !output_json.is_empty() {
                    let summary = if let Ok(val) = serde_json::from_str::<serde_json::Value>(&output_json) {
                        let text = val.as_str()
                            .or_else(|| val.get("stdout").and_then(|v| v.as_str()))
                            .or_else(|| val.get("output").and_then(|v| v.as_str()))
                            .unwrap_or("");
                        let first_line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                        if first_line.chars().count() > 60 {
                            format!("{}…", first_line.chars().take(57).collect::<String>())
                        } else {
                            first_line.to_string()
                        }
                    } else {
                        // 非 JSON 输出：取首行
                        let first_line = output_json.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                        if first_line.chars().count() > 60 {
                            format!("{}…", first_line.chars().take(57).collect::<String>())
                        } else {
                            first_line.to_string()
                        }
                    };
                    if !summary.is_empty() {
                        state.push_timeline_entry(
                            crate::tui::state::TimelineEntry::ToolOutput { summary }
                        );
                    }
                }
                // T-1 fix: O(1) index 查找回填 trace event output
                if let Some(tool) = state.streaming_tools.iter().rev()
                    .find(|(n, s, _, _)| *n == name && *s == crate::tui::state::StreamingToolStatus::Running)
                {
                    let tid = tool.3;
                    if let Some(&idx) = state.trace_event_index.get(&tid) {
                        if let Some(ev) = state.trace_events.get_mut(idx) {
                            if let crate::tui::state::TraceKind::ToolCall { output, .. } = &mut ev.kind {
                                *output = Some(output_json);
                            }
                        }
                    }
                }
                had_streaming_update = true;
            }
            StreamChunk::ToolEnd { name, success, duration_ms, failure_kind } => {
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
                // T-1 fix: ToolEnd 中两处 iter().find() 改为 O(1) index 查找
                if success {
                    if let Some(tid) = updated_trace_id {
                        if let Some(&idx) = state.trace_event_index.get(&tid) {
                            if let Some(ev) = state.trace_events.get(idx) {
                                if let crate::tui::state::TraceKind::ToolCall { args, .. } = &ev.kind {
                                    if !args.is_empty() {
                                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(args) {
                                            let path_opt = json.get("path")
                                                .or_else(|| json.get("file_path"))
                                                .and_then(|v| v.as_str());
                                            if let Some(p) = path_opt {
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
                    if let Some(&idx) = state.trace_event_index.get(&tid) {
                        if let Some(ev) = state.trace_events.get_mut(idx) {
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
                }
                // V40: 更新 timeline Tool entry（status + duration + failure_kind）
                let tl_status = if success {
                    crate::tui::state::StreamingToolStatus::Success
                } else {
                    crate::tui::state::StreamingToolStatus::Failed
                };
                if let Some(crate::tui::state::TimelineEntry::Tool {
                    status: ref mut st, duration_ms: ref mut dur, failure_kind: ref mut fk, ..
                }) = state.streaming_timeline.iter_mut().rev()
                    .find(|e| matches!(e, crate::tui::state::TimelineEntry::Tool { name: ref n, status: ref s, .. }
                        if *n == name && *s == crate::tui::state::StreamingToolStatus::Running))
                {
                    *st = tl_status;
                    *dur = Some(duration_ms);
                    *fk = failure_kind;
                }
                had_streaming_update = true;
            }
            StreamChunk::ConfirmRequired(req) => {
                // V28：实时授权请求——pipeline dispatch 已挂起等待用户决策
                //   弹 ConfirmDialog；用户响应后通过 SessionState.mcip_confirm_channels[nonce]
                //   直发 oneshot::Sender（不再 grant_and_rerun 重发整个 turn）
                use crate::tui::state::{ConfirmDialog, ConfirmType, ConfirmRisk, ConfirmOption};
                use abacus_core::mcip::McipConfirmKind;

                // 授权决策：只有用户主动永久授权的工具直接放行
                // 工具语义判断已移至引擎层（pipeline 计算 suggested_action）
                // TUI 不再做关键词匹配，始终弹窗展示系统建议，由系统+用户共同决策
                let auto_allow = state.always_allow.contains(&req.tool_id);

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
                    // 弹窗 — 使用 assess_command_risk 做细粒度分级
                    let reason_destructive = req.reason.starts_with("[destructive]");
                    let is_destructive_flag = matches!(req.kind, McipConfirmKind::DestructiveOp) || reason_destructive;
                    let (confirm_type, risk) = if is_destructive_flag {
                        (ConfirmType::ShellExec, ConfirmRisk::High)
                    } else if req.tool_id.contains("bash") || req.tool_id.contains("shell") {
                        // bash 工具：从 params_preview 提取命令做精确风险评估
                        let cmd_risk = req.params_preview.as_deref()
                            .map(|p| crate::tui::state::assess_command_risk(p))
                            .unwrap_or(ConfirmRisk::Medium);
                        (ConfirmType::ShellExec, cmd_risk)
                    } else {
                        (ConfirmType::NetworkRequest, ConfirmRisk::Medium)
                    };
                    let mut details = vec![req.reason.clone()];
                    if let Some(ref preview) = req.params_preview {
                        details.push(format!("params: {}", preview));
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
                        suggested_action: req.suggested_action,
                    });
                    state.pending_mcip_confirmations = vec![req.clone()];
                    state.add_event(&ts, "mcip", &crate::tui::i18n::tf("event.wait_auth_tool", &[&req.tool_id]), crate::tui::state::EventLevel::Warning);
                }
                state.mark_render_dirty();
            }
            StreamChunk::CompressStart => {
                // 显式状态切换：压缩是可见操作阶段
                state.pre_compress_input_state = Some(state.input_state);
                state.set_busy_state(InputState::Executing);
                state.processing_phase = crate::tui::i18n::t("compress.phase").to_string();
                state.add_toast(crate::tui::i18n::t("compress.toast_start"), Duration::from_secs(3));
                state.mark_render_dirty();
                state.frame_dirty.set(true);
            }
            StreamChunk::CompressEnd { messages_compressed, tokens_saved } => {
                // 显式恢复压缩前状态
                if let Some(prev) = state.pre_compress_input_state.take() {
                    state.input_state = prev;
                }
                if messages_compressed > 0 {
                    state.add_event(
                        &ts, "context",
                        &format!("{} {} msgs · ~{} tok", crate::tui::i18n::t("compress.event"), messages_compressed, tokens_saved),
                        crate::tui::state::EventLevel::Info,
                    );
                    let note = format!(
                        "--- {} ---\n{} messages → summary (~{} tokens freed)",
                        crate::tui::i18n::t("compress.note"), messages_compressed, tokens_saved
                    );
                    state.push_system_note(&note);
                    state.session_tokens.total_tokens = state.session_tokens.total_tokens
                        .saturating_sub(tokens_saved as u64);
                    state.session_tokens.prompt_tokens = state.session_tokens.prompt_tokens
                        .saturating_sub(tokens_saved as u64);
                    state.session_tokens.compress_count += 1;
                    state.session_tokens.compress_tokens_saved += tokens_saved as u64;
                    state.add_toast(
                        format!("{} · -{} tok", crate::tui::i18n::t("compress.toast_done"), tokens_saved),
                        Duration::from_secs(3),
                    );
                }
                state.processing_phase.clear();
                // pending_compress_input 保留：
                //   Execution 阶段 → CompressAutoResume 来了会消费它
                //   Communication 阶段 → interval tick 检测到非 busy 时消费（见下方）
                state.mark_render_dirty();
                state.frame_dirty.set(true);
            }
            StreamChunk::CompressAutoResume => {
                // Execution 阶段压缩完成，自动续行
                // 优先使用用户暂存消息；无暂存则发送续行提示
                let msg = state.pending_compress_input.take()
                    .unwrap_or_else(|| "continue current task".to_string());
                // 压缩完成 toast 已由 CompressEnd 发出，AutoResume 不再重复
                state.input = msg;
                crate::tui::event::submit_message(state);
                state.mark_render_dirty();
                state.frame_dirty.set(true);
            }
            StreamChunk::ToolHealth(entries) => {
                // 工具健康快照：写入 state 供 panel 渲染
                // 引用关系：api/mod.rs 在 Complete 前发送 → 此处消费
                for entry in entries {
                    state.tool_health.insert(entry.tool_id.clone(), entry);
                }
            }
            StreamChunk::Complete(stats) => {
                // 成功完成：清除网络异常标记
                state.connection_error = false;
                // 同步当前 provider_id（配置中的实际 provider，非推断）
                if !stats.provider_id.is_empty() {
                    state.active_provider_id = stats.provider_id.clone();
                }
                // 同步当前 model_id（LLM 端 alias 解析后的真实模型名）
                // 修复：之前 model_name 只在 setup/discover 阶段更新，chat 完成后没同步
                // 导致看板显示 setup 选的模型名，但实际 LLM 调用的是 alias 解析后的名字
                if !stats.model_id.is_empty() && stats.model_id != state.model_name {
                    state.theme.apply_model_brand(&stats.model_id);
                    state.model_name = stats.model_id.clone();
                }
                // Complete: ctx_live_tokens 优先用 context_tokens（含 system prompt/tools），
                // fallback 到 prompt+completion（API usage 返回值）
                state.ctx_live_tokens = stats.context_tokens
                    .unwrap_or_else(|| stats.prompt_tokens.saturating_add(stats.completion_tokens));
                state.ctx_estimate_at = None;
                // 同步 latest_prompt_tokens — 让 Panel/fallback 路径及时反映最新值
                state.session_tokens.latest_prompt_tokens = stats.prompt_tokens;
                // V40: 实时更新上下文窗口占用（面板每帧可见最新数据）
                state.session_tokens.latest_prompt_tokens = stats.prompt_tokens;
                // context_tokens 覆盖 total_tokens（用 context_manager 的精确值替换 API usage）
                if let Some(ctx_tok) = stats.context_tokens {
                    state.session_tokens.total_tokens = ctx_tok;
                }
                if let Some(ctx_max) = stats.context_max {
                    state.context_window = ctx_max as usize;
                }
                if let Some(ml) = stats.model_limit {
                    state.model_max_context = ml as usize;
                }
                // P1: 不在 Complete 时清空 streaming 内容——等 EngineResponse 到达后统一处理
                // 避免 Complete→EngineResponse 间隔导致内容"闪跳"（ST1 改进）
                state.streaming_complete = true;
                // V40: Complete = "当前 LLM 调用完成"，不是 "整个 turn 结束"
                // Pipeline 可能继续执行工具 + 发起新 LLM 调用。
                // 只有 EngineResponse 到达才真正设 Ready。
                // 这里切到 Executing（表示 pipeline 还在工作，可能调工具）
                state.set_busy_state(InputState::Executing);
                state.processing_phase = "· wrapping up...".into();
                state.mark_render_dirty();
                // 不 break — 继续监听后续 chunks（下一轮 ToolStart/TextDelta）
                // 但如果 EngineResponse 已经在 res_rx 里，外层循环会处理
            }
            StreamChunk::AuthResult { tool, approved } => {
                // 授权结果通知：显示 toast，不产生假工具 trace
                let msg = if approved {
                    format!("✓ Authorized {}", tool)
                } else {
                    format!("✗ Denied {} (unsafe op)", tool)
                };
                let dur = if approved {
                    Duration::from_secs(2)
                } else {
                    Duration::from_secs(4)
                };
                state.add_toast(msg, dur);
                state.mark_render_dirty();
            }
            StreamChunk::Error(e) => {
                let is_net = e.starts_with("NETWORK_ERROR:");
                let is_fatal_net = e.starts_with("NETWORK_ERROR:FATAL:");

                // 2026-05-28: 保留已流式的部分内容（fatal 时不丢弃）
                let partial_text = if is_fatal_net && !state.last_llm_text().is_empty() {
                    Some(state.take_last_llm_text())
                } else {
                    None
                };

                state.reset_streaming();
                if state.input_state != InputState::Editor {
                    state.input_state = InputState::Ready;
                }
                state.op_started_at = None;
                state.accumulated_elapsed = Duration::ZERO;
                state.mark_render_dirty();

                if is_net {
                    // 网络错误：标记状态 + 专属提示
                    state.connection_error = true;
                    let msg = if is_fatal_net {
                        "Network connection failed, please check and retry".to_string()
                    } else {
                        // 重试中：不弹 toast，只更新 processing_phase
                        let phase_msg = e.trim_start_matches("NETWORK_ERROR:")
                            .trim_start_matches("retrying ")
                            .to_string();
                        state.processing_phase = format!("⚠ {}", phase_msg);
                        state.add_event(&ts, "network", "Network failed, retrying...", crate::tui::state::EventLevel::Warning);
                        while stream_rx.try_recv().is_ok() {}
                        // 继续处理下一个 chunk（等待重试结果），不 break
                        continue;
                    };
                    // 2026-05-28: 如果有部分内容，保留为系统消息（不丢弃用户已看到的内容）
                    if let Some(partial) = partial_text {
                        state.push_system_note(&format!(
                            "--- Output interrupted (partial received) ---\n{}\n\n⚠ {}",
                            partial, msg
                        ));
                    } else {
                        state.push_system_note(&format!("--- Network error ---\n{}", msg));
                    }
                    state.add_event(&ts, "network", &msg, crate::tui::state::EventLevel::Warning);
                    state.add_toast(
                        format!("⚠ {}", msg),
                        Duration::from_secs(8),
                    );
                } else {
                    // API / 其他错误
                    state.add_event(&ts, "llm", &format!("Error: {}", e), crate::tui::state::EventLevel::Warning);
                    state.add_toast(format!("Stream error: {}", e), Duration::from_secs(5));
                }
                while stream_rx.try_recv().is_ok() {}
                break;
            }
            // 2026-05-28: 流中断后重试——清除已渲染的部分内容，等待重新生成
            StreamChunk::StreamRetryReset { partial_text } => {
                // V42-B: CardStream handle_chunk 已清除 current LlmCard 内容
                // P2: 重置 markdown 增量解析引擎——text 清空后 md 内部偏移不一致
                *state.streaming_md.borrow_mut() = None;
                // 在 timeline 中标注中断点
                if !partial_text.is_empty() {
                    let truncated = if partial_text.len() > 60 {
                        format!("{}...", &partial_text[..60])
                    } else {
                        partial_text
                    };
                    state.add_event(&ts, "stream", &format!("Output interrupted ({} chars), retrying...", truncated.len()),
                        crate::tui::state::EventLevel::Warning);
                }
                state.processing_phase = "Stream interrupted, retrying...".to_string();
                had_streaming_update = true;
            }
            StreamChunk::RetryProgress { attempt, max_attempts, reason } => {
                state.processing_phase = format!("Retry {}/{}: {}", attempt, max_attempts, reason);
                had_streaming_update = true;
            }
            // ── Team 模式进度通知 → 更新 state.tasks 面板 ──
            StreamChunk::TeamProgress { phase, tasks } => {
                state.tasks = tasks.iter().map(|t| {
                    crate::tui::state::TaskCard {
                        id: t.id.clone(),
                        title: t.title.clone(),
                        assignee: String::new(),
                        status: match t.status.as_str() {
                            "running" => crate::tui::state::TaskStatus::InProgress,
                            "done" => crate::tui::state::TaskStatus::Done,
                            "failed" => crate::tui::state::TaskStatus::Blocked,
                            _ => crate::tui::state::TaskStatus::Pending,
                        },
                        progress: match t.status.as_str() {
                            "done" => 100,
                            "running" => 50,
                            _ => 0,
                        },
                        deps: vec![],
                        description: t.output_preview.clone().unwrap_or_default(),
                    }
                }).collect();
                state.processing_phase = format!("team/{}", phase);
                had_streaming_update = true;
            }
            // ── 预留事件处理（轻量级 — trace event + toast）──
            StreamChunk::ModelEscalation { from_model, to_model, reason } => {
                state.add_event(&ts, "model", &format!("{} → {} ({})", from_model, to_model, reason), crate::tui::state::EventLevel::Info);
                state.add_toast(format!("Model escalation: {} → {}", from_model, to_model), Duration::from_secs(3));
                had_streaming_update = true;
            }
            StreamChunk::SessionFocusUpdate { goal, phase, .. } => {
                state.add_event(&ts, "focus", &format!("[{}] {}", phase, goal), crate::tui::state::EventLevel::Info);
                had_streaming_update = true;
            }
            StreamChunk::ToolBlocked { tool_id, kind, message, .. } => {
                state.add_event(&ts, "tool", &format!("⚠ {} blocked: {} ({})", tool_id, message, kind), crate::tui::state::EventLevel::Warning);
                had_streaming_update = true;
            }
            StreamChunk::MeetingStatusChange { new_status, .. } => {
                state.add_event(&ts, "meeting", &format!("Status: {}", new_status), crate::tui::state::EventLevel::Info);
                had_streaming_update = true;
            }
            StreamChunk::SpecialistThinking { specialist_id, content } => {
                state.add_event(&ts, "specialist", &format!("[{}] {}", specialist_id, content.chars().take(60).collect::<String>()), crate::tui::state::EventLevel::Info);
                had_streaming_update = true;
            }
            StreamChunk::SandboxProgress { phase, message } => {
                state.processing_phase = format!("sandbox/{}", phase);
                state.add_event(&ts, "sandbox", &message, crate::tui::state::EventLevel::Info);
                had_streaming_update = true;
            }
            StreamChunk::InertiaDetected { recommendation, .. } => {
                state.add_event(&ts, "inertia", &recommendation, crate::tui::state::EventLevel::Warning);
                state.add_toast(format!("⚠ Loop detected: {}", recommendation), Duration::from_secs(5));
                had_streaming_update = true;
            }
            StreamChunk::LongOperation { tool_name, estimated_secs } => {
                state.add_toast(
                    format!("Long operation ~{}s, please wait...", estimated_secs),
                    Duration::from_secs(estimated_secs.min(10)),
                );
                state.add_event(&ts, "tool", &format!("{} est. {}s", tool_name, estimated_secs), crate::tui::state::EventLevel::Info);
                had_streaming_update = true;
            }
            // V41: ToolAgent 批量执行结果 — 替代多条 ToolStart/ToolEnd 刷屏
            StreamChunk::ToolAgentResult { agent_id: _, icon, name, call_count, summary, details } => {
                // V42-B: LlmCard 写入已由 writer::handle_chunk 处理
                // 这里仅记录 timeline（V40 字段，暂无 CardStream 等价物）
                state.push_timeline_entry(
                    crate::tui::state::TimelineEntry::ToolAgent {
                        icon: icon.clone(),
                        name: name.clone(),
                        call_count,
                        summary: summary.clone(),
                        details,
                    }
                );
                had_streaming_update = true;
            }
        }
        // V42-B: forward to CardStream writer (clone consumed by match above)
        crate::tui::cards::writer::handle_chunk(state, &chunk_for_cards);
    }
    had_streaming_update
}
fn load_panel_sections_from_config() -> Option<Vec<String>> {
    let path = abacus_core::paths::config_toml();
    let content = std::fs::read_to_string(&path).ok()?;
    let toml_val: toml::Value = toml::from_str(&content).ok()?;
    let sections = toml_val.get("tui")?.get("panel")?.get("sections")?;
    match sections {
        toml::Value::Array(arr) => {
            let ids: Vec<String> = arr.iter().filter_map(|v| v.as_str().map(String::from)).collect();
            if ids.is_empty() { None } else { Some(ids) }
        }
        _ => None,
    }
}



