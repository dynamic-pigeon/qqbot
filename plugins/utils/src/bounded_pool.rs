//! 有界许可池：限制并发资源使用（典型用途：子进程、浏览器上下文等重资源）。
//!
//! 与裸 [`tokio::sync::Semaphore`] 的区别是这里在 acquire 路径上加了 `wait_timeout`，
//! 避免排队的调用者无限阻塞；当 N 个槽位全被占用时，新调用最多等 `wait_timeout`,
//! 超时则返回 `Err`，让上层做「忙退避」/「直接拒绝用户」之类的策略。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use kovi::tokio::sync::OwnedSemaphorePermit;

#[derive(Clone)]
pub struct BoundedPool {
    semaphore: Arc<kovi::tokio::sync::Semaphore>,
    max: usize,
}

impl BoundedPool {
    /// 创建容量为 `max_concurrent` 的资源池。`max_concurrent` 至少为 1。
    pub fn new(max_concurrent: usize) -> Self {
        let max = max_concurrent.max(1);
        Self {
            semaphore: Arc::new(kovi::tokio::sync::Semaphore::new(max)),
            max,
        }
    }

    /// 获取一个许可，最多等待 `wait_timeout`。
    /// - 池有空闲 → 立即返回
    /// - 池满 → 排队等 ≤ `wait_timeout`，超时返回 `Err`
    /// - semaphore 已 close → 返回 `Err`
    pub async fn acquire(&self, wait_timeout: Duration) -> Result<OwnedSemaphorePermit> {
        kovi::tokio::time::timeout(wait_timeout, self.semaphore.clone().acquire_owned())
            .await
            .map_err(|_| anyhow!("bounded pool 等待许可超时 (>{:?})", wait_timeout))?
            .map_err(|e| anyhow!("semaphore closed: {}", e))
    }

    /// 当前可用许可数（观测用）。
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// 池容量上限。
    pub fn max_concurrency(&self) -> usize {
        self.max
    }
}
