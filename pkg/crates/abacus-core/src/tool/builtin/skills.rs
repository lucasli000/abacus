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
        // 元认知与调试技能（来源：think-skills）
        skill_assess_me(),
        skill_reframe(),
        skill_debug_root_cause(),
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

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 元认知与调试技能（来源：think-skills）
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

// ────────────────────────────────────────────────────────────
// skill.assess_me — 认知状态审计
// 来源：https://github.com/MaoChen1980/think-skills
//
// 核心机制：6 问审计（目标/进展/盲点/假设/阻塞/恢复）
// 写入 temp file → 读回 → 批判性分析（认知脱离效应）
// ────────────────────────────────────────────────────────────

fn skill_assess_me() -> SkillDef {
    SkillDef {
        id: SkillId("assess_me".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec![
                "你确定吗".into(), "再检查".into(), "review".into(), "verify".into(),
                "审计".into(), "double check".into(), "检查完整性".into(), "确认一下".into(),
                "assess".into(), "自查".into(), "重新评估".into(),
            ],
            regex: vec![r"(?i)你确定|再检查|double.?check|确认.*完整|review.*result".into()],
            domain: vec!["review".into(), "audit".into(), "verify".into(), "meta".into()],
        },
        workflow: vec![
            SkillStep {
                id: "write_assessment".into(),
                description: "将当前认知状态写入临时文件（6 问审计）".into(),
                tool: "fs_write".into(),
                params: json!({
                    "path": "/tmp/assess-me.md",
                    "content": "# Assess Me\n\n**Goal:** {{goal}}\n**Progress:** {{progress}}\n**Gaps:** {{gaps}}\n**Assumptions:** {{assumptions}}\n**Blocker:** {{blocker}}\n**Recovery:** {{recovery}}"
                }),
                depends_on: None,
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "read_back".into(),
                description: "读回审计文件，以读者视角批判性审视".into(),
                tool: "fs_read".into(),
                params: json!({"path": "/tmp/assess-me.md"}),
                depends_on: Some(vec!["write_assessment".into()]),
                condition: None,
                fallback: None,
            },
        ],
        prompt: "\
认知状态审计协议。当调试陷入循环、结果令人困惑、或用户质疑你的判断时触发。

## 6 问审计

1. **Goal** — 任务是什么？\"完成\"长什么样？
2. **Progress** — 已完成什么？还差什么？
3. **Gaps** — 缺什么信息？哪些数据没拿到？
4. **Assumptions** — 哪些未验证的信念在驱动你的 approach？（不能为空！总有假设）
5. **Blocker** — 具体阻塞是什么？（精确障碍，不是症状）
6. **Recovery** — 如果卡住了，应该做什么不同的？（具体行动，不是\"继续试\"）

## 关键纪律

- 写入 temp file 后必须读回 — 写而不读跳过认知脱离效应
- Progress 必须识别结果，而非复述努力
- Blocker 必须具体，模糊 = 还没找到
- Recovery ≠ \"keep trying\" — 命名具体下一步
- Assumptions 部分不能为空 — 总有假设

## 输出格式

[assess]
Goal: ...
Blocker: ...
Next: ...
[/assess]
".into(),
        knowledge_refs: vec![
            "builtin:knowledge/assess-me-guide.md".into(),
        ],
        palace_tags: vec!["meta".into(), "audit".into(), "cognition".into(), "self_check".into()],
        compound: true,
        template_params: Vec::new(),
    }
}

// ────────────────────────────────────────────────────────────
// skill.reframe — 问题重构
// 来源：https://github.com/MaoChen1980/think-skills
//
// 核心机制：压缩噪音 → 写入 temp file → 读回 → 换角度分析
// 当方案膨胀/重复循环/上下文超载时触发
// ────────────────────────────────────────────────────────────

