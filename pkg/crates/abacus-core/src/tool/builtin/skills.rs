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
        // V42-B: 从 Toolchain 集成的高价值技能
        skill_code_quality_gate(),
        skill_systematic_debugging(),
        skill_verification(),
        skill_tdd(),
        skill_review_matrix(),
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
        template_params: Vec::new(),
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
        template_params: Vec::new(),
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
        template_params: Vec::new(),
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
        template_params: Vec::new(),
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
        template_params: Vec::new(),
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
        template_params: Vec::new(),
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
        template_params: Vec::new(),
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// V42-B: 从 Toolchain 集成的高价值技能
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

// ────────────────────────────────────────────────────────────
// skill.code_quality_gate — 代码质量三阶段审查门控
// 来源：Toolchain code-quality-gate
// ────────────────────────────────────────────────────────────

fn skill_code_quality_gate() -> SkillDef {
    SkillDef {
        id: SkillId("code_quality_gate".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec!["代码审查".into(), "质量检查".into(), "code review".into(), "quality gate".into(), "审查代码".into()],
            regex: vec![r"(?i)code.*review|quality.*gate|审查.*代码|代码.*质量".into()],
            domain: vec!["code".into(), "review".into(), "quality".into()],
        },
        workflow: vec![
            SkillStep {
                id: "security_scan".into(),
                description: "安全扫描：检查注入、越权、敏感信息泄漏".into(),
                tool: "fs_grep".into(),
                params: json!({"pattern": "password|secret|api_key|token|eval|exec|rm -rf|DROP TABLE", "path": "{{root}}", "mode": "fine"}),
                depends_on: None,
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "style_check".into(),
                description: "代码风格检查：长度、复杂度、命名".into(),
                tool: "fs_grep".into(),
                params: json!({"pattern": "TODO|FIXME|HACK|XXX|unwrap\\(\\)", "path": "{{root}}", "mode": "fine"}),
                depends_on: None,
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "test_verify".into(),
                description: "运行测试验证".into(),
                tool: "bash_exec".into(),
                params: json!({"command": "cargo test --workspace 2>&1 | tail -20", "timeout": 300}),
                depends_on: None,
                condition: None,
                fallback: None,
            },
        ],
        prompt: "代码质量三阶段审查：安全扫描 → 风格检查 → 测试验证。任何阶段发现问题都应报告并阻止合并。".into(),
        knowledge_refs: vec![],
        palace_tags: vec!["code".into(), "review".into(), "quality".into(), "security".into()],
        compound: true,
        template_params: Vec::new(),
    }
}

// ────────────────────────────────────────────────────────────
// skill.systematic_debugging — 系统化调试框架
// 来源：Toolchain systematic-debugging
// ────────────────────────────────────────────────────────────

fn skill_systematic_debugging() -> SkillDef {
    SkillDef {
        id: SkillId("systematic_debugging".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec!["调试".into(), "debug".into(), "排查问题".into(), "定位bug".into(), "troubleshoot".into()],
            regex: vec![r"(?i)debug|troubleshoot|排查.*问题|定位.*bug|修复.*bug".into()],
            domain: vec!["debug".into(), "fix".into(), "error".into()],
        },
        workflow: vec![
            SkillStep {
                id: "reproduce".into(),
                description: "复现问题：读取错误日志和相关代码".into(),
                tool: "fs_read".into(),
                params: json!({"path": "{{error_file}}"}),
                depends_on: None,
                condition: Some("error_file != null".into()),
                fallback: None,
            },
            SkillStep {
                id: "locate".into(),
                description: "定位根因：搜索错误模式和调用链".into(),
                tool: "fs_grep".into(),
                params: json!({"pattern": "{{error_pattern}}", "path": "{{root}}", "mode": "fine", "context": 3}),
                depends_on: Some(vec!["reproduce".into()]),
                condition: Some("error_pattern != null".into()),
                fallback: None,
            },
            SkillStep {
                id: "verify".into(),
                description: "验证修复：运行测试确认".into(),
                tool: "bash_exec".into(),
                params: json!({"command": "cargo test --workspace 2>&1 | tail -10", "timeout": 300}),
                depends_on: Some(vec!["locate".into()]),
                condition: None,
                fallback: None,
            },
        ],
        prompt: "系统化调试四阶段：复现问题 → 定位根因 → 修复 → 验证。禁止随机尝试，必须有明确的假设和验证。".into(),
        knowledge_refs: vec![],
        palace_tags: vec!["debug".into(), "fix".into(), "systematic".into()],
        compound: true,
        template_params: Vec::new(),
    }
}

// ────────────────────────────────────────────────────────────
// skill.verification — 完成前验证门控
// 来源：Toolchain verification-before-completion
// ────────────────────────────────────────────────────────────

