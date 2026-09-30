//! Task 4 design §2.3, §8, §9 through the real `via` binary and daemon: the
//! observation channel's 10 s stall (F24; its item and byte bounds are
//! `crates/via-core/tests/s1_observation_budget.rs`), exact vendor messages
//! across split writes (F27), and Route servicing a cancel while its start
//! write is held (§9). Written before the Wire tasks, the bounded
//! channel and the serviceable Route. Every scenario activates the
//! failpoint controller and counts `wire.fallback_drop`: a `WireMessages`
//! dropped without `finish` would hit it, so each asserts it was never hit.
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
#[path = "support/process.rs"]
mod process;
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use daemon::{Daemon, Sandbox, TestResult, cli, collect_available, failure, infra};
use failpoints::Failpoints;
use scenario::{ScenarioError, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const PAUSE: &str = "core.observations.pause";
const FALLBACK: &str = "wire.fallback_drop";
/// The lowered stall (`VIA_TEST_EVENT_STALL_MS`).
const STALL_MS: &str = "500";
/// Runtime §5.2 cleanup allowance after a deadline with no budget left:
/// TERM, at most 2 s, then KILL and at most 1 s for exit.
const CLEANUP_ALLOWANCE: Duration = Duration::from_secs(3);
/// Explicit scheduler tolerance for the end-to-end bound over the cleanup
/// allowance: the harness's stall boundary is taken before the vendor's
/// last writes, so it also covers them and the 5 ms absence polling under
/// a loaded parallel run (a contract upper bound, design T4-A50).
const TOLERANCE: Duration = Duration::from_secs(2);

fn accepted() -> Value {
    json!({"action":"emit","message":{"type":"accepted","id":1,"vendor_turn_id":"fake-turn-1"}})
}

fn text(text: &str) -> Value {
    json!({"type":"text","vendor_turn_id":"fake-turn-1","text":text})
}

fn completed() -> Value {
    json!({"action":"emit","message":{"type":"terminal","vendor_turn_id":"fake-turn-1","status":"completed","final_text":"done","stop_reason":"end_turn"}})
}

fn script(prompt: &str, steps: &[Value]) -> Value {
    json!({"expected_request":{"type":"start","id":1,"turn":1,"prompt":prompt},"steps":steps})
}

/// One step's messages as one write: a tool round, then model output.
fn step_block() -> TestResult<String> {
    let mut block = String::new();
    for message in [
        json!({"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t","name":"shell"}),
        json!({"type":"tool_ended","vendor_turn_id":"fake-turn-1","tool_id":"t"}),
        text("next"),
    ] {
        block.push_str(&serde_json::to_string(&message)?);
        block.push('\n');
    }
    Ok(block)
}

/// Model output (Core is held on it), gate `flood`, then `first` step
/// blocks of three progress items each, gate `burst`, `second` more blocks;
/// then the vendor reports its pid and falls silent.
fn blocks_then_silence(prompt: &str, first: u64, second: u64) -> TestResult<Value> {
    let block = step_block()?;
    Ok(script(
        prompt,
        &[
            accepted(),
            json!({"action":"emit","message":text("first")}),
            json!({"action":"gate","name":"flood"}),
            json!({"action":"flood","text":block,"count":first}),
            json!({"action":"gate","name":"burst"}),
            json!({"action":"flood","text":block,"count":second}),
            json!({"action":"report_pids"}),
            json!({"action":"hang"}),
        ],
    ))
}

fn check(condition: bool, detail: impl FnOnce() -> String) -> Result<(), ScenarioError> {
    if condition {
        Ok(())
    } else {
        Err(failure(detail()))
    }
}

/// One scenario's deployment: the sandbox and its failpoint directory, with
/// `wire.fallback_drop` counted from the daemon's start.
struct Setup {
    sandbox: Sandbox,
    failpoints: Failpoints,
    dir: PathBuf,
}

impl Setup {
    fn new(fixture: &Value) -> TestResult<Self> {
        let sandbox = Sandbox::new(fixture)?;
        let root = sandbox
            .state
            .parent()
            .ok_or("sandbox state has no parent")?
            .to_owned();
        let failpoints = Failpoints::new(&root)?;
        let dir = root.join("failpoints");
        hits::count(&dir, FALLBACK)?;
        Ok(Self {
            sandbox,
            failpoints,
            dir,
        })
    }

    fn start(&self, evidence: &Evidence) -> Result<Daemon<'_>, ScenarioError> {
        Daemon::start_with(&self.sandbox, evidence, |command| {
            self.failpoints.activate(command);
            command.env("VIA_TEST_EVENT_STALL_MS", STALL_MS);
        })
    }

    fn spawn(&self, evidence: &Evidence, prompt: &str) -> Result<String, ScenarioError> {
        let receipt = cli(
            &self.sandbox,
            evidence,
            "spawn",
            &[
                "spawn",
                "--harness",
                "fake",
                "--model",
                "fake",
                "--prompt",
                prompt,
                "--handle",
                HANDLE,
                "--wall-ms",
                "120000",
                "--background",
                "--json",
            ],
        )?;
        receipt["session_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| failure(format!("receipt has no session: {receipt}")))
    }

    fn wait(&self, evidence: &Evidence, session: &str) -> Result<Value, ScenarioError> {
        cli(
            &self.sandbox,
            evidence,
            "wait",
            &[
                "wait",
                &format!("{session}/1"),
                "--timeout-ms",
                "20000",
                "--json",
            ],
        )
    }

    /// Records the turn's envelope and its session's first events page as
    /// scenario evidence.
    fn record(
        &self,
        evidence: &Evidence,
        envelope: &Value,
        session: &str,
    ) -> Result<(), ScenarioError> {
        evidence
            .write("envelopes.ndjson", format!("{envelope}\n").as_bytes())
            .map_err(infra)?;
        let page = cli(
            &self.sandbox,
            evidence,
            "events",
            &["events", session, "--json"],
        )?;
        evidence
            .write("events.ndjson", page["events"].to_string().as_bytes())
            .map_err(infra)
    }

    /// The daemon's pid, as `daemon status` reports it.
    fn daemon_pid(&self, evidence: &Evidence) -> Result<u32, ScenarioError> {
        let status = cli(
            &self.sandbox,
            evidence,
            "daemon_status",
            &["daemon", "status", "--json"],
        )?;
        status["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .ok_or_else(|| failure(format!("daemon status has no pid: {status}")))
    }

    /// Waits until the fake's reported process is gone, Route stopped it,
    /// at most `bound` after `boundary`; returns how long after `boundary`
    /// it was gone.
    fn await_vendor_stopped(
        &self,
        boundary: Instant,
        bound: Duration,
    ) -> Result<Duration, ScenarioError> {
        let pid_file = self.sandbox.sync.join("agent.pid");
        absent_within(boundary, bound, || match fs::read_to_string(&pid_file) {
            // Not reported yet: not proved gone.
            Err(_) => Ok(false),
            Ok(pid) => stopped(pid.trim()),
        })
        .map_err(|error| {
            failure(format!(
                "the silent vendor after the stall's start, while Core was held: {error}"
            ))
        })
    }

    /// `via status <session>` (C1 §3.7).
    fn status(&self, evidence: &Evidence, session: &str) -> Result<Value, ScenarioError> {
        cli(
            &self.sandbox,
            evidence,
            "status",
            &["status", session, "--json"],
        )
    }

    /// Waits until the turn's `last_activity_at` holds still across two
    /// reads 100 ms apart: Route took what the vendor wrote.
    fn activity_settled(&self, evidence: &Evidence, session: &str) -> Result<(), ScenarioError> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let activity = |status: Value| status["progress"]["last_activity_at"].clone();
        let mut previous = activity(self.status(evidence, session)?);
        loop {
            thread::sleep(Duration::from_millis(100));
            let next = activity(self.status(evidence, session)?);
            if next == previous && next.is_string() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout(format!(
                    "activity never settled: {next}"
                )));
            }
            previous = next;
        }
    }

    /// The daemon is gone: no `WireMessages` was dropped without `finish`.
    fn no_fallback_drops(&self) -> Result<(), ScenarioError> {
        let drops = hits::hits(&self.dir, FALLBACK).map_err(infra)?;
        check(drops == 0, || {
            format!("{drops} Wire connections were dropped without finish")
        })
    }
}

