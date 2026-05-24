//! Abacus HTTP Server — bridges HTTP requests to CoreLoop
//!
//! ## Dependencies
//! - axum: HTTP framework
//! - abacus_core: CoreLoop, ToolRegistry, SkillEngine, ConfigManager, SecretsManager

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use indexmap::IndexMap;
use axum::extract::State;
use axum::{Router, middleware};
use tokio::sync::RwLock;
use tower_http::cors::{CorsLayer, AllowOrigin};
use tower_http::limit::RequestBodyLimitLayer;

use subtle::ConstantTimeEq;

use abacus_core::config::{ConfigManager, default_config};
use abacus_core::core::{CoreConfig, CoreLoop, SessionState};
use abacus_core::core::context::{ContextManager, SessionSnapshot, SessionStore};
use abacus_core::core::progressive_gate::{GateConfig, ProgressiveGate};
use abacus_core::llm::fallback_provider::FallbackProvider;
use abacus_core::llm::providers::anthropic::AnthropicProvider;
use abacus_core::llm::providers::openai_compatible::OpenAICompatibleProvider;
use abacus_core::secrets::{SecretsManager, SecretType};
use abacus_core::tool::ToolRegistry;
use abacus_core::skill::SkillEngine;
use abacus_core::capability::CapabilityHub;
use abacus_orchestrator::team::TeamManager;
use abacus_types::{KernelError, ModelId};

use crate::routes;

/// 默认最大活跃会话数（可通过 server.max_sessions 配置覆盖）
const DEFAULT_MAX_SESSIONS: usize = 1000;

/// 会话元数据 — 用于 LRU 驱逐
struct SessionEntry {
    session: Arc<RwLock<SessionState>>,
    last_access: std::time::Instant,
}

/// Unified session store — implements SessionStore for snapshot persistence
/// AND manages active session state. This eliminates the dual-track system
/// where AppState and ContextManager each held overlapping session data.
pub struct ServerSessionManager {
    /// P3-B: 用 BoundedFifo 替代 Vec + 手动 drain（统一资源边界抽象）
    snapshots: RwLock<abacus_types::BoundedFifo<SessionSnapshot>>,
    sessions: RwLock<IndexMap<String, SessionEntry>>,
    max_sessions: usize,
    /// 从 ConfigManager 读取的 GateConfig（R1: 配置孤岛修复）
    gate_config: GateConfig,
}

impl Default for ServerSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerSessionManager {
    pub fn new() -> Self {
        Self {
            snapshots: RwLock::new(abacus_types::BoundedFifo::new(SNAPSHOTS_MAX)),
            sessions: RwLock::new(IndexMap::new()),
            max_sessions: DEFAULT_MAX_SESSIONS,
            gate_config: GateConfig::default(),
        }
    }

    /// Create with custom GateConfig and max sessions for session creation.
    pub fn new_with_gate_config(gate_config: GateConfig, max_sessions: usize) -> Self {
        Self {
            snapshots: RwLock::new(abacus_types::BoundedFifo::new(SNAPSHOTS_MAX)),
            sessions: RwLock::new(IndexMap::new()),
            max_sessions,
            gate_config,
        }
    }

    /// 设置最大会话数（L2-5: 从配置读取）
    pub fn set_max_sessions(&mut self, max: usize) {
        self.max_sessions = max;
    }

    /// Get existing session or create new one. Returns (session, is_new).
    pub async fn get_or_create(&self, session_id: &str) -> (Arc<RwLock<SessionState>>, bool) {
        let mut sessions = self.sessions.write().await;
        let is_new = !sessions.contains_key(session_id);
        let gc = self.gate_config.clone();
        let entry = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionEntry {
                session: Arc::new(RwLock::new(SessionState::new_with_gate_config(session_id, gc))),
                last_access: std::time::Instant::now(),
            });
        entry.last_access = std::time::Instant::now();
        let s = entry.session.clone();
        drop(sessions);
        if is_new {
            self.evict_if_needed().await;
        }
        (s, is_new)
    }

    /// Get session by id. Returns None if not found.
    pub async fn get(&self, session_id: &str) -> Option<Arc<RwLock<SessionState>>> {
        let mut sessions = self.sessions.write().await;
        if let Some(entry) = sessions.get_mut(session_id) {
            entry.last_access = std::time::Instant::now();
            Some(entry.session.clone())
        } else {
            None
        }
    }

    /// List all active sessions with their turn counts as (session_id, turn_count).
    pub async fn list(&self) -> Vec<(String, u32)> {
        let sessions = self.sessions.read().await;
        let mut infos = Vec::new();
        for (id, entry) in sessions.iter() {
            let s = entry.session.read().await;
            infos.push((id.clone(), s.turn_count));
        }
        infos
    }

    /// LRU 驱逐: 移除最少访问的会话 (L2-6: FIFO→LRU)
    pub async fn evict_if_needed(&self) {
        let mut sessions = self.sessions.write().await;
        if sessions.len() > self.max_sessions {
            let to_remove = sessions.len() - self.max_sessions;
            // 按 last_access 排序，移除最久未访问的
            let mut entries: Vec<_> = sessions.iter()
                .map(|(k, v)| (k.clone(), v.last_access))
                .collect();
            entries.sort_by_key(|&(_, t)| t);
            for (key, _) in entries.into_iter().take(to_remove) {
                sessions.shift_remove(&key);
            }
        }
    }

    /// Remove a session by id. Returns true if it existed.
    pub async fn remove(&self, session_id: &str) -> bool {
        self.sessions.write().await.shift_remove(session_id).is_some()
    }
}

/// snapshots Vec 上限（C2 修复）—— 与 SecretsManager.audit_log 同款 1000 条上限保护策略
const SNAPSHOTS_MAX: usize = 1000;

#[async_trait::async_trait]
impl SessionStore for ServerSessionManager {
    async fn save(&self, snapshot: SessionSnapshot) -> Result<(), KernelError> {
        let mut store = self.snapshots.write().await;
        // 同 session_id 去重
        store.retain(|s| s.session_id != snapshot.session_id);
        // BoundedFifo 自动 evict 最旧
        store.push(snapshot);
        Ok(())
    }
    async fn load_recent(&self, limit: usize) -> Result<Vec<SessionSnapshot>, KernelError> {
        let store = self.snapshots.read().await;
        Ok(store.iter().rev().take(limit).cloned().collect())
    }
    async fn search(&self, query: &str) -> Result<Vec<SessionSnapshot>, KernelError> {
        let store = self.snapshots.read().await;
        let query_lower = query.to_lowercase();
        let mut results: Vec<SessionSnapshot> = store.iter()
            .filter(|s| {
                s.session_id.to_lowercase().contains(&query_lower)
                    || s.summary.to_lowercase().contains(&query_lower)
                    || s.key_decisions.iter().any(|d| d.to_lowercase().contains(&query_lower))
            })
            .cloned()
            .collect();
        results.sort_by_key(|s| -s.created_at);
        results.truncate(20);
        Ok(results)
    }
}

