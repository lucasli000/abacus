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
    Dashscope,   // 阿里通义千问
    Moonshot,    // 月之暗面 Kimi
    Zhipu,       // 智谱 GLM
    SiliconFlow, // 硅基流动（多模型聚合）
    Groq,        // Groq（快速推理）
    Generic,     // 其他 OpenAI 兼容 API
}

impl ProviderKind {
    fn detect(base_url: &str) -> Self {
        let lower = base_url.to_lowercase();
        // 按特征关键词匹配（优先级：精确域名 > 路径特征）
        if lower.contains("deepseek") { return ProviderKind::DeepSeek; }
        if lower.contains("anthropic") || lower.contains("claude") {
            return ProviderKind::Anthropic;
        }
        if lower.contains("dashscope") || lower.contains("aliyun") {
            return ProviderKind::Dashscope;
        }
        if lower.contains("moonshot") || lower.contains("kimi") {
            return ProviderKind::Moonshot;
        }
        if lower.contains("bigmodel") || lower.contains("zhipu") {
            return ProviderKind::Zhipu;
        }
        if lower.contains("siliconflow") { return ProviderKind::SiliconFlow; }
        if lower.contains("groq") { return ProviderKind::Groq; }
        if lower.contains("openai") { return ProviderKind::OpenAI; }
        // Fallback: 有 /vN 路径特征 → Generic OpenAI-compatible
        // 无任何特征 → 也当作 Generic（绝大多数新服务都是 OpenAI 兼容）
        ProviderKind::Generic
    }

    fn label(&self) -> &'static str {
        match self {
            ProviderKind::DeepSeek => "DeepSeek API",
            ProviderKind::OpenAI => "OpenAI API",
            ProviderKind::Anthropic => "Anthropic API",
            ProviderKind::Dashscope => "通义千问 (Dashscope)",
            ProviderKind::Moonshot => "Moonshot (Kimi)",
            ProviderKind::Zhipu => "智谱 (GLM)",
            ProviderKind::SiliconFlow => "SiliconFlow",
            ProviderKind::Groq => "Groq",
            ProviderKind::Generic => "OpenAI Compatible",
        }
    }

    fn config_prefix(&self) -> &str {
        match self {
            ProviderKind::DeepSeek => "deepseek",
            ProviderKind::OpenAI => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Dashscope => "dashscope",
            ProviderKind::Moonshot => "moonshot",
            ProviderKind::Zhipu => "zhipu",
            ProviderKind::SiliconFlow => "siliconflow",
            ProviderKind::Groq => "groq",
            ProviderKind::Generic => "openai",
        }
    }

    fn default_model(&self) -> &str {
        match self {
            ProviderKind::DeepSeek => "deepseek-v4-flash",
            ProviderKind::OpenAI => "gpt-4o",
            ProviderKind::Anthropic => "claude-sonnet-4",
            ProviderKind::Dashscope => "qwen-max",
            ProviderKind::Moonshot => "moonshot-v1-128k",
            ProviderKind::Zhipu => "glm-4-flash",
            ProviderKind::SiliconFlow => "deepseek-v4-flash",
            ProviderKind::Groq => "llama-3.3-70b-versatile",
            ProviderKind::Generic => "gpt-4o", // 通用兜底
        }
    }

    fn is_openai_compatible(&self) -> bool {
        // Anthropic 使用独有协议，其余均为 OpenAI 兼容
        !matches!(self, ProviderKind::Anthropic)
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
    /// 模型检索状态
    model_fetch_status: ModelFetchStatus,
    /// 当前在 fetched_models 中的选中 index（Tab 循环）
    model_select_idx: usize,
    /// 异步检索结果接收器
    model_rx: Option<std::sync::mpsc::Receiver<Vec<String>>>,
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
            model_fetch_status: ModelFetchStatus::Idle,
            model_select_idx: 0,
            model_rx: None,
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
        if content.contains("api_key") && !content.contains("api_key: \"\"") {
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

fn save_config(state: &SetupState) -> Result<(), String> {
    let provider = state.provider();
    let base_url = if state.base_url.is_empty() {
        "https://api.openai.com"
    } else {
        // Strip trailing /v1, /v2 etc. — providers append their own path
        state.base_url.trim_end_matches("/v1")
            .trim_end_matches("/v2")
            .trim_end_matches("/v3")
            .trim_end_matches("/v4")
            .trim()
    };

    let resolved_model = if state.model_name.is_empty() {
        provider.default_model()
    } else {
        &state.model_name
    };

    let yaml = if provider.is_openai_compatible() {
        // DeepSeek / OpenAI-compatible: write generic keys that engine_init reads
        format!(
            r#"# Abacus 配置（由 TUI 首次配置向导生成）
llm:
  api_key: "{}"
  base_url: "{}"
  temperature: 0.7
  max_tokens: 4096
  top_p: 0.95

core:
  default_model: "{}"
  stream: true
"#,
            state.api_key, base_url, resolved_model,
        )
    } else {
        // Anthropic: write provider-specific keys
        format!(
            r#"# Abacus 配置（由 TUI 首次配置向导生成）
llm:
  anthropic_api_key: "{}"
  anthropic_base_url: "{}"
  temperature: 0.7
  max_tokens: 4096
  top_p: 0.95

core:
  default_model: "{}"
  stream: true
"#,
            state.api_key, base_url, resolved_model,
        )
    };

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

    let cw = (area.width as f64 * 0.6).max(50.0).min(66.0) as u16;
    let ch = (area.height as f64 * 0.85).max(22.0).min(30.0) as u16;
    let cx = (area.width - cw) / 2;
    let cy = (area.height - ch) / 2;
    let card = Rect::new(cx, cy, cw, ch);

    let block = Block::default()
        .title(" 首次配置 ")
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(theme.gold));
    let inner = block.inner(card);
    block.render(card, f.buffer_mut());

    // 分区: 条款 | URL | 推荐 | Key | Model | 提示
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),   // 0 条款标题
            Constraint::Min(3),      // 1 条款内容
            Constraint::Length(1),   // 2 gap
            Constraint::Length(5),   // 3 API URL
            Constraint::Length(1),   // 4 DeepSeek 推荐
            Constraint::Length(1),   // 5 gap
            Constraint::Length(5),   // 6 API Key
            Constraint::Length(1),   // 7 gap
            Constraint::Length(3),   // 8 Model Name
            Constraint::Length(2),   // 9 底部提示
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
        ModelFetchStatus::Failed => " (检索失败，使用默认)",
        ModelFetchStatus::Idle => "",
    };
    let model_count = if !state.fetched_models.is_empty() {
        format!(" [{}/{}]", state.model_select_idx + 1, state.fetched_models.len())
    } else {
        String::new()
    };
    let model_title = format!("{}Model (Tab 循环){}{}", model_focus, model_count, model_status);
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

    // ── 底部提示 ──
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                " Tab 切换 · Enter 确认 · Esc 退出 · Key Tab=显示/隐藏",
                Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
            )),
            Line::from(Span::styled(
                " Enter 即表示同意使用条款",
                Style::default().fg(theme.border).add_modifier(Modifier::DIM),
            )),
        ]),
        parts[9],
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
        ProviderKind::SiliconFlow, ProviderKind::Groq, ProviderKind::Generic,
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
    let (tx, rx) = std::sync::mpsc::channel();
    state.model_rx = Some(rx);

    let base_url = state.base_url.clone();
    let api_key = state.api_key.clone();

    std::thread::spawn(move || {
        let models = fetch_model_list_sync(&base_url, &api_key);
        let _ = tx.send(models);
    });
}

