//! meeting_cache — Meeting 会议结论本地缓存
//!
//! ## 设计意图
//! Meeting 会议产物（结论、行动项、专家意见）不应随 session 消亡。
//! 本模块将每次 Meeting → Clarify 转移时的结论自动序列化到磁盘，
//! 供用户在未来 session 中检索和复用。
//!
//! ## 目录结构
//! ```text
//! ~/.abacus/meetings/
//!   2026-05-26-14-30-auth-refactor.md
//!   2026-05-26-15-45-redis-vs-postgres.md
//!   ...
//! ```
//!
//! ## 文件格式（YAML frontmatter + Markdown body）
//! ```text
//! ---
//! id: "2026-05-26-14-30-auth-refactor"
//! topic: "auth 模块重构"
//! meeting_type: deliberative
//! verdict: ~
//! date: "2026-05-26T14:30:00Z"
//! cwd: "/Users/admin/myproject"
//! specialists: ["security", "architecture"]
//! action_items: []
//! unresolved: []
//! ---
//! # 会议结论
//! ...Markdown 正文...
//! ```
//!
//! ## 引用关系
//! - 写: `slash_commands::try_switch_mode` — Meeting→Clarify 时自动调用 `quick_save`
//! - 读: `/meeting list` → `list_records`
//!       `/meeting load <id>` → `load_record`
//!
//! ## 生命周期
//! - 文件写入后永久存在（无 TTL）
//! - 由用户手动管理（删除/归档）
//! - 跨 session、跨 project 可检索

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ─── 目录定位 ─────────────────────────────────────────────────────────────

/// 会议缓存目录：`~/.abacus/meetings/`
///
/// 引用关系: 所有读写操作的路径根
/// 生命周期: 进程内多次调用，每次重新计算（轻量）
pub fn meetings_dir() -> PathBuf {
    abacus_core::paths::meetings_dir()
}

/// 确保缓存目录存在（首次调用时创建）
///
/// 生命周期: 创建后目录持久存在
fn ensure_meetings_dir() -> std::io::Result<()> {
    let dir = meetings_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(())
}

// ─── 核心类型 ─────────────────────────────────────────────────────────────

/// 会议类型
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingKind {
    /// 开放讨论 / 决策型（用户提问，专家各持观点）
    Deliberative,
    /// 代码 / 文件审查型（Plan+Team 执行结果的质检）
    Audit,
}

impl MeetingKind {
    pub fn display_zh(&self) -> &str {
        match self {
            MeetingKind::Deliberative => "讨论",
            MeetingKind::Audit => "审查",
        }
    }
}

/// 行动项（持久化格式）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordActionItem {
    pub text: String,
    pub done: bool,
}

/// Meeting 记录 — 磁盘持久化的会议产物
///
/// ## 引用关系
/// - 生产者: `save_record` / `quick_save`
/// - 消费者: `list_records` / `load_record` / TUI 命令 `/meeting list|load`
///
/// ## 生命周期
/// - 创建: Meeting→Clarify 模式转移时
/// - 存活: 永久（文件系统）
/// - 销毁: 用户手动删除文件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingRecord {
    /// 唯一 ID（等于文件名前缀）e.g. `"2026-05-26-14-30-auth-refactor"`
    pub id: String,
    pub meeting_type: MeetingKind,
    /// 会议议题（用于 list 展示和检索）
    pub topic: String,
    /// 裁决摘要: `"pass"` / `"needs_work"` / `"block"` / `"decided"` / `None`
    pub verdict: Option<String>,
    /// 会议结束时间（UTC）
    pub date: DateTime<Utc>,
    /// 写入时的工作目录（用于 `--project` 过滤）
    pub cwd: String,
    /// 参与专家名称列表
    pub specialists: Vec<String>,
    /// 行动项列表（可选）
    pub action_items: Vec<RecordActionItem>,
    /// 未达成共识的问题
    pub unresolved: Vec<String>,
    /// 会议结论 Markdown 正文（不放入 frontmatter）
    #[serde(skip)]
    pub body: String,
}

// ─── 序列化 / 反序列化 ───────────────────────────────────────────────────

