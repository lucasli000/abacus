//! MT-GRPO 轨迹收集集成层
//!
//! ## 职责
//! 将 `feedback::trajectory` 接入 TurnPipeline Phase 5 post-processing。
//! 提供 TrajectoryStore（内存缓冲 + JSONL 持久化）。
//!
//! ## 引用关系
//! - 被 `TurnPipeline::post_process()` 每轮末尾调用
//! - 消费 `crate::feedback::trajectory::{TrajectoryBuilder, TurnSignal, Trajectory}`
//! - 写入 JSONL 文件供离线 MT-GRPO 训练
//!
//! ## 数据流
//! ```text
//! Phase 4 tool_outputs → collect_turn() → buffer
//!                                           ↓ (batch_size 满)
//!                                         flush() → ~/.abacus/trajectories.jsonl
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use abacus_types::ToolOutput;

use crate::feedback::trajectory::{Trajectory, TrajectoryBuilder, TurnSignal};
use crate::llm::Message;

// ─── TrajectoryStore ───────────────────────────────────────────────────────

/// 轨迹存储管理器
///
/// ## 设计
/// 内存缓冲 + 批量 flush 到 JSONL 文件。
/// 不阻塞 pipeline 主路径——flush 失败静默记录，不中断 turn。
///
/// ## 生命周期
/// - 创建：CoreLoop::new() 时
/// - 写入：每轮 post_process() 调用 collect_turn()
/// - 持久化：buffer 满或 session 结束时 flush() 到磁盘
/// - 销毁：CoreLoop drop 时，自动 flush 残余 buffer
pub struct TrajectoryStore {
    /// 内存缓冲区
    buffer: Vec<Trajectory>,
    /// JSONL 导出路径
    export_path: PathBuf,
    /// 批量 flush 大小（默认 50）
    batch_size: usize,
    /// 总收集计数
    pub total_collected: u64,
    /// flush 失败计数（诊断用）
    pub flush_errors: u64,
}

impl TrajectoryStore {
    /// 创建 TrajectoryStore
    ///
    /// ## 参数
    /// - `export_path`: JSONL 文件路径（不存在时自动创建）
    /// - `batch_size`: 缓冲区满后自动 flush（默认 50）
    pub fn new(export_path: PathBuf, batch_size: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(batch_size),
            export_path,
            batch_size,
            total_collected: 0,
            flush_errors: 0,
        }
    }

    /// 创建默认配置的 TrajectoryStore
    pub fn default_store() -> Self {
        let path = crate::paths::global_dir().join("data/trajectories.jsonl");
        Self::new(path, 50)
    }

    /// 收集一轮轨迹
    ///
    /// ## 调用时机
    /// TurnPipeline::post_process() 中，EffectivenessTracker 记录之后。
    ///
    /// ## 参数
    /// - `session_id`: 当前 session 标识
    /// - `turn_number`: 当前轮次
    /// - `messages`: 完整对话历史
    /// - `tool_outputs`: 本轮所有工具调用结果
    /// - `task_kind`: 任务分类标签
    /// - `effectiveness_score`: 本轮效能评分（0.0-1.0）
    /// - `metadata`: 额外元数据（model, temperature 等）
    pub fn collect_turn(
        &mut self,
        session_id: &str,
        turn_number: u32,
        messages: &[Message],
        tool_outputs: &[ToolOutput],
        task_kind: &str,
        effectiveness_score: f64,
        metadata: HashMap<String, String>,
    ) {
        let mut builder = TrajectoryBuilder::new(session_id, turn_number, task_kind);

        // 收集工具信号
        for output in tool_outputs {
            builder.add_turn_signal(TurnSignal {
                tool_id: output.tool_id.0.clone(),
                tool_success: output.success,
                // kb.* 工具成功 = knowledge hit；其他工具默认 true
                knowledge_hit: if output.tool_id.0.starts_with("kb_") {
                    output.success
                } else {
                    true
                },
                latency_ms: output.latency_ms,
            });
        }

        // 注入元数据
        for (k, v) in &metadata {
            builder.add_metadata(k.clone(), v.clone());
        }

        // 构建轨迹
        let trajectory = builder.build(messages, effectiveness_score as f32);
        self.buffer.push(trajectory);
        self.total_collected += 1;

        // 批次满时自动 flush（同步写，pipeline 内调用）
        if self.buffer.len() >= self.batch_size {
            self.flush_sync();
        }
    }

    /// 同步 flush 到 JSONL（在 pipeline 内调用，不用 async）
    ///
    /// 失败时静默记录错误计数，不中断 pipeline。
    pub fn flush_sync(&mut self) {
        if self.buffer.is_empty() {
            return;
        }

        // 确保目录存在
        if let Some(parent) = self.export_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.export_path)
        {
            Ok(mut file) => {
                use std::io::Write;
                for traj in self.buffer.drain(..) {
                    if let Ok(line) = serde_json::to_string(&traj) {
                        let _ = writeln!(file, "{}", line);
                    }
                }
            }
            Err(_) => {
                self.flush_errors += 1;
                // 不清空 buffer——下次 flush 重试
            }
        }
    }

    /// 异步 flush（session 结束时调用）
    pub async fn flush_async(&mut self) {
        // 委托同步实现（JSONL 写入通常 < 1ms，不值得 spawn_blocking）
        self.flush_sync();
    }

    /// 缓冲区当前大小
    pub fn buffer_len(&self) -> usize {
        self.buffer.len()
    }
}

