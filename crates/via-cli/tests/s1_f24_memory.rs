//! Task 4 design §5.1 (A43) through the real `via` binary and daemon: the
//! F24 memory gate. Every kind of holder is driven towards its maximum at
//! once: four running turns whose 16 MiB prompts are dispatched, three of
//! them flooding maximal vendor messages, and every other C1 socket
//! sending maximal request lines (1 MiB with a 65,000-node list) that read
//! event pages. The daemon's RSS is sampled every 10 ms from
//! `/proc/<pid>/status`. Written before the bounded `events` page.
#![cfg(feature = "test-failpoints")]

#[path = "support/daemon.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod daemon;
#[path = "support/failpoints.rs"]
#[expect(dead_code, reason = "shared support; this file uses part of it")]
mod failpoints;
#[path = "support/hits.rs"]
mod hits;
#[path = "support/outer_cleanup.rs"]
mod outer_cleanup;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use daemon::{Daemon, Raw, Sandbox, TestResult, cli, failure, infra, request};
use failpoints::Failpoints;
use scenario::{ScenarioError, collect_available, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const MIB: u64 = 1024 * 1024;
/// A dispatched prompt at `PROMPT_MAX` (design §5.2).
const PROMPT: usize = 16 * 1024 * 1024;
/// Design §5.1's sum over holders, and the gate's 25% margin.
const SUM_MIB: u64 = 332;
/// Maximal vendor messages per flooding turn, in chunks: three turns flood
/// 282 MiB, so the sampled total clears 256 MiB even when the 10 ms sampler
/// misses each fake's last chunk and overflow line (4 MiB each).
const CHUNKS: u64 = 47;
/// Maximal messages per chunk: 2 MiB, within Wire's 4 MiB queue (§8.2).
const CHUNK: u64 = 2;
/// Core's hit before it handles each observation, counted to pace the
/// flood.
const OBSERVED: &str = "core.observations.pause";
/// Sockets sending maximal lines; three more carry the controls (32).
const FLOOD_SOCKETS: usize = 29;
/// Maximal lines each socket has answered before the vendor flood starts.
const WARM_LINES: u64 = 16;
/// A control's reply bound (design §13.2, A52): a starved control fails it;
/// each round's slowest reply is recorded against the 100 ms target.
const CONTROL: Duration = Duration::from_secs(1);

fn check(condition: bool, detail: impl FnOnce() -> String) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(failure(detail()))
    }
}

/// One vendor stdout line of `bytes` bytes, LF included: a `text` message
/// whose skipped `text` member fills it.
fn maximal_text(bytes: usize) -> String {
    let head = r#"{"type":"text","vendor_turn_id":"fake-turn-1","text":""#;
    let tail = "\"}\n";
    format!(
        "{head}{}{tail}",
        "x".repeat(bytes - head.len() - tail.len())
    )
}

/// The held turn's fixture: the fake holds its stdin at gate `held` before
/// it reads the start, then acknowledges its cancellation. A fake reads the
/// fixture when it launches, so each turn is launched under its own.
fn held_fixture() -> Value {
    json!({"expected_request":{"type":"start","id":1,"turn":1},"steps":[
        {"action":"hold_stdin","name":"held"},
        {"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}},
        {"action":"expect_request","expected":{"type":"interrupt"}},
        {"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1",
            "status":"interrupted","final_text":"","stop_reason":"interrupted"}},
    ]})
}

/// Flooding turn `index`'s fixture: gate `f<index>_0` after its start,
/// then its acceptance, then chunks of [`CHUNK`] maximal messages, each
/// after gate `f<index>_<k>`, and a line over the 1 MiB cap, so the turn
/// fails `overflow`. Wire's queue overflows by design when a vendor writes
/// faster than VIA consumes (§8.2), so the test paces the chunks.
fn flood_fixture(index: usize) -> Value {
    let text = maximal_text(usize::try_from(MIB).unwrap_or(0) - 16);
    let mut steps = vec![
        json!({"action":"gate","name":format!("f{index}_0")}),
        json!({"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}}),
        json!({"action":"flood","text":text,"count":CHUNK}),
    ];
    for chunk in 1..CHUNKS {
        steps.push(json!({"action":"gate","name":format!("f{index}_{chunk}")}));
        steps.push(json!({"action":"flood","text":text,"count":CHUNK}));
    }
    steps.push(json!({"action":"emit_raw","text":format!("{}\n", "h".repeat(2 * 1024 * 1024))}));
    steps.push(json!({"action":"hang"}));
    json!({"expected_request":{"type":"start","id":1,"turn":1},"steps":steps})
}

/// Releases the flooding turns' chunks: chunk `k` of every turn once Core
/// has handled every observation of the chunks before it (Core's hits of
/// [`OBSERVED`]), so Wire's queue never holds more than two chunks.
fn pace(sandbox: &Sandbox, dir: &Path, turns: usize) -> Result<(), ScenarioError> {
    let turns_u64 = u64::try_from(turns).map_err(infra)?;
    let mut expected = 0;
    for chunk in 0..CHUNKS {
        let deadline = Instant::now() + Duration::from_secs(60);
        while hits::hits(dir, OBSERVED).map_err(infra)? < expected {
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout(format!(
                    "Core did not handle chunk {chunk}'s predecessors"
                )));
            }
            thread::sleep(Duration::from_millis(2));
        }
        for index in 0..turns {
            let gate = format!("f{index}_{chunk}");
            sandbox.await_gate(&gate)?;
            sandbox.release_gate(&gate)?;
        }
        // The chunk's messages, and the acceptance before the first.
        expected += turns_u64 * (CHUNK + u64::from(chunk == 0));
    }
    Ok(())
}

