//! # ToolActionClassifier — 规则型安全分类器
//!
//! ## 设计意图
//! 借鉴 Claude Code 的独立安全分类器模式，以零 LLM 开销评估工具调用风险。
//! 决策优先级与 Claude Code 一致：hard_deny → soft_deny → allow_rules → user_intent。
//!
//! ## 引用关系
//! - 创建: `CoreLoop::new()` 构建（从 config/内置规则加载）
//! - 消费: `pipeline/mod.rs` 在 execute_tool 前调用 `classify()`
//! - 配置: 内置硬编码规则 + 用户可通过 `~/.abacus/safety_rules.yaml` 扩展
//!
//! ## 生命周期
//! - 创建: 引擎初始化时
//! - 激活: 每次工具调用前
//! - 销毁: 随 CoreLoop drop

use serde::{Deserialize, Serialize};

/// 分类结果 — 工具动作的安全评估
///
/// 引用关系: pipeline execute_tool 匹配此 enum 决定继续/确认/拒绝
#[derive(Debug, Clone, PartialEq)]
pub enum ClassifyResult {
    /// 直接放行——动作在安全范围内
    Allow,
    /// 需要用户确认——动作有潜在风险但可授权
    NeedsConfirm(String),
    /// 绝对拒绝——动作违反硬安全约束
    Deny(String),
}

/// 动作匹配模式 — 定义什么样的工具调用匹配某条规则
///
/// 引用关系: ToolActionClassifier 的 hard_deny/soft_deny/allow_rules 持有
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionPattern {
    /// 匹配的工具 ID（精确匹配，"*" = 通配所有）
    pub tool_id: String,
    /// 参数匹配条件
    pub condition: PatternCondition,
    /// 触发原因（展示给用户）
    pub reason: String,
}

/// 参数匹配条件
///
/// 设计: 用枚举表达常见模式，避免引入完整表达式引擎
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PatternCondition {
    /// 任何调用都匹配
    Any,
    /// 指定字段包含子串
    Contains { field: String, substring: String },
    /// 指定字段匹配前缀
    StartsWith { field: String, prefix: String },
    /// 指定字段的路径在 cwd 外
    PathOutsideCwd { field: String },
    /// 多条件 AND
    All { conditions: Vec<PatternCondition> },
    /// 多条件 OR
    AnyOf { conditions: Vec<PatternCondition> },
}

impl ActionPattern {
    /// 检查工具调用是否匹配此模式
    ///
    /// ## 参数
    /// - `tool_id`: 被调用的工具标识
    /// - `args`: 工具参数 JSON
    /// - `cwd`: 当前工作目录（用于 PathOutsideCwd 判断）
    pub fn matches(&self, tool_id: &str, args: &serde_json::Value, cwd: &str) -> bool {
        // 工具 ID 匹配
        if self.tool_id != "*" && self.tool_id != tool_id {
            return false;
        }
        // 条件匹配
        self.condition.matches(args, cwd)
    }
}

impl PatternCondition {
    fn matches(&self, args: &serde_json::Value, cwd: &str) -> bool {
        match self {
            PatternCondition::Any => true,
            PatternCondition::Contains { field, substring } => {
                extract_field(args, field)
                    .map(|v| v.to_lowercase().contains(&substring.to_lowercase()))
                    .unwrap_or(false)
            }
            PatternCondition::StartsWith { field, prefix } => {
                extract_field(args, field)
                    .map(|v| v.starts_with(prefix))
                    .unwrap_or(false)
            }
            PatternCondition::PathOutsideCwd { field } => {
                extract_field(args, field)
                    .map(|v| !v.starts_with(cwd))
                    .unwrap_or(false)
            }
            PatternCondition::All { conditions } => {
                conditions.iter().all(|c| c.matches(args, cwd))
            }
            PatternCondition::AnyOf { conditions } => {
                conditions.iter().any(|c| c.matches(args, cwd))
            }
        }
    }
}

/// 从 JSON 中提取字段值为字符串
fn extract_field(args: &serde_json::Value, field: &str) -> Option<String> {
    // 支持嵌套路径：如 "command" 或 "options.path"
    let parts: Vec<&str> = field.split('.').collect();
    let mut current = args;
    for part in &parts {
        current = current.get(*part)?;
    }
    current.as_str().map(|s| s.to_string())
}

