//! Pipeline Phase 5 + 6: post_process & detect_inertia
//!
//! ## 引用关系
//! - 由 `pipeline::mod::TurnPipeline::run` 顺序调用：execute_loop → post_process → detect_inertia
//! - inherent impl 块跨文件分布——见 mod.rs 的 TurnPipeline 主类型定义
//!
//! ## 生命周期
//! - 仅在每次 turn 的 Phase 5/6 阶段执行一次
//! - 写入 ctx.final_response（epistemic 违规通知）和 ctx.inertia_warning
//! - 无持久副作用——所有 self.core 的写操作（injector/effectiveness/skill engine 等）都通过 RwLock

use std::collections::HashMap;

use abacus_types::SkillExecutionRecord;
use chrono::Utc;

use crate::core::interaction::{MapAnalyzer, ToolCallRecord};
use crate::core::inertia;
use crate::llm::{LlmRequest, Message, MessageContent, MessageRole};

use super::TurnContext;
use super::TurnPipeline;

impl<'a> TurnPipeline<'a> {
    pub(super) async fn post_process(&self, ctx: &mut TurnContext) {
        if ctx.final_response.is_empty() {
            // final_response 在 execute_loop 中设置；空值表示 max_turns 耗尽，
            // 由 persist_and_build_result 兜底处理。
        }

        // Context compression — 仅在实际压缩发生时才 emit 事件通知 TUI
        {
            let s = self.session.read().await;
            let mut msgs = s.messages.write().await;
            let compressed = self.core.context_manager.auto_compress_messages(&mut msgs).await;
            if !compressed.is_empty() {
                let tokens_saved: usize = compressed.iter()
                    .map(|c| c.original_tokens.saturating_sub(c.compressed_tokens))
                    .sum();
                tracing::info!("compressed {} messages in turn {}", compressed.len(), ctx.turn_number);
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::CompressStart);
                    let _ = stx.send(crate::llm::stream::StreamChunk::CompressEnd {
                        messages_compressed: compressed.len(),
                        tokens_saved,
                    });
                }
            } else if let Some(ref stx) = self.stream_tx {
                // 无压缩发生，不发 CompressStart，只发空 End（让 TUI 知道 post 完成）
                let _ = stx.send(crate::llm::stream::StreamChunk::CompressEnd {
                    messages_compressed: 0,
                    tokens_saved: 0,
                });
            }
        }

        // Phase Ctx-C：扫描 LLM 输出标记 segment 命中 + 周期性 evict
        {
            let turn = ctx.turn_number;
            // 1) 从 final_response 中扫 segment id
            let hits = self.core.context_manager
                .scan_and_mark_used(&ctx.final_response, turn).await;
            if hits > 0 {
                tracing::debug!(turn, hits, "context segments referenced");
                // 行为宫殿协同：记每个命中的 segment id 到 palace（跨 session 学习）
                if let Some(ref palace) = self.core.memory_palace {
                    let snap = self.core.context_manager.retained_snapshot().await;
                    let p = palace.read().await;
                    for (id, _, meta) in &snap {
                        if meta.last_used_turn == turn && meta.ref_count > 0 {
                            p.behavior.record_interaction(
                                &format!("ctx_segment:{id}"),
                                &["context".to_string(), "segment_hit".to_string()],
                            ).await;
                        }
                    }
                }
            }
            // 2) 每 5 turn evict 一次
            //    W4 (Task #102)：双层 evict 策略
            //      ① evict_stale_segments：distance>40 强制 evict（兜底，防 importance 评分 bug）
            //      ② evict_by_importance：当 retained tokens 超 budget 时，按综合分数保留 top-K
            //    顺序：先跑 importance（裁掉中分但占 token 大段），再跑 stale（兜底极远段）
            if turn > 0 && turn.is_multiple_of(5) {
                // W4 默认 retained budget = 4000 tokens（约 16KB 文本）；超出按 importance 降序保留
                const RETAINED_TOKEN_BUDGET: usize = 4000;
                let (by_imp, remaining) = self.core.context_manager
                    .evict_by_importance(RETAINED_TOKEN_BUDGET).await;
                if by_imp > 0 {
                    tracing::info!(turn, evicted = by_imp, remaining_tokens = remaining,
                        "context segments evicted (importance budget)");
                }
                // 兜底：极远段（distance > 40）强 evict，不论 importance
                let by_dist = self.core.context_manager
                    .evict_stale_segments(turn, 20).await;
                if by_dist > 0 {
                    tracing::info!(turn, evicted = by_dist, "context segments evicted (stale fallback)");
                }
            }
            // 3) Task #80：每 10 turn 触发 hot→warm + warm→cold 迁移
            //    阈值默认 30 turn 老化、warm 容量 100；超 cap 的 demote 进 cold（持久化）
            //    生命周期：仅 N 倍 turn 触发，不影响热路径性能
            if turn > 0 && turn.is_multiple_of(10) {
                let stats = self.core.context_manager
                    .run_tier_migration(turn, 30, 100).await;
                if stats.promoted_to_warm + stats.demoted_to_cold > 0 {
                    tracing::info!(
                        turn,
                        promoted = stats.promoted_to_warm,
                        demoted = stats.demoted_to_cold,
                        save_errors = stats.cold_save_errors,
                        "tier migration"
                    );
                }
            }
        }

        // MapAnalyzer
        {
            let tool_records: Vec<ToolCallRecord> = ctx.all_tool_outputs.iter().map(|o| ToolCallRecord {
                tool_id: o.tool_id.0.clone(),
                params: o.output.clone(),
                result: if o.success { "ok".into() } else { "error".into() },
            }).collect();
            let s = self.session.read().await;
            let map = s.interaction_map.read().await;
            if let Some(cp) = MapAnalyzer::analyze_turn(
                self.input, &ctx.final_response, &tool_records, ctx.turn_number, &map
            ) {
                drop(map);
                let mut map = s.interaction_map.write().await;
                let new_id = map.add_checkpoint(cp);
                if let Some(edge) = MapAnalyzer::create_edge_for(&map, new_id) {
                    let _ = map.try_add_edge(edge);
                }
            }
        }

        // DynamicInjector
        {
            let inject_ctx = serde_json::json!({"tool_results": ctx.all_tool_outputs});
            let mut injector = self.core.injector.write().await;
            injector.inject(self.input, &inject_ctx);
        }

        // EffectivenessTracker
        // 段 L1：使用 record_outcome + ToolOutcome::classify_error 让环境失败被识别
        // - success=true → ToolOutcome::Success
        // - success=false + failure_kind 含 Network/Timeout/Unauthorized/RateLimited/SandboxDenied/DependencyMissing
        //   → ToolOutcome::EnvFailure（不拉 success_rate 分母——段 K1）
        // - 其他 success=false → ToolOutcome::ToolFailure
        {
            let mut eff = self.core.effectiveness.write().await;
            for output in &ctx.all_tool_outputs {
                let outcome = if output.success {
                    crate::tool::effectiveness::ToolOutcome::Success
                } else {
                    let kind = output.failure_kind.as_deref().unwrap_or("Other");
                    crate::tool::effectiveness::ToolOutcome::classify_error(kind)
                };
                eff.record_outcome(&output.tool_id, outcome, output.latency_ms);
            }
        }

        // Update turn count
        {
            let mut s = self.session.write().await;
            s.turn_count = ctx.turn_number;
        }

        // Skill execution recording
        {
            let mut engine = self.core.skill_engine.write().await;
            for skill in &ctx.matched_skills {
                engine.record_execution(SkillExecutionRecord {
                    skill_id: skill.id.clone(),
                    input: self.input.to_string(),
                    matched_triggers: Vec::new(),
                    steps_executed: 1,
                    total_steps: 1,
                    total_latency_ms: ctx.start_time.elapsed().as_millis() as u64,
                    exit_code: 0,
                    user_feedback: None,
                    timestamp: Utc::now().timestamp(),
                });
            }
        }

        // Cooldown tick
        self.core.registry.tick_cooldowns().await;

        // Health check — detect degraded subsystems, emit warnings
        {
            let warnings = self.core.health_registry.tick().await;
            for w in &warnings {
                tracing::warn!(subsystem_warning = %w, "health degradation detected");
            }
        }

        // Resource pressure check — automatic load shedding
        {
            let actions = self.core.pressure_monitor.check_and_shed().await;
            for a in &actions {
                tracing::info!(
                    source = %a.source,
                    level = %a.level,
                    items_shed = a.items_shed,
                    "pressure load shedding"
                );
            }
        }

        // Deduction post-turn collection
        {
            let eff = self.core.effectiveness.read().await;
            let tool_stats = eff.all_stats_snapshot().clone();
            let registry_tools = self.core.registry.all_tools().await;
            let tool_set_hash: i64 = registry_tools.iter().fold(0i64, |acc, t| {
                acc.wrapping_add(t.id.0.bytes().fold(0i64, |a, b| a.wrapping_mul(31).wrapping_add(b as i64)))
            });
            let window = self.core.context_manager.window.read().await;
            let pct = window.usage_pct();
            let max_tok = window.max_tokens;
            let cur_tok = window.current_tokens;
            drop(window);
            let was_compressed_bool = {
                let tiers = &self.core.context_manager.tiers;
                !tiers.compressed_messages.read().await.is_empty()
            };
            let s = self.session.read().await;
            let layer_count: usize = registry_tools.len() / 4 + 6;
            let _ = self.core.deduction_engine.collect_post_turn(
                ctx.turn_number, &s.session_id, &tool_stats,
                pct, max_tok, cur_tok, was_compressed_bool,
                layer_count,
                registry_tools.len(),
                tool_set_hash,
                self.core.config.thinking_intent.is_some(),
            ).await;
        }

        // ─── Memory Palace 写入 ────────────────────────────────────────────────
        // 行为模式记录（record_interaction）+ 工具效果记录（record_tool_behavior）
        // 引用：self.core.memory_palace（Option<Arc<RwLock<DualPalaceMemory>>>）
        // 生命周期：每 turn post_process 阶段写入一次，异步不阻塞输出
        if let Some(ref palace) = self.core.memory_palace {
            let p = palace.read().await;
            // 行为模式记录：供 classify_input / recommend_next_tools 学习
            let tags = vec![
                ctx.classification.kind.label().to_string(),
                if ctx.total_tool_calls > 0 { "used_tools".into() } else { "direct_answer".into() },
            ];
            p.record_interaction(self.input, &tags).await;
            // 工具效果记录：与 EffectivenessTracker 互补，面向行为推荐而非评分
            for output in &ctx.all_tool_outputs {
                p.record_tool_behavior(&output.tool_id.0, output.success).await;
            }
        }

        // ─── Epistemic post-check ────────────────────────────────────────────
        // 检测本轮 LLM 输出是否违反认识论约束：
        //   ZeroHitBypass       — KB ZeroHit 后仍从权重输出含版本/日期的事实断言
        //   FastDecayNoWebSearch — 快衰查询未调用 web.search 却含事实断言
        //   UnmarkedFactualClaim — 含信号词但无来源标注且无工具验证
        //
        // 违规时在 final_response 前插入 [认识论约束违规] 警告块并写 warn 日志。
        // EpistemicGuard.after_execute() 已在工具执行阶段注入 _epistemic_constraint，
        // 本检查在输出层做二次审计，形成双层防护。
        //
        // 引用：crate::mag_chain::{DecayRouter, EpistemicPostCheck, EpistemicViolation}
        // 生命周期：每 turn post_process 阶段执行一次，无副作用（除修改 ctx.final_response）
        {
            use crate::mag_chain::{DecayRouter, EpistemicPostCheck, EpistemicViolation};

            let had_zero_hit = ctx.all_tool_outputs.iter().any(|o| {
                o.output
                    .get("_epistemic_constraint")
                    .and_then(|c| c.get("action"))
                    .and_then(|a| a.as_str())
                    == Some("BLOCK_WEIGHT_OUTPUT")
            });
            let decay_tier = DecayRouter::classify(self.input);
            let tools_called: Vec<String> = ctx.all_tool_outputs.iter()
                .map(|o| o.tool_id.0.clone())
                .collect();

            let violations = EpistemicPostCheck::check(
                &ctx.final_response,
                had_zero_hit,
                decay_tier,
                &tools_called,
            );

            if !violations.is_empty() {
                let labels: Vec<&str> = violations.iter().map(|v| match v {
                    EpistemicViolation::ZeroHitBypass          => "ZeroHitBypass",
                    EpistemicViolation::FastDecayNoWebSearch   => "FastDecayNoWebSearch",
                    EpistemicViolation::UnmarkedFactualClaim   => "UnmarkedFactualClaim",
                }).collect();
                tracing::warn!(
                    violations = ?labels,
                    turn = ctx.turn_number,
                    "epistemic violations detected — prepending notice to response"
                );
                // 记录到 EpistemicGuard（累积计数，超 declaration_threshold 后下轮注入声明）
                for _ in &violations {
                    self.core.epistemic_guard.record_violation().await;
                }
                let notice = format!(
                    "[认识论约束违规: {}]\n以下回复可能包含未经验证的事实断言，请自行核实。\n\n",
                    labels.join(", ")
                );
                ctx.final_response.insert_str(0, &notice);
            }
        }

        // ─── V29.13 段1：TurnPostFanOut 广播 ───────────────────────────────
        // 引用：mag_chain::PipelineEvent::TurnPostFanOut
        // 生命周期：post_process 末尾、TurnEnd 之前 emit 一次；hook 链按优先级顺序消费
        // 用途：让段2/段3的协同 hook（PalaceAbsorbHook / TierMigrationHook 等）能在
        //       turn 派生工作期插入逻辑，而无需修改 post.rs 本身——保持 post.rs 稳定。
        // 失败处理：emit 错误吞掉（hook 失败不应阻塞用户响应）；记 warn 日志便于排查
        {
            let session_id = self.session.read().await.session_id.clone();
            let was_compressed = !self.core.context_manager.tiers
                .compressed_messages.read().await.is_empty();
            let all_success = ctx.all_tool_outputs.iter().all(|o| o.success);
            let event = crate::mag_chain::PipelineEvent::TurnPostFanOut {
                turn_number: ctx.turn_number,
                session_id,
                tool_calls: ctx.total_tool_calls as usize,
                all_success,
                was_compressed,
            };
            if let Err(e) = self.core.emit_pipeline_event(event).await {
                tracing::warn!("TurnPostFanOut hook chain error (ignored): {}", e);
            }
        }
    }

    // ─── Phase 6: Inertia Detection ─────────────────────────────────────────

    pub(super) async fn detect_inertia(&self, ctx: &mut TurnContext) {
        let inertia_config = inertia::InertiaConfig::default();
        let detector = inertia::InertiaDetector::new(inertia_config.clone());
        let policy = inertia::InterventionPolicy::new(inertia_config.clone());
        let mut retry_attempt = 0u32;

        loop {
            let tools_failed_count = ctx.all_tool_outputs.iter()
                .filter(|o| !o.success).count() as u32;
            let failed_tool_names: Vec<String> = ctx.all_tool_outputs.iter()
                .filter(|o| !o.success)
                .map(|o| o.tool_id.0.clone())
                .collect();
            let tools_retried = {
                let mut seen = std::collections::HashSet::new();
                ctx.all_tool_outputs.iter()
                    .filter(|o| !o.success)
                    .filter(|o| !seen.insert(o.tool_id.0.clone()))
                    .count() as u32
            };
            let is_progressive_paused = {
                let s = self.session.read().await;
                let ctrl = s.progressive.read().await;
                ctrl.is_blocking()
            };

            let signals = detector.detect(
                self.input,
                &ctx.final_response,
                ctx.total_tool_calls,
                tools_failed_count,
                tools_retried,
                ctx.classification.kind.label(),
                &failed_tool_names,
                is_progressive_paused,
            );

            match policy.decide(&signals, retry_attempt) {
                inertia::InertiaIntervention::None => {
                    break;
                }
                inertia::InertiaIntervention::FlagWarning { signal, .. } => {
                    ctx.inertia_warning = Some(signal);
                    break;
                }
                inertia::InertiaIntervention::RetryWithNudge { nudge_prompt, attempt } => {
                    retry_attempt = attempt;
                    tracing::info!(
                        "inertia retry #{}: {}",
                        attempt,
                        &nudge_prompt[..nudge_prompt.len().min(80)]
                    );

                    {
                        let s = self.session.read().await;
                        let mut msgs = s.messages.write().await;
                        msgs.push(Message {
                            role: MessageRole::User,
                            content: Some(MessageContent::Text(nudge_prompt)),
                            name: None, tool_calls: None, tool_call_id: None, reasoning_content: None, prefix: false,
                        });
                    }

                    let messages = {
                        let s = self.session.read().await;
                        let msgs = s.messages.read().await;
                        msgs.clone()
                    };
                    let retry_req = LlmRequest {
                        model: self.req_ctx.model.clone().unwrap_or_else(|| self.core.config.default_model.clone()),
                        messages,
                        system: Some(ctx.enriched_system.clone()),
                        system_segments: ctx.system_segments.clone(),
                        tools: ctx.tool_defs.clone(),
                        temperature: Some(self.core.config.default_temperature),
                        max_tokens: Some(self.core.config.default_max_tokens),
                        top_p: None, stop: Vec::new(), stream: false,
                        // L1: thinking_intent 单通道——retry 路径用 per-request 或 config 默认
                        thinking_intent: self.req_ctx.thinking_intent.clone()
                            .or_else(|| self.core.build_thinking_intent()),
                        cache_config: Some(crate::llm::prompt_cache::PromptCacheConfig::default()), // 修复: retry 路径开启缓存
                        extra_body: HashMap::new(),
                        // Retry 路径无新 ICL，preamble=None
                        user_message_preamble: None,
                    };

                    match ctx.provider.complete_cancellable(retry_req, self.cancel_token()).await {
                        Ok(retry_response) => {
                            ctx.prompt_tokens += retry_response.usage.prompt_tokens;
                            ctx.completion_tokens += retry_response.usage.completion_tokens;
                            ctx.cached_tokens += retry_response.usage.cached_tokens;
                            let new_text = crate::core::extract_text(&retry_response.message);
                            {
                                let s = self.session.read().await;
                                let mut msgs = s.messages.write().await;
                                msgs.push(retry_response.message);
                            }
                            ctx.final_response = new_text;
                        }
                        Err(_) => {
                            ctx.inertia_warning = signals.into_iter().max_by(|a, b|
                                a.severity().partial_cmp(&b.severity()).unwrap_or(std::cmp::Ordering::Equal)
                            );
                            break;
                        }
                    }
                }
            }
        }
    }
}
