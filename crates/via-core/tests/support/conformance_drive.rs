//! The shared `drive()` harness's pure half (x.3.2 plan, C1 and X1): the
//! steps of a conformance case that start nothing, over the real
//! [`AdapterSet`] of one vendor harness.
//!
//! The harness binary is the replaying fake: `via-fake-agent` linked as
//! `<case dir>/<case>` beside a copy of `<case>.replay.json`, so its launch
//! log `<case>.launches` (one line per start) lands in the case's own scratch
//! directory and every case starts with none. The Store and the runtime
//! directory live in a separate state directory; `pure_writes` scans it
//! too, beside the fixture and case directories.
//!
//! In the checker's order (see `conformance_expect.rs`, `drive()`
//! obligations), [`Pure::run`]:
//! 1. answers `describe` with its `params`, and each `plan_checks` entry with
//!    one plan of the case's harness, the first session's model and cwd,
//!    and that `require`;
//! 2. plans each session's spawn at its logical open, in label order, as
//!    Core's spawn intake does: `plan`, its first refusal, then `check_turn`
//!    of the turn-1 values; a refusal is that turn's `plan_refusal`, in the
//!    C2 name of its `RefusalKind`;
//! 3. records `launches`, the checkpoints and `pure_writes` from the case's
//!    directories; `pure_writes` spans steps 1 and 2, and a directory that
//!    cannot be read fails the case.
//!
//! The run half performs each planned session's real `open_session()`; it
//! samples that session's `after_open` right after it and extends the
//! `pure_writes` interval past every open (review r1 #6).
//!
//! The turns that plan run in the run half (the route's driver), which
//! [`Pure::planned_only`] does not have: it finishes only a case whose
//! every turn was refused before any receipt.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Map, Value};
use via_adapters::{
    AdapterConfig, AdapterSet, BootstrapEnv, Bound, DescribeRequest, ParamSizes, Refusal,
    RefusalKind, RoutePlan, RuntimeConfig, SessionRef, TurnParams, VendorOptions, VerbReq,
};
use via_store::Store;

use crate::conformance_expect::{Outcome, TurnOutcome};

/// A file's size and modification time: what `pure_writes` compares.
type Listing = BTreeMap<PathBuf, (u64, SystemTime)>;

/// The pure steps of one case, done, with what the run half needs.
pub(crate) struct Pure {
    /// The outcome so far: launches, checkpoints, `pure_writes`,
    /// `plan_checks`, `describe`, and the turns refused before any receipt.
    pub(crate) outcome: Outcome,
    /// Each opened session's spawn plan, by label.
    pub(crate) plans: BTreeMap<String, RoutePlan>,
    /// The turns the run half still runs, by index.
    pub(crate) pending: Vec<usize>,
    /// The case's scratch directory: the replay copy, the linked fake and
    /// its launch log.
    pub(crate) case_dir: tempfile::TempDir,
    /// The adapter set under test.
    pub(crate) set: AdapterSet,
    /// The Store's and the runtime directory's parent.
    pub(crate) state: tempfile::TempDir,
    /// The Store the adapter set runs on; the run half commits turns to it
    /// and reads the server anchors' pids from it.
    pub(crate) store: Store,
    pub(crate) name: String,
    /// Each run turn's decode fence once it settled, by index: Route's
    /// decode watermark and the position the Adapter delivered through
    /// (runtime §8; x.3.2 critical r2 #2).
    pub(crate) fences: std::cell::RefCell<BTreeMap<usize, (u64, u64)>>,
    /// Each late observation (C1 §6.1 AD4, as Core attributes it): one
    /// naming the vendor turn an earlier turn of its session accepted,
    /// with that turn's index, in its checker shape.
    pub(crate) late: std::cell::RefCell<Vec<(usize, Value)>>,
    /// At each gate, in the order taken: the turn's decode watermark and
    /// the position the Adapter delivered through.
    pub(crate) gate_fences: std::cell::RefCell<Vec<(u64, u64)>>,
    /// The fixture directory, listed with the case and state directories.
    fixtures: PathBuf,
    /// The listing before the pure steps: where `pure_writes` starts.
    before: Listing,
}

