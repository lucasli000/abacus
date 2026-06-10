pub mod chat;
pub mod config;
pub mod doctor;
pub mod exec;
pub mod mcp;
pub mod meeting;
pub mod model;
pub mod session;
pub mod skill;
pub mod team;
pub mod tui;
pub mod turnkey;
pub mod undo;

use clap::Parser;

// Chat command arguments
#[derive(Parser, Debug)]
pub struct ChatArgs {
    /// Model to use ("auto" = use configured default from config/preference)
    #[arg(long = "model", short = 'm', default_value = "auto")]
    pub model: String,

    /// System prompt
    #[arg(long = "system-prompt", short = 's')]
    pub system_prompt: Option<String>,

    /// Initial message
    #[arg(long = "message", short = 'M')]
    pub message: Option<String>,

    /// Session ID
    #[arg(long = "session", short = 'S')]
    pub session_id: Option<String>,

    /// Thinking depth: off, low, medium, high (default: off)
    #[arg(long = "thinking", short = 'T', default_value = "off")]
    pub thinking: String,
}

// Exec command arguments
#[derive(Parser, Debug)]
pub struct ExecArgs {
    /// Task description
    #[arg(long = "task", short = 't')]
    pub task: String,

    /// Model to use ("auto" = use configured default from config/preference)
    #[arg(long = "model", short = 'm', default_value = "auto")]
    pub model: String,

    /// Session ID
    #[arg(long = "session", short = 'S')]
    pub session_id: Option<String>,

    /// Timeout in seconds
    #[arg(long = "timeout", short = 'T', default_value_t = 300)]
    pub timeout: u64,
}

// Session command arguments
#[derive(Parser, Debug)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub action: SessionAction,
}

#[derive(Parser, Debug)]
pub enum SessionAction {
    /// List sessions
    List,

    /// Show session details
    Show {
        /// Session ID
        id: String,
    },

    /// Create new session
    New {
        /// Session title
        #[arg(long = "title", short = 't')]
        title: Option<String>,
    },

    /// Delete session
    Delete {
        /// Session ID
        id: String,
    },
}

// Skill command arguments
#[derive(Parser, Debug)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub action: SkillAction,
}

#[derive(Parser, Debug)]
pub enum SkillAction {
    /// List installed skills (from ~/.abacus/skills/)
    List,

    /// Install skill from path or URL
    Install {
        /// Path or URL to skill
        source: String,
    },

    /// Remove installed skill  
    Remove {
        /// Skill name or ID
        name: String,
    },

    /// Show skill details
    Show {
        /// Skill name or ID
        name: String,
    },

    /// Enable skill
    Enable {
        /// Skill name or ID
        name: String,
    },

    /// Disable skill
    Disable {
        /// Skill name or ID
        name: String,
    },
}

// Config command arguments
#[derive(Parser, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: ConfigAction,
}

#[derive(Parser, Debug)]
pub enum ConfigAction {
    /// Show current configuration
    Show,

    /// List available configuration keys with descriptions
    ListKeys,

    /// Set configuration value
    Set {
        /// Configuration key
        key: String,

        /// Configuration value
        value: String,
    },

    /// Edit configuration file
    Edit,

    /// Validate configuration
    Validate,
}

// Model command arguments
#[derive(Parser, Debug)]
pub struct ModelArgs {
    #[command(subcommand)]
    pub action: ModelAction,
}

#[derive(Parser, Debug)]
pub enum ModelAction {
    /// List available models（优先 cache，缺失时按静态列表展示）
    List,

    /// Show model details
    Show {
        /// Model name or ID
        name: String,
    },

    /// Ping model to check availability
    Ping {
        /// Model name or ID
        name: String,
    },

    /// 主动从所有 provider 拉取模型列表，刷新 ~/.abacus/models.cache.json
    Discover {
        /// Cache 写入路径（缺省 ~/.abacus/models.cache.json）
        #[arg(long)]
        path: Option<std::path::PathBuf>,

        /// 同时把 union 写入 ~/.abacus/config.toml 的 [available_models] 段
        /// （首次配置默认行为；可手动用此 flag 触发刷新）
        #[arg(long)]
        write_config: bool,
    },
}

// MCP command arguments
#[derive(Parser, Debug)]
pub struct McpArgs {
    #[command(subcommand)]
    pub action: McpAction,
}

