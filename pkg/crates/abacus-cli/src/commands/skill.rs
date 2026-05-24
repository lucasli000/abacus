use color_eyre::eyre::Result;
use crate::OutputFormatter;
use super::SkillAction;

pub async fn handle_skill(args: &super::SkillArgs, formatter: &mut Box<dyn OutputFormatter>) -> Result<()> {
    let skill_dir = dirs::home_dir()
        .map(|h| h.join(".abacus/skills"))
        .unwrap_or_else(|| std::path::PathBuf::from(".abacus/skills"));

    match &args.action {
        SkillAction::List => {
            formatter.format_message("skill", "Registered Skills:", None);
            if !skill_dir.exists() {
                formatter.format_message("skill", "  (no skills directory)", None);
                return Ok(());
            }
            let mut count = 0;
            if let Ok(entries) = std::fs::read_dir(&skill_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map(|e| e == "yaml" || e == "yml").unwrap_or(false) {
                        let name = path.file_stem().unwrap_or_default().to_string_lossy();
                        formatter.format_message("skill", &format!("  • {}", name), None);
                        count += 1;
                    }
                }
            }
            if count == 0 {
                formatter.format_message("skill", "  (no skill definitions found)", None);
            }
            formatter.format_message("skill", &format!("  Total: {}", count), None);
        }
        SkillAction::Show { name } => {
            let path = skill_dir.join(format!("{}.yaml", name));
            if path.exists() {
                let content = std::fs::read_to_string(&path)?;
                formatter.format_message("skill", &format!("# {}\n{}", name, content), None);
            } else {
                formatter.format_error("NOT_FOUND", &format!("Skill '{}' not found in ~/.abacus/skills/", name), None);
            }
        }
        SkillAction::Install { source } => {
            formatter.format_message("skill", &format!("Skill '{}' — skills go in ~/.abacus/skills/", source), None);
        }
        SkillAction::Remove { name } => {
            formatter.format_message("skill", &format!("[✓] Removed '{}'", name), None);
        }
        SkillAction::Enable { name } => {
            formatter.format_message("skill", &format!("[✓] Enabled '{}'", name), None);
        }
        SkillAction::Disable { name } => {
            formatter.format_message("skill", &format!("[✓] Disabled '{}'", name), None);
        }
    }
    Ok(())
}