/// frontmatter only（不含 body，用于 YAML 序列化）
#[derive(Serialize, Deserialize)]
struct Frontmatter {
    id: String,
    meeting_type: MeetingKind,
    topic: String,
    verdict: Option<String>,
    date: DateTime<Utc>,
    cwd: String,
    specialists: Vec<String>,
    action_items: Vec<RecordActionItem>,
    unresolved: Vec<String>,
}

/// 将 `MeetingRecord` 序列化为 YAML frontmatter + Markdown body 格式
fn to_file_content(record: &MeetingRecord) -> String {
    let fm = Frontmatter {
        id: record.id.clone(),
        meeting_type: record.meeting_type.clone(),
        topic: record.topic.clone(),
        verdict: record.verdict.clone(),
        date: record.date,
        cwd: record.cwd.clone(),
        specialists: record.specialists.clone(),
        action_items: record.action_items.clone(),
        unresolved: record.unresolved.clone(),
    };
    let yaml = serde_yaml::to_string(&fm).unwrap_or_default();
    format!("---\n{}---\n{}", yaml, record.body)
}

/// 从文件内容解析 `MeetingRecord`（frontmatter + body 分割）
fn from_file_content(content: &str) -> Option<MeetingRecord> {
    // 期望格式: "---\n{yaml}\n---\n{body}"
    let after_opening = content.strip_prefix("---\n")?;
    let (fm_str, body) = after_opening.split_once("\n---\n")?;
    let fm: Frontmatter = serde_yaml::from_str(fm_str).ok()?;
    Some(MeetingRecord {
        id: fm.id,
        meeting_type: fm.meeting_type,
        topic: fm.topic,
        verdict: fm.verdict,
        date: fm.date,
        cwd: fm.cwd,
        specialists: fm.specialists,
        action_items: fm.action_items,
        unresolved: fm.unresolved,
        body: body.to_string(),
    })
}

// ─── 文件名生成 ───────────────────────────────────────────────────────────

/// 将 topic 转换为文件名安全 slug（取前 4 词，去除非字母数字字符）
fn slugify(s: &str) -> String {
    // 对中文按字符拆分，对英文按单词拆分
    let ascii_words: Vec<&str> = s.split_whitespace().collect();
    let raw = if ascii_words.len() > 1 {
        // 有空格 = 多词（英文或带空格中文）
        ascii_words.into_iter().take(4).collect::<Vec<_>>().join("-")
    } else {
        // 无空格 = CJK 连续文字，取前 8 字符
        s.chars().take(8).collect::<String>()
    };
    raw.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-')
        .collect::<String>()
        .to_lowercase()
}

/// 生成记录 ID: `{YYYY-MM-DD}-{HH-MM}-{slug}`
fn make_id(date: &DateTime<Utc>, topic: &str) -> String {
    format!(
        "{}-{}-{}",
        date.format("%Y-%m-%d"),
        date.format("%H-%M"),
        slugify(topic)
    )
}

// ─── 公开 API ─────────────────────────────────────────────────────────────

/// 保存 `MeetingRecord` 到 `~/.abacus/meetings/{id}.md`
///
/// 若 `record.id` 为空，自动根据 date+topic 生成。
///
/// 引用关系:
///   消费方: `quick_save` (主调用路径)
///   生命周期: 调用后文件持久存在于磁盘
pub fn save_record(record: &mut MeetingRecord) -> std::io::Result<PathBuf> {
    ensure_meetings_dir()?;
    if record.id.is_empty() {
        record.id = make_id(&record.date, &record.topic);
    }
    let path = meetings_dir().join(format!("{}.md", record.id));
    std::fs::write(&path, to_file_content(record))?;
    Ok(path)
}

/// 列出所有会议记录，按时间**倒序**排列
///
/// - `cwd_filter`: 若提供，仅返回 `cwd` 包含该字符串的记录
///
/// 引用关系: `/meeting list` 命令
pub fn list_records(cwd_filter: Option<&str>) -> Vec<MeetingRecord> {
    let dir = meetings_dir();
    if !dir.exists() {
        return vec![];
    }
    let mut records: Vec<MeetingRecord> = std::fs::read_dir(&dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()?.to_str()? != "md" {
                return None;
            }
            let content = std::fs::read_to_string(&path).ok()?;
            from_file_content(&content)
        })
        .filter(|r| cwd_filter.map(|f| r.cwd.contains(f)).unwrap_or(true))
        .collect();
    records.sort_by(|a, b| b.date.cmp(&a.date));
    records
}

