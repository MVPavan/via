//! The Claude Code route (`claude-cli`, adapter design §6): its typed
//! stream-json messages and the encoders of the lines VIA writes (vendor
//! packet `docs/specs/vendors/claude-code.md` §§5–7), and the private
//! per-turn process lifecycle that carries them ([`ClaudeRoute`]).

mod messages;
mod route;

pub use messages::{
    AssistantMessage, Block, ControlRequest, ControlResponse, DecodeError, Init, Message,
    PermissionDenial, PermissionDenied, ResultMessage, ResultUsage, UserContent, UserMessage,
    control_decline, decode, interrupt_request, user_start,
};
pub use route::{ClaudeItem, ClaudeRoute, ClaudeRouteResult, ClaudeStart, ClaudeTurn};
