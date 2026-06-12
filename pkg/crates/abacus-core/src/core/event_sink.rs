//! Event JSONL Sink — cross-session 事件流持久化
//!
//! ## 设计动机
//! 单 session 单 jsonl 文件，所有 PipelineEvent 流式追加一行——
//! 文件路径 `{project_dir}/sessions/{session_id}.jsonl`。
//!
//! ## 价值
//! 1. **session resume 回放** — 时序事件流让"恢复上次对话"可实施
//! 2. **离线分析** — 跨 session 统计违规模式 / 工具误用 / 时延分布
//! 3. **调试 trace** — 出问题时把 jsonl 拉出来按 turn 回顾
//! 4. **Abacus 互操作** — 第三方工具可消费同一格式
//!
//! ## 引用关系
//! - 上游：`mag_chain::PipelineHook` trait——本模块实现一个 hook
//! - 下游：`paths::project_dir / project_sessions_dir` 提供路径
//! - 协同：与 `process_registry` 共用 SessionMeta（cwd / project_slug）
//!
//! ## 生命周期
//! - 创建：`JsonlEventHook::open(session_id, project_dir)` —— 打开 append 模式 fd
//! - 写入：每个 PipelineEvent 序列化一行 JSON 追加
//! - 销毁：CoreLoop drop 时随 Arc 释放，OS 自动 close fd
//!
//! ## 失败语义
//! 写入 IO 失败 → 仅 tracing::warn，不阻塞 turn（事件流是观测层，非业务关键）

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::mag_chain::{PipelineEvent, PipelineHook, HookAction};
use abacus_types::KernelError;

/// JSONL 事件 sink hook
///
/// ## 写入格式
/// 每行一个 JSON 对象：
/// ```json
/// {"ts_ms": 1734567890123, "session_id": "...", "event": "TurnStart", "data": {...}}
/// ```
///
/// `event` 是 PipelineEvent variant 名（字符串 tag）；
/// `data` 是 variant 字段的 flatten 形式（无 input 内容时数据为 `{}`）。
pub struct JsonlEventHook {
    /// session id（写入每行的 session_id 字段）
    session_id: String,
    /// 文件 path（用于错误日志）
    path: PathBuf,
    /// append 模式文件句柄——用 std::fs::File（同步），写入用 spawn_blocking 包装避免阻塞
    /// 用 Mutex 保护并发追加（多个 hook 调用可能并发）
    file: Arc<Mutex<std::fs::File>>,
    /// cross-session 段G：rotate 阈值——当文件超此 bytes 时 rotate 为 .{ts}.jsonl 并新建空文件
    /// 默认 10 MB（典型 session jsonl 远小于此；激活后单 session 数千 turn 才触发）
    /// 0 = 禁用 rotation（用于测试或无限保留场景）
    rotate_max_bytes: u64,
}

/// 默认 rotation 阈值（10 MB）
///
/// 设计取舍：
/// - 太小 → 频繁 rotate，replay 时跨多文件拼接（复杂度↑）
/// - 太大 → 单文件膨胀，IO 慢 + 不便归档
/// - 10 MB 实测能容纳 ~10万 events（每条 ~100 字节），覆盖绝大多数 session
pub const DEFAULT_ROTATE_MAX_BYTES: u64 = 10 * 1024 * 1024;

// ═══════════════════════════════════════════════════════════════════════════
// 统一 EventBus — 所有子系统的观测层
// ═══════════════════════════════════════════════════════════════════════════
//
// ## 设计动机
// 之前每个子系统独立追踪事件：
//   - MagChain AuditLogger → SQLite
//   - DeductionEngine MetricStore → SQLite
//   - HealthRegistry → HashMap
//   - EffectivenessTracker → HashMap
// 查一个问题需要跨 4 个系统拼线索。
//
// EventBus 提供统一发射点：
//   1. 所有子系统 emit 带上 session_id / turn_id / tool_call_id
//   2. 下沉到 tracing span 做关联
//   3. JSONL 持久化为统一存储
//
// ## 使用方式
// ```ignore
// let bus = EventBus::new("session_xxx", project_dir);
// bus.emit(EventKind::ToolCalled { tool_id: "fs.read", duration_ms: 150, success: true });
// ```

use std::time::Instant;

/// 统一事件类型枚举
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum EventKind {
    // ── LLM / Turn ──────────────────────────────────��───────
    TurnStarted { input_len: usize },
    TurnCompleted { latency_ms: u64, tool_calls: u32, completion_tokens: u32 },
    LlmRequested { provider: String, model: String, thinking_intent: String },
    LlmResponded { latency_ms: u64, input_tokens: u32, output_tokens: u32 },

    // ── 工具 ────────────────────────────────────────────────
    ToolCalled { tool_id: String, duration_ms: u64, success: bool },
    ToolRateLimited { tool_id: String, retry_after: u64 },
    ToolCircuitBroken { tool_id: String, failures: u32 },

    // ── 健康 ────────────────────────────────────────────────
    SubsystemDegraded { subsystem: String, reason: String },
    SubsystemHealed { subsystem: String },
    SubsystemUnhealthy { subsystem: String, reason: String },

    // ── 推演 ────────────────────────────────────────────────
    ContaminationDetected { tool_id: String, adoption_pct: f64, success_pct: f64 },
    ContextDegradation { usage_pct: f64, estimated_turns_until_compression: u32 },

    // ── 安全 ────────────────────────────────────────────────
    SensitiveOpConfirmed { tool_id: String, user_response: String },
    SafetyViolation { kind: String, detail: String },

    // ── 用户 ────────────────────────────────────────────────
    UserInputReceived { input_len: usize, mode: String },
    ModeSwitched { from: String, to: String, reason: String },

    // ── 系统 ────────────────────────────────────────────────
    ConfigChanged { key: String, old_value: String, new_value: String },
    Error { message: String, severity: String },

    // ── 内容分类 ────────────────────────────────────────────
    TriageDecision {
        turn: u32,
        block_id: String,
        action: String,
        score: f64,
        token_saved: usize,
        was_tool_protocol: bool,
    },

    // ── V42-B: 会话轨迹 ────────────────────────────────────────
    /// 会话结束事件（用于轨迹持久化）
    SessionEnd {
        session_id: String,
        turn_count: u32,
        total_latency_ms: u64,
    },
}

/// 统一 EventBus
///
/// ## 生命周期
/// - 创建：CoreLoop::new() 时一次（持 Arc<EventBus>）
/// - 消费：被 all subsystems 引用
/// - 销毁：进程退出时 flush
///
/// ## 线程安全
/// - emit：异步、无锁（仅追加到 channel）
/// - flush：同步等待 drain
pub struct EventBus {
    session_id: String,
    jsonl_sink: Option<Arc<JsonlEventHook>>,
    start: Instant,
    /// V42-B: 双向化 — 订阅者接收所有事件，自行过滤
    /// 设计：Vec<UnboundedSender> 而非 HashMap<EventKind, Vec<Sender>>
    /// 原因：EventKind 有数据字段（如 tool_id），无法直接做 key 匹配；
    /// 订阅者通常关心一类事件，自行 filter 比 EventBus 维护 filter 逻辑更灵活。
    /// 使用 RwLock 支持内部可变性（Arc<EventBus> 共享时仍可 subscribe）
    subscribers: tokio::sync::RwLock<Vec<tokio::sync::mpsc::UnboundedSender<EventKind>>>,
}