/// 根据 ID 加载特定会议记录
///
/// 引用关系: `/meeting load <id>` 命令
pub fn load_record(id: &str) -> Option<MeetingRecord> {
    let path = meetings_dir().join(format!("{}.md", id));
    let content = std::fs::read_to_string(path).ok()?;
    from_file_content(&content)
}

/// 从会议结论文本中提取结构化行动项（零 LLM 开销，纯文本解析）
///
/// ## 匹配模式（优先级由高到低）
/// 1. Markdown checkbox: `- [ ] xxx` / `- [x] xxx`
/// 2. Numbered imperative: `1. 实现...` / `2. 修复...`
/// 3. Heading-scoped bullets: "行动项:" / "Action Items:" 标题下的 `- xxx`
/// 4. Bullet with action verb: `- 实现/修复/添加/重构/测试/部署...`
///
/// ## 引用关系
/// - 消费方: `try_switch_mode` (Meeting→Clarify) 提取后传入 `quick_save` + 设置 `pending_meeting_execution`
///
/// ## 设计
/// - 返回空 Vec 表示无结构化行动项（调用方应跳过执行提案）
/// - 不做去重（同一行不会被多模式重复匹配，但不同行内容可能语义相似）
pub fn extract_action_items_from_text(text: &str) -> Vec<RecordActionItem> {
    let mut items: Vec<RecordActionItem> = Vec::new();
    let mut in_action_section = false;

    for line in text.lines() {
        let trimmed = line.trim();

        // Pattern 1: Markdown checkbox — `- [ ] xxx` / `- [x] xxx` / `* [ ] xxx`
        if let Some(rest) = trimmed.strip_prefix("- [") {
            if let Some(content) = rest.strip_prefix("] ") {
                items.push(RecordActionItem { text: content.trim().to_string(), done: false });
                continue;
            }
            if let Some(content) = rest.strip_prefix("x] ") {
                items.push(RecordActionItem { text: content.trim().to_string(), done: true });
                continue;
            }
        }

        // Pattern 2: Numbered imperative — `1. xxx` / `2. xxx` (1-99)
        if let Some(dot_pos) = trimmed.find(". ") {
            let prefix = &trimmed[..dot_pos];
            if prefix.len() <= 2 && prefix.chars().all(|c| c.is_ascii_digit()) {
                let content = trimmed[dot_pos + 2..].trim();
                if !content.is_empty() && !items.iter().any(|i| i.text == content) {
                    items.push(RecordActionItem { text: content.to_string(), done: false });
                    continue;
                }
            }
        }

        // Pattern 3: Section header detection
        let lower = trimmed.to_lowercase();
        if lower.starts_with("行动项") || lower.starts_with("action item")
            || lower.starts_with("## 行动") || lower.starts_with("## action")
            || lower.starts_with("### 行动") || lower.starts_with("### action")
        {
            in_action_section = true;
            continue;
        }
        // Exit section on next heading or empty line after items
        if in_action_section {
            if trimmed.starts_with('#') || (trimmed.is_empty() && !items.is_empty()) {
                in_action_section = false;
                continue;
            }
            // Bullets inside action section
            if let Some(content) = trimmed.strip_prefix("- ") {
                if !content.is_empty() {
                    items.push(RecordActionItem { text: content.trim().to_string(), done: false });
                    continue;
                }
            }
        }

        // Pattern 4: Bullet with Chinese action verb (only if not already captured)
        if let Some(content) = trimmed.strip_prefix("- ") {
            let action_verbs = ["实现", "修复", "添加", "重构", "测试", "部署", "优化", "删除", "更新", "迁移", "创建", "配置"];
            if action_verbs.iter().any(|v| content.starts_with(v)) {
                if !items.iter().any(|i| i.text == content) {
                    items.push(RecordActionItem { text: content.to_string(), done: false });
                }
            }
        }
    }

    items
}

