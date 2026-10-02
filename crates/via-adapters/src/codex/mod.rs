//! `Adapter::Codex` (`codex-app-server`, adapter design §6;
//! docs/specs/vendors/codex.md). The daemon builds it when the harness's
//! binary resolves, with the daemon's instance cache. It plans purely
//! (capabilities, bound gate, effort and vendor-key refusals, inherited
//! configuration); its driver runs no turn until via-5lr.3.2's server
//! driver lands, so a planned turn still ends `harness_unavailable`.

mod launch;
mod normalize;
mod plan;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;

use crate::harness::Harness;
use crate::instance::{BinaryIdentity, InstanceCache};
use crate::plan::{
    Bound, DescribeRequest, Inherit, ModelChoice, Refusal, RefusalKind, RoutePlan, TurnCheck,
    TurnParams, VendorOptions, effective_inherit,
};

/// The harness this adapter serves, a [`crate::HARNESSES`] name.
pub(crate) const HARNESS: &str = "codex";

/// This adapter's version (AD12): changed when stored session state or the
/// server recipe changes.
const ADAPTER_VERSION: &str = "1";

/// The stored adapter versions this one resumes; none before the first.
const COMPATIBLE: &[&str] = &[];

/// The Codex adapter.
pub(crate) struct CodexAdapter {
    /// The resolved vendor binary (design §5.4).
    binary: PathBuf,
    /// The daemon's instance cache (C2 §5 AD7).
    instances: Arc<InstanceCache>,
}

/// One turn's values the route judges purely.
struct PerTurn<'a> {
    effort: Option<&'a str>,
    bound: Option<&'a Bound>,
    max_steps: bool,
    vendor: &'a VendorOptions,
}

impl CodexAdapter {
    pub(crate) fn new(binary: PathBuf, instances: Arc<InstanceCache>) -> Self {
        Self { binary, instances }
    }

    /// The plan of a spawn or `describe` (C2 §2): pure but for one `stat`
    /// of the binary, which names the last version seen for it. With no
    /// bundled catalog the model is taken as given; discovery judges it
    /// at the first turn.
    pub(crate) fn plan(
        &self,
        harness: Harness,
        req: &DescribeRequest,
        requested: Inherit,
    ) -> Result<RoutePlan, Refusal> {
        let route = harness.route();
        let model = req
            .model
            .clone()
            .filter(|model| !model.is_empty())
            .ok_or_else(|| {
                Refusal::new(
                    RefusalKind::UnknownModel,
                    Some(route),
                    format!("route {route} has no default model: name one"),
                )
            })?;
        let capabilities = plan::capabilities();
        let mut refusals = refusals(
            route,
            &PerTurn {
                effort: req.effort.as_deref(),
                bound: req.bound.as_ref(),
                max_steps: false,
                vendor: &req.vendor,
            },
        );
        if let Err(verb) = capabilities.require(&req.require) {
            refusals.push(Refusal::new(
                RefusalKind::MissingCapability { verb },
                Some(route),
                format!(
                    "a required verb is not supported natively on route {route}: {}",
                    verb.as_str()
                ),
            ));
        }
        let effective_bound = req
            .bound
            .clone()
            .filter(|bound| plan::sandbox(bound).is_ok());
        let (inherit, switch_warning) = effective_inherit(&plan::categories(), requested);
        let vendor_version = BinaryIdentity::of(&self.binary)
            .ok()
            .and_then(|identity| self.instances.last_version(&identity));
        let version_status = vendor_version.as_deref().map_or(
            crate::plan::VersionStatus::Untested,
            normalize::version_status,
        );
        Ok(RoutePlan {
            harness: harness.name(),
            model: ModelChoice {
                requested: req.model.clone(),
                resolved: model,
            },
            route,
            adapter_version: ADAPTER_VERSION.to_owned(),
            vendor_version,
            version_status,
            capabilities,
            effective_bound,
            refusals,
            // Core derives `vendor_version_untested` from the status.
            warnings: switch_warning.into_iter().collect(),
            inherit,
            // The server key needs the route's vendor state directory,
            // which the server driver supplies (x.3.2 X2).
            server_key: None,
        })
    }

    /// A resume turn (C2 §2): AD12's version check, then the per-turn
    /// refusals; the bound applies as requested.
    pub(crate) fn check_turn(
        route: &'static str,
        stored_version: &str,
        turn: &TurnParams,
    ) -> Result<TurnCheck, Refusal> {
        if stored_version != ADAPTER_VERSION && !COMPATIBLE.contains(&stored_version) {
            let mut refusal = Refusal::new(
                RefusalKind::HarnessUnavailable,
                Some(route),
                "the session's adapter version is not compatible with this adapter",
            );
            refusal.reason = Some("adapter_version");
            return Err(refusal);
        }
        let per_turn = PerTurn {
            effort: turn.effort.as_deref(),
            bound: turn.bound.as_ref(),
            max_steps: turn.max_steps.is_some(),
            vendor: &turn.vendor,
        };
        match refusals(route, &per_turn).into_iter().next() {
            Some(refusal) => Err(refusal),
            None => Ok(TurnCheck {
                effective_bound: turn.bound.clone(),
            }),
        }
    }
}

/// Every per-turn refusal, in C1 member order: `effort`, `max_steps`,
/// `bound`, `vendor`. `output_schema` is native and never refused.
fn refusals(route: &'static str, turn: &PerTurn<'_>) -> Vec<Refusal> {
    let invalid = |field, message: String| {
        Refusal::new(RefusalKind::InvalidParam { field }, Some(route), message)
    };
    let mut refusals = Vec::new();
    if turn.effort.is_some_and(plan::effort_refused) {
        refusals.push(invalid(
            "effort",
            format!("effort is not a value route {route} accepts"),
        ));
    }
    if turn.max_steps {
        refusals.push(invalid(
            "max_steps",
            format!("max_steps is unsupported on route {route}"),
        ));
    }
    if let Some(Err(refusal)) = turn.bound.map(plan::sandbox) {
        refusals.push(Refusal::new(
            RefusalKind::BoundUnsupported,
            Some(route),
            refusal.message(route),
        ));
    }
    match plan::vendor_refusal(HARNESS, turn.vendor) {
        Some(plan::VendorRefusal::Reserved) => refusals.push(Refusal::new(
            RefusalKind::VendorOptionConflict,
            Some(route),
            format!("a vendor option sets what route {route} reserves"),
        )),
        Some(plan::VendorRefusal::NotAllowed) => refusals.push(invalid(
            "vendor",
            format!("route {route} accepts no other vendor options"),
        )),
        None => {}
    }
    refusals
}
