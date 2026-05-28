//! Abacus TUI — 首次配置向导（含免责声明）
//!
//! ## 流程
//! 1. 检测是否已有配置 → 有则跳过
//! 2. 配置页：上区为使用须知，下区为 API URL + API Key
//! 3. Enter 同时接受条款 + 保存配置

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};
use ratatui::Frame;
use ratatui::Terminal;

#[derive(Debug, Clone, Copy, PartialEq)]
enum ProviderKind {
    DeepSeek,
    OpenAI,
    Anthropic,
    Dashscope,    // 阿里云通义千问
    Moonshot,     // 月之暗面 Kimi
    Zhipu,        // 智谱 GLM
    SiliconFlow,  // 硅基流动
    Groq,         // Groq 快速推理
    Volcengine,   // 火山引擎方舟（豆包）
    Tencent,      // 腾讯云混元
    MiniMax,      // MiniMax
    Yi,           // 零一万物
    Baichuan,     // 百川
    Ollama,       // 本地 Ollama
    Generic,      // 其他 OpenAI 兼容
}

impl ProviderKind {
    fn detect(base_url: &str) -> Self {
        let lower = base_url.to_lowercase();
        // 优先级：精确域名特征 > 路径特征 > 通用兜底
        if lower.contains("deepseek")                                       { return ProviderKind::DeepSeek; }
        if lower.contains("anthropic") || lower.contains("claude")         { return ProviderKind::Anthropic; }
        if lower.contains("dashscope") || lower.contains("aliyun") || lower.contains("bailian") { return ProviderKind::Dashscope; }
        if lower.contains("moonshot")  || lower.contains("kimi")           { return ProviderKind::Moonshot; }
        if lower.contains("bigmodel")  || lower.contains("zhipu")          { return ProviderKind::Zhipu; }
        if lower.contains("siliconflow")                                    { return ProviderKind::SiliconFlow; }
        if lower.contains("groq")                                           { return ProviderKind::Groq; }
        // 火山引擎方舟：ark.cn-* / volces.com / volcengine
        if lower.contains("volces") || lower.contains("volcengine") || lower.contains("ark.cn") {
            return ProviderKind::Volcengine;
        }
        if lower.contains("hunyuan") || lower.contains("tencent")          { return ProviderKind::Tencent; }
        if lower.contains("minimax")                                        { return ProviderKind::MiniMax; }
        if lower.contains("lingyiwanwu") || lower.contains("01.ai")        { return ProviderKind::Yi; }
        if lower.contains("baichuan")                                       { return ProviderKind::Baichuan; }
        // Ollama 本地：localhost:11434 或含 "ollama"
        if lower.contains("localhost:11434") || lower.contains("ollama")   { return ProviderKind::Ollama; }
        if lower.contains("openai")                                         { return ProviderKind::OpenAI; }
        ProviderKind::Generic
    }

    fn label(&self) -> &'static str {
        match self {
            ProviderKind::DeepSeek    => "DeepSeek API",
            ProviderKind::OpenAI      => "OpenAI API",
            ProviderKind::Anthropic   => "Anthropic API",
            ProviderKind::Dashscope   => "阿里云百炼",
            ProviderKind::Moonshot    => "Moonshot (Kimi)",
            ProviderKind::Zhipu       => "智谱 (GLM)",
            ProviderKind::SiliconFlow => "SiliconFlow",
            ProviderKind::Groq        => "Groq",
            ProviderKind::Volcengine  => "火山引擎方舟",
            ProviderKind::Tencent     => "腾讯云混元",
            ProviderKind::MiniMax     => "MiniMax",
            ProviderKind::Yi          => "零一万物",
            ProviderKind::Baichuan    => "百川",
            ProviderKind::Ollama      => "Ollama (本地)",
            ProviderKind::Generic     => "OpenAI Compatible",
        }
    }

    fn config_prefix(&self) -> &str {
        match self {
            ProviderKind::DeepSeek    => "deepseek",
            ProviderKind::OpenAI      => "openai",
            ProviderKind::Anthropic   => "anthropic",
            ProviderKind::Dashscope   => "dashscope",
            ProviderKind::Moonshot    => "moonshot",
            ProviderKind::Zhipu       => "zhipu",
            ProviderKind::SiliconFlow => "siliconflow",
            ProviderKind::Groq        => "groq",
            ProviderKind::Volcengine  => "volcengine",
            ProviderKind::Tencent     => "tencent",
            ProviderKind::MiniMax     => "minimax",
            ProviderKind::Yi          => "yi",
            ProviderKind::Baichuan    => "baichuan",
            ProviderKind::Ollama      | ProviderKind::Generic => "openai",
        }
    }

    fn default_model(&self) -> &str {
        match self {
            ProviderKind::DeepSeek    => "deepseek-v4-flash",
            ProviderKind::OpenAI      => "gpt-4o",
            ProviderKind::Anthropic   => "claude-sonnet-4-5",
            ProviderKind::Dashscope   => "qwen-max",
            ProviderKind::Moonshot    => "moonshot-v1-128k",
            ProviderKind::Zhipu       => "glm-4-flash",
            ProviderKind::SiliconFlow => "deepseek-v4-flash",
            ProviderKind::Groq        => "llama-3.3-70b-versatile",
            ProviderKind::Volcengine  => "doubao-1-5-pro-32k",
            ProviderKind::Tencent     => "hunyuan-turbo",
            ProviderKind::MiniMax     => "abab6.5s-chat",
            ProviderKind::Yi          => "yi-lightning",
            ProviderKind::Baichuan    => "Baichuan4-Air",
            ProviderKind::Ollama      => "llama3.2",
            ProviderKind::Generic     => "gpt-4o",
        }
    }

    fn is_openai_compatible(&self) -> bool {
        // Anthropic 使用独有协议写入配置；其余（含 Ollama、各云厂商）均用 OpenAI 兼容格式
        !matches!(self, ProviderKind::Anthropic)
    }

    /// provider 层级的上下文提示（API 未返回 context_window 时的兜底）
    fn typical_max_context(&self) -> &'static str {
        match self {
            ProviderKind::DeepSeek    => "最大 1M（V4 系列）",
            ProviderKind::OpenAI      => "最大 128k（GPT-4o）",
            ProviderKind::Anthropic   => "最大 200k（Claude 3.x）",
            ProviderKind::Dashscope   => "最大 1M（Qwen-Long）",
            ProviderKind::Moonshot    => "最大 128k",
            ProviderKind::Zhipu       => "最大 128k（GLM-4）",
            ProviderKind::SiliconFlow => "按代理模型规格",
            ProviderKind::Groq        => "最大 128k",
            ProviderKind::Volcengine  => "最大 128k（豆包系列）",
            ProviderKind::Tencent     => "最大 256k（混元）",
            ProviderKind::MiniMax     => "最大 1M（MiniMax-01）",
            ProviderKind::Yi          => "最大 200k",
            ProviderKind::Baichuan    => "最大 128k",
            ProviderKind::Ollama      => "按加载模型规格",
            ProviderKind::Generic     => "按模型规格",
        }
    }
}

