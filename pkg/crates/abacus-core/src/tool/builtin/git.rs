//! git — Built-in Git integration tools
//!
//! ## 场景
//! 提供结构化 Git 操作能力。区别于 bash_exec 的原始 git 命令：
//! 1. 输出已解析为 JSON（LLM 无需解析 diff 格式）
//! 2. 安全分级（read-only vs destructive）
//! 3. 与 CodeGraph 集成（git_diff → cg_analyze impact 联动）
//!
//! ## 安全设计
//! - 读操作（status/diff/log/blame）：confirm_required=false
//! - 写操作（stash/commit）：confirm_required=true（MCIP 拦截）
//! - 绝不执行 git push（用户必须通过 bash 自行 push）
//!
//! ## 依赖
//! - `tokio::process::Command`: 异步执行 git CLI
//!
//! ## 引用关系
//! - 被 `builtin::mod.rs::register_all()` 注册 schemas + executors
//! - 被 `CoreLoop::process_turn()` 通过 ToolRegistry 执行
//! - MCIP 前缀豁免：`git_`（mcip.rs BUILTIN_EXEMPT_PREFIXES）
//!
//! ## 注册工具 (6)
//! | Tool | Confirm | Risk | Idempotent | Description |
//! |------|---------|------|------------|-------------|
//! | git_status | no | low | yes | 工作区状态（branch/staged/unstaged/untracked） |
//! | git_diff | no | low | yes | 差异分析（stat + hunks） |
//! | git_log | no | low | yes | 提交历史 |
//! | git_blame | no | low | yes | 逐行归属（author/commit/date） |
//! | git_stash | yes | medium | no | stash 管理（push/pop/apply/drop/list） |
//! | git_commit | yes | medium | no | 提交变更 |

use std::path::{Path, PathBuf};
use std::sync::Arc;