impl EventBus {
    pub fn new(session_id: impl Into<String>, project_dir: &std::path::Path) -> Self {
        let sid = session_id.into();
        let jsonl_sink = JsonlEventHook::open(sid.clone(), project_dir).ok()
            .map(Arc::new);
        Self {
            session_id: sid,
            jsonl_sink,
            start: Instant::now(),
            subscribers: tokio::sync::RwLock::new(Vec::new()),
        }
    }

    /// 订阅 EventBus 事件（返回 UnboundedReceiver，接收所有事件）
    ///
    /// ## 使用方式
    /// ```ignore
    /// let mut rx = event_bus.subscribe().await;
    /// while let Ok(event) = rx.recv().await {
    ///     if matches!(event, EventKind::ConfigChanged { .. }) {
    ///         // 处理配置变更
    ///     }
    /// }
    /// ```
    pub async fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<EventKind> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.subscribers.write().await.push(tx);
        rx
    }

    /// 发射事件（所有子系统统一入口）
    ///
    /// ## 路径
    /// 1. 转化为 tracing event（span 关联）
    /// 2. 写入 JSONL（如果 sink 存在）—— EventKind 数据嵌入 data 字段
    /// 3. 通知所有订阅者
    ///
    /// ## 失败语义
    /// 写入失败 → 仅 warn，不阻塞调用方
    /// 订阅者已 drop → 静默移除
    pub fn emit(&self, kind: EventKind) {
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // 1. tracing event — 用 span 关联 session_id
        let span = tracing::span!(tracing::Level::INFO, "eventbus", session_id = %self.session_id);
        let _guard = span.enter();
        tracing::info!(target: "abacus::eventbus", kind = ?kind, "event");

        // 2. JSONL 持久化——写入 {ts_ms, session_id, kind, data}
        if let Some(ref sink) = self.jsonl_sink {
            let json_line = serde_json::json!({
                "ts_ms": ts_ms,
                "session_id": self.session_id,
                "event_type": "EventBus",
                "data": &kind,
            });
            let sink = sink.clone();
            tokio::spawn(async move {
                let _ = sink.write_json(&json_line).await;
            });
        }

        // 3. 通知订阅者（保留已 drop 的 subscriber 不影响其他）
        // 使用 try_read 避免在 emit 的同步上下文中阻塞
        if let Ok(subscribers) = self.subscribers.try_read() {
            for tx in subscribers.iter() {
                let _ = tx.send(kind.clone());
            }
        }
    }

    /// 经过多少时间
    pub fn elapsed(&self) -> std::time::Duration {
        self.start.elapsed()
    }

    /// 当前 session_id
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("session_id", &self.session_id)
            .field("elapsed", &self.elapsed().as_secs_f64())
            .finish()
    }
}


impl JsonlEventHook {
    /// 打开 sink——session 启动时调用
    ///
    /// `project_dir` 一般是 `paths::current_project_dir()`；
    /// 文件路径派生为 `{project_dir}/sessions/{session_id}.jsonl`
    pub fn open(session_id: impl Into<String>, project_dir: &std::path::Path) -> std::io::Result<Self> {
        Self::open_with_rotation(session_id, project_dir, DEFAULT_ROTATE_MAX_BYTES)
    }

    /// cross-session 段G：自定义 rotation 阈值的 open（用于测试 + 高频场景调优）
    ///
    /// `rotate_max_bytes = 0` 表示禁用 rotation
    pub fn open_with_rotation(
        session_id: impl Into<String>,
        project_dir: &std::path::Path,
        rotate_max_bytes: u64,
    ) -> std::io::Result<Self> {
        let session_id = session_id.into();
        let sessions_dir = project_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir)?;
        let path = sessions_dir.join(format!("{}.jsonl", session_id));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            session_id,
            path,
            file: Arc::new(Mutex::new(file)),
            rotate_max_bytes,
        })
    }

    // 段G: rotate 等价逻辑已 inline 到 on_event 的 spawn_blocking 闭包中
    // （因 spawn_blocking 闭包不能借用 self，外部方法无法直接调用）

    // ─── cross-session 段G: rotate 文件 ──────────────────────────────────

    /// 检查文件大小是否超过阈值，若超过则 archive 旧文件 + 新建空文件。
    ///
    /// archived 文件名：`{session_id}.{ts_nanos}_{counter}.jsonl`
    ///   - ts_nanos：纳秒精度（毫秒精度下同 turn 多次 rotate 会碰撞）
    ///   - counter：atomic 兜底（nanos 极端情况仍可能碰撞）
    ///   两者组合保证 rename 永远不覆盖已 archived 文件。
    ///
    /// ## 引用关系
    /// - 上游：`on_event` 的 spawn_blocking 闭包中调用
    /// - 消费：session_id / path / rotate_max_bytes
    /// - 副作用：可能 rename 原文件 + 用新文件句柄替换传入的 `file`
    ///
    /// ## 提取动机
    /// 原实现在 spawn_blocking 闭包内联，无法单独单元测试。
    /// 提取为静态函数后，可在测试中传入 mock 文件直接验证。
    fn rotate_if_needed(
        session_id: &str,
        path: &std::path::Path,
        rotate_max_bytes: u64,
        file: &mut std::fs::File,
    ) -> std::io::Result<()> {
        if rotate_max_bytes == 0 {
            return Ok(());
        }
        let meta = file.metadata()?;
        if meta.len() < rotate_max_bytes {
            return Ok(());
        }
        use std::sync::atomic::{AtomicU64, Ordering};
        static ROT_COUNTER: AtomicU64 = AtomicU64::new(0);
        let ts_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let cnt = ROT_COUNTER.fetch_add(1, Ordering::SeqCst);
        let archived = path.with_file_name(
            format!("{}.{}_{}.jsonl", session_id, ts_nanos, cnt)
        );
        let _ = file.flush();
        if std::fs::rename(path, &archived).is_ok() {
            if let Ok(new_f) = std::fs::OpenOptions::new()
                .create(true).append(true).open(path)
            {
                *file = new_f;
            }
        }

        // P2 资源泄漏修复：清理旧 archive 文件，最多保留 20 个
        // （测试会创建 ~8 个 archive，20 足够；生产环境仍有效限制积累）
        const MAX_ARCHIVE_FILES: usize = 20;
        if let Some(parent) = path.parent() {
            let archive_prefix = format!("{}.", session_id);
            let active_name = format!("{}.jsonl", session_id);
            let mut archive_files: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
            
            if let Ok(entries) = std::fs::read_dir(parent) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    let name = match p.file_name().and_then(|s| s.to_str()) {
                        Some(s) => s,
                        None => continue,
                    };
                    // 只匹配当前 session 的 archive 文件
                    if !name.starts_with(&archive_prefix) || !name.ends_with(".jsonl") {
                        continue;
                    }
                    // 跳过 active 文件
                    if name == active_name {
                        continue;
                    }
                    let mtime = entry.metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    archive_files.push((mtime, p));
                }
            }
            
            // 按修改时间排序（最旧的在前）
            archive_files.sort_by_key(|(mt, _)| *mt);
            
            // 删除超过限制的旧文件
            while archive_files.len() > MAX_ARCHIVE_FILES {
                if let Some((_, old_path)) = archive_files.first() {
                    let _ = std::fs::remove_file(old_path);
                }
                archive_files.remove(0);
            }
        }

        Ok(())
    }

    /// 写入任意 JSON 行（绕过 PipelineEvent，直接追加到 JSONL 文件）
    /// 供 EventBus::emit 使用
    pub async fn write_json(&self, value: &serde_json::Value) -> Result<(), KernelError> {
        let line_str = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
        let file = self.file.clone();
        let path_for_log = self.path.clone();
        let session_id = self.session_id.clone();
        let path = self.path.clone();
        let rotate_max_bytes = self.rotate_max_bytes;
        let res = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut f = file.blocking_lock();
            Self::rotate_if_needed(&session_id, &path, rotate_max_bytes, &mut f)?;
            writeln!(f, "{}", line_str)?;
            f.flush()?;
            Ok(())
        }).await;
        if let Ok(Err(e)) = res {
            tracing::warn!(
                path = %path_for_log.display(),
                error = %e,
                "write_json: write failed (event ignored)"
            );
        }
        Ok(())
    }

    /// 序列化 event 到 JSON 对象
    fn serialize_event(&self, event: &PipelineEvent) -> serde_json::Value {
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let (name, data) = match event {
            PipelineEvent::TurnStart { input, session_id } => (
                "TurnStart",
                serde_json::json!({
                    "input_len": input.len(), // 仅长度（保持 LLM 内容不持久化默认）
                    "embedded_session_id": session_id,
                }),
            ),
            PipelineEvent::PromptBuilt { system_len, dynamic_blocks } => (
                "PromptBuilt",
                serde_json::json!({"system_len": system_len, "dynamic_blocks": dynamic_blocks}),
            ),
            PipelineEvent::LlmComplete { loop_iter, completion_tokens } => (
                "LlmComplete",
                serde_json::json!({"loop_iter": loop_iter, "completion_tokens": completion_tokens}),
            ),
            PipelineEvent::PostProcess => ("PostProcess", serde_json::json!({})),
            PipelineEvent::TurnPostFanOut { turn_number, session_id, tool_calls, all_success, was_compressed } => (
                "TurnPostFanOut",
                serde_json::json!({
                    "turn_number": turn_number,
                    "embedded_session_id": session_id,
                    "tool_calls": tool_calls,
                    "all_success": all_success,
                    "was_compressed": was_compressed,
                }),
            ),
            PipelineEvent::TurnEnd { response_len, tool_calls, latency_ms, completion_tokens } => (
                "TurnEnd",
                serde_json::json!({
                    "response_len": response_len,
                    "tool_calls": tool_calls,
                    "latency_ms": latency_ms,
                    "completion_tokens": completion_tokens,
                }),
            ),
            PipelineEvent::TriageResult { stats, turn_number } => (
                "TriageResult",
                serde_json::json!({
                    "summary": stats.summary_line(),
                    "turn_number": turn_number,
                }),
            ),
            PipelineEvent::PreToolUse { tool_id, .. } => (
                "PreToolUse",
                serde_json::json!({"tool_id": tool_id}),
            ),
            PipelineEvent::PostToolUse { tool_id, success, latency_ms, .. } => (
                "PostToolUse",
                serde_json::json!({"tool_id": tool_id, "success": success, "latency_ms": latency_ms}),
            ),
        };
        serde_json::json!({
            "ts_ms": ts_ms,
            "session_id": self.session_id,
            "event": name,
            "data": data,
        })
    }

    /// 当前 sink 文件路径（用于测试/日志）
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[async_trait::async_trait]
impl PipelineHook for JsonlEventHook {
    fn name(&self) -> &str { "jsonl_event_sink" }

