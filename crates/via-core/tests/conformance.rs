//! C2 conformance kit, planning half (adapter design §8 item 2): the pure
//! `plan`, `check_turn` and `models` surface of `AdapterSet`, run on
//! `Adapter::Fake` scenario profiles. Written before the planning code.
//! It lives in Core's tests since `AdapterSet::new` takes Store's runtime
//! resources (C2 §2), which only a crate above Store can open; the run
//! half is `conformance_driver.rs`.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use std::fs;
use std::ops::Deref;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

use serde_json::{Value, json};
use tempfile::TempDir;
use via_adapters::observation::{Observation, ProgressMarks, UsageSample};
use via_adapters::{
    AdapterConfig, AdapterSet, BOOTSTRAP_ENV, BootstrapEnv, Bound, CatalogModel, Category,
    CategoryDecl, DescribeRequest, HARNESSES, InheritState, ModelSource, RefusalKind,
    RuntimeConfig, SessionRef, Switch, TurnCheck, TurnParams, Verb, VerbReq, harness_names,
    resolve_model,
};
use via_store::Store;

/// A fake fixture deployment whose scenario file holds `scenario`.
fn fixture(scenario: &Value) -> (TempDir, BootstrapEnv) {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("fake-agent");
    fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let scenario_path = dir.path().join("scenario.json");
    fs::write(&scenario_path, serde_json::to_vec(scenario).unwrap()).unwrap();
    let sync = dir.path().join("sync");
    fs::create_dir(&sync).unwrap();
    let env = BootstrapEnv::from_vars([
        ("VIA_FAKE_AGENT_BINARY", binary.into_os_string()),
        ("VIA_FAKE_SCENARIO", scenario_path.into_os_string()),
        ("VIA_FAKE_SYNC_DIR", sync.into_os_string()),
    ]);
    (dir, env)
}

/// An adapter set over a private Store; planning never reaches Route, so
/// its anchor binary need not exist.
struct Planner {
    set: AdapterSet,
    _store: Store,
    _dir: TempDir,
}

impl Deref for Planner {
    type Target = AdapterSet;

    fn deref(&self) -> &AdapterSet {
        &self.set
    }
}

fn planner(config: AdapterConfig) -> Planner {
    let dir = tempfile::tempdir().unwrap();
    for part in ["state", "runtime"] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(dir.path().join(part))
            .unwrap();
    }
    let store = Store::open(&dir.path().join("state")).unwrap();
    let set = AdapterSet::new(
        config,
        RuntimeConfig {
            anchor_binary: dir.path().join("anchor"),
            anchor_dir: dir.path().join("runtime"),
        },
        store.runtime_resources(),
    )
    .unwrap();
    Planner {
        set,
        _store: store,
        _dir: dir,
    }
}

/// An adapter set whose fake runs `profile` (object scenario form, H2).
fn fake_set(profile: &Value) -> Planner {
    let (_dir, env) = fixture(&json!({"profile": profile, "scripts": []}));
    planner(AdapterConfig::load(env, None).unwrap())
}

/// The fake with the default profile (legacy scenario form, no profile).
fn default_set() -> Planner {
    let (_dir, env) = fixture(&json!({"scripts": []}));
    planner(AdapterConfig::load(env, None).unwrap())
}

fn describe(harness: Option<&str>, model: Option<&str>) -> DescribeRequest {
    DescribeRequest {
        harness: harness.map(str::to_owned),
        model: model.map(str::to_owned),
        ..DescribeRequest::default()
    }
}

fn session(adapter_version: &str) -> SessionRef {
    SessionRef {
        harness: "fake".to_owned(),
        route: "fake".to_owned(),
        adapter_version: adapter_version.to_owned(),
    }
}

/// Capabilities with `cancel` partial and effort native, else the defaults.
fn partial_capabilities() -> Value {
    json!({
        "verbs": {"spawn":{"support":"native"},"resume":{"support":"native"},
                  "steer":{"support":"unsupported","reason":"no steer input"},
                  "cancel":{"support":"partial","semantics":"aborts_tools_then_result"},
                  "close":{"support":"native"}},
        "params": {"instructions":{"support":"unsupported","reason":"no instructions input"},
                   "output_schema":{"support":"unsupported","reason":"no schema input"},
                   "effort":{"support":"native"},
                   "max_steps":{"support":"unsupported","reason":"no step limit"}},
        "bounds": [], "network_control": false,
        "recover": {"support":"unsupported","reason":"no recovery"},
        "usage": {"tokens":"turn","cost":"unavailable"}
    })
}

