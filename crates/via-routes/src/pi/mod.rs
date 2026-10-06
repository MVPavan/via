//! The Pi route (`pi-rpc`; vendor packet `docs/specs/vendors/pi.md`): its
//! typed RPC records and the lines VIA writes (§§2.1, 5–7), and the
//! private per-turn process lifecycle that carries them ([`PiRoute`]).

mod messages;
mod route;

pub use messages::{
    AssistantEnd, DecodeError, MessageEnd, Record, Response, Role, SHORT_MAX, Section, StateData,
    SystemPatch, UiRequest, Usage, decode,
};
pub use route::{
    AbortFacts, HandshakeFacts, PiExpect, PiItem, PiRoute, PiRouteResult, PiStart, PiTurn,
    is_marker,
};
