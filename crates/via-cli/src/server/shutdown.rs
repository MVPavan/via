//! Final shutdown: joins daemon main's owned work and decides the exit.

use std::{path::PathBuf, sync::Arc, time::Duration};

use serde_json::json;
use tokio::{
    net::UnixListener,
    sync::{mpsc, watch},
    task::JoinSet,
    time::{Instant, timeout_at},
};
use via_core::{ApiError, Deadline, Engine, SessionId, StopMode};

use super::serving::admit;
use super::{Client, Joins, drive_joined, spawn_dispatcher};

/// One absolute budget for all of final shutdown (runtime §6, C1 §3.14).
const FINAL_SHUTDOWN: Duration = Duration::from_secs(10);

/// Part of the final deadline kept for the Store join after clients finish.
const STORE_RESERVE: Duration = Duration::from_secs(2);

/// Final shutdown's diagnostic window (design §7.4 [O1.D5]): after a latch
/// that preceded final shutdown, daemon main keeps the listener and serves
/// new connections and requests concurrently with the pipeline, never
/// reordering it, until `min(failed_at + 5 s, deadline − 2 s)`. Mutations
/// get `store_error`, `daemon/stop` `{"stopping": true}`, and reads are
/// served. When it ends daemon main sends `closing`, drops the listener and
/// unlinks the socket.
pub(super) struct Window {
    pub(super) listener: UnixListener,
    pub(super) client: Client,
    pub(super) socket: PathBuf,
}

/// How long the window serves after the latch (design §7.4).
const WINDOW: Duration = Duration::from_secs(5);

/// What pipeline steps 1 to 5 (design §6.8) report to the disposition.
struct Pipeline {
    report: Result<via_core::EngineShutdown, tokio::time::error::Elapsed>,
    queued_drives: usize,
    pending_joins: usize,
    failed_joins: usize,
}

/// Joins the daemon's owned work under one absolute deadline and decides the
/// process exit: 0 only for a clean shutdown, otherwise 4 (incomplete).
///
/// The deadline is `start + 10 s`, or `failed_at + 10 s` when a latch
/// preceded final shutdown (design §7.4 [r3.17]); a failure during final
/// shutdown never extends it. Idle clients close at once, or when the
/// latch's diagnostic window ends; a client already serving a request (such
/// as a `wait`) delivers it after the final records commit. Only daemon main
/// takes the incomplete exit. Unjoined tasks are aborted and reported, a
/// blocked Store join is abandoned to process exit, and nothing is claimed
/// from abort, handle drop or OS adoption.
pub(super) async fn final_shutdown(
    engine: Arc<Engine>,
    joins: Joins,
    mode: StopMode,
    window: Option<Window>,
) -> i32 {
    let started = Instant::now();
    let failed_at = engine.failed_at();
    let deadline = failed_at.map_or(started + FINAL_SHUTDOWN, |failed_at| {
        (started + FINAL_SHUTDOWN).min(failed_at + FINAL_SHUTDOWN)
    });
    // Force-path reads stop retrying in time for Host cleanup and terminals.
    engine.begin_final_shutdown(deadline);
    let Joins {
        mut clients,
        mut drives,
        mut reprobe,
        mut starts,
        closing,
        failed,
    } = joins;
    let window_ends = failed_at.map_or(started, |failed_at| {
        (failed_at + WINDOW).min(deadline.checked_sub(STORE_RESERVE).unwrap_or(started))
    });
    if window.is_none() {
        // No diagnostic window: idle clients close at once.
        closing.send_replace(true);
    }
    // Test builds: idle expiry reached final shutdown with the listener gone.
    #[cfg(feature = "test-failpoints")]
    if mode == StopMode::Idle {
        let _ = via_core::failpoint::hit_async("daemon.shutdown.idle_final").await;
    }
    let ((), pipeline) = tokio::join!(
        serve_window(window, window_ends, &mut clients, &closing),
        pipeline(&engine, (&mut reprobe, &mut starts, &mut drives), deadline),
    );
    let Pipeline {
        report,
        queued_drives,
        mut pending_joins,
        failed_joins,
    } = pipeline;
    let mut failed_joins = failed + failed_joins;
    // Every final record is committed: pending reads deliver, then clients close.
    let clients_by = deadline.checked_sub(STORE_RESERVE).unwrap_or(started);
    let (pending, failed) = join_clients(&mut clients, clients_by, deadline).await;
    pending_joins += pending;
    failed_joins += failed;
    // Store Drop blocks on its writer and raw threads: keep it off Tokio workers
    // and bounded; a stalled join is left to process exit, never waited out.
    let blob_tasks = engine.blob_tasks();
    let store = match Arc::try_unwrap(engine) {
        Ok(engine) => drop_blocking(engine, deadline).await,
        Err(_) => "not_released",
    };
    // Blob steps still running after the Store's bounded drain are pending
    // work (coding-style §5): never a clean exit.
    let blob_tasks = blob_tasks.outstanding();
    pending_joins += blob_tasks;
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
        "unjoined_dispatchers":host.map(|host| host.unjoined_dispatchers),
        "failure_batches":host.map(|host| json!({
            "committed":host.failure_batches.committed,
            "skipped":host.failure_batches.skipped,
        })),
        "store":store,
        "blob_tasks":blob_tasks,
        "disposition":if clean {"clean"} else {"incomplete"},
    }});
    // Best-effort bounded diagnostic; the exit status is the authoritative
    // result. One `via.log` line (Task 4 design §7.6).
    super::log::line(format!("{summary}\n").as_bytes());
    if clean { 0 } else { 4 }
}

