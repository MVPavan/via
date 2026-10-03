//! What a shared connection needs of Wire's control half (x.3.2 X0 items
//! 12, 13): one seam, so the connection's routing, feeding and failure
//! sequence run over Wire's test pipes in unit tests exactly as over a
//! server's stdin.

use std::future::Future;
use std::pin::Pin;

use via_wire::{
    CloseRequest, CommitOutcome, DataHold, Deadline, OutboundMessage, PendingWrite, SessionId,
    TurnNumber, WireCloseReport, WireError, WireSender, WriteBounds, WriteState, WriteTicket,
};

/// A future the seam returns.
pub(crate) type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Wire's control half, as the connection uses it.
pub(crate) trait Stdio: Send + Sync + 'static {
    /// See [`WireSender::write`].
    fn write(&self, message: OutboundMessage, bounds: WriteBounds) -> PendingWrite;
    /// See [`WireSender::withdraw`].
    fn withdraw(&self, ticket: WriteTicket) -> WriteState;
    /// See [`WireSender::hold_data`].
    fn hold_data(&self) -> DataHold;
    /// See [`WireSender::seal`].
    fn seal(&self);
    /// See [`WireSender::close`].
    fn close(&self, request: CloseRequest) -> Boxed<'_, WireCloseReport>;
    /// See [`WireSender::close_input`].
    fn close_input(&self, deadline: Deadline) -> Boxed<'_, Result<(), WireError>>;
    /// See [`WireSender::keep_undecoded`].
    fn keep_undecoded<'a>(&'a self, bytes: &'a [u8], what: &'a str) -> Boxed<'a, ()>;
    /// See [`WireSender::link_turn`].
    fn link_turn<'a>(
        &'a self,
        session: &'a SessionId,
        turn: TurnNumber,
        deadline: Deadline,
    ) -> Boxed<'a, CommitOutcome<()>>;
}

impl Stdio for WireSender {
    fn write(&self, message: OutboundMessage, bounds: WriteBounds) -> PendingWrite {
        WireSender::write(self, message, bounds)
    }

    fn withdraw(&self, ticket: WriteTicket) -> WriteState {
        WireSender::withdraw(self, ticket)
    }

    fn hold_data(&self) -> DataHold {
        WireSender::hold_data(self)
    }

    fn seal(&self) {
        WireSender::seal(self);
    }

    fn close(&self, request: CloseRequest) -> Boxed<'_, WireCloseReport> {
        Box::pin(WireSender::close(self, request))
    }

    fn close_input(&self, deadline: Deadline) -> Boxed<'_, Result<(), WireError>> {
        Box::pin(WireSender::close_input(self, deadline))
    }

    fn keep_undecoded<'a>(&'a self, bytes: &'a [u8], what: &'a str) -> Boxed<'a, ()> {
        Box::pin(WireSender::keep_undecoded(self, bytes, what))
    }

    fn link_turn<'a>(
        &'a self,
        session: &'a SessionId,
        turn: TurnNumber,
        deadline: Deadline,
    ) -> Boxed<'a, CommitOutcome<()>> {
        Box::pin(WireSender::link_turn(self, session, turn, deadline))
    }
}