/// 建议的 API URL
const SUGGESTED_URL: &str = "https://api.deepseek.com";

struct SetupState {
    focus: FocusField,
    api_key: String,
    base_url: String,
    model_name: String,
    show_api_key: bool,
    show_suggestions: bool,
    exit: bool,
    skip: bool,
    /// 从 API 检索到的模型列表
    fetched_models: Vec<String>,
    /// 模型 → context_window（tokens），API 返回时存入；未返回则无条目
    model_contexts: std::collections::HashMap<String, u64>,
    /// 模型检索状态
    model_fetch_status: ModelFetchStatus,
    /// 当前在 fetched_models 中的选中 index（Tab 循环）
    model_select_idx: usize,
    /// 异步检索结果接收器（携带 context_window）
    model_rx: Option<std::sync::mpsc::Receiver<Vec<(String, Option<u64>)>>>,
    /// 模型支持的最大上下文大小（单位 k token，如 "1000" = 1M，"128" = 128k）
    context_window: String,
    /// Abacus 实际使用的上下文（单位 k token，空 = 全用，最低 128k）
    context_window_use: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ModelFetchStatus {
    Idle,       // 未检索
    Fetching,   // 检索中...
    Done,       // 已完成（结果在 fetched_models）
    Failed,     // 检索失败（用默认列表）
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FocusField {
    BaseUrl,
    ModelName,
    ApiKey,
    ContextWindow,
    ContextWindowUse,
}

impl SetupState {
    fn new() -> Self {
        // 预填默认值：用户只需输入 API Key 即可完成配置
        let default_url = SUGGESTED_URL.to_string();
        let default_model = ProviderKind::detect(&default_url).default_model().to_string();
        Self {
            focus: FocusField::ApiKey, // 直接聚焦到 API Key（URL 和 Model 已有默认值）
            api_key: String::new(),
            base_url: default_url,
            model_name: default_model,
            show_api_key: false,
            show_suggestions: true,
            exit: false,
            skip: false,
            fetched_models: Vec::new(),
            model_contexts: std::collections::HashMap::new(),
            model_fetch_status: ModelFetchStatus::Idle,
            model_select_idx: 0,
            model_rx: None,
            context_window: String::new(),
            context_window_use: String::new(),
        }
    }
    fn provider(&self) -> ProviderKind {
        if self.base_url.is_empty() {
            ProviderKind::OpenAI
        } else {
            ProviderKind::detect(&self.base_url)
        }
    }
    fn detected_label(&self) -> Option<&'static str> {
        if self.base_url.is_empty() {
            return None;
        }
        Some(self.provider().label())
    }
    fn is_all_filled(&self) -> bool {
        !self.api_key.is_empty()
    }
}

fn config_dir() -> PathBuf {
    std::env::var("ABACUS_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".abacus")
        })
}

fn config_path() -> PathBuf {
    config_dir().join("config.yaml")
}

fn disclaimer_path() -> PathBuf {
    config_dir().join("disclaimer_ack")
}