/// Serves the diagnostic window until `ends` (design §7.4), then sends
/// `closing`, drops the listener and unlinks the socket. Without a window
/// it does nothing.
async fn serve_window(
    window: Option<Window>,
    ends: Instant,
    clients: &mut JoinSet<anyhow::Result<()>>,
    closing: &watch::Sender<bool>,
) {
    if let Some(Window {
        listener,
        client,
        socket,
    }) = window
    {
        loop {
            tokio::select! {
                accepted = listener.accept() => admit(clients, accepted, &client),
                () = tokio::time::sleep_until(ends) => break,
            }
        }
        closing.send_replace(true);
        drop(client);
        drop(listener);
        // Best effort: a stale socket refuses connections and the next
        // daemon replaces it.
        let _ = std::fs::remove_file(socket);
    }
}

/// Pipeline steps 1 to 5 (design §6.8), in order: the re-probe join, the
/// start drain, the dispatcher joins, then Core's final shutdown (Host
/// reconciliation and finalization).
async fn pipeline(
    engine: &Arc<Engine>,
    (reprobe, starts, drives): (
        &mut JoinSet<()>,
        &mut mpsc::Receiver<SessionId>,
        &mut JoinSet<Result<(), ApiError>>,
    ),
    deadline: Instant,
) -> Pipeline {
    let mut failed_joins = 0;
    // Pipeline step 1 (design §6.8): the re-probe task returns at entry; a
    // pass in progress finishes under its own bound.
    let mut pending_joins = 0;
    if timeout_at(deadline, join_reprobe(reprobe, &mut failed_joins))
        .await
        .is_err()
    {
        pending_joins += reprobe.len();
        reprobe.abort_all();
    }
    // Step 2. A force can land between a receipt and daemon main starting
    // its session's dispatcher: every requested dispatcher is started, so a
    // force stop still settles its turns. Stop, the Store-failed latch and
    // the final-shutdown fence are set under `admission`, which every
    // receipt and every `Closing` commit holds through its start request,
    // and daemon main gets here only after entry set the fence: each one
    // either put its start in the channel or pending set already, or was
    // refused. Starts only drain now; any slot still `Starting` at the
    // deadline makes the shutdown incomplete.
    let mut queued_drives = 0_usize;
    loop {
        while let Ok(session) = starts.try_recv() {
            spawn_dispatcher(drives, engine, session);
            queued_drives += 1;
        }
        if !engine.starts_pending() || Instant::now() >= deadline {
            break;
        }
        engine.retry_starts();
    }
    // Step 3: force-stopped drives return after Route's bounded force
    // cleanup and hand their turns to Host reconciliation (step 4), which
    // keeps its time; their terminals commit there.
    let (pending, failed) = join_dispatchers(
        drives,
        Engine::dispatchers_by(deadline),
        Engine::aborted_by(deadline),
    )
    .await;
    pending_joins += pending;
    failed_joins += failed;
    let report = timeout_at(deadline, engine.shutdown(Deadline::at(deadline))).await;
    Pipeline {
        report,
        queued_drives,
        pending_joins,
        failed_joins,
    }
}