fn skill_reframe() -> SkillDef {
    SkillDef {
        id: SkillId("reframe".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec![
                "换个角度".into(), "简化".into(), "太复杂".into(), "重新想".into(),
                "reframe".into(), "different approach".into(), "换个思路".into(),
                "跳出循环".into(), "方案膨胀".into(), "算了".into(),
            ],
            regex: vec![r"(?i)换个.*角度|different.*approach|重新.*想|换个.*方法|too.*complex".into()],
            domain: vec!["reframe".into(), "simplify".into(), "meta".into()],
        },
        workflow: vec![
            SkillStep {
                id: "compress".into(),
                description: "将问题压缩为 ≤30 行事实摘要写入 temp file".into(),
                tool: "fs_write".into(),
                params: json!({
                    "path": "/tmp/reframe.md",
                    "content": "## Goal\n{{goal}}\n\n## Stuck On\n{{stuck}}\n\n## What Has Been Tried\n{{tried}}\n\n## Difficulties / Blockers\n{{blockers}}\n\n## Available Resources\n{{resources}}"
                }),
                depends_on: None,
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "read_back".into(),
                description: "读回压缩摘要，以全新视角分析".into(),
                tool: "fs_read".into(),
                params: json!({"path": "/tmp/reframe.md"}),
                depends_on: Some(vec!["compress".into()]),
                condition: None,
                fallback: None,
            },
        ],
        prompt: "\
问题重构协议。当方案越写越复杂、同一问题连续 3 次无进展、或用户要求换方向时触发。

## 核心动作

把问题压成 ≤30 行事实摘要，写入 temp file，读回，然后回答 4 个新角度问题。

## 压缩模板

- Goal：想要什么工作状态
- Stuck On：发生了什么，哪里卡住
- What Has Been Tried：关键尝试和结果（简短！）
- Difficulties / Blockers：错误、约束
- Available Resources：相关文件、数据、上下文

每部分：1-3 个要点，总计 ≤30 行，不要叙事 — 只要事实。

## 读回后回答

- 给定这些证据，最可能的原因是什么？
- 最简单的确认/排除测试是什么？
- 哪个下一步有最高的信息价值？
- 你一直在假设什么可能是错的？

## 关键纪律

- 太多细节 defeat the purpose — 激进压缩
- \"What Has Been Tried\" 是事实，不是挫败感
- 读回文件 — 脱离是洞察的来源
- 如果摘要比它替代的对话还长，停下来继续调试

## 与 assess-me / debug-root-cause 的区别

- assess-me：审计认知状态（不确定/矛盾）
- debug-root-cause：排查外部因果链（error/bug）
- reframe：改变问题框架（打转/噪声/复杂化）
".into(),
        knowledge_refs: vec![
            "builtin:knowledge/assess-me-guide.md".into(),
        ],
        palace_tags: vec!["reframe".into(), "simplify".into(), "meta".into(), "perspective".into()],
        compound: true,
        template_params: Vec::new(),
    }
}

// ────────────────────────────────────────────────────────────
// skill.debug_root_cause — 系统化根因分析（20 种方法）
// 来源：https://github.com/MaoChen1980/think-skills
//
// 核心机制：定义问题 → 从 20 种 RCA 方法选 1-3 种 → 写入计划 → 逐步执行
// 替代随机 grep/试错
// ────────────────────────────────────────────────────────────

