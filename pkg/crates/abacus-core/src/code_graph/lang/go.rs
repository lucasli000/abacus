//! Go 语言分析器
//!
//! ## 职责
//! 实现 `LanguageAnalyzer` trait，从 tree-sitter AST 提取 Go 的符号、
//! 调用关系、import 依赖和接口实现关系。
//!
//! ## 依赖
//! - `tree-sitter`: AST 遍历框架
//! - `tree-sitter-go`: Go grammar
//!
//! ## 引用关系
//! - 被 `super::create_analyzer_registry()` 注册为 Language::Go 的处理器
//! - 被 `Indexer::parallel_parse()` 并行调用（要求 Send + Sync）
//!
//! ## 生命周期
//! - 创建：`create_analyzer_registry()` 时（程序启动一次）
//! - 存活：与 CodeGraphManager 同生命周期（Arc 包装的注册表）
//! - 无可变状态，无需销毁逻辑
//!
//! ## 可见性约定
//! Go 的可见性通过首字母大小写决定：
//! - 首字母大写: Public (exported)
//! - 首字母小写: Private (unexported, package-internal)
//!
//! ## 接口实现说明
//! Go 使用隐式接口实现（structural typing），无 `implements` 关键字。
//! 静态分析无法完整确定接口实现关系（需要方法集匹配），
//! 因此 `extract_impls` 暂返回空 Vec，标记 TODO 待后续实现。

use tree_sitter::{Node, Tree};

use super::{
    AnalysisResult, Language, LanguageAnalyzer, RawCall,
    make_symbol_id, node_text, count_errors,
};
use crate::code_graph::{Symbol, SymbolKind, Visibility, FileDep, DepKind, ImplRelation};

// ─── GoAnalyzer ──────────────────────────────────────────────────────────────

/// Go 无状态分析器
///
/// 处理 `.go` 文件。
/// 无内部状态，满足 Send + Sync 约束。
pub struct GoAnalyzer;

impl GoAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageAnalyzer for GoAnalyzer {
    fn language(&self) -> Language {
        Language::Go
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
        extract_symbols_recursive(root, source, file_path, &mut symbols);
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
        _tree: &Tree,
        _source: &[u8],
        _file_path: &str,
    ) -> AnalysisResult<Vec<ImplRelation>> {
        // TODO: Go 接口实现是隐式的（structural typing）。
        // 完整实现需要收集类型的方法集（method set）并与接口定义做集合匹配。
        // 这需要跨文件分析能力（同 package 内的方法），暂时返回空。
        // 后续可在 Indexer commit 阶段通过已收集的 symbol 信息做匹配。
        let (parse_errors, error_ranges) = count_errors(_tree);
        AnalysisResult::with_errors(Vec::new(), parse_errors, error_ranges)
    }
}

// ─── 符号提取 ────────────────────────────────────────────────────────────────