/// 检测是否已有有效 API 配置
pub fn has_api_config() -> bool {
    if std::env::var("ABACUS_API_KEY").is_ok()
        || std::env::var("DEEPSEEK_API_KEY").is_ok()
        || std::env::var("ANTHROPIC_API_KEY").is_ok()
        || std::env::var("ABACUS_OPENAI_BASE_URL").is_ok()
    {
        return true;
    }
    if let Ok(content) = std::fs::read_to_string(config_path()) {
        // M3 fix: 跳过注释行，仅匹配非空 api_key 赋值行
        // 防止高级配置注释块（含 # openai_api_key: ""）触发误报
        let has_real_key = content.lines().any(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with('#')
                && trimmed.contains("api_key:")
                && !trimmed.contains("api_key: \"\"")
                && !trimmed.contains("api_key: ''")
        });
        if has_real_key {
            return true;
        }
    }
    false
}

/// 检查免责声明是否已接受
pub fn disclaimer_accepted() -> bool {
    disclaimer_path().exists()
}

fn accept_disclaimer() {
    if let Some(parent) = disclaimer_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(disclaimer_path(), "accepted");
}

/// 解析上下文大小输入（单位 k token）
/// 支持格式："1000k" / "1000" / "1m" → 返回 token 数
/// 无法解析或为 0 时返回 None
fn parse_context_k(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    if s.is_empty() { return None; }
    // "1m" / "2m" → 百万
    if let Some(n_str) = s.strip_suffix('m') {
        return n_str.trim().parse::<u64>().ok().map(|n| n.saturating_mul(1_000_000));
    }
    // "1000k" / "128k" / "1000" → 千
    let n_str = s.strip_suffix('k').unwrap_or(&s);
    n_str.trim().parse::<u64>().ok().map(|n| n.saturating_mul(1_000))
}

/// YAML 字符串值转义（H1 fix）
/// 转义双引号、反斜杠、换行，防止用户输入破坏 YAML 结构
fn yaml_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
     .replace('"', "\\\"")
     .replace('\n', "\\n")
     .replace('\r', "\\r")
}

fn save_config(state: &SetupState) -> Result<(), String> {
    let provider = state.provider();
    let raw_url = if state.base_url.is_empty() {
        "https://api.openai.com".to_string()
    } else {
        // M4 fix: 先 trim 空白，再剥离版本路径后缀，再 trim 尾斜杠
        let trimmed = state.base_url.trim();
        let stripped = trimmed
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .trim_end_matches("/v2")
            .trim_end_matches("/v3")
            .trim_end_matches("/v4")
            .trim_end_matches('/')
            .trim()
            .to_string();
        stripped
    };
    // H1 fix: 所有用户输入字段写入 YAML 前转义
    let base_url   = yaml_escape(&raw_url);
    let api_key_e  = yaml_escape(&state.api_key);
    let model_e    = yaml_escape(if state.model_name.is_empty() {
        provider.default_model()
    } else {
        &state.model_name
    });

    // model_e / api_key_e / base_url 已在上方转义，resolved_model 仅用于 cw_tokens 查找
    let resolved_model = model_e.as_str();

    // 解析上下文配置
    // context_window 为空 → 不写入 config，引擎从 model catalog 自动取模型最大值
    let cw_tokens_opt: Option<u64> = parse_context_k(&state.context_window)
        .map(|n| n.max(128_000));
    let cw_ratio = if state.context_window_use.is_empty() {
        1.0f64 // 空 = 全用（不压缩）
    } else {
        let cw_base = cw_tokens_opt.unwrap_or(u64::MAX); // 无上限时按用户指定值直接用
        let use_tokens = parse_context_k(&state.context_window_use)
            .unwrap_or(128_000)
            .max(128_000);
        if cw_base == u64::MAX {
            1.0 // 无法计算比例，全用
        } else {
            (use_tokens as f64 / cw_base as f64).min(1.0)
        }
    };

    // context_window 行：仅在用户明确填写时写入；空 = 引擎从 model catalog 自动取
    let cw_line = match cw_tokens_opt {
        Some(n) => format!("  context_window: {}\n", n),
        None    => String::new(),
    };

    // 2026-05-28: 新格式 — providers 数组 + 固化配置指引头部
    let provider_type_str = if provider.is_openai_compatible() {
        if base_url.contains("deepseek.com") { "deepseek" } else { "openai-compatible" }
    } else {
        "anthropic"
    };

    let yaml = format!(
        r#"# ╔═══════════════════════════════════════════════════════════════════════════════╗
# ║  ABACUS 配置文件 (config.yaml)                                              ║
# ╠═══════════════════════════════════════════════════════════════════════════════╣
# ║                                                                             ║
# ║  providers:                   # 供应商列表（按优先级排序）                   ║
# ║    - id: <唯一标识>           # 供应商 ID（用于 /model 切换）                ║
# ║      type: <协议类型>         # anthropic | openai-compatible | deepseek     ║
# ║      api_key: <密钥>         # 明文 或 env:ENV_VAR（从环境变量读取）         ║
# ║      base_url: <端点>        # API 地址（可选，各 type 有默认值）            ║
# ║      models:                  # 模型列表（简写或详写）                       ║
# ║        - model-name           # 简写：用 provider 默认参数                   ║
# ║        - name: model-name     # 详写：覆盖参数（未指定的用默认值）           ║
# ║          context_window: N    #   上下文窗口（token 数）                     ║
# ║          max_tokens: N        #   单次最大输出 token                         ║
# ║          temperature: 0.0-2.0 #   生成温度                                  ║
# ║          thinking: off|adaptive|low|medium|high|max                         ║
# ║                                                                             ║
# ║  core:                        # 全局运行参数                                 ║
# ║    default_model: <model>     # 默认模型                                    ║
# ║    stream: true               # 流式输出                                    ║
# ║                                                                             ║
# ║  fallback_chain: [id1, id2]   # 回退链（不可达时按序尝试下一个）             ║
# ║                                                                             ║
# ║  TUI: /model <provider>/<model> 切换 | /model list 查看可用                 ║
# ║  参数规则: 未指定的参数使用 provider/模型内置默认值                          ║
# ║                                                                             ║
# ╚═══════════════════════════════════════════════════════════════════════════════╝

# ─── 供应商配置 ─────────────────────────────────────────────────────────────────
providers:
  - id: primary
    type: {}
    api_key: "{}"
    base_url: "{}"
    models:
      - {}

# ─── 全局设置 ───────────────────────────────────────────────────────────────────
core:
  default_model: "{}"
  stream: true
{}  context_window_ratio: {:.4}
"#,
        provider_type_str, api_key_e, base_url, resolved_model,
        resolved_model, cw_line, cw_ratio,
    );

    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;
    std::fs::write(config_path(), &yaml).map_err(|e| format!("写入失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(config_path()) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(config_path(), perms);
        }
    }
    Ok(())
}

