//! 有界资源池：限制并发资源使用（典型用途：子进程、浏览器上下文等重资源）。
//!
//! 与裸 [`tokio::sync::Semaphore`] 的区别是这里在 acquire 路径上加了 `wait_timeout`，
//! 避免排队的调用者无限阻塞；当 N 个槽位全被占用时，新调用最多等 `wait_timeout`,
//! 超时则返回 `Err`，让上层做「忙退避」/「直接拒绝用户」之类的策略。
//!
//! 此外提供 [`BoundedResourcePool<T>`]，在许可控制之上再加一层 idle 资源缓存：
//! - 有可用 idle 资源 → 直接复用
//! - 没有 → 调用 `init` 创建新资源
//! - guard drop 时 healthy 的资源回到 idle 池，由后台任务按超时清理
//!
//! 后台 cleanup 任务在首次 `acquire` 时启动；task 内部用 `Weak` 引用 inner，
//! pool 本身被 drop 后 task 在下一次 tick 时 `upgrade()` 返回 `None` 自然退出循环。
//! 故意不强制 `abort`——避免打断 task 正在执行的清理逻辑。

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use kovi::tokio::sync::OwnedSemaphorePermit;
use tracing::debug;

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

struct IdleEntry<T> {
    item: T,
    last_used: Instant,
}

struct BoundedResourcePoolInner<T> {
    semaphore: BoundedPool,
    idle: Mutex<VecDeque<IdleEntry<T>>>,
    idle_timeout: Duration,
    cleanup_interval: Duration,
    cleanup_started: Once,
}

/// 带 idle 缓存的有界资源池。
///
/// 同时存在的资源总数（借出 + idle）被 `max_concurrent` 限制；
/// guard drop 时 healthy 的资源会回到 idle 池等待复用，超过 `idle_timeout` 未复用则由
/// 后台 cleanup 任务销毁。
pub struct BoundedResourcePool<T: Send + 'static> {
    inner: Arc<BoundedResourcePoolInner<T>>,
}

impl<T: Send + 'static> Clone for BoundedResourcePool<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: Send + 'static> BoundedResourcePool<T> {
    /// 创建资源池。
    ///
    /// - `max_concurrent`：同时最多存在的资源数量（借出 + idle）。
    /// - `idle_timeout`：idle 资源超过多久未被复用就被清理。
    /// - `cleanup_interval`：后台清理任务运行间隔。
    pub fn new(max_concurrent: usize, idle_timeout: Duration, cleanup_interval: Duration) -> Self {
        Self {
            inner: Arc::new(BoundedResourcePoolInner {
                semaphore: BoundedPool::new(max_concurrent),
                idle: Mutex::new(VecDeque::new()),
                idle_timeout,
                cleanup_interval,
                cleanup_started: Once::new(),
            }),
        }
    }

    /// 从池中获取一个资源 guard。
    ///
    /// - 先尝试获取一个并发许可；若当前已达上限则排队等待 `wait_timeout`。
    /// - 拿到许可后，优先从 idle 池弹出可用资源。
    /// - idle 池空时调用 `init` 创建新资源。
    pub async fn acquire<F, Fut>(&self, wait_timeout: Duration, init: F) -> Result<ResourceGuard<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let permit = self.inner.semaphore.acquire(wait_timeout).await?;
        self.ensure_cleanup_task_spawned();

        if let Some(entry) = lock_idle(&self.inner.idle).pop_front() {
            debug!("bounded resource pool: reuse idle resource");
            return Ok(ResourceGuard {
                item: Some(entry.item),
                healthy: true,
                permit,
                pool: self.clone(),
            });
        }

