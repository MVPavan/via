//! The harness-neutral half of a driver turn (C2 §2, §4, §4.1): the
//! delivery loop that hands a route's decoded messages through the
//! harness's normalizer to the session channel beside the route, the one
//! cutoff, the stop-order merge Route reads, and the abandonment latch.
//! Each harness supplies its route future, its hop of messages and its
//! [`Normalize`].

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::{DriverState, ForceWatch, latch, lock};
use crate::observation::{ObservationItem, ObservationSink, Undelivered};
use crate::runtime::event_stall;
use crate::{Deadline, DriverFailure, DriverHealth, StopCause, StopOrder, StopWatch};

/// A harness's normalizer as the delivery loop drives it (C2 §4): it turns
/// one of its route's messages into observations, and learns whether their
/// delivery reached the session channel. Each harness implements it over
/// its own route messages.
pub(crate) trait Normalize {
    /// The route's decoded message.
    type Message;

    /// One message's observations, in order, arrived `at`.
    fn items(&mut self, message: Self::Message, at: tokio::time::Instant) -> Vec<ObservationItem>;

    /// The delivery of the last message's items ended, `delivered` or not.
    fn emitted(&mut self, delivered: bool);
}

/// S1's cleanup allowance: the wall's one cutoff is this after the wall
/// (C2 §4.1).
pub(crate) const CLEANUP_ALLOWANCE: Duration = Duration::from_secs(3);

/// How the delivery after Route ended went.
pub(crate) enum Rest {
    /// Everything Route handed over reached the session channel.
    Delivered,
    /// A delivery failed: Core stalled or went away.
    Undelivered,
    /// The daemon force ended a delivery that had to wait.
    Forced,
}

/// A pending delivery, polled beside the route (Task 4 design §9).
type Delivery = Pin<Box<dyn Future<Output = Result<(), Undelivered>> + Send>>;

/// Armed while `run_turn` awaits its result: dropped armed, the future was
/// abandoned, which latches its own first cause (C2 §2 health) before
/// Route sees the closed hop.
pub(crate) struct Abandonment<'a>(pub(crate) Option<&'a watch::Sender<DriverHealth>>);

impl Drop for Abandonment<'_> {
    fn drop(&mut self) {
        if let Some(health) = self.0 {
            latch(health, DriverFailure::TurnAbandoned);
        }
    }
}

/// Resolves once the turn is ordered to end: Core's stop order, the
/// daemon force, its wall, or the session's cancellation, which a driver
/// close includes.
pub(crate) async fn ordered(
    (mut stop, mut force, wall): (StopWatch, ForceWatch, Deadline),
    cancel: CancellationToken,
) {
    let stopped = async {
        if stop.wait_for(Option::is_some).await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    let forced = async {
        if force.wait_for(Option::is_some).await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        () = stopped => {}
        () = forced => {}
        () = tokio::time::sleep_until(wall.instant()) => {}
        () = cancel.cancelled() => {}
    }
}

/// The running turn's steer lane and close order end with its logical
/// turn; a later turn's are kept.
pub(crate) fn end_active(state: &Mutex<DriverState>, turn: crate::TurnNumber) {
    let mut state = lock(state);
    if state
        .active
        .as_ref()
        .is_some_and(|active| active.turn == turn)
    {
        state.active = None;
    }
}

/// Polls `route` while delivering what it hands over, then delivers the
/// rest by the wall's cutoff: data deliverable at once still goes under
/// the daemon force.
pub(crate) async fn deliver_beside<N: Normalize, R>(
    route: impl Future<Output = Option<R>>,
    hop_rx: mpsc::Receiver<via_routes::Decoded<N::Message>>,
    normalizer: &mut N,
    sink: &ObservationSink,
    activity: &crate::TurnActivity,
    (mut force, cutoff): (ForceWatch, Deadline),
    health: &watch::Sender<DriverHealth>,
) -> (Option<R>, Rest) {
    tokio::pin!(route);
    let stall = event_stall();
    let mut hop_rx = Some(hop_rx);
    let mut delivery: Option<Delivery> = None;
    // The decode position of the message `delivery` carries (critical r2
    // #2): once its observations are in the session channel, Core's decode
    // fence counts it delivered.
    let mut carrying = 0;
    let mut delivered = true;
    let result = loop {
        tokio::select! {
            biased;
            outcome = poll_delivery(delivery.as_mut()), if delivery.is_some() => {
                delivery = None;
                normalizer.emitted(outcome.is_ok());
                if outcome.is_ok() {
                    activity.delivered_through(carrying);
                }
                if outcome.is_err() {
                    // Latched at once (C2 §2); Route observes the closed hop
                    // as overflow, or as the force's stop under a force.
                    latch(health, DriverFailure::ObservationOverflow);
                    hop_rx = None;
                    delivered = false;
                }
            }
            message = recv(hop_rx.as_mut()), if delivery.is_none() && hop_rx.is_some() => {
                match message {
                    Some(message) => {
                        // Its read instant, not now (critical r1 #3).
                        activity.record(message.at);
                        carrying = message.seq;
                        let items = normalizer.items(message.item, message.at);
                        delivery = Some(Box::pin(send_all(items, sink.clone(), stall)));
                    }
                    None => hop_rx = None,
                }
            }
            result = &mut route => break result,
        }
    };
    if !delivered {
        return (result, Rest::Undelivered);
    }
    let rest = async {
        if let Some(delivery) = delivery {
            let outcome = delivery.await;
            normalizer.emitted(outcome.is_ok());
            if outcome.is_err() {
                return Rest::Undelivered;
            }
            activity.delivered_through(carrying);
        }
        if let Some(receiver) = hop_rx.as_mut() {
            while let Ok(message) = receiver.try_recv() {
                activity.record(message.at);
                let items = normalizer.items(message.item, message.at);
                let outcome = send_all(items, sink.clone(), stall).await;
                normalizer.emitted(outcome.is_ok());
                if outcome.is_err() {
                    return Rest::Undelivered;
                }
                activity.delivered_through(message.seq);
            }
        }
        Rest::Delivered
    };
    // One cutoff (C2 §4.1): no delivery outlives the wall plus 3 s.
    let rest = tokio::select! {
        biased;
        rest = tokio::time::timeout_at(cutoff.instant(), rest) => rest.unwrap_or(Rest::Undelivered),
        () = forced(&mut force) => Rest::Forced,
    };
    (result, rest)
}

/// Sends each item in order.
async fn send_all(
    items: Vec<ObservationItem>,
    sink: ObservationSink,
    stall: Duration,
) -> Result<(), Undelivered> {
    // Test builds: one message's items are stamped, not yet delivered.
    #[cfg(feature = "test-failpoints")]
    let _ = via_routes::failpoint::hit_async("adapter.fake.stamped").await;
    for item in items {
        sink.send(item, stall).await?;
    }
    Ok(())
}

async fn poll_delivery(delivery: Option<&mut Delivery>) -> Result<(), Undelivered> {
    match delivery {
        Some(delivery) => delivery.await,
        None => std::future::pending().await,
    }
}

async fn recv<M>(receiver: Option<&mut mpsc::Receiver<M>>) -> Option<M> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => None,
    }
}

