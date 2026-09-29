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
#[path = "support/scenario.rs"]
mod scenario;
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use daemon::{Daemon, Sandbox, TestResult, cli, failure, infra};
use failpoints::Failpoints;
use scenario::{ScenarioError, collect_available, run_scenario};
use serde_json::{Value, json};
use support::evidence::Evidence;

const HANDLE: &str = "h_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const PAUSE: &str = "core.observations.pause";
const FALLBACK: &str = "wire.fallback_drop";
/// The lowered stall (`VIA_TEST_EVENT_STALL_MS`).
const STALL_MS: &str = "500";

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

/// `count` copies of one text message in one write burst, then the vendor
/// reports its pid and falls silent.
fn flood_then_silence(prompt: &str, piece: &str, count: u64) -> TestResult<Value> {
    let line = format!("{}\n", serde_json::to_string(&text(piece))?);
    Ok(script(
        prompt,
        &[
            accepted(),
            json!({"action":"flood","text":line,"count":count}),
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

    /// Committed events of `kind` in `session`.
    fn committed(&self, session: &str, kind: &str) -> Result<i64, ScenarioError> {
        self.sandbox.count(&format!(
            "SELECT count(*) FROM events WHERE session_id='{session}' AND type='{kind}'"
        ))
    }

    /// Waits until the fake's reported process is gone: Route stopped it.
    fn await_vendor_stopped(&self) -> Result<(), ScenarioError> {
        let pid_file = self.sandbox.sync.join("agent.pid");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(pid) = fs::read_to_string(&pid_file)
                && stopped(pid.trim())
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ScenarioError::Timeout(
                    "the silent vendor was not stopped while Core was held".to_owned(),
                ));
            }
            thread::sleep(Duration::from_millis(10));
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

/// Whether process `pid` has exited (absent, or a zombie).
fn stopped(pid: &str) -> bool {
    match fs::read_to_string(Path::new("/proc").join(pid).join("stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

/// Holds Core at its first text observation, lets the stall fail the turn,
/// then releases Core: every observation admitted before the stall commits.
/// Returns the committed text count and the envelope.
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
    setup.await_vendor_stopped()?;
    setup.failpoints.release(PAUSE, 2).map_err(infra)?;
    let envelope = setup.wait(evidence, &session)?;
    setup.record(evidence, &envelope, &session)?;
    let texts = setup.committed(&session, "assistant.text")?;
    drop(daemon);
    Ok((texts, envelope))
}

/// Design §2.3 Stall, §13.2: Core is held at `core.observations.pause` on
/// its first text. The vendor then writes four texts of almost 1 MiB, each
/// split into four items: three fill the 4 MiB byte budget (the held item
/// counts until handled), the fourth's delivery blocks, and the vendor
/// writes nothing more. At the lowered stall the Adapter drops the hop,
/// Route fails the turn `overflow` and stops the vendor; the Wire queue
/// never overflows (four messages, under 4 MiB). Once Core resumes, the
/// three admitted texts commit.
#[test]
fn s1_f24_stall_closes_the_hop_and_fails_overflow_without_vendor_output() -> TestResult {
    let large = "s".repeat(1024 * 1024 - 1024);
    let setup = Setup::new(&flood_then_silence("stall", &large, 4)?)?;
    let evidence = Evidence::new("s1_f24_stall", &setup.sandbox.fake, &setup.sandbox.fixture)?;
    let report = run_scenario(
        evidence,
        |evidence| {
            let (texts, envelope) = held_turn(&setup, evidence, "stall")?;
            check(
                envelope["state"] == "failed" && envelope["failure"]["class"] == "overflow",
                || format!("stalled turn: {envelope}"),
            )?;
            // Three texts of four pieces each were admitted.
            check(texts == 12, || {
                format!("{texts} text pieces committed, not 12")
            })?;
            setup.no_fallback_drops()
        },
        |evidence| collect_available(evidence, &setup.sandbox.state),
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
        |evidence| collect_available(evidence, &setup.sandbox.state),
    );
    report.require_pass()
}

/// F27 through the daemon: a text message written in three pieces cut
/// inside multi-byte characters commits byte-exact; once it has, a 2 MiB line
/// fails the turn `overflow` with its first 64 KiB saved and named.
#[test]
fn s1_f27_daemon_split_writes_keep_exact_text_and_a_huge_line_saves_its_prefix() -> TestResult {
    let exact = "é😀 split ✓";
    let line = format!("{}\n", serde_json::to_string(&text(exact))?);
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
            // The text commits before the huge line: a latched failure
            // drops what is still in flight (design §8.5).
            setup.sandbox.await_gate("three")?;
            let committed_by = Instant::now() + Duration::from_secs(20);
            while setup.committed(&session, "assistant.text")? < 1 {
                check(Instant::now() < committed_by, || {
                    "the split text never committed".to_owned()
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
            let texts = cli(
                &setup.sandbox,
                evidence,
                "events",
                &["events", &session, "--json"],
            )?;
            let committed: Vec<&Value> = texts["events"]
                .as_array()
                .map(|events| {
                    events
                        .iter()
                        .filter(|event| event["type"] == "assistant.text")
                        .collect()
                })
                .unwrap_or_default();
            check(
                committed.len() == 1 && committed[0]["text"] == exact,
                || format!("split text: {committed:?}"),
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
        |evidence| collect_available(evidence, &setup.sandbox.state),
    );
    report.require_pass()
}
