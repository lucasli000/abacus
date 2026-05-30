//! Bun runtime detection and command substitution.
//!
//! ## Responsibilities
//! Detect whether a workspace uses the Bun runtime and provide intelligent
//! command substitution (Node.js → Bun equivalents) when appropriate.
//!
//! ## Dependencies (external)
//! - `std::path::Path`: filesystem checks for project markers
//! - `std::process::Command`: bun binary availability check
//!
//! ## Dependencies (internal)
//! - None (standalone utility module)
//!
//! ## References (callers)
//! - `filengine::bash_exec()` may invoke `bun_substitute_command()` before execution
//! - `crate::core::session_init` may call `detect_bun_project()` during workspace setup
//!
//! ## Lifecycle
//! - Pure functions, no state — created/destroyed per call
//! - `is_bun_available()` spawns a short-lived process (cached by caller if needed)

use std::path::Path;

/// Detect if the project at `workspace` uses the Bun runtime.
///
/// Checks for presence of:
/// - `bunfig.toml` (Bun configuration file)
/// - `bun.lockb` (Bun binary lockfile)
/// - `bun.lock` (Bun text lockfile, v1.2+)
///
/// ## References
/// - Called by `bun_substitute_command()` (this file)
/// - Called by session init logic to set `prefer_bun` flag
pub fn detect_bun_project(workspace: &Path) -> bool {
    workspace.join("bunfig.toml").exists()
        || workspace.join("bun.lockb").exists()
        || workspace.join("bun.lock").exists()
}

