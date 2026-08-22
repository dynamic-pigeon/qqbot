//! 单实例资源管理器，提供异步懒加载和空闲超时回收。

use std::{
    future::Future,
    ops::Deref,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, Weak},
    time::Duration,
};

use anyhow::{Result, anyhow};
use kovi::tokio::{runtime::Handle, sync::Mutex as AsyncMutex, task::JoinHandle};
use tracing::debug;

type BuildFuture<T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'static>>;
type Builder<T> = dyn Fn() -> BuildFuture<T> + Send + Sync + 'static;
type DestroyFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
type Destructor<T> = dyn Fn(T) -> DestroyFuture + Send + Sync + 'static;

struct ResourceState<T> {
    resource: Option<Arc<T>>,
    generation: u64,
    cleanup_task: Option<JoinHandle<()>>,
}

impl<T> ResourceState<T> {
    fn new() -> Self {
        Self {
            resource: None,
            generation: 0,
            cleanup_task: None,
        }
    }

    fn cancel_cleanup(&mut self) {
        if let Some(task) = self.cleanup_task.take() {
            task.abort();
        }
    }
}

impl<T> Drop for ResourceState<T> {
    fn drop(&mut self) {
        self.cancel_cleanup();
    }
}

struct ResourceManagerInner<T: Send + Sync + 'static> {
    state: Mutex<ResourceState<T>>,
    build_lock: AsyncMutex<()>,
    builder: Arc<Builder<T>>,
    destructor: Arc<Destructor<T>>,
    idle_timeout: Duration,
}

/// 按需构建并缓存一个共享资源，在最后一次使用结束后自动回收。
///
/// 同一时刻只会运行一个 builder。资源的空闲时间从最后一个 [`ManagedResource`]
/// 被释放时开始计算；只要仍有 lease 存活，资源就不会被管理器回收。
///
/// # Example
///
/// ```
/// use std::time::Duration;
/// use utils::ResourceManager;
///
/// # async fn example() -> anyhow::Result<()> {
/// let manager = ResourceManager::new(Duration::from_secs(60), || async {
///     Ok(String::from("resource"))
/// });
///
/// let resource = manager.get().await?;
/// assert_eq!(resource.as_str(), "resource");
/// # Ok(())
/// # }
/// ```
pub struct ResourceManager<T: Send + Sync + 'static> {
    inner: Arc<ResourceManagerInner<T>>,
}