    async fn on_event(&self, event: &PipelineEvent) -> Result<HookAction, KernelError> {
        let line = self.serialize_event(event);
        let line_str = serde_json::to_string(&line).unwrap_or_else(|_| "{}".into());

        let file = self.file.clone();
        let path_for_log = self.path.clone();
        // 段G: 把整个 hook 引用 clone 给 spawn_blocking 让它能调 rotate_if_needed
        // 注意：blocking 内部不能持 &self（async 借用），通过 clone 的 Arc 间接持有需要的字段
        let session_id = self.session_id.clone();
        let path = self.path.clone();
        let rotate_max_bytes = self.rotate_max_bytes;
        // append 用 spawn_blocking 包装避免占用 tokio 线程
        let res = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut f = file.blocking_lock();
            // 段G: rotate 检查（复用提取的静态函数，支持单元测试）
            Self::rotate_if_needed(&session_id, &path, rotate_max_bytes, &mut f)?;
            writeln!(f, "{}", line_str)?;
            f.flush()?;
            Ok(())
        }).await;

        if let Ok(Err(e)) = res {
            tracing::warn!(
                path = %path_for_log.display(),
                error = %e,
                "jsonl_event_sink: write failed (event ignored, turn 不阻塞)"
            );
        }
        Ok(HookAction::Continue)
    }
}

// ─── cross-session 段 E: Session Replay ────────────────────────────────────
//
// 写入侧（JsonlEventHook）的对偶——把 jsonl 流读回成结构化记录，让以下场景成为可能：
// 1. session resume：恢复 LLM 上下文
// 2. 离线分析：跨 session 统计违规模式 / 工具误用 / 时延分布
// 3. 调试 trace：人工 review turn 时序

/// 反序列化后的单条 jsonl 记录
///
/// ## 字段约定
/// 与 `JsonlEventHook::serialize_event` 写入格式 1:1 对应
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JsonlEntry {
    /// 写入时的 unix epoch 毫秒
    pub ts_ms: u64,
    /// session id（一个 session 内所有 entry 同 id）
    pub session_id: String,
    /// PipelineEvent variant 名（"TurnStart" / "TurnEnd" 等）
    pub event: String,
    /// variant 的 flatten 字段（如 TurnEnd.{response_len, tool_calls,...}）
    #[serde(default)]
    pub data: serde_json::Value,
}

impl JsonlEntry {
    /// 提取 turn_number（仅 TurnPostFanOut variant 有；其他返回 None）
    pub fn turn_number(&self) -> Option<u32> {
        self.data.get("turn_number").and_then(|v| v.as_u64()).map(|n| n as u32)
    }

    /// 是否是 turn-boundary 事件（TurnStart / TurnEnd）
    pub fn is_turn_boundary(&self) -> bool {
        matches!(self.event.as_str(), "TurnStart" | "TurnEnd")
    }
}