use abacus_types::{
    KernelError, ToolCost, ToolEffectiveness, ToolHandle, ToolId, ToolProvider,
    ToolSchema, ToolSecurity, ToolState,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::process::Command;

use crate::tool::{ExecutionContext, ToolExecutor, ToolRegistry};

// ─── Executor ───────────────────────────────────────────────────────────────

/// Git 工具执行器
///
/// ## 生命周期
/// - 创建：register_executors() 时（register_all 内部调用）
/// - 存活：与 ToolRegistry 同生命周期
/// - 无外部状态依赖：workspace 从 ExecutionContext.filengine.cwd 动态获取
pub struct GitToolExecutor;

impl GitToolExecutor {
    pub fn new() -> Self {
        Self
    }

    /// 从 ExecutionContext 获取 workspace 路径，或使用 params 中的 path 覆盖
    async fn resolve_workspace(ctx: &ExecutionContext, params: &Value) -> PathBuf {
        if let Some(path) = params.get("path").and_then(|v| v.as_str()) {
            PathBuf::from(path)
        } else {
            let session = ctx.filengine.read().await;
            session.cwd.clone()
        }
    }

    /// 执行 git 命令并返回 stdout
    async fn run_git(workspace: &Path, args: &[&str]) -> abacus_types::Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(workspace)
            .output()
            .await
            .map_err(|e| KernelError::Other(format!("failed to execute git: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(KernelError::Other(format!("git {} failed: {}", args[0], stderr.trim())));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    // ─── git_status ──────────────────────────────────────────────────────

    async fn git_status(&self, params: Value, ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        let workspace = Self::resolve_workspace(ctx, &params).await;
        let output = Self::run_git(&workspace, &["status", "--porcelain=v2", "--branch"]).await?;

        let mut branch = String::new();
        let mut ahead: i64 = 0;
        let mut behind: i64 = 0;
        let mut staged: Vec<Value> = Vec::new();
        let mut unstaged: Vec<Value> = Vec::new();
        let mut untracked: Vec<String> = Vec::new();

        for line in output.lines() {
            if line.starts_with("# branch.head ") {
                branch = line.strip_prefix("# branch.head ").unwrap_or("").to_string();
            } else if line.starts_with("# branch.ab ") {
                // Format: # branch.ab +3 -1
                let ab = line.strip_prefix("# branch.ab ").unwrap_or("");
                let parts: Vec<&str> = ab.split_whitespace().collect();
                if parts.len() >= 2 {
                    ahead = parts[0].trim_start_matches('+').parse().unwrap_or(0);
                    behind = parts[1].trim_start_matches('-').parse().unwrap_or(0);
                }
            } else if line.starts_with("1 ") || line.starts_with("2 ") {
                // Changed entry: "1 XY sub mH mI mW hH hI path"
                // or renamed:    "2 XY sub mH mI mW hH hI X{NNN} path\tpath"
                let parts: Vec<&str> = line.splitn(9, ' ').collect();
                if parts.len() >= 9 {
                    let xy = parts[1];
                    let x = xy.chars().next().unwrap_or('.');
                    let y = xy.chars().nth(1).unwrap_or('.');

                    // For renames (prefix "2"), path may contain tab
                    let file_part = parts[8];
                    let file = if line.starts_with("2 ") {
                        // "path\torigPath" → use the new path
                        file_part.split('\t').next().unwrap_or(file_part)
                    } else {
                        file_part
                    };

                    if x != '.' {
                        staged.push(json!({
                            "file": file,
                            "status": status_char_to_str(x),
                        }));
                    }
                    if y != '.' {
                        unstaged.push(json!({
                            "file": file,
                            "status": status_char_to_str(y),
                        }));
                    }
                }
            } else if line.starts_with("? ") {
                // Untracked: "? path"
                let path = line.strip_prefix("? ").unwrap_or("").to_string();
                untracked.push(path);
            }
        }

        Ok(json!({
            "branch": branch,
            "ahead": ahead,
            "behind": behind,
            "staged": staged,
            "unstaged": unstaged,
            "untracked": untracked,
        }))
    }

    // ─── git_diff ────────────────────────────────────────────────────────

    async fn git_diff(&self, params: Value, ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        let workspace = Self::resolve_workspace(ctx, &params).await;
        let staged = params.get("staged").and_then(|v| v.as_bool()).unwrap_or(false);
        let ref1 = params.get("ref1").and_then(|v| v.as_str());
        let ref2 = params.get("ref2").and_then(|v| v.as_str());
        let file_filter = params.get("file").and_then(|v| v.as_str());

        // Build args for numstat (structured counts)
        let mut stat_args: Vec<&str> = vec!["diff", "--numstat"];
        if staged {
            stat_args.push("--cached");
        }
        if let Some(r1) = ref1 {
            stat_args.push(r1);
        }
        if let Some(r2) = ref2 {
            stat_args.push(r2);
        }
        if let Some(f) = file_filter {
            stat_args.push("--");
            stat_args.push(f);
        }

        let numstat_output = Self::run_git(&workspace, &stat_args).await?;

        let mut files_changed: u32 = 0;
        let mut total_insertions: u32 = 0;
        let mut total_deletions: u32 = 0;

        for line in numstat_output.lines() {
            if line.is_empty() { continue; }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 3 {
                files_changed += 1;
                // Binary files show "-" for insertions/deletions
                total_insertions += parts[0].parse::<u32>().unwrap_or(0);
                total_deletions += parts[1].parse::<u32>().unwrap_or(0);
            }
        }

        // Build args for unified diff (hunks)
        let mut diff_args: Vec<&str> = vec!["diff", "-U3"];
        if staged {
            diff_args.push("--cached");
        }
        if let Some(r1) = ref1 {
            diff_args.push(r1);
        }
        if let Some(r2) = ref2 {
            diff_args.push(r2);
        }
        if let Some(f) = file_filter {
            diff_args.push("--");
            diff_args.push(f);
        }

        let diff_output = Self::run_git(&workspace, &diff_args).await?;
        let hunks = parse_diff_hunks(&diff_output);

        Ok(json!({
            "files_changed": files_changed,
            "insertions": total_insertions,
            "deletions": total_deletions,
            "hunks": hunks,
        }))
    }

    // ─── git_log ─────────────────────────────────────────────────────────

    async fn git_log(&self, params: Value, ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        let workspace = Self::resolve_workspace(ctx, &params).await;
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(10).min(100);
        let author = params.get("author").and_then(|v| v.as_str());
        let since = params.get("since").and_then(|v| v.as_str());
        let file_filter = params.get("file").and_then(|v| v.as_str());

        // Delimiter for parsing: ASCII record separator
        let format = "%H\x1e%an\x1e%aI\x1e%s\x1e";

        let mut args: Vec<String> = vec![
            "log".into(),
            format!("--format={}", format),
            "--shortstat".into(),
            format!("-{}", limit),
        ];
        if let Some(a) = author {
            args.push(format!("--author={}", a));
        }
        if let Some(s) = since {
            args.push(format!("--since={}", s));
        }
        if let Some(f) = file_filter {
            args.push("--".into());
            args.push(f.into());
        }

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = Self::run_git(&workspace, &args_refs).await?;

        let commits = parse_git_log(&output);

        Ok(json!({
            "commits": commits,
        }))
    }

    // ─── git_blame ───────────────────────────────────────────────────────

    async fn git_blame(&self, params: Value, ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        let workspace = Self::resolve_workspace(ctx, &params).await;
        let file = params.get("file")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: file".into()))?;
        let start_line = params.get("start_line").and_then(|v| v.as_u64());
        let end_line = params.get("end_line").and_then(|v| v.as_u64());

        let mut args: Vec<String> = vec!["blame".into(), "--porcelain".into()];
        if let (Some(s), Some(e)) = (start_line, end_line) {
            args.push(format!("-L{},{}", s, e));
        } else if let Some(s) = start_line {
            args.push(format!("-L{},", s));
        }
        args.push(file.into());

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = Self::run_git(&workspace, &args_refs).await?;

        let lines = parse_blame_porcelain(&output);

        Ok(json!({
            "lines": lines,
        }))
    }

    // ─── git_stash ───────────────────────────────────────────────────────

    async fn git_stash(&self, params: Value, ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        let workspace = Self::resolve_workspace(ctx, &params).await;
        let action = params.get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: action".into()))?;

        match action {
            "list" => {
                let output = Self::run_git(&workspace, &["stash", "list"]).await?;
                let entries: Vec<Value> = output.lines()
                    .enumerate()
                    .map(|(i, line)| json!({"index": i, "description": line}))
                    .collect();
                Ok(json!({"action": "list", "entries": entries}))
            }
            "push" => {
                let message = params.get("message").and_then(|v| v.as_str());
                let mut args: Vec<&str> = vec!["stash", "push"];
                if let Some(msg) = message {
                    args.push("-m");
                    args.push(msg);
                }
                let output = Self::run_git(&workspace, &args).await?;
                Ok(json!({"action": "push", "message": output.trim()}))
            }
            "pop" => {
                let index = params.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let stash_ref = format!("stash@{{{}}}", index);
                let output = Self::run_git(&workspace, &["stash", "pop", &stash_ref]).await?;
                Ok(json!({"action": "pop", "index": index, "message": output.trim()}))
            }
            "apply" => {
                let index = params.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let stash_ref = format!("stash@{{{}}}", index);
                let output = Self::run_git(&workspace, &["stash", "apply", &stash_ref]).await?;
                Ok(json!({"action": "apply", "index": index, "message": output.trim()}))
            }
            "drop" => {
                let index = params.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let stash_ref = format!("stash@{{{}}}", index);
                let output = Self::run_git(&workspace, &["stash", "drop", &stash_ref]).await?;
                Ok(json!({"action": "drop", "index": index, "message": output.trim()}))
            }
            _ => Err(KernelError::Other(
                format!("invalid stash action: {action}. Must be list|push|pop|apply|drop")
            )),
        }
    }

    // ─── git_commit ──────────────────────────────────────────────────────

    async fn git_commit(&self, params: Value, ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        let workspace = Self::resolve_workspace(ctx, &params).await;
        let message = params.get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| KernelError::Other("missing required parameter: message".into()))?;
        let files = params.get("files").and_then(|v| v.as_array());
        let all = params.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
        let amend = params.get("amend").and_then(|v| v.as_bool()).unwrap_or(false);

        // Stage files if specified
        if let Some(file_list) = files {
            let mut add_args: Vec<String> = vec!["add".into()];
            for f in file_list {
                if let Some(path) = f.as_str() {
                    add_args.push(path.into());
                }
            }
            let add_refs: Vec<&str> = add_args.iter().map(|s| s.as_str()).collect();
            Self::run_git(&workspace, &add_refs).await?;
        }

        // Build commit command
        let mut commit_args: Vec<String> = vec!["commit".into()];
        if all {
            commit_args.push("-a".into());
        }
        if amend {
            commit_args.push("--amend".into());
        }
        commit_args.push("-m".into());
        commit_args.push(message.into());

        let commit_refs: Vec<&str> = commit_args.iter().map(|s| s.as_str()).collect();
        Self::run_git(&workspace, &commit_refs).await?;

        // Get the commit hash and files committed
        let hash_output = Self::run_git(&workspace, &["rev-parse", "HEAD"]).await?;
        let hash = hash_output.trim().to_string();

        let stat_output = Self::run_git(&workspace, &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]).await?;
        let files_committed = stat_output.lines().filter(|l| !l.is_empty()).count() as u32;

        Ok(json!({
            "hash": hash,
            "message": message,
            "files_committed": files_committed,
        }))
    }
}

#[async_trait]
impl ToolExecutor for GitToolExecutor {
    async fn execute(&self, tool_id: &ToolId, params: Value, ctx: &ExecutionContext) -> abacus_types::Result<Value> {
        match tool_id.0.as_str() {
            "git_status" => self.git_status(params, ctx).await,
            "git_diff" => self.git_diff(params, ctx).await,
            "git_log" => self.git_log(params, ctx).await,
            "git_blame" => self.git_blame(params, ctx).await,
            "git_stash" => self.git_stash(params, ctx).await,
            "git_commit" => self.git_commit(params, ctx).await,
            _ => Err(KernelError::Other(format!("unknown git tool: {}", tool_id.0))),
        }
    }
}

// ─── Parsers ────────────────────────────────────────────────────────────────

/// Map porcelain v2 status character to human-readable string
fn status_char_to_str(c: char) -> &'static str {
    match c {
        'M' => "modified",
        'T' => "type_changed",
        'A' => "added",
        'D' => "deleted",
        'R' => "renamed",
        'C' => "copied",
        'U' => "unmerged",
        _ => "unknown",
    }
}

/// Parse unified diff output into structured hunks
///
/// Returns a Vec of hunk objects with file, old_start, new_start, content.
/// Limits content per hunk to avoid token explosion.
fn parse_diff_hunks(diff: &str) -> Vec<Value> {
    let mut hunks: Vec<Value> = Vec::new();
    let mut current_file = String::new();
    let mut current_hunk_content = String::new();
    let mut old_start: u32 = 0;
    let mut new_start: u32 = 0;
    let mut in_hunk = false;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            // Flush previous hunk
            if in_hunk && !current_hunk_content.is_empty() {
                hunks.push(json!({
                    "file": current_file,
                    "old_start": old_start,
                    "new_start": new_start,
                    "content": truncate_hunk_content(&current_hunk_content),
                }));
            }
            in_hunk = false;
            current_hunk_content.clear();
            // Extract file name: "diff --git a/path b/path"
            if let Some(b_path) = line.split(" b/").last() {
                current_file = b_path.to_string();
            }
        } else if line.starts_with("@@ ") {
            // Flush previous hunk
            if in_hunk && !current_hunk_content.is_empty() {
                hunks.push(json!({
                    "file": current_file,
                    "old_start": old_start,
                    "new_start": new_start,
                    "content": truncate_hunk_content(&current_hunk_content),
                }));
            }
            current_hunk_content.clear();
            in_hunk = true;

            // Parse "@@ -old_start,count +new_start,count @@"
            let (os, ns) = parse_hunk_header(line);
            old_start = os;
            new_start = ns;
        } else if in_hunk {
            current_hunk_content.push_str(line);
            current_hunk_content.push('\n');
        }
    }

    // Flush last hunk
    if in_hunk && !current_hunk_content.is_empty() {
        hunks.push(json!({
            "file": current_file,
            "old_start": old_start,
            "new_start": new_start,
            "content": truncate_hunk_content(&current_hunk_content),
        }));
    }

    hunks
}

