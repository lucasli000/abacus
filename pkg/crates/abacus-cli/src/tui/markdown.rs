//! Markdown → ratatui Line 渲染器
//!
//! 使用 pulldown-cmark 解析 Markdown，输出带样式的 ratatui Line 序列。
//! 支持：标题、代码块（带语言标注 + 行号）、行内代码、加粗、斜体、列表、引用块、表格。
//!
//! 引用关系：被 components/mod.rs 的 build_message_lines 调用
//! 生命周期：每次消息渲染时调用（通过缓存减少重复解析）

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, CodeBlockKind};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tui::effects;
use crate::tui::syntax;
use crate::tui::theme::{TextRole, Theme};

/// 将 Markdown 文本渲染为 ratatui Line 序列
///
/// 每行前缀由调用方添加（色条 + 缩进），此函数只负责内容样式。
/// 表格使用默认 max_width=80, 调用方若知精确可用宽度应改用 `render_markdown_bounded`
pub fn render_markdown(text: &str, theme: &Theme, is_user: bool) -> Vec<StyledLine> {
    render_markdown_bounded(text, theme, is_user, 80)
}

/// V27: 同 render_markdown 但支持显式表格最大宽度限制
/// 引用关系: 被 build_message_lines 调用,传入 content_width-bar_indent-1 让表格不溢出
pub fn render_markdown_bounded(text: &str, theme: &Theme, is_user: bool, max_width: usize) -> Vec<StyledLine> {
    let mut renderer = MdRenderer::new(theme, is_user, max_width);
    renderer.render(text);
    renderer.output
}

/// 一行渲染结果
#[derive(Clone)]
pub struct StyledLine {
    pub spans: Vec<StyledSpan>,
    pub line_type: LineType,
    /// 代码块行号（Some(n) = 代码内容行 1-based；None = 其他行或 diff 模式行）
    pub line_num: Option<u32>,
}

/// 行类型（用于调用方决定缩进、折叠判断等）
#[derive(Clone, Copy, PartialEq)]
pub enum LineType {
    Normal,
    Code,       // 代码块内容行
    CodeFence,  // ``` 标记行（▾/▴）
    Heading,    // # 标题
    Quote,      // > 引用
    ListItem,   // - 列表项
    Empty,      // 空行
    Table,      // V27: 表格行 — 自带完整布局,豁免 word-wrap 与额外缩进
}

/// 带样式的文本片段
#[derive(Clone)]
pub struct StyledSpan {
    pub text: String,
    pub style: Style,
}

struct MdRenderer<'a> {
    theme: &'a Theme,
    is_user: bool,
    /// V27: 表格最大显示宽度——超过则按比例缩小列宽并截断 cell
    max_width: usize,
    output: Vec<StyledLine>,
    current_spans: Vec<StyledSpan>,
    in_code_block: bool,
    code_lang: String,
    /// 当前代码块已处理的行数（每次 CodeBlock 开始时归零，用于生成 1-based 行号）
    code_line_num: u32,
    in_heading: bool,
    heading_level: u8,
    in_emphasis: bool,
    in_strong: bool,
    in_quote: bool,
    list_depth: usize,
    // ── 表格状态 ────────────────────────────────────────────────
    in_table: bool,
    /// 所有已完成的行（索引 0 = 表头行）
    table_rows: Vec<Vec<String>>,
    /// 当前正在构建的行（逐格填充）
    current_table_row: Vec<String>,
    /// 当前单元格文本缓冲
    current_cell_buf: String,
}

impl<'a> MdRenderer<'a> {
    fn new(theme: &'a Theme, is_user: bool, max_width: usize) -> Self {
        Self {
            theme,
            is_user,
            max_width,
            output: Vec::new(),
            current_spans: Vec::new(),
            in_code_block: false,
            code_lang: String::new(),
            code_line_num: 0,
            in_heading: false,
            heading_level: 0,
            in_emphasis: false,
            in_strong: false,
            in_quote: false,
            list_depth: 0,
            in_table: false,
            table_rows: Vec::new(),
            current_table_row: Vec::new(),
            current_cell_buf: String::new(),
        }
    }

