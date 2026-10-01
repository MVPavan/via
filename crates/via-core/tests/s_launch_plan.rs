//! S-LAUNCH (adapter design §5.4, §5.5): `AdapterSet::plan` takes the
//! planned harness's configured `inherit`; the fake keeps the OD2 default
//! whatever the vendor harnesses configure. It lives in Core's tests since
//! `AdapterSet::new` takes Store's runtime resources.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

use serde_json::json;
use serde_json::value::RawValue;
use via_adapters::{
    AdapterConfig, AdapterSet, BootstrapEnv, DescribeRequest, Inherit, RefusalKind, RuntimeConfig,
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
    let harnesses = RawValue::from_string(
        r#"{"claude":{"inherit":{"hooks":true,"mcp_servers":true}},
            "codex":{"inherit":{"skills":false}}}"#
            .to_owned(),
    )
    .unwrap();
    let config = AdapterConfig::load(env, Some(&harnesses)).unwrap();
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
    let describe = |harness: &str| DescribeRequest {
        harness: Some(harness.to_owned()),
        ..DescribeRequest::default()
    };
    let fake = set.plan(&describe("fake")).unwrap();
    assert_eq!(fake.inherit, Inherit::OD2_DEFAULT);
    // No vendor adapter exists before x.3.2: a configured vendor harness
    // still refuses, starting nothing.
    for harness in ["claude", "codex"] {
        let refusal = set.plan(&describe(harness)).unwrap_err();
        assert_eq!(refusal.kind, RefusalKind::HarnessUnavailable, "{harness}");
    }
}