/// 快捷保存：Meeting→Clarify 转移时直接调用
///
/// ## 参数
/// - `topic`: 会议议题（通常取 Meeting 首条消息或 ClarifyBrief 摘要）
/// - `conclusion_body`: 会议结论 Markdown 正文
/// - `specialists`: 参与专家名称列表
/// - `cwd`: 当前工作目录
/// - `action_items`: 2026-05-27 新增 — 从结论中提取的行动项（空 Vec = 无结构化行动项）
///
/// ## 错误处理
/// IO 错误不 panic，调用方收到 Err 后 toast 提示即可
///
/// 引用关系:
///   消费方: `slash_commands::try_switch_mode` (Meeting→Clarify 分支)
pub fn quick_save(
    topic: &str,
    conclusion_body: &str,
    specialists: Vec<String>,
    cwd: &str,
    action_items: Vec<RecordActionItem>,
) -> std::io::Result<PathBuf> {
    let now = Utc::now();
    let mut record = MeetingRecord {
        id: make_id(&now, topic),
        meeting_type: MeetingKind::Deliberative,
        topic: topic.to_string(),
        verdict: None,
        date: now,
        cwd: cwd.to_string(),
        specialists,
        action_items,
        unresolved: vec![],
        body: conclusion_body.to_string(),
    };
    save_record(&mut record)
}

// ─── 展示辅助 ─────────────────────────────────────────────────────────────

/// 将 `MeetingRecord` 格式化为 list 条目（单行）
///
/// 示例: `[1] 🎙 05-26 14:30 讨论 — auth 模块重构 · 2/3 行动项`
pub fn format_list_entry(record: &MeetingRecord, index: usize) -> String {
    let verdict_icon = match record.verdict.as_deref() {
        Some("pass") => "✅",
        Some("needs_work") => "🟡",
        Some("block") => "🔴",
        Some("decided") => "📌",
        _ => "🎙",
    };
    let date_str = record.date.format("%m-%d %H:%M").to_string();
    let done = record.action_items.iter().filter(|a| a.done).count();
    let total = record.action_items.len();
    let action_str = if total > 0 {
        format!(" · {}/{} 行动项", done, total)
    } else {
        String::new()
    };
    let unresolved_str = if !record.unresolved.is_empty() {
        format!(" · {} 待决", record.unresolved.len())
    } else {
        String::new()
    };
    format!(
        "[{}] {} {} {} — {}{}{}",
        index + 1,
        verdict_icon,
        date_str,
        record.meeting_type.display_zh(),
        truncate(&record.topic, 28),
        action_str,
        unresolved_str,
    )
}

/// 将 `MeetingRecord` 格式化为 load 注入的摘要（注入 session context 用）
///
/// 引用关系: `/meeting load` → 注入 AppState.messages 作为 System 消息
pub fn format_for_injection(record: &MeetingRecord) -> String {
    let mut buf = String::new();
    buf.push_str(&format!(
        "[会议记录 — {}] 议题: {}\n时间: {}\n",
        record.meeting_type.display_zh(),
        record.topic,
        record.date.format("%Y-%m-%d %H:%M UTC"),
    ));
    if !record.specialists.is_empty() {
        buf.push_str(&format!("专家: {}\n", record.specialists.join(" / ")));
    }
    if let Some(v) = &record.verdict {
        buf.push_str(&format!("裁决: {}\n", v));
    }
    if !record.action_items.is_empty() {
        buf.push_str("\n行动项:\n");
        for item in &record.action_items {
            let check = if item.done { "x" } else { " " };
            buf.push_str(&format!("- [{}] {}\n", check, item.text));
        }
    }
    if !record.unresolved.is_empty() {
        buf.push_str("\n待决问题:\n");
        for u in &record.unresolved {
            buf.push_str(&format!("- {}\n", u));
        }
    }
    if !record.body.trim().is_empty() {
        buf.push_str("\n---\n");
        buf.push_str(&record.body);
    }
    buf
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > max {
        format!("{}…", chars[..max].iter().collect::<String>())
    } else {
        s.to_string()
    }
}