/// Whether process `pid` has exited (absent, or a zombie). A malformed
/// pid or unreadable process state is uncertainty, never absence
/// (S1-evidence2 fix round 2, finding 13).
fn stopped(pid: &str) -> Result<bool, String> {
    let pid: u32 = pid
        .parse()
        .map_err(|error| format!("reported pid {pid:?}: {error}"))?;
    process::exited(pid)
}

/// Holds Core at its first model output, lets the stall fail the turn,
/// then releases Core: every observation admitted before the stall is
/// handled. The vendor's first burst starts once Core holds the model
/// output (Route took every message before it); the second follows once
/// Route took the first
/// (its arrivals stopped moving the activity clock), so the Wire queue of
/// 1,024 messages never overflows (A47). Returns the turn's step rows and
/// the envelope.
fn held_turn(
    setup: &Setup,
    evidence: &Evidence,
    prompt: &str,
) -> Result<(i64, Value), ScenarioError> {
    setup.failpoints.arm(PAUSE, 2, "pause").map_err(infra)?;
    let daemon = setup.start(evidence)?;
    let pid = setup.daemon_pid(evidence)?;
    let session = setup.spawn(evidence, prompt)?;
    setup
        .failpoints
        .wait_ack(PAUSE, 2, "pause", pid, Duration::from_secs(20))
        .map_err(failure)?;
    // Route took `accepted` and the model output, so the first burst's
    // 1,023 messages fit the Wire queue however late Route takes them.
    setup.sandbox.await_gate("flood")?;
    setup.sandbox.release_gate("flood")?;
    setup.sandbox.await_gate("burst")?;
    setup.activity_settled(evidence, &session)?;
    // The stall starts once a delivery blocks, after this release: from
    // it, the vendor is gone within the stall, the cleanup allowance and
    // the tolerance.
    let stall = Duration::from_millis(STALL_MS.parse().map_err(infra)?);
    let bound = stall + CLEANUP_ALLOWANCE + TOLERANCE;
    let started = Instant::now();
    setup.sandbox.release_gate("burst")?;
    let stopped_after = setup.await_vendor_stopped(started, bound)?;
    evidence
        .write(
            "f24_stall_cleanup.json",
            json!({
                "boundary":"burst release (at or before the stall's start)",
                "absent_after_ms":u64::try_from(stopped_after.as_millis()).unwrap_or(u64::MAX),
                "bound_ms":u64::try_from(bound.as_millis()).unwrap_or(u64::MAX),
            })
            .to_string()
            .as_bytes(),
        )
        .map_err(infra)?;
    setup.failpoints.release(PAUSE, 2).map_err(infra)?;
    let envelope = setup.wait(evidence, &session)?;
    setup.record(evidence, &envelope, &session)?;
    let rows = setup.sandbox.count(&format!(
        "SELECT count(*) FROM steps WHERE session_id='{session}' AND turn=1"
    ))?;
    drop(daemon);
    Ok((rows, envelope))
}

