//! 知识提取引擎 — 从非结构化文本提取结构化实体和关系
//!
//! ## 设计来源
//! 参考 Hyper-Extract 的两阶段提取模式：
//! 1. 先提取实体（节点）
//! 2. 再提取关系（边），以已知实体为上下文
//!
//! ## 调用方式
//! - 由 `KnowledgePalace::store_with_strategy()` 在写入时调用
//! - 由 `knowledge_refs` 加载时调用
//! - 提取结果写入 `KnowledgeEntry.entities` 和 `KnowledgeEntry.relations`

use serde::{Deserialize, Serialize};

/// 提取结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedKnowledge {
    pub entities: Vec<ExtractedEntity>,
    pub relations: Vec<ExtractedRelation>,
    pub title: String,
    pub tags: Vec<String>,
}

/// 提取的实体
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedEntity {
    pub name: String,
    #[serde(rename = "type")]
    pub entity_type: String,
    #[serde(default)]
    pub description: String,
}

/// 提取的关系
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedRelation {
    pub source: String,
    pub target: String,
    #[serde(rename = "type")]
    pub relation_type: String,
}

/// 生成实体提取 prompt
pub fn entity_extraction_prompt(text: &str) -> String {
    format!(
        r#"从以下文本中提取所有重要实体。返回 JSON 数组格式。

每个实体包含：
- name: 实体名称（唯一标识）
- type: 实体类型（tool, file, function, concept, person, organization, api, config, error）
- description: 简短描述（一句话）

规则：
1. 提取所有有价值的实体，不要遗漏
2. 保持命名与原文一致
3. 不要提取纯数字或纯标点
4. description 用中文

文本：
{text}

返回格式（纯 JSON，不要 markdown）：
[{{"name": "...", "type": "...", "description": "..."}}]"#
    )
}

/// 生成关系提取 prompt
pub fn relation_extraction_prompt(text: &str, known_entities: &[String]) -> String {
    let entity_list = if known_entities.is_empty() {
        "无已知实体，请从文本中推断。".to_string()
    } else {
        known_entities.join("\n- ")
    };

    format!(
        r#"从以下文本中提取实体之间的关系。返回 JSON 数组格式。

每个关系包含：
- source: 源实体名
- target: 目标实体名
- type: 关系类型（uses, depends_on, calls, contains, implements, creates, modifies, reads, writes, configures）

已知实体列表：
- {entity_list}

规则：
1. 只提取已知实体之间的关系
2. 如果实体不在列表中，不要创建涉及它的关系
3. 关系类型用英文小写
4. source 和 target 必须是已知实体名

文本：
{text}

返回格式（纯 JSON，不要 markdown）：
[{{"source": "...", "target": "...", "type": "..."}}]"#
    )
}

/// 生成完整知识提取 prompt（单阶段，适合短文本）
pub fn full_extraction_prompt(text: &str, domain: &str) -> String {
    format!(
        r#"从以下文本中提取结构化知识。返回 JSON 格式。

领域：{domain}

返回格式：
{{"title": "知识标题（一句话概括）", "entities": [{{"name": "...", "type": "...", "description": "..."}}], "relations": [{{"source": "...", "target": "...", "type": "..."}}], "tags": ["标签1", "标签2"]}}

实体类型：tool, file, function, concept, person, organization, api, config, error
关系类型：uses, depends_on, calls, contains, implements, creates, modifies, reads, writes, configures

规则：
1. 提取所有有价值的实体
2. 只提取明确陈述的关系
3. tags 用中文
4. title 用中文

文本：
{text}"#
    )
}

/// 解析 LLM 返回的 JSON 为 ExtractedKnowledge
pub fn parse_extraction_response(response: &str) -> Option<ExtractedKnowledge> {
    // 尝试直接解析
    if let Ok(knowledge) = serde_json::from_str::<ExtractedKnowledge>(response) {
        return Some(knowledge);
    }

    // 尝试从 markdown 代码块中提取 JSON
    let json_str = if let Some(start) = response.find("```json") {
        let json_start = start + 7;
        if let Some(end) = response[json_start..].find("```") {
            &response[json_start..json_start + end]
        } else {
            response
        }
    } else if let Some(start) = response.find("```") {
        let json_start = start + 3;
        // 跳过语言标识符
        let json_start = response[json_start..]
            .find('\n')
            .map(|i| json_start + i + 1)
            .unwrap_or(json_start);
        if let Some(end) = response[json_start..].find("```") {
            &response[json_start..json_start + end]
        } else {
            response
        }
    } else {
        response
    };

    serde_json::from_str::<ExtractedKnowledge>(json_str.trim()).ok()
}

/// 解析实体列表 JSON
pub fn parse_entities_response(response: &str) -> Option<Vec<ExtractedEntity>> {
    let json_str = extract_json_from_response(response);
    serde_json::from_str::<Vec<ExtractedEntity>>(json_str.trim()).ok()
}

/// 解析关系列表 JSON
pub fn parse_relations_response(response: &str) -> Option<Vec<ExtractedRelation>> {
    let json_str = extract_json_from_response(response);
    serde_json::from_str::<Vec<ExtractedRelation>>(json_str.trim()).ok()
}

/// 从 LLM 响应中提取 JSON 字符串（处理 markdown 代码块）
fn extract_json_from_response(response: &str) -> &str {
    if let Some(start) = response.find("```json") {
        let json_start = start + 7;
        if let Some(end) = response[json_start..].find("```") {
            return response[json_start..json_start + end].trim();
        }
    }
    if let Some(start) = response.find("```") {
        let json_start = start + 3;
        let json_start = response[json_start..]
            .find('\n')
            .map(|i| json_start + i + 1)
            .unwrap_or(json_start);
        if let Some(end) = response[json_start..].find("```") {
            return response[json_start..json_start + end].trim();
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_extraction_response_direct_json() {
        let response = r#"{"title":"Test","entities":[{"name":"foo","type":"tool","description":"bar"}],"relations":[{"source":"foo","target":"bar","type":"uses"}],"tags":["test"]}"#;
        let result = parse_extraction_response(response);
        assert!(result.is_some());
        let knowledge = result.unwrap();
        assert_eq!(knowledge.title, "Test");
        assert_eq!(knowledge.entities.len(), 1);
        assert_eq!(knowledge.relations.len(), 1);
    }

    #[test]
    fn test_parse_extraction_response_markdown_block() {
        let response = r#"Here is the extraction:

```json
{"title":"Test","entities":[{"name":"foo","type":"tool","description":"bar"}],"relations":[],"tags":[]}
```

Hope this helps!"#;
        let result = parse_extraction_response(response);
        assert!(result.is_some());
        assert_eq!(result.unwrap().title, "Test");
    }

    #[test]
    fn test_parse_entities_response() {
        let response = r#"[{"name":"foo","type":"tool","description":"bar"}]"#;
        let result = parse_entities_response(response);
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 1);
    }

    #[test]
    fn test_entity_extraction_prompt() {
        let prompt = entity_extraction_prompt("This is a test text");
        assert!(prompt.contains("test text"));
        assert!(prompt.contains("JSON"));
    }

    #[test]
    fn test_relation_extraction_prompt() {
        let entities = vec!["foo".to_string(), "bar".to_string()];
        let prompt = relation_extraction_prompt("foo uses bar", &entities);
        assert!(prompt.contains("foo"));
        assert!(prompt.contains("bar"));
    }
}