/// Pipeline step 3 (design §6.8): joins the dispatchers until `abort_at`,
/// then aborts the rest and joins them until `aborted_by`, where Host
/// reconciliation begins. An abort takes effect only when the task is next
/// polled, so a dispatcher still running a synchronous section keeps its
/// turn until it is joined; one still unjoined at `aborted_by` is pending,
/// and final shutdown settles none of its session's turns. Returns
/// `(pending, failed)`; an abort's own cancellation is not a failure.
async fn join_dispatchers(
    drives: &mut JoinSet<Result<(), ApiError>>,
    abort_at: Instant,
    aborted_by: Instant,
) -> (usize, usize) {
    let mut failed = 0;
    let mut join = async |drives: &mut JoinSet<Result<(), ApiError>>| {
        while let Some(result) = drives.join_next().await {
            let cancelled = matches!(&result, Err(error) if error.is_cancelled());
            if !cancelled && !drive_joined(result) {
                failed += 1;
            }
        }
    };
    if timeout_at(abort_at, join(drives)).await.is_err() {
        drives.abort_all();
        let _ = timeout_at(aborted_by, join(drives)).await;
    }
    (drives.len(), failed)
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

/// Joins the re-probe task; a panic counts as a failed join.
async fn join_reprobe(reprobe: &mut JoinSet<()>, failed: &mut usize) {
    while let Some(result) = reprobe.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "re-probe task failed");
            *failed += 1;
        }
    }
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

    /// T3-S3 round 1, decision 2 (design §6.8 step 3): a dispatcher that
    /// does not return by `abort_at` is aborted and then joined, so Host
    /// reconciliation never starts while it runs. One that reaches an abort
    /// point in time is joined; one still running at `aborted_by` is pending.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_dispatchers_are_joined_before_reconciliation() {
        let running = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut drives = JoinSet::new();
        let flag = Arc::clone(&running);
        drives.spawn(async move {
            flag.store(true, std::sync::atomic::Ordering::Release);
            // Blocks its worker: the abort takes effect once this returns.
            std::thread::sleep(Duration::from_millis(400));
            tokio::task::yield_now().await;
            flag.store(false, std::sync::atomic::Ordering::Release);
            Ok(())
        });
        while !running.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        let now = Instant::now();
        let joined = join_dispatchers(
            &mut drives,
            now + Duration::from_millis(50),
            now + Duration::from_secs(2),
        )
        .await;
        assert_eq!(joined, (0, 0), "the aborted dispatcher was joined");
        assert!(drives.is_empty());

        let mut stuck = JoinSet::new();
        stuck.spawn(async {
            std::thread::sleep(Duration::from_millis(1_500));
            Ok(())
        });
        tokio::task::yield_now().await;
        let now = Instant::now();
        let joined = join_dispatchers(
            &mut stuck,
            now + Duration::from_millis(50),
            now + Duration::from_millis(300),
        )
        .await;
        assert_eq!(joined, (1, 0), "still running at the bound: pending");
    }

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