/// Design §2.3 Stall, §13.2: Core is held at `core.observations.pause` on
/// its first model output. The vendor then writes 341 steps of three
/// progress items (1,023) and, once Route took them, two more steps whose
/// items fill the 1,024-item channel and the hop until a delivery blocks;
/// the vendor writes nothing more. At the lowered stall the Adapter drops
/// the hop, Route fails the turn `overflow` and stops the vendor; the Wire
/// queue never overflows. Once Core resumes, the admitted items are
/// handled: 341 steps end, and the open step 342's row rides in the
/// terminal (Task 4 design §3.2): step 342's model output was never
/// admitted.
#[test]
fn s1_f24_stall_closes_the_hop_and_fails_overflow_without_vendor_output() -> TestResult {
    let setup = Setup::new(&blocks_then_silence("stall", 341, 2)?)?;
    let evidence = Evidence::new("s1_f24_stall", &setup.sandbox.fake, &setup.sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let (rows, envelope) = held_turn(&setup, evidence, "stall")?;
            check(
                envelope["state"] == "failed" && envelope["failure"]["class"] == "overflow",
                || format!("stalled turn: {envelope}"),
            )?;
            check(rows == 342, || format!("{rows} step rows, not 342"))?;
            setup.no_fallback_drops()
        },
        |evidence| collect_available(evidence, &setup.sandbox.state, &setup.sandbox.teardown),
    );
    report.require_pass()
}

