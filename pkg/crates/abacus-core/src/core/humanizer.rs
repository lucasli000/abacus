//! AI 味后置检测引擎
//!
//! ## 场景
//! LLM 输出后，检测文本中的 AI 写作特征并打分。
//! 用于 detect-only（仅打分）和 full（打分+轻量改写）模式。
//!
//! ## 依赖
//! 无外部依赖，纯规则引擎。
//!
//! ## 引用关系
//! - 被 CoreLoop::process_turn() 在返回前调用（detect-only/full 模式）
//! - 输出 AIPatternReport 附加到 TurnResult
//!
//! ## 边界
//! - 纯规则检测，<1ms，零 LLM 调用
//! - 仅对非代码/非数学输出生效
//! - 轻量改写是确定性字符串替换（不调 LLM）

use serde::{Deserialize, Serialize};

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 检测结果类型
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// AI 味检测报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AIPatternReport {
    /// 总 AI 味分数 [0, 1]
    pub score: f64,
    /// 各维度分数
    pub dimensions: AIPatternDimensions,
    /// 检测到的具体模式
    pub detected_patterns: Vec<DetectedPattern>,
    /// 是否触发改写
    pub rewrite_triggered: bool,
}

/// 6 维 AI 味检测
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AIPatternDimensions {
    /// 结构过度规整
    pub over_structure: f64,
    /// 夸大/空洞词汇
    pub hyperbolic_language: f64,
    /// 连接词过密
    pub connector_density: f64,
    /// 被动语态
    pub passive_voice: f64,
    /// 开场铺垫冗余
    pub preamble_padding: f64,
    /// 破折号/冒号过度使用
    pub punctuation_overuse: f64,
}