// ── 渲染 ─────────────────────────────────────────────────────────────

/// 免责声明条款文本
///
/// V13: 4 个分项色由硬编码 RGB 改为主题感知语义色——保证 setup 屏与用户最终主题一致
/// 引用关系：被 render_setup 调用，传入 setup_theme()
/// 设计意图：不同主题（light/dark/apple…）下视觉一致；不再"配置屏永远品牌深蓝色调，与最终主题脱节"
fn disclaimer_lines(theme: &Theme) -> Vec<Line<'static>> {
    use crate::tui::theme::{SemanticIntent, Strength};
    let danger = theme.semantic_style(SemanticIntent::Danger, Strength::Strong);
    let warning = theme.semantic_style(SemanticIntent::Warning, Strength::Strong);
    let info = theme.semantic_style(SemanticIntent::Info, Strength::Strong);
    let neutral = theme.semantic_style(SemanticIntent::Neutral, Strength::Strong);
    vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled(" 1. 数据安全 ", danger),
            Span::raw("— AI 操作可能具有破坏性，请务必提前备份重要数据。"),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" 2. 人工审查 ", warning),
            Span::raw("— AI 生成的代码可能存在缺陷，运行前请严格审查。"),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" 3. 合规使用 ", info),
            Span::raw("— 严禁用于恶意攻击或非法用途。"),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" 4. 免责条款 ", neutral),
            Span::raw("— 本工具\"按原样\"提供，开发者不对任何损失负责。"),
        ]),
    ]
}

use crate::tui::theme::Theme;

fn setup_theme() -> Theme {
    let mut t = Theme::init();
    t.set_mode_color("clarify");
    t
}

