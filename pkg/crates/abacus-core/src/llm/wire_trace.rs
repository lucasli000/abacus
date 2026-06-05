//! Wire trace writer for LLM providers (debug builds only).
//!
//! 写完整请求 body 到 per-pid 路径：
//! - 避免并发覆盖（多 provider 进程 / 多并行请求）
//! - 0o600 权限（防止 world-readable 泄漏 API key + 对话历史）
//! - tmp_dir() 而非硬编码 `/tmp/`（多用户安全 + Windows 兼容）
//!
//! 路径格式：`{TMP_DIR}/abacus_wire_{provider}.{pid}.json`

use std::path::PathBuf;

/// 计算本次调用应写入的 wire trace 路径。
///
/// 包含 provider 名以避免不同 provider 互相覆盖，pid 避免多进程覆盖。
pub fn wire_trace_path(provider: &str) -> PathBuf {
    let safe = provider
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>();
    std::env::temp_dir().join(format!(
        "abacus_wire_{}.{}.json",
        safe,
        std::process::id()
    ))
}

/// 写入请求 body 到 per-pid wire trace 路径。
///
/// 失败时静默（debug-only，不能影响主流程）。返回 `()` 让调用方 `;` 续接。
pub fn write_wire_trace(provider: &str, base_url: &str, body: &str) {
    let path = wire_trace_path(provider);
    let prefixed = format!("// PROVIDER: {}\n// BASE_URL: {}\n{}", provider, base_url, body);
    if std::fs::write(&path, &prefixed).is_ok() {
        // 收紧权限：含 API key + 对话历史
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&path) {
                let mut perms = meta.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(&path, perms);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_trace_path_includes_provider_and_pid() {
        let p = wire_trace_path("openai-compatible");
        let s = p.to_str().unwrap();
        assert!(s.contains("abacus_wire_openai-compatible."));
        assert!(s.contains(&format!(".{}", std::process::id())));
    }

    #[test]
    fn wire_trace_path_sanitizes_unsafe_chars() {
        // 含 `(` `)` ` ` `/` 等 → 全部替换为 `_`
        // "deepseek (streaming)/v1" → "deepseek__streaming__v1"
        let p = wire_trace_path("deepseek (streaming)/v1");
        let s = p.to_str().unwrap();
        assert!(s.contains("deepseek__streaming__v1"), "got: {}", s);
    }

    #[test]
    fn write_wire_trace_creates_file_with_0600() {
        let provider = "test-write-trace";
        let body = r#"{"messages":[]}"#;
        write_wire_trace(provider, "https://example.com", body);
        let path = wire_trace_path(provider);
        let content = std::fs::read_to_string(&path).expect("file written");
        assert!(content.contains("// PROVIDER: test-write-trace"));
        assert!(content.contains("// BASE_URL: https://example.com"));
        assert!(content.contains(r#""messages":[]"#));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "wire trace must be 0o600, got {:o}", mode);
        }
    }
}
