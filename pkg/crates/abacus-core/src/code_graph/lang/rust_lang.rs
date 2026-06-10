//! Rust 语言分析器 — 最完整的 LanguageAnalyzer 实现
//!
//! ## 职责
//! 从 tree-sitter-rust 语法树中提取：
//! - 符号定义：fn, struct, enum, trait, impl, mod, const, static, type alias, macro_rules!
//! - 函数/方法调用：call_expression, method call
//! - 文件依赖：use 声明 → 路径解析
//! - 实现关系：impl Trait for Type
//!
//! ## 依赖
//! - `tree-sitter`: AST 遍历
//! - `tree-sitter-rust`: Rust grammar 节点类型
//!
//! ## 引用关系
//! - 被 `lang::create_analyzer_registry()` 注册
//! - 被 `Indexer::parallel_parse()` 通过 `LanguageAnalyzer` trait 调用
//!
//! ## 设计约束
//! - 无状态（Send + Sync + 可重入）
//! - 部分解析容错：遇到 ERROR 节点时跳过并继续
//! - 不持有 Parser 实例——Tree 由 Indexer 传入
//!
//! ## tree-sitter-rust 节点类型参考
//! - `function_item`: 顶层 fn / async fn / pub fn
//! - `struct_item`: struct 定义
//! - `enum_item`: enum 定义
//! - `trait_item`: trait 定义
//! - `impl_item`: impl 块（inherent 或 trait impl）
//! - `mod_item`: mod 声明/定义
//! - `const_item`: const 定义
//! - `static_item`: static 定义
//! - `type_item`: type alias
//! - `macro_definition`: macro_rules! 定义
//! - `use_declaration`: use 语句
//! - `call_expression`: 函数调用
//! - `macro_invocation`: 宏调用
//! - `attribute_item`: #[derive(...)] 等属性

use tree_sitter::{Node, Tree};

use super::{
    AnalysisResult, Language, LanguageAnalyzer, RawCall,
    make_symbol_id, node_text, count_errors,
};
use crate::code_graph::{Symbol, SymbolKind, Visibility, FileDep, DepKind, ImplRelation};

// ─── RustAnalyzer ────────────────────────────────────────────────────────────

/// Rust 语言分析器（无状态）
///
/// ## 实现说明
/// 使用深度优先遍历 tree-sitter AST，通过 `node.kind()` 匹配目标节点。
/// 对于嵌套结构（如 impl 块内的方法），通过 parent_id 建立层级关系。
///
/// ## 提取策略
/// - 顶层符号：直接从 source_file 的子节点提取
/// - impl 方法：进入 impl_item → declaration_list → function_item
/// - derive 宏：从 attribute_item 中提取（标记为 SymbolKind::Macro）
#[derive(Default)]
pub struct RustAnalyzer;

impl RustAnalyzer {
    /// 创建 RustAnalyzer 实例
    pub fn new() -> Self {
        Self
    }

    // ─── 符号提取辅助 ────────────────────────────────────────────────────────

    /// 从 function_item 节点提取符号
    ///
    /// 处理：fn, pub fn, async fn, pub async fn, unsafe fn
    fn extract_function_symbol(
        &self,
        node: &Node,
        source: &[u8],
        file_path: &str,
        parent_id: Option<&str>,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = node_text(&name_node, source).to_string();

        let visibility = self.extract_visibility(node, source);
        let signature = self.build_function_signature(node, source);
        let doc_comment = self.extract_doc_comment(node, source);
        let kind = if parent_id.is_some() {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };

        let line = node.start_position().row as u32 + 1;
        let col = node.start_position().column as u32;
        let end_line = Some(node.end_position().row as u32 + 1);
        let id = make_symbol_id(file_path, &name, kind.as_str(), line);

        // 内容 hash（用于检测符号内容变更）
        let hash = {
            use sha1::{Sha1, Digest};
            let content = node_text(node, source);
            let h = Sha1::digest(content.as_bytes());
            format!("{:x}", h)
        };

        Some(Symbol {
            id,
            name,
            kind,
            file: file_path.to_string(),
            line,
            col,
            end_line,
            signature: Some(signature),
            doc_comment,
            visibility,
            parent_id: parent_id.map(|s| s.to_string()),
            hash,
        })
    }

    /// 从 struct_item 节点提取符号
    fn extract_struct_symbol(
        &self,
        node: &Node,
        source: &[u8],
        file_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = node_text(&name_node, source).to_string();
        let visibility = self.extract_visibility(node, source);
        let doc_comment = self.extract_doc_comment(node, source);

        let line = node.start_position().row as u32 + 1;
        let col = node.start_position().column as u32;
        let end_line = Some(node.end_position().row as u32 + 1);
        let id = make_symbol_id(file_path, &name, "struct", line);
        let hash = self.content_hash(node, source);

        Some(Symbol {
            id,
            name,
            kind: SymbolKind::Struct,
            file: file_path.to_string(),
            line,
            col,
            end_line,
            signature: Some(self.build_struct_signature(node, source)),
            doc_comment,
            visibility,
            parent_id: None,
            hash,
        })
    }

