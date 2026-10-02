//! The fake's scenario profile (decision H2): what a conformance profile
//! declares in the scenario's `profile` member. Every member is optional;
//! the defaults are the fake route S1 reports.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::capabilities::{Capabilities, ParamSupport, Support, UsageSupport, Verbs};
use crate::driver::SteerError;
use crate::observation::SteerDelivery;
use crate::plan::{Bound, CatalogModel, Category, CategoryDecl};

/// A fake capability profile.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FakeProfile {
    /// AD12 adapter version.
    #[serde(default = "default_version")]
    pub(crate) adapter_version: String,
    /// Stored adapter versions this version can resume (AD12).
    #[serde(default)]
    pub(crate) compatible: Vec<String>,
    /// The declared C1 §4.1 capabilities.
    #[serde(default = "default_capabilities")]
    pub(crate) capabilities: Capabilities,
    /// The bundled catalog.
    #[serde(default = "default_models")]
    pub(crate) models: Vec<CatalogModel>,
    /// The compiled effort table (AD18).
    #[serde(default)]
    pub(crate) efforts: Vec<String>,
    /// AD13 declarations; an absent category is switchable and verified.
    #[serde(default)]
    pub(crate) categories: BTreeMap<Category, CategoryDecl>,
    /// The persistent-connection test profile (decision H1): the driver
    /// keeps its connection slot between turns and pins it.
    #[serde(default)]
    pub(crate) persistent: bool,
    /// The instance's version handshake (AD7), when the profile has one.
    #[serde(default)]
    pub(crate) handshake: Option<HandshakeDecl>,
    /// The persistent emulation's idle close, when the scenario has one.
    #[serde(default)]
    pub(crate) idle_close: Option<IdleCloseDecl>,
    /// The bound the route reports enforcing for any supported requested
    /// one, when it normalizes bounds (C2 `RoutePlan.effective_bound`).
    #[serde(default)]
    pub(crate) normalized_bound: Option<Bound>,
    /// How the driver refuses every steer admitted into the running turn,
    /// when the scenario declares one (C2 §2 `SteerError`).
    #[serde(default)]
    pub(crate) steer_refusal: Option<SteerRefusalDecl>,
}

/// A declared steer refusal: what a vendor or the control lane answers.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SteerRefusalDecl {
    /// The control lane is full.
    OverCapacity,
    /// The vendor refused steer in the turn's current phase.
    NotSteerable,
    /// The input was not written whole.
    NotDelivered,
    /// The vendor took the input, but its report was not recorded
    /// (critical r2 #3).
    NotRecorded,
}

impl SteerRefusalDecl {
    /// The driver's error for a steer `delivery` would describe.
    pub(crate) fn error(self, delivery: &SteerDelivery) -> SteerError {
        match self {
            Self::OverCapacity => SteerError::OverCapacity,
            Self::NotSteerable => SteerError::NotSteerable,
            Self::NotDelivered => SteerError::NotDelivered,
            Self::NotRecorded => SteerError::NotRecorded {
                delivery: delivery.clone(),
            },
        }
    }
}

/// The persistent emulation's idle close (decision H1, C2 §4): after turn
/// `after_turn` returned, once the scenario releases gate `gate`, the
/// emulated server closes the idle session with `reason`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IdleCloseDecl {
    /// The turn after which the session goes idle.
    pub(crate) after_turn: u32,
    /// The sync-directory gate whose release closes it.
    pub(crate) gate: String,
    /// The vendor's reason.
    pub(crate) reason: String,
}

/// A fake profile's handshake (AD7): the versions maintainers checked and
/// the features VIA relies on.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandshakeDecl {
    /// Versions reported `tested`; any other is `untested`.
    #[serde(default)]
    pub(crate) checked: Vec<String>,
    /// Features the handshake must report, else the instance is refused.
    #[serde(default)]
    pub(crate) requires: Vec<String>,
}

impl Default for FakeProfile {
    fn default() -> Self {
        Self {
            adapter_version: default_version(),
            compatible: Vec::new(),
            capabilities: default_capabilities(),
            models: default_models(),
            efforts: Vec::new(),
            categories: BTreeMap::new(),
            persistent: false,
            handshake: None,
            idle_close: None,
            normalized_bound: None,
            steer_refusal: None,
        }
    }
}

/// Workspace crates share one version; the default fake has no separate constant.
fn default_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

fn default_models() -> Vec<CatalogModel> {
    vec![CatalogModel {
        model: "fake".to_owned(),
        aliases: Vec::new(),
    }]
}

/// The fake route: one prompt, one turn, no controls, bounds or usage.
fn default_capabilities() -> Capabilities {
    let unsupported = |reason: &str| Support::Unsupported {
        reason: reason.to_owned(),
    };
    Capabilities {
        verbs: Verbs {
            spawn: Support::Native,
            resume: Support::Native,
            steer: unsupported("the fake route has no steer input"),
            cancel: Support::Native,
            close: Support::Native,
        },
        params: ParamSupport {
            instructions: unsupported("the fake route has no instructions input"),
            output_schema: unsupported("the fake route has no schema input"),
            effort: unsupported("the fake route has no effort setting"),
            max_steps: unsupported("the fake route has no step limit"),
        },
        bounds: Vec::new(),
        network_control: false,
        recover: unsupported("fake turns do not survive a daemon restart"),
        usage: UsageSupport {
            // Its samples are exact per turn by construction.
            tokens: "turn".to_owned(),
            cost: "unavailable".to_owned(),
        },
    }
}
