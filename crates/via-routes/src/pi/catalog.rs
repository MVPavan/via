//! Pi's live model catalog for C1 `models` (packet §2 `models`, owner
//! 2026-10-12): one private `pi --mode rpc` process, launched only to answer
//! `get_available_models`, then closed. No prompt is ever written, so the
//! process starts no run; its stdin EOF ends it, and Host closes its group
//! on every path before this returns.

use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{Instant, timeout_at};
use via_wire::{
    CloseMode, CloseRequest, OutboundMessage, SendOutcome, WireMessages, WireParts, WireSender,
    WireSignals, WriteBounds,
};

use super::messages::{Record, command, decode, models_data};
use crate::{Deadline, PrivateProcessSpec, RouteRuntime};

/// The one command's ID.
const REQUEST_ID: &str = "via-models";

/// The one command.
const COMMAND: &str = "get_available_models";

/// The bound on the process's close once the catalog was read or the wait
/// ended: stdin EOF, its exit and Host's group close.
const CLOSE: Duration = Duration::from_secs(5);

/// Why the catalog was not read. The text is VIA's own, never the vendor's.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CatalogError {
    /// Host could not start the process.
    #[error("the process could not be started")]
    Launch,
    /// The command was not written whole.
    #[error("the model list request could not be written")]
    Write,
    /// No reply by the deadline.
    #[error("no model list arrived in time")]
    Deadline,
    /// Stdout ended, or the connection failed, before the reply.
    #[error("the process ended before it listed its models")]
    Ended,
    /// The reply failed, or was not a model list VIA reads.
    #[error("the process answered with a failed or malformed model list")]
    Protocol,
}

/// Reads the catalog of the Pi process `process` starts, by `deadline`,
/// as `(provider, id)` pairs in the vendor's order. The process is closed
/// before this returns: gracefully, within [`CLOSE`], once the list was
/// read; at once on any failure. The daemon force is not watched: a probe
/// still running at shutdown is a live Host control, which Host's shutdown
/// stops.
pub(super) async fn available_models(
    runtime: &RouteRuntime,
    process: PrivateProcessSpec,
    deadline: Deadline,
) -> Result<Vec<(String, String)>, CatalogError> {
    // Senders dropped unset: neither watch ever resolves (Wire's rule).
    let force = watch::channel(None).1;
    let wake = watch::channel(0_u64).1;
    let signals = WireSignals {
        force,
        wake,
        gate: std::sync::Arc::new(|| false),
        inbound: via_wire::InboundBounds::DEFAULT,
        capture: via_wire::Capture::On,
    };
    let connection = runtime
        .wire()
        .open_connection(process, deadline, signals)
        .await
        .map_err(|_| CatalogError::Launch)?;
    let WireParts {
        sender,
        mut messages,
    } = connection.into_parts();
    let listed = list(&sender, &mut messages, deadline).await;
    close(&sender, messages, listed.is_ok()).await;
    listed
}

/// Writes the command and reads until its paired reply, by `deadline`.
/// Every other record is ignored: nothing was prompted.
async fn list(
    sender: &WireSender,
    messages: &mut WireMessages,
    deadline: Deadline,
) -> Result<Vec<(String, String)>, CatalogError> {
    let write = sender.write(
        OutboundMessage::Control(command(REQUEST_ID, COMMAND)),
        WriteBounds::CutAt(deadline),
    );
    match write.await {
        Ok(SendOutcome::Written) => {}
        Ok(_) | Err(_) => return Err(CatalogError::Write),
    }
    loop {
        let message = match timeout_at(deadline.instant(), messages.next_message()).await {
            Err(_elapsed) => return Err(CatalogError::Deadline),
            Ok(Ok(Some(message))) => message,
            Ok(Ok(None) | Err(_)) => return Err(CatalogError::Ended),
        };
        let bytes = message.bytes();
        let line = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        match decode(line) {
            Ok(Record::Response(reply)) if reply.id.as_deref() == Some(REQUEST_ID) => {
                return reply
                    .data
                    .as_ref()
                    .filter(|_| reply.success && reply.command == COMMAND)
                    .and_then(models_data)
                    .ok_or(CatalogError::Protocol);
            }
            Ok(_) => {}
            Err(_) => return Err(CatalogError::Protocol),
        }
    }
}

/// Closes the process on every path: after a read list, stdin EOF and
/// Host's graceful close; otherwise Host's force close. Both run within
/// [`CLOSE`] from now, whatever the probe's own deadline was.
async fn close(sender: &WireSender, messages: WireMessages, listed: bool) {
    let by = Deadline::at(Instant::now() + CLOSE);
    let mode = if listed {
        // A timeout or error here is ignored: Host's close below always
        // runs, and escalates at its deadline.
        let _input = sender.close_input(by).await;
        CloseMode::Graceful
    } else {
        CloseMode::Force
    };
    let request = CloseRequest { mode, deadline: by };
    // The reader drains stdout to EOF while Host closes the group.
    let ((), _report) = tokio::join!(messages.finish(by), sender.close(request));
}
