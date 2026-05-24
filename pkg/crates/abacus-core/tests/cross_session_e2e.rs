//! cross-session 段 I：端到端集成测试
//!
//! ## 设计动机
//! 段 A~H 各有 unit test 锁本组件正确性，但"组件间协同"是另一层契约——
//! 段 B 的 jsonl 写入能被段 E 的 replay 完整读回吗？段 G 的 rotation 后段 E 还能跨归档读全吗？
//! 段 H 的 SessionResumeReport 聚合的数字与原始 events 对得上吗？这层契约只有跑 e2e 才能锁。
//!
//! ## 不模拟 LLM
//! 选择手动 emit `PipelineEvent` 而非启动 CoreLoop+Mock LLM——理由：
//! - LLM 路径已被 provider_mock_runtime.rs 覆盖；e2e 重复无价值
//! - CoreLoop 启动会 register 几十个 hook，把 e2e 复杂度推高，掩盖跨 session 链路本身的 bug
//! - 跨 session 的"链路"是写盘+读盘，与 LLM 解耦，更纯粹
//!
//! ## 引用关系
//! - 创建：cargo test 启动 #[tokio::test] runtime
//! - 消费：JsonlEventHook 写盘；replay/build_resume_report 读盘
//! - 销毁：每用例自带 isolated_project_dir，测试结束后 remove_dir_all
//!
//! ## 三段验证
//! 1. **多 session 隔离**：两个 session 写同一 project 不串
//! 2. **重启-续写-发现**：进程重启后 append 模式 + list_replayable 能枚举旧 session
//! 3. **rotation+replay 完整**：超阈值后归档，replay 仍能读完整序列

use abacus_core::core::event_sink::{
    build_resume_report, list_replayable_sessions, replay_session_events,
    JsonlEventHook,
};
use abacus_core::mag_chain::PipelineEvent;
use abacus_core::mag_chain::PipelineHook;
use std::path::PathBuf;

/// 隔离的 project_dir——避免 e2e 用例间互踩
///
/// 用 atomic counter + nanos + pid 三重防碰撞（与 event_sink::tests 内部模式一致）
fn isolated_project_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "abacus_xs_e2e_{}_{}_{}",
        std::process::id(),
        nanos,
        n
    ));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 1 个完整 turn 的事件三连——TurnStart → TurnPostFanOut → TurnEnd
async fn emit_full_turn(
    hook: &JsonlEventHook,
    session_id: &str,
    turn_no: u32,
    tool_calls: usize,
) {
    hook.on_event(&PipelineEvent::TurnStart {
        input: format!("turn {} input", turn_no),
        session_id: session_id.into(),
    })
    .await
    .unwrap();
    hook.on_event(&PipelineEvent::TurnPostFanOut {
        turn_number: turn_no,
        session_id: session_id.into(),
        tool_calls,
        all_success: true,
        was_compressed: false,
    })
    .await
    .unwrap();
    hook.on_event(&PipelineEvent::TurnEnd {
        response_len: 100,
        tool_calls,
        latency_ms: 200,
        completion_tokens: 50,
    })
    .await
    .unwrap();
}

/// E2E #1：多 session 写同一 project，文件按 session 隔离 + replay 不串
///
/// 验证：sessions/ 下每个 session 独立 jsonl，replay session A 不会拿到 session B 的事件
#[tokio::test]
async fn multi_session_isolation_and_replay() {
    let proj = isolated_project_dir();

    // 起两个 session，各跑 2 个 turn
    {
        let hook_a = JsonlEventHook::open("sess_alpha", &proj).expect("open A");
        let hook_b = JsonlEventHook::open("sess_beta", &proj).expect("open B");
        emit_full_turn(&hook_a, "sess_alpha", 1, 2).await;
        emit_full_turn(&hook_b, "sess_beta", 1, 5).await;
        emit_full_turn(&hook_a, "sess_alpha", 2, 1).await;
        emit_full_turn(&hook_b, "sess_beta", 2, 3).await;
        // hook_a / hook_b drop → 文件 fsync 落盘
    }

    // list 应见到两个 session
    let sessions = list_replayable_sessions(&proj).expect("list");
    assert_eq!(sessions.len(), 2, "应发现 2 个 session, got: {sessions:?}");
    assert!(sessions.contains(&"sess_alpha".into()));
    assert!(sessions.contains(&"sess_beta".into()));

    // replay alpha 应严格 6 行（2 turn × 3 event），无 beta 串扰
    let alpha_events = replay_session_events("sess_alpha", &proj).expect("replay A");
    assert_eq!(alpha_events.len(), 6, "alpha 应 6 events");
    assert!(
        alpha_events.iter().all(|e| e.session_id == "sess_alpha"
            || e.data.get("session_id").map(|v| v.as_str()) == Some(Some("sess_alpha"))),
        "alpha replay 不应混入 beta"
    );

    // 段 H 聚合数字与原始一致
    let alpha_report = build_resume_report("sess_alpha", &proj).expect("report A");
    assert_eq!(alpha_report.event_count, 6);
    assert_eq!(alpha_report.turn_count, 2);
    assert_eq!(alpha_report.total_tool_calls, 3, "1+2 tool calls 跨 2 turn");

    let beta_report = build_resume_report("sess_beta", &proj).expect("report B");
    assert_eq!(beta_report.event_count, 6);
    assert_eq!(beta_report.turn_count, 2);
    assert_eq!(beta_report.total_tool_calls, 8, "5+3 tool calls 跨 2 turn");

    let _ = std::fs::remove_dir_all(&proj);
}