/// Design §9: the fake holds its stdin unread while VIA writes a start
/// larger than the pipe buffer, so the start write cannot finish. A cancel
/// is still serviced: Route acts on the stop order at `force_at` and the
/// turn ends `cancelled` long before its wall deadline.
#[test]
fn s1_wire_route_services_cancel_while_stdin_is_held() -> TestResult {
    let prompt = format!("held-{}", "p".repeat(120 * 1024));
    let setup = Setup::new(&script(
        &prompt,
        &[
            json!({"action":"hold_stdin","name":"held"}),
            accepted(),
            completed(),
        ],
    ))?;
    let evidence = Evidence::new(
        "s1_wire_held_stdin",
        &setup.sandbox.fake,
        &setup.sandbox.fixture,
    )?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let daemon = setup.start(evidence)?;
            let session = setup.spawn(evidence, &prompt)?;
            setup.sandbox.await_gate("held")?;
            cli(
                &setup.sandbox,
                evidence,
                "cancel",
                &[
                    "cancel",
                    &session,
                    "--force-after",
                    "200",
                    "--handle",
                    HANDLE,
                    "--json",
                ],
            )?;
            let envelope = setup.wait(evidence, &session)?;
            setup.record(evidence, &envelope, &session)?;
            check(envelope["state"] == "cancelled", || {
                format!("held turn: {envelope}")
            })?;
            drop(daemon);
            setup.no_fallback_drops()
        },
        |evidence| collect_available(evidence, &setup.sandbox.state, &setup.sandbox.teardown),
    );
    report.require_pass()
}

