//! The shared `drive()` harness of the adapter conformance cases (adapters
//! design §6 step 6): the harness-neutral **pure half**. It installs the
//! replaying fake as the harness's configured binary, runs the case's pure
//! operations (`describe`, each `plan_checks` entry, each session's spawn
//! plan) and reports them in the checker's [`Outcome`] vocabulary, with the
//! launch log and the files those operations changed. A harness's own
//! `drive()` continues from [`Pure`] with the driver half (sessions opened,
//! turns run); a case whose every session's spawn plan refuses ends here
//! ([`Pure::refused_case`]).
//!
//! Include it beside `conformance_expect.rs`, at the test crate's root:
//! `#[path = "support/conformance_drive.rs"] mod conformance_drive;`.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::value::RawValue;
use serde_json::{Value, json};
use tempfile::TempDir;
use via_adapters::{
    AdapterConfig, AdapterSet, BootstrapEnv, Bound, DescribeRequest, ParamSizes, Refusal,
    RefusalKind, RoutePlan, RuntimeConfig, VendorOptions, VerbReq,
};
use via_store::Store;

use crate::conformance_expect::{Outcome, TurnOutcome};

/// A workspace binary beside this test executable's `deps` directory; it
/// is only resolved here, never required to exist by the pure half.
pub(crate) fn workspace_binary(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from(name), |dir| dir.join(name))
}

/// One case's deployment: the replaying fake installed as `<vendor>/<case>`
/// beside a copy of its fixture, configured as `harnesses.<harness>.binary`,
/// and an [`AdapterSet`] over a private Store and runtime directory.
pub(crate) struct Rig {
    dir: TempDir,
    name: String,
    fixtures: PathBuf,
    set: AdapterSet,
    _store: Store,
}

impl Rig {
    /// Installs case `name` of `fixtures` for `harness`.
    pub(crate) fn new(harness: &str, fixtures: &Path, name: &str) -> Result<Self, String> {
        let fail = |what: &str, error: &dyn std::fmt::Display| format!("rig {what}: {error}");
        let dir = tempfile::tempdir().map_err(|e| fail("tempdir", &e))?;
        for part in ["state", "runtime", "vendor"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(dir.path().join(part))
                .map_err(|e| fail(part, &e))?;
        }
        let vendor = dir.path().join("vendor");
        let binary = vendor.join(name);
        symlink(workspace_binary("via-fake-agent"), &binary).map_err(|e| fail("binary", &e))?;
        fs::copy(
            fixtures.join(format!("{name}.replay.json")),
            vendor.join(format!("{name}.replay.json")),
        )
        .map_err(|e| fail("replay", &e))?;
        let harnesses = RawValue::from_string(json!({harness: {"binary": binary}}).to_string())
            .map_err(|e| fail("harnesses", &e))?;
        let config = AdapterConfig::load(
            BootstrapEnv::from_vars::<_, &str, &str>([]),
            Some(&harnesses),
        )
        .map_err(|e| fail("config", &e))?;
        let store = Store::open(&dir.path().join("state")).map_err(|e| fail("store", &e))?;
        let set = AdapterSet::new(
            config,
            RuntimeConfig {
                anchor_binary: workspace_binary("via"),
                anchor_dir: dir.path().join("runtime"),
            },
            store.runtime_resources(),
        )
        .map_err(|e| fail("adapters", &e))?;
        Ok(Self {
            dir,
            name: name.to_owned(),
            fixtures: fixtures.to_path_buf(),
            set,
            _store: store,
        })
    }

    /// The adapters under test.
    pub(crate) fn set(&self) -> &AdapterSet {
        &self.set
    }

    /// The directory the fake runs from, its fixture copy and logs beside it.
    pub(crate) fn vendor_dir(&self) -> PathBuf {
        self.dir.path().join("vendor")
    }

    /// The launch log: one line per start of the replaying fake.
    pub(crate) fn launch_log(&self) -> PathBuf {
        self.vendor_dir().join(format!("{}.launches", self.name))
    }

    /// The launches so far: the launch log's lines, 0 with no log.
    pub(crate) fn launches(&self) -> u64 {
        fs::read_to_string(self.launch_log()).map_or(0, |log| log.lines().count() as u64)
    }