/// (4) C1 §4.1: `require` passes only `native` unless written `verb:partial`.
#[test]
fn conformance_require_partial_cancel() {
    let set = fake_set(&json!({"capabilities": partial_capabilities()}));
    let mut request = describe(Some("fake"), None);
    request.require = vec![VerbReq::parse("cancel:partial").unwrap()];
    let plan = set.plan(&request).unwrap();
    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(
        serde_json::to_value(&plan.capabilities).unwrap()["verbs"]["cancel"],
        json!({"support":"partial","semantics":"aborts_tools_then_result"})
    );

    request.require = vec![VerbReq::parse("cancel").unwrap()];
    let plan = set.plan(&request).unwrap();
    assert_eq!(plan.refusals.len(), 1);
    assert_eq!(
        plan.refusals[0].kind,
        RefusalKind::MissingCapability { verb: Verb::Cancel }
    );
    assert_eq!(plan.refusals[0].route, Some("fake"));

    // A native verb meets both spellings; an unknown name is not a verb.
    request.require = vec![
        VerbReq::parse("close").unwrap(),
        VerbReq::parse("close:partial").unwrap(),
    ];
    assert!(set.plan(&request).unwrap().refusals.is_empty());
    assert_eq!(VerbReq::parse("fly"), None);
}

/// (10) Design §5.2: a model-only plan resolves a unique catalog match, by
/// name or alias; several matches are `invalid_params` naming `harness`;
/// none is `unknown_model`.
#[test]
fn conformance_model_only_catalog_match() {
    let set = fake_set(&json!({"models": [{"model":"fake-large","aliases":["large"]}]}));
    for requested in ["fake-large", "large"] {
        let plan = set.plan(&describe(None, Some(requested))).unwrap();
        assert_eq!(plan.harness, "fake");
        assert_eq!(plan.route, "fake");
        assert_eq!(plan.model.requested.as_deref(), Some(requested));
        assert_eq!(plan.model.resolved, "fake-large");
    }

    let absent = set.plan(&describe(None, Some("nope"))).unwrap_err();
    assert_eq!(absent.kind, RefusalKind::UnknownModel);

    // Two configured adapters that both catalog the model.
    let catalog = [CatalogModel {
        model: "shared".to_owned(),
        aliases: vec![],
    }];
    let ambiguous =
        resolve_model("shared", [("fake", &catalog[..]), ("claude", &catalog[..])]).unwrap_err();
    assert_eq!(ambiguous, RefusalKind::InvalidParam { field: "harness" });
    assert_eq!(
        resolve_model("shared", [("fake", &catalog[..])]).unwrap(),
        ("fake", "shared".to_owned())
    );

    // With no fake fixture, nothing is configured, so nothing resolves.
    let unconfigured =
        planner(AdapterConfig::load(BootstrapEnv::from_vars::<_, &str, &str>([]), None).unwrap());
    assert_eq!(
        unconfigured
            .plan(&describe(None, Some("fake")))
            .unwrap_err()
            .kind,
        RefusalKind::UnknownModel
    );
    assert_eq!(
        unconfigured
            .plan(&describe(Some("fake"), None))
            .unwrap_err()
            .kind,
        RefusalKind::HarnessUnavailable
    );
}

