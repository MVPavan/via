//! The Claude Code route (`claude-cli`, adapter design §6): its typed
//! stream-json messages and the encoders of the lines VIA writes (vendor
//! packet `docs/specs/vendors/claude-code.md` §§5–7). The private per-turn
//! process lifecycle that carries them is via-p98.3.2's C2 chunk.

mod messages;

pub use messages::{
    AssistantMessage, Block, ControlRequest, ControlResponse, DecodeError, Init, Message,
    PermissionDenial, PermissionDenied, ResultMessage, ResultUsage, UserContent, UserMessage,
    control_decline, decode, interrupt_request, user_start,
};
