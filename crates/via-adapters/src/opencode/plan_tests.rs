//! The pure adapter surface of `opencode-serve` (`vendors/opencode.md` §2.2,
//! §4.5, §5, §6, §9, §12) through the daemon's `AdapterSet`: capabilities,
//! version status, the per-turn refusals of `plan` and `check_turn`,
//! inherited configuration and the one server key.

use serde_json::json;

use crate::plan::{
    Bound, DescribeRequest, Inherit, InheritState, ParamSizes, RefusalKind, RoutePlan, SessionRef,
    TurnParams, VendorOptions,
};
use crate::{AdapterConfig, AdapterSet, BootstrapEnv, BoundMode};

const MODEL: &str = "opencode/big-pickle";

/// §9: the prompt admission bound.
const PROMPT_MAX: usize = 1_048_576 - 8_192;

/// An adapter set with `opencode` configured (its binary is never run).
fn set() -> (via_routes::codex::testing::TestRuntime, AdapterSet) {
    let runtime = via_routes::codex::testing::TestRuntime::new();
    let (config, resources) = runtime.parts();
    let harnesses = serde_json::value::RawValue::from_string(
        json!({"opencode": {"binary": "/nonexistent/via-test/opencode"}}).to_string(),
    )
    .unwrap();
    let env = BootstrapEnv::from_vars(std::iter::empty::<(&str, &str)>());
    let adapters = AdapterConfig::load(env, Some(&harnesses)).unwrap();
    let set = AdapterSet::new(adapters, config, resources).unwrap();
    (runtime, set)
}

fn args(list: &[&str]) -> crate::VendorArgs {
    crate::VendorArgs::try_from(list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
        .unwrap()
}

fn full() -> Bound {
    Bound {
        mode: BoundMode::Full,
        extra_write_dirs: Vec::new(),
        network: true,
    }
}

fn request() -> DescribeRequest {
    DescribeRequest {
        harness: Some("opencode".to_owned()),
        model: Some(MODEL.to_owned()),
        bound: Some(full()),
        cwd: Some("/work/a".into()),
        ..DescribeRequest::default()
    }
}

fn plan(set: &AdapterSet, edit: impl FnOnce(&mut DescribeRequest)) -> RoutePlan {
    let mut req = request();
    edit(&mut req);
    set.plan(&req).expect("planned")
}

/// The refusals' kinds, in order.
fn kinds(plan: &RoutePlan) -> Vec<RefusalKind> {
    plan.refusals
        .iter()
        .map(|refusal| refusal.kind.clone())
        .collect()
}

/// One edit of a plan request.
type Edit = Box<dyn Fn(&mut DescribeRequest)>;

fn invalid(field: &'static str) -> Vec<RefusalKind> {
    vec![RefusalKind::InvalidParam { field }]
}

fn session() -> SessionRef {
    SessionRef {
        harness: "opencode".to_owned(),
        route: "opencode-serve".to_owned(),
        adapter_version: "1".to_owned(),
    }
}

fn turn() -> TurnParams {
    TurnParams {
        bound: Some(full()),
        inherit: Some(Inherit::OD2_DEFAULT),
        model: Some(MODEL.to_owned()),
        ..TurnParams::default()
    }
}

/// §12: the declared capabilities, the route and a server key; no version
/// seen yet is `untested` with none.
#[tokio::test]
async fn opencode_plan_declares_the_packet_capabilities() {
    let (_runtime, set) = set();
    let plan = plan(&set, |_| {});
    assert_eq!(plan.route, "opencode-serve");
    assert_eq!(plan.adapter_version, "1");
    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.model.resolved, MODEL);
    assert_eq!(plan.vendor_version, None);
    assert_eq!(
        serde_json::to_value(plan.version_status).unwrap(),
        json!("untested")
    );
    assert_eq!(
        serde_json::to_value(&plan.capabilities).unwrap(),
        json!({
            "verbs": {
                "spawn": {"support": "native"},
                "resume": {"support": "native"},
                "steer": {"support": "unsupported", "reason": "deferred past the first release"},
                "cancel": {"support": "native"},
                "close": {"support": "native"},
            },
            "params": {
                "instructions": {"support": "native"},
                "output_schema": {"support": "unsupported",
                    "reason": "No structured-output field on opencode-serve 2.0.22"},
                "effort": {"support": "native"},
                "max_steps": {"support": "unsupported",
                    "reason": "No per-turn step limit on opencode-serve 2.0.22"},
            },
            "bounds": ["full"],
            "network_control": false,
            "recover": {"support": "unsupported",
                "reason": "an owned server cannot rejoin an in-flight turn after a daemon restart"},
            "usage": {"tokens": "turn", "cost": "turn"},
        })
    );
    assert_eq!(plan.effective_bound, Some(full()));
    assert!(plan.server_key.is_some());
}

