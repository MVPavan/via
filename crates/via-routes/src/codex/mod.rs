//! The Codex route (`codex-app-server`, adapter design §6;
//! vendors/codex.md §2): its typed JSON-RPC messages and encoders. The
//! connection task, request pairing and thread demultiplexing come with
//! the server runtime (x.3.2 X3).

mod encode;
mod messages;

pub use encode::*;
pub use messages::*;

#[cfg(test)]
mod tests;