        debug!("bounded resource pool: create new resource");
        let item = init().await?;
        Ok(ResourceGuard {
            item: Some(item),
            healthy: true,
            permit,
            pool: self.clone(),
        })
    }

    fn ensure_cleanup_task_spawned(&self) {
        // task 用 `Weak` 持有 inner；pool 被 drop 后 `upgrade()` 返回 None，
        // task 在下一个 cleanup_interval tick 时自然退出循环（不强制 abort，
        // 避免打断 task 正在执行的清理逻辑）。
        // spawn 返回的 JoinHandle 直接 drop，task 进入 detached 状态——自然退出路径
        // 同样会清理它的栈帧。
        let inner_weak = Arc::downgrade(&self.inner);
        let idle_timeout = self.inner.idle_timeout;
        let cleanup_interval = self.inner.cleanup_interval;
        self.inner.cleanup_started.call_once(move || {
            kovi::spawn(async move {
                let mut interval = kovi::tokio::time::interval(cleanup_interval);
                // 第一次 tick 立即触发，pool 初始为空，是 no-op
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let Some(inner) = inner_weak.upgrade() else {
                        // pool 已被 drop，沿自然退出路径跳出循环。
                        break;
                    };
                    let Ok(mut idle) = inner.idle.lock() else {
                        tracing::warn!("bounded resource pool: idle mutex poisoned, skipping cleanup");
                        continue;
                    };
                    let before = idle.len();
                    let now = Instant::now();
                    // FIFO：front 是最老的 entry。front 没过期则后面都更新。
                    while let Some(front) = idle.front() {
                        if now.duration_since(front.last_used) < idle_timeout {
                            break;
                        }
                        idle.pop_front();
                    }
                    let removed = before - idle.len();
                    if removed > 0 {
                        debug!(
                            "bounded resource pool: cleanup removed {removed} idle resources ({before} → {})",
                            idle.len()
                        );
                    }
                }
            });
        });
    }
}

fn lock_idle<T>(
    idle: &Mutex<VecDeque<IdleEntry<T>>>,
) -> std::sync::MutexGuard<'_, VecDeque<IdleEntry<T>>> {
    idle.lock().unwrap_or_else(|p| p.into_inner())
}

/// 资源句柄的 RAII 包装。
///
/// `Deref` / `DerefMut` 到 `T`，所以可以直接对 guard 调用目标资源的方法。
/// guard drop 时：healthy 的资源会归还到 idle 池；被 [`mark_unhealthy`] 标记过的资源
/// 会直接丢弃，避免污染连接池。
pub struct ResourceGuard<T: Send + 'static> {
    item: Option<T>,
    healthy: bool,
    #[allow(dead_code)]
    permit: OwnedSemaphorePermit,
    pool: BoundedResourcePool<T>,
}

impl<T: Send + 'static> ResourceGuard<T> {
    /// 标记当前资源处于异常状态，归还时直接丢弃。
    pub fn mark_unhealthy(&mut self) {
        self.healthy = false;
    }
}

impl<T: Send + 'static> std::ops::Deref for ResourceGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.item
            .as_ref()
            .expect("ResourceGuard item already taken before drop")
    }
}

impl<T: Send + 'static> std::ops::DerefMut for ResourceGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.item
            .as_mut()
            .expect("ResourceGuard item already taken before drop")
    }
}

impl<T: Send + 'static> Drop for ResourceGuard<T> {
    fn drop(&mut self) {
        let Some(item) = self.item.take() else {
            return;
        };
        if !self.healthy {
            debug!("bounded resource pool: dropping unhealthy resource");
            return;
        }
        let Ok(mut idle) = self.pool.inner.idle.lock() else {
            tracing::warn!("bounded resource pool: idle mutex poisoned, leaking resource");
            return;
        };
        idle.push_back(IdleEntry {
            item,
            last_used: Instant::now(),
        });
        debug!(
            "bounded resource pool: returned resource to idle pool (size={})",
            idle.len()
        );
    }
}

/// `BoundedResourcePool` 通过 `Arc` 共享，`Drop` 只在最后一个 clone 被释放时触发。
/// 后台 cleanup task 内部用 `Weak` 持有 inner；这里 pool 被 drop 后 `Arc<Inner>` 释放，
/// task 下次 tick 时 `inner_weak.upgrade()` 返回 `None` 自然退出循环。
/// 故意不调用 `abort`——避免打断 task 正在执行的清理逻辑。
impl<T: Send + 'static> Drop for BoundedResourcePool<T> {
    fn drop(&mut self) {
        debug!("bounded resource pool: dropped, cleanup task will exit on next tick");
    }
}