/// 递归遍历 AST 提取符号定义
///
/// Go 符号类型：
/// - function_declaration: 包级函数 `func Name(...) ...`
/// - method_declaration: 方法 `func (r Receiver) Name(...) ...`
/// - type_declaration > type_spec: struct/interface/type alias
/// - const_declaration: 常量组
/// - var_declaration: 变量组
///
/// 支持部分解析：遇到 ERROR 节点跳过，继续处理兄弟节点。
fn extract_symbols_recursive(
    node: Node,
    source: &[u8],
    file_path: &str,
    symbols: &mut Vec<Symbol>,
) {
    if node.is_error() {
        return;
    }

    let kind = node.kind();

    match kind {
        "function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                let vis = go_visibility(name);
                let sig = extract_func_signature(&node, source);
                let id = make_symbol_id(file_path, name, "function", name_node.start_position().row as u32);

                symbols.push(Symbol {
                    id,
                    name: name.to_string(),
                    kind: SymbolKind::Function,
                    file: file_path.to_string(),
                    line: name_node.start_position().row as u32,
                    col: name_node.start_position().column as u32,
                    end_line: Some(node.end_position().row as u32),
                    signature: Some(sig),
                    doc_comment: extract_preceding_comment(&node, source),
                    visibility: vis,
                    parent_id: None,
                    hash: content_hash(&node, source),
                });
            }
        }

        "method_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                let vis = go_visibility(name);
                let sig = extract_method_signature(&node, source);
                let id = make_symbol_id(file_path, name, "method", name_node.start_position().row as u32);

                // 获取 receiver 类型作为 parent 信息（嵌入签名中）
                symbols.push(Symbol {
                    id,
                    name: name.to_string(),
                    kind: SymbolKind::Method,
                    file: file_path.to_string(),
                    line: name_node.start_position().row as u32,
                    col: name_node.start_position().column as u32,
                    end_line: Some(node.end_position().row as u32),
                    signature: Some(sig),
                    doc_comment: extract_preceding_comment(&node, source),
                    visibility: vis,
                    parent_id: None, // Go 方法无嵌套定义，receiver 在签名中
                    hash: content_hash(&node, source),
                });
            }
        }

        "type_declaration" => {
            // type_declaration 包含一个或多个 type_spec
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec" {
                    extract_type_spec(child, source, file_path, &node, symbols);
                }
            }
            return; // 已处理子节点
        }

        "const_declaration" => {
            // const 声明可包含多个 const_spec
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "const_spec" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = node_text(&name_node, source);
                        if !name.is_empty() {
                            let vis = go_visibility(name);
                            let id = make_symbol_id(file_path, name, "constant", name_node.start_position().row as u32);
                            symbols.push(Symbol {
                                id,
                                name: name.to_string(),
                                kind: SymbolKind::Constant,
                                file: file_path.to_string(),
                                line: name_node.start_position().row as u32,
                                col: name_node.start_position().column as u32,
                                end_line: Some(child.end_position().row as u32),
                                signature: None,
                                doc_comment: extract_preceding_comment(&node, source),
                                visibility: vis,
                                parent_id: None,
                                hash: content_hash(&child, source),
                            });
                        }
                    }
                }
            }
            return;
        }

        "var_declaration" => {
            // var 声明可包含多个 var_spec
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "var_spec" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = node_text(&name_node, source);
                        if !name.is_empty() {
                            let vis = go_visibility(name);
                            let id = make_symbol_id(file_path, name, "variable", name_node.start_position().row as u32);
                            symbols.push(Symbol {
                                id,
                                name: name.to_string(),
                                kind: SymbolKind::Variable,
                                file: file_path.to_string(),
                                line: name_node.start_position().row as u32,
                                col: name_node.start_position().column as u32,
                                end_line: Some(child.end_position().row as u32),
                                signature: None,
                                doc_comment: extract_preceding_comment(&node, source),
                                visibility: vis,
                                parent_id: None,
                                hash: content_hash(&child, source),
                            });
                        }
                    }
                }
            }
            return;
        }

        _ => {}
    }

    // 默认递归子节点
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_symbols_recursive(child, source, file_path, symbols);
    }
}

/// 提取 type_spec 中的类型定义（struct/interface/type alias）
fn extract_type_spec(
    node: Node,
    source: &[u8],
    file_path: &str,
    parent_decl: &Node,
    symbols: &mut Vec<Symbol>,
) {
    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(&name_node, source);
    if name.is_empty() {
        return;
    }

    let vis = go_visibility(name);

    // 判断类型种类：查看 type field
    let (sym_kind, kind_str) = if let Some(type_node) = node.child_by_field_name("type") {
        match type_node.kind() {
            "struct_type" => (SymbolKind::Struct, "struct"),
            "interface_type" => (SymbolKind::Interface, "interface"),
            _ => (SymbolKind::TypeAlias, "type_alias"),
        }
    } else {
        (SymbolKind::TypeAlias, "type_alias")
    };

    let id = make_symbol_id(file_path, name, kind_str, name_node.start_position().row as u32);
    symbols.push(Symbol {
        id,
        name: name.to_string(),
        kind: sym_kind,
        file: file_path.to_string(),
        line: name_node.start_position().row as u32,
        col: name_node.start_position().column as u32,
        end_line: Some(node.end_position().row as u32),
        signature: None,
        doc_comment: extract_preceding_comment(parent_decl, source),
        visibility: vis,
        parent_id: None,
        hash: content_hash(&node, source),
    });
}

// ─── 调用提取 ────────────────────────────────────────────────────────────────

/// 递归提取函数调用
///
/// Go 调用形式：
/// - `call_expression`: 直接调用 `foo(args)` 或方法调用 `obj.Method(args)`
/// - 方法调用在 tree-sitter 中表现为 call_expression 的 function field 是 selector_expression
fn extract_calls_recursive(
    node: Node,
    source: &[u8],
    _file_path: &str,
    calls: &mut Vec<RawCall>,
) {
    if node.is_error() {
        return;
    }

    if node.kind() == "call_expression" {
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

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_calls_recursive(child, source, _file_path, calls);
    }
}

// ─── 依赖提取 ────────────────────────────────────────────────────────────────