fn render_setup(f: &mut Frame, state: &SetupState) {
    let area = f.area();
    let theme = setup_theme();

    // 全局背景（使用主题色）
    let buf = f.buffer_mut();
    for x in 0..area.width {
        for y in 0..area.height {
            buf[(x, y)].set_bg(theme.bg);
        }
    }

    // H2 fix: clamp cw/ch 不超过终端实际尺寸，用 saturating_sub 防止 u16 下溢 panic
    let cw = ((area.width as f64 * 0.6).max(50.0).min(70.0) as u16).min(area.width);
    let ch = ((area.height as f64 * 0.90).max(32.0).min(42.0) as u16).min(area.height);
    let cx = area.width.saturating_sub(cw) / 2;
    let cy = area.height.saturating_sub(ch) / 2;
    let card = Rect::new(cx, cy, cw, ch);

    let block = Block::default()
        .title(" 首次配置 ")
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(theme.gold));
    let inner = block.inner(card);
    block.render(card, f.buffer_mut());

    // 分区: 条款 | URL | 推荐 | Key | Model | ContextWindow | ContextUse | 提示
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),   // 0  条款标题
            Constraint::Min(2),      // 1  条款内容
            Constraint::Length(1),   // 2  gap
            Constraint::Length(3),   // 3  API URL
            Constraint::Length(1),   // 4  provider 推荐提示
            Constraint::Length(1),   // 5  gap
            Constraint::Length(3),   // 6  API Key
            Constraint::Length(1),   // 7  gap
            Constraint::Length(3),   // 8  默认模型
            Constraint::Length(1),   // 9  gap
            Constraint::Length(3),   // 10 模型上下文大小
            Constraint::Length(1),   // 11 gap
            Constraint::Length(3),   // 12 实际使用上下文
            Constraint::Length(2),   // 13 底部提示
        ])
        .split(inner);

    // ── 条款标题 ──
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " 使用须知",
            Style::default().fg(theme.gold).add_modifier(Modifier::BOLD),
        ))),
        parts[0],
    );

    // ── 条款内容 ──
    let terms_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    let terms_inner = terms_block.inner(parts[1]);
    terms_block.render(parts[1], f.buffer_mut());
    f.render_widget(
        Paragraph::new(disclaimer_lines(&theme)).wrap(Wrap { trim: false }),
        terms_inner,
    );

    // ── API URL ──
    let detected_tag = match state.detected_label() {
        Some(label) => format!("（{label}）"),
        None => String::new(),
    };
    let url_focus = if state.focus == FocusField::BaseUrl { " > " } else { "   " };
    let url_title = format!("{url_focus}API URL {detected_tag}");

    let url_block = Block::default()
        .title(url_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(
            if state.focus == FocusField::BaseUrl { theme.primary } else { theme.border }
        ));
    let url_inner = url_block.inner(parts[3]);
    url_block.render(parts[3], f.buffer_mut());

    let placeholder = "例如: https://api.openai.com/v1";
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            if state.base_url.is_empty() { placeholder } else { &state.base_url },
            Style::default().fg(
                if state.base_url.is_empty() { theme.muted }
                else { theme.success }
            ),
        ))),
        url_inner,
    );

    // ── DeepSeek 推荐 ──
    let suggest_text = format!(" ▸ {SUGGESTED_URL}（DeepSeek）");
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            suggest_text,
            Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
        ))),
        parts[4],
    );

    // ── API Key ──
    let key_focus = if state.focus == FocusField::ApiKey { " > " } else { "   " };
    let key_title = format!("{}API Key", key_focus);

    let key_display = if state.api_key.is_empty() {
        "粘贴或输入你的 API Key...".to_string()
    } else if state.show_api_key {
        state.api_key.clone()
    } else {
        format!("{}{}",
            "•".repeat(state.api_key.len().min(40)),
            if state.api_key.len() > 40 { format!(" ({} chars)", state.api_key.len()) } else { String::new() },
        )
    };
    let api_key_block = Block::default()
        .title(key_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(
            if state.focus == FocusField::ApiKey { theme.gold } else { theme.border }
        ));
    let ak_inner = api_key_block.inner(parts[6]);
    api_key_block.render(parts[6], f.buffer_mut());

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            key_display,
            Style::default().fg(
                if state.api_key.is_empty() { theme.muted }
                else { theme.success }
            ),
        ))),
        ak_inner,
    );

    // ── Model Name ──
    // SU1: 旧静态推荐表已被 fetched_models（线上 /models 接口）取代——删除 dead variable
    let model_focus = if state.focus == FocusField::ModelName { " > " } else { "   " };
    let model_status = match state.model_fetch_status {
        ModelFetchStatus::Fetching => " ⟳ 检索中...",
        ModelFetchStatus::Done => {
            if state.fetched_models.is_empty() { " (无可用模型)" }
            else { "" }
        }
        ModelFetchStatus::Failed => " (检索失败，请手动输入)",
        ModelFetchStatus::Idle => "",
    };
    let model_count = if !state.fetched_models.is_empty() {
        format!(" [{}/{}]", state.model_select_idx + 1, state.fetched_models.len())
    } else {
        String::new()
    };
    let model_title = format!("{}默认模型 (Tab 循环选择，可随时更改){}{}", model_focus, model_count, model_status);
    let model_display = if state.model_name.is_empty() {
        let provider = state.provider();
        let def = provider.default_model();
        format!("默认: {}", def)
    } else {
        state.model_name.clone()
    };
    let model_block = Block::default()
        .title(model_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(
            if state.focus == FocusField::ModelName { theme.primary } else { theme.border }
        ));
    let mn_inner = model_block.inner(parts[8]);
    model_block.render(parts[8], f.buffer_mut());
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            model_display,
            Style::default().fg(
                if state.model_name.is_empty() { theme.muted }
                else { theme.success }
            ),
        ))),
        mn_inner,
    );

    // ── 模型下拉列表（默认模型字段聚焦 + 有候选模型时显示）──
    // 在 model_block 下方绘制浮层：最多 5 条，超出显示省略
    // 引用关系：fetched_models / model_select_idx / model_block 位置
    // 生命周期：焦点离开 ModelName 时下拉消失
    if state.focus == FocusField::ModelName && !state.fetched_models.is_empty() {
        use ratatui::widgets::Clear;
        let max_visible: usize = 5;
        let list_h = (state.fetched_models.len().min(max_visible) as u16) + 2; // 边框
        let model_rect = parts[8];
        // 下拉放在 model_block 正下方（绝对坐标）
        let drop_y = model_rect.y + model_rect.height;
        let drop_x = model_rect.x;
        let drop_w = model_rect.width.min(card.width);
        // H3 fix: saturating_sub 防止 card.height < list_h 时 u16 下溢 panic
        let drop_y = drop_y.min((card.y + card.height).saturating_sub(list_h));
        if drop_y + list_h <= area.height {
            let drop_area = Rect::new(drop_x, drop_y, drop_w, list_h);
            f.render_widget(Clear, drop_area);
            let drop_block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.primary));
            let drop_inner = drop_block.inner(drop_area);
            f.render_widget(drop_block, drop_area);

            let scroll_start = if state.fetched_models.len() <= max_visible { 0 }
                else { state.model_select_idx.saturating_sub(1).min(state.fetched_models.len() - max_visible) };
            let mut model_list_lines: Vec<Line> = Vec::new();
            for (i, name) in state.fetched_models
                .iter()
                .enumerate()
                .skip(scroll_start)
                .take(max_visible)
            {
                let is_sel = i == state.model_select_idx;
                let marker = if is_sel { "> " } else { "  " };
                let style = if is_sel {
                    Style::default().fg(theme.primary).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text)
                };
                model_list_lines.push(Line::from(Span::styled(
                    format!("{}{}", marker, name),
                    style,
                )));
            }
            f.render_widget(Paragraph::new(model_list_lines), drop_inner);
        }
    }

    // ── 模型上下文大小 ──
    let cw_focus = if state.focus == FocusField::ContextWindow { " > " } else { "   " };
    let cw_block = Block::default()
        .title(format!("{}模型上下文大小 (单位 k，如 1000=1M，128=128k，空=128k)", cw_focus))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(
            if state.focus == FocusField::ContextWindow { theme.primary } else { theme.border }
        ));
    let cw_inner = cw_block.inner(parts[10]);
    cw_block.render(parts[10], f.buffer_mut());
    let cw_display = if state.context_window.is_empty() {
        // 优先用 API 返回的当前选中模型 context_window
        if let Some(&ctx_tokens) = state.model_contexts.get(&state.model_name) {
            let ctx_k = ctx_tokens / 1_000;
            format!("空 = 按模型规格（{} 约 {}k）", state.model_name, ctx_k)
        } else {
            // 兜底：provider 层级静态提示
            format!("空 = 按模型规格（{}）", state.provider().typical_max_context())
        }
    } else {
        state.context_window.clone()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            cw_display,
            Style::default().fg(
                if state.context_window.is_empty() { theme.muted } else { theme.success }
            ),
        ))),
        cw_inner,
    );

    // ── 实际使用上下文 ──
    let cwu_focus = if state.focus == FocusField::ContextWindowUse { " > " } else { "   " };
    let cwu_block = Block::default()
        .title(format!("{}实际使用上下文 (单位 k，空=全用，最低 128k)", cwu_focus))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(
            if state.focus == FocusField::ContextWindowUse { theme.primary } else { theme.border }
        ));
    let cwu_inner = cwu_block.inner(parts[12]);
    cwu_block.render(parts[12], f.buffer_mut());
    let cwu_display = if state.context_window_use.is_empty() {
        "空 = 全用 (等于模型上限)".to_string()
    } else {
        state.context_window_use.clone()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            cwu_display,
            Style::default().fg(
                if state.context_window_use.is_empty() { theme.muted } else { theme.success }
            ),
        ))),
        cwu_inner,
    );

    // ── 底部提示 ──
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                " Tab 切换字段 · Enter 确认 · Esc 退出 · Ctrl+H 显示/隐藏 Key",
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            )),
            Line::from(Span::styled(
                " Enter 即表示同意使用条款，配置项后续均可修改",
                Style::default().fg(theme.border).add_modifier(Modifier::DIM),
            )),
        ]),
        parts[13],
    );
}

