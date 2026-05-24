//! Provider-specific prompt adaptation layer
//!
//! ## 设计背景
//! Claude、GPT-4、DeepSeek 对 system prompt 的响应模式不同：
//!   - Claude：XML-style tags 效果最好（<section>/<rule>/<constraint>）
//!   - GPT-4/OpenAI-compat：Markdown 结构化指令最优（## headers + bullet points）
//!   - DeepSeek：中文直接指令 + 少样本示例效果好
//!
//! 统一 system prompt 对三个 provider 都是次优的。
//! PromptAdapter 让每个 provider 在使用前对 prompt 做轻量转换，
//! 无需 fork CoreLoop 或 PromptAssembly。
//!
//! ## 架构
//! ```text
//! CoreLoop.build_system_output() → SystemPromptOutput.text
//!       ↓ apply_adapter()
//! SystemPromptOutput.text (provider-optimized)
//! ```
//!
//! ## 引用关系
//! - 被 `TurnPipeline::setup()` 在构建 TurnContext 前调用
//! - `CoreLoop` 持有 `adapter: Arc<dyn PromptAdapter>`
//! - 由 `register_provider_group / register_openai_group / register_anthropic_group`
//!   在注册 provider 时同步设置对应 adapter
//!
//! ## 生命周期
//! - 创建：CoreLoop::new()，默认为 NeutralAdapter
//! - 设置：register_provider 时通过 set_adapter() 替换
//! - 调用：每次 turn setup() 阶段调用一次

/// System prompt 风格类型
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PromptStyle {
    /// Markdown 结构化（## headers + **bold** + bullet）— GPT-4 / OpenAI-compat 最优
    Markdown,
    /// XML-style tags（<section>/<rule>/<constraint>）— Claude 最优
    Xml,
    /// 中文直接指令 + 少样本示例 — DeepSeek 最优
    DeepSeekChinese,
    /// 不变换（默认，向后兼容）
    Neutral,
}

/// Provider-specific prompt 适配器 trait
///
/// ## 使用约定
/// - `apply()` 是热路径，每次 turn 调用一次；实现应保持 O(n) 且无 alloc 优化
/// - 适配器不应改变 prompt 的语义，只改变格式
/// - 测试：每个实现需要 `adapted != original` 的 round-trip 验证
pub trait PromptAdapter: Send + Sync {
    /// Provider 标识（用于 tracing 和 debug）
    fn provider_id(&self) -> &str;

    /// 当前 adapter 的风格类型
    fn style(&self) -> PromptStyle;

    /// 对 system prompt text 做轻量格式转换
    ///
    /// # 参数
    /// - `text`: `SystemPromptOutput.text`，已包含 focus block + 所有动态段
    ///
    /// # 返回
    /// 适配后的 String（格式变换，语义不变）
    fn apply(&self, text: &str) -> String;

    /// 在 tool description 前后可附加 provider-specific 说明（可选）
    ///
    /// 例：Anthropic 在工具描述里添加 `Use this tool when:` 前缀效果更好
    fn tool_description_wrap(&self, name: &str, description: &str) -> String {
        let _ = name;
        description.to_string()
    }
}

// ─── NeutralAdapter（默认，不做任何变换）──────────────────────────────────────

/// 默认适配器：透传，不做任何格式转换
///
/// 向后兼容——所有现有代码路径的 prompt 保持不变。
pub struct NeutralAdapter;

impl PromptAdapter for NeutralAdapter {
    fn provider_id(&self) -> &str { "neutral" }
    fn style(&self) -> PromptStyle { PromptStyle::Neutral }
    fn apply(&self, text: &str) -> String { text.to_string() }
}

// ─── AnthropicAdapter（XML-style tags）──────────────────────────────────────

/// Anthropic Claude 最优格式：将 Markdown ## headers 转换为 XML <section> tags
///
/// ## 转换规则
/// - `## Section Title` → `<section name="section_title">`
/// - 末尾追加对应 `</section>` 闭合标签
/// - `**bold**` 保留（Claude 支持 Markdown，XML 优先用于段落分隔）
/// - `[CONSTRAINT]` / `[RULE]` 等前缀用 `<rule>` 包裹
///
/// ## 效果
/// Claude 对明确的 XML 分隔边界遵从率更高，尤其在多层 prompt 场景
pub struct AnthropicAdapter;

impl PromptAdapter for AnthropicAdapter {
    fn provider_id(&self) -> &str { "anthropic" }
    fn style(&self) -> PromptStyle { PromptStyle::Xml }

    fn apply(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 256);
        let mut open_section: Option<String> = None;

