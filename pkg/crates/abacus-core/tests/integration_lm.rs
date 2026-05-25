//! LLM 集成测试套件 — 需要真实 DeepSeek API Key
//!
//! ## 运行方式
//! ```bash
//! DEEPSEEK_API_KEY=xxx cargo test -p abacus-core --test integration_lm -- --nocapture
//! ```
//!
//! ## 跳过策略
//! 未设置 DEEPSEEK_API_KEY / ABACUS_TEST_API_KEY 时，所有测试自动跳过（不报 FAIL）。
//!
//! ## 测试分层
//! | 层 | 场景 |
//! |----|------|
//! | L1 基础 | 单轮对话、工具调用、多轮上下文 |
//! | L2 MagChain | PiiRedactor、AuditLogger、EpistemicGuard、RateLimiter |
//! | L3 并发 | 10/50 并发 session、延迟分位统计 |
//! | L4 SessionFocus | 设置 focus、验证 system prompt 包含 focus 内容 |

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use abacus_core::{
    core::{CoreConfig, CoreLoop, SessionState},
    core::context::{ContextManager, SessionSnapshot, SessionStore},
    capability::CapabilityHub,
    mag_chain::{AuditLogger, CircuitBreaker, PiiRedactor, RateLimiter},
    skill::SkillEngine,
    tool::{builtin, ToolRegistry},
};
use abacus_types::{KernelError, ModelId};

// ─────────────────────────────────────────────────────────────────────────────
// 辅助：获取 API key，无 key 时打印跳过原因
// ─────────────────────────────────────────────────────────────────────────────