/// Resolves once `force` is set; never when its sender is gone unset.
async fn forced(force: &mut watch::Receiver<Option<tokio::time::Instant>>) {
    if force.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Keeps `merged` at the earliest of Core's stop order, the driver's close
/// order and, once the session is cancelled, an immediate close. Never
/// returns.
pub(crate) async fn merge_stops(
    mut core: StopWatch,
    mut close: watch::Receiver<Option<StopOrder>>,
    cancel: &CancellationToken,
    merged: &watch::Sender<Option<StopOrder>>,
) -> Infallible {
    let (mut core_open, mut close_open, mut cancelled) = (true, true, false);
    loop {
        let mut order = earliest(
            core.borrow_and_update().clone(),
            close.borrow_and_update().clone(),
        );
        if cancelled {
            let now = tokio::time::Instant::now();
            order = earliest(
                order,
                Some(StopOrder {
                    cause: StopCause::Close,
                    // Route acts only on the times; Core never sees this order.
                    requested_at: String::new(),
                    // Feeds only this driver's merged watch: no provenance
                    // reader sees it (x.3.2 X4 D4.2).
                    attached: now,
                    force_at: Deadline::at(now),
                    close_by: Deadline::at(now + CLEANUP_ALLOWANCE),
                }),
            );
        }
        merged.send_if_modified(|current| {
            if same_order(current.as_ref(), order.as_ref()) {
                false
            } else {
                *current = order;
                true
            }
        });
        tokio::select! {
            changed = core.changed(), if core_open => core_open = changed.is_ok(),
            changed = close.changed(), if close_open => close_open = changed.is_ok(),
            () = cancel.cancelled(), if !cancelled => cancelled = true,
            else => std::future::pending::<()>().await,
        }
    }
}

/// The order whose `force_at` comes first.
pub(crate) fn earliest(first: Option<StopOrder>, second: Option<StopOrder>) -> Option<StopOrder> {
    match (first, second) {
        (Some(first), Some(second)) => {
            Some(if second.force_at.instant() < first.force_at.instant() {
                second
            } else {
                first
            })
        }
        (first, None) => first,
        (None, second) => second,
    }
}

/// Whether two orders act the same: cause and times.
fn same_order(first: Option<&StopOrder>, second: Option<&StopOrder>) -> bool {
    match (first, second) {
        (Some(first), Some(second)) => {
            first.cause == second.cause
                && first.force_at.instant() == second.force_at.instant()
                && first.close_by.instant() == second.close_by.instant()
        }
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}