/// §6: a model is taken as `providerID/id`; with none named there is no
/// default to resolve.
#[tokio::test]
async fn opencode_plan_needs_a_provider_qualified_model() {
    let (_runtime, set) = set();
    let bare = plan(&set, |req| req.model = Some("big-pickle".to_owned()));
    assert_eq!(kinds(&bare), invalid("model"));
    let refusal = set
        .plan(&DescribeRequest {
            model: None,
            ..request()
        })
        .expect_err("no default");
    assert_eq!(refusal.kind, RefusalKind::UnknownModel);
}

/// §2.2, §5, §9, §12: every per-turn refusal of a plan, before any receipt.
#[tokio::test]
async fn opencode_plan_refusals() {
    let (_runtime, set) = set();
    let vendor = |key: &str| {
        let mut options = serde_json::Map::new();
        options.insert(key.to_owned(), json!(1));
        let mut vendor = VendorOptions::new();
        vendor.insert("opencode".to_owned(), options);
        vendor
    };
    let cases: Vec<(Edit, Vec<RefusalKind>)> = vec![
        (
            Box::new(|req| req.vendor_args = args(&["--print-logs"])),
            invalid("vendor_args"),
        ),
        (
            Box::new(|req| {
                req.bound = Some(Bound {
                    mode: BoundMode::ReadOnly,
                    ..full()
                });
            }),
            vec![RefusalKind::BoundUnsupported],
        ),
        (
            Box::new(|req| {
                req.bound = Some(Bound {
                    network: false,
                    ..full()
                });
            }),
            vec![RefusalKind::BoundUnsupported],
        ),
        (
            Box::new(|req| {
                req.bound = Some(Bound {
                    extra_write_dirs: vec!["/x".into()],
                    ..full()
                });
            }),
            invalid("bound"),
        ),
        (
            Box::new(|req| req.sizes.output_schema = 2),
            invalid("output_schema"),
        ),
        (
            Box::new(move |req| req.vendor = vendor("model")),
            vec![RefusalKind::VendorOptionConflict { field: "vendor" }],
        ),
        (
            Box::new(move |req| req.vendor = vendor("anything")),
            invalid("vendor"),
        ),
        (
            Box::new(|req| req.effort = Some(String::new())),
            invalid("effort"),
        ),
        (
            Box::new(|req| req.sizes.instructions_json = 262_145),
            invalid("instructions"),
        ),
        (
            Box::new(|req| {
                req.sizes.prompt_json = PROMPT_MAX - 9;
                req.sizes.cwd_json = 10;
            }),
            invalid("prompt"),
        ),
    ];
    for (index, (edit, expected)) in cases.into_iter().enumerate() {
        let planned = plan(&set, |req| edit(req));
        assert_eq!(kinds(&planned), expected, "case {index}");
    }
    // At the bounds, and the efforts judged later.
    for edit in [
        Box::new(|req: &mut DescribeRequest| req.sizes.instructions_json = 262_144)
            as Box<dyn Fn(&mut DescribeRequest)>,
        Box::new(|req| {
            req.sizes.prompt_json = PROMPT_MAX - 10;
            req.sizes.cwd_json = 10;
        }),
        Box::new(|req| req.effort = Some("default".to_owned())),
        Box::new(|req| req.effort = Some("ultra".to_owned())),
        Box::new(|req| req.vendor = VendorOptions::new()),
    ] {
        let planned = plan(&set, |req| edit(req));
        assert!(planned.refusals.is_empty(), "{:?}", planned.refusals);
    }
}

