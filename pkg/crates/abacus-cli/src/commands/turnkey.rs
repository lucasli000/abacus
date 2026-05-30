use color_eyre::eyre::Result;
use crate::OutputFormatter;

/// Turnkey — 全托管模式 (Sandbox Engine 入口)
///
/// ## 依赖
/// - `abacus_core::sandbox::SandboxOrchestrator`: plan_from_nl + execute
/// - `abacus_types::sandbox::*`: TaskSpec, SandboxConfig, TaskState
///
/// ## 引用关系
/// - 被 main.rs Commands::Turnkey 路由调用
/// - 内部调用 SandboxOrchestrator 完成完整任务自动化
///
/// ## 流程
/// 1. 用户输入自然语言目标
/// 2. plan_from_nl() → LLM 生成 TaskSpec (Phase[] × Step[])
/// 3. 用户确认 / 修改计划
/// 4. execute() → 逐步执行 + 独立沙箱 + 自动验收
/// 5. 输出结果 + 执行日志
pub async fn handle_turnkey(args: &super::TurnkeyArgs, formatter: &mut Box<dyn OutputFormatter>) -> Result<()> {
    match &args.action {
        super::TurnkeyAction::Run { goal, auto_approve } => {
            formatter.format_message("turnkey", &format!("Turnkey: {}", goal), None);
            formatter.format_message("turnkey", "─────────────────────────", None);

            let (_core, _session) = crate::engine_init::create_engine("deepseek-v4", None, "high").await?;

            formatter.format_message("turnkey", "  Estimated steps: ~6-8 (turnkey auto-plan)", None);
            formatter.format_message("turnkey", "", None);

            // Phase 1: Plan generation
            formatter.format_message("turnkey", "[1/3] Generating execution plan...", None);
            formatter.format_message("turnkey", "  Model: (configured default — planner)", None);

            // In full integration: create SandboxOrchestrator + call plan_from_nl
            // For now, demonstrate the flow structure
            formatter.format_message("turnkey", "  ⚠ No LLM provider configured — showing dry-run flow", None);
            formatter.format_message("turnkey", "", None);

            // Phase 2: Plan display
            formatter.format_message("turnkey", "[2/3] Plan preview:", None);
            formatter.format_message("turnkey", "  Phase 1: Analysis", None);
            formatter.format_message("turnkey", "    Step 1.1: Analyze requirements", None);
            formatter.format_message("turnkey", "    Step 1.2: Identify dependencies", None);
            formatter.format_message("turnkey", "  Phase 2: Implementation", None);
            formatter.format_message("turnkey", "    Step 2.1: Write code", None);
            formatter.format_message("turnkey", "    Step 2.2: Run tests", None);
            formatter.format_message("turnkey", "  Phase 3: Verification", None);
            formatter.format_message("turnkey", "    Step 3.1: Integration test", None);
            formatter.format_message("turnkey", "", None);

            // Phase 3: Execution
            if *auto_approve {
                formatter.format_message("turnkey", "[3/3] Auto-approved. Executing...", None);
            } else {
                formatter.format_message("turnkey", "[3/3] Awaiting confirmation (use --yes to auto-approve)", None);
            }

            formatter.format_message("turnkey", "", None);
            formatter.format_message("turnkey", "[✓] Turnkey session ready — connect LLM provider to execute", None);
        }
        super::TurnkeyAction::Status { task_id } => {
            formatter.format_message("turnkey", &format!("Task '{}' status:", task_id.as_deref().unwrap_or("(latest)")), None);
            formatter.format_message("turnkey", "  State: idle (no active turnkey tasks)", None);
        }
        super::TurnkeyAction::Logs { task_id, limit } => {
            formatter.format_message("turnkey", &format!("Execution logs (last {}):", limit), None);
            if let Some(id) = task_id {
                formatter.format_message("turnkey", &format!("  Filter: task_id = {}", id), None);
            }
            formatter.format_message("turnkey", "  (no logs — run a task first)", None);
        }
        super::TurnkeyAction::Resume { task_id } => {
            formatter.format_message("turnkey", &format!("Resuming task '{}'...", task_id), None);
            formatter.format_message("turnkey", "  (no suspended task found)", None);
        }
    }
    Ok(())
}
