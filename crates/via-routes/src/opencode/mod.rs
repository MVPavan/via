//! The `OpenCode` route (`vendors/opencode.md`): one VIA-owned
//! `opencode serve --stdio` per launch key, reached over loopback HTTP and
//! one SSE event stream (runtime §4: Wire owns the framing), its
//! generations serialized on the one data root (§3.2).

mod handshake;
mod launch;
mod server;
mod servers;

pub use handshake::{CatalogModel, Refusal, URL_LINE_BYTES};
pub use server::{GenerationEnd, LOSS_EXIT, SILENCE, Server};
pub use servers::{
    HANDSHAKE, Launch, LaunchError, LaunchFailure, SERVER_RETIRE, ServerEnd, ServerFacts,
    ServerKey, ServerLease, ServerPin, ServerReport, Servers,
};
