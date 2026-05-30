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

use crate::core::context::{SessionCheckpoint, SessionPhase, PendingItem};
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

        // ── 自适应 checkpoint + 压缩 ──────────────────────────────────────────
        // 阈值序列：70%→80%→85%→90%→75%(稳态)，每次压缩后 compress_count +1
        // 流程：检查阈值 → 生成 checkpoint（LLM call + fallback）→ 压缩 → 通知 TUI + LLM
        {
            let usage_pct = self.core.context_manager.window.read().await.usage_pct();
            let threshold = self.core.context_manager.next_compress_threshold_pct();

            if usage_pct >= threshold {
                tracing::info!(
                    turn = ctx.turn_number, usage_pct, threshold,
                    "adaptive compress threshold reached — generating checkpoint"
                );

                // 1. 生成结构化 checkpoint（LLM call，失败自动 fallback 确定性提取）
                let messages_snapshot = {
                    let s = self.session.read().await;
                    let msgs = s.messages.read().await;
                    msgs.clone()
                };
                let mut checkpoint = self.generate_session_checkpoint(ctx, &messages_snapshot).await;
                checkpoint.context_pct = usage_pct;

                // 2. 存储 checkpoint
                self.core.context_manager.store_checkpoint(checkpoint.clone()).await;

                // 3. 通知 TUI 压缩开始
                if let Some(ref stx) = self.stream_tx {
                    let _ = stx.send(crate::llm::stream::StreamChunk::CompressStart);
                }

                // 4. 压缩（按 checkpoint phase 选精度 + 注入历史块）
                let compressed = {
                    let s = self.session.read().await;
                    let mut msgs = s.messages.write().await;
                    self.core.context_manager
                        .auto_compress_with_checkpoint(&mut msgs, &checkpoint)
                        .await
                };

                if !compressed.is_empty() {
                    let tokens_saved: usize = compressed.iter()
                        .map(|c| c.original_tokens.saturating_sub(c.compressed_tokens))
                        .sum();
                    tracing::info!(
                        turn = ctx.turn_number,
                        compressed = compressed.len(),
                        tokens_saved,
                        phase = ?checkpoint.overall_phase,
                        "checkpoint compress done"
                    );

                    // 5. 压缩计数 +1（驱动下次阈值升挡）
                    self.core.context_manager.on_compress_done();

                    // 6. 通知 TUI 压缩完成
                    if let Some(ref stx) = self.stream_tx {
                        let _ = stx.send(crate::llm::stream::StreamChunk::CompressEnd {
                            messages_compressed: compressed.len(),
                            tokens_saved,
                        });
                        // Execution 阶段：发出自动续行信号
                        if checkpoint.overall_phase == crate::core::context::SessionPhase::Execution {
                            let _ = stx.send(crate::llm::stream::StreamChunk::CompressAutoResume);
                        }
                    }

                    // 7. 注入 system message 通知 LLM（含 checkpoint 摘要 + 下次阈值）
                    let next_threshold = self.core.context_manager.next_compress_threshold_pct();
                    let checkpoint_summary = checkpoint.to_context_block();
                    let s = self.session.read().await;
                    let mut msgs = s.messages.write().await;
                    msgs.push(Message {
                        role: MessageRole::System,
                        content: Some(MessageContent::Text(format!(
                            "[Context compressed: {} messages summarized, ~{} tokens freed.]\
                             \n{}\
                             \n[Next compress threshold: {:.0}%. Use messages_recover with recover_id for original details.]",
                            compressed.len(), tokens_saved, checkpoint_summary, next_threshold
                        ))),
                        name: None, tool_calls: None, tool_call_id: None,
                        reasoning_content: None, prefix: false,
                    });
                } else {
                    // 压缩守卫未通过（消息数不足）— 取消 CompressStart
                    if let Some(ref stx) = self.stream_tx {
                        // 补发一个 0-tokens CompressEnd 以保持状态机一致
                        let _ = stx.send(crate::llm::stream::StreamChunk::CompressEnd {
                            messages_compressed: 0,
                            tokens_saved: 0,
                        });
                    }
                }
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

        // 每 5 turn 重评估 Adaptive 子系统注册状态（热重载/冷卸载）
        // 引用：CoreLoop::reevaluate_adaptive_subsystems()
        // 位置：在 effectiveness 记录完成 + cooldown tick 之后（stats 已更新），
        //       health check 之前（tool list 变化可能影响 health 视角）
        self.core.reevaluate_adaptive_subsystems(ctx.turn_number).await;

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

            // P1-A3 + P1-C5: 正常轮次的 Reflexion（D-tier 工具触发）
            // 与停滞信号触发互补：这里仅根据工具评分触发（inertia_triggered=false）
            // 引用：deduction/mod.rs::maybe_reflect()
            self.core.deduction_engine.maybe_reflect(
                ctx.turn_number,
                &s.session_id,
                ctx.classification.kind.label(),
                &tool_stats,
                self.core.knowledge_store.as_ref(),
                false, // inertia_triggered = false（停滞路径单独处理）
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

        // ─── 任务完成摘要检测 ────────────────────────────────────────────────
        // 检测 final_response 末尾的 `---\n✓ summary` 标记（system prompt Layer 188 规定格式）
        // 检测到时写入 pending_accomplishments，供下次 checkpoint 生成时填入 accomplished 字段
        // 引用：generate_session_checkpoint 调用 snapshot_accomplishments()
        if let Some(summary) = Self::extract_completion_summary(&ctx.final_response) {
            tracing::debug!(turn = ctx.turn_number, summary = %summary, "task completion detected");
            self.core.context_manager.push_accomplishment(summary).await;
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

    // ─── Task Completion Detection ──────────────────────────────────────────

    /// 从 final_response 末尾提取 LLM 自报的任务完成摘要
    ///
    /// 格式（由 system prompt Layer 188 规定）：
    ///   ---
    ///   ✓ [summary]
    ///
    /// 引用关系：post_process 每轮结束后调用；结果写入 pending_accomplishments
    /// 生命周期：纯函数，无副作用
    fn extract_completion_summary(response: &str) -> Option<String> {
        let trimmed = response.trim_end();
        // 找末尾 "---" 分隔线（支持前后有空行）
        if let Some(sep_pos) = trimmed.rfind("\n---") {
            let after_sep = trimmed[sep_pos + 4..]
                .trim_start_matches('-')
                .trim_start_matches('\n');
            for line in after_sep.lines() {
                let line = line.trim();
                if line.starts_with('✓') {
                    let summary = line.trim_start_matches('✓').trim().to_string();
                    if !summary.is_empty() && summary.chars().count() <= 200 {
                        return Some(summary);
                    }
                }
            }
        }
        None
    }

    // ─── Checkpoint Generation ──────────────────────────────────────────────

    /// 生成结构化 session checkpoint（LLM call + deterministic fallback）
    ///
    /// 使用当前 turn 已选定的 provider（复用现有调用栈，适配所有 LLM）：
    /// - 轻量 prompt：max_tokens=400, temperature=0.1, no tools, no thinking
    /// - JSON 响应解析；失败时走确定性提取（正则 + 启发式）
    ///
    /// 引用关系：post_process 自适应压缩块调用
    /// 生命周期：每次阈值触发执行一次，结果写入 ContextTiers.checkpoints
    async fn generate_session_checkpoint(
        &self,
        ctx: &TurnContext,
        messages: &[Message],
    ) -> crate::core::context::SessionCheckpoint {

        // 预载 LLM 自报的任务完成摘要（写入 accomplished，比 LLM 提取更准确）
        // 这些摘要由 post_process 每轮检测 `✓` 标记写入，不随压缩丢失
        let prior_accomplishments = self.core.context_manager.snapshot_accomplishments().await;

        // 构建对话片段（最近 15 条，跳过 tool 协议消息）
        let msg_snippet: String = messages.iter()
            .filter(|m| m.tool_calls.is_none() && m.tool_call_id.is_none()
                && !matches!(m.role, MessageRole::Tool))
            .rev()
            .take(15)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .filter_map(|m| match (&m.role, &m.content) {
                (role, Some(MessageContent::Text(t))) => {
                    let short: String = t.chars().take(150).collect();
                    Some(format!("[{:?}]: {}", role, short))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = format!(
            "Analyze this conversation and output a JSON checkpoint. Be concise.\n\
             \nConversation (recent):\n{}\
             \n\nOutput ONLY valid JSON (no markdown):\
             \n{{\
             \n  \"accomplished\": [\"completed task\"],\
             \n  \"current_topic\": \"one sentence about current work\",\
             \n  \"pending\": [{{\"task\": \"item\", \"phase\": \"communication\"}}],\
             \n  \"overall_phase\": \"communication\"\
             \n}}\
             \nRules: accomplished/pending max 5 items. \
             phase: \"communication\" (discussing) or \"execution\" (coding/implementing).",
            msg_snippet
        );

        let request = LlmRequest {
            model: self.core.config.default_model.clone(),
            messages: vec![Message {
                role: MessageRole::User,
                content: Some(MessageContent::Text(prompt)),
                name: None, tool_calls: None, tool_call_id: None,
                reasoning_content: None, prefix: false,
            }],
            // 系统消息：guide LLM 输出格式（属于 system 约束，不是 user 消息）
            system: Some("You are a session analyst. Output only valid JSON, no explanation.".to_string()),
            system_segments: Vec::new(),
            tools: Vec::new(),        // 不需要工具调用
            temperature: Some(0.1),  // 低温，稳定 JSON 输出
            max_tokens: Some(400),
            top_p: None, stop: Vec::new(), stream: false,
            thinking_intent: None,   // 不需要推理，节省 token
            cache_config: None,
            extra_body: HashMap::new(),
            user_message_preamble: None,
        };

        // 使用当前 turn 选定的 provider（已验证可用，适配当前 LLM）
        let mut cp = match ctx.provider.complete_cancellable(request, None).await {
            Ok(resp) => {
                let text = crate::core::extract_text(&resp.message);
                Self::parse_checkpoint_json(&text, ctx.turn_number)
                    .unwrap_or_else(|| {
                        tracing::debug!("checkpoint JSON parse failed, using deterministic fallback");
                        Self::deterministic_checkpoint(messages, ctx.turn_number)
                    })
            }
            Err(e) => {
                tracing::warn!(error = %e, "checkpoint LLM call failed, using deterministic fallback");
                Self::deterministic_checkpoint(messages, ctx.turn_number)
            }
        };

        // LLM 自报摘要优先追加到 accomplished 列表（精确度高于 LLM 提取的 JSON）
        // prior_accomplishments 来自历轮 ✓ 标记，不替换而是前置合并
        if !prior_accomplishments.is_empty() {
            let mut merged = prior_accomplishments;
            merged.extend(cp.accomplished.into_iter());
            merged.dedup(); // 去重（同一摘要可能多次出现）
            merged.truncate(5);
            cp.accomplished = merged;
        }

        cp
    }

    /// 解析 LLM 返回的 checkpoint JSON
    fn parse_checkpoint_json(text: &str, turn_count: u32) -> Option<crate::core::context::SessionCheckpoint> {

        // 从可能的 markdown fence 中提取 JSON
        let json_str = if let Some(start) = text.find('{') {
            if let Some(end) = text.rfind('}') {
                &text[start..=end]
            } else { text.trim() }
        } else { text.trim() };

        let v: serde_json::Value = serde_json::from_str(json_str).ok()?;

        let accomplished: Vec<String> = v.get("accomplished")
            .and_then(|a| a.as_array())
            .map(|arr| arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .take(5).collect())
            .unwrap_or_default();

        let current_topic = v.get("current_topic")
            .and_then(|t| t.as_str())
            .unwrap_or("").to_string();

        let pending: Vec<PendingItem> = v.get("pending")
            .and_then(|p| p.as_array())
            .map(|arr| arr.iter().take(5).filter_map(|item| {
                let task = item.get("task")?.as_str()?.to_string();
                let phase = match item.get("phase").and_then(|p| p.as_str()) {
                    Some("execution") => SessionPhase::Execution,
                    _ => SessionPhase::Communication,
                };
                Some(PendingItem { task, phase })
            }).collect())
            .unwrap_or_default();

        let overall_phase = match v.get("overall_phase").and_then(|p| p.as_str()) {
            Some("execution") => SessionPhase::Execution,
            _ => SessionPhase::Communication,
        };

        Some(SessionCheckpoint {
            context_pct: 0.0, // 由调用方填充
            turn_count,
            accomplished,
            current_topic,
            pending,
            overall_phase,
            created_at: chrono::Utc::now().timestamp(),
        })
    }

    /// 确定性 checkpoint（LLM 失败时的 fallback）
    ///
    /// 从消息中启发式提取：
    ///   - accomplished: ## 标题 + ✓ 标记行
    ///   - pending: TODO/待办/- [ ] 行
    ///   - current_topic: 最后一条 User 消息首 100 字符
    ///   - overall_phase: 工具调用次数 > 5 → Execution，否则 Communication
    fn deterministic_checkpoint(
        messages: &[Message],
        turn_count: u32,
    ) -> crate::core::context::SessionCheckpoint {

        let mut accomplished = Vec::new();
        let mut pending = Vec::new();
        let mut current_topic = String::new();
        let mut tool_call_count = 0usize;

        for msg in messages.iter().rev().take(30) {
            if msg.tool_calls.is_some() { tool_call_count += 1; }
            if let Some(MessageContent::Text(text)) = &msg.content {
                for line in text.lines() {
                    let t = line.trim();
                    if (t.starts_with("## ") || t.starts_with("✓ ") || t.starts_with("完成："))
                        && accomplished.len() < 5
                    {
                        let item = t.trim_start_matches("## ")
                            .trim_start_matches("✓ ")
                            .trim_start_matches("完成：")
                            .to_string();
                        if !item.is_empty() { accomplished.push(item); }
                    }
                    if (t.starts_with("- [ ]") || t.starts_with("TODO")
                        || t.starts_with("待办") || t.starts_with("下一步"))
                        && pending.len() < 5
                    {
                        let item = t.trim_start_matches("- [ ]")
                            .trim_start_matches("TODO:")
                            .trim_start_matches("待办：")
                            .trim().to_string();
                        if !item.is_empty() {
                            pending.push(PendingItem {
                                task: item,
                                phase: SessionPhase::Execution,
                            });
                        }
                    }
                }
            }
            if current_topic.is_empty() {
                if let (MessageRole::User, Some(MessageContent::Text(t))) = (&msg.role, &msg.content) {
                    current_topic = t.chars().take(100).collect();
                }
            }
        }

        let overall_phase = if tool_call_count > 5 {
            SessionPhase::Execution
        } else {
            SessionPhase::Communication
        };

        SessionCheckpoint {
            context_pct: 0.0,
            turn_count,
            accomplished,
            current_topic,
            pending,
            overall_phase,
            created_at: chrono::Utc::now().timestamp(),
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
                    // 发出 InertiaDetected 事件（前端 + LLM 感知）
                    if let Some(ref stx) = self.stream_tx {
                        let _ = stx.send(crate::llm::stream::StreamChunk::InertiaDetected {
                            signals: vec![format!("{:?}", signal)],
                            recommendation: "Consider changing approach or breaking the task into smaller steps.".into(),
                        });
                    }
                    // P1-C5: 停滞信号触发 Reflexion（将失败模式写入 KnowledgeStore 供未来 kb.search 检索）
                    // 引用：deduction/mod.rs::maybe_reflect() 
                    // 触发条件：FlagWarning（重试已达上限）——说明 LLM 持续彺弱
                    {
                        let task_kind = ctx.classification.kind.label().to_string();
                        let tool_stats_snapshot = {
                            let eff = self.core.effectiveness.read().await;
                            eff.all_stats_snapshot().clone()
                        };
                        let ks_ref = self.core.knowledge_store.as_ref();
                        let session_id = { self.session.read().await.session_id.clone() };
                        self.core.deduction_engine.maybe_reflect(
                            ctx.turn_number,
                            &session_id,
                            &task_kind,
                            &tool_stats_snapshot,
                            ks_ref,
                            true, // inertia_triggered
                        ).await;
                    }
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
                            content: Some(MessageContent::Text(format!(
                                "[Abacus 惯性检测——指引与约束] 第 {} 次重试：{}\n\
请按上述指引调整方案后继续。",
                                attempt, nudge_prompt
                            ))),
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
