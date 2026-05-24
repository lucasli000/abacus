use crate::commands::ExecArgs;
use crate::output::*;
use color_eyre::eyre::Result;

/// Run in pipe mode: read stdin, combine with task, execute via engine.
/// Used when `abacus run -t "task"` receives piped input.
pub async fn run_pipe_mode(args: &ExecArgs, formatter: &mut Box<dyn OutputFormatter>) -> Result<i32> {
    // Read all piped input
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;

    // Combine task + stdin context
    let task = if !input.trim().is_empty() {
        format!("{}\n\n---\n[Piped input]:\n{}", args.task, input.trim())
    } else {
        args.task.clone()
    };

    formatter.format_message("user", &task, None);

    // Execute via engine (same path as `abacus run` without pipe)
    let model = &args.model;
    match crate::engine_init::create_engine(model, None, "medium").await {
        Ok((core, session)) => {
            match core.process_turn(&task, &session).await {
                Ok(result) => {
                    formatter.format_message("assistant", &result.response, None);
                    Ok(0)
                }
                Err(e) => {
                    formatter.format_message("error", &e.user_message(), None);
                    Ok(1)
                }
            }
        }
        Err(e) => {
            formatter.format_message("error", &format!("引擎初始化失败: {}", e), None);
            Ok(1)
        }
    }
}