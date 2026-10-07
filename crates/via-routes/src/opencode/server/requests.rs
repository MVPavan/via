//! Generation-owned current HTTP requests retain their original §8 response timeout.

use std::future::{Future, poll_fn};
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};
use std::task::Poll;

use tokio::task::JoinError;

use super::Server;

impl Server {
    /// Owns one current request through its §8 response classification, never a retry.
    pub fn spawn_request(
        self: &Arc<Self>,
        request: impl Future<Output = ()> + Send + 'static,
    ) -> bool {
        let mut jobs = self.requests.lock().unwrap_or_else(PoisonError::into_inner);
        if self.retiring.load(Ordering::Acquire)
            || self.failure().is_some()
            || self.ended().is_some()
        {
            return false;
        }
        self.request_jobs.fetch_add(1, Ordering::AcqRel);
        let guard = RequestJob(Arc::clone(self));
        jobs.spawn(async move {
            let _guard = guard;
            request.await;
        });
        self.activity.notify_waiters();
        true
    }

    /// Sent setup keeps its generation alive after the original turn settles (§8).
    pub(crate) fn has_request_jobs(&self) -> bool {
        self.request_jobs.load(Ordering::Acquire) > 0
    }

    /// Serializes idle retirement with admitting a current request (§8).
    pub(crate) fn try_retire(&self) -> bool {
        let _requests = self.requests.lock().unwrap_or_else(PoisonError::into_inner);
        if self.has_request_jobs() || self.failure().is_some() || self.ended().is_some() {
            return false;
        }
        self.http
            .close_general(|| self.retiring.store(true, Ordering::Release));
        self.activity.notify_waiters();
        true
    }

    pub(super) async fn next_request(&self) -> Result<(), JoinError> {
        // Join polling is cancel-safe; task ownership stays in the locked JoinSet.
        poll_fn(|cx| {
            let mut jobs = self.requests.lock().unwrap_or_else(PoisonError::into_inner);
            match jobs.poll_join_next(cx) {
                Poll::Ready(Some(outcome)) => Poll::Ready(outcome),
                Poll::Ready(None) | Poll::Pending => Poll::Pending,
            }
        })
        .await
    }

    pub(super) async fn finish_requests(&self) {
        // The generation ended; cancelled outcomes cannot change its classified end.
        // Dropping its current sockets cannot affect successors.
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .abort_all();
        while poll_fn(|cx| {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .poll_join_next(cx)
        })
        .await
        .is_some()
        {}
    }
}

/// Releases generation ownership even if a request panics or is aborted (§8).
struct RequestJob(Arc<Server>);

impl Drop for RequestJob {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Publish failure before retirement can observe the last job disappearing.
            self.0.fail_protocol();
        }
        self.0.request_jobs.fetch_sub(1, Ordering::AcqRel);
        if let Some(registry) = self.0.registry.upgrade() {
            registry.turns_changed(self.0.id());
        }
        self.0.activity.notify_waiters();
    }
}