fn skill_verification() -> SkillDef {
    SkillDef {
        id: SkillId("verification".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec!["验证".into(), "verify".into(), "确认完成".into(), "检查结果".into()],
            regex: vec![r"(?i)verify|check.*result|确认.*完成|验证.*结果".into()],
            domain: vec!["verify".into(), "check".into(), "test".into()],
        },
        workflow: vec![
            SkillStep {
                id: "compile".into(),
                description: "编译检查".into(),
                tool: "bash_exec".into(),
                params: json!({"command": "cargo check --workspace 2>&1 | tail -10", "timeout": 120}),
                depends_on: None,
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "test".into(),
                description: "运行测试".into(),
                tool: "bash_exec".into(),
                params: json!({"command": "cargo test --workspace 2>&1 | grep 'test result' | head -10", "timeout": 300}),
                depends_on: Some(vec!["compile".into()]),
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "lint".into(),
                description: "Lint 检查".into(),
                tool: "bash_exec".into(),
                params: json!({"command": "cargo clippy --workspace 2>&1 | tail -10", "timeout": 120}),
                depends_on: Some(vec!["compile".into()]),
                condition: None,
                fallback: None,
            },
        ],
        prompt: "完成前必须运行验证：编译 → 测试 → Lint。任何步骤失败都应报告，不可声称完成。".into(),
        knowledge_refs: vec![],
        palace_tags: vec!["verify".into(), "check".into(), "gate".into()],
        compound: true,
        template_params: Vec::new(),
    }
}

// ────────────────────────────────────────────────────────────
// skill.tdd — 测试驱动开发流程
// 来源：Toolchain test-driven-development
// ────────────────────────────────────────────────────────────

fn skill_tdd() -> SkillDef {
    SkillDef {
        id: SkillId("tdd".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec!["测试驱动".into(), "TDD".into(), "写测试".into(), "test driven".into(), "单元测试".into()],
            regex: vec![r"(?i)tdd|test.*driven|写.*测试|单元.*测试".into()],
            domain: vec!["test".into(), "tdd".into(), "quality".into()],
        },
        workflow: vec![
            SkillStep {
                id: "red".into(),
                description: "Red：先写失败的测试".into(),
                tool: "fs_write".into(),
                params: json!({"path": "{{test_file}}", "content": "{{test_content}}"}),
                depends_on: None,
                condition: Some("test_file != null && test_content != null".into()),
                fallback: None,
            },
            SkillStep {
                id: "green".into(),
                description: "Green：实现代码让测试通过".into(),
                tool: "fs_write".into(),
                params: json!({"path": "{{impl_file}}", "content": "{{impl_content}}"}),
                depends_on: Some(vec!["red".into()]),
                condition: Some("impl_file != null && impl_content != null".into()),
                fallback: None,
            },
            SkillStep {
                id: "verify".into(),
                description: "运行测试确认通过".into(),
                tool: "bash_exec".into(),
                params: json!({"command": "cargo test --workspace 2>&1 | tail -10", "timeout": 300}),
                depends_on: Some(vec!["green".into()]),
                condition: None,
                fallback: None,
            },
        ],
        prompt: "TDD 三阶段：Red（先写失败测试）→ Green（实现让测试通过）→ Refactor（优化代码）。严格按顺序执行。".into(),
        knowledge_refs: vec![],
        palace_tags: vec!["test".into(), "tdd".into(), "red".into(), "green".into()],
        compound: true,
        template_params: Vec::new(),
    }
}

// ────────────────────────────────────────────────────────────
// skill.review_matrix — 多维审查框架
// 来源：Toolchain review-matrix
// ────────────────────────────────────────────────────────────

fn skill_review_matrix() -> SkillDef {
    SkillDef {
        id: SkillId("review_matrix".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec!["多维审查".into(), "代码审查".into(), "review matrix".into(), "全面审查".into()],
            regex: vec![r"(?i)review.*matrix|多维.*审查|全面.*审查|代码.*审查".into()],
            domain: vec!["review".into(), "audit".into(), "quality".into()],
        },
        workflow: vec![
            SkillStep {
                id: "structure".into(),
                description: "结构审查：模块边界、依赖关系、耦合度".into(),
                tool: "fs_grep".into(),
                params: json!({"pattern": "pub (fn|struct|enum|mod)", "path": "{{root}}", "mode": "coarse"}),
                depends_on: None,
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "security".into(),
                description: "安全审查：注入、越权、敏感信息".into(),
                tool: "fs_grep".into(),
                params: json!({"pattern": "unwrap\\(\\)|expect\\(|unsafe|eval|exec|password|secret", "path": "{{root}}", "mode": "fine"}),
                depends_on: None,
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "performance".into(),
                description: "性能审查：阻塞调用、内存分配、并发".into(),
                tool: "fs_grep".into(),
                params: json!({"pattern": "std::fs::|std::thread::sleep|Vec::new\\(\\)|HashMap::new\\(\\)", "path": "{{root}}", "mode": "fine"}),
                depends_on: None,
                condition: None,
                fallback: None,
            },
        ],
        prompt: "多维审查框架：结构 → 安全 → 性能 → 可维护性。每个维度独立审查，最后综合评分。".into(),
        knowledge_refs: vec![],
        palace_tags: vec!["review".into(), "audit".into(), "matrix".into()],
        compound: true,
        template_params: Vec::new(),
    }
}