/// Check if the `bun` binary is available on PATH.
///
/// Spawns `bun --version` and checks for successful exit.
/// This is a blocking call — callers in async context should use `spawn_blocking`.
///
/// ## References
/// - Called by session init or bash_exec pre-check
pub fn is_bun_available() -> bool {
    std::process::Command::new("bun")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Substitute Node.js ecosystem commands with Bun equivalents.
///
/// Only performs substitution when `is_bun_project` is true.
/// Returns the original command unchanged if no substitution applies.
///
/// ## Substitution rules
/// | Original | Bun equivalent |
/// |----------|---------------|
/// | `npm install` | `bun install` |
/// | `npm i <pkg>` | `bun add <pkg>` |
/// | `npm run <script>` | `bun run <script>` |
/// | `npm test` | `bun test` |
/// | `npx <cmd>` | `bunx <cmd>` |
/// | `node <file>` | `bun <file>` |
/// | `ts-node <file>` | `bun <file>` |
/// | `tsx <file>` | `bun <file>` |
/// | `jest` | `bun test` |
///
/// ## References
/// - Called by `filengine::bash_exec()` pre-execution hook (when prefer_bun=true)
///
/// ## Design note
/// Does NOT substitute `npm publish` or other registry commands — those have
/// different semantics between npm and Bun and should remain explicit.
pub fn bun_substitute_command(cmd: &str, is_bun_project: bool) -> String {
    if !is_bun_project {
        return cmd.to_string();
    }

    let trimmed = cmd.trim();

    // Package management: npm install → bun install
    if trimmed.starts_with("npm install") && !trimmed.contains("--global") && !trimmed.contains("-g") {
        return trimmed.replacen("npm install", "bun install", 1);
    }
    // npm i <pkg> → bun add <pkg>
    if trimmed.starts_with("npm i ") && !trimmed.contains("--global") && !trimmed.contains("-g") {
        return trimmed.replacen("npm i ", "bun add ", 1);
    }
    // npm ci → bun install --frozen-lockfile
    if trimmed == "npm ci" || trimmed.starts_with("npm ci ") {
        return trimmed.replacen("npm ci", "bun install --frozen-lockfile", 1);
    }
    // npm run <script> → bun run <script>
    if trimmed.starts_with("npm run ") {
        return trimmed.replacen("npm run ", "bun run ", 1);
    }
    // npm test → bun test
    if trimmed == "npm test" || trimmed.starts_with("npm test ") {
        return trimmed.replacen("npm test", "bun test", 1);
    }
    // npx <cmd> → bunx <cmd>
    if trimmed.starts_with("npx ") {
        return trimmed.replacen("npx ", "bunx ", 1);
    }
    // jest → bun test
    if trimmed == "jest" {
        return "bun test".to_string();
    }
    if trimmed.starts_with("jest ") {
        return format!("bun test {}", &trimmed[5..]);
    }
    // node <file> → bun <file>
    if trimmed.starts_with("node ") {
        return trimmed.replacen("node ", "bun ", 1);
    }
    // ts-node <file> → bun <file>
    if trimmed.starts_with("ts-node ") {
        return trimmed.replacen("ts-node ", "bun ", 1);
    }
    // tsx <file> → bun <file>
    if trimmed.starts_with("tsx ") {
        return trimmed.replacen("tsx ", "bun ", 1);
    }

    // No substitution applies
    cmd.to_string()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_detect_bun_project_with_bunfig() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("bunfig.toml"), "").unwrap();
        assert!(detect_bun_project(dir.path()));
    }

    #[test]
    fn test_detect_bun_project_with_lockb() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("bun.lockb"), b"\x00").unwrap();
        assert!(detect_bun_project(dir.path()));
    }

    #[test]
    fn test_detect_bun_project_with_text_lock() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("bun.lock"), "{}").unwrap();
        assert!(detect_bun_project(dir.path()));
    }

    #[test]
    fn test_detect_bun_project_negative() {
        let dir = TempDir::new().unwrap();
        // Only package.json, no Bun markers
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert!(!detect_bun_project(dir.path()));
    }

    #[test]
    fn test_bun_substitute_npm_install() {
        assert_eq!(
            bun_substitute_command("npm install", true),
            "bun install"
        );
        assert_eq!(
            bun_substitute_command("npm install lodash", true),
            "bun install lodash"
        );
        // Global install should NOT be substituted
        assert_eq!(
            bun_substitute_command("npm install -g typescript", true),
            "npm install -g typescript"
        );
    }

    #[test]
    fn test_bun_substitute_npm_i() {
        assert_eq!(
            bun_substitute_command("npm i express", true),
            "bun add express"
        );
    }

    #[test]
    fn test_bun_substitute_npm_ci() {
        assert_eq!(
            bun_substitute_command("npm ci", true),
            "bun install --frozen-lockfile"
        );
    }

    #[test]
    fn test_bun_substitute_npm_run() {
        assert_eq!(
            bun_substitute_command("npm run build", true),
            "bun run build"
        );
        assert_eq!(
            bun_substitute_command("npm run dev -- --port 3000", true),
            "bun run dev -- --port 3000"
        );
    }

    #[test]
    fn test_bun_substitute_npm_test() {
        assert_eq!(
            bun_substitute_command("npm test", true),
            "bun test"
        );
        assert_eq!(
            bun_substitute_command("npm test -- --watch", true),
            "bun test -- --watch"
        );
    }

    #[test]
    fn test_bun_substitute_npx() {
        assert_eq!(
            bun_substitute_command("npx vite", true),
            "bunx vite"
        );
        assert_eq!(
            bun_substitute_command("npx prisma migrate dev", true),
            "bunx prisma migrate dev"
        );
    }

    #[test]
    fn test_bun_substitute_jest() {
        assert_eq!(
            bun_substitute_command("jest", true),
            "bun test"
        );
        assert_eq!(
            bun_substitute_command("jest --coverage", true),
            "bun test --coverage"
        );
    }

    #[test]
    fn test_bun_substitute_node() {
        assert_eq!(
            bun_substitute_command("node index.ts", true),
            "bun index.ts"
        );
        assert_eq!(
            bun_substitute_command("node server.js", true),
            "bun server.js"
        );
    }

    #[test]
    fn test_bun_substitute_ts_node() {
        assert_eq!(
            bun_substitute_command("ts-node src/index.ts", true),
            "bun src/index.ts"
        );
    }

    #[test]
    fn test_bun_substitute_tsx() {
        assert_eq!(
            bun_substitute_command("tsx src/app.ts", true),
            "bun src/app.ts"
        );
    }

    #[test]
    fn test_bun_substitute_not_bun_project() {
        // When not a Bun project, commands should pass through unchanged
        assert_eq!(
            bun_substitute_command("npm install", false),
            "npm install"
        );
        assert_eq!(
            bun_substitute_command("node index.js", false),
            "node index.js"
        );
    }

    #[test]
    fn test_bun_substitute_no_match() {
        // Commands that don't match any rule pass through unchanged
        assert_eq!(
            bun_substitute_command("cargo build", true),
            "cargo build"
        );
        assert_eq!(
            bun_substitute_command("python3 script.py", true),
            "python3 script.py"
        );
    }
}
