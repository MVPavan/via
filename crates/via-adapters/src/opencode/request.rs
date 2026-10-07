//! Exactly one current HTTP request outlives caller stops after its first byte (§8).

use std::future::Future;
use std::sync::Arc;

use tokio::sync::oneshot;
use via_routes::opencode::Server;
use via_routes::opencode::turn::SentTracker;

use crate::CancellationToken;

/// Dropping the caller withdraws only an exchange which has not written a byte (§8).
struct Caller {
    cancel: CancellationToken,
    sent: Arc<SentTracker>,
}

impl Drop for Caller {
    fn drop(&mut self) {
        // Stabilize first-byte evidence before the turn starts its native stop path.
        self.sent.withdraw_before_send();
        self.cancel.cancel();
    }
}

/// The generation polls the original exchange and classifies it before releasing ownership.
pub(super) async fn owned<T, R, F>(
    server: &Arc<Server>,
    sent: Arc<SentTracker>,
    exchange: impl Future<Output = T> + Send + 'static,
    finish: impl FnOnce(Option<T>) -> F + Send + 'static,
) -> Option<R>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Future<Output = R> + Send + 'static,
{
    let cancelled = CancellationToken::new();
    let _caller = Caller {
        cancel: cancelled.clone(),
        sent: Arc::clone(&sent),
    };
    let (sender, receiver) = oneshot::channel();
    let request_server = Arc::clone(server);
    if !server.spawn_request(async move {
        tokio::pin!(exchange);
        // Losing waits consume nothing; a sent exchange remains pinned until its own timeout.
        let response = tokio::select! {
            biased;
            () = cancelled.cancelled() => {
                if sent.is_sent() { Some(exchange.await) } else { None }
            },
            () = request_server.wait_draining() => {
                if sent.is_sent() { Some(exchange.await) } else { None }
            },
            response = &mut exchange => Some(response),
        };
        let result = finish(response).await;
        // A closed receiver is the expected caller-stop path; classification is already retained.
        let _published = sender.send(result);
    }) {
        return None;
    }
    // Generation cancellation joins the request job and closes this receiver.
    receiver.await.ok()
}
