use crate::commands::TuiArgs;

/// Launch the TUI (terminal user interface)
pub async fn handle_tui(args: &TuiArgs) -> color_eyre::eyre::Result<()> {
    let chat = args.chat;
    let team = args.team;
    crate::tui::run::run_tui(chat, team).await?;
    Ok(())
}
