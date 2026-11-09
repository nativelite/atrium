//! Test-only: the process environment, held still.
//!
//! `std::env::set_var` is process-wide and `cargo test` runs tests in
//! parallel, so a test that moves `XDG_CONFIG_HOME` while another reads
//! `global_dir()` twice sees two different parents (issue #9). Every test that
//! sets a variable, or compares paths derived from one, holds an [`EnvGuard`]:
//! one mutex for the whole crate (the per-module locks it replaces did not know
//! about each other), and whatever a guard set is put back when it drops — on
//! a panic too, where a hand-written `match prev` restore never ran.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());

pub(crate) struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    /// Hold the environment still without changing it: for a test that only
    /// reads it and must not see another test's temporary value.
    pub(crate) fn hold() -> Self {
        // A test that panicked while holding the lock poisons it; its guard's
        // drop still restored what it had set, so the lock is good to take.
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        EnvGuard {
            _lock: lock,
            saved: Vec::new(),
        }
    }

    /// Set `name` for the guard's lifetime. The value it had before the first
    /// `set` of that name (or its absence) comes back when the guard drops.
    pub(crate) fn set(&mut self, name: &'static str, value: impl AsRef<OsStr>) {
        if !self.saved.iter().any(|(n, _)| *n == name) {
            self.saved.push((name, std::env::var_os(name)));
        }
        std::env::set_var(name, value);
    }
}

impl Drop for EnvGuard {
    /// Runs while `_lock` is still held (fields drop after the body).
    fn drop(&mut self) {
        for (name, prev) in self.saved.drain(..).rev() {
            match prev {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EnvGuard;

    #[test]
    fn what_a_guard_set_comes_back_when_it_drops() {
        const NAME: &str = "ATRIUM_TEST_ENV_GUARD_PROBE";
        std::env::remove_var(NAME);
        {
            let mut guard = EnvGuard::hold();
            guard.set(NAME, "one");
            guard.set(NAME, "two"); // the FIRST previous value is what returns
            assert_eq!(std::env::var(NAME).ok().as_deref(), Some("two"));
        }
        assert_eq!(std::env::var_os(NAME), None, "absent again");
    }
}
