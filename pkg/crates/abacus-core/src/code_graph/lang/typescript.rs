//! TypeScript/JavaScript 语言分析器
//!
//! ## 职责
//! 实现 `LanguageAnalyzer` trait，从 tree-sitter AST 提取 TS/JS 的符号、
//! 调用关系、依赖和接口实现关系。
//!
//! ## 依赖
//! - `tree-sitter`: AST 遍历框架
//! - `tree-sitter-typescript`: TypeScript/JavaScript grammar
//!
//! ## 引用关系
//! - 被 `super::create_analyzer_registry()` 注册为 Language::TypeScript 和 Language::JavaScript 的处理器
//! - 被 `Indexer::parallel_parse()` 并行调用（要求 Send + Sync）
//!
//! ## 生命周期
//! - 创建：`create_analyzer_registry()` 时（程序启动一次）
//! - 存活：与 CodeGraphManager 同生命周期（Arc 包装的注册表）
//! - 无可变状态，无需销毁逻辑
//!
//! ## 设计说明
//! TypeScript 和 JavaScript 共用同一个分析器：tree-sitter-typescript grammar
//! 能解析 JS 超集（TSX/JSX），JS 文件只是不使用类型注解部分的节点。
//! `mod.rs` 中 JavaScript 和 TypeScript 都映射到 `TypeScriptAnalyzer`。

use tree_sitter::{Node, Tree};

use super::{
    AnalysisResult, Language, LanguageAnalyzer, RawCall,
    make_symbol_id, node_text, count_errors,
};
use crate::code_graph::{Symbol, SymbolKind, Visibility, FileDep, DepKind, ImplRelation};

// ─── Bun built-in modules ────────────────────────────────────────────────────

/// Bun built-in module specifiers (not resolved to file paths).
///
/// When an import source matches one of these (exact match or "bun:" prefix),
/// the dependency is recorded with `target_file: "bun:<module>"` to indicate
/// it's a runtime built-in rather than a resolvable file path.
///
/// ## References
/// - Consumed by `extract_deps_recursive()` (this file)
/// - Consumed by `Indexer::batch_commit()` (deps with "bun:" prefix are not resolved to disk)
const BUN_BUILTINS: &[&str] = &[
    "bun",
    "bun:test",
    "bun:shell",
    "bun:sqlite",
    "bun:ffi",
    "bun:jsc",
    "bun:wrap",
    "bun:main",
];

// ─── TypeScriptAnalyzer ──────────────────────────────────────────────────────

/// TypeScript/JavaScript 无状态分析器
///
/// 同时处理 `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs` 文件。
/// 无内部状态，满足 Send + Sync 约束。
pub struct TypeScriptAnalyzer;

