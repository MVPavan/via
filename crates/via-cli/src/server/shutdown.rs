//! Final shutdown: joins daemon main's owned work and decides the exit.

use std::{
    io::{self, Write},
    sync::Arc,
    time::Duration,
};

use serde_json::json;
use tokio::{
    task::JoinSet,
    time::{Instant, timeout_at},
};
use via_core::{Deadline, Engine, StopMode};

use super::{Joins, drive_joined, spawn_dispatcher};

/// One absolute budget for all of final shutdown (runtime §6, C1 §3.14).
const FINAL_SHUTDOWN: Duration = Duration::from_secs(10);

/// Part of the final deadline kept for the Store join after clients finish.
const STORE_RESERVE: Duration = Duration::from_secs(2);

/// Joins the daemon's owned work under one absolute deadline and decides the
/// process exit: 0 only for a clean shutdown, otherwise 4 (incomplete).
///
/// Idle clients close at once; a client already serving a request (such as a
/// `wait`) delivers it after the final records commit. Only daemon main takes
/// the incomplete exit. Unjoined tasks are aborted and reported, a blocked
/// Store join is abandoned to process exit, and nothing is claimed from abort,
/// handle drop or OS adoption.
pub(super) async fn final_shutdown(engine: Arc<Engine>, joins: Joins, mode: StopMode) -> i32 {
    let started = Instant::now();
    let deadline = started + FINAL_SHUTDOWN;
    // Force-path reads stop retrying in time for Host cleanup and terminals.
    engine.begin_final_shutdown(deadline);
    let Joins {
        mut clients,
        mut drives,
        mut starts,
        closing,
        failed: mut failed_joins,
    } = joins;
    closing.send_replace(true);
    // A force can land between a receipt and daemon main starting its
    // session's dispatcher: every requested dispatcher is started, so a force
    // stop still settles its turns. Stop and the Store-failed latch are set
    // under `admission`, which every receipt holds through its enqueue and
    // start request, and daemon main gets here only after one of them: a
    // receipt either put its start in the channel or pending set already, or
    // was refused. Starts only drain now; any slot still `Starting` at the
    // deadline makes the shutdown incomplete.
    let mut queued_drives = 0_usize;
    loop {
        while let Ok(session) = starts.try_recv() {
            spawn_dispatcher(&mut drives, &engine, session);
            queued_drives += 1;
        }
        if !engine.starts_pending() || Instant::now() >= deadline {
            break;
        }
        engine.retry_starts();
    }
    // Force-stopped drives return after Route's bounded force cleanup; their
    // terminals commit below.
    let joined = timeout_at(deadline, async {
        while let Some(result) = drives.join_next().await {
            if !drive_joined(result) {
                failed_joins += 1;
            }
        }
    })
    .await;
    let mut pending_joins = 0;
    if joined.is_err() {
        pending_joins = drives.len();
        drives.abort_all();
    }
    let report = timeout_at(deadline, engine.shutdown(Deadline::at(deadline))).await;
    // Every final record is committed: pending reads deliver, then clients close.
    let clients_by = deadline.checked_sub(STORE_RESERVE).unwrap_or(started);
    let (pending, failed) = join_clients(&mut clients, clients_by, deadline).await;
    pending_joins += pending;
    failed_joins += failed;
    // Store Drop blocks on its writer and raw threads: keep it off Tokio workers
    // and bounded; a stalled join is left to process exit, never waited out.
    let store = match Arc::try_unwrap(engine) {
        Ok(engine) => drop_blocking(engine, deadline).await,
        Err(_) => "not_released",
    };
    let host = report.as_ref().ok();
    let clean = pending_joins == 0
        && failed_joins == 0
        && store == "joined"
        && host.is_some_and(via_core::EngineShutdown::is_clean);
    let summary = json!({"daemon_shutdown":{
        "mode":mode.as_str(),
        "queued_drives":queued_drives,
        "elapsed_ms":u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "pending_joins":pending_joins + host.map_or(0, |host| host.pending_tasks),
        "failed_joins":failed_joins + host.map_or(0, |host| host.failed_tasks),
        "anchors":host.map(|host| host.anchors),
        "uncertain_owners":host.map(|host| host.uncertain_owners),
        "host_failure":host.map_or(Some("final shutdown deadline expired"), |host| host.failure.as_deref()),
        "uncommitted_turns":host.map(|host| host.uncommitted_turns),
        "unresolved_turns":host.map(|host| host.unresolved_turns),
        "store_failed":host.map(|host| host.store_failed),
        "unstarted_dispatchers":host.map(|host| host.unstarted_dispatchers),
        "unclosed_sessions":host.map(|host| host.unclosed_sessions),
        "store":store,
        "disposition":if clean {"clean"} else {"incomplete"},
    }});
    // Best-effort bounded diagnostic; the exit status is the authoritative result.
    let _ = writeln!(io::stderr().lock(), "{summary}");
    if clean { 0 } else { 4 }
}

