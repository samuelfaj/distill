//! Kill-and-reap-on-drop ownership of a `std::process::Child`.

/// Kill-and-reap-on-drop ownership of a `std::process::Child`.
///
/// RAII floor for children whose owner normally ends them explicitly (test
/// fixtures, batch helpers): an early return or assertion panic between spawn
/// and the explicit kill must not leak the process. On Unix `Child::kill` is
/// SIGKILL, so the guard also ends children that trap SIGTERM. Shared form
/// of the kill-on-drop guards previously duplicated across test fixtures
/// and helpers.
///
/// Access the child through `Deref`/`DerefMut`; after reaping it by other
/// means (e.g. a clean `try_wait`), call [`Self::into_inner`] to disarm.
///
/// Dropping the guard after an in-handle `wait`/`try_wait` reap is also
/// safe: `std::process::Child` caches the exit status, so the drop's
/// `kill()` refuses to signal an already-waited child (no recycled-PID
/// signal) and its `wait()` returns the cached status. `into_inner` just
/// skips those no-op syscall attempts.
#[must_use = "KillOnDrop kills the child when dropped; bind it for the child's intended lifetime"]
pub struct KillOnDrop(std::mem::ManuallyDrop<std::process::Child>);

impl KillOnDrop {
    pub fn new(child: std::process::Child) -> Self {
        Self(std::mem::ManuallyDrop::new(child))
    }

    /// Release the child without killing it (e.g. after it was reaped).
    #[must_use = "the released child is no longer guarded; discard it only after reaping"]
    pub fn into_inner(self) -> std::process::Child {
        let mut this = std::mem::ManuallyDrop::new(self);
        // SAFETY: `this` is wrapped in ManuallyDrop, so `KillOnDrop::drop`
        // never runs for it and the inner child is moved out exactly once.
        unsafe { std::mem::ManuallyDrop::take(&mut this.0) }
    }
}

impl std::ops::Deref for KillOnDrop {
    type Target = std::process::Child;

    fn deref(&self) -> &std::process::Child {
        &self.0
    }
}

impl std::ops::DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut std::process::Child {
        &mut self.0
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // SAFETY: drop runs at most once, and `into_inner` suppresses it via
        // ManuallyDrop, so the child is taken exactly once here.
        let mut child = unsafe { std::mem::ManuallyDrop::take(&mut self.0) };
        let _ = child.kill();
        let _ = child.wait();
    }
}

// The fixtures spawn `sleep` and probe `/proc`.
#[cfg(all(test, unix))]
#[path = "kill_on_drop_tests.rs"]
mod tests;