/// 工具动作安全分类器
///
/// ## 决策优先级
/// 1. `hard_deny` — 绝对禁止，无法覆盖
/// 2. `soft_deny` — 默认拦截，可被 `allow_rules` 覆盖
/// 3. `allow_rules` — 白名单例外
/// 4. 默认 Allow
///
/// ## 配置来源
/// - 内置规则: `default_rules()` 硬编码
/// - 用户规则: `~/.abacus/safety_rules.yaml` 加载合并
pub struct ToolActionClassifier {
    pub hard_deny: Vec<ActionPattern>,
    pub soft_deny: Vec<ActionPattern>,
    pub allow_rules: Vec<ActionPattern>,
    /// 当前工作目录（用于 PathOutsideCwd 判断）
    pub cwd: String,
}

impl ToolActionClassifier {
    /// 构建分类器（内置规则 + 用户扩展）
    ///
    /// ## 引用关系
    /// - 调用方: CoreLoop::new()
    /// - 用户规则文件: ~/.abacus/safety_rules.yaml
    pub fn new(cwd: String) -> Self {
        let mut classifier = Self {
            hard_deny: Vec::new(),
            soft_deny: Vec::new(),
            allow_rules: Vec::new(),
            cwd,
        };
        classifier.load_builtin_rules();
        classifier.load_user_rules();
        classifier
    }

    /// 评估一次工具调用的安全性
    ///
    /// ## 返回
    /// - `Allow` — 直接放行
    /// - `NeedsConfirm(reason)` — 需要用户确认，reason 展示给用户
    /// - `Deny(reason)` — 拒绝执行
    pub fn classify(&self, tool_id: &str, args: &serde_json::Value) -> ClassifyResult {
        // 1. hard_deny 优先
        for pattern in &self.hard_deny {
            if pattern.matches(tool_id, args, &self.cwd) {
                return ClassifyResult::Deny(pattern.reason.clone());
            }
        }
        // 2. soft_deny — 检查 allow_rules 是否覆盖
        for pattern in &self.soft_deny {
            if pattern.matches(tool_id, args, &self.cwd) {
                let overridden = self.allow_rules.iter()
                    .any(|a| a.matches(tool_id, args, &self.cwd));
                if !overridden {
                    return ClassifyResult::NeedsConfirm(pattern.reason.clone());
                }
            }
        }
        // 3. 默认放行
        ClassifyResult::Allow
    }

    /// 加载内置硬编码规则
    fn load_builtin_rules(&mut self) {
        // ── Hard Deny: 绝对禁止 ──
        self.hard_deny.extend(vec![
            ActionPattern {
                tool_id: "bash_exec".into(),
                condition: PatternCondition::Contains {
                    field: "command".into(),
                    substring: "rm -rf /".into(),
                },
                reason: "禁止删除根目录".into(),
            },
            ActionPattern {
                tool_id: "bash_exec".into(),
                condition: PatternCondition::Contains {
                    field: "command".into(),
                    substring: ":(){ :|:& };:".into(),
                },
                reason: "禁止 fork bomb".into(),
            },
            ActionPattern {
                tool_id: "bash_exec".into(),
                condition: PatternCondition::Contains {
                    field: "command".into(),
                    substring: "DROP DATABASE".into(),
                },
                reason: "禁止删除数据库".into(),
            },
            ActionPattern {
                tool_id: "bash_exec".into(),
                condition: PatternCondition::Contains {
                    field: "command".into(),
                    substring: "mkfs".into(),
                },
                reason: "禁止格式化磁盘".into(),
            },
        ]);

        // ── Soft Deny: 需确认 ──
        self.soft_deny.extend(vec![
            ActionPattern {
                tool_id: "bash_exec".into(),
                condition: PatternCondition::Contains {
                    field: "command".into(),
                    substring: "--force".into(),
                },
                reason: "强制操作需确认".into(),
            },
            ActionPattern {
                tool_id: "bash_exec".into(),
                condition: PatternCondition::Contains {
                    field: "command".into(),
                    substring: "git push".into(),
                },
                reason: "推送到远程需确认".into(),
            },
            ActionPattern {
                tool_id: "bash_exec".into(),
                condition: PatternCondition::Contains {
                    field: "command".into(),
                    substring: "reset --hard".into(),
                },
                reason: "硬重置会丢失未提交更改".into(),
            },
            ActionPattern {
                tool_id: "bash_exec".into(),
                condition: PatternCondition::Contains {
                    field: "command".into(),
                    substring: "DROP TABLE".into(),
                },
                reason: "删除表需确认".into(),
            },
            ActionPattern {
                tool_id: "fs_write".into(),
                condition: PatternCondition::PathOutsideCwd { field: "path".into() },
                reason: "写入工作目录外的文件需确认".into(),
            },
            ActionPattern {
                tool_id: "fs_delete".into(),
                condition: PatternCondition::Any,
                reason: "删除文件需确认".into(),
            },
        ]);
    }