/// Replaces the sandbox's fixture for the next fake launched.
fn use_fixture(sandbox: &Sandbox, fixture: &Value) -> Result<(), ScenarioError> {
    let next = sandbox.fixture.with_extension("next");
    fs::write(&next, fixture.to_string()).map_err(infra)?;
    fs::rename(&next, &sandbox.fixture).map_err(infra)
}

/// A maximal `events` request line: 65,000 type names (1 MiB less a few
/// hundred bytes, under the 65,536-node limit) for one session.
fn maximal_events(id: u64, session: &str) -> String {
    let mut types = Vec::with_capacity(65_000);
    types.extend(std::iter::repeat_n("session.reopened", 33_000));
    types.extend(std::iter::repeat_n("turn.ended", 32_000));
    let line = request(id, "events", &json!({"session":session,"types":types}));
    assert!(line.len() < 1024 * 1024, "{} bytes", line.len());
    line
}

/// A `/proc/<pid>/status` field in KiB.
fn status_kib(pid: u32, field: &str) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
}

/// Bytes a process wrote (`/proc/<pid>/io` `wchar`).
fn written(pid: u32) -> Option<u64> {
    let io = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    io.lines()
        .find_map(|line| line.strip_prefix("wchar:"))
        .and_then(|rest| rest.trim().parse().ok())
}

/// The pids whose executable is `exe`.
fn processes_of(exe: &Path) -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|target| target == exe))
        .collect()
}

/// Whether `pid` is a `via` anchor process (`via __via_host_anchor …`).
fn is_anchor(pid: u32) -> bool {
    fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
        cmdline
            .split(|byte| *byte == 0)
            .nth(1)
            .is_some_and(|arg| arg == b"__via_host_anchor")
    })
}

/// One 10 ms sample: the daemon's RSS and the bytes the fakes had written.
#[derive(Clone, Copy)]
struct Sample {
    rss_kib: u64,
    flooded: u64,
}

/// What the sampler saw.
#[derive(Default)]
struct Samples {
    daemon: Vec<Sample>,
    /// Peak RSS (`VmHWM`) per anchor pid.
    anchors: HashMap<u32, u64>,
}