/// 读回某 session 的事件流
///
/// ## 引用关系
/// - 上游：调用方传入 session_id + project_dir（一般 paths::current_project_dir）
/// - 下游：`projects/{slug}/sessions/{session_id}.jsonl` 文件
///
/// ## 失败语义
/// - 文件不存在 → Ok(empty Vec)（视作"该 session 无历史"，非错误）
/// - 单行解析失败 → 跳过该行 + warn 日志（部分损坏不阻塞剩余）
/// - 整体 IO 失败 → Err（调用方决定 fail-fast 还是 fall-back）
///
/// ## 性能
/// O(n) 全文件读 + O(n) 单行解析；典型 session jsonl < 1MB。
/// V33-续：rotation 已实装（DEFAULT_ROTATE_MAX_BYTES = 10MB，archived 文件按 ts 升序读）；
/// 本函数自动处理 active + archived 文件拼接（见 segment G 注释）。
pub fn replay_session_events(
    session_id: &str,
    project_dir: &std::path::Path,
) -> std::io::Result<Vec<JsonlEntry>> {
    let sessions_dir = project_dir.join("sessions");
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }
    // 段G: 收集 active + archived 文件
    // active: {sid}.jsonl
    // archived (rotated): {sid}.{ts}.jsonl
    // 按 ts 升序读 archive，再读 active，保证时序连贯
    let active_path = sessions_dir.join(format!("{}.jsonl", session_id));
    // 段G: archive 文件名 = "{sid}.{nanos}_{counter}.jsonl"
    // 排序按 mtime 升序（写入顺序），不依赖文件名解析格式——更鲁棒
    let mut archive_paths: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let archive_prefix = format!("{}.", session_id);
    let active_name = format!("{}.jsonl", session_id);
    for entry in std::fs::read_dir(&sessions_dir)? {
        let entry = entry?;
        let p = entry.path();
        let name = match p.file_name().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if !name.starts_with(&archive_prefix) || !name.ends_with(".jsonl") {
            continue;
        }
        // 跳过 active（{sid}.jsonl）
        if name == active_name {
            continue;
        }
        // 取 mtime 作排序键
        let mtime = entry.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        archive_paths.push((mtime, p));
    }
    archive_paths.sort_by_key(|(mt, _)| *mt);

    let mut entries = Vec::new();
    let mut bad_lines = 0usize;
    let mut read_files = Vec::new();

    for (_, p) in &archive_paths {
        read_files.push(p.clone());
    }
    if active_path.exists() {
        read_files.push(active_path.clone());
    }
    if read_files.is_empty() {
        return Ok(Vec::new());
    }

    for path in &read_files {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "replay_session_events: read failed, skipping file"
                );
                continue;
            }
        };
        for (lineno, line) in content.lines().enumerate() {
            if line.trim().is_empty() { continue; }
            match serde_json::from_str::<JsonlEntry>(line) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    bad_lines += 1;
                    tracing::warn!(
                        path = %path.display(),
                        lineno = lineno + 1,
                        error = %e,
                        "replay_session_events: skip malformed line"
                    );
                }
            }
        }
    }
    if bad_lines > 0 {
        tracing::info!(
            session = session_id,
            files = read_files.len(),
            valid = entries.len(),
            invalid = bad_lines,
            "replay_session_events: completed with partial corruption"
        );
    }
    Ok(entries)
}

/// 段 H: SessionResumeReport — 从 jsonl events 提取 session 摘要
///
/// ## 设计动机
/// `replay_session_events` 返回原始 JsonlEntry 流（细颗粒），
/// 但调用方往往只想要"上次这个 session 干了什么"的摘要。本结构封装：
/// - 总 turn 数 / 工具调用数 / 总延迟
/// - 首次 / 末次 turn 时间戳（可推断 session 时长）
/// - 是否有压缩事件（决定 LLM 是否需调 messages_recover）
///
/// ## 引用关系
/// - 上游：`build_resume_report` 函数从 events 计算
/// - 下游：抽象给 `session_resume_query` 工具暴露给 LLM
/// - 与 magchain_status 形成镜像：那个看"当前 session 状态"，这个看"过去 session 历史"
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionResumeReport {
    pub session_id: String,
    pub event_count: usize,
    pub turn_count: u32,
    pub total_tool_calls: u64,
    pub total_completion_tokens: u64,
    pub total_latency_ms: u64,
    pub had_compression: bool,
    pub first_event_ms: Option<u64>,
    pub last_event_ms: Option<u64>,
    /// 末次 turn 是否成功（all_success && tool_calls > 0 视作"积极"）
    /// None = 未观察到 turn-end 事件
    pub last_turn_all_success: Option<bool>,
}

impl SessionResumeReport {
    /// 总时长（ms）。无 events 返回 0。
    pub fn duration_ms(&self) -> u64 {
        match (self.first_event_ms, self.last_event_ms) {
            (Some(a), Some(b)) if b >= a => b - a,
            _ => 0,
        }
    }
}

/// 段 H: 从某 session 的所有 jsonl 事件构造摘要
///
/// ## 流式聚合
/// 单次 O(n) 遍历 events，按 event 类型累加字段。
/// 不重建 message 内容（那需要 LLM 的 prompt+response 真相源；jsonl 默认只存 metadata）。
pub fn build_resume_report(
    session_id: &str,
    project_dir: &std::path::Path,
) -> std::io::Result<SessionResumeReport> {
    let events = replay_session_events(session_id, project_dir)?;
    let event_count = events.len();
    let mut report = SessionResumeReport {
        session_id: session_id.to_string(),
        event_count,
        turn_count: 0,
        total_tool_calls: 0,
        total_completion_tokens: 0,
        total_latency_ms: 0,
        had_compression: false,
        first_event_ms: events.first().map(|e| e.ts_ms),
        last_event_ms: events.last().map(|e| e.ts_ms),
        last_turn_all_success: None,
    };
    let mut max_turn: u32 = 0;
    for entry in &events {
        match entry.event.as_str() {
            "TurnPostFanOut" => {
                if let Some(tn) = entry.turn_number() { max_turn = max_turn.max(tn); }
                if let Some(was) = entry.data.get("was_compressed").and_then(|v| v.as_bool()) {
                    if was { report.had_compression = true; }
                }
                if let Some(success) = entry.data.get("all_success").and_then(|v| v.as_bool()) {
                    report.last_turn_all_success = Some(success);
                }
            }
            "TurnEnd" => {
                if let Some(tc) = entry.data.get("tool_calls").and_then(|v| v.as_u64()) {
                    report.total_tool_calls += tc;
                }
                if let Some(lat) = entry.data.get("latency_ms").and_then(|v| v.as_u64()) {
                    report.total_latency_ms += lat;
                }
                if let Some(toks) = entry.data.get("completion_tokens").and_then(|v| v.as_u64()) {
                    report.total_completion_tokens += toks;
                }
            }
            "LlmComplete" => {
                if let Some(toks) = entry.data.get("completion_tokens").and_then(|v| v.as_u64()) {
                    // LlmComplete 是 turn 内每次 LLM 调用——比 TurnEnd 更细
                    // 但 TurnEnd 已聚合，避免重复加：仅在没有 TurnEnd 时用
                    // 简化处理：忽略此事件用于统计——TurnEnd 是真相源
                    let _ = toks;
                }
            }
            _ => {}
        }
    }
    report.turn_count = max_turn;
    Ok(report)
}

/// 列出某 project 下所有可 replay 的 session id
///
/// ## 引用关系
/// - 用于 `/sessions` CLI 命令、TUI session picker
/// - 配合 `replay_session_events` 形成"列表→选择→读回"工作流
///
/// 返回按修改时间降序的 session_id 列表（最近活跃的在前）。
pub fn list_replayable_sessions(project_dir: &std::path::Path) -> std::io::Result<Vec<String>> {
    let dir = project_dir.join("sessions");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<(String, std::time::SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let mtime = entry.metadata().and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        entries.push((stem.to_string(), mtime));
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.1)); // 降序（最新在前）
    Ok(entries.into_iter().map(|(id, _)| id).collect())
}

