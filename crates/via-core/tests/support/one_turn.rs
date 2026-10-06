//! One fake turn through `SessionDriver::run_turn` (C2 §2) over the real
//! Route, Wire and Host, for the characterization tests that drove the
//! legacy runtime's `execute`: the deployment's adapter set, one session
//! and its turn context.

use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, watch};
use via_adapters::{
    AdapterConfig, AdapterSet, Admitted, BootstrapEnv, CancellationToken, Deadline, Inherit,
    InheritPlan, Prepared, RuntimeConfig, SessionCx, SessionDriver, SessionId, SessionRef,
    SessionSpec, StopOrder, TaskTracker, TurnActivity, TurnCx, TurnNumber, TurnSpec, VendorOptions,
    observation_channel,
};
use via_store::Store;

/// C1 P7's tool-grace window.
const TOOL_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// A fake deployment over `store`, with Host's anchor `anchor` and the
/// fixture this process's environment names.
pub(crate) struct OneTurn {
    pub(crate) set: AdapterSet,
    /// Owns every task the sessions' drivers spawn, such as a turn's route work.
    pub(crate) tracker: TaskTracker,
    cancel: CancellationToken,
}

impl OneTurn {
    pub(crate) fn new(store: &Store, root: &Path, anchor: PathBuf) -> Self {
        let set = AdapterSet::new(
            AdapterConfig::load(BootstrapEnv::capture(), None).unwrap(),
            RuntimeConfig {
                anchor_binary: anchor,
                anchor_dir: root.join("runtime"),
                vendor_state_dir: root.join("vendor"),
            },
            store.runtime_resources(),
        )
        .unwrap();
        Self {
            set,
            tracker: TaskTracker::new(),
            cancel: CancellationToken::new(),
        }
    }

    /// Session `session` of harness `fake`, with its observation channel.
    pub(crate) fn session(
        &self,
        session: &str,
        cwd: &Path,
    ) -> (SessionDriver, mpsc::Receiver<Admitted>) {
        let (observations, receiver) = observation_channel();
        let spec = SessionSpec {
            session_id: SessionId::try_from(session).unwrap(),
            model: "fake".to_owned(),
            instructions: None,
            initial_bound: None,
            cwd: cwd.to_path_buf(),
            vendor: VendorOptions::new(),
            inherit: InheritPlan {
                requested: Inherit::OD2_DEFAULT,
                effective: Inherit::OD2_DEFAULT,
            },
            confirmed_vendor_session_id: None,
            allow_untested: false,
            vendor_args: via_adapters::VendorArgs::default(),
        };
        let reference = SessionRef {
            harness: "fake".to_owned(),
            route: "fake".to_owned(),
            adapter_version: String::new(),
        };
        let cx = SessionCx {
            observations,
            tracker: self.tracker.clone(),
            cancel: self.cancel.clone(),
        };
        (self.set.open_session(&reference, spec, cx), receiver)
    }
}

/// Turn 1's context: a harness-process slot when `prepared` needs one, the wall
/// `wall`, the daemon force `force` and the stop order `stop`.
pub(crate) fn turn_cx(
    prepared: Prepared,
    wall: Deadline,
    force: watch::Receiver<Option<tokio::time::Instant>>,
    stop: watch::Receiver<Option<StopOrder>>,
) -> TurnCx {
    let capacity = matches!(prepared, Prepared::NeedsConnection)
        .then(|| Box::new(()) as via_adapters::CapacityToken);
    TurnCx {
        turn: TurnNumber::try_from(1).unwrap(),
        prepared,
        capacity,
        activity: TurnActivity::new(tokio::time::Instant::now()),
        wall,
        tool_grace: TOOL_GRACE,
        stop,
        force,
        stop_ack: via_adapters::StopAck::new(),
    }
}

/// The turn's spec: prompt `hello`.
pub(crate) fn hello() -> TurnSpec {
    TurnSpec {
        prompt: "hello".to_owned(),
        ..TurnSpec::default()
    }
}