impl<T: Send + Sync + 'static> Clone for ResourceManager<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: Send + Sync + 'static> ResourceManager<T> {
    /// `builder` 仅在缓存中没有资源时执行；构建失败不会被缓存，后续调用会重试。
    /// `idle_timeout` 从最后一个 lease 释放时开始计算。
    pub fn new<B, Fut>(idle_timeout: Duration, builder: B) -> Self
    where
        B: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        Self::new_with_destructor(idle_timeout, builder, |resource| async move {
            drop(resource);
        })
    }

    /// 创建带异步销毁回调的资源管理器。
    ///
    /// `destructor` 会在资源空闲超时或失效实例的最后一个 lease 释放后执行，适合需要
    /// 异步关闭连接、进程等资源的场景。
    pub fn new_with_destructor<B, Fut, D, DestroyFut>(
        idle_timeout: Duration,
        builder: B,
        destructor: D,
    ) -> Self
    where
        B: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
        D: Fn(T) -> DestroyFut + Send + Sync + 'static,
        DestroyFut: Future<Output = ()> + Send + 'static,
    {
        let builder = Arc::new(move || -> BuildFuture<T> { Box::pin(builder()) });
        let destructor =
            Arc::new(move |resource| -> DestroyFuture { Box::pin(destructor(resource)) });
        Self {
            inner: Arc::new(ResourceManagerInner {
                state: Mutex::new(ResourceState::new()),
                build_lock: AsyncMutex::new(()),
                builder,
                destructor,
                idle_timeout,
            }),
        }
    }

    /// 获取资源。缓存为空时自动构建，并发构建请求会合并为一次。
    pub async fn get(&self) -> Result<ManagedResource<T>> {
        let runtime = Handle::try_current()
            .map_err(|_| anyhow!("ResourceManager::get 必须在 Tokio runtime 中调用"))?;

        if let Some(resource) = self.acquire_cached(&runtime) {
            return Ok(resource);
        }

        let _build_guard = self.inner.build_lock.lock().await;
        if let Some(resource) = self.acquire_cached(&runtime) {
            return Ok(resource);
        }

        debug!("resource manager: building resource");
        let resource = Arc::new((self.inner.builder)().await?);
        let mut state = lock_state(&self.inner.state);
        state.cancel_cleanup();
        state.resource = Some(Arc::clone(&resource));
        state.generation = state.generation.wrapping_add(1);
        Ok(ManagedResource {
            resource: Some(resource),
            manager: Arc::downgrade(&self.inner),
            runtime,
        })
    }

    /// 让缓存实例失效、等旧实例销毁完成后构建新实例返回。
    ///
    /// 直接 drop 旧 lease 再 [`get`](Self::get) 时，销毁回调在最后一个 lease
    /// 释放时异步触发、无人等待，新实例可能在旧实例还没彻底销毁时就构建。
    /// 对依赖独占外部资源（如浏览器 profile 锁）的实例，二者会互相冲突。
    /// 本方法保证新实例一定在旧实例销毁完成后才构建。
    pub async fn replace(&self, mut resource: ManagedResource<T>) -> Result<ManagedResource<T>> {
        // 从缓存取出旧实例；缓存为空或传入 lease 与缓存实例不一致时无法原地
        // 替换，退化为普通 get。std MutexGuard 非 Send，取出的动作放在独立块里，
        // 让守卫在进入任何 await 之前释放。
        let old = {
            let mut state = lock_state(&self.inner.state);
            match state.resource.take() {
                Some(cached)
                    if resource
                        .resource
                        .as_ref()
                        .is_some_and(|r| Arc::ptr_eq(r, &cached)) =>
                {
                    state.cancel_cleanup();
                    state.generation = state.generation.wrapping_add(1);
                    Some(cached)
                }
                Some(cached) => {
                    state.resource = Some(cached);
                    None
                }
                None => None,
            }
        };
        let Some(old) = old else {
            drop(resource);
            return self.get().await;
        };

        // 取出 lease 持有的引用，使 lease 的 Drop 不再触发销毁，随后释放两个
        // 引用，让 `old` 成为唯一持有者，即可执行销毁回调并等待其完成。
        let lease_ref = resource.resource.take().expect("lease not released");
        drop(resource);
        drop(lease_ref);

        let _build_guard = self.inner.build_lock.lock().await;
        if let Ok(old_value) = Arc::try_unwrap(old) {
            (self.inner.destructor)(old_value).await;
        } else {
            debug!("resource manager: replace 时旧实例仍有其他引用，跳过显式销毁");
        }

        let runtime = Handle::try_current()?;
        let new_resource = Arc::new((self.inner.builder)().await?);
        let mut state = lock_state(&self.inner.state);
        state.resource = Some(Arc::clone(&new_resource));
        state.generation = state.generation.wrapping_add(1);
        Ok(ManagedResource {
            resource: Some(new_resource),
            manager: Arc::downgrade(&self.inner),
            runtime,
        })
    }

    fn acquire_cached(&self, runtime: &Handle) -> Option<ManagedResource<T>> {
        let mut state = lock_state(&self.inner.state);
        let resource = Arc::clone(state.resource.as_ref()?);
        state.cancel_cleanup();
        state.generation = state.generation.wrapping_add(1);
        Some(ManagedResource {
            resource: Some(resource),
            manager: Arc::downgrade(&self.inner),
            runtime: runtime.clone(),
        })
    }
}

fn lock_state<T>(state: &Mutex<ResourceState<T>>) -> MutexGuard<'_, ResourceState<T>> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 资源的使用 lease。
///
/// 可通过 [`Deref`] 直接访问资源。最后一个 lease 释放后，管理器开始计算空闲超时。
pub struct ManagedResource<T: Send + Sync + 'static> {
    resource: Option<Arc<T>>,
    manager: Weak<ResourceManagerInner<T>>,
    runtime: Handle,
}

impl<T: Send + Sync + 'static> Deref for ManagedResource<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.resource
            .as_deref()
            .expect("managed resource already released")
    }
}

impl<T: Send + Sync + 'static> Drop for ManagedResource<T> {
    fn drop(&mut self) {
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        // 引用已被显式取出（如 [`ResourceManager::replace`] 接管销毁）时，
        // 不再触碰管理器；此时对 `Deref` 的使用仍是 bug，但 drop 本身应安全。
        let Some(resource) = self.resource.as_ref() else {
            return;
        };

        let mut state = lock_state(&manager.state);
        let is_cached = state
            .resource
            .as_ref()
            .is_some_and(|cached| Arc::ptr_eq(cached, resource));
        if !is_cached {
            if Arc::strong_count(resource) == 1 {
                let resource = self
                    .resource
                    .take()
                    .expect("managed resource already released");
                let destructor = Arc::clone(&manager.destructor);
                drop(state);
                self.runtime.spawn(destroy_resource(destructor, resource));
            }
            return;
        }
        // 缓存引用和当前 lease 是仅存的两个引用时，当前 lease 即为最后一个使用者。
        if Arc::strong_count(resource) != 2 {
            return;
        }

        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        state.cancel_cleanup();
        let idle_timeout = manager.idle_timeout;
        let manager = Arc::downgrade(&manager);
        drop(
            self.resource
                .take()
                .expect("managed resource already released"),
        );
        state.cleanup_task = Some(self.runtime.spawn(async move {
            kovi::tokio::time::sleep(idle_timeout).await;
            let Some(manager) = manager.upgrade() else {
                return;
            };
            let resource = {
                let mut state = lock_state(&manager.state);
                if state.generation != generation {
                    return;
                }
                state.cleanup_task.take();
                let resource = state.resource.take();
                let destructor = Arc::clone(&manager.destructor);
                (resource, destructor)
            };
            if let (Some(resource), destructor) = resource {
                debug!("resource manager: idle resource expired");
                destroy_resource(destructor, resource).await;
            }
        }));
    }
}