    fn render(&mut self, text: &str) {
        let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
        let parser = Parser::new_ext(text, options);

        for event in parser {
            match event {
                Event::Start(tag) => self.handle_start(tag),
                Event::End(tag) => self.handle_end(tag),
                Event::Text(text) => self.handle_text(&text),
                Event::Code(code) => self.handle_inline_code(&code),
                Event::SoftBreak | Event::HardBreak => self.flush_line(LineType::Normal),
                Event::Rule => {
                    self.flush_line(LineType::Normal);
                    self.current_spans.push(StyledSpan {
                        text: "────────────────────────────".to_string(),
                        style: Style::default().fg(self.theme.border).add_modifier(Modifier::DIM),
                    });
                    self.flush_line(LineType::Normal);
                }
                _ => {}
            }
        }
        // Flush remaining
        if !self.current_spans.is_empty() {
            self.flush_line(LineType::Normal);
        }
    }

    fn handle_start(&mut self, tag: Tag) {
        match tag {
            Tag::CodeBlock(kind) => {
                self.flush_line(LineType::Normal);
                self.in_code_block = true;
                self.code_line_num = 0;
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                // 输出 ╭── code 标记行（box drawing 风格）
                let label = if self.code_lang.is_empty() {
                    "╭── code".to_string()
                } else {
                    format!("╭── code · {}", self.code_lang)
                };
                self.current_spans.push(StyledSpan {
                    text: label,
                    style: Style::default().fg(self.theme.gold),
                });
                self.current_spans.push(StyledSpan {
                    text: " ─────────────────".to_string(),
                    style: Style::default().fg(self.theme.border).add_modifier(Modifier::DIM),
                });
                self.flush_line(LineType::CodeFence);

            }
            Tag::Heading { level, .. } => {
                self.flush_line(LineType::Normal);
                self.in_heading = true;
                self.heading_level = level as u8;
            }
            Tag::Emphasis => {
                self.in_emphasis = true;
            }
            Tag::Strong => {
                self.in_strong = true;
            }
            Tag::BlockQuote => {
                self.in_quote = true;
            }
            Tag::List(_) => {
                self.list_depth += 1;
            }
            Tag::Item => {
                self.flush_line(LineType::Normal);
                let indent = "  ".repeat(self.list_depth.saturating_sub(1));
                self.current_spans.push(StyledSpan {
                    text: format!("{}• ", indent),
                    style: Style::default().fg(self.theme.accent),
                });
            }
            // MD1: pulldown-cmark Paragraph Start 不需要主动行为；
            // 段落结束的换行由 TagEnd::Paragraph → flush_line 处理。
            // 历史此处曾打算"段间补空行"，最终未实现，删除空逻辑壳。
            Tag::Paragraph => {}
            // ── 表格：开始收集行，等 End(Table) 时统一绘制 ──────────────
            Tag::Table(_) => {
                self.flush_line(LineType::Normal);
                self.in_table = true;
                self.table_rows = Vec::new();
            }
            // TableHead 是 thead 容器，实际行由内部 TableRow 负责
            Tag::TableRow => {
                self.current_table_row = Vec::new();
            }
            Tag::TableCell => {
                self.current_cell_buf = String::new();
            }
            _ => {}
        }
    }