impl TypeScriptAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageAnalyzer for TypeScriptAnalyzer {
    fn language(&self) -> Language {
        Language::TypeScript
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
/// 处理 export 包装：`export_statement` 内的声明提取为 Public 可见性。
/// 支持部分解析：遇到 ERROR 节点跳过，继续处理兄弟节点。
fn extract_symbols_recursive(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_id: Option<&str>,
    symbols: &mut Vec<Symbol>,
) {
    // 跳过 ERROR 节点但继续遍历兄弟
    if node.is_error() {
        return;
    }

    let kind = node.kind();
    let is_exported = node.parent()
        .map(|p| p.kind() == "export_statement")
        .unwrap_or(false);

    match kind {
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                let vis = if is_exported { Visibility::Public } else { Visibility::Private };
                let sig = extract_function_signature(&node, source);
                let id = make_symbol_id(file_path, name, "function", name_node.start_position().row as u32);
                symbols.push(Symbol {
                    id: id.clone(),
                    name: name.to_string(),
                    kind: SymbolKind::Function,
                    file: file_path.to_string(),
                    line: name_node.start_position().row as u32,
                    col: name_node.start_position().column as u32,
                    end_line: Some(node.end_position().row as u32),
                    signature: Some(sig),
                    doc_comment: extract_preceding_comment(&node, source),
                    visibility: vis,
                    parent_id: parent_id.map(|s| s.to_string()),
                    hash: content_hash(&node, source),
                });
            }
        }

        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                let vis = if is_exported { Visibility::Public } else { Visibility::Private };
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
                    doc_comment: extract_preceding_comment(&node, source),
                    visibility: vis,
                    parent_id: parent_id.map(|s| s.to_string()),
                    hash: content_hash(&node, source),
                });
                // 遍历 class body 提取方法
                if let Some(body) = node.child_by_field_name("body") {
                    extract_symbols_recursive(body, source, file_path, Some(&id), symbols);
                    return; // body 已处理，不再递归 children
                }
            }
        }

        "interface_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                let vis = if is_exported { Visibility::Public } else { Visibility::Private };
                let id = make_symbol_id(file_path, name, "interface", name_node.start_position().row as u32);
                symbols.push(Symbol {
                    id,
                    name: name.to_string(),
                    kind: SymbolKind::Interface,
                    file: file_path.to_string(),
                    line: name_node.start_position().row as u32,
                    col: name_node.start_position().column as u32,
                    end_line: Some(node.end_position().row as u32),
                    signature: None,
                    doc_comment: extract_preceding_comment(&node, source),
                    visibility: vis,
                    parent_id: parent_id.map(|s| s.to_string()),
                    hash: content_hash(&node, source),
                });
            }
        }

        "type_alias_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                let vis = if is_exported { Visibility::Public } else { Visibility::Private };
                let id = make_symbol_id(file_path, name, "type_alias", name_node.start_position().row as u32);
                symbols.push(Symbol {
                    id,
                    name: name.to_string(),
                    kind: SymbolKind::TypeAlias,
                    file: file_path.to_string(),
                    line: name_node.start_position().row as u32,
                    col: name_node.start_position().column as u32,
                    end_line: Some(node.end_position().row as u32),
                    signature: Some(node_text(&node, source).to_string()),
                    doc_comment: extract_preceding_comment(&node, source),
                    visibility: vis,
                    parent_id: parent_id.map(|s| s.to_string()),
                    hash: content_hash(&node, source),
                });
            }
        }

        "enum_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                let vis = if is_exported { Visibility::Public } else { Visibility::Private };
                let id = make_symbol_id(file_path, name, "enum", name_node.start_position().row as u32);
                symbols.push(Symbol {
                    id,
                    name: name.to_string(),
                    kind: SymbolKind::Enum,
                    file: file_path.to_string(),
                    line: name_node.start_position().row as u32,
                    col: name_node.start_position().column as u32,
                    end_line: Some(node.end_position().row as u32),
                    signature: None,
                    doc_comment: extract_preceding_comment(&node, source),
                    visibility: vis,
                    parent_id: parent_id.map(|s| s.to_string()),
                    hash: content_hash(&node, source),
                });
            }
        }

        "method_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                let id = make_symbol_id(file_path, name, "method", name_node.start_position().row as u32);
                symbols.push(Symbol {
                    id,
                    name: name.to_string(),
                    kind: SymbolKind::Method,
                    file: file_path.to_string(),
                    line: name_node.start_position().row as u32,
                    col: name_node.start_position().column as u32,
                    end_line: Some(node.end_position().row as u32),
                    signature: Some(extract_function_signature(&node, source)),
                    doc_comment: extract_preceding_comment(&node, source),
                    visibility: Visibility::Public, // TS 方法默认 public
                    parent_id: parent_id.map(|s| s.to_string()),
                    hash: content_hash(&node, source),
                });
            }
        }

        // const/let 变量声明（顶层或 export）
        "lexical_declaration" => {
            // 只在顶层或 export 中提取变量
            let is_toplevel = node.parent()
                .map(|p| p.kind() == "program" || p.kind() == "export_statement")
                .unwrap_or(false);
            if is_toplevel {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "variable_declarator" {
                        if let Some(name_node) = child.child_by_field_name("name") {
                            let name = node_text(&name_node, source);
                            // 跳过解构模式
                            if name_node.kind() == "identifier" {
                                let vis = if is_exported { Visibility::Public } else { Visibility::Private };
                                // 检查值是否是箭头函数
                                let (sym_kind, kind_str) = if child.child_by_field_name("value")
                                    .map(|v| v.kind() == "arrow_function")
                                    .unwrap_or(false)
                                {
                                    (SymbolKind::Function, "function")
                                } else {
                                    (SymbolKind::Variable, "variable")
                                };
                                let id = make_symbol_id(file_path, name, kind_str, name_node.start_position().row as u32);
                                symbols.push(Symbol {
                                    id,
                                    name: name.to_string(),
                                    kind: sym_kind,
                                    file: file_path.to_string(),
                                    line: name_node.start_position().row as u32,
                                    col: name_node.start_position().column as u32,
                                    end_line: Some(child.end_position().row as u32),
                                    signature: None,
                                    doc_comment: extract_preceding_comment(&node, source),
                                    visibility: vis,
                                    parent_id: parent_id.map(|s| s.to_string()),
                                    hash: content_hash(&child, source),
                                });
                            }
                        }
                    }
                }
                return; // 已处理子节点
            }
        }

        // export_statement: 透传给内部声明，由上层 is_exported 判断可见性
        "export_statement" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_symbols_recursive(child, source, file_path, parent_id, symbols);
            }
            return;
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

