//! The Codex route (`codex-app-server`, adapter design §6;
//! vendors/codex.md §2): its typed JSON-RPC messages and encoders, the
//! shared server's connection (request pairing, thread demultiplexing and
//! the never-ask replies) and the lease registry that launches, shares and
//! retires servers (x.3.2 X0, X3).

mod connection;
mod crash;
mod encode;
mod lane;
mod messages;
mod servers;

pub use connection::{
    Connection, ConnectionEnd, ConnectionFailure, Counts, DECLINE_DEADLINE, FINISH_BY,
    RequestError, Requested,
};
pub use crash::{CrashOnPanic, RegistryGuard, crash_on_panic, lock};
pub use encode::*;
pub use lane::{
    ConnectionLoss, LANE_BYTES, LANE_MESSAGES, Lane, LaneEnd, LaneEvent, LaneItem, LossCause,
};
pub use messages::*;
pub use servers::{
    AcquireCause, LaunchFailure, LiveServer, MODEL_BYTES, MODEL_PAGES, SERVER_HANDSHAKE,
    SERVER_RETIRE, ServerEnd, ServerFacts, ServerKey, ServerPin, Servers,
};
/// The Wire types a shared-route driver handles: its writes' bounds and
/// answers, a turn's link and evidence folder.
pub use via_wire::{CommitOutcome, PendingWrite, TurnFolder, WriteBounds};

#[cfg(test)]
mod tests;