// ── 事件处理 ─────────────────────────────────────────────────────────

/// URL 变更时同步默认模型名
///
/// 当用户修改了 base_url，如果 model_name 仍是某个 provider 的默认值
/// （说明用户没有手动修改过），则自动切换到新 provider 的默认模型。
/// 如果用户已经手动输入了自定义模型名，则不覆盖。
fn sync_default_model(state: &mut SetupState) {
    let new_provider = state.provider();
    let new_default = new_provider.default_model();

    // 判断当前 model_name 是否是某个 provider 的默认值（即用户未手动修改）
    const ALL_PROVIDERS: &[ProviderKind] = &[
        ProviderKind::DeepSeek, ProviderKind::OpenAI, ProviderKind::Anthropic,
        ProviderKind::Dashscope, ProviderKind::Moonshot, ProviderKind::Zhipu,
        ProviderKind::SiliconFlow, ProviderKind::Groq, ProviderKind::Volcengine,
        ProviderKind::Tencent, ProviderKind::MiniMax, ProviderKind::Yi,
        ProviderKind::Baichuan, ProviderKind::Ollama, ProviderKind::Generic,
    ];
    let is_still_default = state.model_name.is_empty()
        || ALL_PROVIDERS.iter().any(|p| state.model_name == p.default_model());

    if is_still_default {
        state.model_name = new_default.to_string();
    }
}

/// 触发异步模型列表检索（GET {base_url}/models）
///
/// 使用 std::thread 避免阻塞 setup 事件循环
/// 结果通过 mpsc channel 返回
fn trigger_model_fetch(state: &mut SetupState) {
    if state.base_url.is_empty() || state.api_key.is_empty() {
        return;
    }
    if state.model_fetch_status == ModelFetchStatus::Fetching {
        return; // 已在检索中
    }

    state.model_fetch_status = ModelFetchStatus::Fetching;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<(String, Option<u64>)>>();
    state.model_rx = Some(rx);

    let base_url = state.base_url.clone();
    let api_key = state.api_key.clone();

    std::thread::spawn(move || {
        let models = fetch_model_list_sync(&base_url, &api_key);
        let _ = tx.send(models);
    });
}