/// (6) AD12, plan half: each version declares only its predecessor, so
/// v1 → v2 → v3 resumes step by step; a version two steps back is refused
/// `harness_unavailable` with `reason: adapter_version`.
#[test]
fn conformance_adapter_version_chain() {
    let v2 = fake_set(&json!({"adapter_version":"2","compatible":["1"]}));
    let v3 = fake_set(&json!({"adapter_version":"3","compatible":["2"]}));
    let turn = TurnParams::default();

    assert_eq!(
        v2.check_turn(&session("1"), &turn),
        Ok(TurnCheck::default())
    );
    assert_eq!(
        v2.check_turn(&session("2"), &turn),
        Ok(TurnCheck::default())
    );
    assert_eq!(
        v3.check_turn(&session("2"), &turn),
        Ok(TurnCheck::default())
    );
    assert_eq!(
        v3.check_turn(&session("3"), &turn),
        Ok(TurnCheck::default())
    );
    assert_eq!(
        v3.plan(&describe(Some("fake"), None))
            .unwrap()
            .adapter_version,
        "3"
    );

    let refused = v3.check_turn(&session("1"), &turn).unwrap_err();
    assert_eq!(refused.kind, RefusalKind::HarnessUnavailable);
    assert_eq!(refused.reason, Some("adapter_version"));
    assert_eq!(refused.route, Some("fake"));

    // An unknown harness or a different route is also unavailable.
    let mut other = session("3");
    other.harness = "nope".to_owned();
    assert_eq!(
        v3.check_turn(&other, &turn).unwrap_err().kind,
        RefusalKind::HarnessUnavailable
    );
    let mut rerouted = session("3");
    rerouted.route = "elsewhere".to_owned();
    assert_eq!(
        v3.check_turn(&rerouted, &turn).unwrap_err().kind,
        RefusalKind::HarnessUnavailable
    );
}

/// Sol r2 #4 (C2 `check_turn` → `TurnCheck`): `check_turn` reports the
/// turn's bound as the route will apply it, normalized as the plan's
/// `effective_bound` is; a turn with no bound has none.
#[test]
fn conformance_check_turn_reports_the_effective_bound() {
    let normalized = json!({"mode":"full","extra_write_dirs":["/normalized"],"network":true});
    let mut capabilities = partial_capabilities();
    capabilities["bounds"] = json!(["full"]);
    let set = fake_set(&json!({"capabilities": capabilities, "normalized_bound": normalized}));
    let current = session(env!("CARGO_PKG_VERSION"));
    let bound: Bound =
        serde_json::from_value(json!({"mode":"full","extra_write_dirs":[],"network":true}))
            .unwrap();
    let checked = set
        .check_turn(
            &current,
            &TurnParams {
                bound: Some(bound),
                ..TurnParams::default()
            },
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(checked.effective_bound).unwrap(),
        normalized
    );
    assert_eq!(
        set.check_turn(&current, &TurnParams::default()),
        Ok(TurnCheck::default())
    );
}

/// (18) AD18, plan half: an effort outside the route's compiled table, or
/// empty, is `invalid_params` naming `effort` before any receipt; on a route
/// with no effort setting any effort is.
#[test]
fn conformance_unknown_effort_refused() {
    // `""` in the table must not make an empty effort valid.
    let set = fake_set(&json!({"capabilities": partial_capabilities(),
                                "efforts": ["low", "high", ""]}));
    let with_effort = |effort: &str| DescribeRequest {
        effort: Some(effort.to_owned()),
        ..describe(Some("fake"), None)
    };
    assert!(set.plan(&with_effort("high")).unwrap().refusals.is_empty());
    for effort in ["turbo", ""] {
        let plan = set.plan(&with_effort(effort)).unwrap();
        assert_eq!(plan.refusals.len(), 1, "{effort:?}");
        assert_eq!(
            plan.refusals[0].kind,
            RefusalKind::InvalidParam { field: "effort" }
        );
    }
    let turn = |effort: &str| TurnParams {
        effort: Some(effort.to_owned()),
        ..TurnParams::default()
    };
    let current = session(env!("CARGO_PKG_VERSION"));
    assert_eq!(
        set.check_turn(&current, &turn("low")),
        Ok(TurnCheck::default())
    );
    for effort in ["turbo", ""] {
        assert_eq!(
            set.check_turn(&current, &turn(effort)).unwrap_err().kind,
            RefusalKind::InvalidParam { field: "effort" },
            "{effort:?}"
        );
    }

    let default = default_set();
    assert_eq!(
        default.plan(&with_effort("low")).unwrap().refusals[0].kind,
        RefusalKind::InvalidParam { field: "effort" }
    );
}

fn states(plan: &via_adapters::RoutePlan) -> Vec<(Category, InheritState)> {
    Category::ALL
        .iter()
        .map(|category| (*category, plan.inherit.get(*category)))
        .collect()
}

