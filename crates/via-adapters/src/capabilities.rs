//! The C1 §4.1 capabilities DTO (C2 §5): one shape for `describe`, receipts
//! and C2, declared per route and adapter version.

use serde::{Deserialize, Serialize};

/// One `support` entry: `native`, `partial` with its fixed semantics string,
/// or `unsupported` with a reason.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "support", rename_all = "snake_case", deny_unknown_fields)]
pub enum Support {
    /// Supported as C1 defines it.
    Native,
    /// Supported with the named difference.
    Partial {
        /// A fixed semantics string (C2 §5).
        semantics: String,
    },
    /// Not supported; the verb or value is refused.
    Unsupported {
        /// Why, for the caller.
        reason: String,
    },
}

impl Support {
    /// Whether this support meets a request: `native` always, `partial` only
    /// when the request accepts partial support.
    pub fn meets(&self, partial_ok: bool) -> bool {
        match self {
            Self::Native => true,
            Self::Partial { .. } => partial_ok,
            Self::Unsupported { .. } => false,
        }
    }
}

/// A C1 verb that `require` can name.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verb {
    /// `spawn`.
    Spawn,
    /// `resume`.
    Resume,
    /// `steer`.
    Steer,
    /// `cancel`.
    Cancel,
    /// `close`.
    Close,
}

impl Verb {
    /// The C1 verb name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Resume => "resume",
            Self::Steer => "steer",
            Self::Cancel => "cancel",
            Self::Close => "close",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [
            Self::Spawn,
            Self::Resume,
            Self::Steer,
            Self::Cancel,
            Self::Close,
        ]
        .into_iter()
        .find(|verb| verb.as_str() == name)
    }
}

/// One `require` entry: `verb`, or `verb:partial` that partial support meets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerbReq {
    /// The required verb.
    pub verb: Verb,
    /// Written `verb:partial`.
    pub partial: bool,
}

impl VerbReq {
    /// Parses one C1 `require` entry; `None` when it names no verb.
    pub fn parse(entry: &str) -> Option<Self> {
        let (name, partial) = match entry.strip_suffix(":partial") {
            Some(name) => (name, true),
            None => (entry, false),
        };
        Verb::parse(name).map(|verb| Self { verb, partial })
    }
}

/// `capabilities.verbs`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Verbs {
    /// `spawn`.
    pub spawn: Support,
    /// `resume`.
    pub resume: Support,
    /// `steer`.
    pub steer: Support,
    /// `cancel`.
    pub cancel: Support,
    /// `close`.
    pub close: Support,
}

impl Verbs {
    /// The support declared for `verb`.
    pub fn get(&self, verb: Verb) -> &Support {
        match verb {
            Verb::Spawn => &self.spawn,
            Verb::Resume => &self.resume,
            Verb::Steer => &self.steer,
            Verb::Cancel => &self.cancel,
            Verb::Close => &self.close,
        }
    }
}

/// `capabilities.params`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParamSupport {
    /// `instructions`.
    pub instructions: Support,
    /// `output_schema`.
    pub output_schema: Support,
    /// `effort`.
    pub effort: Support,
    /// `max_steps`.
    pub max_steps: Support,
}

/// `capabilities.usage`: the declared scope of each usage field.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSupport {
    /// Token scope, such as `turn` or `vendor_interval`.
    pub tokens: String,
    /// Cost scope, such as `reported_cumulative` or `unavailable`.
    pub cost: String,
}

/// A C1 §4 bound mode.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundMode {
    /// `read_only`.
    ReadOnly,
    /// `workspace_write`.
    WorkspaceWrite,
    /// `full`.
    Full,
}

/// The C1 §4.1 capabilities DTO.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// Verb support.
    pub verbs: Verbs,
    /// Parameter support.
    pub params: ParamSupport,
    /// Bound modes the route enforces.
    pub bounds: Vec<BoundMode>,
    /// Whether the route enforces `network`.
    pub network_control: bool,
    /// Recovery after a daemon restart.
    pub recover: Support,
    /// Usage scopes.
    pub usage: UsageSupport,
}

impl Capabilities {
    /// C1 §4.1 `require`: the first verb its support does not meet.
    pub fn require(&self, require: &[VerbReq]) -> Result<(), Verb> {
        match require
            .iter()
            .find(|req| !self.verbs.get(req.verb).meets(req.partial))
        {
            Some(unmet) => Err(unmet.verb),
            None => Ok(()),
        }
    }
}