impl Pure {
    /// Runs the pure steps of case `name` of `harness` against the fixture
    /// `replay`.
    pub(crate) fn run(
        harness: &str,
        name: &str,
        expect: &Value,
        replay: &Path,
    ) -> Result<Self, String> {
        let fixtures = replay
            .parent()
            .ok_or("the replay fixture has no directory")?
            .to_path_buf();
        let case_dir = tempfile::tempdir().map_err(|e| format!("case dir: {e}"))?;
        let state = tempfile::tempdir().map_err(|e| format!("state dir: {e}"))?;
        fs::copy(replay, case_dir.path().join(format!("{name}.replay.json")))
            .map_err(|e| format!("replay copy: {e}"))?;
        let binary = case_dir.path().join(name);
        std::os::unix::fs::symlink(fake_agent()?, &binary)
            .map_err(|e| format!("fake link: {e}"))?;
        let (set, store) = adapter_set(harness, &binary, state.path())?;
        let mut pure = Self {
            outcome: Outcome::default(),
            plans: BTreeMap::new(),
            pending: Vec::new(),
            case_dir,
            set,
            state,
            store,
            name: name.to_owned(),
            before: Listing::new(),
            fences: std::cell::RefCell::default(),
            late: std::cell::RefCell::default(),
            gate_fences: std::cell::RefCell::default(),
            fixtures,
        };
        pure.before = pure.listing()?;
        pure.pure_operations(harness, expect)?;
        pure.outcome.checkpoints.after_pure = pure.launches()?;
        // The spawn plans are pure too: the interval covers them.
        pure.open_sessions(harness, expect)?;
        pure.outcome.pure_writes = pure.writes()?;
        Ok(pure)
    }

    /// The outcome of a case none of whose turns runs: each was refused
    /// before any receipt. Otherwise the turns that run need the run half.
    pub(crate) fn planned_only(mut self) -> Result<Outcome, String> {
        if let Some(turn) = self.pending.first() {
            return Err(format!(
                "case {}: turn {turn} plans, and running it needs the route's driver",
                self.name
            ));
        }
        let launches = self.launches()?;
        for _ in 0..self.outcome.turns.len() {
            self.outcome.checkpoints.after_turn.push(launches);
        }
        self.outcome.launches = launches;
        Ok(self.outcome)
    }

    /// `describe`, then each `plan_checks` entry.
    fn pure_operations(&mut self, harness: &str, expect: &Value) -> Result<(), String> {
        if let Some(describe) = expect.get("describe") {
            let before = self.launches()?;
            let request = describe_request(&describe["params"])?;
            let plan = self
                .set
                .plan(&request)
                .map_err(|refusal| format!("describe refused: {refusal:?}"))?;
            let plan = serde_json::to_value(&plan).map_err(|e| e.to_string())?;
            let launches = self.launches()? - before;
            self.outcome.describe = Some(serde_json::json!({
                "capabilities": plan["capabilities"],
                "vendor_version": plan["vendor_version"],
                "version_status": plan["version_status"],
                "launches": launches,
            }));
        }
        let first = expect["sessions"]
            .as_object()
            .and_then(|sessions| sessions.values().next())
            .cloned()
            .unwrap_or(Value::Null);
        for check in expect["plan_checks"].as_array().into_iter().flatten() {
            let entry = check["require"].as_str().ok_or("plan_checks: require")?;
            let request = DescribeRequest {
                harness: Some(harness.to_owned()),
                model: first["model"].as_str().map(str::to_owned),
                require: vec![VerbReq::parse(entry).ok_or("plan_checks: no verb")?],
                cwd: first["cwd"].as_str().map(PathBuf::from),
                ..DescribeRequest::default()
            };
            let refusal = match self.set.plan(&request) {
                Ok(plan) => plan.refusals.first().map(refusal_name),
                Err(refusal) => Some(refusal_name(&refusal)),
            };
            self.outcome.plan_checks.push(refusal);
        }
        Ok(())
    }

    /// Each session's logical open, in label order: its spawn plan, from
    /// its first turn's values.
    fn open_sessions(&mut self, harness: &str, expect: &Value) -> Result<(), String> {
        let turns = expect["turns"].as_array().ok_or("turns")?;
        self.outcome.turns = vec![TurnOutcome::default(); turns.len()];
        let mut refused = BTreeMap::new();
        for (label, session) in expect["sessions"].as_object().into_iter().flatten() {
            self.outcome.closes.insert(label.clone(), None);
            let first = turns
                .iter()
                .position(|turn| session_of(turn) == label)
                .ok_or_else(|| format!("session {label} has no turn"))?;
            match self.plan_spawn(harness, session, &turns[first]["params"])? {
                Ok(plan) => {
                    self.plans.insert(label.clone(), plan);
                }
                Err(name) => {
                    self.outcome.turns[first].plan_refusal = Some(name);
                    refused.insert(label.clone(), first);
                }
            }
            let launches = self.launches()?;
            self.outcome
                .checkpoints
                .after_open
                .insert(label.clone(), launches);
        }
        for (index, turn) in turns.iter().enumerate() {
            match refused.get(session_of(turn)) {
                Some(&first) if first == index => {}
                Some(_) => return Err(format!("turn {index} follows a refused spawn")),
                None => self.pending.push(index),
            }
        }
        Ok(())
    }