fn skill_debug_root_cause() -> SkillDef {
    SkillDef {
        id: SkillId("debug_root_cause".into()),
        version: "1.0".into(),
        triggers: SkillTriggers {
            keywords: vec![
                "报错".into(), "对不上".into(), "排查".into(), "根因".into(),
                "still broken".into(), "why".into(), "重复失败".into(), "不符合预期".into(),
                "root cause".into(), "rca".into(), "找原因".into(),
            ],
            regex: vec![r"(?i)root.?cause|rca|根因.*分析|排查.*原因|why.*fail|still.*broken".into()],
            domain: vec!["debug".into(), "error".into(), "diagnosis".into()],
        },
        workflow: vec![
            SkillStep {
                id: "define_problem".into(),
                description: "定义问题：写入问题描述 + 选择的 RCA 方法".into(),
                tool: "fs_write".into(),
                params: json!({
                    "path": "/tmp/debug-rca.md",
                    "content": "## Problem\nWhat: {{error_description}}\nExpected: {{expected_behavior}}\nFrequency: {{frequency}}\nImpact: {{impact}}\n\n## Method\nSelected: {{method_name}}\nRationale: {{method_rationale}}\nPlan: {{investigation_plan}}"
                }),
                depends_on: None,
                condition: None,
                fallback: None,
            },
            SkillStep {
                id: "read_plan".into(),
                description: "读回计划，确认方法选择合理".into(),
                tool: "fs_read".into(),
                params: json!({"path": "/tmp/debug-rca.md"}),
                depends_on: Some(vec!["define_problem".into()]),
                condition: None,
                fallback: None,
            },
        ],
        prompt: "\
系统化根因分析协议。当工具返回 error 或不符合预期的结果、同一操作反复失败时触发。

## 什么时候不调

- 错误直接指向具体位置 → 先修，不需要方法论
- 你清楚问题在哪 → 浪费时间

## Phase 1: 定义问题

写入 /tmp/debug-rca.md：
- What: 错误信息 / 不符合预期的行为
- Expected: 应该发生什么
- Frequency: 总是 / 间歇 / 特定条件
- Impact: 什么坏了

**模糊的问题 = 模糊的调试。先写问题再调查。**

## Phase 2: 选择方法（20 种 RCA 方法）

根据场景选 1-3 种：

| 场景 | 最佳方法 |
|------|---------|
| 未知原因，多变量 | 分解法、单变量法 |
| 回归（曾经能用） | 回退法、对比法 |
| 间歇性失败 | 复现法、静候法 |
| 错误信息指向某处 | 逆推法、依赖链追溯 |
| 复杂系统，多层 | 分层剥离法、排除法 |
| 数据看起来不对 | 透视法、边界法 |
| 需要理解未知代码 | 日志注入法、时间回溯法 |
| 找不到模式 | 离群分析、假设法 |

### 20 种方法速查

1. **分解法** — 对半分问题空间，递归缩小
2. **对比法** — 对比 working vs failing，找差异
3. **回退法** — 回滚到已知好状态，逐个重新应用
4. **假设法** — \"如果 X 则 Y 应该 Z\"，预测→测试→确认/排除
5. **逆推法** — 从失败点反向追溯因果链
6. **尝试法** — 搜索空间小+每次快试，快速迭代
7. **透视法** — 不信表面，检查内部状态（日志/转储/调试器）
8. **单变量法** — 一次只改一个因素，隔离变量
9. **边界法** — 测试空/null/零/最大/最小/溢出
10. **复现法** — 找最小可靠复现步骤
11. **排除法** — 逐个禁用/移除组件，定位相关项
12. **置换法** — 用已知好的组件替换可疑组件
13. **依赖链追溯** — 沿依赖链走，bug 常不在症状处
14. **日志注入法** — 在决策点加日志，看实际执行路径
15. **时间回溯法** — 从失败时间点向前追溯：什么变了？
16. **静候法** — 间歇性问题，延长观察
17. **分层剥离法** — 绕过外层直接测核心，逐层加回
18. **离群分析** — 失败案例有什么共同点？
19. **强制失败法** — 主动诱导失败条件，验证理解
20. **橡皮鸭法** — 向假想同事解释问题，结构化过程暴露答案

## Phase 3: 执行

按计划逐步执行，每步记录发现，更新 temp file。

## 关键纪律

- 先写问题再调查 — 模糊问题 = 模糊调试
- 选 1-3 种方法深入 — 方法跳转是恐慌
- 主动尝试推翻假设，而非确认
- 症状不是原因（\"null pointer\" 是症状不是根因）
- 2 种方法后仍卡住 → 问题定义可能有误 → 重做 Phase 1
".into(),
        knowledge_refs: vec![
            "builtin:knowledge/rca-methods.md".into(),
        ],
        palace_tags: vec!["debug".into(), "rca".into(), "root_cause".into(), "systematic".into()],
        compound: true,
        template_params: Vec::new(),
    }
}
