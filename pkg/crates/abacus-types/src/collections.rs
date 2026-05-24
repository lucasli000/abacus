//! # BoundedFifo — 有上限的先进先出容器
//!
//! ## 场景
//! 后端多处都需要"保留最近 N 条"的有界滚动语义：
//! - `RateLimiter.clients`（OOM 防护，配 GC）
//! - `ServerSessionManager.snapshots`
//! - `ContextManager.retained_content` / `pending`
//! - `SecretsManager.audit_log`
//! - `McipGateway.decision_log`
//!
//! 之前散落用 `Vec` + `Vec::remove(0)`（O(n) 每次）或 `Vec::drain(..k)`，
//! 既效率低也容易写错边界。`BoundedFifo` 用 `VecDeque` 做 O(1) push/pop，
//! 容量上限达到后自动丢弃最旧的元素。
//!
//! ## 引用关系
//! - 定义在 `abacus-types` 让所有 crate（core/orchestrator/server）都能用
//! - 不依赖 tokio，纯 std；调用方决定是否包 RwLock/Mutex
//!
//! ## 边界
//! - 容量为 0 时所有 push 都立即丢弃（合理：调用方该用 `Option<T>` 替代）
//! - 不是线程安全：调用方负责包同步原语
//! - 不实现 Clone（避免误复制大容器）

use std::collections::VecDeque;

#[derive(Debug)]
pub struct BoundedFifo<T> {
    inner: VecDeque<T>,
    capacity: usize,
}

impl<T> BoundedFifo<T> {
    /// 创建空 fifo，指定上限。capacity=0 等同 disabled（push 立即丢弃）。
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: VecDeque::with_capacity(capacity.min(64)),
            capacity,
        }
    }

    /// 推入新元素；超出 capacity 时丢弃最旧（front）的元素。
    /// 返回 Some(被丢弃的元素) 或 None。
    pub fn push(&mut self, item: T) -> Option<T> {
        if self.capacity == 0 {
            return Some(item);
        }
        let dropped = if self.inner.len() >= self.capacity {
            self.inner.pop_front()
        } else {
            None
        };
        self.inner.push_back(item);
        dropped
    }

    pub fn len(&self) -> usize { self.inner.len() }
    pub fn is_empty(&self) -> bool { self.inner.is_empty() }
    pub fn capacity(&self) -> usize { self.capacity }

    /// 最旧的元素（队首）
    pub fn front(&self) -> Option<&T> { self.inner.front() }

    /// 最新的元素（队尾）
    pub fn back(&self) -> Option<&T> { self.inner.back() }

    /// 迭代（旧到新顺序，支持双端 .rev()）
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator { self.inner.iter() }

    /// 反向迭代（新到旧）
    pub fn iter_rev(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator { self.inner.iter().rev() }

    /// 全清
    pub fn clear(&mut self) { self.inner.clear(); }

    /// 移除满足 predicate 的元素（保留 retain 语义）
    pub fn retain<F: FnMut(&T) -> bool>(&mut self, f: F) {
        self.inner.retain(f);
    }

    /// 转为 Vec（拷贝；用于序列化或外部 API 兼容）
    pub fn to_vec(&self) -> Vec<T> where T: Clone {
        self.inner.iter().cloned().collect()
    }

    /// drain 全部（move 语义）
    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.inner.drain(..)
    }

    /// 批量 push（替代 Vec.extend）；返回被丢弃的元素数
    pub fn push_iter<I: IntoIterator<Item = T>>(&mut self, iter: I) -> usize {
        let mut dropped = 0;
        for item in iter {
            if self.push(item).is_some() { dropped += 1; }
        }
        dropped
    }

    /// 取最后 n 个元素（新到旧）的引用（克隆为 Vec 由调用方决定）
    pub fn tail(&self, n: usize) -> impl DoubleEndedIterator<Item = &T> {
        let len = self.inner.len();
        let start = len.saturating_sub(n);
        self.inner.range(start..len)
    }
}

impl<T> Default for BoundedFifo<T> {
    fn default() -> Self { Self::new(1024) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_under_capacity() {
        let mut q: BoundedFifo<i32> = BoundedFifo::new(3);
        assert_eq!(q.push(1), None);
        assert_eq!(q.push(2), None);
        assert_eq!(q.push(3), None);
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn test_push_evicts_oldest() {
        let mut q: BoundedFifo<i32> = BoundedFifo::new(3);
        q.push(1); q.push(2); q.push(3);
        assert_eq!(q.push(4), Some(1)); // evict oldest
        assert_eq!(q.push(5), Some(2));
        assert_eq!(q.iter().copied().collect::<Vec<_>>(), vec![3, 4, 5]);
    }

    #[test]
    fn test_zero_capacity() {
        let mut q: BoundedFifo<i32> = BoundedFifo::new(0);
        assert_eq!(q.push(42), Some(42)); // immediately dropped
        assert!(q.is_empty());
    }

    #[test]
    fn test_iter_rev() {
        let mut q: BoundedFifo<i32> = BoundedFifo::new(5);
        q.push(1); q.push(2); q.push(3);
        assert_eq!(q.iter_rev().copied().collect::<Vec<_>>(), vec![3, 2, 1]);
    }

    #[test]
    fn test_retain() {
        let mut q: BoundedFifo<i32> = BoundedFifo::new(5);
        q.push(1); q.push(2); q.push(3); q.push(4);
        q.retain(|&x| x % 2 == 0);
        assert_eq!(q.iter().copied().collect::<Vec<_>>(), vec![2, 4]);
    }
}
