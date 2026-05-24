use color_eyre::eyre::Result;
use clap::Parser;
use crate::OutputFormatter;
use abacus_orchestrator::meeting::{MeetingManager, SpecialistConfig};

#[derive(Parser, Debug)]
pub struct MeetingArgs {
    /// 会议主题
    #[arg(long = "topic", short = 't', default_value = "Untitled Meeting")]
    pub topic: String,

    /// 参会专家 (可多次指定)
    #[arg(long = "specialist", short = 's')]
    pub specialists: Vec<String>,

    /// YAML 配置文件路径 (examples/ 目录下有示例)
    #[arg(long = "config", short = 'c')]
    pub config: Option<String>,

    /// 会议模式: demo / live
    #[arg(long = "mode", default_value = "demo")]
    pub mode: String,

    /// Model to use
    #[arg(long = "model", short = 'm', default_value = "deepseek-v4-flash")]
    pub model: String,
}

pub async fn handle_meeting(
    args: &MeetingArgs,
    formatter: &mut Box<dyn OutputFormatter>,
) -> Result<()> {
    let title = format!("🚀 启动会议: {}", args.topic);
    formatter.format_message("system", &title, None);

    let specialist_list = if args.specialists.is_empty() {
        "（请在 CLI 中指定，或在配置文件中定义）".to_string()
    } else {
        args.specialists.join(", ")
    };
    formatter.format_message("system", &format!("👤 专家: {}", specialist_list), None);
    formatter.format_message("system", &format!("📋 模式: {}", args.mode), None);
    if let Some(cfg) = &args.config {
        formatter.format_message("system", &format!("📄 配置: {}", cfg), None);
    }
    formatter.format_message("system", "", None);

    if args.mode == "live" {
        return handle_live_meeting(args, formatter).await;
    }

    let mut builder = abacus_orchestrator::meeting::MeetingSessionBuilder::new(&args.topic);
    if let Some(path) = &args.config {
        builder = builder.with_config_file(path);
    }
    for sp_id in &args.specialists {
        builder = builder.with_specialist(sp_id);
    }
    let mut handle = builder.build().await
        .map_err(|e| color_eyre::eyre::eyre!("创建会议失败: {}", e))?;

    handle.session_mut().status = abacus_orchestrator::meeting::MeetingStatus::Inviting;
    handle.start()?;
    formatter.format_message("system", "✅ 会议已开始，输入 '/help' 查看命令", None);
    {
        let names: Vec<&str> = handle.session().participants.keys()
            .filter_map(|k| k.split_once("sp-").and_then(|(_, rest)| rest.split_once('-')))
            .map(|(name, _)| name)
            .collect();
        let hint = if names.is_empty() {
            "参会专家名单为空".to_string()
        } else {
            let mentions: Vec<String> = names.iter().map(|n| format!("@{}", n)).collect();
            format!("💡 输入 {} 等自然语言来与专家对话", mentions.join("、"))
        };
        formatter.format_message("system", &hint, None);
    }
    formatter.format_message("system", "", None);

    let demo_inputs: Vec<String> = handle.session().participants.keys()
        .filter_map(|k| {
            let after = k.strip_prefix("sp-")?;
            let name = after.split('-').next()?;
            Some(format!("请 @{} 分析当前问题", name))
        })
        .collect();

    for input in &demo_inputs {
        formatter.format_message("user", input, None);

        let decision = handle.route_debug(input);
        formatter.format_message("system", &format!("📡 路由决策: {:?}", decision), None);

        let sp_id = match &decision {
            abacus_orchestrator::meeting::RoutingDecision::Direct(id, _) => id.0.clone(),
            _ => {
                formatter.format_message("assistant", "当前无匹配专家，请尝试 @coder / @reviewer", None);
                continue;
            }
        };

        let sp = match handle.session().participants.get(&sp_id) {
            Some(sp) => sp,
            None => {
                formatter.format_message("system", &format!("⚠️  专家 {} 不在会议中", sp_id), None);
                continue;
            }
        };
        let mode = match &decision {
            abacus_orchestrator::meeting::RoutingDecision::Direct(_, mode) => mode.clone(),
            _ => continue,
        };

        let prompt = abacus_orchestrator::meeting::MeetingPromptAssembler::assemble_specialist_prompt(
            &handle.session().topic,
            &handle.session().participants,
            &handle.session().context_pool,
            sp,
            &mode,
        );
        formatter.format_message("system", &format!("📝 {} 的 prompt (前 500 字符):", sp.name), None);
        let preview: String = prompt.chars().take(500).collect();
        formatter.format_message("assistant", &format!("```\n{}\n...```", preview), None);

        formatter.format_message("system", "🔍 harness pre_check 通过 (模拟)", None);

        let opinion = abacus_orchestrator::specialist::SpecialistOpinion {
            specialist_id: sp.id.clone(),
            turn: handle.session().context_pool.turn_count() + 1,
            conclusion: format!("[{} 的 demo 回复] 收到问题: {}", sp.name, input),
            confidence: 0.85,
            reasoning_summary: "这是 demo 模式的模拟推理过程".into(),
            tool_evidence: vec![],
            suggestions: vec![],
            requires_attention: vec![],
            auto_approve: true,
            host_review_required: false,
        };

        let _ = handle.session_mut().process_opinion(opinion.clone());
        formatter.format_message("assistant", &opinion.conclusion, None);

        while let Ok(event) = handle.try_recv_event() {
            let event_str = match &event {
                abacus_orchestrator::meeting::MeetingEvent::ParticipantJoined { name, .. } =>
                    format!("📥 {} 加入会议", name),
                abacus_orchestrator::meeting::MeetingEvent::SpecialistOpinionReady { specialist_id, .. } =>
                    format!("💬 {} 发表意见", specialist_id.0),
                _ => format!("📡 {:?}", event),
            };
            formatter.format_message("debug", &event_str, None);
        }

        formatter.format_message("system", "───", None);
    }

    let minutes = handle.generate_minutes();
    formatter.format_message("system", "\n📋 会议纪要", None);
    formatter.format_message("system", &format!("总轮次: {}", minutes.total_turns), None);
    for c in &minutes.conclusions {
        formatter.format_message("assistant", &format!("• {}", c), None);
    }

    handle.complete()?;
    formatter.format_message("system", "\n✅ 会议结束", None);

    Ok(())
}

