//! `Adapter::Fake` (adapter design §5.5): the test double, reachable only
//! with its fixture configured. Its plan comes from its scenario profile;
//! it has no handshake and never refuses a version.

mod profile;

pub(crate) use profile::FakeProfile;

use crate::harness::Harness;
use crate::plan::{
    CatalogModel, DescribeRequest, Inherit, ModelChoice, Refusal, RefusalKind, RoutePlan,
    TurnParams, VersionStatus, Warning, effective_inherit,
};

/// The fake adapter's planning half.
pub(crate) struct FakeAdapter {
    profile: FakeProfile,
}

/// The values a turn sets that the route must accept.
struct PerTurn<'a> {
    effort: Option<&'a str>,
    bound: Option<&'a crate::plan::Bound>,
    output_schema: bool,
    max_steps: bool,
    vendor: &'a crate::plan::VendorOptions,
}

impl FakeAdapter {
    pub(crate) fn new(profile: FakeProfile) -> Self {
        Self { profile }
    }

    /// The bundled catalog.
    pub(crate) fn catalog(&self) -> &[CatalogModel] {
        &self.profile.models
    }

    /// The catalogued model `requested` names; with none, the first one.
    pub(crate) fn resolve(&self, requested: Option<&str>) -> Option<String> {
        let catalog = self.catalog();
        match requested {
            None => catalog.first(),
            Some(name) => catalog.iter().find(|entry| entry.matches(name)),
        }
        .map(|entry| entry.model.clone())
    }

    /// AD12: the stored version is this adapter's, or one it declares compatible.
    pub(crate) fn check_version(&self, route: &'static str, stored: &str) -> Result<(), Refusal> {
        let profile = &self.profile;
        if stored == profile.adapter_version || profile.compatible.iter().any(|v| v == stored) {
            return Ok(());
        }
        let mut refusal = Refusal::new(
            RefusalKind::HarnessUnavailable,
            Some(route),
            "the session's adapter version is not compatible with this adapter",
        );
        refusal.reason = Some("adapter_version");
        Err(refusal)
    }

    pub(crate) fn plan(
        &self,
        harness: Harness,
        req: &DescribeRequest,
        model: ModelChoice,
        requested: Inherit,
    ) -> RoutePlan {
        let route = harness.route();
        let capabilities = self.profile.capabilities.clone();
        let mut refusals = self.refusals(
            route,
            &PerTurn {
                effort: req.effort.as_deref(),
                bound: req.bound.as_ref(),
                output_schema: false,
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
        let effective_bound = req.bound.clone().filter(|_| {
            !refusals
                .iter()
                .any(|r| r.kind == RefusalKind::BoundUnsupported)
        });
        let (inherit, switch_warning) = effective_inherit(&self.profile.categories, requested);
        let mut warnings = vec![Warning {
            code: "vendor_version_untested",
            message: "the fake agent reports no version".to_owned(),
            data: None,
        }];
        warnings.extend(switch_warning);
        RoutePlan {
            harness: harness.name(),
            model,
            route,
            adapter_version: self.profile.adapter_version.clone(),
            vendor_version: None,
            version_status: VersionStatus::Untested,
            capabilities,
            effective_bound,
            refusals,
            warnings,
            inherit,
        }
    }

    /// Every per-turn refusal of a resume turn, in C1 member order.
    pub(crate) fn check_turn(&self, route: &'static str, turn: &TurnParams) -> Vec<Refusal> {
        self.refusals(
            route,
            &PerTurn {
                effort: turn.effort.as_deref(),
                bound: turn.bound.as_ref(),
                output_schema: turn.output_schema,
                max_steps: turn.max_steps.is_some(),
                vendor: &turn.vendor,
            },
        )
    }

    fn refusals(&self, route: &'static str, turn: &PerTurn<'_>) -> Vec<Refusal> {
        let capabilities = &self.profile.capabilities;
        let params = &capabilities.params;
        let invalid = |field, message: String| {
            Refusal::new(RefusalKind::InvalidParam { field }, Some(route), message)
        };
        let mut refusals = Vec::new();
        if let Some(effort) = turn.effort {
            // AD18: a value the compiled table does not map is refused.
            if !params.effort.meets(true) {
                refusals.push(invalid(
                    "effort",
                    format!("effort is unsupported on route {route}"),
                ));
            } else if !self.profile.efforts.iter().any(|known| known == effort) {
                refusals.push(invalid(
                    "effort",
                    format!("effort is not a value route {route} accepts"),
                ));
            }
        }
        if turn.output_schema && !params.output_schema.meets(true) {
            refusals.push(invalid(
                "output_schema",
                format!("output_schema is unsupported on route {route}"),
            ));
        }
        if turn.max_steps && !params.max_steps.meets(true) {
            refusals.push(invalid(
                "max_steps",
                format!("max_steps is unsupported on route {route}"),
            ));
        }
        if let Some(bound) = turn.bound
            && (!capabilities.bounds.contains(&bound.mode)
                || (!bound.network && !capabilities.network_control))
        {
            refusals.push(Refusal::new(
                RefusalKind::BoundUnsupported,
                Some(route),
                format!("route {route} cannot enforce the requested bound"),
            ));
        }
        // The fake declares no vendor options: only empty objects pass.
        if turn.vendor.values().any(|options| !options.is_empty()) {
            refusals.push(invalid(
                "vendor",
                format!("vendor options are unsupported on route {route}"),
            ));
        }
        refusals
    }
}
