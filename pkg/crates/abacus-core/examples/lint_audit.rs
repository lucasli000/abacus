//! Phase 1：跑 register_all 后输出 lint audit 报告
use abacus_core::tool::ToolRegistry;
use abacus_core::tool::builtin::register_all;

#[tokio::main]
async fn main() {
    // 屏蔽 register 时 tracing warn 输出（用 RUST_LOG=error 控制；这里依赖 default subscriber 静默）
    let registry = ToolRegistry::new();
    register_all(&registry).await;
    let issues = registry.lint_audit().await;

    let mut by_severity: std::collections::BTreeMap<&str, Vec<_>> = std::collections::BTreeMap::new();
    for i in &issues {
        let key = match i.severity {
            abacus_core::tool::schema_lint::LintSeverity::Error => "Errors",
            abacus_core::tool::schema_lint::LintSeverity::Warn => "Warnings",
            abacus_core::tool::schema_lint::LintSeverity::Info => "Info",
        };
        by_severity.entry(key).or_default().push(i);
    }

    println!("═══ Schema Lint Report (38 tools) ═══");
    for (k, v) in &by_severity {
        println!("\n  ── {} ({}) ──", k, v.len());
        for i in v {
            println!("    [{}] {} : {}", i.rule, i.tool_id.0, i.reason);
            if let Some(s) = &i.suggestion {
                println!("        → {}", s);
            }
        }
    }
    let totals = (
        by_severity.get("Errors").map(|v| v.len()).unwrap_or(0),
        by_severity.get("Warnings").map(|v| v.len()).unwrap_or(0),
        by_severity.get("Info").map(|v| v.len()).unwrap_or(0),
    );
    println!("\nTotals: {} errors, {} warnings, {} info", totals.0, totals.1, totals.2);
}
