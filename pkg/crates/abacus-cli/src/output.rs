use std::io::Write;
use std::sync::OnceLock;
use clap::ValueEnum;
use owo_colors::OwoColorize;
use serde::Serialize;

/// Cached terminal width — stty 只调一次
fn terminal_width() -> usize {
    static WIDTH: OnceLock<usize> = OnceLock::new();
    *WIDTH.get_or_init(|| {
        std::env::var("COLUMNS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| {
                std::process::Command::new("stty")
                    .arg("size")
                    .arg("-F")
                    .arg("/dev/tty")
                    .output()
                    .ok()
                    .and_then(|o| {
                        let s = String::from_utf8_lossy(&o.stdout);
                        let w = s.split_whitespace().nth(1)?;
                        w.parse().ok()
                    })
            })
            .unwrap_or(80)
            .max(40)
    })
}

/// Wrap text to terminal width with proper indent prefix
fn wrap_line(line: &str, width: usize, indent: &str) -> String {
    let avail = width.saturating_sub(indent.len());
    if line.len() <= avail || avail < 10 {
        return format!("{}{}", indent, line);
    }
    let mut out = String::new();
    let mut remaining = line;
    let mut first = true;
    while !remaining.is_empty() {
        let prefix = if first { indent } else { &" ".repeat(indent.len()) };
        first = false;
        let take = remaining
            .char_indices()
            .take(avail)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(remaining.len());
        out.push_str(prefix);
        out.push_str(&remaining[..take]);
        out.push('\n');
        remaining = &remaining[take..];
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Output format types
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
#[derive(Default)]
pub enum OutputFormat {
    /// Plain text output
    Text,

    /// Markdown output with colors
    #[default]
    Markdown,

    /// JSON output for script consumption
    Json,

    /// Compact output for events stream
    Compact,

    /// Silent output (no printed output)
    Silent,
}


impl OutputFormat {
    /// Returns true if output format should include ANSI colors
    pub fn supports_colors(&self) -> bool {
        matches!(self, OutputFormat::Markdown | OutputFormat::Compact)
    }

    /// Returns true if output format is machine-readable
    pub fn is_machine_readable(&self) -> bool {
        matches!(self, OutputFormat::Json | OutputFormat::Text)
    }
}

/// Structured output for JSON format
#[derive(Serialize, Debug)]
#[serde(tag = "type", content = "data")]
pub enum JsonOutput {
    /// Regular message from LLM
    Message {
        role: String,
        content: String,
        thinking: Option<String>,
    },

    /// Tool call information
    ToolCall {
        tool: String,
        arguments: serde_json::Value,
        result: Option<serde_json::Value>,
        status: String,
    },

    /// Session information
    Session {
        id: String,
        title: String,
        message_count: u32,
        token_usage: u64,
        created_at: String,
        last_active: String,
    },

    /// Skill information
    Skill {
        id: String,
        name: String,
        description: String,
        enabled: bool,
    },

    /// Configuration information
    Config {
        key: String,
        value: String,
    },

    /// Model information
    Model {
        id: String,
        name: String,
        status: String,
        latency_ms: Option<u64>,
    },

    /// Error information
    Error {
        code: String,
        message: String,
        details: Option<String>,
    },

    /// Done message
    Done {
        exit_code: i32,
        total_tokens: Option<u64>,
        duration_ms: Option<u64>,
    },
}

/// Output formatter trait
pub trait OutputFormatter {
    fn format_message(&self, role: &str, content: &str, thinking: Option<&str>);
    fn format_tool_call(&self, tool: &str, arguments: &serde_json::Value, result: Option<&serde_json::Value>, status: &str);
    fn format_error(&self, code: &str, message: &str, details: Option<&str>);
    fn format_done(&self, exit_code: i32, total_tokens: Option<u64>, duration_ms: Option<u64>);
}

/// Text formatter
pub struct TextFormatter;

impl OutputFormatter for TextFormatter {
    fn format_message(&self, role: &str, content: &str, thinking: Option<&str>) {
        let w = terminal_width();
        println!("{}", "─".repeat(w.min(60)));
        if let Some(thought) = thinking {
            println!("│ {} {}", "💭".blue(), thought);
        }
        let role_display = match role {
            "user" => "🙋 You".yellow().to_string(),
            "assistant" => "🤖 Assistant".green().to_string(),
            "system" => "⚙ System".blue().to_string(),
            "team" => "👥 Team".cyan().to_string(),
            "config" => "🔧 Config".magenta().to_string(),
            r => r.to_string(),
        };
        println!("│ {}", role_display);
        for line in content.lines() {
            println!("{}", wrap_line(line, w, "│ "));
        }
        println!("{}", "─".repeat(w.min(60)));
    }

    fn format_tool_call(&self, tool: &str, arguments: &serde_json::Value, result: Option<&serde_json::Value>, status: &str) {
        let w = terminal_width();
        let status_color = match status {
            "success" => "✓".green().to_string(),
            "error" => "✗".red().to_string(),
            "running" => "…".yellow().to_string(),
            _ => "?".to_string(),
        };
        println!("│ {} [{}] {} {}", "🔧".yellow(), status_color, tool, arguments);
        if let Some(res) = result {
            for line in res.to_string().lines() {
                println!("{}", wrap_line(line, w, "│   "));
            }
        }
    }

    fn format_error(&self, code: &str, message: &str, details: Option<&str>) {
        let w = terminal_width();
        println!("{} [{}] {}", "✘".red(), code.red(), message);
        if let Some(details) = details {
            for line in details.lines() {
                println!("{}", wrap_line(line, w, "  "));
            }
        }
    }

    fn format_done(&self, exit_code: i32, total_tokens: Option<u64>, duration_ms: Option<u64>) {
        let mut line = format!("{} Done (exit: {})", "✔".cyan(), exit_code);
        if let Some(tokens) = total_tokens {
            line.push_str(&format!(" · {} tokens", tokens));
        }
        if let Some(ms) = duration_ms {
            line.push_str(&format!(" · {}ms", ms));
        }
        println!("{}", line);
    }
}

/// Markdown formatter
pub struct MarkdownFormatter;

impl OutputFormatter for MarkdownFormatter {
    fn format_message(&self, role: &str, content: &str, thinking: Option<&str>) {
        let w = terminal_width();
        if let Some(thought) = thinking {
            println!("{} {}", "💭 Thinking:".blue(), thought);
        }
        println!("{}", "─".repeat(w.min(60)));
        let role_tag = match role {
            "user" => "🙋 You".yellow().to_string(),
            "assistant" => "🤖 Assistant".green().to_string(),
            "system" => "⚙ System".blue().to_string(),
            "team" => "👥 Team".cyan().to_string(),
            "config" => "🔧 Config".magenta().to_string(),
            r => r.to_string(),
        };
        println!("│ {}", role_tag);
        for line in content.lines() {
            println!("{}", wrap_line(line, w, "│ "));
        }
        println!("{}", "─".repeat(w.min(60)));
    }

    fn format_tool_call(&self, tool: &str, arguments: &serde_json::Value, result: Option<&serde_json::Value>, status: &str) {
        let w = terminal_width();
        let status_str = match status {
            "success" => "✓".green().to_string(),
            "error" => "✗".red().to_string(),
            "running" => "…".yellow().to_string(),
            _ => status.into(),
        };
        println!("{} [{}] {} {}", "  •".yellow(), status_str, tool, arguments);
        if let Some(res) = result {
            for line in res.to_string().lines() {
                println!("{}", wrap_line(line, w, "    "));
            }
        }
    }

    fn format_error(&self, _code: &str, message: &str, details: Option<&str>) {
        let w = terminal_width();
        println!("{} {}", "[Error]".red(), message);
        if let Some(details) = details {
            for line in details.lines() {
                println!("{}", wrap_line(line, w, "  "));
            }
        }
    }

    fn format_done(&self, exit_code: i32, total_tokens: Option<u64>, duration_ms: Option<u64>) {
        let mut line = format!("{} Done (exit: {})", "▸".cyan(), exit_code);
        if let Some(tokens) = total_tokens {
            line.push_str(&format!(" · {} tokens", tokens));
        }
        if let Some(ms) = duration_ms {
            line.push_str(&format!(" · {}ms", ms));
        }
        println!("{}", line);
    }
}

/// JSON formatter
pub struct JsonFormatter;

impl OutputFormatter for JsonFormatter {
    fn format_message(&self, role: &str, content: &str, thinking: Option<&str>) {
        let output = JsonOutput::Message {
            role: role.to_string(),
            content: content.to_string(),
            thinking: thinking.map(|s| s.to_string()),
        };
        // serde_json::to_string 对纯 Serialize 类型不会失败；防御性 default 避免 panic
        println!("{}", serde_json::to_string(&output).unwrap_or_else(|_| "{}".into()));
    }

    fn format_tool_call(&self, tool: &str, arguments: &serde_json::Value, result: Option<&serde_json::Value>, status: &str) {
        let output = JsonOutput::ToolCall {
            tool: tool.to_string(),
            arguments: arguments.clone(),
            result: result.cloned(),
            status: status.to_string(),
        };
        // serde_json::to_string 对纯 Serialize 类型不会失败；防御性 default 避免 panic
        println!("{}", serde_json::to_string(&output).unwrap_or_else(|_| "{}".into()));
    }

    fn format_error(&self, code: &str, message: &str, details: Option<&str>) {
        let output = JsonOutput::Error {
            code: code.to_string(),
            message: message.to_string(),
            details: details.map(|s| s.to_string()),
        };
        // serde_json::to_string 对纯 Serialize 类型不会失败；防御性 default 避免 panic
        println!("{}", serde_json::to_string(&output).unwrap_or_else(|_| "{}".into()));
    }

    fn format_done(&self, exit_code: i32, total_tokens: Option<u64>, duration_ms: Option<u64>) {
        let output = JsonOutput::Done {
            exit_code,
            total_tokens,
            duration_ms,
        };
        // serde_json::to_string 对纯 Serialize 类型不会失败；防御性 default 避免 panic
        println!("{}", serde_json::to_string(&output).unwrap_or_else(|_| "{}".into()));
    }
}

/// Compact formatter for event stream
pub struct CompactFormatter;

impl OutputFormatter for CompactFormatter {
    fn format_message(&self, _role: &str, content: &str, thinking: Option<&str>) {
        if let Some(thought) = thinking {
            println!("[{}] {}", "T".blue(), thought);
        }
        println!("[{}] {}", "M".green(), content);
    }

    fn format_tool_call(&self, tool: &str, _arguments: &serde_json::Value, _result: Option<&serde_json::Value>, status: &str) {
        let status_char = match status {
            "success" => "✓",
            "error" => "✗",
            "running" => "…",
            _ => "?",
        };
        println!("[{}] {}({})", "T".yellow(), tool, status_char);
    }

    fn format_error(&self, _code: &str, message: &str, details: Option<&str>) {
        println!("[{}] {}", "E".red(), message);
        if let Some(details) = details {
            println!("[{}] {}", "E".red(), details);
        }
    }

    fn format_done(&self, exit_code: i32, total_tokens: Option<u64>, duration_ms: Option<u64>) {
        let mut line = format!("[{}] Exit: {}", "D".cyan(), exit_code);
        if let Some(tokens) = total_tokens {
            line.push_str(&format!(" Tokens: {}", tokens));
        }
        if let Some(ms) = duration_ms {
            line.push_str(&format!(" Time: {}ms", ms));
        }
        println!("{}", line);
    }
}

/// Silent formatter (no output)
pub struct SilentFormatter;

impl OutputFormatter for SilentFormatter {
    fn format_message(&self, _role: &str, _content: &str, _thinking: Option<&str>) {}
    fn format_tool_call(&self, _tool: &str, _arguments: &serde_json::Value, _result: Option<&serde_json::Value>, _status: &str) {}
    fn format_error(&self, _code: &str, _message: &str, _details: Option<&str>) {}
    fn format_done(&self, _exit_code: i32, _total_tokens: Option<u64>, _duration_ms: Option<u64>) {}
}

/// Copy text to system clipboard via OSC 52 escape sequence
pub fn copy_to_clipboard(text: &str) {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((triple >> 6) & 0x3F) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(triple & 0x3F) as usize] as char } else { '=' });
    }
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b]52;c;{}\x07", out);
    let _ = stdout.flush();
}

/// Get formatter based on output format
pub fn get_formatter(format: OutputFormat) -> Box<dyn OutputFormatter> {
    match format {
        OutputFormat::Text => Box::new(TextFormatter),
        OutputFormat::Markdown => Box::new(MarkdownFormatter),
        OutputFormat::Json => Box::new(JsonFormatter),
        OutputFormat::Compact => Box::new(CompactFormatter),
        OutputFormat::Silent => Box::new(SilentFormatter),
    }
}