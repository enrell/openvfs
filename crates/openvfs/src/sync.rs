//! Lock helpers.
//!
//! A panic while a lock is held poisons it. Every critical section in this
//! crate leaves its data structurally valid at each step, so the poison flag
//! carries no information for us and we simply keep going.

use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub(crate) fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}
