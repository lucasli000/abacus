//! Path 1 测试加固：用 wiremock 拦截真实 HTTP，覆盖以下高风险路径——
//!
//! 1. **Gemini complete()** 真实 JSON 响应解析（thought/text 拆分、finish_reason、usage）
//! 2. **Gemini complete()** 错误路径：401/403→Unauthorized、429→RateLimited、200+error 对象
//! 3. **Gemini stream_complete()** SSE 解析（thought delta + text delta + usage）
//! 4. **DeepSeek complete()** 错误路径：401→Unauthorized、429→RateLimited
//!
//! ## 不测什么
//! - 不打真实 endpoint（无 API key）
//! - 不重复 build_request wire-format（snapshot 矩阵已锁）
//! - 不重复 parse_response 单测（lib mod 已覆盖）
//!
//! ## 引用关系
//! - 创建：cargo test 启动 #[tokio::test] runtime
//! - 消费：MockServer 与 provider 通过 base_url 钩接；测试结束 server drop 销毁
//! - 销毁：tokio runtime 关闭时 wiremock task 自动清理

use abacus_core::llm::provider::{
    LlmProvider, LlmRequest, Message, MessageContent, MessageRole,
};
use abacus_core::llm::providers::deepseek::DeepSeekProvider;
use abacus_core::llm::providers::gemini::GeminiProvider;
use abacus_types::{KernelError, ModelId};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 最小 LlmRequest 构造器。无 thinking、单条 user message。
fn req_for(model: &str) -> LlmRequest {
    LlmRequest {
        model: ModelId(model.into()),
        messages: vec![Message {
            role: MessageRole::User,
            content: Some(MessageContent::Text("hello".into())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            prefix: false,
        }],
        system: None,
        system_segments: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: Some(64),
        top_p: None,
        stop: vec![],
        stream: false,
        thinking_intent: None,
        cache_config: None,
        extra_body: Default::default(),
        user_message_preamble: None,
    }
}

// ── Gemini ────────────────────────────────────────────────────────────

/// G1 测试 #1：Gemini complete() 解析含 thought 的响应。
/// 锁定行为：thought:true 部分进 thinking 字段，其余进 message.content。
#[tokio::test]
async fn gemini_complete_parses_thought_and_text() {
    let server = MockServer::start().await;

    let resp_body = serde_json::json!({
        "candidates": [{
            "content": {
                "parts": [
                    {"text": "internal cot", "thought": true},
                    {"text": "user-visible answer"}
                ],
                "role": "model"
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 5,
            "candidatesTokenCount": 4,
            "thoughtsTokenCount": 2,
            "totalTokenCount": 11
        }
    });
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(resp_body))
        .mount(&server)
        .await;

    let provider = GeminiProvider::new(
        "test-key".into(),
        ModelId("gemini-2.5-pro".into()),
        Some(server.uri()),
    );
    let resp = provider
        .complete(req_for("gemini-2.5-pro"))
        .await
        .expect("complete should succeed");

    let content = match resp.message.content.as_ref().expect("content") {
        MessageContent::Text(t) => t.clone(),
        _ => panic!("expected text content"),
    };
    assert_eq!(content, "user-visible answer");
    assert_eq!(resp.thinking.as_deref(), Some("internal cot"));
    assert_eq!(resp.usage.prompt_tokens, 5);
    // completion = candidates + thoughts
    assert_eq!(resp.usage.completion_tokens, 6);
    assert_eq!(resp.finish_reason, "STOP");
}

/// G1 测试 #2：401 → KernelError::Unauthorized
#[tokio::test]
async fn gemini_complete_401_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("API key invalid"))
        .mount(&server)
        .await;

    let provider = GeminiProvider::with_config(
        "bad-key".into(),
        ModelId("gemini-2.5-pro".into()),
        Some(server.uri()),
        Some(5),
    );
    let err = provider
        .complete(req_for("gemini-2.5-pro"))
        .await
        .expect_err("should fail");
    assert!(
        matches!(err, KernelError::Unauthorized(_)),
        "401 应映射 Unauthorized，实际：{err:?}"
    );
}