/// 递归遍历 AST 提取函数调用
///
/// 提取 `call_expression` 和 `new_expression` 两种调用形式。
fn extract_calls_recursive(
    node: Node,
    source: &[u8],
    file_path: &str,
    calls: &mut Vec<RawCall>,
) {
    if node.is_error() {
        return;
    }

    match node.kind() {
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if !callee_name.is_empty() {
                    // caller_id 由 Indexer 后续通过位置关联匹配
                    calls.push(RawCall {
                        caller_id: String::new(),
                        callee_name,
                        call_site_line: node.start_position().row as u32,
                        call_site_col: node.start_position().column as u32,
                    });
                }
            }
        }
        "new_expression" => {
            if let Some(constructor) = node.child_by_field_name("constructor") {
                let callee_name = format!("new {}", node_text(&constructor, source));
                calls.push(RawCall {
                    caller_id: String::new(),
                    callee_name,
                    call_site_line: node.start_position().row as u32,
                    call_site_col: node.start_position().column as u32,
                });
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_calls_recursive(child, source, file_path, calls);
    }
}

// ─── 依赖提取 ────────────────────────────────────────────────────────────────

/// 递归遍历 AST 提取 import/require 依赖
///
/// 处理三种形式：
/// 1. `import X from 'Y'` (ES module import)
/// 2. `require('Y')` (CommonJS require)
/// 3. `import('Y')` (dynamic import)
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
            // 提取 import source: `import X from 'module'` → source = 'module'
            if let Some(source_node) = node.child_by_field_name("source") {
                let target = strip_quotes(node_text(&source_node, source));
                if !target.is_empty() {
                    // Bun built-in: normalize to "bun:<module>" to skip file resolution
                    let target_file = normalize_bun_builtin(&target);
                    deps.push(FileDep {
                        source_file: file_path.to_string(),
                        target_file,
                        dep_kind: DepKind::Use,
                    });
                }
            }
        }
        "call_expression" => {
            // require('module') or import('module')
            if let Some(func_node) = node.child_by_field_name("function") {
                let func_name = node_text(&func_node, source);
                if func_name == "require" || func_name == "import" {
                    if let Some(args) = node.child_by_field_name("arguments") {
                        // 第一个参数是模块路径
                        if let Some(first_arg) = args.named_child(0) {
                            if first_arg.kind() == "string" {
                                let target = strip_quotes(node_text(&first_arg, source));
                                if !target.is_empty() {
                                    // Bun built-in: normalize to "bun:<module>"
                                    let target_file = normalize_bun_builtin(&target);
                                    deps.push(FileDep {
                                        source_file: file_path.to_string(),
                                        target_file,
                                        dep_kind: DepKind::Use,
                                    });
                                }
                            }
                        }
                    }
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

/// 递归提取 `implements` 关系
///
/// TypeScript: `class Foo implements Bar, Baz { ... }`
/// tree-sitter 节点结构：class_declaration → implements_clause → type_identifier*
fn extract_impls_recursive(
    node: Node,
    source: &[u8],
    file_path: &str,
    impls: &mut Vec<ImplRelation>,
) {
    if node.is_error() {
        return;
    }

    if node.kind() == "class_declaration" || node.kind() == "abstract_class_declaration" {
        let class_name = node.child_by_field_name("name")
            .map(|n| node_text(&n, source).to_string())
            .unwrap_or_default();

        if !class_name.is_empty() {
            // 在 class 子节点中查找 implements_clause（非 field_name 方式访问）
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "class_heritage" {
                    let mut inner_cursor = child.walk();
                    for heritage_child in child.children(&mut inner_cursor) {
                        if heritage_child.kind() == "implements_clause" {
                            let mut impl_cursor = heritage_child.walk();
                            for type_node in heritage_child.children(&mut impl_cursor) {
                                // 类型标识符节点
                                if type_node.is_named() && type_node.kind() != "implements" {
                                    let trait_name = node_text(&type_node, source).to_string();
                                    if !trait_name.is_empty() && trait_name != "," {
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
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_impls_recursive(child, source, file_path, impls);
    }
}

// ─── 辅助函数 ────────────────────────────────────────────────────────────────

/// 提取函数签名（参数列表部分）
fn extract_function_signature(node: &Node, source: &[u8]) -> String {
    let name = node.child_by_field_name("name")
        .map(|n| node_text(&n, source))
        .unwrap_or("");
    let params = node.child_by_field_name("parameters")
        .map(|n| node_text(&n, source))
        .unwrap_or("()");
    let return_type = node.child_by_field_name("return_type")
        .map(|n| node_text(&n, source))
        .unwrap_or("");

    if return_type.is_empty() {
        format!("{name}{params}")
    } else {
        format!("{name}{params}{return_type}")
    }
}

/// 提取节点前方的注释（JSDoc 或行注释）
fn extract_preceding_comment(node: &Node, source: &[u8]) -> Option<String> {
    let prev = node.prev_sibling();
    if let Some(p) = prev {
        if p.kind() == "comment" {
            let text = node_text(&p, source).to_string();
            return Some(text);
        }
    }
    None
}

/// Check if an import specifier is a Bun built-in module.
///
/// Returns true for exact matches ("bun") and "bun:*" prefixed imports.
/// Used by `normalize_bun_builtin` to decide whether to rewrite the target.
///
/// ## References
/// - Called by `normalize_bun_builtin()` (this file)
/// - References `BUN_BUILTINS` constant (this file)
fn is_bun_builtin(specifier: &str) -> bool {
    BUN_BUILTINS.contains(&specifier) || specifier.starts_with("bun:")
}

/// Normalize a Bun built-in import to a canonical "bun:<module>" form.
///
/// - "bun" → "bun:main" (bare "bun" import = runtime entry point)
/// - "bun:test" → "bun:test" (already in canonical form)
/// - Non-Bun imports → returned unchanged
///
/// ## References
/// - Called by `extract_deps_recursive()` (this file)
/// - Depends on `is_bun_builtin()` (this file)
fn normalize_bun_builtin(specifier: &str) -> String {
    if !is_bun_builtin(specifier) {
        return specifier.to_string();
    }
    if specifier == "bun" {
        // Bare "bun" import maps to the main runtime module
        "bun:main".to_string()
    } else {
        // Already in "bun:xxx" form
        specifier.to_string()
    }
}

/// 去除字符串首尾的引号（'或"或`）
fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('\'') && s.ends_with('\''))
        || (s.starts_with('"') && s.ends_with('"'))
        || (s.starts_with('`') && s.ends_with('`'))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
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

    /// 辅助：解析 TypeScript 源码并返回 tree
    fn parse_ts(source: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_language(Language::TypeScript)).unwrap();
        parser.parse(source, None).unwrap()
    }

    #[test]
    fn test_extract_function_and_class_symbols() {
        let source = r#"
export function greet(name: string): string {
    return `Hello, ${name}`;
}

class UserService {
    async fetchUser(id: number): Promise<User> {
        return await db.get(id);
    }
}

export const add = (a: number, b: number) => a + b;
"#;
        let tree = parse_ts(source);
        let analyzer = TypeScriptAnalyzer::new();
        let result = analyzer.extract_symbols(&tree, source.as_bytes(), "src/app.ts");

        let names: Vec<&str> = result.data.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"greet"), "should find exported function 'greet', got: {:?}", names);
        assert!(names.contains(&"UserService"), "should find class 'UserService', got: {:?}", names);
        assert!(names.contains(&"fetchUser"), "should find method 'fetchUser', got: {:?}", names);
        assert!(names.contains(&"add"), "should find arrow function 'add', got: {:?}", names);

        // 验证可见性
        let greet_sym = result.data.iter().find(|s| s.name == "greet").unwrap();
        assert_eq!(greet_sym.visibility, Visibility::Public);
        assert_eq!(greet_sym.kind, SymbolKind::Function);

        let user_svc = result.data.iter().find(|s| s.name == "UserService").unwrap();
        assert_eq!(user_svc.visibility, Visibility::Private); // 非 export
        assert_eq!(user_svc.kind, SymbolKind::Class);
    }

    #[test]
    fn test_extract_imports_and_deps() {
        let source = r#"
import { useState } from 'react';
import express from 'express';
const fs = require('fs');
"#;
        let tree = parse_ts(source);
        let analyzer = TypeScriptAnalyzer::new();
        let result = analyzer.extract_deps(&tree, source.as_bytes(), "src/index.ts");

        let targets: Vec<&str> = result.data.iter().map(|d| d.target_file.as_str()).collect();
        assert!(targets.contains(&"react"), "should find 'react' dep, got: {:?}", targets);
        assert!(targets.contains(&"express"), "should find 'express' dep, got: {:?}", targets);
        assert!(targets.contains(&"fs"), "should find 'fs' dep from require(), got: {:?}", targets);
    }

    #[test]
    fn test_extract_calls() {
        let source = r#"
const result = fetchData("url");
const instance = new MyClass();
console.log(result);
"#;
        let tree = parse_ts(source);
        let analyzer = TypeScriptAnalyzer::new();
        let result = analyzer.extract_calls(&tree, source.as_bytes(), "src/main.ts");

        let callees: Vec<&str> = result.data.iter().map(|c| c.callee_name.as_str()).collect();
        assert!(callees.contains(&"fetchData"), "should find call to 'fetchData', got: {:?}", callees);
        assert!(callees.contains(&"new MyClass"), "should find 'new MyClass', got: {:?}", callees);
        assert!(callees.iter().any(|c| c.contains("console.log")), "should find 'console.log', got: {:?}", callees);
    }

    #[test]
    fn test_partial_parse_recovery() {
        // 包含语法错误但部分可解析
        let source = r#"
export function validFunc() { return 1; }

// 下面这行有语法错误
const x = @@@broken syntax;

export function anotherValid() { return 2; }
"#;
        let tree = parse_ts(source);
        let analyzer = TypeScriptAnalyzer::new();
        let result = analyzer.extract_symbols(&tree, source.as_bytes(), "src/broken.ts");

        // 即使有错误也应该提取出有效符号
        let names: Vec<&str> = result.data.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"validFunc"), "should recover 'validFunc' despite errors, got: {:?}", names);
        assert!(names.contains(&"anotherValid"), "should recover 'anotherValid' despite errors, got: {:?}", names);
        assert!(result.parse_errors > 0, "should report parse errors");
    }

    #[test]
    fn test_bun_builtin_not_resolved_to_file() {
        let source = r#"
import { serve } from "bun";
import { test, expect } from "bun:test";
import { $ } from "bun:shell";
import { Database } from "bun:sqlite";
import express from "express";
const path = require("bun:ffi");
"#;
        let tree = parse_ts(source);
        let analyzer = TypeScriptAnalyzer::new();
        let result = analyzer.extract_deps(&tree, source.as_bytes(), "src/server.ts");

        let targets: Vec<&str> = result.data.iter().map(|d| d.target_file.as_str()).collect();

        // Bun built-ins should be normalized to "bun:<module>" form
        assert!(targets.contains(&"bun:main"), "bare 'bun' should become 'bun:main', got: {:?}", targets);
        assert!(targets.contains(&"bun:test"), "should preserve 'bun:test', got: {:?}", targets);
        assert!(targets.contains(&"bun:shell"), "should preserve 'bun:shell', got: {:?}", targets);
        assert!(targets.contains(&"bun:sqlite"), "should preserve 'bun:sqlite', got: {:?}", targets);
        assert!(targets.contains(&"bun:ffi"), "should preserve 'bun:ffi' from require(), got: {:?}", targets);

        // Non-Bun imports should remain unchanged
        assert!(targets.contains(&"express"), "non-bun import 'express' should be unchanged, got: {:?}", targets);

        // All deps should have DepKind::Use
        for dep in &result.data {
            assert_eq!(dep.dep_kind, DepKind::Use);
        }
    }

    #[test]
    fn test_bun_builtin_helpers() {
        // is_bun_builtin
        assert!(is_bun_builtin("bun"));
        assert!(is_bun_builtin("bun:test"));
        assert!(is_bun_builtin("bun:shell"));
        assert!(is_bun_builtin("bun:sqlite"));
        assert!(is_bun_builtin("bun:ffi"));
        assert!(is_bun_builtin("bun:jsc"));
        assert!(is_bun_builtin("bun:unknown")); // any bun: prefix
        assert!(!is_bun_builtin("react"));
        assert!(!is_bun_builtin("express"));
        assert!(!is_bun_builtin("bunyan")); // "bunyan" != "bun"

        // normalize_bun_builtin
        assert_eq!(normalize_bun_builtin("bun"), "bun:main");
        assert_eq!(normalize_bun_builtin("bun:test"), "bun:test");
        assert_eq!(normalize_bun_builtin("bun:shell"), "bun:shell");
        assert_eq!(normalize_bun_builtin("react"), "react");
        assert_eq!(normalize_bun_builtin("express"), "express");
    }

    #[test]
    fn test_extract_interface_and_type_alias() {
        let source = r#"
export interface Serializable {
    serialize(): string;
}

export type UserId = string | number;

export enum Status {
    Active,
    Inactive,
}
"#;
        let tree = parse_ts(source);
        let analyzer = TypeScriptAnalyzer::new();
        let result = analyzer.extract_symbols(&tree, source.as_bytes(), "src/types.ts");

        let names: Vec<&str> = result.data.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Serializable"), "should find interface, got: {:?}", names);
        assert!(names.contains(&"UserId"), "should find type alias, got: {:?}", names);
        assert!(names.contains(&"Status"), "should find enum, got: {:?}", names);

        let iface = result.data.iter().find(|s| s.name == "Serializable").unwrap();
        assert_eq!(iface.kind, SymbolKind::Interface);
        assert_eq!(iface.visibility, Visibility::Public);
    }
}
