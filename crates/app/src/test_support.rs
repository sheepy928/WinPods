//! Test-only helpers shared by the unit tests of this crate.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Serialises tests that change process environment variables.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Sets an environment variable for the lifetime of the guard (restoring the previous
/// value on drop) while holding [`ENV_LOCK`].
pub(crate) struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvVarGuard {
    pub(crate) fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let previous = std::env::var_os(key);
        // SAFETY: on Windows the environment block is guarded by std's own lock, and
        // every test that writes variables holds ENV_LOCK.
        unsafe { std::env::set_var(key, value) };
        Self {
            key,
            previous,
            _lock: lock,
        }
    }

    pub(crate) fn unset(key: &'static str) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let previous = std::env::var_os(key);
        // SAFETY: as in `set`.
        unsafe { std::env::remove_var(key) };
        Self {
            key,
            previous,
            _lock: lock,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: as in `set`; the lock is still held while restoring.
        unsafe {
            match &self.previous {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// A random hex suffix for test-only object names (registry keys, kernel objects).
pub(crate) fn unique_suffix() -> String {
    let r = getrandom::u64().unwrap_or(0);
    format!("{}-{r:016x}", std::process::id())
}