/// 运行时指标（原子计数，无外部依赖，直接渲染为 Prometheus 格式）
///
/// ## 指标覆盖
/// - 请求维度：total / success / error / timeout
/// - 延迟维度：sum_ms + count（均值）+ 5 个分桶（P95 近似）
/// - LLM 维度：prompt/completion/cached tokens
/// - 工具维度：tool_calls_total
/// - 流式维度：stream_requests_total
///
/// ## 生命周期
/// - 创建：AbacusServer::new()
/// - 写入：chat_handler / stream_handler（每次请求后原子 fetch_add）
/// - 读取：metrics_handler（渲染为 Prometheus 文本）
/// - 重置：不支持（进程重启）
///
/// ## 一致性模型（P3-D）
/// 所有计数器使用 `Ordering::Relaxed` 写入，scrape 可能读到 record_request
/// 中段的瞬时状态（例：total=5 但 latency_count=4）。这是 Prometheus
/// 业界 lock-free metrics 的标准取舍——避免热路径上互斥导致的尾延迟。
///
/// 暴露 `write_epoch` 让 Grafana / scrape 客户端感知"非原子快照"：
/// - epoch 在每次 record_request 前 +1 (奇数 = 写入中)
/// - epoch 在每次 record_request 后 +1 (偶数 = 写入完成)
/// - render 前后两次读 epoch，相同且为偶数 ⇒ 当次 scrape 全程无并发写入
/// - 否则 render 输出额外的 `abacus_metrics_dirty_scrape{}` 计数器供告警
pub struct AbacusMetrics {
    // 请求计数
    pub requests_total:   std::sync::atomic::AtomicU64,
    pub requests_success: std::sync::atomic::AtomicU64,
    pub requests_error:   std::sync::atomic::AtomicU64,
    pub requests_timeout: std::sync::atomic::AtomicU64,
    // 延迟（用于均值 + 分桶 P95 近似）
    pub latency_sum_ms:     std::sync::atomic::AtomicU64,
    pub latency_count:      std::sync::atomic::AtomicU64,
    pub lat_bucket_500ms:   std::sync::atomic::AtomicU64, // < 500ms
    pub lat_bucket_1s:      std::sync::atomic::AtomicU64, // 500ms–1s
    pub lat_bucket_3s:      std::sync::atomic::AtomicU64, // 1s–3s
    pub lat_bucket_10s:     std::sync::atomic::AtomicU64, // 3s–10s
    pub lat_bucket_inf:     std::sync::atomic::AtomicU64, // > 10s
    // LLM Token 使用
    pub tokens_prompt_total:      std::sync::atomic::AtomicU64,
    pub tokens_completion_total:  std::sync::atomic::AtomicU64,
    pub tokens_cached_total:      std::sync::atomic::AtomicU64,
    // 工具调用
    pub tool_calls_total:         std::sync::atomic::AtomicU64,
    // 流式请求
    pub stream_requests_total:    std::sync::atomic::AtomicU64,
    // P3-D: 写入 seqlock epoch（奇数=写入中，偶数=已稳态）
    pub write_epoch:              std::sync::atomic::AtomicU64,
    // P3-D: scrape 期间检测到并发写入的次数（暴露给 Grafana 用于告警一致性误差）
    pub dirty_scrape_total:       std::sync::atomic::AtomicU64,
}

impl AbacusMetrics {
    pub fn new() -> Self {
        use std::sync::atomic::AtomicU64;
        Self {
            requests_total:         AtomicU64::new(0),
            requests_success:       AtomicU64::new(0),
            requests_error:         AtomicU64::new(0),
            requests_timeout:       AtomicU64::new(0),
            latency_sum_ms:         AtomicU64::new(0),
            latency_count:          AtomicU64::new(0),
            lat_bucket_500ms:       AtomicU64::new(0),
            lat_bucket_1s:          AtomicU64::new(0),
            lat_bucket_3s:          AtomicU64::new(0),
            lat_bucket_10s:         AtomicU64::new(0),
            lat_bucket_inf:         AtomicU64::new(0),
            tokens_prompt_total:    AtomicU64::new(0),
            tokens_completion_total:AtomicU64::new(0),
            tokens_cached_total:    AtomicU64::new(0),
            tool_calls_total:       AtomicU64::new(0),
            stream_requests_total:  AtomicU64::new(0),
            write_epoch:            AtomicU64::new(0),
            dirty_scrape_total:     AtomicU64::new(0),
        }
    }

    /// 记录一次请求结果（在 handler 返回前调用）
    ///
    /// P3-D: epoch 协议——开始 +1（奇数表示写入中），完成再 +1（偶数表示稳态）
    pub fn record_request(&self, latency_ms: u64, success: bool, timed_out: bool,
                          prompt_tokens: u64, completion_tokens: u64, cached_tokens: u64,
                          tool_calls: u64) {
        use std::sync::atomic::Ordering::{Relaxed, AcqRel, Acquire};
        // 进入写入临界区
        self.write_epoch.fetch_add(1, AcqRel);
        // Acquire fence ensures subsequent writes can't be reordered before epoch bump
        std::sync::atomic::fence(Acquire);
        self.requests_total.fetch_add(1, Relaxed);
        if timed_out       { self.requests_timeout.fetch_add(1, Relaxed); }
        else if success    { self.requests_success.fetch_add(1, Relaxed); }
        else               { self.requests_error.fetch_add(1, Relaxed); }

        self.latency_sum_ms.fetch_add(latency_ms, Relaxed);
        self.latency_count.fetch_add(1, Relaxed);
        match latency_ms {
            0..=499    => { self.lat_bucket_500ms.fetch_add(1, Relaxed); }
            500..=999  => { self.lat_bucket_1s.fetch_add(1, Relaxed); }
            1000..=2999=> { self.lat_bucket_3s.fetch_add(1, Relaxed); }
            3000..=9999=> { self.lat_bucket_10s.fetch_add(1, Relaxed); }
            _          => { self.lat_bucket_inf.fetch_add(1, Relaxed); }
        }
        self.tokens_prompt_total.fetch_add(prompt_tokens, Relaxed);
        self.tokens_completion_total.fetch_add(completion_tokens, Relaxed);
        self.tokens_cached_total.fetch_add(cached_tokens, Relaxed);
        self.tool_calls_total.fetch_add(tool_calls, Relaxed);
        // 离开写入临界区（epoch 变成偶数）
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        self.write_epoch.fetch_add(1, AcqRel);
    }