/// G1 测试 #3：403 PERMISSION_DENIED → 也归为 Unauthorized（Google 常返 403）
#[tokio::test]
async fn gemini_complete_403_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_string("PERMISSION_DENIED"))
        .mount(&server)
        .await;

    let provider = GeminiProvider::with_config(
        "key".into(),
        ModelId("gemini-2.5-pro".into()),
        Some(server.uri()),
        Some(5),
    );
    let err = provider
        .complete(req_for("gemini-2.5-pro"))
        .await
        .expect_err("403 should fail");
    assert!(
        matches!(err, KernelError::Unauthorized(_)),
        "403 应归 Unauthorized：{err:?}"
    );
}

/// G1 测试 #4：429 + retry-after → KernelError::RateLimited{retry_after}
/// wiremock 默认会按 mount 顺序匹配；这里所有请求都返回 429，验证重试 2 次后仍失败。
#[tokio::test]
async fn gemini_complete_429_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "42")
                .set_body_string("rate limit"),
        )
        .mount(&server)
        .await;

    let provider = GeminiProvider::with_config(
        "key".into(),
        ModelId("gemini-2.5-pro".into()),
        Some(server.uri()),
        Some(5),
    );
    let err = provider
        .complete(req_for("gemini-2.5-pro"))
        .await
        .expect_err("429 应失败");
    match err {
        KernelError::RateLimited { retry_after } => {
            assert_eq!(retry_after, 42, "应解析 retry-after header");
        }
        other => panic!("应得 RateLimited，实际 {other:?}"),
    }
}

/// G1 测试 #5：200 + error 对象（Google 偶尔以 200 包错误体）→ ApiError
#[tokio::test]
async fn gemini_complete_200_with_error_object() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "error": { "code": 400, "message": "INVALID_ARGUMENT: bad model", "status": "INVALID_ARGUMENT" }
    });
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let provider = GeminiProvider::new(
        "key".into(),
        ModelId("gemini-2.5-pro".into()),
        Some(server.uri()),
    );
    let err = provider
        .complete(req_for("gemini-2.5-pro"))
        .await
        .expect_err("200+error 应失败");
    match err {
        KernelError::ApiError { status, body } => {
            assert_eq!(status, 400, "应从 error.code 取状态码");
            assert!(body.contains("INVALID_ARGUMENT"));
        }
        other => panic!("应得 ApiError，实际 {other:?}"),
    }
}

/// G1 测试 #6：streamGenerateContent SSE 解析
/// 模拟 Gemini SSE：每个 event 是 `data: {完整 GeminiResponse 增量}\n\n`。
/// 验证 thought:true delta 走 thinking，普通 text 走 content。
#[tokio::test]
async fn gemini_stream_emits_thought_and_text_deltas() {
    let server = MockServer::start().await;

    // 三段 SSE：thought delta、text delta、usage chunk
    let sse_body = "\
data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"reasoning A\",\"thought\":true}]}}]}\n\n\
data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"answer B\"}]}}]}\n\n\
data: {\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2,\"thoughtsTokenCount\":4,\"totalTokenCount\":9}}\n\n\
";
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-flash:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&server)
        .await;

    let provider = GeminiProvider::new(
        "key".into(),
        ModelId("gemini-2.5-flash".into()),
        Some(server.uri()),
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let resp = provider
        .stream_complete(req_for("gemini-2.5-flash"), tx)
        .await
        .expect("stream should succeed");

    // 聚合断言
    let content = match resp.message.content.as_ref().unwrap() {
        MessageContent::Text(t) => t.clone(),
        _ => panic!("expected text content"),
    };
    assert_eq!(content, "answer B");
    assert_eq!(resp.thinking.as_deref(), Some("reasoning A"));
    assert_eq!(resp.usage.prompt_tokens, 3);
    assert_eq!(resp.usage.completion_tokens, 6); // candidates + thoughts

    // 流事件断言
    use abacus_core::llm::stream::StreamEvent;
    let mut saw_thinking = false;
    let mut saw_text = false;
    let mut saw_done = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            StreamEvent::ThinkingDelta(s) => {
                assert!(s.contains("reasoning A"));
                saw_thinking = true;
            }
            StreamEvent::TextDelta(s) => {
                assert!(s.contains("answer B"));
                saw_text = true;
            }
            StreamEvent::Done => saw_done = true,
            _ => {}
        }
    }
    assert!(saw_thinking, "应至少推送一次 ThinkingDelta");
    assert!(saw_text, "应至少推送一次 TextDelta");
    assert!(saw_done, "流尾应推送 Done");
}