    /// Core's spawn intake: the plan, its first refusal, then `check_turn`
    /// of the turn-1 values. `Err` is the refusal's C2 name.
    fn plan_spawn(
        &self,
        harness: &str,
        session: &Value,
        params: &Value,
    ) -> Result<Result<RoutePlan, String>, String> {
        let bound = optional_bound(&params["bound"])?;
        let vendor = vendor_options(&session["vendor_options"])?;
        let json_len = |value: &Value| value.as_str().map_or(0, |_| value.to_string().len());
        let sizes = ParamSizes {
            instructions: session["instructions"].as_str().map_or(0, str::len),
            instructions_json: json_len(&session["instructions"]),
            output_schema: if params["output_schema"].is_null() {
                0
            } else {
                params["output_schema"].to_string().len()
            },
            prompt_json: json_len(&params["prompt"]),
            ..ParamSizes::default()
        };
        let request = DescribeRequest {
            harness: Some(harness.to_owned()),
            model: session["model"].as_str().map(str::to_owned),
            effort: params["effort"].as_str().map(str::to_owned),
            bound: bound.clone(),
            vendor: vendor.clone(),
            cwd: session["cwd"].as_str().map(PathBuf::from),
            sizes,
            ..DescribeRequest::default()
        };
        let plan = match self.set.plan(&request) {
            Ok(plan) => plan,
            Err(refusal) => return Ok(Err(refusal_name(&refusal))),
        };
        if let Some(refusal) = plan.refusals.first() {
            return Ok(Err(refusal_name(refusal)));
        }
        let session_ref = SessionRef {
            harness: plan.harness.to_owned(),
            route: plan.route.to_owned(),
            adapter_version: plan.adapter_version.clone(),
        };
        let turn = TurnParams {
            effort: request.effort.clone(),
            bound,
            output_schema: !params["output_schema"].is_null(),
            max_steps: params["max_steps"].as_u64(),
            vendor,
            sizes,
            ..TurnParams::default()
        };
        Ok(match self.set.check_turn(&session_ref, &turn) {
            Ok(_) => Ok(plan),
            Err(refusal) => Err(refusal_name(&refusal)),
        })
    }

    /// The launch log's line count: the fake's starts so far.
    pub(crate) fn launches(&self) -> Result<u64, String> {
        let log = self.case_dir.path().join(format!("{}.launches", self.name));
        match fs::read_to_string(&log) {
            Ok(text) => Ok(text.lines().count() as u64),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(format!("launch log: {error}")),
        }
    }

    /// The pids in the launch log, one per start of the fake, in the order
    /// the fake's starts appended them: launch *n* is line *n*.
    pub(crate) fn launch_pids(&self) -> Result<Vec<u32>, String> {
        let log = self.case_dir.path().join(format!("{}.launches", self.name));
        match fs::read_to_string(&log) {
            Ok(text) => text
                .lines()
                .map(|line| line.trim().parse().map_err(|e| format!("launch log: {e}")))
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(format!("launch log: {error}")),
        }
    }

    /// The files created, changed or removed since the pure steps began.
    pub(crate) fn writes(&self) -> Result<Vec<String>, String> {
        Ok(changed(&self.before, &self.listing()?))
    }

    /// Every file of the fixture, case and state directories (the Store
    /// and the runtime directory) but the launch log.
    fn listing(&self) -> Result<Listing, String> {
        let mut listing = Listing::new();
        for dir in [
            self.fixtures.as_path(),
            self.case_dir.path(),
            self.state.path(),
        ] {
            list(dir, &mut listing).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        listing.remove(&self.case_dir.path().join(format!("{}.launches", self.name)));
        Ok(listing)
    }
}

/// The C2 name of a refusal's kind, as `plan_refusal` and `plan_checks`
/// state it.
pub(crate) fn refusal_name(refusal: &Refusal) -> String {
    match &refusal.kind {
        RefusalKind::UnsupportedVerb => "unsupported_verb".to_owned(),
        RefusalKind::BoundUnsupported => "bound_unsupported".to_owned(),
        RefusalKind::HarnessUnavailable => "harness_unavailable".to_owned(),
        RefusalKind::UnknownModel => "unknown_model".to_owned(),
        RefusalKind::VersionRefused => "version_refused".to_owned(),
        RefusalKind::VendorOptionConflict { .. } => "vendor_option_conflict".to_owned(),
        RefusalKind::InvalidParam { field } => format!("invalid_param:{field}"),
        RefusalKind::MissingCapability { verb } => format!("missing_capability:{}", verb.as_str()),
    }
}

/// The label of the session a turn runs on; `main` when absent.
fn session_of(turn: &Value) -> &str {
    turn["session"].as_str().unwrap_or("main")
}

/// The built `via-fake-agent`, beside this test's directory.
fn fake_agent() -> Result<PathBuf, String> {
    sibling("via-fake-agent")
}

/// The workspace binary `name`, beside this test's directory.
fn sibling(name: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let path = exe
        .parent()
        .and_then(Path::parent)
        .ok_or("no target directory")?
        .join(name);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "missing {}; build the workspace first",
            path.display()
        ))
    }
}