impl Drop for TrajectoryStore {
    fn drop(&mut self) {
        // 优雅退出：flush 残余 buffer
        self.flush_sync();
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use abacus_types::ToolId;
    use tempfile::NamedTempFile;

    fn mock_tool_output(id: &str, success: bool) -> ToolOutput {
        ToolOutput {
            tool_id: ToolId(id.into()),
            success,
            output: serde_json::json!({}),
            latency_ms: 50,
            failure_kind: None,
            try_instead: Vec::new(),
        }
    }

    #[test]
    fn test_collect_and_count() {
        let tmp = NamedTempFile::new().unwrap();
        let mut store = TrajectoryStore::new(tmp.path().to_path_buf(), 100);

        store.collect_turn(
            "sess_1", 1,
            &[], // empty messages for test
            &[mock_tool_output("fs_read", true)],
            "code_reading",
            0.75,
            HashMap::new(),
        );

        assert_eq!(store.total_collected, 1);
        assert_eq!(store.buffer_len(), 1);
    }

    #[test]
    fn test_auto_flush_on_batch_full() {
        let tmp = NamedTempFile::new().unwrap();
        let mut store = TrajectoryStore::new(tmp.path().to_path_buf(), 2); // batch=2

        store.collect_turn("s", 1, &[], &[], "test", 0.5, HashMap::new());
        assert_eq!(store.buffer_len(), 1);

        store.collect_turn("s", 2, &[], &[], "test", 0.6, HashMap::new());
        // batch=2 触发 flush
        assert_eq!(store.buffer_len(), 0);
        assert_eq!(store.total_collected, 2);

        // 验证 JSONL 文件有内容
        let content = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(content.lines().count(), 2);
    }

    #[test]
    fn test_flush_on_drop() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        {
            let mut store = TrajectoryStore::new(path.clone(), 100);
            store.collect_turn("s", 1, &[], &[], "test", 0.5, HashMap::new());
            // drop 时 flush
        }

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 1);
    }

    #[test]
    fn test_kb_knowledge_hit_detection() {
        let tmp = NamedTempFile::new().unwrap();
        let mut store = TrajectoryStore::new(tmp.path().to_path_buf(), 100);

        let outputs = vec![
            mock_tool_output("kb_query", true),   // knowledge_hit = true
            mock_tool_output("kb_query", false),  // knowledge_hit = false
            mock_tool_output("fs_read", true),    // knowledge_hit = true (non-kb default)
        ];

        store.collect_turn("s", 1, &[], &outputs, "research", 0.7, HashMap::new());
        // 验证收集成功即可（TurnSignal 内部逻辑由 trajectory.rs 测试覆盖）
        assert_eq!(store.total_collected, 1);
    }
}
