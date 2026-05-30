use crate::commands::TuiArgs;

/// 启动 banner（在 alternate screen 前短暂显示）
///
/// 引用关系：handle_tui 调用，TUI 启动前展示
/// 生命周期：打印后立即进入 alternate screen（banner 被覆盖）
fn print_banner() {
    use std::io::Write;
    let version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")");
    let banner = format!(
        "\x1b[38;5;75m\
         \n\
         \x20   ┌────────────────────────────────┐\n\
         \x20   │          ▄     ▄        ▄      │\n\
         \x20   │ █  █  ▄  █  █  █  █  ▄  █      │\n\
         \x20   │ █  ▀  █  █  █  █  ▀  █  █      │\n\
         \x20   │       ▀              ▀          │\n\
         \x20   └────────────────────────────────┘\n\
         \x20          A  B  A  C  U  S\n\
         \x1b[0m\
         \x20          {}\n\n",
        version
    );
    let _ = std::io::stderr().write_all(banner.as_bytes());
    let _ = std::io::stderr().flush();
}

/// Launch the TUI (terminal user interface)
pub async fn handle_tui(args: &TuiArgs) -> color_eyre::eyre::Result<()> {
    print_banner();
    let chat = args.chat;
    let team = args.team;
    crate::tui::run::run_tui(chat, team).await?;
    Ok(())
}