async fn handle_live_meeting(
    args: &MeetingArgs,
    formatter: &mut Box<dyn OutputFormatter>,
) -> Result<()> {
    let (core, session) = crate::engine_init::create_engine(&args.model, None, "high").await?;

    let mut manager = MeetingManager::new(core, session, args.topic.clone());

    if args.specialists.is_empty() {
        formatter.format_message("system", "⚠️  未指定专家，使用 --specialist 添加", None);
        return Ok(());
    }

    for sp_id in &args.specialists {
        manager.add_specialist(SpecialistConfig {
            id: sp_id.clone(),
            name: sp_id.clone(),
            model: args.model.clone(),
            system_prompt: format!("You are a domain expert in {}.", sp_id),
            role: abacus_orchestrator::team::AgentRole::Member,
        });
    }

    manager.build().await
        .map_err(|e| color_eyre::eyre::eyre!("创建会议失败: {}", e))?;

    let results = manager.run_all().await
        .map_err(|e| color_eyre::eyre::eyre!("会议运行失败: {}", e))?;

    formatter.format_message("system", "\n📋 会议结论", None);

    for result in &results {
        let sp_name = &result.target_specialist.0;
        formatter.format_message("system", &format!("\n─── {} ───", sp_name), None);
        formatter.format_message("assistant", &result.engine_output, None);
        if let Some(opinion) = &result.opinion {
            formatter.format_message("system", &format!("置信度: {:.2}", opinion.confidence), None);
        }
    }

    formatter.format_message("system", "\n✅ 会议结束", None);
    Ok(())
}
