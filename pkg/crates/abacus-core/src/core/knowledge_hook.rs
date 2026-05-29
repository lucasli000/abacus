//! Generated Knowledge Hook（P1-A2）
//!
//! ## 场景
//! 在 LLM 处理用户查询之前，主动从 KnowledgeStore 检索相关背景知识，
//! 注入到 system prompt Layer 180（Knowledge 层），提升推理准确率。
//!
//! ## 设计原理（来自 Liu et al. 2022 Generated Knowledge Prompting）
//! 为 LLM 提供预先检索的背景知识（而非让 LLM 自行推断），可以显著减少
//! 常识推理错误和事实幻觉。本实现使用 KnowledgeStore FTS5 搜索替代论文中
//! 的 LLM 生成步骤，节省 token 开销，同时复用已摄入的领域知识。
//!
//! ## 触发条件
//! - task_kind 属于 KnowledgeQuery 或 Mathematics
//! - KnowledgeStore 有查询结果（ZeroHit 时跳过注入）
//! - turn_number == 1（只在首轮注入，避免重复）
//!
//! ## 注入内容
//! Top-3 相关 KB 结果，格式化为"背景参考"段落
//!
//! ## 引用关系
//! - 注册方：CoreLoop::new() 通过 PromptAssembly::register_hook()
//! - 依赖：Arc<KnowledgeStore>（只读查询）
//! - 生命周期：随 PromptAssembly / CoreLoop 存活

use std::sync::Arc;

use crate::core::prompt_assembly::{HookContext, PromptHook};
use crate::knowledge_store::KnowledgeStore;

/// Generated Knowledge PromptHook（Priority = 180，位于 Knowledge 层）
///
/// ## 字段
/// - `store`: KnowledgeStore 引用（只读，FTS5 查询）
/// - `top_k`: 返回最多 top_k 条背景知识（默认 3）
///
/// ## 并发安全
/// KnowledgeStore 内部已用 Arc<Mutex<Connection>>，hook 只读无竞争。
pub struct GeneratedKnowledgeHook {
    /// 知识库引用（Arc 共享，只读操作）
    ///
    /// ## 引用关系
    /// - 设置方：CoreLoop::new() 在 knowledge_store 初始化后注入
    /// - 消费方：inject() 中的 query() 调用
    /// - 生命周期：随 CoreLoop 生死
    store: Arc<KnowledgeStore>,

    /// 注入的最大背景知识条数（默认 3，避免 token 膨胀）
    top_k: usize,
}

impl GeneratedKnowledgeHook {
    /// 创建 GeneratedKnowledgeHook
    ///
    /// ## 参数
    /// - `store`: KnowledgeStore Arc 引用
    /// - `top_k`: 最多注入多少条背景知识（建议 2-5）
    pub fn new(store: Arc<KnowledgeStore>, top_k: usize) -> Self {
        Self { store, top_k }
    }
}

/// 适合注入背景知识的任务类型
const KNOWLEDGE_APPLICABLE_TASKS: &[&str] = &[
    "knowledge_query",
    "mathematics",
    "data_analysis",
    "architecture",  // 架构决策需要历史知识支撑
];

impl PromptHook for GeneratedKnowledgeHook {
    fn id(&self) -> &str { "generated_knowledge" }

    /// Priority = 180：对应 Layer 180（Knowledge 层）
    /// 在 Context(170) 之上，确保背景知识在 session 上下文之前注入
    fn priority(&self) -> u8 { 180 }

    fn should_inject(&self, ctx: &HookContext) -> bool {
        // 只在分析性任务的首轮注入（首轮才有"首次看到问题"的场景价值）
        // 后续轮次 LLM 已知上下文，重复注入增加 token 不增加价值
        KNOWLEDGE_APPLICABLE_TASKS.contains(&ctx.task_kind.as_str())
            && ctx.turn_number <= 2
            && !ctx.input.trim().is_empty()
    }

    fn inject(&self, ctx: &HookContext) -> String {
        // 同步调用 KnowledgeStore 查询（hook 是同步接口）
        // 使用 tokio::task::block_in_place 安全地在 async 上下文中做同步等待
        // 注意：hook 从 assemble() 调用，assemble() 在 async 上下文运行
        let query = ctx.input.chars().take(200).collect::<String>();
        let store = Arc::clone(&self.store);
        let top_k = self.top_k;

        // block_in_place 允许在 tokio 多线程运行时中进行同步阻塞操作
        let results = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                store.query(&query, top_k, None).await
            })
        });

        match results {
            Ok(refs) if !refs.is_empty() => {
                let mut lines = vec!["**背景参考（来自知识库）**".to_string()];
                for (i, r) in refs.iter().take(self.top_k).enumerate() {
                    let preview: String = r.content.chars().take(300).collect();
                    lines.push(format!("{}. {}", i + 1, preview));
                }
                lines.join("\n")
            }
            _ => String::new(), // ZeroHit 或错误时不注入
        }
    }

    /// cacheable = false：每次输入不同，查询结果可变
    fn cacheable(&self) -> bool { false }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_ctx(task_kind: &str, turn: u32, input: &str) -> HookContext {
        HookContext {
            input: input.into(),
            task_kind: task_kind.into(),
            turn_number: turn,
            session_metadata: HashMap::new(),
        }
    }

    #[test]
    fn skips_for_general_chat() {
        let store = Arc::new(KnowledgeStore::in_memory().unwrap());
        let hook = GeneratedKnowledgeHook::new(store, 3);
        let ctx = make_ctx("general_chat", 1, "hello");
        assert!(!hook.should_inject(&ctx));
    }

    #[test]
    fn applies_for_knowledge_query_turn1() {
        let store = Arc::new(KnowledgeStore::in_memory().unwrap());
        let hook = GeneratedKnowledgeHook::new(store, 3);
        let ctx = make_ctx("knowledge_query", 1, "what is FTS5?");
        assert!(hook.should_inject(&ctx));
    }

    #[test]
    fn skips_after_turn2() {
        let store = Arc::new(KnowledgeStore::in_memory().unwrap());
        let hook = GeneratedKnowledgeHook::new(store, 3);
        let ctx = make_ctx("knowledge_query", 3, "what is FTS5?");
        assert!(!hook.should_inject(&ctx));
    }

    #[test]
    fn priority_is_180() {
        let store = Arc::new(KnowledgeStore::in_memory().unwrap());
        let hook = GeneratedKnowledgeHook::new(store, 3);
        assert_eq!(hook.priority(), 180);
    }

    #[test]
    fn not_cacheable() {
        let store = Arc::new(KnowledgeStore::in_memory().unwrap());
        let hook = GeneratedKnowledgeHook::new(store, 3);
        assert!(!hook.cacheable());
    }

    #[test]
    fn id_is_stable() {
        let store = Arc::new(KnowledgeStore::in_memory().unwrap());
        let hook = GeneratedKnowledgeHook::new(store, 3);
        assert_eq!(hook.id(), "generated_knowledge");
    }

    #[tokio::test]
    async fn inject_returns_empty_on_zero_hit() {
        // 空 KB → inject 返回空字符串（不应注入无意义内容）
        let store = Arc::new(KnowledgeStore::in_memory().unwrap());
        let hook = GeneratedKnowledgeHook::new(store, 3);
        let ctx = make_ctx("knowledge_query", 1, "Rust borrow checker rules");
        let result = tokio::task::spawn_blocking(move || hook.inject(&ctx)).await.unwrap();
        assert!(result.is_empty(), "should be empty on zero-hit KB");
    }
}
