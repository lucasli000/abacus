//! Python 语言分析器
//!
//! ## 职责
//! 实现 `LanguageAnalyzer` trait，从 tree-sitter AST 提取 Python 的符号、
//! 调用关系、依赖（import）和类继承关系。
//!
//! ## 依赖
//! - `tree-sitter`: AST 遍历框架
//! - `tree-sitter-python`: Python grammar
//!
//! ## 引用关系
//! - 被 `super::create_analyzer_registry()` 注册为 Language::Python 的处理器
//! - 被 `Indexer::parallel_parse()` 并行调用（要求 Send + Sync）
//!
//! ## 生命周期
//! - 创建：`create_analyzer_registry()` 时（程序启动一次）
//! - 存活：与 CodeGraphManager 同生命周期（Arc 包装的注册表）
//! - 无可变状态，无需销毁逻辑
//!
//! ## 可见性约定
//! Python 无语言级可见性关键字，按命名约定判断：
//! - `_name`: Private（单下划线前缀）
//! - `__name`: Protected（双下划线前缀，name mangling）
//! - 其他: Public

use tree_sitter::{Node, Tree};

use super::{
    AnalysisResult, Language, LanguageAnalyzer, RawCall,
    make_symbol_id, node_text, count_errors,
};
use crate::code_graph::{Symbol, SymbolKind, Visibility, FileDep, DepKind, ImplRelation};

// ─── PythonAnalyzer ──────────────────────────────────────────────────────────

/// Python 无状态分析器
///
/// 处理 `.py` 和 `.pyi` (stub) 文件。
/// 无内部状态，满足 Send + Sync 约束。
#[derive(Default)]
pub struct PythonAnalyzer;

impl PythonAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageAnalyzer for PythonAnalyzer {
    fn language(&self) -> Language {
        Language::Python
    }

    fn extract_symbols(
        &self,
        tree: &Tree,
        source: &[u8],
        file_path: &str,
    ) -> AnalysisResult<Vec<Symbol>> {
        let (parse_errors, error_ranges) = count_errors(tree);
        let mut symbols = Vec::new();
        let root = tree.root_node();
        extract_symbols_recursive(root, source, file_path, None, &mut symbols);
        AnalysisResult::with_errors(symbols, parse_errors, error_ranges)
    }

    fn extract_calls(
        &self,
        tree: &Tree,
        source: &[u8],
        file_path: &str,
    ) -> AnalysisResult<Vec<RawCall>> {
        let (parse_errors, error_ranges) = count_errors(tree);
        let mut calls = Vec::new();
        let root = tree.root_node();
        extract_calls_recursive(root, source, file_path, &mut calls);
        AnalysisResult::with_errors(calls, parse_errors, error_ranges)
    }

    fn extract_deps(
        &self,
        tree: &Tree,
        source: &[u8],
        file_path: &str,
    ) -> AnalysisResult<Vec<FileDep>> {
        let (parse_errors, error_ranges) = count_errors(tree);
        let mut deps = Vec::new();
        let root = tree.root_node();
        extract_deps_recursive(root, source, file_path, &mut deps);
        AnalysisResult::with_errors(deps, parse_errors, error_ranges)
    }

    fn extract_impls(
        &self,
        tree: &Tree,
        source: &[u8],
        file_path: &str,
    ) -> AnalysisResult<Vec<ImplRelation>> {
        let (parse_errors, error_ranges) = count_errors(tree);
        let mut impls = Vec::new();
        let root = tree.root_node();
        extract_impls_recursive(root, source, file_path, &mut impls);
        AnalysisResult::with_errors(impls, parse_errors, error_ranges)
    }
}

// ─── 符号提取 ────────────────────────────────────────────────────────────────