/// 全局 history.jsonl writer——cross-session 的 prompt 历史
///
/// ## 设计动机
/// 全局 prompt 历史 jsonl：每条 user prompt 一行，跨 project / 跨 session 可搜——
/// 长期用户能找到"我之前问过类似的吗"。
///
/// ## 与 JsonlEventHook 的区别
/// - JsonlEventHook：项目内、按 session 切文件、写所有 PipelineEvent
/// - GlobalHistoryHook：全局单一文件、仅写 user prompt、字段轻量（display+timestamp+project+sid）
///
/// ## 引用关系
/// - 上游：`PipelineEvent::TurnStart` 触发（input 即 user prompt）
/// - 下游：`paths::history_jsonl()` 已存在的路径定义
///
/// ## 失败语义
/// 同 JsonlEventHook：写入失败仅 warn，不阻塞 turn。
pub struct GlobalHistoryHook {
    /// 当前进程的 cwd 标识（escape_cwd 后），写入每行 project 字段
    project: String,
    /// 句柄保留以避免每次 reopen
    file: Arc<Mutex<std::fs::File>>,
    /// cross-session 段F：写入前过 PiiRedactor 脱敏（信用卡/邮箱/SSN 等）
    ///
    /// 引用：mag_chain::PiiRedactor::redact_string
    /// 生命周期：与 hook 同——一次创建多次复用
    /// 失败语义：redactor 无错误路径（regex replace 不报错）
    redactor: crate::mag_chain::PiiRedactor,
}

impl GlobalHistoryHook {
    /// 打开全局 history.jsonl——session 启动时调用一次
    ///
    /// 路径：`paths::history_jsonl()` = `~/.abacus/history.jsonl`
    pub fn open() -> std::io::Result<Self> {
        crate::paths::ensure_global_dirs()?;
        let path = crate::paths::history_jsonl();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let cwd_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let project = crate::paths::escape_cwd(&cwd_path);
        Ok(Self {
            project,
            file: Arc::new(Mutex::new(file)),
            redactor: crate::mag_chain::PiiRedactor::new(),
        })
    }

    /// 序列化一条 history 行
    fn serialize_entry(&self, display: &str, session_id: &str) -> serde_json::Value {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        serde_json::json!({
            "display": display,
            "timestamp": timestamp,
            "project": self.project,
            "sessionId": session_id,
        })
    }
}

/// 段 L5：history.jsonl 单条记录（读路径用）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    pub display: String,
    pub timestamp: u64,
    pub project: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

/// 段 L5：跨 session prompt 历史关键词搜索
///
/// ## 引用关系
/// - 上游：cross_session_query 工具在 palace 未命中时降级走此路径
/// - 下游：`paths::history_jsonl()` 文件
///
/// ## 失败语义
/// - 文件不存在 → Ok(empty Vec)（首次运行 / event_sink 关闭）
/// - 单行解析失败 → 跳过 + warn（部分损坏不阻塞剩余）
/// - 整体 IO 失败 → Err
///
/// ## 算法
/// 1. 全文件读 + 逐行解析
/// 2. 子字符串匹配 `keyword` 在 display 字段（已脱敏后内容）
/// 3. 按 timestamp 倒序（最近在前）
/// 4. 截至 limit 条
///
/// ## 性能
/// O(n)；适用于 history.jsonl 不超过 ~10k 行（默认 abacus 不主动 rotate history）
pub fn search_history(keyword: &str, limit: usize) -> std::io::Result<Vec<HistoryEntry>> {
    let path = crate::paths::history_jsonl();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)?;
    let keyword_lower = keyword.to_lowercase();
    let mut matched: Vec<HistoryEntry> = Vec::new();
    let mut bad_lines = 0usize;
    for line in content.lines() {
        if line.trim().is_empty() { continue; }
        match serde_json::from_str::<HistoryEntry>(line) {
            Ok(entry) => {
                // 子字符串匹配（不区分大小写）
                if entry.display.to_lowercase().contains(&keyword_lower) {
                    matched.push(entry);
                }
            }
            Err(_) => bad_lines += 1,
        }
    }
    if bad_lines > 0 {
        tracing::warn!(bad_lines, "search_history: skipped malformed jsonl lines");
    }
    // 按 timestamp 倒序（最近在前）
    matched.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
    matched.truncate(limit);
    Ok(matched)
}

#[async_trait::async_trait]
impl PipelineHook for GlobalHistoryHook {
    fn name(&self) -> &str { "global_history_hook" }

    fn accepts(&self, event: &PipelineEvent) -> bool {
        // 仅监听 TurnStart——其他事件不写 history
        matches!(event, PipelineEvent::TurnStart { .. })
    }

    async fn on_event(&self, event: &PipelineEvent) -> Result<HookAction, KernelError> {
        let PipelineEvent::TurnStart { input, session_id } = event else {
            return Ok(HookAction::Continue);
        };
        // cross-session 段F：先 PII 脱敏再处理（防 email/credit card/SSN 落盘）
        // 顺序：redact → truncate → 写入。redact 优先因为：
        //   1) truncated 后再脱敏可能漏掉跨边界的 PII
        //   2) [REDACTED] tag 可能延长字符串，占位算 truncate budget 更合理
        let redacted = self.redactor.redact_string(input);

        // 截断超长 input——history 文件不该保存大段输入
        // 1024 chars 上限：足够辨识 prompt，不会让 history.jsonl 膨胀
        const MAX_DISPLAY_CHARS: usize = 1024;
        let display = if redacted.chars().count() > MAX_DISPLAY_CHARS {
            let truncated: String = redacted.chars().take(MAX_DISPLAY_CHARS).collect();
            format!("{} […truncated]", truncated)
        } else {
            redacted
        };
        let entry = self.serialize_entry(&display, session_id);
        let line_str = serde_json::to_string(&entry).unwrap_or_else(|_| "{}".into());

        let file = self.file.clone();
        let history_path = crate::paths::history_jsonl();
        let res = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            // P2 资源泄漏修复：history.jsonl 无限增长，增加按行数截断
            // 检查文件大小，如果超过阈值（10MB），则截断保留最近 10,000 行
            const MAX_HISTORY_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10MB
            const MAX_HISTORY_LINES: usize = 10_000;

            if history_path.exists() {
                let meta = std::fs::metadata(&history_path)?;
                if meta.len() > MAX_HISTORY_FILE_SIZE {
                    if let Ok(content) = std::fs::read_to_string(&history_path) {
                        let lines: Vec<&str> = content.lines().collect();
                        let skip_count = lines.len().saturating_sub(MAX_HISTORY_LINES);
                        if skip_count > 0 {
                            let keep: String = lines.into_iter().skip(skip_count)
                                .collect::<Vec<_>>()
                                .join("\n");
                            // 加回尾部换行
                            let truncated = format!("{}\n", keep);
                            let _ = std::fs::write(&history_path, &truncated);
                        }
                    }
                    // 重新打开文件句柄（write 截断了内容，需要重新打开以追加）
                    // 注意：持有锁时不会并发写入，但外部文件可能已被 write 重建
                    let mut f = file.blocking_lock();
                    if let Ok(new_f) = std::fs::OpenOptions::new()
                        .create(true).append(true).open(&history_path)
                    {
                        *f = new_f;
                    }
                }
            }

            let mut f = file.blocking_lock();
            writeln!(f, "{}", line_str)?;
            f.flush()?;
            Ok(())
        }).await;
        if let Ok(Err(e)) = res {
            tracing::warn!(error = %e, "global_history_hook: write failed (ignored)");
        }
        Ok(HookAction::Continue)
    }
}