/// 同步 HTTP 请求模型列表（在子线程中执行）
///
/// ## URL 策略
/// URL 已含版本路径（/v1、/v2、/v3）→ 直接追加 /models
/// 否则依次尝试 /v1/models → /models，第一个返回非空结果即用
///
/// ## Anthropic 特殊处理
/// Anthropic 使用 `x-api-key` + `anthropic-version` 头，非标 Bearer
fn fetch_model_list_sync(base_url: &str, api_key: &str) -> Vec<(String, Option<u64>)> {
    let lower = base_url.to_lowercase();
    if lower.contains("anthropic") || lower.contains("claude") {
        return fetch_anthropic_models(base_url, api_key);
    }

    // 构建候选 URL 列表
    let base = base_url.trim_end_matches('/');
    let has_version = base.ends_with("/v1") || base.ends_with("/v2")
        || base.ends_with("/v3") || base.ends_with("/v4");
    let candidates: Vec<String> = if has_version {
        vec![format!("{}/models", base)]
    } else {
        vec![
            format!("{}/v1/models", base),
            format!("{}/models", base),
        ]
    };

    for url in &candidates {
        match ureq::get(url.as_str())
            .set("Authorization", &format!("Bearer {}", api_key))
            .call()
        {
            Ok(resp) => {
                let body = resp.into_string().unwrap_or_default();
                let models = parse_models_response(&body);
                if !models.is_empty() {
                    return models;
                }
            }
            Err(_) => continue,
        }
    }
    Vec::new()
}

/// Anthropic /v1/models 专用请求（x-api-key + anthropic-version 头）
///
/// 引用关系：被 fetch_model_list_sync 在检测到 Anthropic URL 时调用
fn fetch_anthropic_models(base_url: &str, api_key: &str) -> Vec<(String, Option<u64>)> {
    let base = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let url = format!("{}/v1/models", base);
    match ureq::get(&url)
        .set("x-api-key", api_key)
        .set("anthropic-version", "2023-06-01")
        .call()
    {
        Ok(resp) => {
            let body = resp.into_string().unwrap_or_default();
            parse_anthropic_models(&body)
        }
        Err(_) => Vec::new(),
    }
}

/// 解析 Anthropic /v1/models 响应
/// 返回格式：{"data": [{"id": "claude-...", "type": "model", ...}]}
/// Anthropic 模型列表不含 context_window，统一返回 None
fn parse_anthropic_models(json: &str) -> Vec<(String, Option<u64>)> {
    let mut models = Vec::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
        if let Some(data) = v.get("data").and_then(|d| d.as_array()) {
            for item in data {
                if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                    models.push((id.to_string(), None));
                }
            }
        }
    }
    models.sort_by(|a, b| a.0.cmp(&b.0));
    models.reverse();
    models
}

/// 解析 /models API 响应（OpenAI 兼容格式）
/// 返回 (model_id, context_window_tokens)；context_window 字段缺失时为 None
fn parse_models_response(json: &str) -> Vec<(String, Option<u64>)> {
    let mut models = Vec::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
        if let Some(data) = v.get("data").and_then(|d| d.as_array()) {
            for item in data {
                if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                    // 过滤：只保留对话类模型（排除 embedding/tts/whisper/dall）
                    let lower = id.to_lowercase();
                    if lower.contains("embed") || lower.contains("tts")
                        || lower.contains("whisper") || lower.contains("dall") {
                        continue;
                    }
                    // 兼容多种字段名：context_window / context_length / max_context_length
                    let ctx = item.get("context_window")
                        .or_else(|| item.get("context_length"))
                        .or_else(|| item.get("max_context_length"))
                        .and_then(|v| v.as_u64())
                        .filter(|&n| n > 0);
                    models.push((id.to_string(), ctx));
                }
            }
        }
    }
    models.sort_by(|a, b| a.0.cmp(&b.0));
    models.reverse(); // 最新模型排前面
    models
}

/// 检查异步检索结果（非阻塞，每帧调用）
fn poll_model_fetch(state: &mut SetupState) {
    if let Some(ref rx) = state.model_rx {
        match rx.try_recv() {
            Ok(items) => {
                if items.is_empty() {
                    state.model_fetch_status = ModelFetchStatus::Failed;
                } else {
                    // 拆分：names 用于 UI 列表，contexts 用于上下文大小提示
                    for (id, ctx) in &items {
                        if let Some(c) = ctx {
                            state.model_contexts.insert(id.clone(), *c);
                        }
                    }
                    state.fetched_models = items.into_iter().map(|(id, _)| id).collect();
                    state.model_fetch_status = ModelFetchStatus::Done;
                    // 自动填入第一个模型（如果用户还没手动输入）
                    if state.model_name.is_empty() {
                        if let Some(first) = state.fetched_models.first() {
                            state.model_name = first.clone();
                        }
                    }
                }
                state.model_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {} // 还在检索中
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                state.model_fetch_status = ModelFetchStatus::Failed;
                state.model_rx = None;
            }
        }
    }
}

