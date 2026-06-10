//! vLLM HTTP Embedding/Reranker 客户端
//!
//! 实现 `MemoryEmbedder` trait，通过 HTTP 调用本地 vLLM 服务。
//! - Embedding: 标准 OpenAI `/v1/embeddings` 接口
//! - Reranker: Cohere-style `/v1/rerank` 接口
//!
//! ## 配置
//! 在 `config.toml` 中添加：
//! ```toml
//! [local]
//! embedding_url = "http://127.0.0.1:8001"
//! embedding_model = "/path/to/Qwen3-Embedding-0.6B"
//! embedding_dim = 1024
//! reranker_url = "http://127.0.0.1:8002"
//! reranker_model = "models/Qwen3-Reranker-0.6B"
//! ```
//!
//! ## 长连接
//! 使用 `reqwest::Client` 内置连接池（HTTP/1.1 keep-alive），同一 Client 实例
//! 复用 TCP 连接，无需额外配置。

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::triage::{TriageBlock, TriageReranker};
use crate::memory_palace::MemoryEmbedder;

// ─── 请求/响应结构体 ────────────────────────────────────────────────────

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    input: &'a str,
    model: &'a str,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    #[allow(dead_code)]
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Serialize)]
struct RerankRequest<'a> {
    query: &'a str,
    documents: &'a [&'a str],
    model: &'a str,
    top_n: usize,
}

#[derive(Deserialize)]
struct RerankResponse {
    results: Vec<RerankResult>,
}

#[derive(Deserialize)]
struct RerankResult {
    index: usize,
    relevance_score: f64,
}

// ─── VllmEmbedder ──────────────────────────────────────────────────────

/// 基于 vLLM HTTP API 的 embedding 服务
///
/// 使用标准 OpenAI `/v1/embeddings` 接口，支持连接池复用。
pub struct VllmEmbedder {
    client: Client,
    base_url: String,
    model: String,
    dim: usize,
}

impl VllmEmbedder {
    /// 创建新的 VllmEmbedder
    ///
    /// - `base_url`: vLLM 服务地址，如 `http://127.0.0.1:8001`
    /// - `model`: 模型名称/路径
    /// - `dim`: embedding 向量维度
    pub fn new(base_url: &str, model: &str, dim: usize) -> Self {
        let client = Client::builder()
            .pool_max_idle_per_host(4)
            .tcp_keepalive(std::time::Duration::from_secs(60))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            dim,
        }
    }

    /// 使用共享 HttpClient 创建（用于多个 embedder 实例复用连接池）
    pub fn with_client(client: Client, base_url: &str, model: &str, dim: usize) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            dim,
        }
    }

    /// 轻量健康检查：验证服务端 `/v1/models` 可达
    ///
    /// 不消耗 embedding token，仅做 HTTP 探测。
    pub async fn health_check(&self) -> bool {
        crate::local_provider::health_check_embedding(&self.base_url).await
    }
}

#[async_trait]
impl MemoryEmbedder for VllmEmbedder {
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, String> {
        let url = format!("{}/v1/embeddings", self.base_url);
        let body = EmbeddingRequest {
            input: text,
            model: &self.model,
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("VllmEmbedder HTTP error: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "VllmEmbedder API error {status}: {body_text}"
            ));
        }

        let parsed: EmbeddingResponse = resp
            .json()
            .await
            .map_err(|e| format!("VllmEmbedder JSON parse error: {e}"))?;

