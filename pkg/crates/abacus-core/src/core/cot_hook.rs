//! Zero-shot CoT Fallback Hook（P0-A1）
//!
//! ## 场景
//! 当模型不支持 native thinking 但用户启用了 thinking intent 时，
//! 自动在 system prompt 末尾注入逐步推理触发词，补强推理质量。
//!
//! ## 触发条件
//! - model_supports_native_thinking = false（从 ModelCatalog 查询后由外部更新）
//! - thinking_enabled = true（当前 turn 的 thinking intent 非 Off）
//! - task_kind 不是 "general_chat"（对话类无需强制推理）
//!
//! ## 引用关系
//! - 被 CoreLoop::new() 注册到 PromptAssembly（通过 register_hook()）
//! - 实现 PromptHook trait（priority = 195，位于 Knowledge(180) 之上）
//! - model_supports_native_thinking / thinking_enabled 由外部持有者通过 Arc<AtomicBool> 动态更新
//!
//! ## 生命周期
//! - 创建：CoreLoop::new() 构建 PromptAssembly 后立即注册
//! - 激活：每轮 assemble()/assemble_segments() 调用时 should_inject() 判定
//! - 销毁：随 PromptAssembly（进而随 CoreLoop）drop

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::core::prompt_assembly::{HookContext, PromptHook};

/// Zero-shot CoT Fallback Hook
///
/// 两个 AtomicBool 均通过 Arc 共享，外部可以在不同线程安全地更新：
/// - `model_supports_native_thinking`：模型是否原生支持 thinking（session 内不变）
/// - `thinking_enabled`：当前 turn 是否有推理意图（per-turn 更新）
pub struct ZeroShotCotHook {
    /// 模型是否原生支持 thinking（false 时触发 CoT 注入）
    ///
    /// ## 引用关系
    /// - 写入方：CoreLoop 在每轮 turn 解析 thinking_intent 并查询 ModelCatalog 后更新
    /// - 读取方：should_inject()
    /// - 生命周期：随 PromptAssembly 存活；session 内 model 不切换时字节稳定
    pub model_supports_native_thinking: Arc<AtomicBool>,

    /// 当前 turn 用户是否有推理意图（非 Off）
    ///
    /// ## 引用关系
    /// - 写入方：CoreLoop 在每轮 turn 构建 thinking_intent 后更新
    /// - 读取方：should_inject()
    /// - 生命周期：per-turn 更新（与 model_supports_native_thinking 相同 Arc 共享）
    pub thinking_enabled: Arc<AtomicBool>,
}

impl ZeroShotCotHook {
    /// 创建 ZeroShotCotHook，并返回用于外部更新的 Arc 引用对。
    ///
    /// ## 返回
    /// `(hook, Arc<model_supports_native_thinking>, Arc<thinking_enabled>)`
    ///
    /// 调用方（CoreLoop::new）持有 Arc 副本，在每轮 turn 开始前更新 AtomicBool。
    pub fn new() -> (Self, Arc<AtomicBool>, Arc<AtomicBool>) {
        let model_flag = Arc::new(AtomicBool::new(false));
        let thinking_flag = Arc::new(AtomicBool::new(false));
        let hook = Self {
            model_supports_native_thinking: model_flag.clone(),
            thinking_enabled: thinking_flag.clone(),
        };
        (hook, model_flag, thinking_flag)
    }
}

impl PromptHook for ZeroShotCotHook {
    fn id(&self) -> &str {
        "zero_shot_cot"
    }

    /// 优先级 195：位于 Knowledge(180) 之上、Constraints(190) 之下，
    /// 确保推理触发词在上下文知识之前被 LLM 读到。
    fn priority(&self) -> u8 {
        195
    }

    fn should_inject(&self, ctx: &HookContext) -> bool {
        !self.model_supports_native_thinking.load(Ordering::Relaxed)
            && self.thinking_enabled.load(Ordering::Relaxed)
            && !matches!(ctx.task_kind.as_str(), "general_chat")
    }

    fn inject(&self, _ctx: &HookContext) -> String {
        "请在回答前逐步分析思路，列出推理步骤后再给出结论。".to_owned()
    }

    /// cacheable = true：session 内 model 不切换，输出字节稳定，
    /// 允许 provider 端 KV cache 复用该 hook 贡献的 prefix。
    fn cacheable(&self) -> bool {
        true
    }
}

// ─── Default 实现（方便测试构造） ───────────────────────────────────────────

impl Default for ZeroShotCotHook {
    fn default() -> Self {
        Self::new().0
    }
}

// ─── 单元测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_ctx(task_kind: &str) -> HookContext {
        HookContext {
            input: String::new(),
            task_kind: task_kind.to_owned(),
            turn_number: 1,
            session_metadata: HashMap::new(),
        }
    }

    /// 默认状态（model_supports=false, thinking=false）→ 不注入
    #[test]
    fn test_no_inject_when_thinking_disabled() {
        let hook = ZeroShotCotHook::default();
        // thinking_enabled = false（默认）
        assert!(!hook.should_inject(&make_ctx("debugging")));
    }

    /// model_supports=false, thinking=true, task=debugging → 注入
    #[test]
    fn test_inject_when_no_native_thinking_and_intent_enabled() {
        let (hook, model_flag, thinking_flag) = ZeroShotCotHook::new();
        model_flag.store(false, Ordering::Relaxed);
        thinking_flag.store(true, Ordering::Relaxed);
        assert!(hook.should_inject(&make_ctx("debugging")));
        let text = hook.inject(&make_ctx("debugging"));
        assert!(text.contains("逐步"));
    }

    /// model_supports=true → 不注入（model 已有 native thinking，无需 CoT fallback）
    #[test]
    fn test_no_inject_when_model_supports_native() {
        let (hook, model_flag, thinking_flag) = ZeroShotCotHook::new();
        model_flag.store(true, Ordering::Relaxed);
        thinking_flag.store(true, Ordering::Relaxed);
        assert!(!hook.should_inject(&make_ctx("debugging")));
    }

    /// task_kind = general_chat → 不注入（对话场景无需强制推理）
    #[test]
    fn test_no_inject_for_general_chat() {
        let (hook, model_flag, thinking_flag) = ZeroShotCotHook::new();
        model_flag.store(false, Ordering::Relaxed);
        thinking_flag.store(true, Ordering::Relaxed);
        assert!(!hook.should_inject(&make_ctx("general_chat")));
    }

    /// priority = 195
    #[test]
    fn test_priority() {
        let hook = ZeroShotCotHook::default();
        assert_eq!(hook.priority(), 195);
    }

    /// cacheable = true
    #[test]
    fn test_cacheable() {
        let hook = ZeroShotCotHook::default();
        assert!(hook.cacheable());
    }

    /// id = "zero_shot_cot"
    #[test]
    fn test_id() {
        let hook = ZeroShotCotHook::default();
        assert_eq!(hook.id(), "zero_shot_cot");
    }

    /// Arc 共享：外部更新 AtomicBool 后 should_inject 立即感知
    #[test]
    fn test_arc_shared_flag_update() {
        let (hook, model_flag, thinking_flag) = ZeroShotCotHook::new();
        // 初始：不注入
        assert!(!hook.should_inject(&make_ctx("code_writing")));
        // 开启 thinking
        thinking_flag.store(true, Ordering::Relaxed);
        assert!(hook.should_inject(&make_ctx("code_writing")));
        // 模拟 model 切换为支持 native thinking 的型号
        model_flag.store(true, Ordering::Relaxed);
        assert!(!hook.should_inject(&make_ctx("code_writing")));
    }
}