/// 同步 HTTP 请求模型列表（在子线程中执行）
fn fetch_model_list_sync(base_url: &str, api_key: &str) -> Vec<String> {
    // 构建 URL: {base_url}/models
    let url = if base_url.ends_with('/') {
        format!("{}models", base_url)
    } else {
        format!("{}/models", base_url)
    };

    // 使用 ureq（同步 HTTP）— 如果没有 ureq，用 std::process::Command 调 curl
    // 这里用 std::net 最小实现，避免额外依赖
    match std::process::Command::new("curl")
        .args(["-s", "-H", &format!("Authorization: Bearer {}", api_key), &url])
        .output()
    {
        Ok(output) => {
            if !output.status.success() {
                return Vec::new();
            }
            let body = String::from_utf8_lossy(&output.stdout);
            // 解析 OpenAI 格式响应: {"data": [{"id": "model-name"}, ...]}
            parse_models_response(&body)
        }
        Err(_) => Vec::new(),
    }
}

/// 解析 /models API 响应（OpenAI 兼容格式）
fn parse_models_response(json: &str) -> Vec<String> {
    // SU7: 删除"简单字符串切分"的死循环占位（已被 serde_json 路径完全取代）
    let mut models = Vec::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
        if let Some(data) = v.get("data").and_then(|d| d.as_array()) {
            for item in data {
                if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                    // 过滤：只保留对话类模型（排除 embedding/tts/whisper）
                    let lower = id.to_lowercase();
                    if lower.contains("embed") || lower.contains("tts")
                        || lower.contains("whisper") || lower.contains("dall") {
                        continue;
                    }
                    models.push(id.to_string());
                }
            }
        }
    }
    // 按名称排序（新模型通常名字靠后）
    models.sort();
    models.reverse(); // 最新模型排前面
    models
}

/// 检查异步检索结果（非阻塞，每帧调用）
fn poll_model_fetch(state: &mut SetupState) {
    if let Some(ref rx) = state.model_rx {
        match rx.try_recv() {
            Ok(models) => {
                if models.is_empty() {
                    state.model_fetch_status = ModelFetchStatus::Failed;
                } else {
                    state.fetched_models = models;
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

fn handle_edit(state: &mut SetupState, key: KeyCode) {
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
                KeyCode::Char(c) => state.model_name.push(c),
                KeyCode::Backspace => { state.model_name.pop(); }
                KeyCode::Tab => {
                    // Tab: 在检索到的模型列表中循环选择
                    let candidates = if !state.fetched_models.is_empty() {
                        &state.fetched_models
                    } else {
                        // 没有检索结果时用静态推荐
                        return; // 跳到下一个字段
                    };
                    if !candidates.is_empty() {
                        state.model_select_idx = (state.model_select_idx + 1) % candidates.len();
                        state.model_name = candidates[state.model_select_idx].clone();
                    } else {
                        state.focus = FocusField::ApiKey;
                    }
                }
                KeyCode::Enter => state.focus = FocusField::ApiKey,
                _ => {}
            }
        }
        FocusField::ApiKey => {
            match key {
                KeyCode::Char(c) => state.api_key.push(c),
                KeyCode::Backspace => { state.api_key.pop(); }
                KeyCode::Tab => {
                    state.show_api_key = false;
                    state.focus = FocusField::BaseUrl;
                    // Key 输入完后如果还没检索过，自动触发
                    if state.model_fetch_status == ModelFetchStatus::Idle && !state.api_key.is_empty() {
                        trigger_model_fetch(state);
                    }
                }
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
                    handle_edit(&mut state, key.code);
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