    /// 记录流式请求完成（不写延迟分桶，防止 latency_ms=0 污染直方图）
    ///
    /// SSE 流式无确定结束时刻，延迟无意义。只记录成功失败次数和 token 使用。
    /// P3-D: 同样使用 epoch 协议保证 scrape 一致性
    pub fn record_stream_complete(&self, success: bool,
                                  prompt_tokens: u64, completion_tokens: u64,
                                  cached_tokens: u64, tool_calls: u64) {
        use std::sync::atomic::Ordering::{Relaxed, AcqRel, Acquire, Release};
        self.write_epoch.fetch_add(1, AcqRel);
        std::sync::atomic::fence(Acquire);
        self.requests_total.fetch_add(1, Relaxed);
        if success { self.requests_success.fetch_add(1, Relaxed); }
        else       { self.requests_error.fetch_add(1, Relaxed); }
        // 不更新 latency_sum_ms / latency_count / 分桶（流式没有有意义的延迟）
        self.tokens_prompt_total.fetch_add(prompt_tokens, Relaxed);
        self.tokens_completion_total.fetch_add(completion_tokens, Relaxed);
        self.tokens_cached_total.fetch_add(cached_tokens, Relaxed);
        self.tool_calls_total.fetch_add(tool_calls, Relaxed);
        std::sync::atomic::fence(Release);
        self.write_epoch.fetch_add(1, AcqRel);
    }

    /// 渲染为 Prometheus 文本格式
    ///
    /// P3-D: 使用 seqlock 协议保证一次 scrape 内的快照一致性
    /// - 进入前读 epoch_begin（必须为偶数 = 无写入中）
    /// - 加载所有计数器
    /// - 退出后读 epoch_end，若与 begin 不同 → dirty_scrape++
    ///
    /// 一致性失败时仍输出当前快照（best-effort），但暴露 dirty 计数供告警
    pub fn render_prometheus(&self, session_count: usize, team_count: usize) -> String {
        use std::sync::atomic::Ordering::{Relaxed, Acquire};
        // seqlock begin
        let epoch_begin = self.write_epoch.load(Acquire);
        let in_flight_begin = epoch_begin & 1; // 奇数 = 写入中

        let total      = self.requests_total.load(Relaxed);
        let success    = self.requests_success.load(Relaxed);
        let error      = self.requests_error.load(Relaxed);
        let timeout    = self.requests_timeout.load(Relaxed);
        let b500_raw   = self.lat_bucket_500ms.load(Relaxed);
        let b1s_raw    = self.lat_bucket_1s.load(Relaxed);
        let b3s_raw    = self.lat_bucket_3s.load(Relaxed);
        let b10s_raw   = self.lat_bucket_10s.load(Relaxed);
        let lat_count  = self.latency_count.load(Relaxed);
        let lat_sum    = self.latency_sum_ms.load(Relaxed);
        let prompt     = self.tokens_prompt_total.load(Relaxed);
        let completion = self.tokens_completion_total.load(Relaxed);
        let cached     = self.tokens_cached_total.load(Relaxed);
        let tools      = self.tool_calls_total.load(Relaxed);
        let streams    = self.stream_requests_total.load(Relaxed);

        // seqlock end — 检测 scrape 期间是否有并发写入
        let epoch_end = self.write_epoch.load(Acquire);
        if in_flight_begin == 1 || epoch_end != epoch_begin {
            self.dirty_scrape_total.fetch_add(1, Relaxed);
        }
        let dirty_count = self.dirty_scrape_total.load(Relaxed);

        let count = lat_count.max(1); // avoid div-by-zero
        let avg_ms = lat_sum / count;
        format!(
            "# HELP abacus_requests_total Total requests received\n\
             # TYPE abacus_requests_total counter\n\
             abacus_requests_total {total}\n\
             abacus_requests_success_total {success}\n\
             abacus_requests_error_total {error}\n\
             abacus_requests_timeout_total {timeout}\n\
             # HELP abacus_request_latency_ms Request latency distribution\n\
             # TYPE abacus_request_latency_ms histogram\n\
             abacus_request_latency_ms_bucket{{le=\"500\"}} {b500}\n\
             abacus_request_latency_ms_bucket{{le=\"1000\"}} {b1000}\n\
             abacus_request_latency_ms_bucket{{le=\"3000\"}} {b3000}\n\
             abacus_request_latency_ms_bucket{{le=\"10000\"}} {b10000}\n\
             abacus_request_latency_ms_bucket{{le=\"+Inf\"}} {binf}\n\
             abacus_request_latency_ms_sum {sum}\n\
             abacus_request_latency_ms_count {lcount}\n\
             abacus_request_latency_ms_avg {avg}\n\
             # HELP abacus_tokens_total LLM tokens consumed\n\
             # TYPE abacus_tokens_total counter\n\
             abacus_tokens_prompt_total {prompt}\n\
             abacus_tokens_completion_total {completion}\n\
             abacus_tokens_cached_total {cached}\n\
             # HELP abacus_tool_calls_total Tool executions\n\
             # TYPE abacus_tool_calls_total counter\n\
             abacus_tool_calls_total {tools}\n\
             abacus_stream_requests_total {streams}\n\
             # HELP abacus_active_sessions Active sessions\n\
             # TYPE abacus_active_sessions gauge\n\
             abacus_active_sessions {sessions}\n\
             # HELP abacus_active_teams Active teams\n\
             # TYPE abacus_active_teams gauge\n\
             abacus_active_teams {teams}\n\
             # HELP abacus_metrics_dirty_scrape_total Scrapes that occurred during concurrent writes (snapshot may be inconsistent)\n\
             # TYPE abacus_metrics_dirty_scrape_total counter\n\
             abacus_metrics_dirty_scrape_total {dirty}\n\
             # HELP abacus_metrics_write_epoch Internal seqlock counter (even = idle, odd = writing)\n\
             # TYPE abacus_metrics_write_epoch gauge\n\
             abacus_metrics_write_epoch {epoch}\n",
            total   = total,
            success = success,
            error   = error,
            timeout = timeout,
            b500    = b500_raw,
            b1000   = b500_raw + b1s_raw,
            b3000   = b500_raw + b1s_raw + b3s_raw,
            b10000  = b500_raw + b1s_raw + b3s_raw + b10s_raw,
            // +Inf 必须是实际延迟观测次数（latency_count），而非 requests_total
            // requests_total 包含流式连接计数和其他非延迟场景，直方图会失真
            binf    = count,
            sum     = lat_sum,
            lcount  = count,
            avg     = avg_ms,
            prompt  = prompt,
            completion = completion,
            cached  = cached,
            tools   = tools,
            streams = streams,
            sessions= session_count,
            teams   = team_count,
            dirty   = dirty_count,
            epoch   = epoch_end,
        )
    }
}

