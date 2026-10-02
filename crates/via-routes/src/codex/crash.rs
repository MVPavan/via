//! The registry's crash-only pieces (x.3.2 X0 item 2.5, Ruling D): a
//! panic in code that holds the registry guard, or in the supervisor, is a
//! VIA bug, and the daemon aborts rather than run on half-applied state.
//! Recovery is the existing crash path: each anchor cleans its group on its
//! control's end, and the next daemon's restart recovery ends each
//! in-flight turn `unknown`.

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

/// The registry's lock guard. It records whether a panic was already
/// unwinding when it was taken; its `Drop`, which runs before the inner
/// guard's, aborts the process when a panic began while it was held. So
/// the mutex is never poisoned and no caller sees a half-applied
/// mutation. A guard taken during an unrelated unwind (a pin dropped by a
/// panicking task) does not abort for that panic.
pub struct RegistryGuard<'a, T> {
    unwinding: bool,
    inner: MutexGuard<'a, T>,
}

/// Takes `mutex` as a [`RegistryGuard`]. A poisoned mutex cannot occur
/// (a panic under the guard aborts first); its state is taken as is.
pub fn lock<T>(mutex: &Mutex<T>) -> RegistryGuard<'_, T> {
    let unwinding = std::thread::panicking();
    RegistryGuard {
        unwinding,
        inner: mutex.lock().unwrap_or_else(PoisonError::into_inner),
    }
}

impl<T> Deref for RegistryGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for RegistryGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> Drop for RegistryGuard<'_, T> {
    fn drop(&mut self) {
        if !self.unwinding && std::thread::panicking() {
            std::process::abort();
        }
    }
}

/// A future whose poll and destruction abort the process on a panic
/// ([`crash_on_panic`]).
pub struct CrashOnPanic<F> {
    inner: Option<Pin<Box<F>>>,
}

/// Wraps `future` so that a panic while polling it, or while destroying
/// it (at completion inside the same scope, or when the wrapper is
/// dropped pending), aborts the process: Tokio's catch of a task's panic
/// never sees one (r8 R8-2).
pub fn crash_on_panic<F: Future>(future: F) -> CrashOnPanic<F> {
    CrashOnPanic {
        inner: Some(Box::pin(future)),
    }
}

/// Drops `value` under `catch_unwind`, aborting on a panic.
fn drop_or_abort<T>(value: T) {
    if catch_unwind(AssertUnwindSafe(move || drop(value))).is_err() {
        std::process::abort();
    }
}

impl<F: Future> Future for CrashOnPanic<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = &mut *self;
        let Some(inner) = this.inner.as_mut() else {
            // Polled after completion: nothing is left to run.
            return Poll::Pending;
        };
        match catch_unwind(AssertUnwindSafe(|| inner.as_mut().poll(cx))) {
            Err(_) => std::process::abort(),
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => {
                drop_or_abort(this.inner.take());
                Poll::Ready(output)
            }
        }
    }
}

impl<F> Drop for CrashOnPanic<F> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            drop_or_abort(inner);
        }
    }
}

#[cfg(test)]
mod tests {
    //! Each crash case runs in a child copy of this test binary: the parent
    //! re-executes itself on the one test, the child runs the scenario, and
    //! the parent reads how the child ended (x.3.2 X0 item 2.5, Ruling D).

    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, ExitStatus};
    use std::sync::Mutex;

    use super::{crash_on_panic, lock};

    /// Names the scenario a child runs.
    const CHILD: &str = "VIA_CRASH_CHILD";
    const SIGABRT: i32 = 6;

    /// Runs `scenario` in a child when this process is that child (then
    /// exits 0 if it returns); otherwise starts the child on test `name`
    /// and returns how it ended.
    fn in_child(name: &str, scenario: impl FnOnce()) -> ExitStatus {
        if std::env::var(CHILD).as_deref() == Ok(name) {
            scenario();
            std::process::exit(0);
        }
        let test = format!("codex::crash::tests::{name}");
        Command::new(std::env::current_exe().unwrap())
            .args([test.as_str(), "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, name)
            .status()
            .unwrap()
    }

    fn assert_aborted(status: ExitStatus) {
        assert_eq!(status.signal(), Some(SIGABRT), "child ended {status:?}");
    }

    /// A value whose destruction panics.
    struct PanicsOnDrop;

    impl Drop for PanicsOnDrop {
        fn drop(&mut self) {
            panic!("destroyed");
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// A panic while the wrapped future is polled aborts the process.
    #[test]
    fn crash_on_panic_aborts_on_poll() {
        assert_aborted(in_child("crash_on_panic_aborts_on_poll", || {
            block_on(crash_on_panic(async { panic!("polled") }));
        }));
    }

    /// A panic in the future's destruction at completion aborts too.
    #[test]
    fn crash_on_panic_aborts_on_destruction() {
        assert_aborted(in_child("crash_on_panic_aborts_on_destruction", || {
            let held = PanicsOnDrop;
            block_on(crash_on_panic(async move {
                let _held = &held;
            }));
        }));
    }

    /// A pending wrapper dropped with a panicking value aborts.
    #[test]
    fn crash_on_panic_aborts_on_pending_drop() {
        assert_aborted(in_child("crash_on_panic_aborts_on_pending_drop", || {
            let held = PanicsOnDrop;
            let pending = crash_on_panic(async move {
                let _held = &held;
                std::future::pending::<()>().await;
            });
            drop(pending);
        }));
    }

    /// A panic while the registry guard is held aborts, so no caller sees
    /// a half-applied mutation.
    #[test]
    fn guard_aborts_on_panic_under_it() {
        assert_aborted(in_child("guard_aborts_on_panic_under_it", || {
            let registry = Mutex::new(0_u32);
            let mut guard = lock(&registry);
            *guard += 1;
            panic!("under the guard");
        }));
    }

    /// A guard taken during an unrelated unwind (a pin dropped by a
    /// panicking task) does not abort for that panic.
    #[test]
    fn guard_during_unrelated_unwind_does_not_abort() {
        struct Releases<'a>(&'a Mutex<u32>);
        impl Drop for Releases<'_> {
            fn drop(&mut self) {
                *lock(self.0) += 1;
            }
        }
        let status = in_child("guard_during_unrelated_unwind_does_not_abort", || {
            let registry = Mutex::new(0_u32);
            let caught = std::panic::catch_unwind(|| {
                let _pin = Releases(&registry);
                panic!("unrelated");
            });
            assert!(caught.is_err());
            assert_eq!(*lock(&registry), 1);
        });
        assert!(status.success());
    }
}