#[derive(Parser, Debug)]
pub enum McpAction {
    /// Add MCP server configuration
    Add {
        /// Server ID
        server_id: String,
        /// Command or URL for the MCP server
        #[arg(long = "command", short = 'c')]
        command: Option<String>,
        /// Environment variables (KEY=VALUE,...)
        #[arg(long = "env", short = 'e')]
        env: Option<String>,
    },
    /// Remove MCP server configuration
    Remove {
        /// Server ID
        server_id: String,
    },
    /// List configured MCP servers
    List,
    /// Connect to MCP server
    Connect {
        /// Server ID
        server_id: String,
    },
    /// Discover tools from MCP server
    Discover {
        /// Server ID
        server_id: String,
    },
    /// Show MCP connection status
    Status,
}

// Team command arguments (Mode 2 entry)
#[derive(Parser, Debug)]
pub struct TeamArgs {
    #[command(subcommand)]
    pub action: TeamAction,
}

#[derive(Parser, Debug)]
pub enum TeamAction {
    /// Start a new team session
    Start {
        /// Team goal
        #[arg(long = "goal", short = 'g')]
        goal: String,
        /// Comma-separated role names
        #[arg(long = "roles", short = 'r')]
        roles: Option<String>,
    },
    /// Show team status
    Status {
        /// Team ID (optional, shows latest if omitted)
        team_id: Option<String>,
    },
    /// List active teams
    List,
    /// Stop a team
    Stop {
        /// Team ID
        team_id: String,
    },
}

// Turnkey command arguments (全托管模式)
#[derive(Parser, Debug)]
pub struct TurnkeyArgs {
    #[command(subcommand)]
    pub action: TurnkeyAction,
}

#[derive(Parser, Debug)]
pub enum TurnkeyAction {
    /// Run a task in fully-managed mode
    Run {
        /// Task goal (natural language)
        #[arg(long = "goal", short = 'g')]
        goal: String,
        /// Auto-approve plan without confirmation
        #[arg(long = "yes", short = 'y', default_value_t = false)]
        auto_approve: bool,
    },
    /// Check turnkey task status
    Status {
        /// Task ID (optional)
        task_id: Option<String>,
    },
    /// View execution logs
    Logs {
        /// Task ID filter
        #[arg(long = "task")]
        task_id: Option<String>,
        /// Max number of logs
        #[arg(long = "limit", short = 'n', default_value_t = 20)]
        limit: usize,
    },
    /// Resume a suspended task
    Resume {
        /// Task ID to resume
        task_id: String,
    },
}

// ─── Phase 5 file-undo CLI ─────────────────────────────────────────

/// Undo command arguments — `abacus undo [seq <N>|turn <N>] [--session <id>]`
#[derive(Parser, Debug)]
pub struct UndoArgs {
    /// Specific session ID (default: latest active session in current project)
    #[arg(long = "session", short = 'S')]
    pub session: Option<String>,
    /// Undo specific seq (overrides --turn if both set)
    #[arg(long = "seq")]
    pub seq: Option<u64>,
    /// Undo all entries in this turn
    #[arg(long = "turn")]
    pub turn: Option<u32>,
    /// Output format: json | markdown
    #[arg(long = "format", default_value = "markdown")]
    pub format: String,
}

/// Redo command arguments
#[derive(Parser, Debug)]
pub struct RedoArgs {
    /// Session ID to redo within (required)
    #[arg(long = "session", short = 'S')]
    pub session: String,
    /// Output format
    #[arg(long = "format", default_value = "markdown")]
    pub format: String,
}

/// History command arguments — `abacus history [--project] [--since 1h]`
#[derive(Parser, Debug)]
pub struct HistoryArgs {
    /// Show full project timeline (across sessions); otherwise current session only
    #[arg(long = "project")]
    pub project: bool,
    /// Specific session ID (only when --project not set)
    #[arg(long = "session", short = 'S')]
    pub session: Option<String>,
    /// Limit (single-session mode only); ignored with --project (uses --since)
    #[arg(long = "limit", short = 'n', default_value_t = 20)]
    pub limit: usize,
    /// Time window for --project (default 1h). Accepts: "1h", "30m", "7d", or RFC3339
    #[arg(long = "since", default_value = "1h")]
    pub since: String,
    /// Output format: json | markdown
    #[arg(long = "format", default_value = "markdown")]
    pub format: String,
}

/// Launch the TUI (terminal user interface)
#[derive(Parser, Debug)]
pub struct TuiArgs {
    /// Start in Chat mode
    #[arg(long)]
    pub chat: bool,

    /// Start in Team mode
    #[arg(long)]
    pub team: bool,

    /// Start in Meeting mode (default)
    #[arg(long)]
    pub meeting: bool,
}