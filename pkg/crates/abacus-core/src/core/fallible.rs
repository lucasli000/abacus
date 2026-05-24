//! Fallible — zero-panic production code helpers
//!
//! Provides alternatives to unwrap()/expect() that return structured errors.
//! Used throughout production code to eliminate panic risks.

use abacus_types::KernelError;

/// Extension trait for Option — contextual error instead of panic.
pub trait OptionExt<T> {
    fn or_kernel_err(self, context: &str) -> Result<T, KernelError>;
}

impl<T> OptionExt<T> for Option<T> {
    fn or_kernel_err(self, context: &str) -> Result<T, KernelError> {
        self.ok_or_else(|| KernelError::Other(format!("unexpected None: {}", context)))
    }
}

/// Extension trait for Result — add context to any error.
pub trait ResultExt<T, E: std::fmt::Display> {
    fn with_context(self, context: &str) -> Result<T, KernelError>;
}

impl<T, E: std::fmt::Display> ResultExt<T, E> for Result<T, E> {
    fn with_context(self, context: &str) -> Result<T, KernelError> {
        self.map_err(|e| KernelError::Other(format!("{}: {}", context, e)))
    }
}

/// Extension trait for std::sync::Mutex — poison recovery instead of panic.
pub trait MutexExt<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for std::sync::Mutex<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("recovered from poisoned mutex");
            poisoned.into_inner()
        })
    }
}

/// Extension trait for std::sync::RwLock — poison recovery.
pub trait RwLockExt<T> {
    fn read_or_recover(&self) -> std::sync::RwLockReadGuard<'_, T>;
    fn write_or_recover(&self) -> std::sync::RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for std::sync::RwLock<T> {
    fn read_or_recover(&self) -> std::sync::RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|poisoned| {
            tracing::warn!("recovered from poisoned rwlock (read)");
            poisoned.into_inner()
        })
    }
    fn write_or_recover(&self) -> std::sync::RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(|poisoned| {
            tracing::warn!("recovered from poisoned rwlock (write)");
            poisoned.into_inner()
        })
    }
}