    /// 加载用户自定义规则（~/.abacus/safety_rules.yaml）
    fn load_user_rules(&mut self) {
        let path = dirs::home_dir()
            .unwrap_or_default()
            .join(".abacus")
            .join("safety_rules.yaml");
        if !path.exists() {
            return;
        }
        // 🟡#17 治本：检查文件权限——group/other 写权限意味着其他用户可改安全规则
        // 安全规则 YAML 不应 group/other 可写（owner-only 0o600）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&path) {
                let mode = meta.permissions().mode();
                if mode & 0o077 != 0 {
                    tracing::warn!(
                        "safety_rules.yaml {} has world/group writable bits ({:o}); \
                         for safety, restrict to 0o600 (owner read/write only)",
                        path.display(), mode
                    );
                }
            }
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("safety_rules: cannot read {}: {e}", path.display());
                return;
            }
        };

        #[derive(Deserialize)]
        pub(crate) struct UserRules {
            #[serde(default)]
            hard_deny: Vec<ActionPattern>,
            #[serde(default)]
            soft_deny: Vec<ActionPattern>,
            #[serde(default)]
            allow: Vec<ActionPattern>,
        }

        // 🟡#17 治本：parse 错误不再静默吞——给用户看到
        let rules: UserRules = match serde_yaml::from_str(&content) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("safety_rules: YAML parse error in {}: {e}", path.display());
                return;
            }
        };
        tracing::info!(
            "safety_rules: loaded {} hard_deny, {} soft_deny, {} allow from {}",
            rules.hard_deny.len(), rules.soft_deny.len(), rules.allow.len(), path.display()
        );
        self.hard_deny.extend(rules.hard_deny);
        self.soft_deny.extend(rules.soft_deny);
        self.allow_rules.extend(rules.allow);
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_classifier() -> ToolActionClassifier {
        ToolActionClassifier::new("/Users/test/project".into())
    }

    #[test]
    fn test_hard_deny_rm_rf() {
        let c = make_classifier();
        let result = c.classify("bash_exec", &json!({"command": "rm -rf /"}));
        assert!(matches!(result, ClassifyResult::Deny(_)));
    }

    #[test]
    fn test_hard_deny_fork_bomb() {
        let c = make_classifier();
        let result = c.classify("bash_exec", &json!({"command": ":(){ :|:& };:"}));
        assert!(matches!(result, ClassifyResult::Deny(_)));
    }

    #[test]
    fn test_soft_deny_force_push() {
        let c = make_classifier();
        let result = c.classify("bash_exec", &json!({"command": "git push --force origin main"}));
        assert!(matches!(result, ClassifyResult::NeedsConfirm(_)));
    }

    #[test]
    fn test_soft_deny_git_push() {
        let c = make_classifier();
        let result = c.classify("bash_exec", &json!({"command": "git push origin feature"}));
        assert!(matches!(result, ClassifyResult::NeedsConfirm(_)));
    }

    #[test]
    fn test_allow_normal_command() {
        let c = make_classifier();
        let result = c.classify("bash_exec", &json!({"command": "cargo test"}));
        assert_eq!(result, ClassifyResult::Allow);
    }

    #[test]
    fn test_allow_read_file() {
        let c = make_classifier();
        let result = c.classify("fs_read", &json!({"path": "/etc/hosts"}));
        assert_eq!(result, ClassifyResult::Allow);
    }

    #[test]
    fn test_soft_deny_write_outside_cwd() {
        let c = make_classifier();
        let result = c.classify("fs_write", &json!({"path": "/tmp/evil.sh"}));
        assert!(matches!(result, ClassifyResult::NeedsConfirm(_)));
    }

    #[test]
    fn test_allow_write_inside_cwd() {
        let c = make_classifier();
        let result = c.classify("fs_write", &json!({"path": "/Users/test/project/src/main.rs"}));
        assert_eq!(result, ClassifyResult::Allow);
    }

    #[test]
    fn test_allow_rule_overrides_soft_deny() {
        let mut c = make_classifier();
        // 添加 allow rule: 允许 push 到 feature 分支
        c.allow_rules.push(ActionPattern {
            tool_id: "bash_exec".into(),
            condition: PatternCondition::Contains {
                field: "command".into(),
                substring: "feature".into(),
            },
            reason: "feature 分支 push 允许".into(),
        });
        let result = c.classify("bash_exec", &json!({"command": "git push origin feature/v2"}));
        assert_eq!(result, ClassifyResult::Allow); // allow_rule 覆盖了 soft_deny
    }

    #[test]
    fn test_hard_deny_cannot_be_overridden() {
        let mut c = make_classifier();
        c.allow_rules.push(ActionPattern {
            tool_id: "bash_exec".into(),
            condition: PatternCondition::Any,
            reason: "allow all".into(),
        });
        // hard_deny 仍然生效
        let result = c.classify("bash_exec", &json!({"command": "rm -rf /"}));
        assert!(matches!(result, ClassifyResult::Deny(_)));
    }

    #[test]
    fn test_priority_hard_deny_over_soft_deny() {
        let c = make_classifier();
        // DROP DATABASE 是 hard_deny，DROP TABLE 是 soft_deny
        let result = c.classify("bash_exec", &json!({"command": "DROP DATABASE production"}));
        assert!(matches!(result, ClassifyResult::Deny(_)));

        let result = c.classify("bash_exec", &json!({"command": "DROP TABLE users"}));
        assert!(matches!(result, ClassifyResult::NeedsConfirm(_)));
    }

    /// 🟡#17 治本测试：恶意 YAML 不被静默吞
    ///
    /// 旧 `if let Ok(rules) = ...` 吞掉所有错配 YAML。
    /// 治本：`load_user_rules` 内部显式 match，**真**记 `tracing::warn!` 后 return。
    ///
    /// 验证：能写出安全结构的反序列化（Vec<ActionPattern>）并在错误时返回 Err。
    #[test]
    fn safety_rules_struct_deserializes_valid_yaml() {
        use serde::Deserialize;
        #[derive(Deserialize)]
        struct TestRules {
            #[serde(default)]
            hard_deny: Vec<String>,
            #[serde(default)]
            soft_deny: Vec<String>,
            #[serde(default)]
            allow: Vec<String>,
        }
        let yaml = r#"
hard_deny: ["rm -rf /", "shutdown"]
soft_deny: ["curl http://unknown"]
allow: ["git status"]
"#;
        let r: TestRules = serde_yaml::from_str(yaml).expect("valid yaml should parse");
        assert_eq!(r.hard_deny.len(), 2);
        assert_eq!(r.soft_deny.len(), 1);
        assert_eq!(r.allow.len(), 1);
    }

    #[test]
    fn safety_rules_struct_rejects_malformed_yaml() {
        use serde::Deserialize;
        #[derive(Deserialize)]
        struct TestRules {
            #[serde(default)]
            hard_deny: Vec<String>,
        }
        // hard_deny 是 list，给 string 应该是 Err
        let bad = r#"
hard_deny: "not a list"
"#;
        let r: Result<TestRules, _> = serde_yaml::from_str(bad);
        assert!(r.is_err(), "malformed YAML must be rejected, not silently dropped");
    }
}