/// 递归提取 import 依赖
///
/// Go import 形式：
/// - 单行: `import "fmt"`
/// - 块:   `import ("fmt"; "os")`
/// - 别名: `import f "fmt"`
///
/// tree-sitter 结构：import_declaration → import_spec_list → import_spec → path(interpreted_string_literal)
fn extract_deps_recursive(
    node: Node,
    source: &[u8],
    file_path: &str,
    deps: &mut Vec<FileDep>,
) {
    if node.is_error() {
        return;
    }

    if node.kind() == "import_declaration" {
        // 遍历所有 import_spec
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "import_spec" => {
                    if let Some(path_node) = child.child_by_field_name("path") {
                        let target = strip_quotes(node_text(&path_node, source));
                        if !target.is_empty() {
                            deps.push(FileDep {
                                source_file: file_path.to_string(),
                                target_file: target,
                                dep_kind: DepKind::Use,
                            });
                        }
                    }
                }
                "import_spec_list" => {
                    let mut inner_cursor = child.walk();
                    for spec in child.children(&mut inner_cursor) {
                        if spec.kind() == "import_spec" {
                            if let Some(path_node) = spec.child_by_field_name("path") {
                                let target = strip_quotes(node_text(&path_node, source));
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
                }
                // 单个 interpreted_string_literal (单行 import "fmt")
                "interpreted_string_literal" => {
                    let target = strip_quotes(node_text(&child, source));
                    if !target.is_empty() {
                        deps.push(FileDep {
                            source_file: file_path.to_string(),
                            target_file: target,
                            dep_kind: DepKind::Use,
                        });
                    }
                }
                _ => {}
            }
        }
        return; // 已处理子节点
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_deps_recursive(child, source, file_path, deps);
    }
}

// ─── 辅助函数 ────────────────────────────────────────────────────────────────

/// 根据 Go 命名约定推断可见性
///
/// Go 规则：首字母大写 = exported (Public)，小写 = unexported (Private)
fn go_visibility(name: &str) -> Visibility {
    if name.starts_with(|c: char| c.is_uppercase()) {
        Visibility::Public
    } else {
        Visibility::Private
    }
}

/// 提取函数签名 `func Name(params) returns`
fn extract_func_signature(node: &Node, source: &[u8]) -> String {
    let name = node.child_by_field_name("name")
        .map(|n| node_text(&n, source))
        .unwrap_or("");
    let params = node.child_by_field_name("parameters")
        .map(|n| node_text(&n, source))
        .unwrap_or("()");
    let result = node.child_by_field_name("result")
        .map(|n| format!(" {}", node_text(&n, source)))
        .unwrap_or_default();

    format!("func {name}{params}{result}")
}

/// 提取方法签名 `func (r Receiver) Name(params) returns`
fn extract_method_signature(node: &Node, source: &[u8]) -> String {
    let receiver = node.child_by_field_name("receiver")
        .map(|n| node_text(&n, source))
        .unwrap_or("()");
    let name = node.child_by_field_name("name")
        .map(|n| node_text(&n, source))
        .unwrap_or("");
    let params = node.child_by_field_name("parameters")
        .map(|n| node_text(&n, source))
        .unwrap_or("()");
    let result = node.child_by_field_name("result")
        .map(|n| format!(" {}", node_text(&n, source)))
        .unwrap_or_default();

    format!("func {receiver} {name}{params}{result}")
}

/// 提取节点前方的注释（Go 的 // 或 /* */ 注释）
fn extract_preceding_comment(node: &Node, source: &[u8]) -> Option<String> {
    let mut prev = node.prev_sibling();
    let mut comments = Vec::new();

    // Go 的文档注释是紧邻声明上方的连续 comment 行
    while let Some(p) = prev {
        if p.kind() == "comment" {
            comments.push(node_text(&p, source).to_string());
            prev = p.prev_sibling();
        } else {
            break;
        }
    }

    if comments.is_empty() {
        None
    } else {
        comments.reverse();
        Some(comments.join("\n"))
    }
}

/// 去除字符串首尾的引号
fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"'))
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

    /// 辅助：解析 Go 源码并返回 tree
    fn parse_go(source: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_language(Language::Go)).unwrap();
        parser.parse(source, None).unwrap()
    }

    #[test]
    fn test_extract_functions_and_types() {
        let source = r#"package main

import "fmt"

// Greeter is the main greeter interface.
type Greeter interface {
    Greet(name string) string
}

// User represents a user in the system.
type User struct {
    Name string
    Age  int
}

// NewUser creates a new User.
func NewUser(name string, age int) *User {
    return &User{Name: name, Age: age}
}

func (u *User) Greet(name string) string {
    return fmt.Sprintf("Hello %s, I'm %s", name, u.Name)
}

var defaultTimeout = 30

const MaxRetries = 3
"#;
        let tree = parse_go(source);
        let analyzer = GoAnalyzer::new();
        let result = analyzer.extract_symbols(&tree, source.as_bytes(), "main.go");

        let names: Vec<&str> = result.data.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Greeter"), "should find interface 'Greeter', got: {:?}", names);
        assert!(names.contains(&"User"), "should find struct 'User', got: {:?}", names);
        assert!(names.contains(&"NewUser"), "should find function 'NewUser', got: {:?}", names);
        assert!(names.contains(&"Greet"), "should find method 'Greet', got: {:?}", names);
        assert!(names.contains(&"MaxRetries"), "should find const 'MaxRetries', got: {:?}", names);

        // 验证可见性
        let new_user = result.data.iter().find(|s| s.name == "NewUser").unwrap();
        assert_eq!(new_user.visibility, Visibility::Public);
        assert_eq!(new_user.kind, SymbolKind::Function);

        let greeter = result.data.iter().find(|s| s.name == "Greeter").unwrap();
        assert_eq!(greeter.kind, SymbolKind::Interface);
        assert_eq!(greeter.visibility, Visibility::Public);

        let user = result.data.iter().find(|s| s.name == "User").unwrap();
        assert_eq!(user.kind, SymbolKind::Struct);

        let greet_method = result.data.iter().find(|s| s.name == "Greet").unwrap();
        assert_eq!(greet_method.kind, SymbolKind::Method);
        // 签名应包含 receiver
        assert!(greet_method.signature.as_ref().unwrap().contains("*User"),
            "method signature should contain receiver, got: {:?}", greet_method.signature);
    }

    #[test]
    fn test_extract_imports() {
        let source = r#"package main

import (
    "fmt"
    "os"
    "github.com/gin-gonic/gin"
    log "github.com/sirupsen/logrus"
)
"#;
        let tree = parse_go(source);
        let analyzer = GoAnalyzer::new();
        let result = analyzer.extract_deps(&tree, source.as_bytes(), "main.go");

        let targets: Vec<&str> = result.data.iter().map(|d| d.target_file.as_str()).collect();
        assert!(targets.contains(&"fmt"), "should find 'fmt', got: {:?}", targets);
        assert!(targets.contains(&"os"), "should find 'os', got: {:?}", targets);
        assert!(targets.contains(&"github.com/gin-gonic/gin"),
            "should find gin import, got: {:?}", targets);
        assert!(targets.contains(&"github.com/sirupsen/logrus"),
            "should find logrus import, got: {:?}", targets);
    }

    #[test]
    fn test_extract_calls() {
        let source = r#"package main

import "fmt"

func main() {
    user := NewUser("Alice", 30)
    greeting := user.Greet("Bob")
    fmt.Println(greeting)
}
"#;
        let tree = parse_go(source);
        let analyzer = GoAnalyzer::new();
        let result = analyzer.extract_calls(&tree, source.as_bytes(), "main.go");

        let callees: Vec<&str> = result.data.iter().map(|c| c.callee_name.as_str()).collect();
        assert!(callees.contains(&"NewUser"), "should find 'NewUser', got: {:?}", callees);
        assert!(callees.iter().any(|c| c.contains("user.Greet")),
            "should find method call 'user.Greet', got: {:?}", callees);
        assert!(callees.iter().any(|c| c.contains("fmt.Println")),
            "should find 'fmt.Println', got: {:?}", callees);
    }

    #[test]
    fn test_partial_parse_recovery() {
        let source = r#"package main

func ValidFunc() int {
    return 42
}

// 语法错误
func Broken(@@@ {
}

func AnotherValid() string {
    return "ok"
}
"#;
        let tree = parse_go(source);
        let analyzer = GoAnalyzer::new();
        let result = analyzer.extract_symbols(&tree, source.as_bytes(), "broken.go");

        let names: Vec<&str> = result.data.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"ValidFunc"), "should recover 'ValidFunc', got: {:?}", names);
        assert!(names.contains(&"AnotherValid"), "should recover 'AnotherValid', got: {:?}", names);
        assert!(result.parse_errors > 0, "should report parse errors");
    }

    #[test]
    fn test_go_visibility_convention() {
        assert_eq!(go_visibility("Exported"), Visibility::Public);
        assert_eq!(go_visibility("unexported"), Visibility::Private);
        assert_eq!(go_visibility("_private"), Visibility::Private);
        assert_eq!(go_visibility("URL"), Visibility::Public);
    }

    #[test]
    fn test_impls_returns_empty() {
        // Go 接口实现是隐式的，当前实现返回空
        let source = r#"package main

type Writer interface {
    Write(p []byte) (n int, err error)
}

type MyWriter struct{}

func (w *MyWriter) Write(p []byte) (int, error) {
    return len(p), nil
}
"#;
        let tree = parse_go(source);
        let analyzer = GoAnalyzer::new();
        let result = analyzer.extract_impls(&tree, source.as_bytes(), "writer.go");

        // 当前实现返回空（TODO：后续实现方法集匹配）
        assert!(result.data.is_empty(), "impls should be empty for now (implicit interfaces)");
    }
}