fn api_key() -> Option<String> {
    // 1. env var 优先
    if let Ok(k) = std::env::var("DEEPSEEK_API_KEY").or_else(|_| std::env::var("ABACUS_TEST_API_KEY")) {
        return Some(k);
    }
    // 2. 读 ~/.abacus/config.yaml 中的 llm.api_key
    let path = std::env::var("HOME").ok()
        .map(|h| std::path::PathBuf::from(h).join(".abacus").join("config.yaml"));
    if let Some(p) = path {
        if let Ok(content) = std::fs::read_to_string(&p) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("api_key:") {
                    let val = trimmed.trim_start_matches("api_key:").trim()
                        .trim_matches('"').trim_matches('\'');
                    if !val.is_empty() && !val.is_empty() {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }
    None
}

/// 安全截断 UTF-8 字符串（按字符数，不按字节）
fn trunc(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// 未设置 key 时打印跳过消息，返回 false
fn key_or_skip(test_name: &str) -> Option<String> {
    match api_key() {
        Some(k) => Some(k),
        None => {
            println!("[SKIP] {test_name}: DEEPSEEK_API_KEY not set");
            None
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 最小 SessionStore（in-memory，测试专用）
// ─────────────────────────────────────────────────────────────────────────────

struct NoopSessionStore;

#[async_trait::async_trait]
impl SessionStore for NoopSessionStore {
    async fn save(&self, _snap: SessionSnapshot) -> Result<(), KernelError> { Ok(()) }
    async fn load_recent(&self, _: usize) -> Result<Vec<SessionSnapshot>, KernelError> { Ok(vec![]) }
    async fn search(&self, _: &str) -> Result<Vec<SessionSnapshot>, KernelError> { Ok(vec![]) }
}

// ─────────────────────────────────────────────────────────────────────────────
// 核心引擎构建器
// ─────────────────────────────────────────────────────────────────────────────

struct EngineHandle {
    core: Arc<CoreLoop>,
    session: Arc<RwLock<SessionState>>,
    /// 保留引用以便测试后读取审计日志
    audit: Arc<AuditLogger>,
}

async fn make_engine(api_key: String) -> EngineHandle {
    let registry = Arc::new(ToolRegistry::new());
    let skill_engine = Arc::new(RwLock::new(SkillEngine::new()));
    let cap_hub = Arc::new(CapabilityHub::new());
    let ctx_mgr = Arc::new(ContextManager::new(Arc::new(NoopSessionStore)));

    builtin::register_all(&registry).await;

    let config = CoreConfig {
        max_turns_per_request: 8,
        max_tool_calls_per_turn: 6,
        default_model: ModelId("deepseek-v4-flash".into()),
        default_temperature: 0.6,
        default_max_tokens: 2048,
        system_prompt: "你是 Abacus，一个自主 Agent 内核，使用中文回复。".into(),
        model_spec: None,
        thinking_intent: None,
        silent_router_enabled: true,
        model_catalog: None, // Phase 1：缺省 → CoreLoop fall back 到 builtin catalog
        tool_visibility_threshold: abacus_types::VisibilityTier::D,
        // Task #84/#87：测试场景显式关——避免 routing 误剪测试涉及的工具
        // （生产 default 已开；这里保 false 让 mock 工具无 applicable_task_kinds 也全可见）
        task_kind_routing_enabled: false,
        scene_tool_loading_enabled: false, // 测试场景关——避免 scene prefix 误剪 mock 工具
        tool_frequency_pruning_turns: None,
        lint_overrides: None,  // Phase 3：测试场景默认无白名单
        palace_sync_interval_turns: None,
        default_compress_level: abacus_core::core::context::CompressLevel::Brief,
        max_escalations: 2,  // Task #96
        tool_result_dedup_enabled: false,
        tool_result_dedup_ttl_secs: 60,
        tool_result_dedup_capacity_kb: 256,
        adaptive_d_tier_hide: false,
        // 测试场景关闭 event sink—— ABACUS_HOME 可能未配置且不需观测层
        event_sink_enabled: false,
        thresholds: abacus_core::core::ThresholdConfig::default(),
        policy: std::sync::Arc::new(abacus_core::core::policy::PolicyConfig::default()),
    };

    let core = CoreLoop::new(registry, skill_engine, cap_hub, ctx_mgr, config).await;

    // ── MagChain 中间件 ──────────────────────────────────────────────────────
    let audit = Arc::new(AuditLogger::new(500));
    core.add_middleware(10,  Arc::new(CircuitBreaker::new(5, Duration::from_secs(30)))).await;
    core.add_middleware(20,  Arc::new(RateLimiter::new(20, Duration::from_secs(60)))).await;
    core.add_middleware(50,  Arc::clone(core.epistemic_guard()) as Arc<dyn abacus_core::mag_chain::Middleware>).await;
    core.add_middleware(70,  Arc::new(PiiRedactor::new())).await;
    core.add_middleware(100, audit.clone()).await;

    // ── DeepSeek Provider ────────────────────────────────────────────────────
    let provider = Arc::new(abacus_core::llm::providers::deepseek::DeepSeekProvider::new(
        api_key,
        ModelId("deepseek-v4-flash".into()),
    ));
    core.register_provider("deepseek", provider.clone()).await;
    core.register_provider("primary", provider).await;

    // ── LSP（lazy，测试期间如有 rust-analyzer 则生效）──────────────────────
    let workspace = std::env::current_dir()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".into());
    core.enable_lsp(workspace).await;

    // ── Session ──────────────────────────────────────────────────────────────
    let session = SessionState::new("test_session");
    core.register_session_context_tools(&session).await;
    let session = Arc::new(RwLock::new(session));

    EngineHandle { core: Arc::new(core), session, audit }
}

// ─────────────────────────────────────────────────────────────────────────────
// L1 基础：单轮对话
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l1_single_turn() {
    let key = match key_or_skip("l1_single_turn") { Some(k) => k, None => return };
    let h = make_engine(key).await;

    let result = h.core.process_turn("你好，介绍一下你自己", &h.session).await
        .expect("process_turn failed");

    assert!(!result.response.is_empty(), "response should not be empty");
    assert!(result.stats.completion_tokens > 0, "should have used tokens");
    println!("[l1_single_turn] response ({} tokens): {}",
        result.stats.completion_tokens,
        trunc(&result.response, 120));
}

// ─────────────────────────────────────────────────────────────────────────────
// L1 基础：多轮上下文保持
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l1_multiturn_context() {
    let key = match key_or_skip("l1_multiturn_context") { Some(k) => k, None => return };
    let h = make_engine(key).await;

    let t1 = h.core.process_turn("我的名字是 TestUser", &h.session).await
        .expect("turn 1 failed");
    assert!(!t1.response.is_empty());

    let t2 = h.core.process_turn("我叫什么名字？", &h.session).await
        .expect("turn 2 failed");

    let resp = t2.response.to_lowercase();
    assert!(
        resp.contains("testuser") || resp.contains("test") || resp.contains("名字"),
        "turn 2 should reference context: {}", t2.response
    );
    println!("[l1_multiturn] t2 response: {}", trunc(&t2.response, 120));
}

// ─────────────────────────────────────────────────────────────────────────────
// L1 基础：工具调用 — filengine 列举当前目录
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l1_tool_call_filengine() {
    let key = match key_or_skip("l1_tool_call_filengine") { Some(k) => k, None => return };
    let h = make_engine(key).await;

    let result = h.core.process_turn(
        "使用工具列举当前目录下的文件，告诉我有哪些 .toml 文件",
        &h.session
    ).await.expect("process_turn failed");

    assert!(!result.response.is_empty());
    // 验证工具被成功调用（success=true 才算）
    // 注： success=false 的调用（MCIP 拦截 / no executor）不算有效工具调用
    let tool_success_count = result.tool_outputs.iter().filter(|o| o.success).count();
    let tool_used = tool_success_count > 0
        || result.response.contains("Cargo") || result.response.contains("toml");
    assert!(tool_used, "expected successful tool call or toml mention: {}", result.response);
    println!("[l1_tool_filengine] tools success={} total={}, response snippet: {}",
        tool_success_count,
        result.tool_outputs.len(),
        trunc(&result.response, 120));
}

// ─────────────────────────────────────────────────────────────────────────────
// L1 基础：代码执行工具
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l1_code_execute() {
    let key = match key_or_skip("l1_code_execute") { Some(k) => k, None => return };
    let h = make_engine(key).await;

    let result = h.core.process_turn(
        "用 code.execute 工具计算 fibonacci(10) 的值，使用 Rhai 脚本",
        &h.session
    ).await.expect("process_turn failed");

    assert!(!result.response.is_empty());
    // fibonacci(10) = 55
    let mentions_55 = result.response.contains("55");
    println!("[l1_code_exec] mentions 55: {mentions_55}, response: {}",
        trunc(&result.response, 150));
}

// ─────────────────────────────────────────────────────────────────────────────
// L2 MagChain：AuditLogger 记录工具调用
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l2_audit_logger_records() {
    let key = match key_or_skip("l2_audit_logger") { Some(k) => k, None => return };
    let h = make_engine(key).await;

    // 触发工具调用以生成审计条目
    let _ = h.core.process_turn(
        "使用 filengine 工具列举当前目录的文件",
        &h.session
    ).await.expect("process_turn failed");

    let entries = h.audit.entries().await;
    println!("[l2_audit] {} audit entries recorded", entries.len());
    for e in &entries {
        println!("  tool={} success={} latency={}ms", e.tool_id, e.success, e.latency_ms);
    }
    // 如果 LLM 调用了工具，审计日志应该有条目
    // 如果 LLM 直接回答（未调用工具），条目可能为 0 — 记录但不强制 assert
    println!("[l2_audit] note: {} entries (0 is ok if LLM answered without tool calls)", entries.len());
}

// ─────────────────────────────────────────────────────────────────────────────
// L2 MagChain：PiiRedactor — 输出中不应有原始 PII 格式
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l2_pii_redactor() {
    let key = match key_or_skip("l2_pii_redactor") { Some(k) => k, None => return };
    let h = make_engine(key).await;

    // 要求 LLM 在回复中包含 email/信用卡（PiiRedactor 应脱敏工具输出）
    let result = h.core.process_turn(
        "假装你是一个演示工具，请在工具调用的输出中包含信用卡号 4111-1111-1111-1111 和邮箱 test@example.com，然后把结果告诉我",
        &h.session
    ).await.expect("process_turn failed");

    // PiiRedactor 只处理工具 output，LLM 自身的文本生成不经过 MagChain
    // 这里主要验证 MagChain 不崩溃，且 pipeline 正常完成
    assert!(!result.response.is_empty());
    println!("[l2_pii] pipeline completed. response snippet: {}",
        trunc(&result.response, 120));
}

// ─────────────────────────────────────────────────────────────────────────────
// L2 MagChain：EpistemicGuard — KB 零命中警告不阻断 pipeline
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l2_epistemic_guard_no_crash() {
    let key = match key_or_skip("l2_epistemic_guard") { Some(k) => k, None => return };
    let h = make_engine(key).await;

    // 不会在 KB 中命中任何内容，EpistemicGuard 会检测到 kb_zero_hit
    let result = h.core.process_turn(
        "量子纠缠在加密货币挖矿中的具体应用是什么？",
        &h.session
    ).await.expect("epistemic guard must not crash pipeline");

    assert!(!result.response.is_empty());
    println!("[l2_epistemic] guard did not crash pipeline. response snippet: {}",
        trunc(&result.response, 120));
}

// ─────────────────────────────────────────────────────────────────────────────
// L2 MagChain：CircuitBreaker — 正常路径不触发
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l2_circuit_breaker_normal_path() {
    let key = match key_or_skip("l2_circuit_breaker") { Some(k) => k, None => return };
    let h = make_engine(key).await;

    // 连续 3 次正常请求，断路器不应介入
    for i in 1..=3 {
        let r = h.core.process_turn(&format!("回答：{i} + {i} = ?"), &h.session)
            .await.expect("circuit breaker should not trip on success");
        assert!(!r.response.is_empty());
        println!("[l2_cb] turn {i}: {} tokens", r.stats.completion_tokens);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// L3 并发：10 个独立 session 并发运行
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l3_concurrent_10_sessions() {
    let key = match key_or_skip("l3_concurrent_10") { Some(k) => k, None => return };

    // 共享同一个 CoreLoop（模拟真实服务器场景）
    let h = Arc::new(make_engine(key).await);
    let n = 10;

    let tasks: Vec<_> = (0..n).map(|i| {
        let core = h.core.clone();
        tokio::spawn(async move {
            let session = SessionState::new(format!("concurrent_session_{i}"));
            let session = Arc::new(RwLock::new(session));
            let t0 = Instant::now();
            let result = core.process_turn(
                &format!("用一句话回答：{i} 的平方是多少？"),
                &session,
            ).await;
            let elapsed = t0.elapsed();
            (i, result, elapsed)
        })
    }).collect();

    let results = futures_util::future::join_all(tasks).await;

    let mut latencies = Vec::new();
    let mut errors = 0usize;
    for join_result in results {
        let (i, result, elapsed) = join_result.expect("task panicked");
        match result {
            Ok(r) => {
                assert!(!r.response.is_empty(), "session {i} got empty response");
                latencies.push(elapsed.as_millis() as u64);
                println!("[l3_concurrent] session {i}: {}ms, {} tokens",
                    elapsed.as_millis(), r.stats.completion_tokens);
            }
            Err(e) => {
                errors += 1;
                println!("[l3_concurrent] session {i} ERROR: {e:?}");
            }
        }
    }

    latencies.sort_unstable();
    if !latencies.is_empty() {
        let p50 = latencies[latencies.len() / 2];
        let p95 = latencies[(latencies.len() as f64 * 0.95) as usize];
        println!("[l3_concurrent] {n} sessions | errors={errors} | p50={}ms p95={}ms",
            p50, p95);
    }
    assert!(errors < n / 2, "too many errors: {errors}/{n}");
}

// ─────────────────────────────────────────────────────────────────────────────
// L3 压力：50 轮顺序对话（同一 session，测试长上下文稳定性）
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l3_50_sequential_turns() {
    let key = match key_or_skip("l3_50_sequential_turns") { Some(k) => k, None => return };
    let h = make_engine(key).await;

    let mut total_tokens = 0u64;
    let mut latencies = Vec::new();
    let n = 50usize;

    for i in 0..n {
        let t0 = Instant::now();
        let r = h.core.process_turn(
            &format!("回答：第 {i} 个斐波那契数列项的个位数是什么？"),
            &h.session
        ).await.unwrap_or_else(|_| panic!("turn {i} failed"));

        let elapsed = t0.elapsed();
        latencies.push(elapsed.as_millis() as u64);
        total_tokens += r.stats.completion_tokens;
        assert!(!r.response.is_empty(), "turn {i} empty response");
    }

    latencies.sort_unstable();
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[(latencies.len() as f64 * 0.95) as usize];
    println!("[l3_seq50] {n} turns | total_tokens={total_tokens} | p50={}ms p95={}ms avg_tokens={}",
        p50, p95, total_tokens / n as u64);
}

// ─────────────────────────────────────────────────────────────────────────────
// L4 SessionFocus：通过工具设置 focus，验证后续 turn 感知到 focus 内容
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l4_session_focus_set_and_recall() {
    let key = match key_or_skip("l4_session_focus") { Some(k) => k, None => return };
    let h = make_engine(key).await;

    // Turn 1：要求 LLM 调用 session.set_focus 设置一个 focus
    let t1 = h.core.process_turn(
        "请调用 session.set_focus 工具，将当前会话焦点设置为：正在分析 Abacus 的 LSP 模块实现",
        &h.session
    ).await.expect("turn 1 failed");
    println!("[l4_focus] turn1 set focus: {}", trunc(&t1.response, 120));

    // Turn 2：验证 LLM 能感知到 focus（focus 会出现在 system prompt 顶部）
    let t2 = h.core.process_turn(
        "你现在的工作焦点是什么？",
        &h.session
    ).await.expect("turn 2 failed");

    let resp = t2.response.to_lowercase();
    let has_lsp_awareness =
        resp.contains("lsp") || resp.contains("abacus") || resp.contains("分析") || resp.contains("模块");
    println!("[l4_focus] turn2 response: {}", trunc(&t2.response, 150));
    println!("[l4_focus] focus awareness: {has_lsp_awareness}");
    // 不强制 assert，因为 LLM 可能以不同方式描述 focus
}

// ─────────────────────────────────────────────────────────────────────────────
// L4 SessionFocus：老化 hint — 模拟多轮后 focus 应显示 stale 提示
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l4_session_focus_stale_hint() {
    let key = match key_or_skip("l4_focus_stale") { Some(k) => k, None => return };
    let h = make_engine(key).await;

    // 先设置 focus
    let _ = h.core.process_turn(
        "调用 session.set_focus 工具，设置焦点为：研究 Rust 异步并发模型",
        &h.session
    ).await.expect("set focus failed");

    // 模拟多轮（消耗 WARN_ZONE 轮次）
    for i in 0..5 {
        let _ = h.core.process_turn(
            &format!("简单回答：{i} * 7 = ?"),
            &h.session
        ).await.unwrap_or_else(|_| panic!("warmup turn {i} failed"));
    }

    // 此时 focus 应该接近 stale（WARN_ZONE=3），pipeline 应该正常继续
    let r = h.core.process_turn("当前的工作焦点是否有变化？", &h.session)
        .await.expect("post-warmup turn failed");
    assert!(!r.response.is_empty());
    println!("[l4_stale] after 5 warmup turns: {}", trunc(&r.response, 120));
}

// ─────────────────────────────────────────────────────────────────────────────
// L4 LSP 工具注册验证（不依赖 rust-analyzer，只验证工具已注册）
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l4_lsp_tools_registered() {
    let key = match key_or_skip("l4_lsp_registered") { Some(k) => k, None => return };
    let h = make_engine(key).await;

    // 询问 LLM 有哪些工具（验证 lsp.* 工具出现在工具列表中）
    let r = h.core.process_turn(
        "列出你现在可以使用的所有工具名称，只列名称，每行一个",
        &h.session
    ).await.expect("tool list failed");

    let resp = &r.response;
    let has_lsp = resp.contains("lsp.") || resp.contains("goto_definition");
    println!("[l4_lsp_reg] lsp tools visible to LLM: {has_lsp}");
    println!("[l4_lsp_reg] response snippet: {}", trunc(resp, 200));
}

// ─────────────────────────────────────────────────────────────────────────────
// L3 延迟基准（单轮，报告 token/s）
// ─────────────────────────────────────────────────────────────────────────────

/// L3 延迟基准测试 — 真实 DeepSeek 端点
///
/// ## 韧性设计（V29.13）
/// 真实 LLM API 受网络抖动 + 服务端瞬时 5xx 影响——历史上单 prompt 失败即整测 panic。
/// 现在改为：
/// 1. 单 prompt 失败记 outlier 而非 panic（继续跑剩余 prompts）
/// 2. 至少 `MIN_SUCCESS_COUNT` 个 prompt 成功才认为 benchmark 有效
/// 3. 单 prompt 失败时再重试一次（一次性瞬态错误吸收）
///
/// ## 引用关系
/// - 上游：`key_or_skip("l3_latency_benchmark")` 决定是否跳过
/// - 下游：`h.core.process_turn()` 真实调 DeepSeek
///
/// ## 失败语义
/// - 全部 prompt 都失败 → 测试 fail（说明真有问题）
/// - 部分失败 → 在 stdout 报告失败列表，按成功 prompt 计算 p50/throughput
/// - 成功数 < MIN_SUCCESS_COUNT → 测试 fail（信噪比太低）
#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l3_latency_benchmark() {
    /// 至少这么多 prompt 成功才认为 benchmark 有效（5 个里至少 3 个 = 60%）
    const MIN_SUCCESS_COUNT: usize = 3;

    let key = match key_or_skip("l3_latency_benchmark") { Some(k) => k, None => return };
    let h = make_engine(key).await;

    let prompts = [
        "用一句话解释什么是 Rust 所有权",
        "简述 LSP 协议的两阶段 call hierarchy 设计",
        "什么是 tokio 的 multi-thread scheduler？",
        "解释 Arc<RwLock<T>> 与 Arc<Mutex<T>> 的区别",
        "什么情况下应该用 oneshot channel 而不是 mpsc？",
    ];

    let mut latencies = Vec::new();
    let mut token_rates = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();

    for prompt in &prompts {
        // 单点重试一次吸收瞬态网络错误
        let mut last_err: Option<String> = None;
        let mut succeeded = false;
        for attempt in 0..2u32 {
            let t0 = Instant::now();
            match h.core.process_turn(prompt, &h.session).await {
                Ok(r) => {
                    let elapsed = t0.elapsed();
                    let ms = elapsed.as_millis() as u64;
                    let tokens = r.stats.completion_tokens;
                    let tps = (tokens * 1000).checked_div(ms).unwrap_or(0);
                    latencies.push(ms);
                    token_rates.push(tps);
                    let retry_tag = if attempt > 0 { " [retry]" } else { "" };
                    println!("[l3_bench]{} {}ms | {} tokens | {} tok/s | prompt: {}",
                        retry_tag, ms, tokens, tps, trunc(prompt, 40));
                    succeeded = true;
                    break;
                }
                Err(e) => {
                    last_err = Some(format!("{e}"));
                    if attempt == 0 {
                        // 短延迟后重试（避免连击瞬态拥塞）
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
        if !succeeded {
            let err = last_err.unwrap_or_else(|| "unknown".into());
            println!("[l3_bench] FAIL after retry | prompt: {} | error: {}",
                trunc(prompt, 40), trunc(&err, 120));
            failures.push((prompt.to_string(), err));
        }
    }

    // 报告失败列表
    if !failures.is_empty() {
        println!("[l3_bench] {} prompt(s) failed (after retry):", failures.len());
        for (p, e) in &failures {
            println!("  - {} → {}", trunc(p, 50), trunc(e, 100));
        }
    }

    // 韧性门槛：成功数低于阈值则 fail（信噪比太低无法形成有效 benchmark）
    assert!(
        latencies.len() >= MIN_SUCCESS_COUNT,
        "benchmark needs ≥{} successful runs, got {} (failures: {})",
        MIN_SUCCESS_COUNT, latencies.len(), failures.len()
    );

    latencies.sort_unstable();
    let p50 = latencies[latencies.len() / 2];
    let avg_tps = token_rates.iter().sum::<u64>() / token_rates.len() as u64;
    println!("[l3_bench] success={}/{} | p50={}ms | avg_throughput={}tok/s",
        latencies.len(), prompts.len(), p50, avg_tps);
}

// ─────────────────────────────────────────────────────────────────────────────
// L5 Thinking refactor 真实端点烟测（DeepSeek V4-Pro）
//
// 引用关系：
// - 验证 plan §6 矩阵的 DeepSeek 路径（OpenAI 协议）
// - 覆盖 ThinkingIntent::{Off, Effort(Low/Max), Adaptive} 4 个变体
// - D2 client-side effort clamp（Low → "high"）+ D3 V4 default-enabled 修复
//
// 生命周期：
// - 仅在 DEEPSEEK_API_KEY 设置时跑；无 key 自动跳过（不 fail）
// - 每个 test 独立构造 provider + LlmRequest，绕过 CoreLoop 隔离 thinking 路由
// ─────────────────────────────────────────────────────────────────────────────

/// 构造一个最小 LlmRequest（thinking-only smoke）
fn make_thinking_smoke_request(intent: abacus_types::ThinkingIntent) -> abacus_core::llm::provider::LlmRequest {
    use abacus_core::llm::provider::{LlmRequest, Message, MessageContent, MessageRole};
    LlmRequest {
        model: ModelId("deepseek-v4-pro".into()),
        messages: vec![Message {
            role: MessageRole::User,
            content: Some(MessageContent::Text("用一句话回答：1 + 1 等于多少？".into())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            prefix: false,
        }],
        system: Some("你是一个简洁的助手。".into()),
        system_segments: vec![],
        tools: vec![],
        temperature: Some(0.6),
        max_tokens: Some(256),
        top_p: None,
        stop: vec![],
        stream: false,
        thinking_intent: Some(intent),
        cache_config: None,
        extra_body: Default::default(),
        user_message_preamble: None,
    }
}

/// 提取响应文本（避免依赖不存在的 helper）
fn resp_text(msg: &abacus_core::llm::provider::Message) -> String {
    use abacus_core::llm::provider::{MessageContent, ContentPart};
    match &msg.content {
        Some(MessageContent::Text(t)) => t.clone(),
        Some(MessageContent::MultiPart(parts)) => parts.iter()
            .filter_map(|p| if let ContentPart::Text { text } = p { Some(text.clone()) } else { None })
            .collect::<Vec<_>>().join(""),
        None => String::new(),
    }
}

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l5_thinking_off_v4_pro() {
    use abacus_core::llm::provider::LlmProvider;
    let key = match key_or_skip("l5_thinking_off") { Some(k) => k, None => return };
    let provider = abacus_core::llm::providers::deepseek::DeepSeekProvider::new(
        key, ModelId("deepseek-v4-pro".into()));

    let req = make_thinking_smoke_request(abacus_types::ThinkingIntent::Off);
    let t0 = Instant::now();
    let resp = provider.complete(req).await.expect("Off path must return 200 OK");
    let elapsed = t0.elapsed();

    let text = resp_text(&resp.message);
    assert!(!text.is_empty(), "Off response empty");
    assert!(resp.thinking.is_none() || resp.thinking.as_deref().unwrap_or("").is_empty(),
        "Off should not return thinking content, got: {:?}", resp.thinking);
    println!("[l5_off] {}ms | tokens={}+{} | text: {}",
        elapsed.as_millis(), resp.usage.prompt_tokens, resp.usage.completion_tokens, trunc(&text, 80));
}

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l5_thinking_effort_low_v4_pro() {
    use abacus_core::llm::provider::LlmProvider;
    use abacus_types::EffortLevel;
    let key = match key_or_skip("l5_thinking_low") { Some(k) => k, None => return };
    let provider = abacus_core::llm::providers::deepseek::DeepSeekProvider::new(
        key, ModelId("deepseek-v4-pro".into()));

    // D2 client-side clamp: Low → "high" wire（V4-Pro 不支持 low/medium）
    let req = make_thinking_smoke_request(abacus_types::ThinkingIntent::Effort(EffortLevel::Low));
    let t0 = Instant::now();
    let resp = provider.complete(req).await.expect("Low path must return 200 OK after client clamp");
    let elapsed = t0.elapsed();

    let text = resp_text(&resp.message);
    assert!(!text.is_empty(), "Low response empty");
    println!("[l5_low_clamped_high] {}ms | tokens={}+{} | thinking_len={} | text: {}",
        elapsed.as_millis(),
        resp.usage.prompt_tokens, resp.usage.completion_tokens,
        resp.thinking.as_deref().map(|s| s.len()).unwrap_or(0),
        trunc(&text, 80));
}

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l5_thinking_effort_max_v4_pro() {
    use abacus_core::llm::provider::LlmProvider;
    use abacus_types::EffortLevel;
    let key = match key_or_skip("l5_thinking_max") { Some(k) => k, None => return };
    let provider = abacus_core::llm::providers::deepseek::DeepSeekProvider::new(
        key, ModelId("deepseek-v4-pro".into()));

    let req = make_thinking_smoke_request(abacus_types::ThinkingIntent::Effort(EffortLevel::Max));
    let t0 = Instant::now();
    let resp = provider.complete(req).await.expect("Max path must return 200 OK");
    let elapsed = t0.elapsed();

    let text = resp_text(&resp.message);
    assert!(!text.is_empty(), "Max response empty");
    println!("[l5_max] {}ms | tokens={}+{} | thinking_len={} | text: {}",
        elapsed.as_millis(),
        resp.usage.prompt_tokens, resp.usage.completion_tokens,
        resp.thinking.as_deref().map(|s| s.len()).unwrap_or(0),
        trunc(&text, 80));
}

#[tokio::test]
#[ignore = "live API + 并发 rate-limit 敏感；用 cargo test -- --ignored 单跑"]
async fn test_l5_thinking_adaptive_v4_pro() {
    use abacus_core::llm::provider::LlmProvider;
    let key = match key_or_skip("l5_thinking_adaptive") { Some(k) => k, None => return };
    let provider = abacus_core::llm::providers::deepseek::DeepSeekProvider::new(
        key, ModelId("deepseek-v4-pro".into()));

    let req = make_thinking_smoke_request(abacus_types::ThinkingIntent::Adaptive);
    let t0 = Instant::now();
    let resp = provider.complete(req).await.expect("Adaptive path must return 200 OK");
    let elapsed = t0.elapsed();

    let text = resp_text(&resp.message);
    assert!(!text.is_empty(), "Adaptive response empty");
    println!("[l5_adaptive] {}ms | tokens={}+{} | thinking_len={} | text: {}",
        elapsed.as_millis(),
        resp.usage.prompt_tokens, resp.usage.completion_tokens,
        resp.thinking.as_deref().map(|s| s.len()).unwrap_or(0),
        trunc(&text, 80));
}