    fn handle_end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                // 输出 ╰── /code 标记行（box drawing 风格）
                self.current_spans.push(StyledSpan {
                    text: "╰── /code".to_string(),
                    style: Style::default().fg(self.theme.gold),
                });
                self.current_spans.push(StyledSpan {
                    text: " ─────────────────".to_string(),
                    style: Style::default().fg(self.theme.border).add_modifier(Modifier::DIM),
                });
                self.flush_line(LineType::CodeFence);
            }
            TagEnd::Heading(_) => {
                let level = self.heading_level;
                self.in_heading = false;
                self.flush_line(LineType::Heading);
                // H1/H2 追加底部分隔线（termimad 风格）
                if level <= 2 {
                    self.current_spans.push(StyledSpan {
                        text: "───────────────────────────".to_string(),
                        style: Style::default().fg(self.theme.border).add_modifier(Modifier::DIM),
                    });
                    self.flush_line(LineType::Normal);
                }
            }
            TagEnd::Emphasis => {
                self.in_emphasis = false;
            }
            TagEnd::Strong => {
                self.in_strong = false;
            }
            TagEnd::BlockQuote => {
                self.in_quote = false;
            }
            TagEnd::List(_) => {
                self.list_depth = self.list_depth.saturating_sub(1);
            }
            TagEnd::Item => {
                self.flush_line(LineType::ListItem);
            }
            TagEnd::Paragraph => {
                self.flush_line(LineType::Normal);
            }
            // ── 表格结束事件 ────────────────────────────────────────────
            TagEnd::TableCell => {
                self.current_table_row.push(std::mem::take(&mut self.current_cell_buf));
            }
            TagEnd::TableRow => {
                if !self.current_table_row.is_empty() {
                    self.table_rows.push(std::mem::take(&mut self.current_table_row));
                }
            }
            TagEnd::Table => {
                self.render_table();
                self.in_table = false;
                self.table_rows = Vec::new();
            }
            _ => {}
        }
    }

    fn handle_text(&mut self, text: &str) {
        // 表格单元格内容：缓冲到 current_cell_buf，等整行完成后统一排版
        if self.in_table {
            self.current_cell_buf.push_str(text);
            return;
        }

        if self.in_code_block {
            // 代码块内容：语法高亮 或 Diff 渲染
            if effects::is_diff_content(text) {
                // Diff 模式：+绿 -红 @蓝（diff 自带行号语义，不叠加行号）
                for line in text.lines() {
                    let diff_type = effects::detect_diff_line(line);
                    let style = effects::diff_style(
                        diff_type,
                        self.theme.success,
                        self.theme.error,
                        self.theme.accent,
                    );
                    self.current_spans.push(StyledSpan {
                        text: line.to_string(),
                        style,
                    });
                    self.flush_line(LineType::Code);
                }
            } else {
                // 语法高亮模式：使用 syntect，追加 1-based 行号
                let highlighted = syntax::highlight_code(text, &self.code_lang, self.theme);
                for line_spans in highlighted {
                    // 将 syntect Span 转为 StyledSpan
                    for span in line_spans {
                        self.current_spans.push(StyledSpan {
                            text: span.content.to_string(),
                            style: span.style,
                        });
                    }
                    let num = self.code_line_num;
                    self.code_line_num += 1;
                    self.flush_line_with_num(LineType::Code, Some(num + 1));
                }
            }
            return;
        }

        let style = self.compute_text_style();

        // 按行分割（Markdown 文本可能含换行）
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            return;
        }
        for (i, line) in lines.iter().enumerate() {
            self.current_spans.push(StyledSpan {
                text: line.to_string(),
                style,
            });
            // 中间行需要 flush
            if i < lines.len() - 1 {
                self.flush_line(if self.in_quote { LineType::Quote } else { LineType::Normal });
            }
        }
    }

    fn handle_inline_code(&mut self, code: &str) {
        // F2：去掉源码反引号；gold + DIM 样式仍提供视觉区分
        self.current_spans.push(StyledSpan {
            text: code.to_string(),
            style: self.theme.text_style(TextRole::InlineCode),
        });
    }

    /// 表格渲染：收集完所有行后一次性绘制 box-drawing 边框
    ///
    /// 引用关系：由 handle_end(TagEnd::Table) 调用
    /// 第 0 行为表头（accent + BOLD），其余行为数据行（text 色）。
    /// 列宽根据所有行内容的最大字符数动态计算（最小 3）。
    fn render_table(&mut self) {
        if self.table_rows.is_empty() { return; }
        let col_count = self.table_rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if col_count == 0 { return; }

        // 动态列宽（至少 3 列）——按显示列宽（CJK 全角=2 列）
        // 统一走 tui::util::display_width，避免再次落入 chars().count() 陷阱
        use crate::tui::util::{display_width, pad_to_width, truncate_to_width};
        let mut col_widths: Vec<usize> = vec![3; col_count];
        for row in &self.table_rows {
            for (i, cell) in row.iter().enumerate() {
                col_widths[i] = col_widths[i].max(display_width(cell.as_str()));
            }
        }

        // V27: 列宽收缩 — 总宽超过 max_width 时按比例缩小
        // 边框开销：每列 "│ ... " (1 + 1 + w + 1) + 末尾 │ = col_count*3 + 1
        let overhead = col_count.saturating_mul(3).saturating_add(1);
        let total_content: usize = col_widths.iter().sum();
        if total_content + overhead > self.max_width && self.max_width > overhead {
            let avail = self.max_width.saturating_sub(overhead);
            // 按比例缩放（保底每列 3）
            let scaled: Vec<usize> = col_widths
                .iter()
                .map(|w| {
                    let v = (*w as u64 * avail as u64 / total_content.max(1) as u64) as usize;
                    v.max(3)
                })
                .collect();
            col_widths = scaled;
        }

        let border_style = Style::default().fg(self.theme.border);
        let header_style = self.theme.text_style(TextRole::H2);
        let cell_style   = self.theme.text_style(TextRole::Body);

        // ┌──────┬──────┐
        let top = table_border_row(&col_widths, '┌', '┬', '┐', '─');
        self.current_spans.push(StyledSpan { text: top, style: border_style });
        self.flush_line(LineType::Table);

        // 借走 table_rows 以避免迭代时 &self 借用阻塞 self.flush_line(&mut self)
        let rows = std::mem::take(&mut self.table_rows);
        let total_rows = rows.len();
        for (row_idx, row) in rows.iter().enumerate() {
            // V29.11 修复: │ 竖线用 border_style, cell 内容用 header/cell style
            // 之前整行一个 span → │ 也染了文本色, 视觉上左右边框"有颜色"
            let text_style = if row_idx == 0 { header_style } else { cell_style };
            self.current_spans.push(StyledSpan { text: "│".to_string(), style: border_style });
            for (i, w) in col_widths.iter().enumerate() {
                let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
                // V27: cell 超过列宽 → 截断 + 省略号
                let cell_owned = if display_width(cell) > *w {
                    let target = w.saturating_sub(1).max(1);
                    let mut t = truncate_to_width(cell, target);
                    t.push('…');
                    t
                } else {
                    cell.to_string()
                };
                let padded = format!(" {} ", pad_to_width(&cell_owned, *w));
                self.current_spans.push(StyledSpan { text: padded, style: text_style });
                self.current_spans.push(StyledSpan { text: "│".to_string(), style: border_style });
            }
            self.flush_line(LineType::Table);

            // ├──────┼──────┤ 表头分隔线
            if row_idx == 0 && total_rows > 1 {
                let sep = table_border_row(&col_widths, '├', '┼', '┤', '─');
                self.current_spans.push(StyledSpan { text: sep, style: border_style });
                self.flush_line(LineType::Table);
            }
        }

        // └──────┴──────┘
        let bottom = table_border_row(&col_widths, '└', '┴', '┘', '─');
        self.current_spans.push(StyledSpan { text: bottom, style: border_style });
        self.flush_line(LineType::Table);
    }

    fn compute_text_style(&self) -> Style {
        let mut style = Style::default().fg(self.theme.text);

        if self.is_user {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.in_heading {
            style = style.fg(self.theme.accent).add_modifier(Modifier::BOLD);
        }
        if self.in_strong {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.in_emphasis {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.in_quote {
            style = style.fg(self.theme.muted).add_modifier(Modifier::ITALIC);
        }

        style
    }

    /// 带行号的 flush（代码块语法高亮行使用）
    fn flush_line_with_num(&mut self, line_type: LineType, line_num: Option<u32>) {
        if self.current_spans.is_empty() && line_type != LineType::Empty {
            self.output.push(StyledLine { spans: vec![], line_type: LineType::Empty, line_num: None });
            return;
        }
        let spans = std::mem::take(&mut self.current_spans);
        self.output.push(StyledLine { spans, line_type, line_num });
    }

    fn flush_line(&mut self, line_type: LineType) {
        self.flush_line_with_num(line_type, None);
    }
}

/// 构建表格边框行（box-drawing 字符）
///
/// 引用关系：被 MdRenderer::render_table() 调用
fn table_border_row(col_widths: &[usize], left: char, mid: char, right: char, fill: char) -> String {
    let mut s = left.to_string();
    for (i, w) in col_widths.iter().enumerate() {
        s.push_str(&fill.to_string().repeat(w + 2)); // 各列 padding 左右各 1
        if i + 1 < col_widths.len() {
            s.push(mid);
        }
    }
    s.push(right);
    s
}

/// 将 StyledLine 转为 ratatui Line（带色条前缀）
///
/// 代码行（line_num.is_some()）：使用右对齐行号替代固定缩进，暗灰色显示。
/// 其他行：使用 line_type 决定的固定缩进。
///
/// 引用关系：被 components/mod.rs 的 build_message_lines 调用
/// 生命周期：每帧消息渲染时按需调用
pub fn styled_line_to_ratatui(
    styled: &StyledLine,
    bar: &Span<'static>,
    theme: &Theme,
) -> Line<'static> {
    if styled.spans.is_empty() {
        return Line::from(vec![bar.clone()]);
    }

    let (indent, indent_style): (String, Style) = if let Some(n) = styled.line_num {
        // 代码行：右对齐行号 + │ 分隔符，使用主题 muted（跟随明/暗主题切换）
        (format!("{:>3}│ ", n), theme.text_style(TextRole::Caption))
    } else {
        let s = match styled.line_type {
            LineType::Code     => "    ",   // 4 空格（diff 行，无行号）
            LineType::CodeFence => "   ",   // 3 空格（▾/▴ 标记行）
            LineType::Heading  => "   ",
            LineType::Quote    => "   ▎ ",  // 引用块柔和竖线 (U+258E LIGHT VERTICAL BAR)
            LineType::ListItem => "   ",
            LineType::Normal | LineType::Empty => "   ",
            LineType::Table    => "",        // V27: 表格行自带布局，豁免额外缩进（见 enum 定义注释）
        };
        (s.to_string(), Style::default())
    };

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(styled.spans.len() + 2);
    spans.push(bar.clone());
    spans.push(Span::styled(indent, indent_style));

    for s in &styled.spans {
        spans.push(Span::styled(s.text.clone(), s.style));
    }

    Line::from(spans)
}

#[cfg(test)]
mod st5_streaming_tests {
    //! ST5 回归：流式 markdown 必须能识别完整的多行结构
    //!
    //! 原 bug：按 \n 边界增量解析时，``` 跨 chunk delta 会被两段独立解析破坏 code block 上下文。
    //! 修复：渲染层每帧整段重解析 streaming_text，pulldown-cmark 看到完整闭合 fence 即可正确识别。
    //!
    //! 这些测试不直接调用渲染层（涉及 ratatui 状态），而是验证 markdown 层的契约：
    //! "整段解析" 与 "分段拼接的解析" 在结构识别上的关键差异，证明每帧重解析的必要性。

    use super::*;

    fn count_line_types(lines: &[StyledLine]) -> (usize, usize, usize) {
        let mut code = 0;
        let mut fence = 0;
        let mut normal = 0;
        for l in lines {
            match l.line_type {
                LineType::Code => code += 1,
                LineType::CodeFence => fence += 1,
                LineType::Normal => normal += 1,
                _ => {}
            }
        }
        (code, fence, normal)
    }

    #[test]
    fn complete_fenced_code_block_recognized() {
        let theme = Theme::init();
        let md = "```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n";
        let lines = render_markdown(md, &theme, false);
        let (code, fence, _) = count_line_types(&lines);
        // 完整 fence：开 + 闭 共 2 个 CodeFence 行 + 3 行代码内容
        assert_eq!(fence, 2, "完整 ```...``` 应产生 2 个 CodeFence（开+闭）");
        assert!(code >= 3, "应至少 3 行 Code 内容，实得 {}", code);
    }

    #[test]
    fn split_fence_loses_structure_when_parsed_in_pieces() {
        // 证明 ST5 旧路径的 bug 形态：两段分开解析会破坏 code block 结构
        let theme = Theme::init();
        let part1 = "```rust\nfn main() {\n";
        let part2 = "    println!();\n}\n```\n";

        let lines1 = render_markdown(part1, &theme, false);
        let lines2 = render_markdown(part2, &theme, false);
        let (_, fence1, _) = count_line_types(&lines1);
        let (_, fence2, _) = count_line_types(&lines2);

        // part1 单独解析：pulldown-cmark 在 EOF 虚拟闭合 → 仍输出开+闭 CodeFence
        // part2 单独解析：仅看到孤立 ``` 又是新 fence → 又输出开+闭
        // 合计 2+2=4 个 fence 标记 — 旧增量缓存路径会产生这种重复
        assert!(fence1 >= 1 && fence2 >= 1,
            "分段解析双方都至少 1 fence，证明状态无法跨调用持续：part1={}, part2={}",
            fence1, fence2);

        // 对比：完整一次解析只有 2 个 fence
        let merged = format!("{}{}", part1, part2);
        let lines_full = render_markdown(&merged, &theme, false);
        let (_, fence_full, _) = count_line_types(&lines_full);
        assert_eq!(fence_full, 2,
            "完整段落解析应仅产生 2 个 fence（开+闭）— 这就是 ST5 修复后渲染层的行为");

        // 关键断言：分段拼接 ≠ 整段解析
        assert!(fence1 + fence2 > fence_full,
            "分段解析的 fence 总数 ({}+{}) 应严格大于整段 ({}) — 即旧路径会重复结构标记",
            fence1, fence2, fence_full);
    }

    #[test]
    fn streaming_partial_fence_renders_as_code_via_eof_close() {
        // 流式中半段 ``` 仍可读：pulldown-cmark EOF 虚拟闭合让代码内容可见
        // 这是为什么修复后流式体验仍流畅的关键——不会因为 fence 未闭合就显示空白
        let theme = Theme::init();
        let partial = "```python\nprint(\"hello\")\n";
        let lines = render_markdown(partial, &theme, false);
        let (code, fence, _) = count_line_types(&lines);
        assert!(fence >= 1, "未闭合 fence 应仍输出至少 1 个 CodeFence 标记");
        assert!(code >= 1, "代码内容应被识别为 Code 行");
    }
}

