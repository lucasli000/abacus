//! Cold Block Batch Writer — buffer + flush 模式
//!
//! 防止 per-message SQLite 写入延迟：先 buffer 到内存 Vec，
//! 达到 cap 后批量 flush 写入 ColdTier（message_blocks 表）。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::core::context::{BlockRecord, SessionStore};

/// Cold 数据写入接口——供 TriageEngine COLD action 调用
pub trait ColdBlockWriter: Send + Sync {
    fn save_block<'a>(&'a self, block: BlockRecord) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

/// 批量写入器——先 buffer 再 flush
pub struct ColdBufferWriter {
    buffer: RwLock<Vec<BlockRecord>>,
    cap: usize,
    store: Arc<dyn SessionStore>,
    flush_count: AtomicU32,
}

impl ColdBufferWriter {
    pub fn new(store: Arc<dyn SessionStore>, cap: usize) -> Self {
        Self {
            buffer: RwLock::new(Vec::new()),
            cap,
            store,
            flush_count: AtomicU32::new(0),
        }
    }

    /// 推入缓冲区；达 cap 时自动 flush
    pub async fn push(&self, block: BlockRecord) {
        {
            let mut buf = self.buffer.write().await;
            buf.push(block);
            if buf.len() < self.cap {
                return;
            }
        }
        self.flush().await;
    }

    /// 强制 flush
    pub async fn flush(&self) {
        let batch = {
            let mut buf = self.buffer.write().await;
            if buf.is_empty() {
                return;
            }
            std::mem::take(&mut *buf)
        };

        let count = self.flush_count.fetch_add(1, Ordering::SeqCst);
        tracing::debug!("cold_buffer flush #{}: {} blocks", count, batch.len());

        for block in batch {
            if let Err(e) = self.store.save_block(block).await {
                tracing::warn!("cold_buffer flush: save_block failed: {e}");
            }
        }
    }
}

impl ColdBlockWriter for ColdBufferWriter {
    fn save_block<'a>(&'a self, block: BlockRecord) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move { self.push(block).await })
    }
}
