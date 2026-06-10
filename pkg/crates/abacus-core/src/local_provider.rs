//! 本地模型服务自动发现与健康探测
//!
//! ## 职责
//! - **自动发现**：扫描常见端口，检测 Ollama / vLLM / MLX / Generic OpenAI-compatible 服务
//! - **健康探测**：轻量 HTTP probe 验证 embedding / reranker 端点是否可达
//! - **配置合并**：自动发现结果与用户手动配置 `[local]` 段合并（手动配置优先）
//!
//! ## 探测策略
//! | 类型 | 默认 URL | 探测端点 | 成功标识 |
//! |---|---|---|---|
//! | Ollama | `http://127.0.0.1:11434` | `GET /api/tags` | HTTP 200 + JSON `models` 数组 |
//! | vLLM | `http://127.0.0.1:8000` | `GET /v1/models` | HTTP 200 + JSON `data` 数组 |
//! | Generic | `http://127.0.0.1:8001` | `GET /v1/models` | HTTP 200 + JSON `data` 数组 |
//!
//! ## 健康检查
//! - Embedding: `GET {url}/v1/models`（轻量，不耗 token）
//! - Reranker: 同上，或尝试一次 dummy rerank

use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;

/// 本地模型服务类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalProviderType {
    Ollama,
    Vllm,
    Mlx,
    Generic,
}

impl LocalProviderType {
    pub fn label(&self) -> &'static str {
        match self {
            LocalProviderType::Ollama => "Ollama",
            LocalProviderType::Vllm => "vLLM",
            LocalProviderType::Mlx => "MLX",
            LocalProviderType::Generic => "Local",
        }
    }
}

/// 发现的服务端点
#[derive(Debug, Clone)]
pub struct LocalEndpoint {
    pub provider: LocalProviderType,
    pub base_url: String,
    /// 服务端报告的可用模型列表（可能为空）
    pub models: Vec<String>,
}

/// 本地模型健康状态（供 TUI 面板展示）
///
/// V42-B: 从 `MlxHealth` 重命名为 `LocalModelHealth`，支持 Ollama/vLLM/MLX 等多种本地后端。
#[derive(Debug, Clone, Default)]
pub struct LocalModelHealth {
    pub provider_type: String,
    pub embedding_running: bool,
    pub embedding_model: String,
    pub reranker_running: bool,
    pub reranker_model: String,
    pub knowledge_chunks: u32,
    pub embeddings_cached: u32,
}

/// 本地模型服务探测器
pub struct LocalProviderDetector {
    client: Client,
    timeout: Duration,
}