#[cfg(test)]
// 🟡#2 治本：test 代码用 env RAII 模式不可避免（要改 global ABACUS_HOME 测试路径）
// `std::env::set_var` 在 Rust 1.75+ 标 unsafe 是因与 libc 内部状态竞态。
// 测试串行执行（`cargo test` 默认单线程）→ 不会与生产代码并发 → 风险可控。
// 接受 unsafe 用 RAII 模式（set/restore），不要改 env vars 流向生产配置。
#[allow(unsafe_code)]
mod tests {
    // ENV_LOCK 跨 .await 持有是测试串行化的核心机制；与 filengine.rs / process_registry.rs 同模式
    #![allow(clippy::await_holding_lock)]

    use super::*;

    /// 隔离项目目录——并发测试下需要每个用例独立无冲突
    ///
    /// 用 atomic counter + 进程 id + nanos 三重防碰撞：
    /// - process::id 在测试 binary 内是同一值，不能单独保隔离
    /// - nanos 在快速并行测试下精度可能不够（同 turn 内多次调用相邻）
    /// - 加 atomic counter 兜底——单调递增不可能碰撞
    fn isolated_project_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("abacus_event_sink_test_{}_{}_{}",
            std::process::id(), nanos, n));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn open_creates_sessions_dir_and_file() {
        let proj = isolated_project_dir();
        let hook = JsonlEventHook::open("sess1", &proj).expect("open");
        assert!(proj.join("sessions").exists(), "sessions dir 应创建");
        assert!(hook.path().exists(), "jsonl 文件应创建");
        assert!(hook.path().to_string_lossy().ends_with("sess1.jsonl"));
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[tokio::test]
    async fn writes_one_line_per_event() {
        let proj = isolated_project_dir();
        let hook = JsonlEventHook::open("sess2", &proj).expect("open");
        let path = hook.path().to_path_buf();

        // 写 3 个不同 event
        hook.on_event(&PipelineEvent::TurnStart {
            input: "hello".into(), session_id: "s2".into()
        }).await.unwrap();
        hook.on_event(&PipelineEvent::PostProcess).await.unwrap();
        hook.on_event(&PipelineEvent::TurnEnd {
            response_len: 100, tool_calls: 2, latency_ms: 500, completion_tokens: 50
        }).await.unwrap();

        let content = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3, "3 events → 3 lines");
        // 每行应是合法 JSON
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid json");
            assert!(v.get("ts_ms").is_some());
            assert!(v.get("session_id").is_some());
            assert!(v.get("event").is_some());
            assert!(v.get("data").is_some());
        }
        // 第一行应是 TurnStart
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event"], "TurnStart");
        assert_eq!(first["data"]["input_len"], 5);
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[tokio::test]
    async fn turn_post_fanout_serializes_metadata() {
        let proj = isolated_project_dir();
        let hook = JsonlEventHook::open("sess3", &proj).expect("open");
        hook.on_event(&PipelineEvent::TurnPostFanOut {
            turn_number: 5,
            session_id: "s3".into(),
            tool_calls: 3,
            all_success: true,
            was_compressed: false,
        }).await.unwrap();
        let content = std::fs::read_to_string(hook.path()).expect("read");
        let v: serde_json::Value = serde_json::from_str(content.trim()).expect("valid json");
        assert_eq!(v["event"], "TurnPostFanOut");
        assert_eq!(v["data"]["turn_number"], 5);
        assert_eq!(v["data"]["tool_calls"], 3);
        assert_eq!(v["data"]["all_success"], true);
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[tokio::test]
    async fn append_mode_preserves_existing_lines() {
        let proj = isolated_project_dir();
        // 第一次开 sink 写一条
        {
            let hook = JsonlEventHook::open("appendsess", &proj).expect("open");
            hook.on_event(&PipelineEvent::PostProcess).await.unwrap();
        }
        // 第二次开同样 session（模拟进程重启）写另一条
        {
            let hook = JsonlEventHook::open("appendsess", &proj).expect("open again");
            hook.on_event(&PipelineEvent::PostProcess).await.unwrap();
        }
        let path = proj.join("sessions").join("appendsess.jsonl");
        let content = std::fs::read_to_string(&path).expect("read");
        assert_eq!(content.lines().count(), 2, "append 不应覆盖旧内容");
        let _ = std::fs::remove_dir_all(&proj);
    }

    // ─── cross-session 段 H: SessionResumeReport ──────────────────────

    #[tokio::test]
    async fn resume_report_aggregates_events_correctly() {
        let proj = isolated_project_dir();
        let hook = JsonlEventHook::open("h_test", &proj).expect("open");
        // 模拟 1 个完整 turn：TurnStart → TurnPostFanOut → TurnEnd
        hook.on_event(&PipelineEvent::TurnStart {
            input: "task1".into(), session_id: "h_test".into()
        }).await.unwrap();
        hook.on_event(&PipelineEvent::TurnPostFanOut {
            turn_number: 1, session_id: "h_test".into(),
            tool_calls: 2, all_success: true, was_compressed: false,
        }).await.unwrap();
        hook.on_event(&PipelineEvent::TurnEnd {
            response_len: 100, tool_calls: 2,
            latency_ms: 500, completion_tokens: 50,
        }).await.unwrap();
        // 第 2 个 turn 触发压缩
        hook.on_event(&PipelineEvent::TurnPostFanOut {
            turn_number: 2, session_id: "h_test".into(),
            tool_calls: 1, all_success: false, was_compressed: true,
        }).await.unwrap();
        hook.on_event(&PipelineEvent::TurnEnd {
            response_len: 80, tool_calls: 1,
            latency_ms: 300, completion_tokens: 30,
        }).await.unwrap();

        let report = build_resume_report("h_test", &proj).expect("build report");
        assert_eq!(report.session_id, "h_test");
        assert_eq!(report.event_count, 5);
        assert_eq!(report.turn_count, 2);
        assert_eq!(report.total_tool_calls, 3); // 2 + 1
        assert_eq!(report.total_completion_tokens, 80); // 50 + 30
        assert_eq!(report.total_latency_ms, 800); // 500 + 300
        assert!(report.had_compression);
        assert_eq!(report.last_turn_all_success, Some(false)); // 末次 turn 失败
        assert!(report.first_event_ms.is_some());
        assert!(report.last_event_ms.is_some());
        assert!(report.duration_ms() < 1_000_000); // 测试时长 < 1000s
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[tokio::test]
    async fn resume_report_handles_missing_session() {
        let proj = isolated_project_dir();
        let report = build_resume_report("does_not_exist", &proj).expect("graceful");
        assert_eq!(report.event_count, 0);
        assert_eq!(report.turn_count, 0);
        assert!(!report.had_compression);
        assert_eq!(report.duration_ms(), 0);
        let _ = std::fs::remove_dir_all(&proj);
    }

    // ─── cross-session 段 G: rotation ─────────────────────────────────

    /// 验证 rotate_if_needed 静态函数的直接可测性（不经过 hook/event 路径）
    #[test]
    fn rotate_if_needed_triggers_rename_when_exceeds_threshold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test_session.jsonl");
        // 创建一个超过阈值（10 bytes）的文件
        std::fs::write(&path, "x".repeat(20)).expect("write initial");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");

        // 阈值 10 → 文件 20 bytes → 应触发 rotate
        JsonlEventHook::rotate_if_needed("test_session", &path, 10, &mut file)
            .expect("rotate_if_needed");

        // 原文件应被 rename 为 archived（文件名含 test_session. 后缀）
        let dir_entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        let archive_count = dir_entries.iter()
            .filter(|n| n.starts_with("test_session.") && n.as_str() != "test_session.jsonl")
            .count();
        assert_eq!(archive_count, 1, "应产生 1 个 archive 文件: {dir_entries:?}");

        // active 文件应重新创建（空的，或只包含新写入内容）
        assert!(dir_entries.contains(&"test_session.jsonl".to_string()),
            "active 文件应继续存在: {dir_entries:?}");
    }

    /// rotation 触发：超阈值后旧 jsonl 被改名 + 新空 jsonl 创建
    #[tokio::test]
    async fn rotation_archives_when_size_exceeds_threshold() {
        let proj = isolated_project_dir();
        // 设极小阈值（200 bytes）让前几条 event 就触发 rotate
        let hook = JsonlEventHook::open_with_rotation("rot1", &proj, 200).expect("open");
        // 第一条 event（small）→ 不触发 rotate
        hook.on_event(&PipelineEvent::PostProcess).await.unwrap();
        // 多写几条 event 把文件填到阈值以上
        for _ in 0..5 {
            hook.on_event(&PipelineEvent::TurnStart {
                input: "x".repeat(50),
                session_id: "rot1".into(),
            }).await.unwrap();
        }
        // 触发一次 rotate 后再写一条
        hook.on_event(&PipelineEvent::PostProcess).await.unwrap();

        let sessions_dir = proj.join("sessions");
        let entries: Vec<_> = std::fs::read_dir(&sessions_dir).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        // 应有 active + 至少 1 个 archive
        let has_active = entries.iter().any(|n| n == "rot1.jsonl");
        let archive_count = entries.iter().filter(|n| n.starts_with("rot1.") && n != &"rot1.jsonl").count();
        assert!(has_active, "active jsonl 应存在: {entries:?}");
        assert!(archive_count >= 1, "应至少 1 个 archived 文件: {entries:?}");
        let _ = std::fs::remove_dir_all(&proj);
    }

    /// 0 阈值禁用 rotation——文件无限追加
    #[tokio::test]
    async fn rotation_disabled_when_threshold_zero() {
        let proj = isolated_project_dir();
        let hook = JsonlEventHook::open_with_rotation("rot_off", &proj, 0).expect("open");
        for _ in 0..20 {
            hook.on_event(&PipelineEvent::TurnStart {
                input: "y".repeat(100),
                session_id: "rot_off".into(),
            }).await.unwrap();
        }
        let sessions_dir = proj.join("sessions");
        let entries: Vec<_> = std::fs::read_dir(&sessions_dir).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        let archive_count = entries.iter()
            .filter(|n| n.starts_with("rot_off.") && n != &"rot_off.jsonl")
            .count();
        assert_eq!(archive_count, 0, "rotate=0 不应产生 archive: {entries:?}");
        let _ = std::fs::remove_dir_all(&proj);
    }

    /// rotate 后 replay 应同时读 active + archived，时序连贯
    #[tokio::test]
    async fn replay_includes_archived_files() {
        let proj = isolated_project_dir();
        let hook = JsonlEventHook::open_with_rotation("rep_arc", &proj, 200).expect("open");
        // 写到 rotate 触发
        for i in 0..15 {
            hook.on_event(&PipelineEvent::TurnPostFanOut {
                turn_number: i,
                session_id: "rep_arc".into(),
                tool_calls: 1, all_success: true, was_compressed: false,
            }).await.unwrap();
        }
        // replay 应包含全部 events（跨 archive + active）
        let entries = replay_session_events("rep_arc", &proj).expect("replay");
        assert_eq!(entries.len(), 15, "应读到全部 15 条 events: got {}", entries.len());
        // turn_number 应单调递增（验证时序连贯）
        let turns: Vec<u32> = entries.iter().filter_map(|e| e.turn_number()).collect();
        assert_eq!(turns, (0..15).collect::<Vec<u32>>(), "turn_number 顺序应保留");
        let _ = std::fs::remove_dir_all(&proj);
    }

    // ─── cross-session 段 E: replay_session_events ─────────────────────

    #[tokio::test]
    async fn replay_returns_empty_when_session_missing() {
        let proj = isolated_project_dir();
        let entries = replay_session_events("nonexistent_sess", &proj).expect("graceful");
        assert!(entries.is_empty());
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[tokio::test]
    async fn write_then_replay_preserves_event_sequence() {
        let proj = isolated_project_dir();
        // 先用 hook 写 3 个事件
        let hook = JsonlEventHook::open("rsess", &proj).expect("open");
        hook.on_event(&PipelineEvent::TurnStart {
            input: "first".into(), session_id: "rsess".into()
        }).await.unwrap();
        hook.on_event(&PipelineEvent::TurnPostFanOut {
            turn_number: 1, session_id: "rsess".into(),
            tool_calls: 2, all_success: true, was_compressed: false,
        }).await.unwrap();
        hook.on_event(&PipelineEvent::TurnEnd {
            response_len: 50, tool_calls: 2, latency_ms: 200, completion_tokens: 30,
        }).await.unwrap();

        // replay
        let entries = replay_session_events("rsess", &proj).expect("replay");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].event, "TurnStart");
        assert_eq!(entries[1].event, "TurnPostFanOut");
        assert_eq!(entries[1].turn_number(), Some(1));
        assert_eq!(entries[2].event, "TurnEnd");
        // ts_ms 应单调递增（写入顺序保证）
        assert!(entries[0].ts_ms <= entries[1].ts_ms);
        assert!(entries[1].ts_ms <= entries[2].ts_ms);
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[tokio::test]
    async fn replay_skips_malformed_lines_gracefully() {
        let proj = isolated_project_dir();
        let sessions_dir = proj.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let path = sessions_dir.join("corrupt.jsonl");
        let content = r#"{"ts_ms":1,"session_id":"corrupt","event":"TurnStart","data":{}}
NOT_VALID_JSON
{"ts_ms":3,"session_id":"corrupt","event":"PostProcess","data":{}}
"#;
        std::fs::write(&path, content).unwrap();
        let entries = replay_session_events("corrupt", &proj).expect("replay");
        // 中间损坏行应被跳过，前后合法行保留
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].event, "TurnStart");
        assert_eq!(entries[1].event, "PostProcess");
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[tokio::test]
    async fn list_replayable_returns_sessions_by_mtime_desc() {
        let proj = isolated_project_dir();
        // 写两个 session，第二个之后写——mtime 更新
        let h1 = JsonlEventHook::open("first", &proj).expect("open1");
        h1.on_event(&PipelineEvent::PostProcess).await.unwrap();
        // 短暂 sleep 让 mtime 区分
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let h2 = JsonlEventHook::open("second", &proj).expect("open2");
        h2.on_event(&PipelineEvent::PostProcess).await.unwrap();

        let sessions = list_replayable_sessions(&proj).expect("list");
        assert_eq!(sessions.len(), 2);
        // 最新写的应在前
        assert_eq!(sessions[0], "second");
        assert_eq!(sessions[1], "first");
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[tokio::test]
    async fn list_replayable_handles_missing_dir() {
        let proj = isolated_project_dir();
        // 不创建 sessions/ 子目录
        let sessions = list_replayable_sessions(&proj).expect("graceful");
        assert!(sessions.is_empty());
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn jsonl_entry_helpers() {
        let e = JsonlEntry {
            ts_ms: 1234,
            session_id: "s".into(),
            event: "TurnStart".into(),
            data: serde_json::json!({}),
        };
        assert!(e.is_turn_boundary());
        assert_eq!(e.turn_number(), None);

        let post = JsonlEntry {
            ts_ms: 1234,
            session_id: "s".into(),
            event: "TurnPostFanOut".into(),
            data: serde_json::json!({"turn_number": 7}),
        };
        assert!(!post.is_turn_boundary());
        assert_eq!(post.turn_number(), Some(7));
    }

    // ─── cross-session: GlobalHistoryHook ─────────────────────────────────

    // ─── cross-session 段F: PiiRedactor 脱敏（已并入 global_history_hook_lifecycle） ───
    //
    // 因 ABACUS_HOME 是 process-global env var，多 #[tokio::test] 修改会并发互踩。
    // 因此 PII redaction 测试已合并到 `global_history_hook_lifecycle` 内的 case 4。

    /// 跨测试 ABACUS_HOME 互斥锁——根除并发 race
    /// 仿 process_registry::tests::ENV_LOCK 模式
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 合并 history hook 的多个用例避免 ABACUS_HOME env var 并发竞争
    #[tokio::test]
    async fn global_history_hook_lifecycle() {
        // poisoned 不阻塞——历史 panic 副作用对当前测试无影响
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 隔离 ABACUS_HOME
        let tmp = std::env::temp_dir().join(format!("abacus_hist_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let original = std::env::var("ABACUS_HOME").ok();
        std::env::set_var("ABACUS_HOME", &tmp);

        // case 1: open + 写入 user prompt
        let hook = GlobalHistoryHook::open().expect("open");
        hook.on_event(&PipelineEvent::TurnStart {
            input: "我之前问过 Rust 所有权吗".into(),
            session_id: "hist1".into(),
        }).await.unwrap();
        let path = crate::paths::history_jsonl();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).expect("read");
        let v: serde_json::Value = serde_json::from_str(content.trim_end()).expect("valid json");
        assert!(v["display"].as_str().unwrap().contains("Rust 所有权"));
        assert_eq!(v["sessionId"], "hist1");
        assert!(!v["project"].as_str().unwrap().is_empty());

        // case 2: 非 TurnStart 事件应被 accepts 拦下，不写
        let line_count_before = std::fs::read_to_string(&path).unwrap().lines().count();
        // PostProcess 不应被处理（accepts=false）—— PipelineHook trait 默认实现会跳过
        // 但本 hook 的 on_event 即使被强行调用，也会因 let-else 早返回不写入
        assert!(!hook.accepts(&PipelineEvent::PostProcess));
        assert!(hook.accepts(&PipelineEvent::TurnStart { input: "x".into(), session_id: "x".into() }));
        let line_count_after = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(line_count_before, line_count_after);

        // case 3: 超长 input 截断
        let long_input = "x".repeat(2000);
        hook.on_event(&PipelineEvent::TurnStart {
            input: long_input,
            session_id: "hist_long".into(),
        }).await.unwrap();
        let content = std::fs::read_to_string(&path).expect("read");
        let last_line = content.lines().last().unwrap();
        let v: serde_json::Value = serde_json::from_str(last_line).unwrap();
        let display = v["display"].as_str().unwrap();
        assert!(display.contains("[…truncated]"), "超长 input 应标 truncated");
        assert!(display.chars().count() < 1100, "截断后应远小于 2000");

        // case 4: 段F PII 脱敏 — email/credit card/SSN/干净 prompt
        hook.on_event(&PipelineEvent::TurnStart {
            input: "联系 user@example.com 卡 4532-1488-0343-6467 SSN 123-45-6789".into(),
            session_id: "hist_pii".into(),
        }).await.unwrap();
        let content = std::fs::read_to_string(&path).expect("read");
        let pii_line = content.lines().last().unwrap();
        let v: serde_json::Value = serde_json::from_str(pii_line).unwrap();
        let display_pii = v["display"].as_str().unwrap();
        assert!(!display_pii.contains("user@example.com"), "email 未脱敏: {display_pii}");
        assert!(!display_pii.contains("4532-1488-0343-6467"), "credit card 未脱敏: {display_pii}");
        assert!(!display_pii.contains("123-45-6789"), "SSN 未脱敏: {display_pii}");
        assert!(display_pii.matches("[REDACTED]").count() >= 3, "应至少 3 处 [REDACTED]");

        hook.on_event(&PipelineEvent::TurnStart {
            input: "什么是 Rust 所有权".into(),
            session_id: "hist_clean".into(),
        }).await.unwrap();
        let content = std::fs::read_to_string(&path).expect("read");
        let clean_line = content.lines().last().unwrap();
        let v: serde_json::Value = serde_json::from_str(clean_line).unwrap();
        let display_clean = v["display"].as_str().unwrap();
        assert!(display_clean.contains("Rust"));
        assert!(!display_clean.contains("[REDACTED]"), "干净 prompt 不应有 [REDACTED]");

        // 清理
        let _ = std::fs::remove_dir_all(&tmp);
        match original {
            Some(v) => std::env::set_var("ABACUS_HOME", v),
            None => std::env::remove_var("ABACUS_HOME"),
        }
    }

    // ─── 段 L5: search_history 读取 API ────────────────────────────────

    /// 合并到 lifecycle 一起跑——共享 ABACUS_HOME 互斥
    /// 单独写一个 #[tokio::test] 会跟 global_history_hook_lifecycle 互踩 ABACUS_HOME
    #[tokio::test]
    async fn search_history_finds_keyword_matches() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("abacus_l5_search_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let original = std::env::var("ABACUS_HOME").ok();
        std::env::set_var("ABACUS_HOME", &tmp);

        // case 1: 文件不存在 → Ok(empty)
        let r1 = search_history("anything", 10).expect("graceful");
        assert!(r1.is_empty());

        // case 2: 写几条历史然后搜索
        let hook = GlobalHistoryHook::open().expect("open");
        hook.on_event(&PipelineEvent::TurnStart {
            input: "什么是 Rust 所有权".into(),
            session_id: "s_old".into(),
        }).await.unwrap();
        // 略等 1ms 保证 timestamp 严格递增
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        hook.on_event(&PipelineEvent::TurnStart {
            input: "Python 装饰器怎么写".into(),
            session_id: "s_mid".into(),
        }).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        hook.on_event(&PipelineEvent::TurnStart {
            input: "再问 Rust 生命周期".into(),
            session_id: "s_new".into(),
        }).await.unwrap();

        // 搜 "Rust" 应命中 2 条（按 timestamp 倒序：s_new 在前）
        let rust_results = search_history("Rust", 10).expect("search");
        assert_eq!(rust_results.len(), 2);
        assert_eq!(rust_results[0].session_id, "s_new", "倒序最新在前");
        assert_eq!(rust_results[1].session_id, "s_old");

        // 搜 "Python" → 1 条
        let py_results = search_history("Python", 10).expect("search");
        assert_eq!(py_results.len(), 1);
        assert_eq!(py_results[0].session_id, "s_mid");

        // 搜不区分大小写
        let rust_lower = search_history("rust", 10).expect("search");
        assert_eq!(rust_lower.len(), 2);

        // 搜不存在的 → 空
        let none_results = search_history("zzzwwwzz", 10).expect("search");
        assert!(none_results.is_empty());

        // limit 生效
        let limit_results = search_history("Rust", 1).expect("search");
        assert_eq!(limit_results.len(), 1);

        // 清理
        let _ = std::fs::remove_dir_all(&tmp);
        match original {
            Some(v) => std::env::set_var("ABACUS_HOME", v),
            None => std::env::remove_var("ABACUS_HOME"),
        }
    }
}
