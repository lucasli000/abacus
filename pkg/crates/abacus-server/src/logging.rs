//! Structured logging setup for Abacus Server.
//!
//! Outputs JSON logs to file (rotated daily) + plain text to stdout.
//! Configured via environment:
//! - `RUST_LOG`: filter level (default: info)
//! - `ABACUS_LOG_DIR`: directory for log files (default: ./logs)

use tracing_subscriber::{fmt, EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize logging with dual output: stdout (human) + file (JSON, daily rotation).
pub fn init() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("abacus_engine=info,abacus_server=info"));

    let stdout_layer = fmt::layer()
        .with_target(true)
        .with_thread_ids(false)
        .compact();

    // File logging (JSON, daily rotation)
    let log_dir = std::env::var("ABACUS_LOG_DIR").unwrap_or_else(|_| "./logs".into());
    let file_appender = tracing_appender::rolling::daily(&log_dir, "abacus-server.log");
    let file_layer = fmt::layer()
        .json()
        .with_writer(file_appender)
        .with_target(true)
        .with_span_events(fmt::format::FmtSpan::CLOSE);

    tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    tracing::info!(log_dir = %log_dir, "Logging initialized");
}
