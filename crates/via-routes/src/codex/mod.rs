//! The Codex route (`codex-app-server`, adapter design §6;
//! vendors/codex.md §2): its typed JSON-RPC messages and encoders, the
//! shared server's connection (request pairing, thread demultiplexing and
//! the never-ask replies) and the lease registry that launches, shares and
//! retires servers (x.3.2 X0, X3).

mod connection;
mod crash;
mod encode;
mod feeder;
mod lane;
mod messages;
mod servers;
mod stdio;
#[cfg(any(feature = "test-support", all(test, feature = "test-failpoints")))]
pub mod testing;
mod threads;

pub use connection::{
    Connection, ConnectionEnd, ConnectionFailure, Counts, DECLINE_DEADLINE, FINISH_BY, Fenced,
    LOSS_EVIDENCE, LaneLease, Purpose, RequestError, Requested, Reservation, Subscription,
    TurnWrites, WriteCanceller,
};
pub use crash::{CrashOnPanic, RegistryGuard, crash_on_panic, lock};
pub use encode::*;
pub use lane::{
    AbnormalEnd, ConnectionLoss, ENTRY_BYTES, INBOUND, LANE_BYTES, LANE_MESSAGES, LANE_OVERHEAD,
    Lane, LaneCharge, LaneEnd, LaneEvent, LaneItem, LeaseSignal, LossCause, MESSAGE_BYTES, Mark,
    Reply, Routed, START_BYTES, Start,
};
pub use messages::*;
pub use servers::{
    AcquireCause, HandshakeBound, LaunchError, LaunchFailure, MODEL_BYTES, MODEL_PAGES,
    SERVER_FIRST_HANDSHAKE, SERVER_HANDSHAKE, SERVER_RETIRE, ServerEnd, ServerFacts, ServerKey,
    ServerLease, ServerPin, ServerReport, Servers,
};
pub use threads::{CORRELATION_BYTES, CORRELATION_ENTRIES};
/// The Wire types a shared-route driver handles: its writes' bounds and
/// answers, a turn's link and evidence folder, a routed message's raw
/// form, and a server instance's ID.
pub use via_wire::{
    BoundedBytes, CommitOutcome, PendingWrite, ServerId, TurnFolder, VendorMessage, WriteBounds,
};

#[cfg(test)]
#[cfg(feature = "test-failpoints")]
mod connection_tests;
#[cfg(test)]
mod tests;