// ─── 测试 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_record() -> MeetingRecord {
        MeetingRecord {
            id: "2026-05-26-14-30-test-meeting".to_string(),
            meeting_type: MeetingKind::Deliberative,
            topic: "test meeting topic".to_string(),
            verdict: Some("decided".to_string()),
            date: DateTime::parse_from_rfc3339("2026-05-26T14:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
            cwd: "/tmp/test".to_string(),
            specialists: vec!["security".to_string(), "arch".to_string()],
            action_items: vec![
                RecordActionItem { text: "fix auth".to_string(), done: true },
                RecordActionItem { text: "review api".to_string(), done: false },
            ],
            unresolved: vec!["接口设计".to_string()],
            body: "# 结论\n\n使用方案 A。".to_string(),
        }
    }

    #[test]
    fn round_trip_serialization() {
        let record = make_test_record();
        let content = to_file_content(&record);
        let parsed = from_file_content(&content).expect("parse failed");
        assert_eq!(parsed.id, record.id);
        assert_eq!(parsed.topic, record.topic);
        assert_eq!(parsed.verdict, record.verdict);
        assert_eq!(parsed.cwd, record.cwd);
        assert_eq!(parsed.specialists, record.specialists);
        assert_eq!(parsed.action_items.len(), record.action_items.len());
        assert_eq!(parsed.unresolved, record.unresolved);
        assert_eq!(parsed.body.trim(), record.body.trim());
    }

    #[test]
    fn slugify_english() {
        assert_eq!(slugify("auth module refactor review"), "auth-module-refactor-review");
    }

    #[test]
    fn slugify_cjk() {
        let s = slugify("auth模块重构");
        assert!(!s.is_empty());
    }

    #[test]
    fn make_id_format() {
        let dt = DateTime::parse_from_rfc3339("2026-05-26T14:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let id = make_id(&dt, "redis vs postgres");
        assert!(id.starts_with("2026-05-26-14-30"));
    }

    #[test]
    fn format_list_entry_renders() {
        let record = make_test_record();
        let line = format_list_entry(&record, 0);
        assert!(line.contains("[1]"));
        assert!(line.contains("讨论"));
        assert!(line.contains("1/2 行动项"));
        assert!(line.contains("1 待决"));
    }

    #[test]
    fn format_for_injection_contains_key_info() {
        let record = make_test_record();
        let injected = format_for_injection(&record);
        assert!(injected.contains("test meeting topic"));
        assert!(injected.contains("security"));
        assert!(injected.contains("fix auth"));
        assert!(injected.contains("接口设计"));
    }

    // ─── 2026-05-27: extract_action_items_from_text 测试 ──────────────────────

    #[test]
    fn extract_markdown_checkboxes() {
        let text = "讨论完毕：\n- [ ] 实现登录功能\n- [x] 修复 bug\n- [ ] 部署上线";
        let items = extract_action_items_from_text(text);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].text, "实现登录功能");
        assert!(!items[0].done);
        assert_eq!(items[1].text, "修复 bug");
        assert!(items[1].done);
    }

    #[test]
    fn extract_numbered_list() {
        let text = "结论：\n1. 重构 auth 模块\n2. 添加测试覆盖\n3. 部署到 staging";
        let items = extract_action_items_from_text(text);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].text, "重构 auth 模块");
    }

    #[test]
    fn extract_action_section() {
        let text = "# 会议结论\n讨论很好\n\n## 行动项\n- 优化数据库查询\n- 添加缓存层\n\n## 其他";
        let items = extract_action_items_from_text(text);
        assert!(items.len() >= 2);
        assert!(items.iter().any(|i| i.text == "优化数据库查询"));
        assert!(items.iter().any(|i| i.text == "添加缓存层"));
    }

    #[test]
    fn extract_action_verbs() {
        let text = "建议如下：\n- 实现用户认证\n- 这是普通描述不是行动项\n- 部署到生产环境";
        let items = extract_action_items_from_text(text);
        assert!(items.iter().any(|i| i.text == "实现用户认证"));
        assert!(items.iter().any(|i| i.text == "部署到生产环境"));
        assert!(!items.iter().any(|i| i.text.contains("普通描述")));
    }

    #[test]
    fn extract_empty_for_no_structure() {
        let text = "这是一段没有任何结构化行动项的纯文本讨论。大家同意了方案 A。";
        let items = extract_action_items_from_text(text);
        assert!(items.is_empty());
    }
}