    /// 从 enum_item 节点提取符号
    fn extract_enum_symbol(
        &self,
        node: &Node,
        source: &[u8],
        file_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = node_text(&name_node, source).to_string();
        let visibility = self.extract_visibility(node, source);
        let doc_comment = self.extract_doc_comment(node, source);

        let line = node.start_position().row as u32 + 1;
        let col = node.start_position().column as u32;
        let end_line = Some(node.end_position().row as u32 + 1);
        let id = make_symbol_id(file_path, &name, "enum", line);
        let hash = self.content_hash(node, source);

        Some(Symbol {
            id,
            name,
            kind: SymbolKind::Enum,
            file: file_path.to_string(),
            line,
            col,
            end_line,
            signature: None,
            doc_comment,
            visibility,
            parent_id: None,
            hash,
        })
    }

    /// 从 trait_item 节点提取符号
    fn extract_trait_symbol(
        &self,
        node: &Node,
        source: &[u8],
        file_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = node_text(&name_node, source).to_string();
        let visibility = self.extract_visibility(node, source);
        let doc_comment = self.extract_doc_comment(node, source);

        let line = node.start_position().row as u32 + 1;
        let col = node.start_position().column as u32;
        let end_line = Some(node.end_position().row as u32 + 1);
        let id = make_symbol_id(file_path, &name, "trait", line);
        let hash = self.content_hash(node, source);

        Some(Symbol {
            id,
            name,
            kind: SymbolKind::Trait,
            file: file_path.to_string(),
            line,
            col,
            end_line,
            signature: None,
            doc_comment,
            visibility,
            parent_id: None,
            hash,
        })
    }

    /// 从 const_item / static_item 节点提取符号
    fn extract_const_symbol(
        &self,
        node: &Node,
        source: &[u8],
        file_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = node_text(&name_node, source).to_string();
        let visibility = self.extract_visibility(node, source);
        let doc_comment = self.extract_doc_comment(node, source);

        let line = node.start_position().row as u32 + 1;
        let col = node.start_position().column as u32;
        let id = make_symbol_id(file_path, &name, "constant", line);
        let hash = self.content_hash(node, source);

        Some(Symbol {
            id,
            name,
            kind: SymbolKind::Constant,
            file: file_path.to_string(),
            line,
            col,
            end_line: None,
            signature: None,
            doc_comment,
            visibility,
            parent_id: None,
            hash,
        })
    }

    /// 从 type_item 节点提取类型别名符号
    fn extract_type_alias_symbol(
        &self,
        node: &Node,
        source: &[u8],
        file_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = node_text(&name_node, source).to_string();
        let visibility = self.extract_visibility(node, source);
        let doc_comment = self.extract_doc_comment(node, source);

        let line = node.start_position().row as u32 + 1;
        let col = node.start_position().column as u32;
        let id = make_symbol_id(file_path, &name, "type_alias", line);
        let hash = self.content_hash(node, source);

        Some(Symbol {
            id,
            name,
            kind: SymbolKind::TypeAlias,
            file: file_path.to_string(),
            line,
            col,
            end_line: None,
            signature: Some(node_text(node, source).to_string()),
            doc_comment,
            visibility,
            parent_id: None,
            hash,
        })
    }

    /// 从 macro_definition (macro_rules!) 节点提取符号
    fn extract_macro_symbol(
        &self,
        node: &Node,
        source: &[u8],
        file_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = node_text(&name_node, source).to_string();
        let doc_comment = self.extract_doc_comment(node, source);

        let line = node.start_position().row as u32 + 1;
        let col = node.start_position().column as u32;
        let end_line = Some(node.end_position().row as u32 + 1);
        let id = make_symbol_id(file_path, &name, "macro", line);
        let hash = self.content_hash(node, source);

        Some(Symbol {
            id,
            name,
            kind: SymbolKind::Macro,
            file: file_path.to_string(),
            line,
            col,
            end_line,
            signature: None,
            doc_comment,
            // macro_rules! 的可见性由 #[macro_export] 决定，此处默认 Private
            visibility: Visibility::Private,
            parent_id: None,
            hash,
        })
    }

    /// 从 mod_item 节点提取模块符号
    fn extract_mod_symbol(
        &self,
        node: &Node,
        source: &[u8],
        file_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = node_text(&name_node, source).to_string();
        let visibility = self.extract_visibility(node, source);
        let doc_comment = self.extract_doc_comment(node, source);

        let line = node.start_position().row as u32 + 1;
        let col = node.start_position().column as u32;
        let id = make_symbol_id(file_path, &name, "module", line);
        let hash = self.content_hash(node, source);

        Some(Symbol {
            id,
            name,
            kind: SymbolKind::Module,
            file: file_path.to_string(),
            line,
            col,
            end_line: None,
            signature: None,
            doc_comment,
            visibility,
            parent_id: None,
            hash,
        })
    }