fn switch_warning(plan: &via_adapters::RoutePlan) -> Option<Value> {
    let warnings = serde_json::to_value(&plan.warnings).unwrap();
    let mut found = warnings
        .as_array()
        .unwrap()
        .iter()
        .filter(|warning| warning["code"] == "config_switch_unverified");
    let first = found.next().cloned();
    assert_eq!(found.next(), None, "one config_switch_unverified warning");
    first
}

/// (19) AD13, state half, on the compiled OD2 defaults (hooks and MCP off,
/// the rest on): `on`/`off` only when verified, else `unknown`; every
/// category whose effective state is not the requested one is listed in one
/// `config_switch_unverified` warning, in both directions.
#[test]
fn conformance_inherit_effective_states() {
    use Category::{Agents, Hooks, InstructionFiles, McpServers, Plugins, Skills};
    use InheritState::{Off, On, Unknown};

    // Codex-like: a verified hooks switch, an unverified MCP switch, and no
    // switch applied with an unverified vendor default for the rest.
    let codex = fake_set(&json!({"categories": {
        "hooks": {"off": "verified"}, "mcp_servers": {"off": "unverified"},
        "plugins": {}, "skills": {}, "agents": {}, "instruction_files": {}}}))
    .plan(&describe(Some("fake"), None))
    .unwrap();
    assert_eq!(
        states(&codex),
        [
            (Hooks, Off),
            (McpServers, Unknown),
            (Plugins, Unknown),
            (Skills, Unknown),
            (Agents, Unknown),
            (InstructionFiles, Unknown),
        ]
    );
    assert_eq!(
        switch_warning(&codex).unwrap()["data"]["categories"],
        json!([
            {"category":"mcp_servers","requested":"off","effective":"unknown"},
            {"category":"plugins","requested":"on","effective":"unknown"},
            {"category":"skills","requested":"on","effective":"unknown"},
            {"category":"agents","requested":"on","effective":"unknown"},
            {"category":"instruction_files","requested":"on","effective":"unknown"},
        ])
    );

    // OpenCode-like: a private profile observed state and no switch.
    let opencode = fake_set(&json!({"categories": {
        "hooks": {"observed": "off"}, "mcp_servers": {"observed": "off"},
        "plugins": {"observed": "off"}, "skills": {"observed": "on"},
        "agents": {"observed": "off"}, "instruction_files": {}}}))
    .plan(&describe(Some("fake"), None))
    .unwrap();
    assert_eq!(
        states(&opencode),
        [
            (Hooks, Off),
            (McpServers, Off),
            (Plugins, Off),
            (Skills, On),
            (Agents, Off),
            (InstructionFiles, Unknown),
        ]
    );
    assert_eq!(
        switch_warning(&opencode).unwrap()["data"]["categories"],
        json!([
            {"category":"plugins","requested":"on","effective":"off"},
            {"category":"agents","requested":"on","effective":"off"},
            {"category":"instruction_files","requested":"on","effective":"unknown"},
        ])
    );

    // Claude-like: an unverified hooks switch; skills listed by inventory.
    let claude = fake_set(&json!({"categories": {
        "hooks": {"off": "unverified"}, "mcp_servers": {"off": "verified"},
        "skills": {"observed": "on"}}}))
    .plan(&describe(Some("fake"), None))
    .unwrap();
    assert_eq!(claude.inherit.get(Hooks), Unknown);
    assert_eq!(claude.inherit.get(McpServers), Off);
    assert_eq!(claude.inherit.get(Skills), On);
    assert_eq!(
        switch_warning(&claude).unwrap()["data"]["categories"],
        json!([{"category":"hooks","requested":"off","effective":"unknown"}])
    );

    // The default fake applies every request, verified: no warning.
    let default = default_set().plan(&describe(Some("fake"), None)).unwrap();
    assert_eq!(
        states(&default),
        [
            (Hooks, Off),
            (McpServers, Off),
            (Plugins, On),
            (Skills, On),
            (Agents, On),
            (InstructionFiles, On),
        ]
    );
    assert_eq!(switch_warning(&default), None);
}

