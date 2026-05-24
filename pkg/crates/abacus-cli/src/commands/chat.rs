use std::io::Write;
use color_eyre::eyre::Result;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use abacus_core::mcip::{McipConfirmKind, McipGrantDecision};
use crate::ChatArgs;
use crate::OutputFormatter;
use crate::engine_init;

/// 运行一轮 LLM 处理，支持 Ctrl+C 中断（P2）。
///
/// 行为：
/// - 正常完成 → 返回 Ok(Some(TurnResult))
/// - Ctrl+C 触发 → cancel token 让 in-flight reqwest 中断；返回 Ok(None)
/// - LLM 错误 → 返回 Err
async fn run_turn_cancellable(
    core: &std::sync::Arc<abacus_core::CoreLoop>,
    session: &tokio::sync::RwLock<abacus_core::SessionState>,
    input: &str,
) -> Result<Option<abacus_core::TurnResult>> {
    let cancel = tokio_util::sync::CancellationToken::new();
    let token_for_pipeline = cancel.clone();
    let work = core.process_turn_cancellable(input, session, token_for_pipeline);

    let outcome = tokio::select! {
        biased; // 优先检查 Ctrl+C（用户体验：响应速度优先）
        _ = tokio::signal::ctrl_c() => {
            cancel.cancel();
            None
        }
        r = work => Some(r),
    };

    match outcome {
        Some(Ok(r)) => Ok(Some(r)),
        Some(Err(e)) => Err(color_eyre::eyre::eyre!("{e}")),
        None => Ok(None),
    }
}

/// MCIP 授权对话框：展示待授权工具，收集用户决定，重运 turn。
///
/// ## 授权选项
/// - `1` / `y` — 单次允许（仅本次 turn 生效）
/// - `2` / `a` — 总是允许（写入 session 永久授权）
/// - `3` / `n` / Enter — 拒绝（工具保持锁定）
async fn handle_mcip_confirmations(
    core: &std::sync::Arc<abacus_core::CoreLoop>,
    session: &tokio::sync::RwLock<abacus_core::SessionState>,
    confirmations: &[abacus_core::mcip::McipConfirmRequest],
    original_input: &str,
) -> Result<abacus_core::TurnResult> {
    let mut decisions: Vec<(String, McipGrantDecision)> = Vec::new();

    for req in confirmations {
        println!();
        // 根据拦截类型显示不同标题：破坏性操作用警告色 + 参数预览
        match req.kind {
            McipConfirmKind::DestructiveOp => {
                println!("┌─ 🚨 破坏性操作警告 ─────────────────────────────────────────");
                println!("│  工具：  {}", req.tool_id);
                println!("│  警告：  {}", req.reason);
                if let Some(ref preview) = req.params_preview {
                    println!("│  参数：  {}", preview);
                }
            }
            McipConfirmKind::McipPolicy => {
                println!("┌─ ⚠️  MCIP 授权请求 ─────────────────────────────────────────");
                println!("│  工具：  {}", req.tool_id);
                println!("│  原因：  {}", req.reason);
            }
        }
        println!("│");
        println!("│  请选择：");
        println!("│    [1] 单次允许  — 仅本次生效");
        println!("│    [2] 总是允许  — session 内永久生效");
        println!("│    [3] 拒绝      — 阻止执行（默认）");
        print!("└─ 选择 [1/2/3]: ");
        std::io::stdout().flush().ok();

        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap_or(0);
        let choice = line.trim().to_lowercase();

        let decision = match choice.as_str() {
            "1" | "y" | "once" => {
                println!("  ✔ 单次允许 {}\n", req.tool_id);
                McipGrantDecision::Once
            }
            "2" | "a" | "always" => {
                println!("  ✔ 已将 {} 加入永久允许列表\n", req.tool_id);
                McipGrantDecision::Always
            }
            _ => {
                println!("  ✘ 已拒绝 {}\n", req.tool_id);
                McipGrantDecision::Deny
            }
        };
        decisions.push((req.tool_id.clone(), decision));
    }

    // 处理授权决定并重运同一 turn
    core.grant_and_rerun(&decisions, original_input, session).await
        .map_err(|e| color_eyre::eyre::eyre!("MCIP rerun error: {e:?}"))
}