/// Joins client tasks until `clients_by`, then aborts the rest and awaits
/// their exit only until the final `deadline`; one that has not reached an
/// abort point by then is left to process exit. Returns `(pending, failed)`:
/// `pending` counts clients still unjoined at `deadline`, and `failed` those
/// that panicked (an abort's own cancellation is not a failure).
async fn join_clients(
    clients: &mut JoinSet<anyhow::Result<()>>,
    clients_by: Instant,
    deadline: Instant,
) -> (usize, usize) {
    let mut failed = 0;
    let mut join = async |clients: &mut JoinSet<anyhow::Result<()>>| {
        while let Some(result) = clients.join_next().await {
            // A client's own I/O error is its connection's end, not a failed join.
            if let Err(error) = result
                && !error.is_cancelled()
            {
                tracing::error!(%error, "client task failed");
                failed += 1;
            }
        }
    };
    if timeout_at(clients_by, join(clients)).await.is_err() {
        clients.abort_all();
        let _ = timeout_at(deadline, join(clients)).await;
    }
    (clients.len(), failed)
}

/// Drops `value` on the blocking pool, waiting at most until `deadline`.
async fn drop_blocking<T: Send + 'static>(value: T, deadline: Instant) -> &'static str {
    match timeout_at(deadline, tokio::task::spawn_blocking(move || drop(value))).await {
        Ok(Ok(())) => "joined",
        Ok(Err(_)) => "join_failed",
        Err(_) => "join_timed_out",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W3-F Sol 4: a client task that does not reach an abort point promptly
    /// cannot hold daemon main past the final deadline; it is counted pending
    /// and left to process exit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unabortable_client_join_stops_at_the_final_deadline() {
        let mut clients = JoinSet::new();
        clients.spawn(async {
            // Blocks its worker: abort takes effect only once it returns.
            std::thread::sleep(Duration::from_secs(2));
            Ok(())
        });
        let started = Instant::now();
        let (pending, failed) = join_clients(
            &mut clients,
            started + Duration::from_millis(50),
            started + Duration::from_millis(200),
        )
        .await;
        assert_eq!((pending, failed), (1, 0));
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "client join passed the final deadline: {:?}",
            started.elapsed()
        );
    }

    /// W4-H Sol 3: a client aborted at `clients_by` that then joins before the
    /// final deadline is not pending; only tasks still unjoined count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_client_that_joins_is_not_pending() {
        let mut clients = JoinSet::new();
        clients.spawn(async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        });
        let started = Instant::now();
        let joined = join_clients(
            &mut clients,
            started + Duration::from_millis(50),
            started + Duration::from_millis(500),
        )
        .await;
        assert_eq!(joined, (0, 0), "an aborted, joined client is not pending");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stalled_blocking_drop_is_abandoned_at_the_deadline() {
        struct Stalled(std::sync::mpsc::Receiver<()>);
        impl Drop for Stalled {
            fn drop(&mut self) {
                let _ = self.0.recv_timeout(Duration::from_secs(5));
            }
        }
        let (release, held) = std::sync::mpsc::channel();
        let started = Instant::now();
        let outcome = drop_blocking(Stalled(held), started + Duration::from_millis(100)).await;
        assert_eq!(outcome, "join_timed_out");
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(release);
        assert_eq!(
            drop_blocking((), Instant::now() + Duration::from_secs(1)).await,
            "joined"
        );
    }
}
