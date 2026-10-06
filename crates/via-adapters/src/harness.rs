//! The harness table (adapter design §5.2, §6 step 4): the one place that
//! names each first-release harness, its route and its default binary.
//! `scripts/check-harness-literals.py` reads the `name` literals of
//! [`HARNESSES`], so the table keeps its pinned format.

/// One first-release harness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HarnessRow {
    /// The C1 harness name.
    pub name: &'static str,
    /// The route that serves it.
    pub route: &'static str,
    /// The binary looked up on `PATH` unless `harnesses.<name>.binary` is set.
    pub default_binary: &'static str,
}

/// Every first-release vendor harness, in C1 §4 order.
pub const HARNESSES: &[HarnessRow] = &[
    HarnessRow {
        name: "claude",
        route: "claude-cli",
        default_binary: "claude",
    },
    HarnessRow {
        name: "codex",
        route: "codex-app-server",
        default_binary: "codex",
    },
    HarnessRow {
        name: "opencode",
        route: "opencode-serve",
        default_binary: "opencode",
    },
    HarnessRow {
        name: "pi",
        route: "pi-rpc",
        default_binary: "pi",
    },
];

/// The fake test double's harness name, which is also its route (design §5.5).
pub const FAKE: &str = "fake";

/// A harness string parsed against the table; Core never compares it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Harness {
    /// A row of [`HARNESSES`].
    Vendor(&'static HarnessRow),
    /// The fake test double, reachable only with its fixture configured.
    Fake,
}

impl Harness {
    /// The canonical harness named by `name`, or `None` when no row names it.
    pub fn parse(name: &str) -> Option<Self> {
        if name == FAKE {
            return Some(Self::Fake);
        }
        HARNESSES
            .iter()
            .find(|row| row.name == name)
            .map(Self::Vendor)
    }

    /// The canonical harness name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Vendor(row) => row.name,
            Self::Fake => FAKE,
        }
    }

    /// The route that serves this harness.
    pub fn route(self) -> &'static str {
        match self {
            Self::Vendor(row) => row.route,
            Self::Fake => FAKE,
        }
    }
}

/// Every harness name this build knows: the table's, then the fake's.
pub fn harness_names() -> impl Iterator<Item = &'static str> {
    HARNESSES
        .iter()
        .map(|row| row.name)
        .chain(std::iter::once(FAKE))
}