impl Default for LocalProviderDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalProviderDetector {
    /// 创建探测器（使用轻量 client，无连接池共享需求）
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("reqwest client build");
        Self {
            client,
            timeout: Duration::from_secs(3),
        }
    }

    /// 自动发现所有本地服务
    ///
    /// 并行探测候选 URL，返回所有响应成功的端点。
    /// 调用方可根据 `provider` 字段区分服务类型。
    pub async fn discover(&self) -> Vec<LocalEndpoint> {
        let candidates = [
            (LocalProviderType::Ollama, "http://127.0.0.1:11434"),
            (LocalProviderType::Vllm, "http://127.0.0.1:8000"),
            (LocalProviderType::Vllm, "http://127.0.0.1:8001"),
            (LocalProviderType::Vllm, "http://127.0.0.1:8002"),
            (LocalProviderType::Generic, "http://127.0.0.1:8080"),
        ];

        let mut endpoints = Vec::new();
        for (provider, url) in candidates {
            if let Some(ep) = self.probe(provider, url).await {
                endpoints.push(ep);
            }
        }
        endpoints
    }

    /// 探测单个 URL，返回端点信息或 None
    async fn probe(&self, provider: LocalProviderType, url: &str) -> Option<LocalEndpoint> {
        match provider {
            LocalProviderType::Ollama => self.probe_ollama(url).await,
            _ => self.probe_openai_compatible(provider, url).await,
        }
    }

    /// Ollama 探测：`/api/tags` 返回模型列表
    async fn probe_ollama(&self, url: &str) -> Option<LocalEndpoint> {
        let probe_url = format!("{}/api/tags", url.trim_end_matches('/'));
        let resp = match tokio::time::timeout(
            self.timeout,
            self.client.get(&probe_url).send(),
        )
        .await
        {
            Ok(Ok(r)) => r,
            _ => return None,
        };

        if !resp.status().is_success() {
            return None;
        }

        #[derive(Deserialize)]
        struct OllamaTags {
            models: Vec<OllamaModel>,
        }
        #[derive(Deserialize)]
        struct OllamaModel {
            name: String,
        }

        let body: OllamaTags = resp.json().await.ok()?;
        let models: Vec<String> = body.models.into_iter().map(|m| m.name).collect();
        if models.is_empty() {
            return None;
        }
        Some(LocalEndpoint {
            provider: LocalProviderType::Ollama,
            base_url: url.to_string(),
            models,
        })
    }

    /// OpenAI-compatible 探测：`/v1/models` 返回模型列表
    async fn probe_openai_compatible(
        &self,
        provider: LocalProviderType,
        url: &str,
    ) -> Option<LocalEndpoint> {
        let probe_url = format!("{}/v1/models", url.trim_end_matches('/'));
        let resp = match tokio::time::timeout(
            self.timeout,
            self.client.get(&probe_url).send(),
        )
        .await
        {
            Ok(Ok(r)) => r,
            _ => return None,
        };

        if !resp.status().is_success() {
            return None;
        }

        #[derive(Deserialize)]
        struct ModelList {
            data: Vec<ModelItem>,
        }
        #[derive(Deserialize)]
        struct ModelItem {
            id: String,
        }

        let body: ModelList = resp.json().await.ok()?;
        let models: Vec<String> = body.data.into_iter().map(|m| m.id).collect();
        if models.is_empty() {
            return None;
        }
        Some(LocalEndpoint {
            provider,
            base_url: url.to_string(),
            models,
        })
    }

    /// 从发现结果中推断 embedding 配置
    ///
    /// 启发式：
    /// - 优先选含 "embed" 的模型名
    /// - 其次选列表第一个
    pub fn guess_embedding_config(endpoints: &[LocalEndpoint]) -> Option<(String, String, usize)> {
        for ep in endpoints {
            let model = ep
                .models
                .iter()
                .find(|m| m.to_lowercase().contains("embed"))
                .or(ep.models.first())?;
            // 维度默认 1024（常见值），用户可通过 config 覆盖
            return Some((ep.base_url.clone(), model.clone(), 1024));
        }
        None
    }

    /// 从发现结果中推断 reranker 配置
    pub fn guess_reranker_config(endpoints: &[LocalEndpoint]) -> Option<(String, String)> {
        for ep in endpoints {
            let model = ep
                .models
                .iter()
                .find(|m| m.to_lowercase().contains("rank"))
                .or(ep.models.first())?;
            return Some((ep.base_url.clone(), model.clone()));
        }
        None
    }
}

/// 对 embedding 服务端点做轻量健康检查
///
/// 策略：`GET /v1/models`（不消耗 token，只验证 HTTP 可达）
pub async fn health_check_embedding(url: &str) -> bool {
    let client = match Client::builder().timeout(Duration::from_secs(3)).build() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let probe = format!("{}/v1/models", url.trim_end_matches('/'));
    match tokio::time::timeout(Duration::from_secs(3), client.get(&probe).send()).await {
        Ok(Ok(resp)) => resp.status().is_success(),
        _ => false,
    }
}

/// 对 reranker 服务端点做轻量健康检查
pub async fn health_check_reranker(url: &str) -> bool {
    // reranker 通常共享同一 vLLM 实例或独立实例，probe 策略与 embedding 相同
    health_check_embedding(url).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_construction() {
        let _d = LocalProviderDetector::new();
    }

    #[test]
    fn provider_type_label() {
        assert_eq!(LocalProviderType::Ollama.label(), "Ollama");
        assert_eq!(LocalProviderType::Vllm.label(), "vLLM");
    }

    #[test]
    fn guess_embedding_prefers_embed_in_name() {
        let eps = vec![LocalEndpoint {
            provider: LocalProviderType::Vllm,
            base_url: "http://127.0.0.1:8001".into(),
            models: vec![
                "Qwen3-0.6B".into(),
                "Qwen3-Embedding-0.6B".into(),
            ],
        }];
        let (url, model, dim) = LocalProviderDetector::guess_embedding_config(&eps).unwrap();
        assert_eq!(url, "http://127.0.0.1:8001");
        assert_eq!(model, "Qwen3-Embedding-0.6B");
        assert_eq!(dim, 1024);
    }

    #[test]
    fn guess_reranker_prefers_rank_in_name() {
        let eps = vec![LocalEndpoint {
            provider: LocalProviderType::Vllm,
            base_url: "http://127.0.0.1:8002".into(),
            models: vec![
                "other-model".into(),
                "Qwen3-Reranker-0.6B".into(),
            ],
        }];
        let (url, model) = LocalProviderDetector::guess_reranker_config(&eps).unwrap();
        assert_eq!(model, "Qwen3-Reranker-0.6B");
    }
}
