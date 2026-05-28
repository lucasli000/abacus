//! 内置场景 Skill 定义
//!
//! 基于工具特征矩阵 + 场景分解系统构建，7 个覆盖核心检索场景。
//! 设计原则：目标达成 > 成本/性能；Discover → Locate → Extract 三阶段。
//!
//! ## 引用关系
//! - 被 `CoreLoop::load_builtin_skills()` 调用（core/mod.rs）
//! - 消费 SkillEngine::register_skill() + CoreLoop::load_skill()
//!
//! ## 生命周期
//! 进程启动后注册一次，随 SkillEngine Arc 存活直到进程退出

use abacus_types::{SkillDef, SkillId, SkillStep, SkillTriggers};
use serde_json::json;

/// 返回所有内置 Skill 定义
///
/// 调用方：CoreLoop::load_builtin_skills()
pub fn builtin_skill_defs() -> Vec<SkillDef> {
    vec![
        skill_search_file(),
        skill_search_code(),
        skill_web_research(),
        skill_knowledge(),
        skill_data_query(),
        skill_diagnose(),
        skill_config_find(),
    ]
}

// ────────────────────────────────────────────────────────────
// skill.search_file — 文件搜索 + 内容定位
// ────────────────────────────────────────────────────────────

fn skill_search_file() -> SkillDef {
    SkillDef {
        id: SkillId("search_file".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec!["找文件".into(), "搜索文件".into(), "查找文件".into(), "find file".into()],
            regex: vec![r"(?i)找.*文件".into(), r"(?i)search.*file".into()],
            domain: vec!["file".into(), "search".into(), "fs".into()],
        },
        workflow: vec![
            SkillStep {
                id: "discover".into(),
                description: "按 glob 模式发现候选文件（广撒网）".into(),
                tool: "fs_search".into(),
                params: json!({"pattern": "{{pattern}}", "path": "{{root}}"}),
                depends_on: None,
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "locate".into(),
                description: "在候选文件中按内容模式定位（缩窄范围）".into(),
                tool: "fs_grep".into(),
                params: json!({"pattern": "{{content_pattern}}", "path": "{{root}}", "mode": "fine"}),
                depends_on: Some(vec!["discover".into()]),
                // 只有明确有 content_pattern 且 discover 结果 > 1 时才执行
                condition: Some("content_pattern != null".into()),
                fallback: Some("discover".into()),
            },
            SkillStep {
                id: "extract".into(),
                description: "读取目标文件完整内容".into(),
                tool: "fs_read".into(),
                params: json!({"path": "{{locate.file}}"}),
                depends_on: Some(vec!["locate".into()]),
                condition: Some("locate.matches.len > 0".into()),
                fallback: None,
            },
        ],
        prompt: "用于在文件系统中按名称或内容查找文件，三阶段：glob 发现 → 内容定位 → 读取".into(),
        knowledge_refs: vec![],
        palace_tags: vec!["file_search".into(), "fs".into(), "discovery".into()],
        compound: true,
    }
}

// ────────────────────────────────────────────────────────────
// skill.search_code — 代码符号定位
// ────────────────────────────────────────────────────────────

fn skill_search_code() -> SkillDef {
    SkillDef {
        id: SkillId("search_code".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec!["找函数".into(), "找类".into(), "代码搜索".into(), "find function".into(), "find class".into()],
            regex: vec![r"(?i)找.*函数|找.*方法|find.*fn|find.*class".into()],
            domain: vec!["code".into(), "rust".into(), "python".into(), "search".into()],
        },
        workflow: vec![
            SkillStep {
                id: "discover".into(),
                description: "按扩展名发现候选源文件".into(),
                tool: "fs_search".into(),
                params: json!({"pattern": "{{ext_pattern}}", "path": "{{root}}"}),
                depends_on: None,
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "locate".into(),
                description: "在源文件中 grep 符号定义（粒度：行级）".into(),
                tool: "fs_grep".into(),
                params: json!({"pattern": "{{symbol}}", "path": "{{root}}", "mode": "fine", "context": 3}),
                depends_on: Some(vec!["discover".into()]),
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "extract".into(),
                description: "读取符号所在文件完整上下文".into(),
                tool: "fs_read".into(),
                params: json!({"path": "{{locate.matches[0].file}}"}),
                depends_on: Some(vec!["locate".into()]),
                condition: Some("locate.matches.len > 0".into()),
                fallback: None,
            },
        ],
        prompt: "在代码库中定位函数/类/符号，三阶段：文件发现 → 符号 grep → 读取上下文".into(),
        knowledge_refs: vec![],
        palace_tags: vec!["code_search".into(), "fs".into(), "symbol".into()],
        compound: true,
    }
}