/// C1 §3.13: `models` lists the bundled catalog with `source: bundled`,
/// only for a configured harness.
#[test]
fn conformance_models_are_bundled() {
    let set = default_set();
    let models = serde_json::to_value(set.models(None)).unwrap();
    assert_eq!(
        models,
        json!([{"model":"fake","harness":"fake","aliases":[],"source":"bundled"}])
    );
    assert_eq!(set.models(Some("fake"))[0].source, ModelSource::Bundled);
    assert!(set.models(Some("codex")).is_empty());
    let unconfigured =
        planner(AdapterConfig::load(BootstrapEnv::from_vars::<_, &str, &str>([]), None).unwrap());
    assert!(unconfigured.models(None).is_empty());
}

/// H6: the default fake's plan serializes to C1 §3.1, with the capability
/// DTO of §4.1 exactly as S1 reports it.
#[test]
fn conformance_route_plan_c1_shape() {
    let plan = default_set()
        .plan(&describe(Some("fake"), Some("fake")))
        .unwrap();
    assert_eq!(
        serde_json::to_value(&plan).unwrap(),
        json!({
            "harness":"fake","model":{"requested":"fake","resolved":"fake"},
            "route":"fake","adapter_version":env!("CARGO_PKG_VERSION"),
            "vendor_version":null,"version_status":"untested",
            "capabilities":{
                "verbs":{"spawn":{"support":"native"},"resume":{"support":"native"},
                         "steer":{"support":"unsupported","reason":"the fake route has no steer input"},
                         "cancel":{"support":"native"},"close":{"support":"native"}},
                "params":{"instructions":{"support":"unsupported","reason":"the fake route has no instructions input"},
                          "output_schema":{"support":"unsupported","reason":"the fake route has no schema input"},
                          "effort":{"support":"unsupported","reason":"the fake route has no effort setting"},
                          "max_steps":{"support":"unsupported","reason":"the fake route has no step limit"}},
                "bounds":[],"network_control":false,
                "recover":{"support":"unsupported","reason":"fake turns do not survive a daemon restart"},
                "usage":{"tokens":"turn","cost":"unavailable"}},
            "effective_bound":null,"refusals":[],
            "warnings":[{"code":"vendor_version_untested","message":"the fake agent reports no version"}]
        })
    );
}

/// The default fake keeps S1's refusals: any bound, and any vendor option,
/// listed by member with the route.
#[test]
fn conformance_default_fake_refusals() {
    let set = default_set();
    let request = DescribeRequest {
        bound: Some(
            serde_json::from_value(json!({"mode":"full","extra_write_dirs":[],"network":true}))
                .unwrap(),
        ),
        vendor: serde_json::from_value(json!({"fake":{"k":"v"},"codex":{}})).unwrap(),
        ..describe(Some("fake"), None)
    };
    let plan = set.plan(&request).unwrap();
    let kinds: Vec<_> = plan
        .refusals
        .iter()
        .map(|refusal| refusal.kind.clone())
        .collect();
    assert_eq!(
        kinds,
        [
            RefusalKind::BoundUnsupported,
            RefusalKind::InvalidParam { field: "vendor" }
        ]
    );
    assert_eq!(plan.effective_bound, None);
    let empty = DescribeRequest {
        vendor: serde_json::from_value(json!({"fake":{},"codex":{}})).unwrap(),
        ..describe(Some("fake"), None)
    };
    assert!(set.plan(&empty).unwrap().refusals.is_empty());

    // Harnesses outside this build are unavailable, named or not.
    for harness in ["claude", "codex", "opencode", "acp:devin", "nope"] {
        assert_eq!(
            set.plan(&describe(Some(harness), None)).unwrap_err().kind,
            RefusalKind::HarnessUnavailable,
            "{harness}"
        );
    }
}

/// Design §5.2: with the harness given, a catalogued name or alias
/// resolves, and an uncatalogued model passes through for the vendor to
/// judge; `UnknownModel` is only for an unresolved model-only request.
#[test]
fn conformance_explicit_model_passes_through() {
    let set = fake_set(&json!({"models": [{"model":"fake-large","aliases":["large"]}]}));
    let resolved = |model| {
        set.plan(&describe(Some("fake"), Some(model)))
            .unwrap()
            .model
            .resolved
    };
    assert_eq!(resolved("large"), "fake-large");
    assert_eq!(resolved("other"), "other");
    assert_eq!(
        set.plan(&describe(None, Some("other"))).unwrap_err().kind,
        RefusalKind::UnknownModel
    );
    let empty = fake_set(&json!({"models": []}));
    assert_eq!(
        empty
            .plan(&describe(Some("fake"), Some("other")))
            .unwrap()
            .model
            .resolved,
        "other"
    );
    let plan = default_set()
        .plan(&describe(Some("fake"), Some("other")))
        .unwrap();
    assert_eq!(plan.model.resolved, "other");
    assert_eq!(plan.server_key, None);
}