    /// Every file under the case's fixture and vendor directories, the
    /// launch log excepted, with its size and modification time.
    fn snapshot(&self) -> BTreeMap<PathBuf, (u64, Option<SystemTime>)> {
        let mut files = BTreeMap::new();
        for root in [self.fixtures.clone(), self.vendor_dir()] {
            list(&root, &mut files);
        }
        files.remove(&self.launch_log());
        files
    }
}

fn list(dir: &Path, into: &mut BTreeMap<PathBuf, (u64, Option<SystemTime>)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            list(&path, into);
        } else {
            into.insert(path, (meta.len(), meta.modified().ok()));
        }
    }
}

/// The files that differ between two snapshots, as display paths.
fn changed(
    before: &BTreeMap<PathBuf, (u64, Option<SystemTime>)>,
    after: &BTreeMap<PathBuf, (u64, Option<SystemTime>)>,
) -> Vec<String> {
    let mut paths: Vec<&PathBuf> = before.keys().chain(after.keys()).collect();
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .map(|path| path.display().to_string())
        .collect()
}

/// A refusal in the checker's vocabulary: the C2 kind, with its field or
/// verb after a colon where it has one.
pub(crate) fn refusal_code(refusal: &Refusal) -> String {
    match &refusal.kind {
        RefusalKind::InvalidParam { field } => format!("invalid_param:{field}"),
        RefusalKind::MissingCapability { verb } => format!("missing_capability:{}", verb.as_str()),
        RefusalKind::UnsupportedVerb => "unsupported_verb".to_owned(),
        RefusalKind::BoundUnsupported => "bound_unsupported".to_owned(),
        RefusalKind::HarnessUnavailable => "harness_unavailable".to_owned(),
        RefusalKind::UnknownModel => "unknown_model".to_owned(),
        RefusalKind::VersionRefused => "version_refused".to_owned(),
        RefusalKind::VendorOptionConflict => "vendor_option_conflict".to_owned(),
    }
}

/// A plan's first refusal, the way Core refuses a request on it.
fn first_refusal(planned: &Result<RoutePlan, Refusal>) -> Option<String> {
    match planned {
        Ok(plan) => plan.refusals.first().map(refusal_code),
        Err(refusal) => Some(refusal_code(refusal)),
    }
}

/// The encoded sizes Core fills (C2 §2 `ParamSizes`): the instructions'
/// UTF-8 bytes and the schema's compact JSON bytes.
pub(crate) fn param_sizes(instructions: &Value, schema: &Value) -> ParamSizes {
    ParamSizes {
        instructions: instructions.as_str().map_or(0, str::len),
        output_schema: if schema.is_null() {
            0
        } else {
            schema.to_string().len()
        },
    }
}

fn parsed<T: serde::de::DeserializeOwned>(value: &Value, what: &str) -> Result<T, String> {
    serde_json::from_value(value.clone()).map_err(|e| format!("{what}: {e}"))
}

/// Session `session`'s spawn as `plan` sees it, with turn `turn`'s
/// parameters (`expect.turns[i].params`).
pub(crate) fn spawn_request(
    harness: &str,
    session: &Value,
    params: &Value,
) -> Result<DescribeRequest, String> {
    let bound: Option<Bound> = parsed(&params["bound"], "params.bound")?;
    let vendor: VendorOptions = if session["vendor_options"].is_null() {
        VendorOptions::new()
    } else {
        parsed(&session["vendor_options"], "vendor_options")?
    };
    Ok(DescribeRequest {
        harness: Some(harness.to_owned()),
        model: session["model"].as_str().map(str::to_owned),
        effort: params["effort"].as_str().map(str::to_owned),
        bound,
        require: Vec::new(),
        vendor,
        cwd: session["cwd"].as_str().map(PathBuf::from),
        allow_untested: false,
        sizes: param_sizes(&session["instructions"], &params["output_schema"]),
    })
}