/// 递归遍历 AST 提取符号定义
///
/// 处理装饰器：`decorated_definition` 包含 decorator + 实际定义，
/// 从内部 definition 提取符号，保留 decorator 信息在签名中。
/// 支持部分解析：遇到 ERROR 节点跳过，继续处理兄弟节点。
fn extract_symbols_recursive(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_id: Option<&str>,
    symbols: &mut Vec<Symbol>,
) {
    if node.is_error() {
        return;
    }

    let kind = node.kind();

    match kind {
        "function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                let vis = python_visibility(name);
                let is_method = parent_id.is_some();
                let sym_kind = if is_method { SymbolKind::Method } else { SymbolKind::Function };
                let kind_str = if is_method { "method" } else { "function" };

                let sig = extract_function_signature(&node, source);
                let id = make_symbol_id(file_path, name, kind_str, name_node.start_position().row as u32);

                symbols.push(Symbol {
                    id,
                    name: name.to_string(),
                    kind: sym_kind,
                    file: file_path.to_string(),
                    line: name_node.start_position().row as u32,
                    col: name_node.start_position().column as u32,
                    end_line: Some(node.end_position().row as u32),
                    signature: Some(sig),
                    doc_comment: extract_docstring(&node, source),
                    visibility: vis,
                    parent_id: parent_id.map(|s| s.to_string()),
                    hash: content_hash(&node, source),
                });
            }
        }

        "class_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                let vis = python_visibility(name);
                let id = make_symbol_id(file_path, name, "class", name_node.start_position().row as u32);

                symbols.push(Symbol {
                    id: id.clone(),
                    name: name.to_string(),
                    kind: SymbolKind::Class,
                    file: file_path.to_string(),
                    line: name_node.start_position().row as u32,
                    col: name_node.start_position().column as u32,
                    end_line: Some(node.end_position().row as u32),
                    signature: None,
                    doc_comment: extract_docstring(&node, source),
                    visibility: vis,
                    parent_id: parent_id.map(|s| s.to_string()),
                    hash: content_hash(&node, source),
                });

                // 遍历 class body 提取方法
                if let Some(body) = node.child_by_field_name("body") {
                    extract_symbols_recursive(body, source, file_path, Some(&id), symbols);
                }
                return;
            }
        }

        "decorated_definition" => {
            // 装饰器定义：提取内部的 function_definition 或 class_definition
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "function_definition" || child.kind() == "class_definition" {
                    extract_symbols_recursive(child, source, file_path, parent_id, symbols);
                }
            }
            return;
        }

        // 顶层赋值（模块级常量/变量）
        "expression_statement" => {
            // 仅在 module 顶层提取（parent 是 module）
            let is_module_level = node.parent()
                .map(|p| p.kind() == "module")
                .unwrap_or(false);
            if is_module_level {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "assignment" {
                        if let Some(left) = child.child_by_field_name("left") {
                            if left.kind() == "identifier" {
                                let name = node_text(&left, source);
                                // 只提取 UPPER_CASE 常量命名（约定）
                                if name.chars().all(|c| c.is_uppercase() || c == '_') && !name.is_empty() {
                                    let vis = python_visibility(name);
                                    let id = make_symbol_id(file_path, name, "constant", left.start_position().row as u32);
                                    symbols.push(Symbol {
                                        id,
                                        name: name.to_string(),
                                        kind: SymbolKind::Constant,
                                        file: file_path.to_string(),
                                        line: left.start_position().row as u32,
                                        col: left.start_position().column as u32,
                                        end_line: Some(child.end_position().row as u32),
                                        signature: None,
                                        doc_comment: None,
                                        visibility: vis,
                                        parent_id: parent_id.map(|s| s.to_string()),
                                        hash: content_hash(&child, source),
                                    });
                                }
                            }
                        }
                    }
                }
                return;
            }
        }

        _ => {}
    }

    // 默认递归子节点
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_symbols_recursive(child, source, file_path, parent_id, symbols);
    }
}

// ─── 调用提取 ────────────────────────────────────────────────────────────────

