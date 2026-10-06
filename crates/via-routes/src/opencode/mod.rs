//! The `OpenCode` route (`vendors/opencode.md`): one VIA-owned
//! `opencode serve --stdio` per launch key, reached over loopback HTTP and
//! one SSE event stream (runtime §4: Wire owns the framing), its
//! generations serialized on the one data root (§3.2).

pub mod events;
mod handshake;
mod launch;
pub mod router;
mod server;
mod servers;
pub mod session;
pub mod state;
pub mod turn;

pub use handshake::{CatalogModel, Refusal, URL_LINE_BYTES};
pub use launch::ACQUISITION;
pub use server::{GenerationEnd, LOSS_EXIT, SILENCE, Server};
pub use servers::{
    HANDSHAKE, Launch, LaunchError, LaunchFailure, Prepare, SERVER_RETIRE, ServerEnd, ServerFacts,
    ServerKey, ServerLease, ServerPin, ServerReport, Servers,
};
