//! kb_ingest_docs — 离线把仓库内 design 文档摄入 Abacus 知识库
//!
//! ## 用途
//! 把 `pkg/docs/design/*.md` 等开发决策文档摄入 `data/knowledge.db`，
//! 让 abacus 运行时通过 `kb.search` / `kb.query` 工具检索到这些设计原因。
//!
//! ## 引用关系
//! - 调用 `abacus_core::knowledge_store::KnowledgeStore::{new, ingest}`
//! - 写入 `data/knowledge.db`（与运行时同一 DB，additive 写入 + hash 去重）
//!
//! ## 生命周期
//! - 创建时机：手动运行 `cargo run --example kb_ingest_docs`
//! - 副作用：仅 INSERT 到 chunks/file_meta；hash 未变 → 跳过；不修改既有记录
//! - 销毁：进程退出
//!
//! ## 用法
//! 仓库 `pkg/` 目录下执行：
//! ```bash
//! cargo run -p abacus-core --example kb_ingest_docs
//! ```
//! 或指定额外路径（覆盖默认列表）：
//! ```bash
//! cargo run -p abacus-core --example kb_ingest_docs -- ../docs/MEMORY-DEDUCTION.md
//! ```
//!
//! 仓库根误判：默认假设 cwd 是 `pkg/`，向上一级为 repo root；
//! 如 cwd 不同，使用 `ABACUS_REPO_ROOT=/path/to/repo` env var 覆盖。

use std::path::PathBuf;

use abacus_core::knowledge_store::KnowledgeStore;
use abacus_core::paths;

/// 默认摄入路径：相对仓库根的设计文档
/// 维护提示：新增 design 文档时把路径加入此列表
const DEFAULT_DOCS: &[&str] = &[
    "pkg/docs/design/llm-thinking-design-decisions.md",
    "pkg/docs/design/multi-instance-isolation.md",
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 解析仓库根：默认假设 cwd 是 pkg/，向上一级；允许 env 覆盖
    let repo_root: PathBuf = std::env::var("ABACUS_REPO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_dir()
                .expect("cwd")
                .parent()
                .expect("repo root")
                .to_path_buf()
        });

    // Phase 7：KB 迁移到全局路径（paths::knowledge_db）。
    // 原仓库内 data/knowledge.db 已废止（多开、圈外使用不友好）。
    // ABACUS_HOME env var 有效。
    let db_path = paths::knowledge_db();
    let _ = paths::ensure_global_dirs();
    println!("KB path: {} (global)", db_path.display());

    // 如果 args 提供了路径，使用 args；否则使用 DEFAULT_DOCS
    let args: Vec<String> = std::env::args().skip(1).collect();
    let targets: Vec<PathBuf> = if args.is_empty() {
        DEFAULT_DOCS.iter().map(|p| repo_root.join(p)).collect()
    } else {
        args.into_iter().map(PathBuf::from).collect()
    };

    let store = KnowledgeStore::new(&db_path).map_err(|e| format!("open kb: {e}"))?;

    let mut ok = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    for path in &targets {
        let path_str = path.to_string_lossy();
        match store.ingest(&path_str, false).await {
            Ok(result) => {
                let status = result.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                // ingest 返回的字段名是 "chunks"（跳过则是 "hash"）
                let chunks = result.get("chunks").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("  [{}] {} (chunks={})", status, path.display(), chunks);
                if status == "skipped" {
                    skipped += 1;
                } else {
                    ok += 1;
                }
            }
            Err(e) => {
                eprintln!("  [FAIL] {}: {}", path.display(), e);
                failed += 1;
            }
        }
    }

    println!("\nSummary: ok={ok}, skipped={skipped}, failed={failed}");
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