    /// 处理 impl_item 节点——提取 impl 块内的方法
    ///
    /// 生成 impl 块本身不作为符号（它不是独立命名实体），
    /// 但其内部的 function_item 作为 Method 符号提取。
    fn extract_impl_methods(
        &self,
        node: &Node,
        source: &[u8],
        file_path: &str,
    ) -> Vec<Symbol> {
        let mut methods = Vec::new();

        // 确定 impl 块的 "parent name"（用于 parent_id）
        // impl Type { ... } → parent = Type
        // impl Trait for Type { ... } → parent = Type
        let type_node = node.child_by_field_name("type");
        let parent_name = type_node
            .map(|n| node_text(&n, source).to_string())
            .unwrap_or_default();

        // 为 impl 块生成一个虚拟 parent_id
        let impl_line = node.start_position().row as u32 + 1;
        let impl_parent_id = make_symbol_id(file_path, &parent_name, "struct", impl_line);

        // 遍历 body (declaration_list)
        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            if cursor.goto_first_child() {
                loop {
                    let child = cursor.node();
                    if child.kind() == "function_item" {
                        if let Some(sym) = self.extract_function_symbol(
                            &child, source, file_path, Some(&impl_parent_id)
                        ) {
                            methods.push(sym);
                        }
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }

        methods
    }

    // ─── 调用提取辅助 ────────────────────────────────────────────────────────

    /// 从 call_expression 提取被调用者名称
    ///
    /// tree-sitter-rust call_expression 结构：
    /// ```text
    /// (call_expression
    ///   function: (identifier) | (scoped_identifier) | (field_expression)
    ///   arguments: (arguments))
    /// ```
    fn extract_call_name(node: &Node, source: &[u8]) -> Option<String> {
        let function_node = node.child_by_field_name("function")?;
        match function_node.kind() {
            "identifier" => Some(node_text(&function_node, source).to_string()),
            "scoped_identifier" => Some(node_text(&function_node, source).to_string()),
            "field_expression" => {
                // obj.method() → extract "method"
                let field = function_node.child_by_field_name("field")?;
                Some(node_text(&field, source).to_string())
            }
            _ => Some(node_text(&function_node, source).to_string()),
        }
    }

    /// 从 macro_invocation 提取宏名
    ///
    /// tree-sitter-rust macro_invocation 结构：
    /// ```text
    /// (macro_invocation
    ///   macro: (identifier) | (scoped_identifier)
    ///   ...)
    /// ```
    fn extract_macro_call_name(node: &Node, source: &[u8]) -> Option<String> {
        let macro_node = node.child_by_field_name("macro")?;
        let name = node_text(&macro_node, source).to_string();
        Some(format!("{}!", name))
    }

    // ─── 依赖提取辅助 ────────────────────────────────────────────────────────

    /// 从 use_declaration 提取依赖路径
    ///
    /// ## 路径解析策略
    /// - `use crate::module::Item` → 解析为 `src/module.rs` 或 `src/module/mod.rs`
    /// - `use super::sibling` → 相对于当前文件的父目录
    /// - `use std::*` / `use external_crate::*` → 记录为外部依赖（target_file = crate name）
    ///
    /// ## 简化
    /// 当前实现只做路径记录，不做文件系统验证。
    /// 实际路径解析在 Indexer commit 阶段通过文件存在性检查完成。
    fn resolve_use_path(
        &self,
        node: &Node,
        source: &[u8],
        file_path: &str,
    ) -> Vec<FileDep> {
        // 获取 use 声明的路径部分
        // use_declaration → argument（scoped_identifier / use_wildcard / use_list 等）
        let argument = match node.child_by_field_name("argument") {
            Some(arg) => arg,
            None => {
                // 回退：直接取整个 use 语句文本
                let text = node_text(node, source).trim().to_string();
                if text.starts_with("use ") {
                    let path_part = text
                        .strip_prefix("use ")
                        .unwrap_or("")
                        .trim_end_matches(';')
                        .trim();
                    return vec![FileDep {
                        source_file: file_path.to_string(),
                        target_file: self.use_path_to_file_path(path_part, file_path),
                        dep_kind: DepKind::Use,
                    }];
                }
                return Vec::new();
            }
        };

        let path_text = node_text(&argument, source).trim().to_string();

        // 处理 use list: use crate::{A, B}
        // 简化处理：取公共前缀作为依赖目标
        let effective_path = if path_text.contains('{') {
            // 取 { 之前的部分
            path_text.split('{').next().unwrap_or("").trim_end_matches("::").to_string()
        } else {
            // 取最后一个 :: 之前的部分作为模块路径
            match path_text.rsplit_once("::") {
                Some((module_path, _item)) => module_path.to_string(),
                None => path_text.clone(),
            }
        };

        if effective_path.is_empty() {
            return Vec::new();
        }

        vec![FileDep {
            source_file: file_path.to_string(),
            target_file: self.use_path_to_file_path(&effective_path, file_path),
            dep_kind: DepKind::Use,
        }]
    }

    /// 将 Rust use path 转换为可能的文件路径
    ///
    /// ## 规则
    /// - `crate::module::sub` → `src/module/sub.rs` 或 `src/module/sub/mod.rs`
    /// - `super::sibling` → 相对路径计算
    /// - `self::child` → 当前模块子路径
    /// - 其他（std / 外部 crate）→ 返回 crate 名作为标记
    ///
    /// 实际路径验证由 Indexer 在 commit 阶段完成。
    fn use_path_to_file_path(&self, use_path: &str, _source_file: &str) -> String {
        let segments: Vec<&str> = use_path.split("::").collect();

        if segments.is_empty() {
            return use_path.to_string();
        }

        match segments[0] {
            "crate" => {
                // crate::a::b → src/a/b.rs
                let module_path = segments[1..].join("/");
                if module_path.is_empty() {
                    "src/lib.rs".to_string()
                } else {
                    format!("src/{}.rs", module_path)
                }
            }
            "super" => {
                // super::module → 父目录/module.rs（简化表示）
                let module_path = segments[1..].join("/");
                format!("super::{}", module_path)
            }
            "self" => {
                // self::child → 当前模块/child.rs
                let module_path = segments[1..].join("/");
                format!("self::{}", module_path)
            }
            _ => {
                // 外部 crate 或 std → 返回 crate 名
                segments[0].to_string()
            }
        }
    }

    // ─── Impl 关系提取辅助 ────────────────────────────────────────────────────

    /// 从 impl_item 提取 trait 实现关系
    ///
    /// ## 节点结构
    /// ```text
    /// (impl_item
    ///   trait: (type_identifier)?   ← trait impl 时存在
    ///   type: (type_identifier)     ← 实现类型
    ///   body: (declaration_list))
    /// ```
    ///
    /// 只有 `impl Trait for Type` 形式产生 ImplRelation，
    /// `impl Type` (inherent impl) 不产生。
    fn extract_impl_relation(
        &self,
        node: &Node,
        source: &[u8],
        file_path: &str,
    ) -> Option<ImplRelation> {
        // trait impl 的标志：存在 trait 字段
        let trait_node = node.child_by_field_name("trait")?;
        let trait_name = node_text(&trait_node, source).to_string();

        let type_node = node.child_by_field_name("type")?;
        let impl_name = node_text(&type_node, source).to_string();

        let line = node.start_position().row as u32 + 1;

        Some(ImplRelation {
            trait_name,
            impl_name,
            impl_file: file_path.to_string(),
            impl_line: line,
        })
    }

    // ─── 通用辅助 ────────────────────────────────────────────────────────────

    /// 提取节点的可见性修饰符
    ///
    /// tree-sitter-rust 的可见性表示为 visibility_modifier 子节点：
    /// - `pub` → Public
    /// - `pub(crate)` → PublicCrate
    /// - 无 → Private
    fn extract_visibility(&self, node: &Node, source: &[u8]) -> Visibility {
        // 遍历直接子节点查找 visibility_modifier
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "visibility_modifier" {
                    let text = node_text(&child, source);
                    return match text {
                        "pub" => Visibility::Public,
                        t if t.contains("crate") => Visibility::PublicCrate,
                        t if t.contains("super") => Visibility::PublicCrate,
                        _ => Visibility::Public,
                    };
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        Visibility::Private
    }

    /// 提取紧邻节点前的文档注释
    ///
    /// ## 规则
    /// - `///` 或 `//!` 开头的连续行注释
    /// - `/** ... */` 块注释
    /// - 必须紧邻目标节点（中间无空行或其他语句）
    ///
    /// ## 实现
    /// 从目标节点向上查找前驱兄弟节点，收集 line_comment / block_comment。
    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        let mut comments = Vec::new();
        let mut current = *node;

        // 向前查找连续的注释节点
        while let Some(prev) = current.prev_sibling() {
            match prev.kind() {
                "line_comment" => {
                    let text = node_text(&prev, source);
                    if text.starts_with("///") || text.starts_with("//!") {
                        comments.push(text.to_string());
                        current = prev;
                    } else {
                        break;
                    }
                }
                "block_comment" => {
                    let text = node_text(&prev, source);
                    if text.starts_with("/**") || text.starts_with("/*!") {
                        comments.push(text.to_string());
                        current = prev;
                    } else {
                        break;
                    }
                }
                "attribute_item" => {
                    // #[...] 属性不中断文档注释收集
                    current = prev;
                }
                _ => break,
            }
        }

        if comments.is_empty() {
            return None;
        }

        // 反转（因为是从下往上收集的）
        comments.reverse();
        Some(comments.join("\n"))
    }

    /// 构造函数签名字符串
    ///
    /// 包含：async? + pub? + fn name(params) -> ReturnType
    /// 不包含函数体。
    fn build_function_signature(&self, node: &Node, source: &[u8]) -> String {
        let mut parts = Vec::new();

        // 收集修饰符和签名部分（到 block/body 之前）
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                match child.kind() {
                    "block" | ";" => break,
                    _ => {
                        parts.push(node_text(&child, source).to_string());
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        parts.join(" ").trim().to_string()
    }

    /// 构造 struct 签名（名称 + 泛型参数）
    fn build_struct_signature(&self, node: &Node, source: &[u8]) -> String {
        let mut parts = Vec::new();
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                match child.kind() {
                    "field_declaration_list" | "ordered_field_declaration_list" | ";" => break,
                    _ => parts.push(node_text(&child, source).to_string()),
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        parts.join(" ").trim().to_string()
    }

    /// 计算节点内容 hash
    fn content_hash(&self, node: &Node, source: &[u8]) -> String {
        use sha1::{Sha1, Digest};
        let content = node_text(node, source);
        let h = Sha1::digest(content.as_bytes());
        format!("{:x}", h)
    }

    // ─── AST 遍历核心 ────────────────────────────────────────────────────────

    /// 深度优先遍历收集符号
    ///
    /// 只处理顶层和 impl 块内的节点，不递归进入函数体。
    fn walk_for_symbols(&self, tree: &Tree, source: &[u8], file_path: &str) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        let root = tree.root_node();
        let mut cursor = root.walk();

        if !cursor.goto_first_child() {
            return symbols;
        }

        loop {
            let node = cursor.node();

            // 跳过 ERROR 节点
            if node.is_error() || node.is_missing() {
                if !cursor.goto_next_sibling() {
                    break;
                }
                continue;
            }

            match node.kind() {
                "function_item" => {
                    if let Some(sym) = self.extract_function_symbol(&node, source, file_path, None) {
                        symbols.push(sym);
                    }
                }
                "struct_item" => {
                    if let Some(sym) = self.extract_struct_symbol(&node, source, file_path) {
                        symbols.push(sym);
                    }
                }
                "enum_item" => {
                    if let Some(sym) = self.extract_enum_symbol(&node, source, file_path) {
                        symbols.push(sym);
                    }
                }
                "trait_item" => {
                    if let Some(sym) = self.extract_trait_symbol(&node, source, file_path) {
                        symbols.push(sym);
                    }
                }
                "impl_item" => {
                    let methods = self.extract_impl_methods(&node, source, file_path);
                    symbols.extend(methods);
                }
                "const_item" | "static_item" => {
                    if let Some(sym) = self.extract_const_symbol(&node, source, file_path) {
                        symbols.push(sym);
                    }
                }
                "type_item" => {
                    if let Some(sym) = self.extract_type_alias_symbol(&node, source, file_path) {
                        symbols.push(sym);
                    }
                }
                "macro_definition" => {
                    if let Some(sym) = self.extract_macro_symbol(&node, source, file_path) {
                        symbols.push(sym);
                    }
                }
                "mod_item" => {
                    if let Some(sym) = self.extract_mod_symbol(&node, source, file_path) {
                        symbols.push(sym);
                    }
                }
                _ => {}
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }

        symbols
    }

    /// 递归遍历收集调用表达式
    ///
    /// 需要深度遍历函数体（call_expression 出现在函数体内部）。
    fn walk_for_calls(
        &self,
        tree: &Tree,
        source: &[u8],
        file_path: &str,
    ) -> Vec<RawCall> {
        let mut calls = Vec::new();
        let root = tree.root_node();

        // 先收集所有 function_item / impl method 的 (name, id, body_node)
        let function_bodies = self.collect_function_bodies(&root, source, file_path);

        // 对每个函数体递归搜索 call_expression / macro_invocation
        for (caller_id, body_node) in &function_bodies {
            self.collect_calls_in_subtree(body_node, source, caller_id, &mut calls);
        }

        calls
    }

    /// 收集所有函数/方法的 (caller_id, body_node)
    fn collect_function_bodies<'a>(
        &self,
        root: &'a Node<'a>,
        source: &[u8],
        file_path: &str,
    ) -> Vec<(String, Node<'a>)> {
        let mut bodies = Vec::new();
        let mut stack: Vec<Node<'a>> = vec![*root];

        while let Some(node) = stack.pop() {
            match node.kind() {
                "function_item" => {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        let name = node_text(&name_node, source);
                        let line = node.start_position().row as u32 + 1;
                        let parent_in_impl = self.is_inside_impl(&node);
                        let kind_str = if parent_in_impl { "method" } else { "function" };
                        let caller_id = make_symbol_id(file_path, name, kind_str, line);

                        if let Some(body) = node.child_by_field_name("body") {
                            bodies.push((caller_id, body));
                        }
                    }
                }
                "impl_item" => {
                    // 递归进入 impl 块
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        if cursor.goto_first_child() {
                            loop {
                                stack.push(cursor.node());
                                if !cursor.goto_next_sibling() {
                                    break;
                                }
                            }
                        }
                    }
                }
                _ => {
                    // 顶层其他节点——push children
                    let mut cursor = node.walk();
                    if cursor.goto_first_child() {
                        loop {
                            stack.push(cursor.node());
                            if !cursor.goto_next_sibling() {
                                break;
                            }
                        }
                    }
                }
            }
        }

        bodies
    }

    /// 判断 function_item 是否在 impl 块内
    fn is_inside_impl(&self, node: &Node) -> bool {
        let mut parent = node.parent();
        while let Some(p) = parent {
            if p.kind() == "impl_item" {
                return true;
            }
            // 只检查 declaration_list → impl_item
            if p.kind() == "declaration_list" {
                if let Some(pp) = p.parent() {
                    if pp.kind() == "impl_item" {
                        return true;
                    }
                }
            }
            parent = p.parent();
        }
        false
    }

    /// 递归搜索子树中的调用表达式
    fn collect_calls_in_subtree(
        &self,
        node: &Node,
        source: &[u8],
        caller_id: &str,
        calls: &mut Vec<RawCall>,
    ) {
        let mut cursor = node.walk();
        self.walk_calls_recursive(&mut cursor, source, caller_id, calls);
    }

    /// 递归遍历辅助
    fn walk_calls_recursive(
        &self,
        cursor: &mut tree_sitter::TreeCursor,
        source: &[u8],
        caller_id: &str,
        calls: &mut Vec<RawCall>,
    ) {
        loop {
            let node = cursor.node();

            match node.kind() {
                "call_expression" => {
                    if let Some(callee_name) = Self::extract_call_name(&node, source) {
                        calls.push(RawCall {
                            caller_id: caller_id.to_string(),
                            callee_name,
                            call_site_line: node.start_position().row as u32 + 1,
                            call_site_col: node.start_position().column as u32,
                        });
                    }
                }
                "macro_invocation" => {
                    if let Some(macro_name) = Self::extract_macro_call_name(&node, source) {
                        calls.push(RawCall {
                            caller_id: caller_id.to_string(),
                            callee_name: macro_name,
                            call_site_line: node.start_position().row as u32 + 1,
                            call_site_col: node.start_position().column as u32,
                        });
                    }
                }
                _ => {}
            }

            // 递归子节点
            if cursor.goto_first_child() {
                self.walk_calls_recursive(cursor, source, caller_id, calls);
                cursor.goto_parent();
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

// ─── LanguageAnalyzer 实现 ────────────────────────────────────────────────────

impl LanguageAnalyzer for RustAnalyzer {
    fn language(&self) -> Language {
        Language::Rust
    }

    /// 提取 Rust 源文件中的所有符号定义
    ///
    /// ## 提取目标
    /// - `function_item`: fn, async fn, pub fn
    /// - `struct_item`: struct 定义
    /// - `enum_item`: enum 定义
    /// - `trait_item`: trait 定义
    /// - `impl_item`: 内部方法（作为 Method）
    /// - `const_item` / `static_item`: 常量/静态变量
    /// - `type_item`: 类型别名
    /// - `macro_definition`: macro_rules!
    /// - `mod_item`: 模块声明
    fn extract_symbols(
        &self,
        tree: &Tree,
        source: &[u8],
        file_path: &str,
    ) -> AnalysisResult<Vec<Symbol>> {
        let (error_count, error_ranges) = count_errors(tree);
        let symbols = self.walk_for_symbols(tree, source, file_path);
        AnalysisResult::with_errors(symbols, error_count, error_ranges)
    }

    /// 提取 Rust 源文件中的所有调用关系
    ///
    /// ## 提取目标
    /// - `call_expression`: 函数调用（含方法调用）
    /// - `macro_invocation`: 宏调用
    ///
    /// ## 限制
    /// - 不追踪 trait 动态分发的具体目标（需运行时信息）
    /// - 闭包内的调用归属于包含该闭包的函数
    fn extract_calls(
        &self,
        tree: &Tree,
        source: &[u8],
        file_path: &str,
    ) -> AnalysisResult<Vec<RawCall>> {
        let (error_count, error_ranges) = count_errors(tree);
        let calls = self.walk_for_calls(tree, source, file_path);
        AnalysisResult::with_errors(calls, error_count, error_ranges)
    }

    /// 提取 Rust 源文件的依赖关系（use 声明）
    ///
    /// ## 路径解析
    /// - `use crate::module` → `src/module.rs`
    /// - `use super::*` → 父模块
    /// - `use std::*` → 标准库标记
    /// - `use external_crate::*` → 外部 crate 标记
    fn extract_deps(
        &self,
        tree: &Tree,
        source: &[u8],
        file_path: &str,
    ) -> AnalysisResult<Vec<FileDep>> {
        let (error_count, error_ranges) = count_errors(tree);
        let mut deps = Vec::new();
        let root = tree.root_node();
        let mut cursor = root.walk();

        if cursor.goto_first_child() {
            loop {
                let node = cursor.node();
                if node.kind() == "use_declaration" {
                    let mut file_deps = self.resolve_use_path(&node, source, file_path);
                    deps.append(&mut file_deps);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        // 也处理 mod 声明（mod foo; → 隐式依赖）
        cursor = root.walk();
        if cursor.goto_first_child() {
            loop {
                let node = cursor.node();
                if node.kind() == "mod_item" {
                    // mod foo; (无 body) → 依赖 foo.rs 或 foo/mod.rs
                    if node.child_by_field_name("body").is_none() {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let mod_name = node_text(&name_node, source);
                            deps.push(FileDep {
                                source_file: file_path.to_string(),
                                target_file: format!("{}.rs", mod_name),
                                dep_kind: DepKind::Module,
                            });
                        }
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        AnalysisResult::with_errors(deps, error_count, error_ranges)
    }

    /// 提取 trait 实现关系
    ///
    /// 只提取 `impl Trait for Type` 形式。
    /// `impl Type` (inherent impl) 不产生 ImplRelation。
    fn extract_impls(
        &self,
        tree: &Tree,
        source: &[u8],
        file_path: &str,
    ) -> AnalysisResult<Vec<ImplRelation>> {
        let (error_count, error_ranges) = count_errors(tree);
        let mut impls = Vec::new();
        let root = tree.root_node();
        let mut cursor = root.walk();

        if cursor.goto_first_child() {
            loop {
                let node = cursor.node();
                if node.kind() == "impl_item" {
                    if let Some(rel) = self.extract_impl_relation(&node, source, file_path) {
                        impls.push(rel);
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        AnalysisResult::with_errors(impls, error_count, error_ranges)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 辅助：解析 Rust 源码为 tree-sitter Tree
    fn parse_rust(source: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("failed to set rust language");
        parser.parse(source.as_bytes(), None).expect("parse failed")
    }

    /// 测试基本符号提取：函数、结构体、枚举、trait
    #[test]
    fn test_extract_symbols_basic() {
        let source = r#"
/// A public function
pub fn process(input: &str) -> Result<(), Error> {
    do_work(input);
    Ok(())
}

/// Private helper
fn do_work(s: &str) {
    println!("{}", s);
}

/// Main data structure
pub struct Config {
    pub name: String,
    timeout: u64,
}

pub enum Status {
    Active,
    Inactive,
    Error(String),
}

pub trait Handler {
    fn handle(&self, req: Request) -> Response;
}
"#;
        let tree = parse_rust(source);
        let analyzer = RustAnalyzer::new();
        let result = analyzer.extract_symbols(&tree, source.as_bytes(), "src/lib.rs");

        // 应提取出：2 functions + 1 struct + 1 enum + 1 trait = 5 symbols
        assert!(result.data.len() >= 5, "got {} symbols", result.data.len());

        // 验证 process 函数
        let process_fn = result.data.iter().find(|s| s.name == "process").unwrap();
        assert_eq!(process_fn.kind, SymbolKind::Function);
        assert_eq!(process_fn.visibility, Visibility::Public);
        assert!(process_fn.signature.as_ref().unwrap().contains("process"));
        assert!(process_fn.signature.as_ref().unwrap().contains("Result"));
        assert!(process_fn.doc_comment.is_some());

        // 验证 do_work 函数
        let do_work_fn = result.data.iter().find(|s| s.name == "do_work").unwrap();
        assert_eq!(do_work_fn.kind, SymbolKind::Function);
        assert_eq!(do_work_fn.visibility, Visibility::Private);

        // 验证 struct
        let config = result.data.iter().find(|s| s.name == "Config").unwrap();
        assert_eq!(config.kind, SymbolKind::Struct);
        assert_eq!(config.visibility, Visibility::Public);

        // 验证 enum
        let status = result.data.iter().find(|s| s.name == "Status").unwrap();
        assert_eq!(status.kind, SymbolKind::Enum);

        // 验证 trait
        let handler = result.data.iter().find(|s| s.name == "Handler").unwrap();
        assert_eq!(handler.kind, SymbolKind::Trait);
    }

    /// 测试 impl 块内方法提取
    #[test]
    fn test_extract_impl_methods() {
        let source = r#"
pub struct Server {
    port: u16,
}

impl Server {
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    pub async fn start(&self) -> Result<(), Error> {
        listen(self.port).await
    }

    fn internal_helper(&self) {
        // ...
    }
}
"#;
        let tree = parse_rust(source);
        let analyzer = RustAnalyzer::new();
        let result = analyzer.extract_symbols(&tree, source.as_bytes(), "src/server.rs");

        // struct + 3 methods = 4 symbols
        assert!(result.data.len() >= 4, "got {} symbols", result.data.len());

        let new_method = result.data.iter().find(|s| s.name == "new").unwrap();
        assert_eq!(new_method.kind, SymbolKind::Method);
        assert_eq!(new_method.visibility, Visibility::Public);
        assert!(new_method.parent_id.is_some());

        let start_method = result.data.iter().find(|s| s.name == "start").unwrap();
        assert_eq!(start_method.kind, SymbolKind::Method);
        // 验证 async 在签名中
        assert!(start_method.signature.as_ref().unwrap().contains("async"));

        let helper = result.data.iter().find(|s| s.name == "internal_helper").unwrap();
        assert_eq!(helper.visibility, Visibility::Private);
    }

    /// 测试函数签名提取
    #[test]
    fn test_function_signature_extraction() {
        let source = r#"
pub async fn fetch_data<T: Serialize>(url: &str, timeout: Duration) -> Result<T, FetchError> {
    todo!()
}
"#;
        let tree = parse_rust(source);
        let analyzer = RustAnalyzer::new();
        let result = analyzer.extract_symbols(&tree, source.as_bytes(), "src/net.rs");

        assert_eq!(result.data.len(), 1);
        let sig = result.data[0].signature.as_ref().unwrap();
        assert!(sig.contains("async"), "signature should contain 'async': {sig}");
        assert!(sig.contains("fetch_data"), "signature should contain function name: {sig}");
        assert!(sig.contains("Result<T, FetchError>"), "signature should contain return type: {sig}");
    }

    /// 测试调用提取
    #[test]
    fn test_extract_calls() {
        let source = r#"
fn main() {
    let config = Config::load("config.toml");
    let result = process(&config);
    println!("done: {:?}", result);
    config.validate();
}

fn process(config: &Config) -> Result<(), Error> {
    helper_a();
    config.transform();
    Ok(())
}
"#;
        let tree = parse_rust(source);
        let analyzer = RustAnalyzer::new();
        let result = analyzer.extract_calls(&tree, source.as_bytes(), "src/main.rs");

        // main 内: Config::load, process, println!, validate
        // process 内: helper_a, transform, Ok
        assert!(result.data.len() >= 5, "got {} calls", result.data.len());

        // 验证 process 调用存在
        let process_call = result.data.iter().find(|c| c.callee_name == "process");
        assert!(process_call.is_some(), "should find 'process' call");

        // 验证宏调用
        let println_call = result.data.iter().find(|c| c.callee_name == "println!");
        assert!(println_call.is_some(), "should find 'println!' macro call");

        // 验证方法调用（field_expression → 提取方法名）
        let validate_call = result.data.iter().find(|c| c.callee_name == "validate");
        assert!(validate_call.is_some(), "should find 'validate' method call");
    }

    /// 测试依赖提取
    #[test]
    fn test_extract_deps() {
        let source = r#"
use crate::config::Config;
use crate::utils::{helper_a, helper_b};
use super::parent_module;
use std::collections::HashMap;
use serde::Serialize;

mod child_module;

fn main() {}
"#;
        let tree = parse_rust(source);
        let analyzer = RustAnalyzer::new();
        let result = analyzer.extract_deps(&tree, source.as_bytes(), "src/main.rs");

        assert!(!result.data.is_empty(), "should extract deps");

        // 验证 crate 内部依赖解析
        let crate_dep = result.data.iter().find(|d| d.target_file.contains("config"));
        assert!(crate_dep.is_some(), "should resolve crate::config");

        // 验证 mod 声明产生 Module 类型依赖
        let mod_dep = result.data.iter().find(|d| d.dep_kind == DepKind::Module);
        assert!(mod_dep.is_some(), "should extract mod dependency");
        assert!(
            mod_dep.unwrap().target_file.contains("child_module"),
            "mod dep should reference child_module"
        );
    }

    /// 测试 impl trait 关系提取
    #[test]
    fn test_extract_impls() {
        let source = r#"
pub trait Serializer {
    fn serialize(&self) -> Vec<u8>;
}

pub struct JsonSerializer;

impl Serializer for JsonSerializer {
    fn serialize(&self) -> Vec<u8> {
        vec![]
    }
}

impl JsonSerializer {
    pub fn new() -> Self {
        Self
    }
}
"#;
        let tree = parse_rust(source);
        let analyzer = RustAnalyzer::new();
        let result = analyzer.extract_impls(&tree, source.as_bytes(), "src/serial.rs");

        // 只有 `impl Serializer for JsonSerializer` 应产生关系
        // `impl JsonSerializer` (inherent) 不产生
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].trait_name, "Serializer");
        assert_eq!(result.data[0].impl_name, "JsonSerializer");
        assert_eq!(result.data[0].impl_file, "src/serial.rs");
    }

    /// 测试部分解析恢复（文件含语法错误）
    ///
    /// tree-sitter 的 error recovery 允许在有语法错误的文件中
    /// 仍然提取出有效节点。验证 RustAnalyzer 不会因 ERROR 节点崩溃。
    #[test]
    fn test_partial_parse_recovery() {
        // 使用明确的语法错误：非法 token 强制 tree-sitter 产生 ERROR 节点
        let source = r#"
/// This function is valid
pub fn valid_function(x: i32) -> i32 {
    x + 1
}

// Definitely broken: stray tokens that can't form valid Rust
pub fn @@@ broken !!! invalid syntax {{{

pub struct ValidStruct {
    pub field: u32,
}
"#;
        let tree = parse_rust(source);
        let analyzer = RustAnalyzer::new();
        let result = analyzer.extract_symbols(&tree, source.as_bytes(), "src/broken.rs");

        // tree-sitter 应该产生 ERROR 节点（@@@ 和 !!! 是非法 token）
        assert!(result.parse_errors > 0, "should report parse errors for invalid tokens");

        // 但仍应提取出部分有效符号
        assert!(
            !result.data.is_empty(),
            "should still extract symbols from partially-parsed file"
        );

        // 验证能找到有效的 valid_function
        let valid = result.data.iter().find(|s| s.name == "valid_function");
        assert!(valid.is_some(), "should extract valid_function despite errors");
    }

    /// 测试 const / static / type alias / macro_rules 提取
    #[test]
    fn test_extract_misc_symbols() {
        let source = r#"
pub const MAX_RETRIES: u32 = 3;

static GLOBAL_COUNTER: AtomicU64 = AtomicU64::new(0);

pub type Result<T> = std::result::Result<T, AppError>;

macro_rules! log_info {
    ($($arg:tt)*) => {
        println!("[INFO] {}", format!($($arg)*));
    };
}

pub mod submodule;
"#;
        let tree = parse_rust(source);
        let analyzer = RustAnalyzer::new();
        let result = analyzer.extract_symbols(&tree, source.as_bytes(), "src/lib.rs");

        let names: Vec<&str> = result.data.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"MAX_RETRIES"), "should extract const");
        assert!(names.contains(&"GLOBAL_COUNTER"), "should extract static");
        assert!(names.contains(&"Result"), "should extract type alias");
        assert!(names.contains(&"log_info"), "should extract macro_rules");
        assert!(names.contains(&"submodule"), "should extract mod declaration");

        // 验证 kind 正确
        let max_retries = result.data.iter().find(|s| s.name == "MAX_RETRIES").unwrap();
        assert_eq!(max_retries.kind, SymbolKind::Constant);
        assert_eq!(max_retries.visibility, Visibility::Public);

        let result_alias = result.data.iter().find(|s| s.name == "Result").unwrap();
        assert_eq!(result_alias.kind, SymbolKind::TypeAlias);

        let macro_sym = result.data.iter().find(|s| s.name == "log_info").unwrap();
        assert_eq!(macro_sym.kind, SymbolKind::Macro);

        let mod_sym = result.data.iter().find(|s| s.name == "submodule").unwrap();
        assert_eq!(mod_sym.kind, SymbolKind::Module);
    }

    /// 测试 symbol_id 的确定性
    #[test]
    fn test_symbol_id_determinism() {
        let source = "pub fn hello() {}";
        let tree = parse_rust(source);
        let analyzer = RustAnalyzer::new();

        let result1 = analyzer.extract_symbols(&tree, source.as_bytes(), "src/lib.rs");
        let result2 = analyzer.extract_symbols(&tree, source.as_bytes(), "src/lib.rs");

        assert_eq!(result1.data[0].id, result2.data[0].id);
    }

    /// 测试空文件不 panic
    #[test]
    fn test_empty_file() {
        let source = "";
        let tree = parse_rust(source);
        let analyzer = RustAnalyzer::new();

        let symbols = analyzer.extract_symbols(&tree, source.as_bytes(), "src/empty.rs");
        let calls = analyzer.extract_calls(&tree, source.as_bytes(), "src/empty.rs");
        let deps = analyzer.extract_deps(&tree, source.as_bytes(), "src/empty.rs");
        let impls = analyzer.extract_impls(&tree, source.as_bytes(), "src/empty.rs");

        assert!(symbols.data.is_empty());
        assert!(calls.data.is_empty());
        assert!(deps.data.is_empty());
        assert!(impls.data.is_empty());
    }
}