/// G1 测试 #7：stream_complete 上 401 直接返回 Unauthorized（不进 SSE 解析）
#[tokio::test]
async fn gemini_stream_401_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;

    let provider = GeminiProvider::with_config(
        "key".into(),
        ModelId("gemini-2.5-pro".into()),
        Some(server.uri()),
        Some(5),
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let err = provider
        .stream_complete(req_for("gemini-2.5-pro"), tx)
        .await
        .expect_err("401 应失败");
    assert!(matches!(err, KernelError::Unauthorized(_)));
}

// ── DeepSeek ──────────────────────────────────────────────────────────

/// DS 测试 #1：401 → Unauthorized
#[tokio::test]
async fn deepseek_complete_401_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid api key"))
        .mount(&server)
        .await;

    let provider = DeepSeekProvider::with_config(
        "bad".into(),
        ModelId("deepseek-v4-flash".into()),
        Some(server.uri()),
        None,
        Some(5),
    );
    let err = provider
        .complete(req_for("deepseek-v4-flash"))
        .await
        .expect_err("401 应失败");
    assert!(
        matches!(err, KernelError::Unauthorized(_)),
        "实际 {err:?}"
    );
}

/// DS 测试 #2：429 + retry-after → RateLimited
#[tokio::test]
async fn deepseek_complete_429_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "17")
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;

    let provider = DeepSeekProvider::with_config(
        "key".into(),
        ModelId("deepseek-v4-flash".into()),
        Some(server.uri()),
        None,
        Some(5),
    );
    let err = provider
        .complete(req_for("deepseek-v4-flash"))
        .await
        .expect_err("429 应失败");
    match err {
        KernelError::RateLimited { retry_after } => assert_eq!(retry_after, 17),
        other => panic!("应得 RateLimited，实际 {other:?}"),
    }
}

/// DS 测试 #3：成功 200 + reasoning_content 解析
#[tokio::test]
async fn deepseek_complete_parses_reasoning_content() {
    let server = MockServer::start().await;
    let resp_body = serde_json::json!({
        "id": "abc",
        "model": "deepseek-v4-flash",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "final answer",
                "reasoning_content": "step-by-step reasoning"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 8,
            "completion_tokens": 4,
            "total_tokens": 12
        }
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(resp_body))
        .mount(&server)
        .await;

    let provider = DeepSeekProvider::with_config(
        "key".into(),
        ModelId("deepseek-v4-flash".into()),
        Some(server.uri()),
        None,
        Some(5),
    );
    // 显式开 thinking，让 build_request 走 reasoning model 全路径
    let mut r = req_for("deepseek-v4-flash");
    r.thinking_intent = Some(abacus_types::ThinkingIntent::Effort(
        abacus_types::EffortLevel::High,
    ));
    let resp = provider.complete(r).await.expect("应成功");
    let content = match resp.message.content.as_ref().unwrap() {
        MessageContent::Text(t) => t.clone(),
        _ => panic!("expected text"),
    };
    assert_eq!(content, "final answer");
    assert_eq!(
        resp.message.reasoning_content.as_deref(),
        Some("step-by-step reasoning"),
        "reasoning_content 必须保留——下一轮 build_messages 需要它"
    );
}
