// L2-10: 允许死代码 — 这些模块为未来功能预留（repl, disclaimer, layout 等）
#![allow(dead_code)]

// V13 生产级稳定 — 集中放行风格类 clippy 警告（行为等价惯用法），真问题已逐项修复
//
// 放行原则：
//   - 已确认行为等价、不影响正确性 / 性能 / 安全的"风格警告"
//   - 真风险（如 Iterator::last on DoubleEndedIterator → next_back、OpenOptions truncate
//     未指定、unwrap on Option that may be None 等）必须显式修，不在此放行
//
// 维护提示：新增警告时优先评估"是真问题还是风格"——是真问题就修，风格则归此清单
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_match)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::manual_div_ceil)]
#![allow(clippy::manual_is_multiple_of)]
#![allow(clippy::manual_clamp)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::unnecessary_filter_map)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::if_same_then_else)]
#![allow(clippy::useless_conversion)]
#![allow(clippy::manual_contains)]
#![allow(clippy::filter_map_bool_then)]
#![allow(clippy::map_flatten)]
#![allow(clippy::manual_checked_ops)]
#![allow(clippy::vec_init_then_push)]

// Re-exports for ease of use
pub use clap;
pub use color_eyre;
pub use owo_colors;
pub use thiserror;
pub use tracing;
pub use tracing_subscriber;

// Core CLI functionality
pub mod commands;
pub mod output;
pub mod pipe;
pub mod engine_init;

// TUI module
pub mod tui;

// Main entry point
pub use crate::commands::*;
pub use crate::output::*;
pub use crate::pipe::*;

// Re-export from submodules
pub use commands::ChatArgs;
pub use commands::ExecArgs;
pub use commands::SessionArgs;
pub use commands::SkillArgs;
pub use commands::ConfigArgs;
pub use commands::ModelArgs;
pub use output::OutputFormat;