async fn destroy_resource<T: Send + Sync + 'static>(
    destructor: Arc<Destructor<T>>,
    resource: Arc<T>,
) {
    match Arc::try_unwrap(resource) {
        Ok(resource) => destructor(resource).await,
        Err(resource) => {
            debug!("resource manager: resource still has active references during destruction");
            drop(resource);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct TrackedResource {
        value: usize,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for TrackedResource {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn builds_lazily_and_reuses_cached_resource() {
        let builds = Arc::new(AtomicUsize::new(0));
        let manager = ResourceManager::new(Duration::from_secs(10), {
            let builds = Arc::clone(&builds);
            move || {
                let builds = Arc::clone(&builds);
                async move { Ok(builds.fetch_add(1, Ordering::SeqCst) + 1) }
            }
        });

        let first = manager.get().await.unwrap();
        let second = manager.get().await.unwrap();

        assert_eq!(*first, 1);
        assert_eq!(*second, 1);
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_cold_access_only_builds_once() {
        let builds = Arc::new(AtomicUsize::new(0));
        let manager = ResourceManager::new(Duration::from_secs(10), {
            let builds = Arc::clone(&builds);
            move || {
                let builds = Arc::clone(&builds);
                async move {
                    builds.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    Ok(42)
                }
            }
        });

        let (first, second, third) = tokio::join!(manager.get(), manager.get(), manager.get());

        assert_eq!(*first.unwrap(), 42);
        assert_eq!(*second.unwrap(), 42);
        assert_eq!(*third.unwrap(), 42);
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn expires_only_after_last_lease_is_released() {
        let builds = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let manager = ResourceManager::new(Duration::from_secs(10), {
            let builds = Arc::clone(&builds);
            let drops = Arc::clone(&drops);
            move || {
                let builds = Arc::clone(&builds);
                let drops = Arc::clone(&drops);
                async move {
                    Ok(TrackedResource {
                        value: builds.fetch_add(1, Ordering::SeqCst) + 1,
                        drops,
                    })
                }
            }
        });

        let resource = manager.get().await.unwrap();
        tokio::time::advance(Duration::from_secs(20)).await;
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        drop(resource);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        let rebuilt = manager.get().await.unwrap();
        assert_eq!(rebuilt.value, 2);
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn reuse_restarts_the_idle_timeout() {
        let destroyed = Arc::new(AtomicUsize::new(0));
        let manager =
            ResourceManager::new_with_destructor(Duration::from_secs(10), || async { Ok(42) }, {
                let destroyed = Arc::clone(&destroyed);
                move |_| {
                    let destroyed = Arc::clone(&destroyed);
                    async move {
                        destroyed.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });

        drop(manager.get().await.unwrap());
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;

        drop(manager.get().await.unwrap());
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        assert_eq!(destroyed.load(Ordering::SeqCst), 0);

        tokio::time::advance(Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        assert_eq!(destroyed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_build_is_retried() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let manager = ResourceManager::new(Duration::from_secs(10), {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        anyhow::bail!("first build failed");
                    }
                    Ok(42)
                }
            }
        });

        assert!(manager.get().await.is_err());
        assert_eq!(*manager.get().await.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn replace_destroys_old_before_building_new() {
        let destroyed = Arc::new(AtomicUsize::new(0));
        let manager =
            ResourceManager::new_with_destructor(Duration::from_secs(10), || async { Ok(42) }, {
                let destroyed = Arc::clone(&destroyed);
                move |resource| {
                    let destroyed = Arc::clone(&destroyed);
                    async move {
                        assert_eq!(resource, 42);
                        destroyed.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });

        let old = manager.get().await.unwrap();
        let new_lease = manager.replace(old).await.unwrap();

        assert_eq!(*new_lease, 42);
        // replace 返回时销毁回调已跑完。
        assert_eq!(destroyed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_runs_custom_destructor() {
        let destroyed = Arc::new(AtomicUsize::new(0));
        let manager =
            ResourceManager::new_with_destructor(Duration::from_secs(10), || async { Ok(42) }, {
                let destroyed = Arc::clone(&destroyed);
                move |resource| {
                    let destroyed = Arc::clone(&destroyed);
                    async move {
                        assert_eq!(resource, 42);
                        destroyed.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });

        drop(manager.get().await.unwrap());
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;

        assert_eq!(destroyed.load(Ordering::SeqCst), 1);
    }
}
