pub mod fallback_provider;
pub mod retry_provider;
pub mod provider;
pub mod prompt_cache;
pub mod providers;
pub mod noop_provider;
pub mod stream;
pub mod model_cache;
pub mod model_catalog;
pub mod thinking_resolver;
pub mod tool_view;
pub mod text_tool_parser;
pub mod tool_catalog;

pub use provider::*;
pub use prompt_cache::*;
pub use stream::*;
pub use model_cache::*;
pub use model_catalog::ModelCatalog;
pub use thinking_resolver::{
    ResolveOutcome, DegradeNote, validate_intent_against_caps,
    pick_nearest_supported, clamp_budget, highest_supported_effort,
};

// ─── 进程级共享 reqwest::Client（H8 修复）────────────────────────────────────
//
// 之前每个 LLM provider 实例独立 build 一个 Client，无法共享：
//   - 连接池：每个 provider 维护自己的池，HTTP/2 multiplex 失效
//   - DNS 缓存：每个 provider 独立解析
//   - TLS handshake：每个 provider 独立握手
//
// 共享 Client 后，所有 provider 共用底层连接池 + DNS 缓存 + 握手结果，
// 同时通过 `RequestBuilder.timeout(...)` 在每个请求上设置自己的超时。

use std::sync::OnceLock;

static SHARED_HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// 获取进程级共享 reqwest Client。
///
/// ## 注意
/// - 不设全局 timeout — 由每个请求的 `RequestBuilder.timeout(...)` 决定
/// - 连接池：默认每 host 32 idle，按 reqwest 默认；适配 LLM 调用模式
/// - 失败时回退到 `Client::default()`（永远不会 panic）
pub fn shared_http_client() -> &'static reqwest::Client {
    SHARED_HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .pool_max_idle_per_host(32)
            .build()
            .unwrap_or_default()
    })
}