// ────────────────────────────────────────────────────────────
// skill.web_research — 网络调研（搜索 + 摘要 + 深度抓取）
// ────────────────────────────────────────────────────────────

fn skill_web_research() -> SkillDef {
    SkillDef {
        id: SkillId("web_research".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec!["搜索网络".into(), "网络搜索".into(), "查文档".into(), "web search".into(), "research".into()],
            regex: vec![r"(?i)search.*web|web.*search|online.*search".into()],
            domain: vec!["web".into(), "research".into(), "internet".into()],
        },
        workflow: vec![
            SkillStep {
                id: "discover".into(),
                description: "搜索获取摘要列表".into(),
                tool: "web_search".into(),
                params: json!({"query": "{{query}}", "count": 10}),
                depends_on: None,
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "extract".into(),
                description: "抓取高质量结果页面提取可读文本".into(),
                tool: "web_search".into(),
                params: json!({"query": "{{query}}", "count": 5, "deep": true}),
                depends_on: Some(vec!["discover".into()]),
                // 仅当 discover 有 High 质量结果时执行深度抓取
                condition: Some("discover.results.len > 0".into()),
                fallback: Some("discover".into()),
            },
        ],
        prompt: "网络调研：搜索摘要后自动抓取高质量页面正文，适合文档查阅、最新信息检索".into(),
        knowledge_refs: vec![],
        palace_tags: vec!["web".into(), "research".into(), "fetch".into()],
        compound: true,
    }
}

// ────────────────────────────────────────────────────────────
// skill.knowledge — 知识库语义检索
// ────────────────────────────────────────────────────────────

fn skill_knowledge() -> SkillDef {
    SkillDef {
        id: SkillId("knowledge".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec!["知识库".into(), "检索知识".into(), "knowledge base".into(), "kb query".into()],
            regex: vec![r"(?i)knowledge.*base|kb.*search|知识.*检索".into()],
            domain: vec!["knowledge".into(), "kb".into(), "memory".into()],
        },
        workflow: vec![
            SkillStep {
                id: "broad".into(),
                description: "宽泛语义搜索知识库".into(),
                tool: "kb_query".into(),
                params: json!({"query": "{{query}}", "domain": "{{domain}}", "top_k": 10}),
                depends_on: None,
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "narrow".into(),
                description: "精化查询锁定最相关条目".into(),
                tool: "kb_query".into(),
                params: json!({"query": "{{refined_query}}", "domain": "{{domain}}", "top_k": 3}),
                depends_on: Some(vec!["broad".into()]),
                // 仅当 broad 结果不够精确（≥5条）时精化
                condition: Some("broad.results.len >= 5 && refined_query != null".into()),
                fallback: Some("broad".into()),
            },
        ],
        prompt: "知识库两阶段检索：宽泛语义搜索 → 精化锁定，适合领域知识查阅".into(),
        knowledge_refs: vec![],
        palace_tags: vec!["knowledge".into(), "kb".into(), "semantic".into()],
        compound: true,
    }
}

// ────────────────────────────────────────────────────────────
// skill.data_query — 结构化数据查询
// ────────────────────────────────────────────────────────────

