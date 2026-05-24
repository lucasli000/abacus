use color_eyre::eyre::Result;
use crate::ExecArgs;
use crate::OutputFormatter;
use crate::engine_init;

pub async fn handle_exec(args: &ExecArgs, formatter: &mut Box<dyn OutputFormatter>) -> Result<()> {
    formatter.format_message("system", &format!("Task: {} | Model: {} | Timeout: {}s", args.task, args.model, args.timeout), None);

    let (core, session) = engine_init::create_engine(&args.model, None, "high").await?;

    // Execute with timeout
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(args.timeout),
        core.process_turn(&args.task, &session),
    ).await;

    match result {
        Ok(Ok(turn)) => {
            formatter.format_message("assistant", &turn.response, None);
            if !turn.tool_outputs.is_empty() {
                for o in &turn.tool_outputs {
                    formatter.format_tool_call(&o.tool_id.0, &o.output, None, if o.success { "ok" } else { "fail" });
                }
            }
            formatter.format_done(0, Some(turn.stats.total_tokens), Some(turn.stats.latency_ms));
        }
        Ok(Err(e)) => {
            formatter.format_error("ENGINE", &e.to_string(), None);
            formatter.format_done(1, None, None);
        }
        Err(_) => {
            formatter.format_error("TIMEOUT", &format!("Task exceeded {}s timeout", args.timeout), None);
            formatter.format_done(1, None, None);
        }
    }

    Ok(())
}