/// An adapter set with `harness` pinned to `binary`, over a fresh Store.
fn adapter_set(harness: &str, binary: &Path, state: &Path) -> Result<(AdapterSet, Store), String> {
    // `vendor`: the server routes' state, as bootstrap creates it.
    for part in ["state", "runtime", "vendor"] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(state.join(part))
            .map_err(|e| format!("{part}: {e}"))?;
    }
    let mut harnesses = Map::new();
    let mut settings = serde_json::json!({ "binary": binary });
    if harness == "claude" {
        // Every Claude fixture was recorded with `--restricted` and
        // `--strict-mcp-config`: MCP servers requested off.
        settings["restricted"] = Value::Bool(true);
        settings["inherit"] = serde_json::json!({ "mcp_servers": false });
    }
    if harness == "pi" {
        // Pi's private agent directory (packet §4.2-4.3), as the owner
        // prepares it: the one required setting, nothing else.
        let agent = state.join("vendor").join("pi").join("agent");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&agent)
            .map_err(|e| format!("pi agent dir: {e}"))?;
        write_private(&agent.join("settings.json"), br#"{"cacheWarming":"off"}"#)?;
    }
    harnesses.insert(harness.to_owned(), settings);
    let raw = serde_json::value::RawValue::from_string(Value::Object(harnesses).to_string())
        .map_err(|e| e.to_string())?;
    let env = BootstrapEnv::from_vars(std::env::var_os("PATH").map(|path| ("PATH", path)));
    let config = AdapterConfig::load(env, Some(&raw)).map_err(|e| e.to_string())?;
    let store = Store::open(&state.join("state")).map_err(|e| e.to_string())?;
    let set = AdapterSet::new(
        config,
        RuntimeConfig {
            // The real anchor, when built: the run half launches through it.
            anchor_binary: sibling("via").unwrap_or_else(|_| state.join("anchor")),
            anchor_dir: state.join("runtime"),
            vendor_state_dir: state.join("vendor"),
        },
        store.runtime_resources(),
    )
    .map_err(|e| e.to_string())?;
    Ok((set, store))
}

/// Writes `bytes` to a new file at `path`, readable by its owner only.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// A `describe` request from C1 §3.1 `params`.
fn describe_request(params: &Value) -> Result<DescribeRequest, String> {
    let require = params["require"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| entry.as_str().and_then(VerbReq::parse))
        .collect::<Option<Vec<_>>>()
        .ok_or("describe.params.require: not verbs")?;
    Ok(DescribeRequest {
        harness: params["harness"].as_str().map(str::to_owned),
        model: params["model"].as_str().map(str::to_owned),
        bound: optional_bound(&params["bound"])?,
        require,
        vendor: vendor_options(&params["vendor"])?,
        cwd: params["cwd"].as_str().map(PathBuf::from),
        allow_untested: params["allow_untested"].as_bool().unwrap_or(false),
        ..DescribeRequest::default()
    })
}

/// A C1 `bound`, or none when null or absent.
fn optional_bound(value: &Value) -> Result<Option<Bound>, String> {
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|e| format!("bound: {e}"))
}

/// C1 `vendor` options, or none when null or absent.
fn vendor_options(value: &Value) -> Result<VendorOptions, String> {
    if value.is_null() {
        return Ok(VendorOptions::new());
    }
    serde_json::from_value(value.clone()).map_err(|e| format!("vendor: {e}"))
}

/// Adds every file under `dir`, recursively, to `into`.
fn list(dir: &Path, into: &mut Listing) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            list(&path, into)?;
        } else {
            into.insert(path, (metadata.len(), metadata.modified()?));
        }
    }
    Ok(())
}

/// The file names created, changed or removed between two listings.
fn changed(before: &Listing, after: &Listing) -> Vec<String> {
    let mut names: Vec<String> = before
        .iter()
        .filter(|(path, stamp)| after.get(*path) != Some(stamp))
        .map(|(path, _)| path)
        .chain(after.keys().filter(|path| !before.contains_key(*path)))
        .map(|path| path.display().to_string())
        .collect();
    names.sort();
    names.dedup();
    names
}