/// Parse hunk header "@@ -old_start[,count] +new_start[,count] @@" → (old_start, new_start)
fn parse_hunk_header(line: &str) -> (u32, u32) {
    // Format: "@@ -111,22 +111,25 @@ optional context"
    let stripped = line.trim_start_matches("@@ ");
    let parts: Vec<&str> = stripped.splitn(3, ' ').collect();
    let old = parts.first()
        .and_then(|p| p.strip_prefix('-'))
        .and_then(|p| p.split(',').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let new = parts.get(1)
        .and_then(|p| p.strip_prefix('+'))
        .and_then(|p| p.split(',').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    (old, new)
}

/// Truncate hunk content to avoid token explosion (max 2000 chars)
fn truncate_hunk_content(content: &str) -> &str {
    const MAX_HUNK_CHARS: usize = 2000;
    if content.len() <= MAX_HUNK_CHARS {
        content
    } else {
        // Find a line boundary near the limit
        match content[..MAX_HUNK_CHARS].rfind('\n') {
            Some(pos) => &content[..pos + 1],
            None => &content[..MAX_HUNK_CHARS],
        }
    }
}

/// Parse `git log --format="%H\x1e%an\x1e%aI\x1e%s\x1e" --shortstat` output
///
/// The format interleaves commit metadata lines with optional shortstat lines.
pub(crate) fn parse_git_log(output: &str) -> Vec<Value> {
    let mut commits: Vec<Value> = Vec::new();
    let mut lines = output.lines().peekable();

    while let Some(line) = lines.next() {
        if line.is_empty() { continue; }

        // Try to parse as commit line (contains \x1e separators)
        if line.contains('\x1e') {
            let parts: Vec<&str> = line.split('\x1e').collect();
            if parts.len() >= 4 {
                let hash = parts[0].to_string();
                let author = parts[1].to_string();
                let date = parts[2].to_string();
                let message = parts[3].to_string();

                // Check if next non-empty line is a shortstat
                let mut files_changed: u32 = 0;
                // Skip empty line between commit and shortstat
                while lines.peek().map(|l| l.is_empty()).unwrap_or(false) {
                    lines.next();
                }
                if let Some(stat_line) = lines.peek() {
                    if stat_line.contains("file") && stat_line.contains("changed") {
                        files_changed = parse_shortstat_files(stat_line);
                        lines.next(); // consume
                    }
                }

                commits.push(json!({
                    "hash": hash,
                    "author": author,
                    "date": date,
                    "message": message,
                    "files_changed": files_changed,
                }));
            }
        }
    }

    commits
}

/// Extract file count from shortstat: " 3 files changed, 10 insertions(+), 2 deletions(-)"
fn parse_shortstat_files(line: &str) -> u32 {
    let trimmed = line.trim();
    trimmed.split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Parse `git blame --porcelain` output into structured lines
///
/// Porcelain format:
/// ```text
/// <hash> <orig_line> <final_line> [<num_lines>]
/// author <name>
/// author-time <timestamp>
/// ...
/// \t<content>
/// ```
pub(crate) fn parse_blame_porcelain(output: &str) -> Vec<Value> {
    let mut lines_out: Vec<Value> = Vec::new();
    let mut current_hash = String::new();
    let mut current_author = String::new();
    let mut current_date = String::new();
    let mut current_line_num: u32 = 0;

    for line in output.lines() {
        if line.starts_with('\t') {
            // Content line — finalize this entry
            let content = &line[1..]; // strip leading tab
            lines_out.push(json!({
                "line_num": current_line_num,
                "hash": current_hash,
                "author": current_author,
                "date": current_date,
                "content": content,
            }));
        } else if line.starts_with("author ") {
            current_author = line.strip_prefix("author ").unwrap_or("").to_string();
        } else if line.starts_with("author-time ") {
            // Unix timestamp → ISO 8601 (simplified: just keep timestamp for now)
            current_date = line.strip_prefix("author-time ").unwrap_or("").to_string();
        } else {
            // First line of a blame entry: "<hash> <orig_line> <final_line> [<num_lines>]"
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 && parts[0].len() >= 7 {
                // Validate it looks like a hash (hex chars)
                if parts[0].chars().all(|c| c.is_ascii_hexdigit()) {
                    current_hash = parts[0].to_string();
                    // final_line is the third field
                    current_line_num = parts[2].parse().unwrap_or(0);
                }
            }
        }
    }

    lines_out
}

// ─── Schema ─────────────────────────────────────────────────────────────────

fn git_read_schema(
    name: &str,
    desc: &str,
    props: Value,
    required: &[&str],
    tokens: u32,
    latency: &str,
) -> ToolSchema {
    ToolSchema {
        short_description: None,
        name: name.into(),
        description: desc.into(),
        parameters: json!({
            "type": "object",
            "properties": props,
            "required": required,
        }),
        returns: None,
        security: Some(ToolSecurity {
            allowed_paths: None,
            max_size_mb: None,
            confirm_required: false,
            needs_sandbox: false,
        }),
        cost: Some(ToolCost { tokens, latency: latency.into(), risk: "low".into() }),
        examples: Vec::new(),
        applicable_task_kinds: Some(vec![
            "code_review".into(),
            "debugging".into(),
            "refactoring".into(),
            "code_generation".into(),
            "architecture".into(),
        ]),
        idempotent: true,
        schema_stable: true,
    }
}

fn git_write_schema(
    name: &str,
    desc: &str,
    props: Value,
    required: &[&str],
    tokens: u32,
    latency: &str,
) -> ToolSchema {
    ToolSchema {
        short_description: None,
        name: name.into(),
        description: desc.into(),
        parameters: json!({
            "type": "object",
            "properties": props,
            "required": required,
        }),
        returns: None,
        security: Some(ToolSecurity {
            allowed_paths: None,
            max_size_mb: None,
            confirm_required: true,
            needs_sandbox: false,
        }),
        cost: Some(ToolCost { tokens, latency: latency.into(), risk: "medium".into() }),
        examples: Vec::new(),
        applicable_task_kinds: Some(vec![
            "code_review".into(),
            "debugging".into(),
            "refactoring".into(),
            "code_generation".into(),
        ]),
        idempotent: false,
        schema_stable: true,
    }
}

pub fn schemas() -> Vec<ToolSchema> {
    let mut v = vec![
        git_read_schema(
            "git_status",
            "Show working tree status (branch, staged, unstaged, untracked)",
            json!({
                "path": {"type": "string", "description": "Repository path (default: cwd)"}
            }),
            &[],
            32, "50ms",
        ),
        git_read_schema(
            "git_diff",
            "Show file differences (hunks + stats, staged or working tree)",
            json!({
                "path": {"type": "string", "description": "Repository path (default: cwd)"},
                "staged": {"type": "boolean", "description": "Show staged changes (default: false)"},
                "ref1": {"type": "string", "description": "Start ref (commit/branch)"},
                "ref2": {"type": "string", "description": "End ref (commit/branch)"},
                "file": {"type": "string", "description": "Filter to specific file path"}
            }),
            &[],
            96, "100ms",
        ),
        git_read_schema(
            "git_log",
            "Show commit history (hash, author, date, message, files)",
            json!({
                "path": {"type": "string", "description": "Repository path (default: cwd)"},
                "limit": {"type": "integer", "description": "Max commits (default 10, max 100)"},
                "author": {"type": "string", "description": "Filter by author name/email"},
                "since": {"type": "string", "description": "Show commits since (e.g. '2024-01-01')"},
                "file": {"type": "string", "description": "Filter to specific file path"}
            }),
            &[],
            64, "80ms",
        ),
        git_read_schema(
            "git_blame",
            "Show per-line commit attribution (author, hash, date)",
            json!({
                "file": {"type": "string", "description": "File path to blame"},
                "start_line": {"type": "integer", "description": "Start line number"},
                "end_line": {"type": "integer", "description": "End line number"}
            }),
            &["file"],
            64, "100ms",
        ),
        git_write_schema(
            "git_stash",
            "Manage stash entries (push/pop/apply/drop/list)",
            json!({
                "action": {"type": "string", "description": "list|push|pop|apply|drop"},
                "message": {"type": "string", "description": "Stash message (push only)"},
                "index": {"type": "integer", "description": "Stash index (default 0)"}
            }),
            &["action"],
            32, "100ms",
        ),
        git_write_schema(
            "git_commit",
            "Create a commit (stage files + commit message)",
            json!({
                "message": {"type": "string", "description": "Commit message"},
                "files": {"type": "array", "items": {"type": "string"}, "description": "Files to stage before commit"},
                "all": {"type": "boolean", "description": "Stage all tracked changes (-a)"},
                "amend": {"type": "boolean", "description": "Amend previous commit"}
            }),
            &["message"],
            32, "200ms",
        ),
    ];
    // Short-Mode 短描述注入
    for s in v.iter_mut() {
        s.short_description = Some(match s.name.as_str() {
            "git_status" => "Working tree status",
            "git_diff"   => "File differences (hunks + stats)",
            "git_log"    => "Commit history",
            "git_blame"  => "Per-line commit attribution",
            "git_stash"  => "Stash management (push/pop/list)",
            "git_commit" => "Create a git commit",
            _ => continue,
        }.into());
    }
    v
}

// ─── Registration ───────────────────────────────────────────────────────────

/// 注册 Git 工具 schemas
///
/// 在 register_all() 中调用。所有 6 个 git 工具的 schema 无条件注册。
/// Executor 同步注册（无外部状态依赖）。
pub async fn register(registry: &ToolRegistry) {
    for s in schemas() {
        registry.register(ToolHandle {
            id: ToolId(s.name.clone()),
            schema: s,
            provider: ToolProvider::BuiltIn,
            state: ToolState::Loaded,
            effectiveness: ToolEffectiveness::default(),
        }).await;
    }
}

/// 注册 Git executors（无状态——直接构造）
///
/// 由 register_all() 中 register() 之后立即调用。
pub async fn register_executors(registry: &ToolRegistry) {
    let executor = Arc::new(GitToolExecutor::new());
    for s in schemas() {
        registry.register_executor(ToolId(s.name.clone()), executor.clone()).await;
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_git_status_porcelain() {
        // Simulate --porcelain=v2 --branch output
        let output = "\
# branch.oid abc123def456
# branch.head main
# branch.upstream origin/main
# branch.ab +2 -1
1 M. N... 100644 100644 100644 abc123 def456 src/main.rs
1 .M N... 100644 100644 100644 abc123 def456 src/lib.rs
1 A. N... 100644 100644 100644 abc123 def456 src/new_file.rs
? untracked.txt
";
        let mut branch = String::new();
        let mut ahead: i64 = 0;
        let mut behind: i64 = 0;
        let mut staged: Vec<Value> = Vec::new();
        let mut unstaged: Vec<Value> = Vec::new();
        let mut untracked: Vec<String> = Vec::new();

        for line in output.lines() {
            if line.starts_with("# branch.head ") {
                branch = line.strip_prefix("# branch.head ").unwrap_or("").to_string();
            } else if line.starts_with("# branch.ab ") {
                let ab = line.strip_prefix("# branch.ab ").unwrap_or("");
                let parts: Vec<&str> = ab.split_whitespace().collect();
                if parts.len() >= 2 {
                    ahead = parts[0].trim_start_matches('+').parse().unwrap_or(0);
                    behind = parts[1].trim_start_matches('-').parse().unwrap_or(0);
                }
            } else if line.starts_with("1 ") || line.starts_with("2 ") {
                let parts: Vec<&str> = line.splitn(9, ' ').collect();
                if parts.len() >= 9 {
                    let xy = parts[1];
                    let x = xy.chars().next().unwrap_or('.');
                    let y = xy.chars().nth(1).unwrap_or('.');
                    let file = parts[8];

                    if x != '.' {
                        staged.push(json!({"file": file, "status": status_char_to_str(x)}));
                    }
                    if y != '.' {
                        unstaged.push(json!({"file": file, "status": status_char_to_str(y)}));
                    }
                }
            } else if line.starts_with("? ") {
                let path = line.strip_prefix("? ").unwrap_or("").to_string();
                untracked.push(path);
            }
        }

        assert_eq!(branch, "main");
        assert_eq!(ahead, 2);
        assert_eq!(behind, 1);
        assert_eq!(staged.len(), 2); // M. and A.
        assert_eq!(staged[0]["file"], "src/main.rs");
        assert_eq!(staged[0]["status"], "modified");
        assert_eq!(staged[1]["file"], "src/new_file.rs");
        assert_eq!(staged[1]["status"], "added");
        assert_eq!(unstaged.len(), 1); // .M
        assert_eq!(unstaged[0]["file"], "src/lib.rs");
        assert_eq!(unstaged[0]["status"], "modified");
        assert_eq!(untracked, vec!["untracked.txt"]);
    }

    #[test]
    fn test_parse_git_log_format() {
        let output = "\
abc123def456789\x1eJohn Doe\x1e2024-01-15T10:30:00+08:00\x1efix: resolve null pointer\x1e\n\
\n\
 2 files changed, 10 insertions(+), 3 deletions(-)\n\
def789abc123456\x1eJane Smith\x1e2024-01-14T09:00:00+08:00\x1efeat: add user auth\x1e\n\
\n\
 5 files changed, 200 insertions(+), 15 deletions(-)\n\
";
        let commits = parse_git_log(output);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["hash"], "abc123def456789");
        assert_eq!(commits[0]["author"], "John Doe");
        assert_eq!(commits[0]["message"], "fix: resolve null pointer");
        assert_eq!(commits[0]["files_changed"], 2);
        assert_eq!(commits[1]["hash"], "def789abc123456");
        assert_eq!(commits[1]["files_changed"], 5);
    }

    #[test]
    fn test_schemas_under_150_bytes() {
        for schema in schemas() {
            let len = schema.description.len();
            assert!(
                len <= 150,
                "git tool '{}' description is {} bytes (max 150): {:?}",
                schema.name, len, &schema.description[..60.min(len)]
            );
        }
    }

    #[test]
    fn test_git_commit_requires_confirm() {
        let all = schemas();
        let commit_schema = all.iter().find(|s| s.name == "git_commit").unwrap();
        assert!(commit_schema.security.as_ref().unwrap().confirm_required);

        let stash_schema = all.iter().find(|s| s.name == "git_stash").unwrap();
        assert!(stash_schema.security.as_ref().unwrap().confirm_required);

        // Read-only tools should NOT require confirmation
        let status_schema = all.iter().find(|s| s.name == "git_status").unwrap();
        assert!(!status_schema.security.as_ref().unwrap().confirm_required);

        let diff_schema = all.iter().find(|s| s.name == "git_diff").unwrap();
        assert!(!diff_schema.security.as_ref().unwrap().confirm_required);

        let log_schema = all.iter().find(|s| s.name == "git_log").unwrap();
        assert!(!log_schema.security.as_ref().unwrap().confirm_required);

        let blame_schema = all.iter().find(|s| s.name == "git_blame").unwrap();
        assert!(!blame_schema.security.as_ref().unwrap().confirm_required);
    }

    #[test]
    fn test_parse_hunk_header() {
        assert_eq!(parse_hunk_header("@@ -10,5 +12,7 @@ fn main()"), (10, 12));
        assert_eq!(parse_hunk_header("@@ -1 +1 @@"), (1, 1));
        assert_eq!(parse_hunk_header("@@ -0,0 +1,25 @@"), (0, 1));
    }

    #[test]
    fn test_parse_diff_hunks() {
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
index abc123..def456 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,3 +10,4 @@ fn main() {
     let x = 1;
+    let y = 2;
     println!(\"hello\");
 }
";
        let hunks = parse_diff_hunks(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0]["file"], "src/main.rs");
        assert_eq!(hunks[0]["old_start"], 10);
        assert_eq!(hunks[0]["new_start"], 10);
        assert!(hunks[0]["content"].as_str().unwrap().contains("+    let y = 2;"));
    }

    #[test]
    fn test_parse_blame_porcelain() {
        let output = "\
abc123def456789012345678901234567890 1 1 1
author John Doe
author-mail <john@example.com>
author-time 1705000000
author-tz +0800
committer John Doe
committer-mail <john@example.com>
committer-time 1705000000
committer-tz +0800
summary Initial commit
filename src/main.rs
\tfn main() {
def456abc789012345678901234567890123 2 2 1
author Jane Smith
author-mail <jane@example.com>
author-time 1705100000
author-tz +0800
committer Jane Smith
committer-mail <jane@example.com>
committer-time 1705100000
committer-tz +0800
summary Add println
filename src/main.rs
\t    println!(\"hello\");
";
        let lines = parse_blame_porcelain(output);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["line_num"], 1);
        assert_eq!(lines[0]["author"], "John Doe");
        assert_eq!(lines[0]["content"], "fn main() {");
        assert_eq!(lines[1]["line_num"], 2);
        assert_eq!(lines[1]["author"], "Jane Smith");
        assert_eq!(lines[1]["content"], "    println!(\"hello\");");
    }

    #[test]
    fn test_idempotency_classification() {
        let all = schemas();
        // Read-only tools must be idempotent
        for name in &["git_status", "git_diff", "git_log", "git_blame"] {
            let schema = all.iter().find(|s| s.name == *name).unwrap();
            assert!(schema.idempotent, "{} should be idempotent", name);
        }
        // Write tools must NOT be idempotent
        for name in &["git_stash", "git_commit"] {
            let schema = all.iter().find(|s| s.name == *name).unwrap();
            assert!(!schema.idempotent, "{} should NOT be idempotent", name);
        }
    }

    #[test]
    fn test_status_char_to_str() {
        assert_eq!(status_char_to_str('M'), "modified");
        assert_eq!(status_char_to_str('A'), "added");
        assert_eq!(status_char_to_str('D'), "deleted");
        assert_eq!(status_char_to_str('R'), "renamed");
        assert_eq!(status_char_to_str('C'), "copied");
        assert_eq!(status_char_to_str('?'), "unknown");
    }
}
