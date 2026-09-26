use std::{fmt, fs::File, io::Read};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{SessionId, TurnNumber};

/// Strict C1 parameters for creating Task 1's fake session and first turn.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnParams {
    /// Selected harness, currently `fake` in S1.
    pub harness: String,
    /// Explicit model name.
    pub model: String,
    /// User prompt delivered once after submission intent commits.
    pub prompt: String,
    /// Caller-owned 256-bit bearer handle.
    pub handle: String,
}

/// Strict C1 steer parameters; fake must refuse after authentication.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteerParams {
    /// Session to mutate.
    pub session: SessionId,
    /// Text that will not be sent on unsupported routes.
    pub text: String,
    /// Caller-owned bearer handle.
    pub handle: String,
}

/// Strict C1 read-address parameter set.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    /// A canonical session or turn address.
    pub address: String,
}

/// Named C1 request error without sensitive input in its message.
#[derive(Clone, Copy, Debug)]
pub struct ApiError {
    /// JSON-RPC error code.
    pub code: i32,
    /// Stable C1 kind.
    pub kind: &'static str,
    /// Bounded public explanation.
    pub message: &'static str,
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// Invalid request fields or identifier format.
    pub const INVALID_PARAMS: Self = Self {
        code: -32602,
        kind: "invalid_params",
        message: "invalid parameters",
    };
    /// A caller handle did not authorize a mutation.
    pub const INVALID_HANDLE: Self = Self {
        code: -32002,
        kind: "invalid_handle",
        message: "invalid session handle",
    };
    /// The selected route has no such control capability.
    pub const UNSUPPORTED_VERB: Self = Self {
        code: -32006,
        kind: "unsupported_verb",
        message: "verb is unsupported on this route",
    };
    /// The fake route is not configured or selected.
    pub const HARNESS_UNAVAILABLE: Self = Self {
        code: -32009,
        kind: "harness_unavailable",
        message: "harness is unavailable",
    };
    /// The Store cannot establish or read the required durable state.
    pub const STORE: Self = Self {
        code: -32018,
        kind: "store_error",
        message: "durable storage failed",
    };
    /// The turn has not yet ended.
    pub const TURN_NOT_FINISHED: Self = Self {
        code: -32015,
        kind: "turn_not_finished",
        message: "turn has not finished",
    };
    /// A wait deadline elapsed while the turn remains active.
    pub const WAIT_TIMEOUT: Self = Self {
        code: -32016,
        kind: "wait_timeout",
        message: "wait timed out",
    };
    /// The requested session is absent.
    pub const SESSION_NOT_FOUND: Self = Self {
        code: -32003,
        kind: "session_not_found",
        message: "session does not exist",
    };
}

/// Hashes only a canonical handle; the plaintext must never cross into Store.
pub fn hash_handle(handle: &str) -> Result<[u8; 32], ApiError> {
    let body = handle.strip_prefix("h_").ok_or(ApiError::INVALID_HANDLE)?;
    if body.len() != 43
        || !body
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(ApiError::INVALID_HANDLE);
    }
    let last = body.as_bytes()[42];
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let index = alphabet
        .iter()
        .position(|byte| *byte == last)
        .ok_or(ApiError::INVALID_HANDLE)?;
    if index & 0b11 != 0 {
        return Err(ApiError::INVALID_HANDLE);
    }
    Ok(Sha256::digest(handle.as_bytes()).into())
}

/// Parses a session or one-based turn address without inventing a latest turn.
pub fn parse_address(address: &str) -> Result<(SessionId, TurnNumber), ApiError> {
    let (session, turn) = match address.split_once('/') {
        Some((session, turn)) => {
            let turn = turn.parse::<u32>().map_err(|_| ApiError::INVALID_PARAMS)?;
            (
                session,
                TurnNumber::try_from(turn).map_err(|_| ApiError::INVALID_PARAMS)?,
            )
        }
        None => (
            address,
            TurnNumber::try_from(1).map_err(|_| ApiError::INVALID_PARAMS)?,
        ),
    };
    Ok((
        SessionId::try_from(session).map_err(|_| ApiError::INVALID_PARAMS)?,
        turn,
    ))
}

pub(crate) fn new_session_id() -> Result<SessionId, ApiError> {
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut random = [0; 8];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .map_err(|_| ApiError::STORE)?;
    let mut bits = u64::from_be_bytes(random) & ((1_u64 << 60) - 1);
    let mut digits = [b'0'; 12];
    for digit in digits.iter_mut().rev() {
        *digit = ALPHABET[(bits & 31) as usize];
        bits >>= 5;
    }
    let suffix = std::str::from_utf8(&digits).map_err(|_| ApiError::STORE)?;
    SessionId::try_from(format!("s_{suffix}").as_str()).map_err(|_| ApiError::STORE)
}