/// 检测到的单个模式
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedPattern {
    pub category: PatternCategory,
    pub fragment: String,
    pub position: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PatternCategory {
    TripleStructure,
    AIVocabulary,
    HyperbolicWord,
    VagueAttribution,
    PreamblePadding,
    RedundantSummary,
    OverConnecting,
    NegativeParallelism,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 配置
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Humanizer 模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum HumanizerMode {
    Off,
    #[default]
    PromptOnly,
    DetectOnly,
    Full,
}


/// 检测配置
#[derive(Debug, Clone)]
pub struct HumanizerConfig {
    pub mode: HumanizerMode,
    /// 触发改写的阈值（仅 full 模式）
    pub rewrite_threshold: f64,
    /// 豁免的 task types
    pub exempt_types: Vec<String>,
}

impl Default for HumanizerConfig {
    fn default() -> Self {
        Self {
            mode: HumanizerMode::PromptOnly,
            rewrite_threshold: 0.6,
            exempt_types: vec![
                "code_writing".into(), "code_reading".into(),
                "mathematics".into(), "file_edit".into(),
            ],
        }
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 检测器
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// AI 味检测器 — 纯规则引擎
pub struct AIPatternDetector {
    config: HumanizerConfig,
}

impl AIPatternDetector {
    pub fn new(config: HumanizerConfig) -> Self {
        Self { config }
    }

    /// 检测文本中的 AI 味模式
    pub fn detect(&self, text: &str) -> AIPatternReport {
        let mut patterns: Vec<DetectedPattern> = Vec::new();

        let d1 = self.detect_over_structure(text, &mut patterns);
        let d2 = self.detect_hyperbolic(text, &mut patterns);
        let d3 = self.detect_connector_density(text, &mut patterns);
        let d4 = self.detect_passive_voice(text);
        let d5 = self.detect_preamble(text, &mut patterns);
        let d6 = self.detect_punctuation_overuse(text);

        let dimensions = AIPatternDimensions {
            over_structure: d1,
            hyperbolic_language: d2,
            connector_density: d3,
            passive_voice: d4,
            preamble_padding: d5,
            punctuation_overuse: d6,
        };

        // 加权合成
        let score = d1 * 0.25 + d2 * 0.20 + d3 * 0.20
                  + d4 * 0.10 + d5 * 0.15 + d6 * 0.10;

        AIPatternReport {
            score,
            dimensions,
            detected_patterns: patterns,
            rewrite_triggered: score > self.config.rewrite_threshold,
        }
    }

    /// 是否应该跳过检测
    pub fn should_skip(&self, task_type: &str) -> bool {
        self.config.mode == HumanizerMode::Off
            || self.config.exempt_types.contains(&task_type.to_string())
    }

    // ─── D1: 结构过度规整 ────────────────────────────────

    fn detect_over_structure(&self, text: &str, patterns: &mut Vec<DetectedPattern>) -> f64 {
        let mut score: f64 = 0.0;

        // 三段式检测
        let triple_sets: &[&[&str]] = &[
            &["首先", "其次", "最后"],
            &["第一", "第二", "第三"],
            &["一方面", "另一方面", "总之"],
            &["First", "Second", "Third"],
            &["Firstly", "Secondly", "Finally"],
        ];

        for set in triple_sets {
            let hits = set.iter().filter(|m| text.contains(*m)).count();
            if hits >= 2 {
                score += 0.5;
                patterns.push(DetectedPattern {
                    category: PatternCategory::TripleStructure,
                    fragment: set.join("/"),
                    position: 0,
                });
            }
        }

        // 连续 bullet 过多（>5）
        let max_bullets = count_consecutive_bullets(text);
        if max_bullets > 7 {
            score += 0.3;
        } else if max_bullets > 5 {
            score += 0.15;
        }

        score.min(1.0)
    }

    // ─── D2: 夸大/空洞词汇 ──────────────────────────────

    fn detect_hyperbolic(&self, text: &str, patterns: &mut Vec<DetectedPattern>) -> f64 {
        let ai_words: &[&str] = &[
            // 中文 AI 高频词
            "显著", "至关重要", "不容忽视", "深入探讨", "值得注意的是",
            "综上所述", "不言而喻", "毋庸置疑", "由此可见", "众所周知",
            "日益增长", "具有重要意义", "核心竞争力", "赋能", "助力",
            "全方位", "多维度", "深远影响", "极其重要",
            // 英文 AI 高频词
            "crucial", "pivotal", "paramount", "noteworthy",
            "landscape", "leverage", "synergy", "holistic", "paradigm",
            "delve into", "it's worth noting", "game-changer",
        ];

        let mut hits = 0;
        for word in ai_words {
            if text.contains(word) {
                hits += 1;
                patterns.push(DetectedPattern {
                    category: PatternCategory::AIVocabulary,
                    fragment: word.to_string(),
                    position: text.find(word).unwrap_or(0),
                });
            }
        }

        // 每 500 字 3 个 AI 词 → 满分
        let text_units = (text.chars().count() as f64 / 500.0).max(1.0);
        let density = hits as f64 / text_units;
        (density / 3.0).min(1.0)
    }

    // ─── D3: 连接词过密 ──────────────────────────────────

    fn detect_connector_density(&self, text: &str, patterns: &mut Vec<DetectedPattern>) -> f64 {
        let connectors: &[&str] = &[
            "因此", "然而", "此外", "与此同时", "另外", "尽管如此",
            "换言之", "也就是说", "总而言之", "归根结底", "值得一提的是",
            "therefore", "however", "moreover", "furthermore", "nevertheless",
            "in addition", "consequently",
        ];

        let sentence_count = text.matches(|c: char| "。！？.!?".contains(c)).count().max(1);
        let mut connector_count = 0;
        for conn in connectors {
            let count = text.matches(conn).count();
            if count > 0 {
                connector_count += count;
                if count >= 2 {
                    patterns.push(DetectedPattern {
                        category: PatternCategory::OverConnecting,
                        fragment: format!("{}(×{})", conn, count),
                        position: text.find(conn).unwrap_or(0),
                    });
                }
            }
        }

        let ratio = connector_count as f64 / sentence_count as f64;
        (ratio / 0.4).min(1.0) // 40% 句子有连接词 → 满分
    }

    // ─── D4: 被动语态 ────────────────────────────────────

    fn detect_passive_voice(&self, text: &str) -> f64 {
        let passive_markers: &[&str] = &[
            "被认为", "被视为", "被广泛", "据了解", "据悉",
            "被称为", "被用于", "被发现",
            "is considered", "is regarded", "it is believed",
            "it can be seen", "it should be noted",
        ];
        let hits = passive_markers.iter().filter(|m| text.contains(*m)).count();
        (hits as f64 / 3.0).min(1.0)
    }

    // ─── D5: 开场铺垫冗余 ────────────────────────────────

    fn detect_preamble(&self, text: &str, patterns: &mut Vec<DetectedPattern>) -> f64 {
        let first_line = text.lines().next().unwrap_or("");
        let head_end = text.char_indices().nth(150).map(|(i, _)| i).unwrap_or(text.len());
        let head = &text[..head_end];

        let preambles: &[&str] = &[
            "在当今", "随着", "众所周知", "不可否认",
            "毫无疑问", "在现代社会", "纵观", "回顾",
            "In today's", "As we navigate", "In the ever-changing",
            "It goes without saying",
        ];

        let mut score: f64 = 0.0;
        for p in preambles {
            if head.contains(p) {
                score += 0.6;
                patterns.push(DetectedPattern {
                    category: PatternCategory::PreamblePadding,
                    fragment: first_line.chars().take(50).collect::<String>(),
                    position: 0,
                });
                break;
            }
        }

        // 冗余结尾总结检测
        let tail_start = text.char_indices().rev().nth(200).map(|(i, _)| i).unwrap_or(0);
        let last_200 = &text[tail_start..];
        let summary_markers = ["综上所述", "总之", "归纳", "总结一下", "In conclusion", "To sum up"];
        if summary_markers.iter().any(|m| last_200.contains(m)) {
            score += 0.4;
            patterns.push(DetectedPattern {
                category: PatternCategory::RedundantSummary,
                fragment: "尾部重复总结".into(),
                position: text.len().saturating_sub(100),
            });
        }

        score.min(1.0)
    }

    // ─── D6: 破折号/冒号过度使用 ─────────────────────────

    fn detect_punctuation_overuse(&self, text: &str) -> f64 {
        let dashes = text.matches('—').count() + text.matches("——").count() * 2;
        let char_count = text.chars().count().max(1);
        // 每 200 字超过 3 个破折号 → AI 味
        let density = dashes as f64 / (char_count as f64 / 200.0).max(1.0);
        (density / 3.0).min(1.0)
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// 轻量改写（full 模式）
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// 确定性轻量改写（字符串替换，不调 LLM）
///
/// ## 场景
/// full 模式下，score > threshold 时触发。
/// 只做安全的字符串替换，不改变语义。
pub fn lightweight_rewrite(text: &str, patterns: &[DetectedPattern]) -> String {
    let mut result = text.to_string();

    // 删除开场废话
    for p in patterns {
        if p.category == PatternCategory::PreamblePadding && p.position == 0 {
            // 找第一个句号，删除之前的内容
            if let Some((pos, ch)) = result.char_indices().find(|(_, c)| *c == '。' || *c == '.') {
                result = result[pos + ch.len_utf8()..].trim_start().to_string();
            }
        }
    }

    // 替换 AI 高频词
    let replacements: &[(&str, &str)] = &[
        ("显著", "明显"),
        ("至关重要", "重要"),
        ("不容忽视", "值得关注"),
        ("深入探讨", "分析"),
        ("值得注意的是", ""),
        ("综上所述，", ""),
        ("总而言之，", ""),
        ("不言而喻", ""),
        ("毋庸置疑", ""),
        ("众所周知，", ""),
        ("由此可见，", ""),
        ("具有重要意义", "有意义"),
        ("全方位", "全面"),
        ("多维度", "多角度"),
        ("核心竞争力", "优势"),
        ("赋能", "支持"),
        ("助力", "帮助"),
    ];

    for (from, to) in replacements {
        result = result.replace(from, to);
    }

    // 清理多余空行
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }

    result.trim().to_string()
}

/// 辅助：计算连续 bullet 最大长度
fn count_consecutive_bullets(text: &str) -> usize {
    let mut max_run = 0;
    let mut current_run = 0;
    for line in text.lines() {
        let trimmed = line.trim();
        let is_bullet = trimmed.starts_with('-')
            || trimmed.starts_with('•')
            || trimmed.starts_with("* ")
            || (trimmed.len() > 2 && trimmed.chars().next().is_some_and(|c| c.is_ascii_digit())
                && trimmed.contains('.'));
        if is_bullet {
            current_run += 1;
        } else {
            max_run = max_run.max(current_run);
            current_run = 0;
        }
    }
    max_run.max(current_run)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_detector() -> AIPatternDetector {
        AIPatternDetector::new(HumanizerConfig::default())
    }

    #[test]
    fn test_clean_text_low_score() {
        let det = default_detector();
        let text = "系统吞吐量在负载测试中达到 1200 QPS。\
                    瓶颈在数据库连接池，当前配置 50 个连接，建议扩到 100。\
                    测试环境：4C8G 单节点，MySQL 8.0。";
        let report = det.detect(text);
        assert!(report.score < 0.3, "clean text should score low, got {}", report.score);
    }

    #[test]
    fn test_ai_heavy_text_high_score() {
        let det = default_detector();
        let text = "在当今数字时代，数据安全至关重要。\
                    首先，我们需要深入探讨加密方案的核心竞争力。\
                    其次，不容忽视的是，合规要求日益增长。\
                    最后，综上所述，我们必须全方位赋能安全体系。";
        let report = det.detect(text);
        assert!(report.score > 0.4, "AI-heavy text should score notably, got {}", report.score);
        assert!(!report.detected_patterns.is_empty());
        // 应检出三段式 + AI 词汇 + 开场铺垫
        assert!(report.dimensions.over_structure > 0.3);
        assert!(report.dimensions.hyperbolic_language > 0.2);
        assert!(report.dimensions.preamble_padding > 0.3);
    }

    #[test]
    fn test_triple_structure_detection() {
        let det = default_detector();
        let text = "首先分析需求。其次设计架构。最后编码实现。";
        let report = det.detect(text);
        assert!(report.dimensions.over_structure > 0.3);
        let has_triple = report.detected_patterns.iter()
            .any(|p| p.category == PatternCategory::TripleStructure);
        assert!(has_triple);
    }

    #[test]
    fn test_preamble_detection() {
        let det = default_detector();
        let text = "在当今快速发展的技术环境中，微服务架构已成为主流。具体来说，服务拆分的粒度应该...";
        let report = det.detect(text);
        assert!(report.dimensions.preamble_padding > 0.3);
    }

    #[test]
    fn test_connector_density() {
        let det = default_detector();
        let text = "系统需要重构。因此我们选择微服务。然而成本较高。\
                    此外团队经验不足。因此需要培训。此外还需要招人。";
        let report = det.detect(text);
        assert!(report.dimensions.connector_density > 0.4,
            "heavy connectors should score high, got {}", report.dimensions.connector_density);
    }

    #[test]
    fn test_lightweight_rewrite() {
        let text = "在当今数字时代，数据安全至关重要。不容忽视的是加密方案的核心竞争力。";
        let patterns = vec![
            DetectedPattern {
                category: PatternCategory::PreamblePadding,
                fragment: "在当今".into(),
                position: 0,
            },
        ];
        let rewritten = lightweight_rewrite(text, &patterns);
        // 开场废话应被删除
        assert!(!rewritten.contains("在当今数字时代"));
        // AI 词应被替换
        assert!(!rewritten.contains("至关重要"));
        assert!(!rewritten.contains("不容忽视"));
        assert!(!rewritten.contains("核心竞争力"));
    }

    #[test]
    fn test_skip_for_code() {
        let det = default_detector();
        assert!(det.should_skip("code_writing"));
        assert!(det.should_skip("mathematics"));
        assert!(!det.should_skip("architecture"));
        assert!(!det.should_skip("data_analysis"));
    }

    #[test]
    fn test_rewrite_threshold() {
        let config = HumanizerConfig {
            rewrite_threshold: 0.8,
            ..Default::default()
        };
        let det = AIPatternDetector::new(config);
        let text = "首先，至关重要的是深入探讨这个问题。其次，不容忽视的是...最后，综上所述...";
        let report = det.detect(text);
        // 分数可能高但未到 0.8 → 不触发改写
        // 具体是否触发取决于实际分数
        assert_eq!(report.rewrite_triggered, report.score > 0.8);
    }
}