/// 递归提取函数调用
///
/// tree-sitter-python 中 call 的结构：
/// - `call` 节点，`function` field 为被调函数
/// - function 可以是 identifier (直接调用) 或 attribute (方法调用 obj.method)
fn extract_calls_recursive(
    node: Node,
    source: &[u8],
    _file_path: &str,
    calls: &mut Vec<RawCall>,
) {
    if node.is_error() {
        return;
    }

    if node.kind() == "call" {
        if let Some(func_node) = node.child_by_field_name("function") {
            let callee_name = node_text(&func_node, source).to_string();
            if !callee_name.is_empty() {
                calls.push(RawCall {
                    caller_id: String::new(), // 由 Indexer 后续关联
                    callee_name,
                    call_site_line: node.start_position().row as u32,
                    call_site_col: node.start_position().column as u32,
                });
            }
        }
    }

    // 检查装饰器（也是调用）
    if node.kind() == "decorator" {
        // decorator 的第一个子节点是被调用的装饰器表达式
        if let Some(expr) = node.named_child(0) {
            let callee_name = node_text(&expr, source).to_string();
            if !callee_name.is_empty() {
                calls.push(RawCall {
                    caller_id: String::new(),
                    callee_name: format!("@{callee_name}"),
                    call_site_line: node.start_position().row as u32,
                    call_site_col: node.start_position().column as u32,
                });
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_calls_recursive(child, source, _file_path, calls);
    }
}

// ─── 依赖提取 ────────────────────────────────────────────────────────────────

/// 递归提取 import 依赖
///
/// Python import 形式：
/// 1. `import module` → import_statement
/// 2. `from module import name` → import_from_statement
fn extract_deps_recursive(
    node: Node,
    source: &[u8],
    file_path: &str,
    deps: &mut Vec<FileDep>,
) {
    if node.is_error() {
        return;
    }

    match node.kind() {
        "import_statement" => {
            // `import module` or `import module as alias`
            // module_name 是 dotted_name 节点
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "dotted_name" || child.kind() == "aliased_import" {
                    let target = if child.kind() == "aliased_import" {
                        child.child_by_field_name("name")
                            .map(|n| node_text(&n, source).to_string())
                            .unwrap_or_default()
                    } else {
                        node_text(&child, source).to_string()
                    };
                    if !target.is_empty() {
                        deps.push(FileDep {
                            source_file: file_path.to_string(),
                            target_file: target,
                            dep_kind: DepKind::Use,
                        });
                    }
                }
            }
        }
        "import_from_statement" => {
            // `from module import name`
            // module_name field 存储模块路径
            if let Some(module_node) = node.child_by_field_name("module_name") {
                let target = node_text(&module_node, source).to_string();
                if !target.is_empty() {
                    deps.push(FileDep {
                        source_file: file_path.to_string(),
                        target_file: target,
                        dep_kind: DepKind::Use,
                    });
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_deps_recursive(child, source, file_path, deps);
    }
}

// ─── 实现关系提取 ─────────────────────────────────────────────────────────────

/// 递归提取类继承关系
///
/// Python: `class Foo(Bar, Baz):` → ImplRelation { trait_name: "Bar", impl_name: "Foo" }
/// Python 的继承在语义上同时是 "实现" 和 "继承"，此处统一映射为 ImplRelation。
fn extract_impls_recursive(
    node: Node,
    source: &[u8],
    file_path: &str,
    impls: &mut Vec<ImplRelation>,
) {
    if node.is_error() {
        return;
    }

    if node.kind() == "class_definition" {
        let class_name = node.child_by_field_name("name")
            .map(|n| node_text(&n, source).to_string())
            .unwrap_or_default();

        if !class_name.is_empty() {
            // superclasses field: argument_list 包含基类
            if let Some(bases) = node.child_by_field_name("superclasses") {
                let mut cursor = bases.walk();
                for base in bases.children(&mut cursor) {
                    // 基类可以是 identifier 或 attribute (module.Class)
                    if base.is_named() && base.kind() != "keyword_argument" {
                        let trait_name = node_text(&base, source).to_string();
                        // 过滤 metaclass=... 等关键字参数和空值
                        if !trait_name.is_empty() && trait_name != "object" {
                            impls.push(ImplRelation {
                                trait_name,
                                impl_name: class_name.clone(),
                                impl_file: file_path.to_string(),
                                impl_line: node.start_position().row as u32,
                            });
                        }
                    }
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_impls_recursive(child, source, file_path, impls);
    }
}

// ─── 辅助函数 ────────────────────────────────────────────────────────────────

/// 根据 Python 命名约定推断可见性
///
/// - `__name`: Protected (name mangling)
/// - `_name`: Private (convention)
/// - 其他: Public
fn python_visibility(name: &str) -> Visibility {
    if name.starts_with("__") && !name.ends_with("__") {
        // __name (但不是 __dunder__) → protected (name mangling)
        Visibility::Protected
    } else if name.starts_with('_') {
        Visibility::Private
    } else {
        Visibility::Public
    }
}

/// 提取函数签名（def name(params) -> return_type）
fn extract_function_signature(node: &Node, source: &[u8]) -> String {
    let name = node.child_by_field_name("name")
        .map(|n| node_text(&n, source))
        .unwrap_or("");
    let params = node.child_by_field_name("parameters")
        .map(|n| node_text(&n, source))
        .unwrap_or("()");
    let return_type = node.child_by_field_name("return_type")
        .map(|n| format!(" -> {}", node_text(&n, source)))
        .unwrap_or_default();

    format!("def {name}{params}{return_type}")
}

/// 提取 docstring（函数/类 body 的第一个 expression_statement 中的字符串）
fn extract_docstring(node: &Node, source: &[u8]) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    // body 的第一个子节点是 expression_statement
    let first_stmt = body.named_child(0)?;
    if first_stmt.kind() != "expression_statement" {
        return None;
    }
    let expr = first_stmt.named_child(0)?;
    if expr.kind() == "string" || expr.kind() == "concatenated_string" {
        let text = node_text(&expr, source);
        // 去除三引号
        let trimmed = text.trim_start_matches("\"\"\"")
            .trim_start_matches("'''")
            .trim_end_matches("\"\"\"")
            .trim_end_matches("'''")
            .trim();
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// 计算节点内容的 sha1 hash
fn content_hash(node: &Node, source: &[u8]) -> String {
    use sha1::{Sha1, Digest};
    let start = node.start_byte();
    let end = node.end_byte();
    let slice = &source[start..end.min(source.len())];
    let hash = Sha1::digest(slice);
    format!("{:x}", hash)
}

// ─── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tree_sitter_language;

    /// 辅助：解析 Python 源码并返回 tree
    fn parse_py(source: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_language(Language::Python)).unwrap();
        parser.parse(source, None).unwrap()
    }

    #[test]
    fn test_extract_functions_and_classes() {
        let source = r#"
def greet(name: str) -> str:
    """Greet someone."""
    return f"Hello, {name}"

class UserService:
    """User service class."""

    def fetch_user(self, user_id: int) -> User:
        return db.get(user_id)

    def _internal_method(self):
        pass

MAX_RETRIES = 3
"#;
        let tree = parse_py(source);
        let analyzer = PythonAnalyzer::new();
        let result = analyzer.extract_symbols(&tree, source.as_bytes(), "app/service.py");

        let names: Vec<&str> = result.data.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"greet"), "should find 'greet', got: {:?}", names);
        assert!(names.contains(&"UserService"), "should find 'UserService', got: {:?}", names);
        assert!(names.contains(&"fetch_user"), "should find method 'fetch_user', got: {:?}", names);
        assert!(names.contains(&"_internal_method"), "should find '_internal_method', got: {:?}", names);
        assert!(names.contains(&"MAX_RETRIES"), "should find constant 'MAX_RETRIES', got: {:?}", names);

        // 验证可见性
        let internal = result.data.iter().find(|s| s.name == "_internal_method").unwrap();
        assert_eq!(internal.visibility, Visibility::Private);

        // 验证 docstring
        let greet_sym = result.data.iter().find(|s| s.name == "greet").unwrap();
        assert_eq!(greet_sym.doc_comment.as_deref(), Some("Greet someone."));

        // 验证 parent_id
        let fetch_user = result.data.iter().find(|s| s.name == "fetch_user").unwrap();
        assert!(fetch_user.parent_id.is_some(), "method should have parent_id");
    }

    #[test]
    fn test_extract_imports() {
        let source = r#"
import os
import sys
from pathlib import Path
from typing import Optional, List
import numpy as np
"#;
        let tree = parse_py(source);
        let analyzer = PythonAnalyzer::new();
        let result = analyzer.extract_deps(&tree, source.as_bytes(), "app/main.py");

        let targets: Vec<&str> = result.data.iter().map(|d| d.target_file.as_str()).collect();
        assert!(targets.contains(&"os"), "should find 'os', got: {:?}", targets);
        assert!(targets.contains(&"sys"), "should find 'sys', got: {:?}", targets);
        assert!(targets.contains(&"pathlib"), "should find 'pathlib', got: {:?}", targets);
        assert!(targets.contains(&"typing"), "should find 'typing', got: {:?}", targets);
    }

    #[test]
    fn test_extract_class_inheritance() {
        let source = r#"
class Animal:
    pass

class Dog(Animal):
    pass

class GuideDog(Dog, Serializable):
    pass
"#;
        let tree = parse_py(source);
        let analyzer = PythonAnalyzer::new();
        let result = analyzer.extract_impls(&tree, source.as_bytes(), "models.py");

        // Dog inherits Animal
        assert!(result.data.iter().any(|i| i.impl_name == "Dog" && i.trait_name == "Animal"),
            "Dog should inherit Animal, got: {:?}", result.data);
        // GuideDog inherits Dog and Serializable
        assert!(result.data.iter().any(|i| i.impl_name == "GuideDog" && i.trait_name == "Dog"),
            "GuideDog should inherit Dog, got: {:?}", result.data);
        assert!(result.data.iter().any(|i| i.impl_name == "GuideDog" && i.trait_name == "Serializable"),
            "GuideDog should inherit Serializable, got: {:?}", result.data);
    }

    #[test]
    fn test_partial_parse_recovery() {
        let source = r#"
def valid_function():
    return 42

# 语法错误
def broken(:::
    pass

def another_valid():
    return 99
"#;
        let tree = parse_py(source);
        let analyzer = PythonAnalyzer::new();
        let result = analyzer.extract_symbols(&tree, source.as_bytes(), "broken.py");

        let names: Vec<&str> = result.data.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"valid_function"), "should recover 'valid_function', got: {:?}", names);
        assert!(names.contains(&"another_valid"), "should recover 'another_valid', got: {:?}", names);
        assert!(result.parse_errors > 0, "should report parse errors");
    }

    #[test]
    fn test_extract_calls_and_decorators() {
        let source = r#"
@app.route("/api")
def handler():
    result = fetch_data()
    print(result)
"#;
        let tree = parse_py(source);
        let analyzer = PythonAnalyzer::new();
        let result = analyzer.extract_calls(&tree, source.as_bytes(), "routes.py");

        let callees: Vec<&str> = result.data.iter().map(|c| c.callee_name.as_str()).collect();
        assert!(callees.contains(&"fetch_data"), "should find 'fetch_data', got: {:?}", callees);
        assert!(callees.contains(&"print"), "should find 'print', got: {:?}", callees);
        // decorator 也作为调用记录
        assert!(callees.iter().any(|c| c.contains("app.route")),
            "should find decorator call '@app.route', got: {:?}", callees);
    }
}