/// `check_turn` refuses each per-turn value the route cannot take, by
/// member; a refusal serializes with its field, kind, route and reason.
#[test]
fn conformance_check_turn_refusals() {
    let set = default_set();
    let current = session(env!("CARGO_PKG_VERSION"));
    let bound: Bound =
        serde_json::from_value(json!({"mode":"read_only","extra_write_dirs":[],"network":false}))
            .unwrap();
    let cases = [
        (
            TurnParams {
                output_schema: true,
                ..TurnParams::default()
            },
            RefusalKind::InvalidParam {
                field: "output_schema",
            },
        ),
        (
            TurnParams {
                max_steps: Some(3),
                ..TurnParams::default()
            },
            RefusalKind::InvalidParam { field: "max_steps" },
        ),
        (
            TurnParams {
                bound: Some(bound),
                ..TurnParams::default()
            },
            RefusalKind::BoundUnsupported,
        ),
        (
            TurnParams {
                vendor: serde_json::from_value(json!({"fake":{"k":"v"}})).unwrap(),
                ..TurnParams::default()
            },
            RefusalKind::InvalidParam { field: "vendor" },
        ),
    ];
    for (turn, kind) in cases {
        let refusal = set.check_turn(&current, &turn).unwrap_err();
        assert_eq!(refusal.kind, kind);
        assert_eq!(refusal.route, Some("fake"));
    }
    assert_eq!(
        set.check_turn(&current, &TurnParams::default()),
        Ok(TurnCheck::default())
    );

    let partial = fake_set(&json!({"capabilities": partial_capabilities()}));
    let mut request = describe(Some("fake"), None);
    request.require = vec![VerbReq::parse("cancel").unwrap()];
    let plan = partial.plan(&request).unwrap();
    let refusal = serde_json::to_value(&plan.refusals).unwrap();
    assert_eq!(refusal[0]["field"], "cancel");
    assert_eq!(refusal[0]["kind"], "missing_capability");
    assert_eq!(refusal[0]["route"], "fake");
    assert!(refusal[0]["message"].is_string());

    let version = set.check_turn(&session("0.0.0-old"), &TurnParams::default());
    let refusal = serde_json::to_value(vec![version.unwrap_err()]).unwrap();
    assert_eq!(refusal[0]["kind"], "harness_unavailable");
    assert_eq!(refusal[0]["reason"], "adapter_version");
    assert_eq!(refusal[0]["route"], "fake");
    assert_eq!(refusal[0].get("field"), None);
}

/// AD13, per direction: the request only through a verified switch; an
/// unverified switch is `unknown`; with no switch, the verified observation,
/// else `unknown`.
#[test]
fn conformance_inherit_directional_table() {
    use InheritState::{Off, On, Unknown};
    use Switch::{None as NoSwitch, Unverified, Verified};
    let cases = [
        // (on switch, off switch, observed, requested, effective)
        (Verified, NoSwitch, None, On, On),
        (Unverified, NoSwitch, None, On, Unknown),
        (Unverified, NoSwitch, Some(Off), On, Unknown),
        (NoSwitch, NoSwitch, Some(Off), On, Off),
        (NoSwitch, NoSwitch, Some(On), On, On),
        (NoSwitch, NoSwitch, None, On, Unknown),
        (NoSwitch, Verified, None, Off, Off),
        (NoSwitch, Unverified, None, Off, Unknown),
        (NoSwitch, Unverified, Some(On), Off, Unknown),
        (NoSwitch, NoSwitch, Some(On), Off, On),
        (NoSwitch, NoSwitch, Some(Off), Off, Off),
        (NoSwitch, NoSwitch, None, Off, Unknown),
    ];
    for (on, off, observed, requested, effective) in cases {
        let decl = CategoryDecl { on, off, observed };
        assert_eq!(
            decl.effective(requested),
            effective,
            "{decl:?} {requested:?}"
        );
    }
}

