use std::{marker::PhantomData, sync::atomic::Ordering};

use crossbeam_epoch::{Atomic, Owned};

/// A lightweight RCU container with lock-free reads and copy-on-write updates.
pub struct RcuCell<T> {
    inner: Atomic<T>,
}

impl<T> RcuCell<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: Atomic::new(value),
        }
    }

    #[inline(always)]
    pub fn read(&self) -> RcuReadGuard<'_, T> {
        RcuReadGuard::new(&self.inner)
    }

    pub fn replace(&self, next: T) {
        let guard = &crossbeam_epoch::pin();
        // Release: 保证新值在指针发布前对所有读者可见。
        let prev = self.inner.swap(Owned::new(next), Ordering::Release, guard);
        // Safety: the old value stays valid until all active readers leave their epoch.
        unsafe {
            guard.defer_destroy(prev);
        }
    }

    pub fn snapshot(&self) -> T
    where
        T: Clone,
    {
        let guard = &crossbeam_epoch::pin();
        // Acquire: 与 writer 的 Release swap 配对，确保读到完整初始化的新值。
        let current = self.inner.load(Ordering::Acquire, guard);
        // Safety: pointer remains valid while the guard is pinned.
        unsafe { current.deref().clone() }
    }
}

/// Guarded read handle for a value stored in [`RcuCell`].
///
/// NOTE: this is `!Send`; do not move it across threads.
pub struct RcuReadGuard<'a, T> {
    #[allow(dead_code)]
    guard: crossbeam_epoch::Guard,
    ptr: *const T,
    _marker: PhantomData<&'a ()>,
}

impl<'a, T> RcuReadGuard<'a, T> {
    fn new(value: &'a Atomic<T>) -> Self {
        let guard = crossbeam_epoch::pin();
        let ptr = value.load(Ordering::Relaxed, &guard).as_raw();
        Self {
            guard,
            ptr,
            _marker: PhantomData,
        }
    }

    fn load(&self) -> &T {
        debug_assert!(!self.ptr.is_null(), "RcuReadGuard::ptr not initialized");
        // Safety: pointer stays valid during the lifetime of this guard.
        unsafe { &*self.ptr }
    }
}

impl<T> std::ops::Deref for RcuReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.load()
    }
}