/// §6 `check_turn`: the same per-turn refusals, a step limit, AD12's
/// adapter version, and no location-dependent effort check.
#[tokio::test]
async fn opencode_check_turn_refusals() {
    let (_runtime, set) = set();
    let check = |edit: &dyn Fn(&mut TurnParams)| {
        let mut params = turn();
        edit(&mut params);
        set.check_turn(&session(), &params)
    };
    assert_eq!(check(&|_| {}).unwrap().effective_bound, Some(full()));
    assert!(check(&|turn| turn.effort = Some("ultra".to_owned())).is_ok());
    assert!(check(&|turn| turn.effort = Some("default".to_owned())).is_ok());
    for (edit, field) in [
        (
            Box::new(|turn: &mut TurnParams| turn.max_steps = Some(3))
                as Box<dyn Fn(&mut TurnParams)>,
            "max_steps",
        ),
        (Box::new(|turn| turn.output_schema = true), "output_schema"),
        (
            Box::new(|turn| turn.vendor_args = args(&["-x"])),
            "vendor_args",
        ),
        (
            Box::new(|turn| {
                turn.sizes = ParamSizes {
                    prompt_json: PROMPT_MAX,
                    cwd_json: 1,
                    ..ParamSizes::default()
                };
            }),
            "prompt",
        ),
        (
            Box::new(|turn| turn.sizes.instructions_json = 262_145),
            "instructions",
        ),
    ] {
        let refusal = check(&*edit).expect_err(field);
        assert_eq!(refusal.kind, RefusalKind::InvalidParam { field });
    }
    let refusal = set
        .check_turn(
            &SessionRef {
                adapter_version: "0".to_owned(),
                ..session()
            },
            &turn(),
        )
        .expect_err("another adapter version");
    assert_eq!(refusal.kind, RefusalKind::HarnessUnavailable);
    assert_eq!(refusal.reason, Some("adapter_version"));
}

/// §4.5: every category requested on or off is `unknown`, but skills off,
/// which a session rule applies; the warning lists each other category.
#[tokio::test]
async fn opencode_inherit_states() {
    let (_runtime, set) = set();
    let default = plan(&set, |_| {});
    let states = |inherit: Inherit| {
        crate::plan::Category::ALL
            .into_iter()
            .map(|category| inherit.get(category))
            .collect::<Vec<_>>()
    };
    let unknown = vec![InheritState::Unknown; 6];
    assert_eq!(states(default.inherit.effective), unknown);
    let warning = &default.warnings[0];
    assert_eq!(warning.code, "config_switch_unverified");
    assert_eq!(
        warning.data.as_ref().unwrap()["categories"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    // Through the harness's configured `inherit`: skills off.
    let runtime = via_routes::codex::testing::TestRuntime::new();
    let (config, resources) = runtime.parts();
    let harnesses = serde_json::value::RawValue::from_string(
        json!({"opencode": {"binary": "/nonexistent/via-test/opencode",
            "inherit": {"hooks": false, "mcp_servers": false, "plugins": true,
                "skills": false, "agents": true, "instruction_files": false}}})
        .to_string(),
    )
    .unwrap();
    let env = BootstrapEnv::from_vars(std::iter::empty::<(&str, &str)>());
    let skills_off = AdapterSet::new(
        AdapterConfig::load(env, Some(&harnesses)).unwrap(),
        config,
        resources,
    )
    .unwrap();
    let off = plan(&skills_off, |_| {});
    let mut skills_off_states = unknown.clone();
    skills_off_states[3] = InheritState::Off;
    assert_eq!(states(off.inherit.effective), skills_off_states);
    let listed: Vec<String> = off.warnings[0].data.as_ref().unwrap()["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["category"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        listed,
        [
            "hooks",
            "mcp_servers",
            "plugins",
            "agents",
            "instruction_files"
        ]
    );
    // §3.1: one server for all of VIA.
    assert_eq!(off.server_key, default.server_key);
}

/// §3.1: nothing per session enters the server key: model, cwd, effort,
/// instructions and bound leave it unchanged.
#[tokio::test]
async fn opencode_server_key_is_per_daemon() {
    let (_runtime, set) = set();
    let first = plan(&set, |_| {});
    let other = plan(&set, |req| {
        req.model = Some("opencode/other".to_owned());
        req.cwd = Some("/work/b".into());
        req.effort = Some("high".to_owned());
        req.sizes.instructions_json = 10;
    });
    assert_eq!(first.server_key, other.server_key);
    // §6 `models`: nothing before a live server's catalog.
    assert!(set.models(Some("opencode")).is_empty());
    assert!(set.servers().is_empty());
}
