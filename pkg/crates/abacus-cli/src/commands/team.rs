use std::sync::OnceLock;
use color_eyre::eyre::Result;
use crate::OutputFormatter;
use abacus_orchestrator::team::{TeamManager, TeamBuilder, AgentRole};

static TEAM_MGR: OnceLock<TeamManager> = OnceLock::new();

fn manager() -> &'static TeamManager {
    TEAM_MGR.get_or_init(TeamManager::new)
}

pub async fn handle_team(args: &super::TeamArgs, formatter: &mut Box<dyn OutputFormatter>) -> Result<()> {
    match &args.action {
        super::TeamAction::Start { goal, roles } => {
            let (core, _session) = crate::engine_init::create_engine("deepseek-v4", None, "high").await?;
            let mgr = manager();
            let team_id = format!("team_{}", chrono::Utc::now().timestamp_millis());

            let mut builder = TeamBuilder::new(&team_id, goal);
            if let Some(role_str) = roles {
                for _role in role_str.split(',').map(|r| r.trim()) {
                    builder = builder.with_role(AgentRole::Member);
                }
            }
            // Add a default task based on goal
            builder = builder.with_task(abacus_orchestrator::team::TaskSpec {
                id: format!("task_{}", chrono::Utc::now().timestamp_millis()),
                description: goal.clone(),
                required_capabilities: vec![],
                allowed_tools: vec![],
                priority: 0,
                depends_on: vec![],
                required_role: None,
            });

            let session = builder.build();
            let registered = mgr.register(session).await;
            formatter.format_message("team", &format!("[✓] Team '{}' created", registered.team_id), None);
            formatter.format_message("team", &format!("    Goal: {}", goal), None);
            if let Some(r) = roles {
                formatter.format_message("team", &format!("    Roles: {}", r), None);
            }

            // Execute team tasks
            formatter.format_message("team", "▶ Executing team tasks...", None);
            let start = std::time::Instant::now();
            match registered.execute_ready_tasks(&core).await {
                Ok(results) => {
                    let elapsed = start.elapsed();
                    for (task_id, response) in results.iter() {
                        formatter.format_message("assistant", &format!("[{}] {}", task_id, response), None);
                    }
                    formatter.format_done(0, None, Some(elapsed.as_millis() as u64));
                }
                Err(e) => {
                    formatter.format_error("TEAM", &format!("Task execution failed: {}", e), None);
                }
            }
        }
        super::TeamAction::Status { team_id } => {
            let mgr = manager();
            if let Some(id) = team_id {
                if let Some(session) = mgr.get(id).await {
                    let status = session.status().await;
                    formatter.format_message("team", &format!("Team '{}': {:?}", id, status), None);
                } else {
                    formatter.format_message("team", &format!("Team '{}' not found", id), None);
                }
            } else {
                formatter.format_message("team", "Usage: team status <team_id>", None);
            }
        }
        super::TeamAction::List => {
            let mgr = manager();
            let list = mgr.list().await;
            if list.is_empty() {
                formatter.format_message("team", "Active teams: (none)", None);
            } else {
                formatter.format_message("team", "Active teams:", None);
                for id in &list {
                    formatter.format_message("team", &format!("  - {}", id), None);
                }
            }
        }
        super::TeamAction::Stop { team_id } => {
            let mgr = manager();
            if mgr.remove(team_id).await {
                formatter.format_message("team", &format!("[✓] Team '{}' stopped", team_id), None);
            } else {
                formatter.format_message("team", &format!("Team '{}' not found", team_id), None);
            }
        }
    }
    Ok(())
}