/// C2 §4: a progress observation carries an AD6 usage sample whose
/// components are independently nullable.
#[test]
fn conformance_progress_carries_usage_sample() {
    let usage = UsageSample {
        key: Some("call-1".to_owned()),
        input: Some(10),
        cached_input: None,
        output: Some(3),
        reasoning_output: None,
        total: None,
    };
    let marks = ProgressMarks {
        model: true,
        tools_started: vec![],
        tools_ended: vec![],
        usage: Some(usage.clone()),
    };
    let Observation::Progress(marks) = Observation::Progress(marks) else {
        unreachable!()
    };
    assert_eq!(marks.usage, Some(usage));
}

/// H2 configuration: only the bootstrap names are read (S-LAUNCH: runtime
/// §6.1's nine); the fixture is all or nothing and kept with its profile;
/// `harnesses` must be an object, and is parsed (S-LAUNCH's own tests
/// cover its rules); the legacy scenario forms keep the default profile;
/// malformed or non-JSON scenarios are refused.
#[test]
fn conformance_config_load() {
    assert_eq!(
        BOOTSTRAP_ENV,
        [
            "HOME",
            "PATH",
            "LANG",
            "USER",
            "LOGNAME",
            "XDG_RUNTIME_DIR",
            "VIA_FAKE_AGENT_BINARY",
            "VIA_FAKE_SCENARIO",
            "VIA_FAKE_SYNC_DIR"
        ]
    );
    let names: Vec<_> = harness_names().collect();
    assert_eq!(names, ["claude", "codex", "opencode", "fake"]);

    let object = serde_json::value::RawValue::from_string(r#"{"codex":{}}"#.to_owned()).unwrap();
    let array = serde_json::value::RawValue::from_string("[]".to_owned()).unwrap();
    let none = || BootstrapEnv::from_vars::<_, &str, &str>([]);
    let config = AdapterConfig::load(none(), Some(&object)).unwrap();
    let codex = HARNESSES.iter().find(|row| row.name == "codex").unwrap();
    assert_eq!(config.harness(codex).binary(), None);
    assert!(config.fake_fixture().is_none());
    assert!(AdapterConfig::load(none(), Some(&array)).is_err());

    let (_dir, env) = fixture(&json!({"scripts": []}));
    let partial = BootstrapEnv::from_vars(
        env.vars()
            .filter(|(name, _)| *name != "VIA_FAKE_SYNC_DIR")
            .map(|(name, value)| (name, value.to_owned())),
    );
    assert!(AdapterConfig::load(partial, None).is_err());

    let paths: Vec<_> = env.vars().map(|(_, value)| value.to_owned()).collect();
    let config = AdapterConfig::load(env, None).unwrap();
    let fake = config.fake_fixture().unwrap();
    assert_eq!(fake.binary().as_os_str(), paths[0]);
    assert_eq!(fake.scenario().as_os_str(), paths[1]);
    assert_eq!(fake.sync_dir().as_os_str(), paths[2]);

    // The legacy forms via-fake-agent reads keep the default profile.
    for scenario in [
        r#"{"scripts": []}"#,
        r#"{"expected_request": {}, "steps": []}"#,
    ] {
        let (dir, env) = fixture(&json!(null));
        fs::write(dir.path().join("scenario.json"), scenario).unwrap();
        let set = planner(AdapterConfig::load(env, None).unwrap());
        assert_eq!(set.models(None).len(), 1, "{scenario}");
    }
    // Anything else is refused, never defaulted.
    for scenario in [
        "not json\n",
        "",
        "[]",
        "null",
        r#"{"scripts": ["#,
        r#"{"profile": {}}"#,
        r#"{"scripts": {}}"#,
        r#"{"scripts": [], "other": 1}"#,
        r#"{"expected_request": {}}"#,
        r#"{"profile": {}, "expected_request": {}, "steps": []}"#,
    ] {
        let (dir, env) = fixture(&json!(null));
        fs::write(dir.path().join("scenario.json"), scenario).unwrap();
        assert!(AdapterConfig::load(env, None).is_err(), "{scenario}");
    }
    let (_dir, env) = fixture(&json!({"profile": {"no_such_field": 1}, "scripts": []}));
    assert!(AdapterConfig::load(env, None).is_err());
}