fn skill_data_query() -> SkillDef {
    SkillDef {
        id: SkillId("data_query".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec!["查数据库".into(), "数据查询".into(), "database".into(), "sql query".into()],
            regex: vec![r"(?i)db.*query|database.*search|sql.*select".into()],
            domain: vec!["database".into(), "db".into(), "data".into()],
        },
        workflow: vec![
            SkillStep {
                id: "schema".into(),
                description: "获取表结构（了解可用字段）".into(),
                tool: "db_table_schema".into(),
                params: json!({"tableName": "{{table}}"}),
                depends_on: None,
                condition: Some("table != null".into()),
                fallback: None,
            },
            SkillStep {
                id: "coarse".into(),
                description: "粗查：统计符合条件的行数".into(),
                tool: "db_query".into(),
                params: json!({"sql": "SELECT COUNT(*) as cnt FROM {{table}} WHERE {{condition}}", "values": "{{params}}"}),
                depends_on: Some(vec!["schema".into()]),
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "fine".into(),
                description: "精查：获取完整数据行".into(),
                tool: "db_query".into(),
                params: json!({"sql": "SELECT * FROM {{table}} WHERE {{condition}} LIMIT {{limit}}", "values": "{{params}}"}),
                depends_on: Some(vec!["coarse".into()]),
                condition: Some("coarse.rows[0].cnt > 0".into()),
                fallback: None,
            },
        ],
        prompt: "数据库三阶段查询：schema 探查 → count 粗查 → full 精查，避免无效大查询".into(),
        knowledge_refs: vec![],
        palace_tags: vec!["database".into(), "db".into(), "query".into()],
        compound: true,
    }
}

// ────────────────────────────────────────────────────────────
// skill.diagnose — 系统/错误诊断
// ────────────────────────────────────────────────────────────

fn skill_diagnose() -> SkillDef {
    SkillDef {
        id: SkillId("diagnose".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec!["诊断".into(), "排查".into(), "错误日志".into(), "diagnose".into(), "debug".into()],
            regex: vec![r"(?i)diagnos|error.*log|log.*error|troubleshoot|排查".into()],
            domain: vec!["diagnose".into(), "debug".into(), "error".into(), "system".into()],
        },
        workflow: vec![
            SkillStep {
                id: "quick".into(),
                description: "快速系统状态检查".into(),
                tool: "bash_exec".into(),
                params: json!({"command": "{{status_cmd}}", "timeout": 10}),
                depends_on: None,
                condition: Some("status_cmd != null".into()),
                fallback: None,
            },
            SkillStep {
                id: "pattern".into(),
                description: "在日志/源码中 grep 错误模式".into(),
                tool: "fs_grep".into(),
                params: json!({"pattern": "{{error_pattern}}", "path": "{{log_path}}", "mode": "fine"}),
                depends_on: Some(vec!["quick".into()]),
                condition: Some("error_pattern != null".into()),
                fallback: None,
            },
            SkillStep {
                id: "read".into(),
                description: "读取问题上下文文件".into(),
                tool: "fs_read".into(),
                params: json!({"path": "{{pattern.matches[0].file}}"}),
                depends_on: Some(vec!["pattern".into()]),
                condition: Some("pattern.matches.len > 0".into()),
                fallback: None,
            },
        ],
        prompt: "系统错误诊断三阶段：快速状态检查 → 错误模式搜索 → 读取上下文".into(),
        knowledge_refs: vec![],
        palace_tags: vec!["diagnose".into(), "debug".into(), "error".into()],
        compound: true,
    }
}

// ────────────────────────────────────────────────────────────
// skill.config_find — 配置文件查找与读取
// ────────────────────────────────────────────────────────────

fn skill_config_find() -> SkillDef {
    SkillDef {
        id: SkillId("config_find".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec!["找配置".into(), "配置文件".into(), "config file".into(), "settings".into()],
            regex: vec![r"(?i)config.*file|find.*config|配置.*文件".into()],
            domain: vec!["config".into(), "settings".into(), "toml".into(), "yaml".into()],
        },
        workflow: vec![
            SkillStep {
                id: "search".into(),
                description: "搜索 yaml/toml/json/ini 配置文件".into(),
                tool: "fs_search".into(),
                params: json!({"pattern": "*.{yaml,toml,json,ini,conf,cfg}", "path": "{{root}}"}),
                depends_on: None,
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "read".into(),
                description: "读取匹配的配置文件内容".into(),
                tool: "fs_read".into(),
                params: json!({"path": "{{search.matches[0]}}"}),
                depends_on: Some(vec!["search".into()]),
                condition: Some("search.matches.len > 0".into()),
                fallback: None,
            },
        ],
        prompt: "配置文件两阶段：搜索 yaml/toml/json 等配置文件 → 读取内容，适合项目配置查阅".into(),
        knowledge_refs: vec![],
        palace_tags: vec!["config".into(), "settings".into(), "file".into()],
        compound: true,
    }
}