impl Default for AbacusMetrics {
    fn default() -> Self { Self::new() }
}

/// Shared application state accessible from all request handlers.
pub struct AppState {
    pub core_loop: Arc<CoreLoop>,
    pub sessions: Arc<ServerSessionManager>,
    pub team_manager: Arc<TeamManager>,
    pub config_manager: Arc<ConfigManager>,
    pub specialist_registry: Arc<RwLock<abacus_orchestrator::specialist::SpecialistRegistry>>,
    /// P1: 多 Meeting 实例容器；与 team_manager 同构 register/get/list/remove API
    pub meetings: Arc<abacus_orchestrator::meeting::MeetingStore>,
    pub request_timeout_secs: u64,
    /// 运行时指标（无锁原子计数，所有 handler 共享写入）
    pub metrics: Arc<AbacusMetrics>,
}

/// Per-client rate limiter using a sliding window per client key.
///
/// Uses std::sync::Mutex because critical sections are extremely short
/// and do not cross await points.
///
/// ## 资源边界（C1 修复）
/// `clients` HashMap 在每次 check 时机会性 GC：当条目数 ≥ `RL_GC_THRESHOLD`
/// 时，扫描并移除 last_seen 早于 (now - RL_GC_IDLE_FACTOR × window) 的 bucket，
/// 防止恶意客户端通过不同 Authorization 值膨胀内存。GC 摊销到正常 check 路径，
/// 不引入定时任务（与 SecretsManager.audit_log 的 1000 条上限同款资源保护策略）。
pub struct RateLimiter {
    max_requests: u64,
    window_ns: u64,
    clients: Mutex<HashMap<String, ClientBucket>>,
}

/// 当 clients 数量超过此阈值时触发机会性 GC（防 OOM）
const RL_GC_THRESHOLD: usize = 4096;
/// GC 时移除 last_seen 早于 (now - GC_IDLE_FACTOR × window_ns) 的 bucket
const RL_GC_IDLE_FACTOR: u64 = 5;

struct ClientBucket {
    window_start: u64,
    count: u64,
    /// 最后一次 check 命中时间（用于 GC 决策）
    last_seen: u64,
}

impl RateLimiter {
    pub fn new(max_requests: u64, window_secs: u64) -> Self {
        Self {
            max_requests,
            window_ns: std::time::Duration::from_secs(window_secs).as_nanos() as u64,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Extract a client key from the request. Uses the full Authorization header
    /// for proper rate limit isolation (L2-7: 修复前 8 字符熵不足).
    pub fn client_key(req: &axum::http::Request<axum::body::Body>) -> String {
        req.headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| {
                use sha2::{Sha256, Digest};
                let mut hasher = Sha256::new();
                hasher.update(s.as_bytes());
                format!("{:x}", hasher.finalize())
            })
            .unwrap_or_else(|| "anonymous".into())
    }

    /// Check if a client is within rate limits. Returns true if allowed.
    ///
    /// 副作用：当 `clients.len() >= RL_GC_THRESHOLD` 时，先扫描清理 idle bucket；
    /// 这是 amortized O(1) GC，不阻塞请求路径。
    pub fn check(&self, client_key: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());

        // 机会性 GC：仅当达到阈值时才扫描，不影响普通路径
        if clients.len() >= RL_GC_THRESHOLD {
            let idle_threshold = now.saturating_sub(self.window_ns.saturating_mul(RL_GC_IDLE_FACTOR));
            clients.retain(|_, b| b.last_seen >= idle_threshold);
        }

        let bucket = clients.entry(client_key.to_string()).or_insert(ClientBucket {
            window_start: now,
            count: 0,
            last_seen: now,
        });
        bucket.last_seen = now;

        if now >= bucket.window_start + self.window_ns {
            bucket.window_start = now;
            bucket.count = 1;
            true
        } else {
            bucket.count += 1;
            bucket.count <= self.max_requests
        }
    }
}

fn error_response(status: axum::http::StatusCode, body: &'static str) -> axum::response::Response {
    // H9 修复：builder 失败时仍保持错误状态码，而非默认 200 OK。
    // 用 Response::builder 失败一般只发生在 header 名/值非法上，固定字符串绝不会触发；
    // 但若真触发，构造一个手动 Response 并设置 status 保留语义。
    match axum::http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body))
    {
        Ok(resp) => resp,
        Err(_) => {
            let mut resp = axum::response::Response::new(axum::body::Body::from(body));
            *resp.status_mut() = status;
            resp
        }
    }
}

