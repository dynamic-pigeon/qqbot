use std::{marker::PhantomData, ops::Deref, sync::Arc};

use arc_swap::{ArcSwap, Guard};

/// 适合读多写少场景的无锁快照容器。
pub struct RcuCell<T> {
    inner: ArcSwap<T>,
}

impl<T> RcuCell<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: ArcSwap::from_pointee(value),
        }
    }

    #[inline(always)]
    pub fn read(&self) -> RcuReadGuard<'_, T> {
        RcuReadGuard {
            guard: self.inner.load(),
            _marker: PhantomData,
        }
    }

    pub fn replace(&self, next: T) {
        self.inner.store(Arc::new(next));
    }

    pub fn snapshot(&self) -> T
    where
        T: Clone,
    {
        self.inner.load().as_ref().clone()
    }
}

/// 持有底层 `Arc` 的一致性读取快照。
pub struct RcuReadGuard<'a, T> {
    guard: Guard<Arc<T>>,
    _marker: PhantomData<&'a RcuCell<T>>,
}

impl<T> Deref for RcuReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::RcuCell;

    #[test]
    fn existing_guard_keeps_its_snapshot_after_replace() {
        let cell = RcuCell::new(String::from("old"));
        let old = cell.read();
        cell.replace(String::from("new"));
        assert_eq!(old.as_str(), "old");
        assert_eq!(cell.read().as_str(), "new");
    }
}