/// F27 through the daemon: a `tool_started` message written in three pieces
/// cut inside multi-byte characters reaches `status` byte-exact as the
/// running tool's name; then a 2 MiB line fails the turn `overflow` with its
/// first 64 KiB saved and named.
#[test]
fn s1_f27_daemon_split_writes_keep_exact_text_and_a_huge_line_saves_its_prefix() -> TestResult {
    let exact = "é😀 split ✓";
    let started =
        json!({"type":"tool_started","vendor_turn_id":"fake-turn-1","tool_id":"t","name":exact});
    let line = format!("{}\n", serde_json::to_string(&started)?);
    let bytes = line.as_bytes();
    let cut = |at: &str| {
        line.find(at)
            .map(|index| index + 1)
            .ok_or_else(|| format!("{at} not in the line"))
    };
    let (first, second) = (cut("é")?, cut("😀")?);
    let huge = format!("{}\n", "h".repeat(2 * 1024 * 1024 - 1));
    let setup = Setup::new(&script(
        "split",
        &[
            accepted(),
            json!({"action":"emit_bytes","bytes":bytes[..first]}),
            json!({"action":"gate","name":"one"}),
            json!({"action":"emit_bytes","bytes":bytes[first..second]}),
            json!({"action":"gate","name":"two"}),
            json!({"action":"emit_bytes","bytes":bytes[second..]}),
            json!({"action":"gate","name":"three"}),
            json!({"action":"emit_raw","text":huge}),
            json!({"action":"hang"}),
        ],
    ))?;
    let evidence = Evidence::new("s1_f27_daemon", &setup.sandbox.fake, &setup.sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let daemon = setup.start(evidence)?;
            let session = setup.spawn(evidence, "split")?;
            for gate in ["one", "two"] {
                setup.sandbox.await_gate(gate)?;
                // Elapsed time only: the piece is read before the next.
                thread::sleep(Duration::from_millis(50));
                setup.sandbox.release_gate(gate)?;
            }
            // The tool is running before the huge line: a latched failure
            // drops what is still in flight (design §8.5).
            setup.sandbox.await_gate("three")?;
            let running_by = Instant::now() + Duration::from_secs(20);
            loop {
                let status = setup.status(evidence, &session)?;
                if status["progress"]["running_tools"] == json!([exact]) {
                    break;
                }
                check(Instant::now() < running_by, || {
                    format!("the split tool never ran: {status}")
                })?;
                thread::sleep(Duration::from_millis(10));
            }
            setup.sandbox.release_gate("three")?;
            let envelope = setup.wait(evidence, &session)?;
            setup.record(evidence, &envelope, &session)?;
            check(
                envelope["state"] == "failed" && envelope["failure"]["class"] == "overflow",
                || format!("huge-line turn: {envelope}"),
            )?;
            let saved = fs::read(
                setup
                    .sandbox
                    .state
                    .join("evidence")
                    .join(&session)
                    .join("1")
                    .join("undecoded.bin"),
            )
            .map_err(infra)?;
            check(saved == huge.as_bytes()[..64 * 1024], || {
                format!("undecoded.bin holds {} bytes", saved.len())
            })?;
            check(
                envelope["failure"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("undecoded.bin")),
                || format!("the failure does not name the file: {envelope}"),
            )?;
            drop(daemon);
            setup.no_fallback_drops()
        },
        |evidence| collect_available(evidence, &setup.sandbox.state, &setup.sandbox.teardown),
    );
    report.require_pass()
}

/// Polls `gone` until it reports absence, at most `bound` after `boundary`
/// (S1-evidence2 fix round 1): each probe is timestamped after it returns
/// and the bound is enforced before its absence is accepted, so a probe
/// that resumes after expiry and finds the process gone fails. Returns how
/// long after `boundary` absence was proved.
fn absent_within(
    boundary: Instant,
    bound: Duration,
    mut gone: impl FnMut() -> Result<bool, String>,
) -> Result<Duration, String> {
    loop {
        let absent = gone()?;
        let after = Instant::now().saturating_duration_since(boundary);
        if after > bound {
            return Err(format!(
                "absence not proved within {bound:?} of the boundary (probe at {after:?})"
            ));
        }
        if absent {
            return Ok(after);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// [`absent_within`] rejects an absence observed only after its bound: the
/// probe resumes late and finds the process gone (S1-evidence2 fix round 1).
#[test]
fn s1_f24_absence_after_the_bound_fails() {
    let late = absent_within(Instant::now(), Duration::from_millis(50), || {
        thread::sleep(Duration::from_millis(100));
        Ok(true)
    });
    assert!(late.is_err(), "late absence accepted: {late:?}");
    let timely = absent_within(Instant::now(), Duration::from_secs(5), || Ok(true));
    assert!(timely.is_ok(), "{timely:?}");
    // A malformed pid is uncertainty, never absence (S1-evidence2 fix
    // round 2, finding 13): the old probe read `/proc/<garbage>/stat`,
    // failed, and accepted the vendor as stopped.
    let malformed = absent_within(Instant::now(), Duration::from_secs(5), || stopped("x1"));
    assert!(malformed.is_err(), "malformed pid accepted: {malformed:?}");
}