async fn rate_limit_mw(
    State(rl): State<Arc<RateLimiter>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let key = RateLimiter::client_key(&req);
    if !rl.check(&key) {
        return error_response(axum::http::StatusCode::TOO_MANY_REQUESTS, r#"{"error":"rate limit exceeded"}"#);
    }
    next.run(req).await
}

/// 自适应请求超时计算
///
/// ## 设计原则
/// - `ceiling_secs` 是"极端情况"硬上限（默认 1800s = 30 分钟），
///   正常推理（即使很慢）应远低于此值
/// - 只有真正无响应（LLM 挂死/网络中断）才会触发
/// - 不惩罚深度思考和长上下文——它们需要更多时间
///
/// ## 计算因子
/// - 模型类型：flash 更快，pro/default 需要更长等待
/// - thinking 开启：extended reasoning 显著增加延迟
/// - max_tokens：生成量上界，按平均 40 tokens/s 折算
/// - session 深度：turn 越多上下文越大，LLM prefill 越慢
///
/// ## 生命周期
/// - 每次请求计算一次，无副作用，纯函数
pub(crate) fn adaptive_timeout_secs(
    model_id: &str,
    thinking_enabled: bool,
    max_tokens: u32,
    turn_count: u32,
    ceiling_secs: u64,
) -> u64 {
    const FLOOR_SECS: u64 = 300;

    // 模型基础时间：flash 模型推理更快
    let base: u64 = if model_id.contains("flash") { 600 } else { 900 };

    // Thinking 奖励：extended reasoning 可能需要更长额外时间
    let thinking_bonus: u64 = if thinking_enabled { 600 } else { 0 };

    // Token 生成时间：按保守 15 tokens/s 估算上限
    // max_tokens=8192 → +546s；max_tokens=32000 → +2133s（会被 ceiling 截断）
    let token_time: u64 = (max_tokens as u64) / 15;

    // Session 深度：每 5 轮 +30s，上限 600s（超长会话上下文很大）
    let session_depth: u64 = ((turn_count as u64 / 5) * 30).min(600);

    let computed = base + thinking_bonus + token_time + session_depth;
    // 防止 ceiling_secs < FLOOR_SECS 时 u64::clamp panic（clamp 要求 min ≤ max）
    // 配置错误时取两者最大值作为有效上限，保证行为可预期而非 panic
    let effective_ceiling = ceiling_secs.max(FLOOR_SECS);
    computed.clamp(FLOOR_SECS, effective_ceiling)
}

pub struct AbacusServer {
    router: Router,
    addr: SocketAddr,
}

impl AbacusServer {
    /// Initialize server with ConfigManager-driven configuration,
    /// SecretsManager-backed credentials, and unified session management.
    pub async fn new(addr: SocketAddr) -> Self {
        // ─── Configuration ──────────────────────────────────────────────
        let mut cfg_mgr = ConfigManager::new(default_config());
        cfg_mgr.load_env("ABACUS_");

        // 配置加载顺序：默认层 < models.yaml < config.yaml < security.yaml < conf.d/*.yaml < 环境变量
        // 路径走 abacus_core::paths，遵循 ABACUS_HOME 覆盖。
        use abacus_core::paths;
        let _ = cfg_mgr.load_file(paths::models_yaml());
        let _ = cfg_mgr.load_file(paths::config_yaml());
        let _ = cfg_mgr.load_file(paths::security_yaml());
        cfg_mgr.load_dir(paths::conf_d_dir());

        let default_model = cfg_mgr.get_str("core.default_model")
            .unwrap_or("deepseek-v4-flash");
        let max_turns = cfg_mgr.get_number("core.max_turns")
            .map(|n| n as u32).unwrap_or(50);
        let max_tool_calls = cfg_mgr.get_number("core.max_tool_calls")
            .map(|n| n as u32).unwrap_or(100);
        let temperature = cfg_mgr.get_number("core.temperature").unwrap_or(0.6);
        let max_tokens = cfg_mgr.get_number("core.max_tokens")
            .map(|n| n as u32).unwrap_or(32000);
        let context_window = cfg_mgr.get_number("core.context_window")
            .map(|n| n as usize).unwrap_or(128_000);
        let silent_router = cfg_mgr.get_bool("core.silent_router_enabled").unwrap_or(true);
        let system_prompt = cfg_mgr.get_str("core.system_prompt")
            .unwrap_or("You are Abacus, an intelligent assistant.");

        // Phase 3：统一 thinking 入口，整合新旧 key
        let intent: abacus_types::ThinkingIntent =
            cfg_mgr.get_thinking_intent().unwrap_or(abacus_types::ThinkingIntent::Off);

        // L1 后：thinking_intent 直接传 CoreConfig，无兼容外壳。
        let legacy_effort: Option<abacus_types::ThinkingEffort> = match &intent {
            abacus_types::ThinkingIntent::Off => None,
            abacus_types::ThinkingIntent::Adaptive => Some(abacus_types::ThinkingEffort::High),
            abacus_types::ThinkingIntent::Effort(level) => Some(match level {
                abacus_types::EffortLevel::Minimal | abacus_types::EffortLevel::Low => abacus_types::ThinkingEffort::Low,
                abacus_types::EffortLevel::Medium => abacus_types::ThinkingEffort::Medium,
                abacus_types::EffortLevel::High | abacus_types::EffortLevel::Max | abacus_types::EffortLevel::XHigh => abacus_types::ThinkingEffort::High,
            }),
            abacus_types::ThinkingIntent::Budget(_) => Some(abacus_types::ThinkingEffort::High),
        };
        let thinking_intent = match &intent {
            abacus_types::ThinkingIntent::Off => None,
            _ => Some(intent.clone()),
        };

        let model_spec = Some(abacus_types::ModelSpec {
            context_window,
            max_output_tokens: max_tokens,
            thinking_config: abacus_types::ModelThinkingConfig {
                enabled: intent.is_enabled(),
                effort: legacy_effort,
                preserve_thinking: false,
            },
            ..Default::default()
        });

        // Phase 3：模型能力 catalog——builtin + ~/.abacus/models.yaml 覆盖
        let mut catalog = abacus_core::llm::ModelCatalog::builtin();
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok();
        if let Some(home) = home {
            let yaml_path = std::path::PathBuf::from(home).join(".abacus").join("models.yaml");
            match catalog.merge_yaml(&yaml_path) {
                Ok(0) => {}
                Ok(n) => tracing::info!("Loaded {} model spec override(s) from {}", n, yaml_path.display()),
                Err(e) => tracing::warn!("Failed to merge {}: {}", yaml_path.display(), e),
            }
        }
        let model_catalog = Some(std::sync::Arc::new(catalog));

        let config = CoreConfig {
            max_turns_per_request: max_turns,
            max_tool_calls_per_turn: max_tool_calls,
            default_model: ModelId(default_model.to_string()),
            default_temperature: temperature,
            default_max_tokens: max_tokens,
            system_prompt: system_prompt.to_string(),
            model_spec,
            thinking_intent,
            silent_router_enabled: silent_router,
            model_catalog,
            tool_visibility_threshold: abacus_types::VisibilityTier::D,
            // Task #84/#87：默认开启路由 + 频率剪枝（与 CoreConfig::default 对齐）
            task_kind_routing_enabled: true,
            tool_frequency_pruning_turns: Some(20),
            palace_sync_interval_turns: None,
            default_compress_level: abacus_core::core::context::CompressLevel::Brief,
            // Phase 3 (lint)：从 cfg_mgr 读 lint 配置；缺省 None
            lint_overrides: cfg_mgr.get_typed::<abacus_core::tool::schema_lint::LintOverrides>("lint"),
            // Task #96：单 session 模型升级预算
            max_escalations: cfg_mgr.get_number("core.max_escalations").map(|n| n as u32).unwrap_or(2),
            // W2 (Task #100)：tool result dedup —— 默认关；运维通过 core.dedup.* 显式开启
            tool_result_dedup_enabled: cfg_mgr.get_bool("core.dedup.enabled").unwrap_or(false),
            tool_result_dedup_ttl_secs: cfg_mgr.get_number("core.dedup.ttl_secs").map(|n| n as u64).unwrap_or(60),
            tool_result_dedup_capacity_kb: cfg_mgr.get_number("core.dedup.capacity_kb").map(|n| n as usize).unwrap_or(256),
            adaptive_d_tier_hide: cfg_mgr.get_bool("core.adaptive_d_tier_hide").unwrap_or(false),
            event_sink_enabled: cfg_mgr.get_bool("core.event_sink_enabled").unwrap_or(true),
            scene_tool_loading_enabled: cfg_mgr.get_bool("core.scene_tool_loading").unwrap_or(true),
        };

        // ─── Progressive Gate config ────────────────────────────────────
        // R1: 从 ConfigManager 读取 progressive 配置，替代硬编码默认值
        let gate = ProgressiveGate::from_config_manager(&cfg_mgr);
        let gate_config = gate.config().clone();

        // ─── Engine initialization ──────────────────────────────────────
        let registry = Arc::new(ToolRegistry::new());
        let skill_engine = Arc::new(RwLock::new(SkillEngine::new()));
        let cap_hub = Arc::new(CapabilityHub::new());
        let max_sessions = cfg_mgr.get_number("server.max_sessions")
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_SESSIONS);
        let session_manager = Arc::new(ServerSessionManager::new_with_gate_config(gate_config, max_sessions));
        let session_store: Arc<dyn SessionStore> = session_manager.clone();
        let ctx_mgr = Arc::new(ContextManager::new(session_store));

        let core_loop = CoreLoop::new(registry, skill_engine, cap_hub, ctx_mgr, config).await;

        // ─── 知识库 + 记忆宫殿 wire-up ───────────────────────────────────
        // Server 路径与 CLI 相同：持久化到 ~/.abacus/knowledge.db 和 palace.db。
        // 磁盘失败时静默降级为内存模式，不中断 server 启动。
        {
            let kb_db_path = paths::knowledge_db();
            let palace_db_path = paths::palace_db();

            let kb_store = std::sync::Arc::new(
                abacus_core::knowledge_store::KnowledgeStore::new(&kb_db_path)
                    .unwrap_or_else(|e| {
                        tracing::warn!("KnowledgeStore 磁盘初始化失败，降级内存模式: {e}");
                        abacus_core::knowledge_store::KnowledgeStore::in_memory()
                            .expect("in-memory KnowledgeStore must succeed")
                    })
            );

            let palace_sqlite = abacus_core::memory_palace::SqlitePalaceStore::new(&palace_db_path)
                .ok().map(std::sync::Arc::new);

            let palace = std::sync::Arc::new(tokio::sync::RwLock::new(
                abacus_core::memory_palace::DualPalaceMemory::with_store(palace_sqlite.clone())
            ));

            if let Some(ref store) = palace_sqlite {
                if let Err(e) = store.warmup(&*palace.read().await).await {
                    tracing::warn!("记忆宫殿 warmup 失败（server 从空宫殿启动）: {e}");
                }
            }

            abacus_core::tool::builtin::kb::register_executors(
                core_loop.tool_registry_ref(), kb_store, palace,
            ).await;
        }

        // ─── MagChain 中间件注册 ─────────────────────────────────────────
        // 注册窗口：CoreLoop::new() 之后、Arc::new() 之前。
        // Server 路径使用 PersistentAuditLogger 替代内存 AuditLogger，审计跨 session 可查。
        {
            use abacus_core::mag_chain::{CircuitBreaker, PiiRedactor, PersistentAuditLogger, RateLimiter};
            use std::time::Duration;

            // P10: 熔断 — 连续 5 次失败后熔断，30s 自动恢复
            core_loop.add_middleware(10, Arc::new(CircuitBreaker::new(5, Duration::from_secs(30)))).await;
            // P20: 限流 — 每工具每分钟最多 200 次调用
            core_loop.add_middleware(20, Arc::new(RateLimiter::new(200, Duration::from_secs(60)))).await;
            // P50: 认识论约束 — 与 CoreLoop.epistemic_guard 共享同一 Arc 实例（热插拔版）
            core_loop.add_middleware(50, Arc::clone(core_loop.epistemic_guard()) as Arc<dyn abacus_core::mag_chain::Middleware>).await;
            // P70: PII 脱敏 — 递归清洗 output 中的信用卡/Email/SSN
            core_loop.add_middleware(70, Arc::new(PiiRedactor::new())).await;
            // P100: 持久化审计 — SQLite 落盘，跨 session 可查；失败降级静默
            let audit_path = std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join(".abacus")
                .join("audit.db");
            match PersistentAuditLogger::new(audit_path, 10_000) {
                Ok(logger) => { core_loop.add_middleware(100, Arc::new(logger)).await; }
                Err(e) => { tracing::warn!("PersistentAuditLogger init failed, audit disabled: {e}"); }
            }
        }

        // ─── Provider registration ──────────────────────────────────────
        // 双协议自动回退: 同时注册 Anthropic + OpenAI 协议 provider，
        // FallbackProvider 在主协议失败时自动切换到备用协议。
        // 优先级: Anthropic > OpenAI-compatible > DeepSeek > NoApiKey
        let secrets = SecretsManager::new();

        let anthropic_base_url = cfg_mgr.get_str("llm.anthropic_base_url");
        let anthropic_api_key = cfg_mgr.get_str("llm.anthropic_api_key");
        let openai_base_url = cfg_mgr.get_str("llm.openai_base_url");
        let openai_api_key = cfg_mgr.get_str("llm.openai_api_key");

        let mut has_anthropic = false;
        let mut has_openai = false;

        // 注册 Anthropic provider
        if let Some(api_key) = anthropic_api_key {
            let provider = Arc::new(AnthropicProvider::new(
                api_key.to_string(),
                ModelId(default_model.to_string()),
                anthropic_base_url.map(|s| s.to_string()),
                None,
            ));
            core_loop.register_provider("anthropic", provider.clone()).await;
            has_anthropic = true;

            // H6 修复：仅当同时配置 OpenAI base_url 且 api_key 非空时才注册回退 provider；
            // 空 key 注册等于注入一个保证 401 的 provider，污染 CapabilityHub 候选池。
            if let (Some(base_url), Some(oapi_key)) = (
                openai_base_url,
                openai_api_key.filter(|k| !k.is_empty()),
            ) {
                let oprovider = Arc::new(OpenAICompatibleProvider::new(
                    oapi_key.to_string(),
                    ModelId(default_model.to_string()),
                    base_url.to_string(),
                    None, None, None,
                ));
                core_loop.register_provider("openai-compatible", oprovider.clone()).await;
                has_openai = true;

                // 双协议自动回退
                let fallback = Arc::new(FallbackProvider::new(
                    provider, oprovider, "anthropic", "openai-compatible",
                ));
                core_loop.register_provider("primary", fallback).await;
                // FallbackProvider id="primary" 的自动 adapter 为 NeutralAdapter（不识别内部实现）
                // 显式绑定主协议 adapter：主协议为 Anthropic → AnthropicAdapter
                core_loop.set_adapter("primary", "anthropic").await;
            } else if openai_base_url.is_some() {
                tracing::warn!("OPENAI_BASE_URL 已配置但 OPENAI_API_KEY 为空 — 跳过 OpenAI fallback 注册");
            }
        } else if let (Some(base_url), Some(api_key)) = (
            openai_base_url,
            openai_api_key.filter(|k| !k.is_empty()),
        ) {
            let provider = Arc::new(OpenAICompatibleProvider::new(
                api_key.to_string(),
                ModelId(default_model.to_string()),
                base_url.to_string(),
                None, None, None,
            ));
            core_loop.register_provider("openai-compatible", provider).await;
            has_openai = true;
        }

        // DeepSeek fallback（仅当没有 Anthropic/OpenAI 配置时）
        if !has_anthropic && !has_openai {
            // 优先环境变量，其次配置文件中的 llm.api_key
            let api_key_loaded = secrets.load_from_env("ABACUS_API_KEY", SecretType::ApiKey("deepseek".into()))
                .await.is_ok()
                || secrets.load_from_env("DEEPSEEK_API_KEY", SecretType::ApiKey("deepseek".into()))
                .await.is_ok();

            let api_key = if api_key_loaded {
                secrets.get(&SecretType::ApiKey("deepseek".into()), "abacus-server").await
                    .and_then(|k| k.as_str().map(|s| s.to_string()))
            } else {
                cfg_mgr.get_str("llm.api_key").map(|s| s.to_string())
            };

            if let Some(key) = api_key {
                let provider = Arc::new(abacus_core::llm::providers::deepseek::DeepSeekProvider::new(
                    key,
                    ModelId(default_model.to_string()),
                ));
                core_loop.register_provider("deepseek", provider).await;
            } else {
                core_loop.register_provider("no-api-key", Arc::new(abacus_core::NoApiKeyProvider)).await;
            }
        }
        // ─── MCIP 权限配置（来自 security.yaml）────────────────────────
        core_loop.configure_mcip_permissions(
            &cfg_mgr.get_list("mcip.exempt_prefixes").unwrap_or_default(),
            &cfg_mgr.get_list("mcip.allow_tools").unwrap_or_default(),
            &cfg_mgr.get_list("mcip.deny_tools").unwrap_or_default(),
        );

        // ─── LSP 支持（按需激活）──────────────────────────────────────────
        // 语言服务器是 lazy start：首次调用 lsp.* 工具时才实际启动。
        // 可用 `lsp.enabled = false` 禁用（不安装语言服务器的环境应禁用）。
        if cfg_mgr.get_bool("lsp.enabled").unwrap_or(true) {
            let workspace = std::env::current_dir()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".into());
            core_loop.enable_lsp(workspace).await;
        }

        // ─── Skill workflow executor（默认禁用）──────────────────────────
        // 启用后，SkillDef.workflow 中的步骤会被注册为虚拟 ToolHandle，
        // 让 LLM 可以像调普通工具一样触发 skill 步骤。
        // 引用关系：core_loop.skill_workflow_executor 持有 Weak<ToolRegistry/SkillEngine>，
        // 不构成循环引用；销毁随 CoreLoop drop 自动归零。
        if cfg_mgr.get_bool("core.skill_workflow_enabled").unwrap_or(false) {
            core_loop.enable_skill_workflow_executor().await;
        }

        // ─── AutoEngine 持久化（默认禁用）──────────────────────────────
        // 配置 `auto.persist_path` 指向 SQLite 文件即启用——失败仅 warn 不阻断启动。
        // Pipeline 定义本身仍是运行时注册（重启需重新注册）；持久化的是运行历史。
        if let Some(path) = cfg_mgr.get_str("auto.persist_path") {
            match abacus_core::auto::AutoStore::new(path) {
                Ok(store) => {
                    core_loop.enable_auto_store(std::sync::Arc::new(store)).await;
                    tracing::info!(path = path, "AutoEngine SQLite persistence enabled");
                }
                Err(e) => tracing::warn!(error = %e, "AutoStore init failed; running in-memory"),
            }
        }

        // ─── WASM Plugins（默认禁用，启用必须签名）──────────────────────
        // 配置：
        //   core:
        //     plugins:
        //       base_dir: "/etc/abacus/plugins"   # 触发启用的开关
        //       signing_required: true            # 默认 true；false 仅用于本地开发
        // 安全：每个 plugin/manifest.yaml 应含 signature: { algorithm: sha256, value: <hex> }
        // 部署流程更新 wasm 后必须同步更新 manifest hash（否则启动跳过）。
        if let Some(base_dir) = cfg_mgr.get_str("core.plugins.base_dir") {
            let require_signing = cfg_mgr.get_bool("core.plugins.signing_required").unwrap_or(true);
            match core_loop.enable_plugins_with_options(base_dir.to_string(), require_signing).await {
                Ok(n) => tracing::info!(tools = n, signing = require_signing, "WASM plugins enabled"),
                Err(e) => tracing::error!(error = %e, "plugins enable failed"),
            }
        }

        // ─── MCP server 列表（默认禁用）──────────────────────────────────
        // 配置示例（YAML）：
        //   mcp:
        //     servers:
        //       - server_id: "filengine"
        //         transport: "stdio"
        //         address: "filengine"
        //         tls: false
        //         request_signing: false
        // 单个 server discover_tools 失败会跳过，不中断启动。
        // MCIP policy 默认 NeedsConfirm —— 调用前请通过 mcip.allow_tools 白名单显式授权。
        if let Some(mcp_configs) = cfg_mgr.get_typed::<Vec<abacus_types::McpConfig>>("mcp.servers") {
            if !mcp_configs.is_empty() {
                match core_loop.enable_mcp(mcp_configs).await {
                    Ok(n) => tracing::info!(tools = n, "MCP servers enabled"),
                    Err(e) => tracing::error!(error = %e, "MCP enable failed"),
                }
            }
        }

        let rate_limit = cfg_mgr.get_number("server.rate_limit_per_sec")
            .map(|n| n as u64).unwrap_or(60);

        // request_timeout_secs 语义：自适应超时的硬上限（极端情况保护）
        // 正常请求由 adaptive_timeout_secs() 按 LLM 状态计算实际超时
        // 可通过 server.timeout_ceiling_secs 配置覆盖
        let timeout_ceiling = cfg_mgr.get_number("server.timeout_ceiling_secs")
            .map(|n| n as u64)
            .unwrap_or(1800); // 默认 30 分钟上限

        let app_state = Arc::new(AppState {
            core_loop: Arc::new(core_loop),
            sessions: session_manager,
            team_manager: Arc::new(TeamManager::new()),
            config_manager: Arc::new(cfg_mgr),
            specialist_registry: Arc::new(RwLock::new(abacus_orchestrator::specialist::SpecialistRegistry::new())),
            meetings: Arc::new(abacus_orchestrator::meeting::MeetingStore::new()),
            request_timeout_secs: timeout_ceiling,
            metrics: Arc::new(AbacusMetrics::new()),
        });

        // ─── 模型自动发现（首次启动 + 后台异步）────────────────────────
        // 不阻塞启动：spawn 后台任务，结果写入 ~/.abacus/models.cache.json。
        // 单个 provider 失败 fallback 到 supported_models()；磁盘失败静默降级。
        // /api/v1/models 端点会优先实时拉取；cache 仅作 fallback 兜底。
        {
            let core_for_discover = app_state.core_loop.clone();
            tokio::spawn(async move {
                tracing::info!("background model discovery started");
                let cache = core_for_discover.discover_and_cache(None).await;
                tracing::info!(
                    providers = cache.providers.len(),
                    total_models = cache.all_models().len(),
                    "background model discovery complete"
                );
            });
        }

        // CORS（M7 修复）
        // 生产环境必须显式配置 ABACUS_CORS_ORIGINS；
        // 仅当 ABACUS_ENV != "production" 时才回退到 dev origins，避免无意中放行 localhost。
        let cors = match std::env::var("ABACUS_CORS_ORIGINS") {
            Ok(origins) => {
                let allowed: Vec<_> = origins.split(',')
                    .filter_map(|o| o.trim().parse().ok())
                    .collect();
                CorsLayer::new().allow_origin(AllowOrigin::list(allowed))
            }
            Err(_) => {
                let env = std::env::var("ABACUS_ENV").unwrap_or_default();
                if env == "production" || env == "prod" {
                    tracing::warn!(
                        "ABACUS_ENV=production 但 ABACUS_CORS_ORIGINS 未设置 — 拒绝所有跨域请求"
                    );
                    CorsLayer::new() // 默认 deny all
                } else {
                    tracing::info!("CORS dev mode: allowing localhost:3000/8080 + 127.0.0.1:3000");
                    CorsLayer::new().allow_origin(AllowOrigin::list([
                        "http://localhost:3000".parse().unwrap(),
                        "http://localhost:8080".parse().unwrap(),
                        "http://127.0.0.1:3000".parse().unwrap(),
                    ]))
                }
            }
        };

        let rate_limiter = Arc::new(RateLimiter::new(rate_limit, 1));

        // 安全: rate limiter 在 auth 之前执行，确保失败认证也计入速率限制
        // Body size limit: 10MB max to prevent OOM via large POST payloads
        // Concurrency limit: max 256 in-flight requests (tower layer)
        let router = routes::build_router(app_state)
            .layer(RequestBodyLimitLayer::new(10 * 1024 * 1024)) // 10MB
            .layer(tower::limit::ConcurrencyLimitLayer::new(256))
            .layer(cors)
            .layer(middleware::from_fn_with_state(rate_limiter, rate_limit_mw))
            .layer(middleware::from_fn(auth_middleware));

        Self { router, addr }
    }

    pub async fn serve(self) -> Result<(), Box<dyn std::error::Error>> {
        std::panic::set_hook(Box::new(|info| {
            eprintln!("[FATAL] Abacus panicked: {}", info);
        }));
        tracing::info!("AbacusServer listening on {}", self.addr);
        let listener = match tokio::net::TcpListener::bind(self.addr).await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                eprintln!("[ERROR] 端口 {} 已被占用 — 可能已有 AbacusServer 实例在运行", self.addr);
                eprintln!("  解决: 使用 ABACUS_PORT=<其他端口> 或关闭已有实例");
                return Err(Box::new(e));
            }
            Err(e) => return Err(Box::new(e)),
        };
        axum::serve(listener, self.router)
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        tracing::info!("Server shut down gracefully");
        Ok(())
    }
}

