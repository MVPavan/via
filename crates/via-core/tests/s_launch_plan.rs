//! S-LAUNCH (adapter design §5.4, §5.5): `AdapterSet::plan` takes the
//! planned harness's configured `inherit`; the fake keeps the OD2 default
//! whatever the vendor harnesses configure. It lives in Core's tests since
//! `AdapterSet::new` takes Store's runtime resources.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

use serde_json::json;
use serde_json::value::RawValue;
use via_adapters::{
    AdapterConfig, AdapterSet, BootstrapEnv, DescribeRequest, Inherit, InheritPlan, RefusalKind,
    RuntimeConfig,
};
use via_store::Store;

#[test]
fn s_launch_plan_inherit_per_harness() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("fake-agent");
    fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    // Every switch verified both ways: the effective state is the request.
    let both = json!({"on": "verified", "off": "verified"});
    let scenario = dir.path().join("scenario.json");
    let profile = json!({"categories": {"hooks": both, "mcp_servers": both, "plugins": both,
        "skills": both, "agents": both, "instruction_files": both}});
    fs::write(
        &scenario,
        serde_json::to_vec(&json!({"profile": profile, "scripts": []})).unwrap(),
    )
    .unwrap();
    let sync = dir.path().join("sync");
    fs::create_dir(&sync).unwrap();
    for part in ["state", "runtime"] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(dir.path().join(part))
            .unwrap();
    }
    let env = BootstrapEnv::from_vars([
        ("VIA_FAKE_AGENT_BINARY", binary.into_os_string()),
        ("VIA_FAKE_SCENARIO", scenario.into_os_string()),
        ("VIA_FAKE_SYNC_DIR", sync.into_os_string()),
    ]);
    // Pinned, so each vendor harness's adapter is built: its plan, not a
    // missing binary, answers. Nothing here runs either.
    let harnesses = RawValue::from_string(
        json!({"claude":{"binary":dir.path().join("claude"),
                         "inherit":{"hooks":true,"mcp_servers":true}},
               "codex":{"binary":dir.path().join("codex"),"inherit":{"skills":false}}})
        .to_string(),
    )
    .unwrap();
    let config = AdapterConfig::load(env, Some(&harnesses)).unwrap();
    let store = Store::open(&dir.path().join("state")).unwrap();
    let set = AdapterSet::new(
        config,
        RuntimeConfig {
            anchor_binary: dir.path().join("anchor"),
            anchor_dir: dir.path().join("runtime"),
            vendor_state_dir: dir.path().join("vendor"),
        },
        store.runtime_resources(),
    )
    .unwrap();
    let describe = |harness: &str, model: Option<&str>| DescribeRequest {
        harness: Some(harness.to_owned()),
        model: model.map(str::to_owned),
        ..DescribeRequest::default()
    };
    let od2 = InheritPlan {
        requested: Inherit::OD2_DEFAULT,
        effective: Inherit::OD2_DEFAULT,
    };
    let inherit = |states: serde_json::Value| serde_json::from_value::<Inherit>(states).unwrap();
    // Claude plans (x.3.2 C1) with its configured request, hooks and MCP
    // servers on. The default mode, without `--restricted` or
    // `--strict-mcp-config`, loads every category (via-umz; owner,
    // 2026-10-05).
    let all_on = inherit(json!({"hooks":"on","mcp_servers":"on","plugins":"on",
        "skills":"on","agents":"on","instruction_files":"on"}));
    let claude = InheritPlan {
        requested: all_on,
        effective: all_on,
    };
    // Codex (x.3.2 X1): `harnesses.codex.inherit` sets skills off over
    // Codex's default request, every category on (owner 2026-10-05). Hooks,
    // MCP servers and instruction files are `on` by the packet's recorded
    // live evidence (owner 2026-10-06); skills, plugins and agents have
    // none, so they are `unknown`.
    let codex = InheritPlan {
        requested: inherit(json!({"hooks": "on", "mcp_servers": "on", "plugins": "on",
            "skills": "off", "agents": "on", "instruction_files": "on"})),
        effective: inherit(json!({"hooks": "on", "mcp_servers": "on",
            "plugins": "unknown", "skills": "unknown", "agents": "unknown",
            "instruction_files": "on"})),
    };
    // Per harness, with the model the request names: the plan's inherit,
    // or the refusal. Nothing here runs a binary. Each adapter track flips
    // its own row; Codex has no bundled catalog, so it plans a named model.
    let rows: [(&str, Option<&str>, Result<InheritPlan, RefusalKind>); 3] = [
        ("claude", None, Ok(claude)),
        ("codex", Some("gpt-6-sol"), Ok(codex)),
        ("fake", None, Ok(od2)),
    ];
    for (harness, model, expected) in rows {
        let planned = set
            .plan(&describe(harness, model))
            .map(|plan| plan.inherit)
            .map_err(|refusal| refusal.kind);
        assert_eq!(planned, expected, "{harness}");
    }
    // Per-turn routes only: no shared server (C2 §2 `servers`).
    assert!(set.servers().is_empty());
}
