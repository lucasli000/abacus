// L2-10: 允许死代码 — 为未来功能预留
#![allow(dead_code)]

// V14：bin crate 与 lib crate 编译时分别检查 lints；lib.rs 的 allow 不传染到 bin。
// 这里同步 lib.rs 的风格类放行清单，保持 cargo clippy --all-targets 0-warning
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_match)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::manual_div_ceil)]
#![allow(clippy::manual_is_multiple_of)]
#![allow(clippy::manual_clamp)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::unnecessary_filter_map)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::if_same_then_else)]
#![allow(clippy::useless_conversion)]
#![allow(clippy::manual_contains)]
#![allow(clippy::filter_map_bool_then)]
#![allow(clippy::map_flatten)]
#![allow(clippy::manual_checked_ops)]
#![allow(clippy::vec_init_then_push)]
#![allow(clippy::type_complexity)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::manual_strip)]
#![allow(clippy::expect_fun_call)]
#![allow(clippy::while_let_loop)]
#![allow(clippy::unnecessary_unwrap)]

use clap::{Parser, CommandFactory};
use clap_complete::{generate, Shell};
use color_eyre::eyre::Result;
use tracing_subscriber::EnvFilter;

mod commands;
mod output;
mod pipe;
mod engine_init;
mod tui;

use commands::*;
use output::*;
use pipe::*;

/// Abacus - LLM Agent Kernel CLI
#[derive(Parser, Debug)]
#[command(name = "abacus")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "CLI interface for Abacus LLM Agent kernel", long_about = None)]
struct Cli {
    /// Output format
    #[arg(long = "format", short = 'f', value_enum, default_value = "markdown")]
    format: OutputFormat,

    /// Enable event stream output
    #[arg(long = "events")]
    events: bool,

    /// Verbose mode (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Subcommands (optional — defaults to TUI Chat mode)
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Parser, Debug)]
enum Commands {
    /// Chat with LLM agent (REPL mode)
    Chat(ChatArgs),

    /// Run a single task (one-shot execution, supports pipe input)
    #[command(alias = "exec")]
    Run(ExecArgs),

    /// Manage sessions
    Session(SessionArgs),

    /// Manage skills
    Skill(SkillArgs),

    /// Manage configuration
    Config(ConfigArgs),

    /// Manage models
    Model(ModelArgs),

    /// Doctor - check system status
    Doctor,

    /// Meeting — expert consultation mode (Mode 3)
    Meeting(commands::meeting::MeetingArgs),

    /// Manage MCP servers and plugins
    Mcp(McpArgs),

    /// Team mode — multi-agent orchestration (Mode 2)
    Team(TeamArgs),

    /// Turnkey — fully-managed task execution
    Turnkey(TurnkeyArgs),

    /// TUI — terminal user interface (interactive modes)
    Tui(TuiArgs),

    /// Generate shell completions for bash/zsh/fish
    Completions {
        /// Shell type
        #[arg(value_enum)]
        shell: Shell,
    },

    // ─── Phase 5 file-undo CLI ─────────────────────────────────
    /// Undo file operations (fs.write/edit/move/mkdir) recorded by agent
    Undo(commands::UndoArgs),

    /// Redo last undone operation (within same process — redo stack is in-memory)
    Redo(commands::RedoArgs),

    /// Show undo history (current session) or project timeline (--project)
    History(commands::HistoryArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    // i18n: 检测系统语言（ABACUS_LANG > LC_ALL > LANG，默认 En）
    crate::tui::i18n::init_lang();

    std::panic::set_hook(Box::new(|info| {
        eprintln!("[FATAL] Abacus panicked: {}", info);
    }));

    // Initialize logging
    // TUI 模式下跳过 stderr subscriber——由 run_tui() 初始化文件 writer，
    // 否则日志输出到 stderr 会穿透 TUI alternate screen 渲染。
    let is_tui_mode = {
        let pre_cli = Cli::parse();
        matches!(
            pre_cli.command.as_ref().unwrap_or(&Commands::Tui(TuiArgs { chat: true, team: false, meeting: false })),
            Commands::Tui(_)
        )
    };

    if !is_tui_mode {
        let filter = match std::env::var("RUST_LOG") {
            Ok(val) => val,
            Err(_) => match Cli::parse().verbose {
                0 => "abacus_cli=info,abacus_core=info,abacus_engine=warn".to_string(),
                1 => "abacus_cli=debug,abacus_core=debug,abacus_engine=info".to_string(),
                2 => "abacus_cli=trace,abacus_core=trace,abacus_engine=debug".to_string(),
                3 => "trace".to_string(),
                _ => "trace".to_string(),
            },
        };
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(filter))
            .init();
    }

    let cli = Cli::parse();
    let mut formatter = get_formatter(cli.format);

    tracing::debug!("CLI parsed: {:?}", cli);

    // 无子命令 → 默认进入 TUI Chat 模式
    let command = cli.command.as_ref().unwrap_or(&Commands::Tui(TuiArgs {
        chat: true,
        team: false,
        meeting: false,
    }));

    match command {
        Commands::Chat(args) => {
            commands::chat::handle_chat(args, &mut formatter).await?;
        }

        Commands::Run(args) => {
            if is_input_available()? {
                let exit_code = run_pipe_mode(args, &mut formatter).await?;
                formatter.format_done(exit_code, None, None);
            } else {
                commands::exec::handle_exec(args, &mut formatter).await?;
            }
        }

        Commands::Session(args) => commands::session::handle_session(args, &mut formatter).await?,
        Commands::Skill(args) => commands::skill::handle_skill(args, &mut formatter).await?,
        Commands::Config(args) => commands::config::handle_config(args, &mut formatter).await?,
        Commands::Model(args) => commands::model::handle_model(args, &mut formatter).await?,
        Commands::Doctor => commands::doctor::handle_doctor(&mut formatter).await?,
        Commands::Meeting(args) => commands::meeting::handle_meeting(args, &mut formatter).await?,
        Commands::Mcp(args) => commands::mcp::handle_mcp(args, &mut formatter).await?,
        Commands::Team(args) => commands::team::handle_team(args, &mut formatter).await?,
        Commands::Turnkey(args) => commands::turnkey::handle_turnkey(args, &mut formatter).await?,
        Commands::Tui(args) => commands::tui::handle_tui(args).await?,
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            generate(*shell, &mut cmd, "abacus", &mut std::io::stdout());
        }
        Commands::Undo(args) => commands::undo::handle_undo(args).await
            .map_err(|e| color_eyre::eyre::eyre!(e))?,
        Commands::Redo(args) => commands::undo::handle_redo(args).await
            .map_err(|e| color_eyre::eyre::eyre!(e))?,
        Commands::History(args) => commands::undo::handle_history(args).await
            .map_err(|e| color_eyre::eyre::eyre!(e))?,
    }

    Ok(())
}

// Helper to check if there's input available on stdin
fn is_input_available() -> Result<bool> {
    use std::io::{BufRead, BufReader};
    let mut reader = BufReader::new(std::io::stdin().lock());
    let buf = reader.fill_buf().map_err(|e| std::io::Error::other(e))?;
    Ok(!buf.is_empty())
}