        for line in text.lines() {
            if let Some(title) = line.strip_prefix("## ") {
                // 关闭前一个 section
                if let Some(prev) = open_section.take() {
                    out.push_str(&format!("</section> <!-- {} -->\n\n", prev));
                }
                // 过滤非法 XML 属性名字符：只保留 [a-z0-9_]，其余替换为 _
                // 避免 [Session Focus — Turn 7] 中的 [, ], — 产生无效 XML
                let tag: String = title.to_lowercase()
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
                    .collect::<String>()
                    .trim_matches('_')
                    .to_string();
                out.push_str(&format!("<section name=\"{}\">\n", tag));
                open_section = Some(tag);
            } else if line.starts_with("# ") {
                // 顶层标题不转换，保留原样
                out.push_str(line);
                out.push('\n');
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        // 关闭最后一个 section
        if let Some(prev) = open_section.take() {
            out.push_str(&format!("</section> <!-- {} -->\n", prev));
        }
        out
    }

    fn tool_description_wrap(&self, _name: &str, description: &str) -> String {
        // Claude 对"Use this tool when:"前缀的响应更准确
        if description.to_lowercase().contains("use this tool") {
            description.to_string()
        } else {
            format!("Use this tool when: {}", description)
        }
    }
}

// ─── OpenAIAdapter（Markdown 强化）──────────────────────────────────────────

/// OpenAI GPT-4 / OpenAI-compatible 最优格式
///
/// GPT-4 对结构化 Markdown 响应最好，保持现有格式并做微调：
/// - 确保各段之间有空行分隔
/// - `[CONSTRAINT]` 标注改为 `> **CONSTRAINT:**`（blockquote 更醒目）
/// - 不做大幅结构变换（GPT-4 对 Markdown 原生支持良好）
pub struct OpenAIAdapter;

impl PromptAdapter for OpenAIAdapter {
    fn provider_id(&self) -> &str { "openai" }
    fn style(&self) -> PromptStyle { PromptStyle::Markdown }

    fn apply(&self, text: &str) -> String {
        // GPT-4 基本不需要特殊转换，对现有 Markdown 格式响应良好
        // 将 [CONSTRAINT] / [RULE] 前缀转为 blockquote 使其更突出
        text.replace("[认识论约束违规:", "> **⚠ 认识论约束违规:**")
            .replace("[EPISTEMIC VIOLATION", "> **⚠ EPISTEMIC VIOLATION")
            .replace("[Session Focus", "> **📍 Session Focus")
    }
}

// ─── DeepSeekAdapter（中文直接指令）────────────────────────────────────────

/// DeepSeek 最优格式
///
/// DeepSeek V3/R1 对中文直接指令 + 少样本示例响应最好。
/// 主要转换：
/// - 将英文节标题翻译为中文（常见 header 映射表）
/// - 约束类内容使用"禁止：" / "必须：" 等中文强制语气词
/// - 保持代码块不变
pub struct DeepSeekAdapter;

impl PromptAdapter for DeepSeekAdapter {
    fn provider_id(&self) -> &str { "deepseek" }
    fn style(&self) -> PromptStyle { PromptStyle::DeepSeekChinese }

    fn apply(&self, text: &str) -> String {
        // 映射常见英文 header 到中文（不修改代码块内内容）
        let mappings = [
            ("## Core Behavior",   "## 核心行为"),
            ("## Output Rules",    "## 输出规范"),
            ("## Safety",          "## 安全约束"),
            ("## Identity",        "## 身份定义"),
            ("## Session Context", "## 会话上下文"),
            ("## Model Routing",   "## 模型路由"),
            ("## Interaction Map", "## 交互地图"),
            ("## Session Focus",   "## 会话焦点"),
        ];
        let mut out = text.to_string();
        for (en, zh) in &mappings {
            out = out.replace(en, zh);
        }
        out
    }
}

// ─── 工厂函数 ──────────────────────────────────────────────────────────────

/// 根据 provider_id 创建对应 adapter
///
/// 注册 provider 时调用此函数，返回 Arc<dyn PromptAdapter>
pub fn adapter_for_provider(provider_id: &str) -> std::sync::Arc<dyn PromptAdapter> {
    match provider_id {
        "anthropic" => std::sync::Arc::new(AnthropicAdapter),
        "openai-compatible" | "openai" => std::sync::Arc::new(OpenAIAdapter),
        "deepseek" => std::sync::Arc::new(DeepSeekAdapter),
        _ => std::sync::Arc::new(NeutralAdapter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_neutral_passthrough() {
        let a = NeutralAdapter;
        let text = "## Hello\nworld";
        assert_eq!(a.apply(text), text);
    }

    #[test]
    fn test_anthropic_xml_section() {
        let a = AnthropicAdapter;
        let text = "## Core Behavior\nDo good things\n## Safety\nDon't harm";
        let out = a.apply(text);
        assert!(out.contains("<section name=\"core_behavior\">"));
        assert!(out.contains("<section name=\"safety\">"));
        assert!(out.contains("</section>"));
    }

    #[test]
    fn test_anthropic_tool_wrap() {
        let a = AnthropicAdapter;
        let desc = "Read a file";
        let out = a.tool_description_wrap("filengine_fs_read", desc);
        assert!(out.starts_with("Use this tool when:"));
    }

    #[test]
    fn test_deepseek_header_mapping() {
        let a = DeepSeekAdapter;
        let text = "## Core Behavior\nBe helpful\n## Safety\nNo harm";
        let out = a.apply(text);
        assert!(out.contains("## 核心行为"));
        assert!(out.contains("## 安全约束"));
        assert!(!out.contains("## Core Behavior"));
    }

    #[test]
    fn test_adapter_factory() {
        assert_eq!(adapter_for_provider("anthropic").provider_id(), "anthropic");
        assert_eq!(adapter_for_provider("deepseek").provider_id(), "deepseek");
        assert_eq!(adapter_for_provider("unknown").provider_id(), "neutral");
    }
}