fn handle_edit(state: &mut SetupState, key: KeyCode, key_modifiers: KeyModifiers) {
    match state.focus {
        FocusField::BaseUrl => {
            match key {
                KeyCode::Char(c) => state.base_url.push(c),
                KeyCode::Backspace => { state.base_url.pop(); }
                KeyCode::Tab => {
                    // 离开 URL 字段时：同步默认模型 + 触发 API 检索
                    sync_default_model(state);
                    state.focus = FocusField::ModelName;
                    trigger_model_fetch(state);
                }
                KeyCode::Enter => if !state.base_url.is_empty() {
                    sync_default_model(state);
                    state.focus = FocusField::ModelName;
                    trigger_model_fetch(state);
                }
                _ => {}
            }
        }
        FocusField::ModelName => {
            match key {
                KeyCode::Char(c) => {
                    state.model_name.push(c);
                    // 手动输入时解除与列表的绑定（model_select_idx 不再代表当前选中）
                    state.model_select_idx = usize::MAX;
                }
                KeyCode::Backspace => { state.model_name.pop(); }
                KeyCode::Tab => {
                    if !state.fetched_models.is_empty() {
                        // M1 fix: 有候选列表时循环选择
                        let next = if state.model_select_idx >= state.fetched_models.len() {
                            0
                        } else {
                            (state.model_select_idx + 1) % state.fetched_models.len()
                        };
                        state.model_select_idx = next;
                        state.model_name = state.fetched_models[next].clone();
                    } else {
                        // M1 fix: 无候选时直接切换焦点，不卡死
                        state.focus = FocusField::ApiKey;
                    }
                }
                KeyCode::Enter => state.focus = FocusField::ApiKey,
                _ => {}
            }
        }
        FocusField::ApiKey => {
            match key {
                // L5 fix: Ctrl+H 切换显示/隐藏 API Key
                KeyCode::Char('h') if key_modifiers.contains(KeyModifiers::CONTROL) => {
                    state.show_api_key = !state.show_api_key;
                }
                KeyCode::Char(c) => state.api_key.push(c),
                KeyCode::Backspace => { state.api_key.pop(); }
                KeyCode::Tab => {
                    state.show_api_key = false;
                    // Key 填完后如果还没检索过，自动触发
                    if state.model_fetch_status == ModelFetchStatus::Idle && !state.api_key.is_empty() {
                        trigger_model_fetch(state);
                    }
                    state.focus = FocusField::ContextWindow;
                }
                KeyCode::Enter => if state.is_all_filled() { state.exit = true; }
                _ => {}
            }
        }
        FocusField::ContextWindow => {
            match key {
                KeyCode::Char(c) => state.context_window.push(c),
                KeyCode::Backspace => { state.context_window.pop(); }
                KeyCode::Tab | KeyCode::Enter => state.focus = FocusField::ContextWindowUse,
                _ => {}
            }
        }
        FocusField::ContextWindowUse => {
            match key {
                KeyCode::Char(c) => state.context_window_use.push(c),
                KeyCode::Backspace => { state.context_window_use.pop(); }
                KeyCode::Tab => state.focus = FocusField::BaseUrl,
                KeyCode::Enter => if state.is_all_filled() { state.exit = true; }
                _ => {}
            }
        }
    }
}

/// 运行首次配置向导（含免责声明）
///
/// 返回 true 表示配置完成，false 表示用户跳过或退出
pub fn run_setup(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
) -> io::Result<bool> {
    let mut state = SetupState::new();

    loop {
        // 轮询异步模型检索结果
        poll_model_fetch(&mut state);

        terminal.draw(|f| render_setup(f, &state))?;

        if state.exit {
            break;
        }

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                        return Ok(false);
                    }
                    if key.code == KeyCode::Esc {
                        state.skip = true;
                        state.exit = true;
                        continue;
                    }
                    handle_edit(&mut state, key.code, key.modifiers);
                }
            }
        }
    }

    if state.skip {
        return Ok(false);
    }

    // Enter 同时接受条款 + 保存配置
    // V13: 终页 bg / 反馈色由硬 RGB 改为主题感知（success/error 语义 + 主题 bg）
    let final_theme = setup_theme();
    match save_config(&state) {
        Ok(()) => {
            accept_disclaimer();
            let _ = terminal.draw(|f| {
                let area = f.area();
                Block::default()
                    .style(Style::default().bg(final_theme.bg))
                    .render(area, f.buffer_mut());
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        " ✓ 配置已保存，正在启动...",
                        final_theme.semantic_style(crate::tui::theme::SemanticIntent::Success, crate::tui::theme::Strength::Strong),
                    ))).alignment(Alignment::Center),
                    area,
                );
            });
            std::thread::sleep(Duration::from_millis(800));
            Ok(true)
        }
        Err(e) => {
            let _ = terminal.draw(|f| {
                let area = f.area();
                Block::default()
                    .style(Style::default().bg(final_theme.bg))
                    .render(area, f.buffer_mut());
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        format!(" ✗ 保存失败: {e}"),
                        Style::default().fg(final_theme.error),
                    ))).alignment(Alignment::Center),
                    area,
                );
            });
            std::thread::sleep(Duration::from_secs(2));
            Ok(false)
        }
    }
}