pub async fn handle_chat(args: &ChatArgs, formatter: &mut Box<dyn OutputFormatter>) -> Result<()> {
    formatter.format_message("system", &format!("Model: {} | Thinking: {}", args.model, args.thinking), None);

    // Initialize engine
    let (core, session) = engine_init::create_engine(
        &args.model,
        args.system_prompt.as_deref(),
        &args.thinking,
    ).await?;

    // If initial message provided, process it directly
    if let Some(msg) = &args.message {
        formatter.format_message("user", msg, None);
        match run_turn_cancellable(&core, &session, msg).await {
            Ok(Some(result)) => {
                let thinking = if args.thinking != "off" { result.stats.skills_matched.first().map(|s| s.as_str()) } else { None };
                formatter.format_message("assistant", &result.response, thinking);
                // MCIP 授权请求：展示授权对话框并重运
                if !result.pending_confirmations.is_empty() {
                    handle_mcip_confirmations(&core, &session, &result.pending_confirmations, msg).await?;
                }
                formatter.format_done(0, Some(result.stats.total_tokens), Some(result.stats.latency_ms));
            }
            Ok(None) => {
                formatter.format_message("system", "⚡ 已中断（Ctrl+C）", None);
            }
            Err(e) => {
                formatter.format_error("ENGINE", &e.to_string(), None);
            }
        }
    } else {
        // Interactive REPL with rustyline (history + line editing)
        let history_path = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".abacus")
            .join("history.txt");
        // 防御性：parent() 在根路径下为 None，回退到当前目录
        if let Some(parent) = history_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let mut rl = DefaultEditor::new().ok();
        if let Some(ref mut editor) = rl {
            let _ = editor.load_history(&history_path);
        }

        formatter.format_message("system", "Interactive mode. /quit to exit, /copy to copy last reply, ↑↓ history.", None);
        let mut last_response: Option<String> = None;
        loop {
            let prompt = "› ";
            let line = match rl.as_mut() {
                Some(editor) => match editor.readline(prompt) {
                    Ok(line) => line,
                    Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
                    Err(e) => { formatter.format_error("INPUT", &format!("read error: {e}"), None); break; }
                },
                None => {
                    print!("{}", prompt);
                    std::io::stdout().flush().ok();
                    let mut line = String::new();
                    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 { break; }
                    line.trim().to_string()
                }
            };

            let input = line.trim();
            if input.is_empty() { continue; }
            if input == "/quit" || input == "/exit" { break; }

            if input == "/copy" {
                if let Some(text) = &last_response {
                    crate::output::copy_to_clipboard(text);
                    formatter.format_message("system", &format!("📋 Copied {} chars to clipboard", text.len()), None);
                } else {
                    formatter.format_message("system", "No reply to copy yet", None);
                }
                continue;
            }

            if let Some(ref mut editor) = rl {
                let _ = editor.add_history_entry(input);
            }

            formatter.format_message("user", input, None);
            match run_turn_cancellable(&core, &session, input).await {
                Ok(Some(result)) => {
                    last_response = Some(result.response.clone());
                    formatter.format_message("assistant", &result.response, None);
                    // MCIP 授权请求：展示对话框 + 重运
                    if !result.pending_confirmations.is_empty() {
                        if let Ok(rerun) = handle_mcip_confirmations(&core, &session, &result.pending_confirmations, input).await {
                            last_response = Some(rerun.response.clone());
                            formatter.format_message("assistant", &rerun.response, None);
                        }
                    }
                }
                Ok(None) => {
                    formatter.format_message("system", "⚡ 已中断（Ctrl+C）— 输入下一条消息或 /quit", None);
                }
                Err(e) => {
                    formatter.format_error("ENGINE", &e.to_string(), None);
                }
            }
        }

        if let Some(ref mut editor) = rl {
            let _ = editor.save_history(&history_path);
        }
    }

    Ok(())
}