/// Samples the daemon's RSS every 10 ms, and the fakes' written bytes and
/// the anchors' peak RSS, until `stop`.
fn sampler(
    daemon: u32,
    fake: PathBuf,
    via: PathBuf,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<Samples> {
    thread::spawn(move || {
        let mut samples = Samples::default();
        let mut fakes: HashMap<u32, u64> = HashMap::new();
        let mut anchors: Vec<u32> = Vec::new();
        let mut refreshed: Option<Instant> = None;
        while !stop.load(Ordering::Acquire) {
            if refreshed.is_none_or(|at| at.elapsed() >= Duration::from_millis(200)) {
                refreshed = Some(Instant::now());
                for pid in processes_of(&fake) {
                    fakes.entry(pid).or_insert(0);
                }
                anchors = processes_of(&via)
                    .into_iter()
                    .filter(|pid| *pid != daemon && is_anchor(*pid))
                    .collect();
            }
            for (pid, bytes) in &mut fakes {
                if let Some(now) = written(*pid) {
                    *bytes = (*bytes).max(now);
                }
            }
            for pid in &anchors {
                if let Some(peak) = status_kib(*pid, "VmHWM:") {
                    let seen = samples.anchors.entry(*pid).or_insert(0);
                    *seen = (*seen).max(peak);
                }
            }
            if let Some(rss_kib) = status_kib(daemon, "VmRSS:") {
                samples.daemon.push(Sample {
                    rss_kib,
                    flooded: fakes.values().sum(),
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
        samples
    })
}

/// Sends maximal `events` lines on one socket until `stop`; the replies
/// must be pages or a full Public lane's refusal.
fn flood_socket(
    sandbox: &Sandbox,
    session: &str,
    (answered, stop): (&AtomicU64, &AtomicBool),
) -> Result<u64, ScenarioError> {
    let mut raw = Raw::open(sandbox)?;
    let mut sent = 0;
    while !stop.load(Ordering::Acquire) {
        sent += 1;
        let reply = raw.exchange(&maximal_events(sent, session))?;
        let paged = reply["result"]["events"].is_array();
        let refused = reply["error"]["data"]["kind"] == "admission_refused";
        if !(paged || refused) {
            return Err(failure(format!("maximal events line: {reply}")));
        }
        answered.fetch_add(1, Ordering::AcqRel);
    }
    Ok(sent)
}

/// The three controls, each timed on its own socket: `daemon/status`,
/// `status` of `observed`, and `cancel` of the held turn.
struct Controls {
    daemon: Raw,
    status: Raw,
    cancel: Raw,
    id: u64,
    slowest: Duration,
}

impl Controls {
    fn open(sandbox: &Sandbox) -> Result<Self, ScenarioError> {
        Ok(Self {
            daemon: Raw::open(sandbox)?,
            status: Raw::open(sandbox)?,
            cancel: Raw::open(sandbox)?,
            id: 100,
            slowest: Duration::ZERO,
        })
    }

    fn timed(
        &mut self,
        which: usize,
        method: &str,
        params: &Value,
    ) -> Result<Value, ScenarioError> {
        self.id += 1;
        let line = request(self.id, method, params);
        let raw = match which {
            0 => &mut self.daemon,
            1 => &mut self.status,
            _ => &mut self.cancel,
        };
        let started = Instant::now();
        let reply = raw.exchange(&line)?;
        let took = started.elapsed();
        self.slowest = self.slowest.max(took);
        check(took <= CONTROL, || {
            format!("{method} answered in {took:?}: {reply}")
        })?;
        Ok(reply)
    }

    /// One round of the three controls; `status` of `observed`.
    fn round(&mut self, observed: &str, held: &str) -> Result<Value, ScenarioError> {
        self.timed(0, "daemon/status", &json!({}))?;
        let status = self.timed(1, "status", &json!({"session":observed}))?;
        self.timed(
            2,
            "cancel",
            &json!({"session":held,"handle":HANDLE,"force_after_ms":60_000}),
        )?;
        Ok(status)
    }
}

fn spawn_file(
    sandbox: &Sandbox,
    evidence: &Evidence,
    name: &str,
    prompt_file: &Path,
) -> Result<String, ScenarioError> {
    let path = prompt_file
        .to_str()
        .ok_or_else(|| infra("prompt path is not UTF-8"))?;
    let receipt = cli(
        sandbox,
        evidence,
        &format!("spawn_{name}"),
        &[
            "spawn",
            "--harness",
            "fake",
            "--model",
            "fake",
            "--prompt-file",
            path,
            "--handle",
            HANDLE,
            "--wall-ms",
            "300000",
            "--background",
            "--json",
        ],
    )?;
    receipt["session_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| failure(format!("receipt has no session: {receipt}")))
}

/// Design §5.1, §13.2 [t4r16.2, t4r16.5.6] (A43): with every holder driven
/// at once, the daemon's peak RSS less its idle baseline stays within
/// 1.25 × the §5.1 sum; RSS grows by less than 32 MiB after the first
/// 64 MiB of the 256 MiB flood; each anchor stays within 32 MiB;
/// `daemon/status`, `status` and `cancel` of another turn answer within
/// 100 ms, also while that turn's stdin is held with its interrupt behind
/// the start (`hold_stdin`); each flooding turn ends `failed(overflow)`.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "every holder is driven at once in one scenario"
)]
fn s1_f24_flood_fails_overflow_with_bounded_rss_and_prompt_control() -> TestResult {
    let sandbox = Sandbox::new(&held_fixture())?;
    let root = sandbox.state.parent().ok_or("no sandbox root")?.to_owned();
    let failpoints = Failpoints::new(&root)?;
    let dir = root.join("failpoints");
    hits::count(&dir, OBSERVED)?;
    let prompt_file = root.join("maximal.prompt");
    fs::write(&prompt_file, "p".repeat(PROMPT))?;
    let fake = fs::canonicalize(&sandbox.fake)?;
    let via = fs::canonicalize(&sandbox.via)?;
    let evidence = Evidence::new("s1_f24_flood_rss", &sandbox.fake, &sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let daemon = Daemon::start_with(&sandbox, evidence, |command| {
                failpoints.activate(command);
            })?;
            // The child's own pid, which readiness confirmed serves the socket.
            let pid = daemon.pid();
            // Elapsed time only: the daemon settles before its baseline.
            thread::sleep(Duration::from_millis(300));
            let baseline = status_kib(pid, "VmRSS:").ok_or_else(|| infra("no daemon VmRSS"))?;
            let stop_sampling = Arc::new(AtomicBool::new(false));
            let sampling = sampler(pid, fake.clone(), via.clone(), Arc::clone(&stop_sampling));

            // Each fake reads its fixture at launch: the held turn's, then
            // one per flooding turn, each waiting at its own gate.
            let held = spawn_file(&sandbox, evidence, "held", &prompt_file)?;
            sandbox.await_gate("held")?;
            let mut flooding = Vec::new();
            for index in 0..3 {
                use_fixture(&sandbox, &flood_fixture(index))?;
                let name = format!("flood_{index}");
                flooding.push(spawn_file(&sandbox, evidence, &name, &prompt_file)?);
                sandbox.await_gate(&format!("f{index}_0"))?;
            }
            let mut controls = Controls::open(&sandbox)?;
            controls.round(&held, &held)?;
            let held_round = controls.slowest;

            let stop_flood = AtomicBool::new(false);
            let answered = AtomicU64::new(0);
            let outcome = thread::scope(|scope| -> Result<(u64, Duration), ScenarioError> {
                let sockets: Vec<_> = (0..FLOOD_SOCKETS)
                    .map(|_| {
                        scope.spawn(|| {
                            flood_socket(&sandbox, &flooding[0], (&answered, &stop_flood))
                        })
                    })
                    .collect();
                // The socket load reaches its steady state before the flood,
                // so RSS growth during the flood is the flood's.
                let warm = u64::try_from(FLOOD_SOCKETS).map_err(infra)? * WARM_LINES;
                let deadline = Instant::now() + Duration::from_secs(60);
                while answered.load(Ordering::Acquire) < warm {
                    if Instant::now() >= deadline {
                        stop_flood.store(true, Ordering::Release);
                        return Err(ScenarioError::Timeout(
                            "the socket load did not warm".to_owned(),
                        ));
                    }
                    controls.round(&held, &held)?;
                    thread::sleep(Duration::from_millis(20));
                }
                sandbox.release_gate("held")?;
                let pacing = scope.spawn(|| pace(&sandbox, &dir, flooding.len()));
                let deadline = Instant::now() + Duration::from_secs(120);
                let result = loop {
                    let mut all_ended = true;
                    for session in &flooding {
                        let status = controls.round(session, &held)?;
                        let running = status["result"]["turns"].as_array().is_some_and(|turns| {
                            turns.iter().any(|turn| turn["state"] == "running")
                        });
                        all_ended &= !running;
                    }
                    if all_ended {
                        break Ok(controls.slowest);
                    }
                    if Instant::now() >= deadline {
                        break Err(ScenarioError::Timeout(
                            "the flooding turns did not end".to_owned(),
                        ));
                    }
                    thread::sleep(Duration::from_millis(100));
                };
                stop_flood.store(true, Ordering::Release);
                let mut lines = 0;
                for socket in sockets {
                    lines += socket
                        .join()
                        .map_err(|_| infra("a flood socket panicked"))??;
                }
                pacing
                    .join()
                    .map_err(|_| infra("the pacing thread panicked"))??;
                result.map(|slowest| (lines, slowest))
            });
            stop_sampling.store(true, Ordering::Release);
            let samples = sampling.join().map_err(|_| infra("the sampler panicked"))?;
            let (lines, slowest) = outcome?;

            let peak_hwm = status_kib(pid, "VmHWM:").unwrap_or(0);
            let peak_sampled = samples
                .daemon
                .iter()
                .map(|sample| sample.rss_kib)
                .max()
                .unwrap_or(0);
            let peak = peak_hwm.max(peak_sampled);
            let first = 64 * MIB;
            // Recorded only: the sample at 64 MiB flooded sits in a transient
            // dip (11 to 16 MiB below the level around it), so it is not the
            // level the flood has reached.
            let at_first = samples
                .daemon
                .iter()
                .rev()
                .find(|sample| sample.flooded < first)
                .map_or(baseline, |sample| sample.rss_kib);
            // Growth is peak to peak, the dip excluded: the highest sampled
            // RSS with 32 MiB <= flooded < 64 MiB (the idle baseline if that
            // window has no sample) against the highest sampled RSS from
            // 64 MiB on. Sampled, not VmHWM; the peak check keeps VmHWM.
            let level = samples
                .daemon
                .iter()
                .filter(|sample| (first / 2..first).contains(&sample.flooded))
                .map(|sample| sample.rss_kib)
                .max()
                .unwrap_or(baseline);
            let after = samples
                .daemon
                .iter()
                .filter(|sample| sample.flooded >= first)
                .map(|sample| sample.rss_kib)
                .max()
                .unwrap_or(0);
            let flooded = samples
                .daemon
                .iter()
                .map(|sample| sample.flooded)
                .max()
                .unwrap_or(0);
            let anchor_peak = samples.anchors.values().copied().max().unwrap_or(0);
            let metrics = json!({
                "baseline_kib": baseline, "peak_kib": peak, "peak_hwm_kib": peak_hwm,
                "peak_sampled_kib": peak_sampled, "samples": samples.daemon.len(),
                "limit_kib": SUM_MIB * 1024 * 5 / 4, "rss_at_64_mib_kib": at_first,
                "level_before_64_mib_kib": level, "rss_after_64_mib_kib": after,
                "flooded_bytes": flooded, "anchors": samples.anchors.len(),
                "anchor_peak_kib": anchor_peak,
                "maximal_lines": lines, "slowest_control_ms": slowest.as_millis(),
                "slowest_control_held_ms": held_round.as_millis(),
            });
            evidence
                .write("rss.json", metrics.to_string().as_bytes())
                .map_err(infra)?;
            let mut series = String::new();
            for sample in &samples.daemon {
                let _ = writeln!(series, "{} {}", sample.flooded, sample.rss_kib);
            }
            evidence
                .write("rss_by_flooded_bytes.txt", series.as_bytes())
                .map_err(infra)?;

            for session in &flooding {
                let envelope = cli(
                    &sandbox,
                    evidence,
                    &format!("result_{session}"),
                    &["result", &format!("{session}/1"), "--json"],
                )?;
                check(
                    envelope["state"] == "failed" && envelope["failure"]["class"] == "overflow",
                    || format!("flooding turn {session}: {}", envelope["failure"]),
                )?;
            }
            let envelope = cli(
                &sandbox,
                evidence,
                "wait_held",
                &[
                    "wait",
                    &format!("{held}/1"),
                    "--timeout-ms",
                    "30000",
                    "--json",
                ],
            )?;
            check(envelope["state"] == "cancelled", || {
                format!("held turn: {envelope}")
            })?;
            check(flooded >= 256 * MIB, || {
                format!("the fakes wrote {flooded} bytes")
            })?;
            check(lines > u64::try_from(FLOOD_SOCKETS).map_err(infra)?, || {
                format!("only {lines} maximal lines were sent")
            })?;
            check(
                peak.saturating_sub(baseline) <= SUM_MIB * 1024 * 5 / 4,
                || format!("peak RSS less baseline is over 1.25 × the §5.1 sum: {metrics}"),
            )?;
            check(after.saturating_sub(level) < 32 * 1024, || {
                format!("RSS grew 32 MiB or more over its level before 64 MiB flooded: {metrics}")
            })?;
            check(
                !samples.anchors.is_empty() && anchor_peak <= 32 * 1024,
                || format!("anchor RSS: {metrics}"),
            )
        },
        |evidence| collect(evidence, &sandbox),
    );
    report.require_pass()
}

/// Scenario cleanup: records every stored envelope and event as evidence,
/// then collects the Store and the evidence folders.
fn collect(evidence: &Evidence, sandbox: &Sandbox) -> Result<(), ScenarioError> {
    let path = sandbox.state.join("store.sqlite3");
    let (mut envelopes, mut events) = (String::new(), String::new());
    if path.is_file() {
        let store = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(infra)?;
        for (sql, out) in [
            (
                "SELECT envelope FROM turns WHERE envelope IS NOT NULL ORDER BY session_id,number",
                &mut envelopes,
            ),
            (
                "SELECT event FROM events ORDER BY session_id,seq",
                &mut events,
            ),
        ] {
            let mut statement = store.prepare(sql).map_err(infra)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(infra)?;
            for row in rows {
                out.push_str(&row.map_err(infra)?);
                out.push('\n');
            }
        }
    }
    evidence
        .write("envelopes.ndjson", envelopes.as_bytes())
        .map_err(infra)?;
    evidence
        .write("events.ndjson", events.as_bytes())
        .map_err(infra)?;
    collect_available(evidence, &sandbox.state)
}