/// E2E #2：进程"重启"——重开同 session append + 跨开关枚举
///
/// 验证：模拟 abacus 进程结束→重启同 session_id，新事件 append 到旧文件，
/// build_resume_report 应聚合"重启前+重启后"全部事件
#[tokio::test]
async fn restart_append_and_aggregate_full_history() {
    let proj = isolated_project_dir();
    let sid = "restart_test";

    // 进程 1：写 1 个 turn
    {
        let hook = JsonlEventHook::open(sid, &proj).expect("open #1");
        emit_full_turn(&hook, sid, 1, 4).await;
        // hook drop
    }

    // 进程"重启"：重开同 session，写 2 个 turn
    {
        let hook = JsonlEventHook::open(sid, &proj).expect("open #2 (restart)");
        emit_full_turn(&hook, sid, 2, 1).await;
        emit_full_turn(&hook, sid, 3, 7).await;
    }

    // 验证：跨进程总事件数 = 9 (3 turn × 3 events)
    let report = build_resume_report(sid, &proj).expect("report");
    assert_eq!(report.event_count, 9, "重启后应聚合全部 9 events");
    assert_eq!(report.turn_count, 3);
    assert_eq!(report.total_tool_calls, 12, "4+1+7 跨重启");

    // 验证 first/last_event_ms 跨重启不丢失
    assert!(report.first_event_ms.is_some());
    assert!(report.last_event_ms.is_some());
    assert!(
        report.last_event_ms.unwrap() >= report.first_event_ms.unwrap(),
        "last_ts >= first_ts"
    );

    // replay 顺序应严格 turn 1 → 2 → 3（按 ts_ms 单调）
    let events = replay_session_events(sid, &proj).expect("replay");
    assert_eq!(events.len(), 9);
    let post_fanout_turns: Vec<_> = events
        .iter()
        .filter(|e| e.event == "TurnPostFanOut")
        .map(|e| e.turn_number().unwrap())
        .collect();
    assert_eq!(
        post_fanout_turns,
        vec![1, 2, 3],
        "TurnPostFanOut 严格递增"
    );

    let _ = std::fs::remove_dir_all(&proj);
}

/// E2E #3：rotation 后 replay 跨归档完整
///
/// 验证：同 session 写超阈值触发 rotate（段 G）后，replay（段 E）能读全部包括归档
#[tokio::test]
async fn rotation_then_replay_spans_archives() {
    let proj = isolated_project_dir();
    let sid = "rotate_e2e";

    // 用最小阈值（1 byte）强制每条 event 都触发 rotate
    let hook = JsonlEventHook::open_with_rotation(sid, &proj, 1).expect("open");
    // 写 5 个 turn → 15 events，每条事件后必定触发 rotate（除非紧贴写入未达阈值）
    for i in 1..=5u32 {
        emit_full_turn(&hook, sid, i, i as usize).await;
    }
    // 显式 drop hook 释放句柄，避免 macOS unlink-while-open 行为差异
    drop(hook);

    // sessions/ 下应有归档文件 + 当前 active 文件
    let sessions_dir = proj.join("sessions");
    let entries: Vec<_> = std::fs::read_dir(&sessions_dir)
        .expect("read sessions dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().any(|n| n.starts_with(sid) && n.contains('.') && !n.ends_with(".tmp")),
        "应见到归档文件 + 主文件; entries: {entries:?}"
    );

    // replay 应读全 15 events 跨所有归档
    let events = replay_session_events(sid, &proj).expect("replay across archives");
    assert_eq!(
        events.len(),
        15,
        "5 turn × 3 events = 15，replay 跨归档应完整"
    );

    // 段 H 聚合也应正确
    let report = build_resume_report(sid, &proj).expect("report");
    assert_eq!(report.event_count, 15);
    assert_eq!(report.turn_count, 5);
    assert_eq!(report.total_tool_calls, 1 + 2 + 3 + 4 + 5, "5+4+3+2+1=15");

    let _ = std::fs::remove_dir_all(&proj);
}

/// E2E #4：未知 session_id 全链路降级
///
/// 验证：即使误传不存在的 session_id，全链路 graceful 不 panic 不串数据
/// 设计选择：replay 返回 Ok(empty)、build_resume_report 返回 Ok(empty report)
/// 而非 Err——因为"该 session 无历史"是合法状态，由调用方自行检测 event_count==0
#[tokio::test]
async fn unknown_session_graceful_degradation() {
    let proj = isolated_project_dir();

    // replay 未知 session → 空 vec（已被 segment E 测过；此处验全链路一致）
    let events = replay_session_events("does_not_exist", &proj).expect("graceful");
    assert!(events.is_empty(), "未知 session replay 应空");

    // build_resume_report 未知 session → Ok(empty)；调用方用 event_count==0 判定
    let report = build_resume_report("does_not_exist", &proj).expect("graceful");
    assert_eq!(report.event_count, 0, "未知 session 应得空 report");
    assert_eq!(report.turn_count, 0);
    assert_eq!(report.total_tool_calls, 0);
    assert!(!report.had_compression);
    assert_eq!(report.duration_ms(), 0);
    assert!(report.first_event_ms.is_none());
    assert!(report.last_event_ms.is_none());

    // list_replayable_sessions 在空 project 应返回空（不 panic）
    let sessions = list_replayable_sessions(&proj).expect("list graceful");
    assert!(sessions.is_empty());

    let _ = std::fs::remove_dir_all(&proj);
}