/// C1 §3.1 `describe` params as `plan` takes them.
fn describe_request(params: &Value) -> Result<DescribeRequest, String> {
    let require = params["require"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| {
            entry
                .as_str()
                .and_then(VerbReq::parse)
                .ok_or_else(|| format!("describe.params.require: {entry}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DescribeRequest {
        harness: params["harness"].as_str().map(str::to_owned),
        model: params["model"].as_str().map(str::to_owned),
        bound: parsed(&params["bound"], "describe.params.bound")?,
        require,
        vendor: if params["vendor"].is_null() {
            VendorOptions::new()
        } else {
            parsed(&params["vendor"], "describe.params.vendor")?
        },
        cwd: params["cwd"].as_str().map(PathBuf::from),
        allow_untested: params["allow_untested"].as_bool().unwrap_or(false),
        ..DescribeRequest::default()
    })
}

/// What the pure half established.
pub(crate) struct Pure {
    /// `describe`, `plan_checks`, `pure_writes` and
    /// `launch_checkpoints.after_pure` filled; the rest default.
    pub(crate) outcome: Outcome,
    /// Each session's spawn plan, by label: the plan, or the code of the
    /// refusal Core would answer the spawn with.
    pub(crate) spawns: BTreeMap<String, Result<RoutePlan, String>>,
}

/// Runs the case's pure operations in the checker's order: `describe`,
/// each `plan_checks` entry (a plan of the case's harness with that
/// `require`), then each session's spawn plan with its first turn's
/// parameters. None may launch or write: the launch log and the files
/// changed are part of the outcome.
pub(crate) fn pure(rig: &Rig, expect: &Value) -> Result<Pure, String> {
    let harness = expect["harness"].as_str().ok_or("case.harness")?;
    let before = rig.snapshot();
    let mut outcome = Outcome::default();
    if let Some(describe) = expect.get("describe") {
        let launched = rig.launches();
        let plan = rig
            .set()
            .plan(&describe_request(&describe["params"])?)
            .map_err(|refusal| format!("describe refused: {}", refusal_code(&refusal)))?;
        outcome.describe = Some(json!({
            "capabilities": plan.capabilities,
            "vendor_version": plan.vendor_version,
            "version_status": plan.version_status,
            "launches": rig.launches() - launched,
        }));
    }
    for check in expect["plan_checks"].as_array().into_iter().flatten() {
        let require = check["require"]
            .as_str()
            .and_then(VerbReq::parse)
            .ok_or_else(|| format!("plan_checks.require: {}", check["require"]))?;
        let request = DescribeRequest {
            harness: Some(harness.to_owned()),
            require: vec![require],
            ..DescribeRequest::default()
        };
        outcome
            .plan_checks
            .push(first_refusal(&rig.set().plan(&request)));
    }
    let mut spawns = BTreeMap::new();
    let sessions = expect["sessions"].as_object().ok_or("case.sessions")?;
    for (label, session) in sessions {
        let first = expect["turns"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|turn| turn["session"].as_str().unwrap_or("main") == label)
            .ok_or_else(|| format!("session {label} has no turn"))?;
        let planned = rig
            .set()
            .plan(&spawn_request(harness, session, &first["params"])?);
        let spawn = match first_refusal(&planned) {
            Some(code) => Err(code),
            None => planned.map_err(|refusal| refusal_code(&refusal)),
        };
        spawns.insert(label.clone(), spawn);
    }
    outcome.pure_writes = changed(&before, &rig.snapshot());
    outcome.checkpoints.after_pure = rig.launches();
    Ok(Pure { outcome, spawns })
}

impl Pure {
    /// The whole outcome of a case whose every session's spawn plan
    /// refused: nothing opens, and each session's one turn is its plan
    /// refusal. A case with a planned session needs the driver half.
    pub(crate) fn refused_case(self, rig: &Rig, expect: &Value) -> Result<Outcome, String> {
        let Self {
            mut outcome,
            spawns,
        } = self;
        if let Some((label, _)) = spawns.iter().find(|(_, spawn)| spawn.is_ok()) {
            return Err(format!(
                "session {label} planned: its turns need the driver half"
            ));
        }
        for label in spawns.keys() {
            outcome
                .checkpoints
                .after_open
                .insert(label.clone(), rig.launches());
        }
        let mut seen = Vec::new();
        for turn in expect["turns"].as_array().into_iter().flatten() {
            let label = turn["session"].as_str().unwrap_or("main");
            if seen.contains(&label) {
                return Err(format!("session {label}: a refused spawn has one turn"));
            }
            seen.push(label);
            let code = spawns
                .get(label)
                .and_then(|spawn| spawn.as_ref().err())
                .ok_or_else(|| format!("turn of unknown session {label}"))?;
            outcome.turns.push(TurnOutcome {
                plan_refusal: Some(code.clone()),
                ..TurnOutcome::default()
            });
            outcome.checkpoints.after_turn.push(rig.launches());
        }
        outcome.launches = rig.launches();
        Ok(outcome)
    }
}