        parsed
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| "VllmEmbedder: empty embedding response".to_string())
    }

    async fn batch_embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        // vLLM 支持批量 input
        let url = format!("{}/v1/embeddings", self.base_url);
        let body = serde_json::json!({
            "input": texts,
            "model": self.model,
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("VllmEmbedder batch HTTP error: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "VllmEmbedder batch API error {status}: {body_text}"
            ));
        }

        let parsed: EmbeddingResponse = resp
            .json()
            .await
            .map_err(|e| format!("VllmEmbedder batch JSON parse error: {e}"))?;

        // 按 index 排序（vLLM 返回的 data 按 input 顺序）
        let mut results: Vec<(usize, Vec<f32>)> = parsed
            .data
            .into_iter()
            .map(|d| (d.index, d.embedding))
            .collect();
        results.sort_by_key(|(i, _)| *i);

        Ok(results.into_iter().map(|(_, v)| v).collect())
    }

    fn dimension(&self) -> usize {
        self.dim
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

// ─── VllmReranker ──────────────────────────────────────────────────────

/// 基于 vLLM HTTP API 的 reranker 服务
///
/// 使用 Cohere-style `/v1/rerank` 接口。
pub struct VllmReranker {
    client: Client,
    base_url: String,
    model: String,
}

/// Rerank 结果项
#[derive(Debug, Clone)]
pub struct RerankItem {
    pub index: usize,
    pub score: f64,
}

impl VllmReranker {
    pub fn new(base_url: &str, model: &str) -> Self {
        let client = Client::builder()
            .pool_max_idle_per_host(4)
            .tcp_keepalive(std::time::Duration::from_secs(60))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
        }
    }

    pub fn with_client(client: Client, base_url: &str, model: &str) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
        }
    }

    /// 轻量健康检查
    pub async fn health_check(&self) -> bool {
        crate::local_provider::health_check_reranker(&self.base_url).await
    }

    /// 对 documents 按 query 相关性重排序，返回 top_n 结果
    pub async fn rerank(
        &self,
        query: &str,
        documents: &[&str],
        top_n: usize,
    ) -> Result<Vec<RerankItem>, String> {
        let url = format!("{}/v1/rerank", self.base_url);
        let body = RerankRequest {
            query,
            documents,
            model: &self.model,
            top_n,
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("VllmReranker HTTP error: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "VllmReranker API error {status}: {body_text}"
            ));
        }

        let parsed: RerankResponse = resp
            .json()
            .await
            .map_err(|e| format!("VllmReranker JSON parse error: {e}"))?;

        Ok(parsed
            .results
            .into_iter()
            .map(|r| RerankItem {
                index: r.index,
                score: r.relevance_score,
            })
            .collect())
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }
}

#[async_trait]
impl TriageReranker for VllmReranker {
    async fn rerank(&self, blocks: &[TriageBlock], query: &str) -> Vec<TriageBlock> {
        if blocks.is_empty() || query.is_empty() {
            return blocks.to_vec();
        }
        let docs: Vec<String> = blocks.iter()
            .map(|b| {
                let text = crate::core::triage::extract_text(&b.messages);
                if text.len() > 512 { text[..512].to_string() } else { text }
            })
            .collect();
        let doc_refs: Vec<&str> = docs.iter().map(|s| s.as_str()).collect();
        let top_n = doc_refs.len();
        match self.rerank(query, &doc_refs, top_n).await {
            Ok(items) => {
                let mut result: Vec<TriageBlock> = items.iter()
                    .filter_map(|item| blocks.get(item.index).cloned())
                    .collect();
                // 补全缺失的 block（reranker 可能返回少于 top_n）
                for block in blocks {
                    if !result.iter().any(|r| r.block_id == block.block_id) {
                        result.push(block.clone());
                    }
                }
                result
            }
            Err(_) => blocks.to_vec(),
        }
    }
}

// ─── 工厂函数 ─────────────────────────────────────────────────────────

/// 从配置创建 VllmEmbedder（读取 `[local]` 配置段）
///
/// 返回 `None` 当 `embedding_url` 未配置时。
pub fn create_embedder_from_config(
    embedding_url: &str,
    embedding_model: &str,
    embedding_dim: usize,
) -> Arc<VllmEmbedder> {
    Arc::new(VllmEmbedder::new(embedding_url, embedding_model, embedding_dim))
}

/// 从配置创建 VllmReranker
pub fn create_reranker_from_config(
    reranker_url: &str,
    reranker_model: &str,
) -> Arc<VllmReranker> {
    Arc::new(VllmReranker::new(reranker_url, reranker_model))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vllm_embedder_construction() {
        let e = VllmEmbedder::new("http://127.0.0.1:8001", "test-model", 1024);
        assert_eq!(e.dimension(), 1024);
        assert_eq!(e.model_name(), "test-model");
        assert_eq!(e.base_url, "http://127.0.0.1:8001");
    }

    #[test]
    fn test_url_trailing_slash() {
        let e = VllmEmbedder::new("http://127.0.0.1:8001/", "m", 768);
        assert_eq!(e.base_url, "http://127.0.0.1:8001");
    }

    #[test]
    fn test_reranker_construction() {
        let r = VllmReranker::new("http://127.0.0.1:8002", "reranker-model");
        assert_eq!(r.model_name(), "reranker-model");
    }
}