/// Bearer token auth middleware.
///
/// Checks `Authorization: Bearer <token>` header against `ABACUS_SERVER_TOKEN` env var.
async fn auth_middleware(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if req.uri().path() == "/api/v1/health" {
        return next.run(req).await;
    }

    let expected_token = match std::env::var("ABACUS_SERVER_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            // 安全: 未配置 token 时拒绝所有请求，而非跳过认证
            return error_response(
                axum::http::StatusCode::UNAUTHORIZED,
                r#"{"error":"unauthorized","hint":"ABACUS_SERVER_TOKEN not set"}"#,
            );
        }
    };

    let auth_header = req.headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // C5 修复：所有失败路径都执行一次等长 ct_eq，避免按"是否含 Bearer 前缀"提前
    // 短路引入的时序差异。strip_prefix 失败时使用空 token 与 expected 做 ct_eq，
    // 长度不一致的 ct_eq 仍然 constant-time 在内部 padding。
    let provided = auth_header.strip_prefix("Bearer ").unwrap_or("");
    let valid: bool = provided.as_bytes().ct_eq(expected_token.as_bytes()).into();
    // 长度也 constant-time 比较：subtle::ct_eq 对不同长度直接返回 0（不短路），但
    // 我们额外让两个分支走同样的代码路径（避免 if has_prefix 分支前置短路）。
    if valid {
        return next.run(req).await;
    }

    error_response(
        axum::http::StatusCode::UNAUTHORIZED,
        r#"{"error":"unauthorized","hint":"set Authorization: Bearer <ABACUS_SERVER_TOKEN>"}"#,
    )
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("Shutdown signal received");
}
